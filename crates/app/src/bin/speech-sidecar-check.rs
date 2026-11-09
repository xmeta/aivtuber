//! Manual smoke and clock-error measurement for the speech sidecar (#92 S2).
//!
//! Spawns the sidecar exactly as the app would, ingests its batches into a
//! `SpeechObservationTracker` and prints, per observation, the signed
//! `lag = now_ms - observed_at_ms` (positive: the observation is older than
//! now), then a summary. Nothing is written to disk and no audio is handled
//! here (the sidecar keeps audio in its own memory).
//!
//! ```text
//! speech-sidecar-check [--duration-secs N] [--resync-ms N] [--wide] -- \
//!     speech-vad-sidecar --model M.onnx --language ja
//! ```
//!
//! `--wide` widens the tracker's age and skew limits to one second so every
//! observation is accepted and measured; without it the S1 defaults apply and
//! over-age or future-dated observations are counted as rejects.

use aivtuber_app::{SpeechSidecar, SpeechSidecarConfig};
use aivtuber_domain::{SpeechObservation, SpeechObservationBatch};
use aivtuber_runtime::{IngressPolicy, SpeechObservationConfig, SpeechObservationTracker};
use std::process::ExitCode;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("speech-sidecar-check: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut duration = Duration::from_secs(30);
    let mut resync = Duration::from_secs(5);
    let mut wide = false;
    let mut program: Vec<String> = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--duration-secs" => {
                let n = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--duration-secs needs a number")?;
                duration = Duration::from_secs(n);
            }
            "--resync-ms" => {
                let n = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--resync-ms needs a number")?;
                resync = Duration::from_millis(n);
            }
            "--wide" => wide = true,
            "--" => {
                program = iter.by_ref().collect();
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let (exe, args) = program
        .split_first()
        .ok_or("missing `-- <program> <args...>`")?;

    let mut tracker_config = SpeechObservationConfig::default();
    if wide {
        tracker_config.max_age_ms = 1_000;
        tracker_config.max_future_skew_ms = 1_000;
    }
    let mut tracker = SpeechObservationTracker::new(tracker_config, IngressPolicy::default())
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut config = SpeechSidecarConfig::new(exe, args.to_vec());
    config.resync_interval = resync;
    let mut sidecar = SpeechSidecar::spawn(config, started).map_err(|e| e.to_string())?;

    let mut lags: Vec<i64> = Vec::new();
    let deadline = started + duration;
    while Instant::now() < deadline && !sidecar.is_degraded() {
        let now_ms = started.elapsed().as_millis() as u64;
        sidecar.poll(&mut tracker, now_ms, |line, _report| {
            let Ok(batch) = SpeechObservationBatch::from_json_bytes(line) else {
                return;
            };
            for observation in &batch.observations {
                if let SpeechObservation::SpeechActivity(activity) = observation {
                    let lag = now_ms as i64 - activity.observed_at_ms as i64;
                    lags.push(lag);
                    println!(
                        "t={now_ms}ms edge={:?} observed_at={}ms lag={lag}ms confidence={:?}",
                        activity.edge, activity.observed_at_ms, activity.confidence
                    );
                }
            }
        });
        std::thread::sleep(Duration::from_millis(2));
    }
    let stats = sidecar.stats();
    let state = sidecar.state().clone();
    sidecar.shutdown();

    lags.sort_unstable();
    let pick = |q: f64| {
        lags.get(((lags.len() as f64 - 1.0) * q).round() as usize)
            .copied()
    };
    println!(
        "summary observations={} lag_ms min={:?} p50={:?} p99={:?} max={:?} negative={} state={state:?}",
        lags.len(),
        lags.first(),
        pick(0.5),
        pick(0.99),
        lags.last(),
        lags.iter().filter(|l| **l < 0).count(),
    );
    println!("sidecar {stats:?}");
    println!("tracker {:?}", tracker.metrics());
    Ok(())
}
