//! Issue #92 S2: the `speech-vad-sidecar` binary end to end, without a
//! microphone. Run with `--features silero-vad`.
//!
//! Model-dependent tests fetch the pinned Silero VAD v6.2.3 export (or use
//! `AIVTUBER_SILERO_MODEL`) and always verify its SHA-256. Neither the model
//! nor any audio is committed; the WAV inputs below are synthetic signals
//! generated in memory and written under the cargo test temp dir.
#![cfg(feature = "silero-vad")]

use aivtuber_adapters::speech_vad::{SILERO_MODEL_SHA256, SILERO_MODEL_URL, verify_model_bytes};
use aivtuber_domain::{SpeechEdge, SpeechObservation, SpeechObservationBatch};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SIDECAR: &str = env!("CARGO_BIN_EXE_speech-vad-sidecar");

fn model_path() -> &'static Path {
    static MODEL: OnceLock<PathBuf> = OnceLock::new();
    MODEL.get_or_init(|| {
        if let Ok(path) = std::env::var("AIVTUBER_SILERO_MODEL") {
            let bytes = std::fs::read(&path).expect("read AIVTUBER_SILERO_MODEL");
            verify_model_bytes(&bytes).expect("AIVTUBER_SILERO_MODEL must be the pinned export");
            return PathBuf::from(path);
        }
        let cached =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("silero_vad_op18_ifless_v6.2.3.onnx");
        if let Ok(bytes) = std::fs::read(&cached)
            && verify_model_bytes(&bytes).is_ok()
        {
            return cached;
        }
        let bytes = ureq::get(SILERO_MODEL_URL)
            .call()
            .unwrap_or_else(|e| panic!("fetch {SILERO_MODEL_URL}: {e}"))
            .body_mut()
            .read_to_vec()
            .expect("read model body");
        verify_model_bytes(&bytes).unwrap_or_else(|e| panic!("{e} (pinned {SILERO_MODEL_SHA256})"));
        std::fs::write(&cached, &bytes).expect("cache model under the cargo test temp dir");
        cached
    })
}

/// Synthetic vowel-like signal: a glottal pulse train with vibrato through
/// three formant resonators, with 0.5 s of digital silence on each side.
/// Silero v6 scores it as speech (0.9+) and returns to near zero in silence.
fn synthetic_vowel_16k() -> Vec<i16> {
    let sr = 16_000.0_f64;
    let secs = 1.5_f64;
    let n = (secs * sr) as usize;
    let mut phase = 0.0_f64;
    let mut pulses = vec![0.0_f64; n];
    for (i, p) in pulses.iter_mut().enumerate() {
        let t = i as f64 / sr;
        let before = phase.floor();
        phase += 130.0 * (1.0 + 0.02 * (2.0 * std::f64::consts::PI * 5.0 * t).sin()) / sr;
        if phase.floor() > before {
            *p = 1.0;
        }
    }
    let mut y = 0.0;
    let mut x: Vec<f64> = pulses
        .iter()
        .map(|p| {
            y = 0.9 * y + p;
            y
        })
        .collect();
    for (fc, bw) in [(800.0, 80.0), (1200.0, 90.0), (2600.0, 120.0)] {
        let r = (-std::f64::consts::PI * bw / sr).exp();
        let theta = 2.0 * std::f64::consts::PI * fc / sr;
        let (a1, a2) = (-2.0 * r * theta.cos(), r * r);
        let (mut y1, mut y2) = (0.0, 0.0);
        for v in x.iter_mut() {
            let out = *v - a1 * y1 - a2 * y2;
            y2 = y1;
            y1 = out;
            *v = out;
        }
    }
    let peak = x.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    let mut signal = vec![0.0_f64; 8_000];
    signal.extend(x.iter().enumerate().map(|(i, v)| {
        let t = i as f64 / sr;
        let env = (t / 0.05).min((secs - t) / 0.05).min(1.0);
        v / peak * 0.5 * env
    }));
    signal.extend(vec![0.0_f64; 8_000]);
    signal
        .iter()
        .map(|v| (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect()
}

fn write_wav(name: &str, rate: u32, channels: u16, samples: &[i16]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("wav");
    for s in samples {
        writer.write_sample(*s).expect("sample");
    }
    writer.finalize().expect("finalize");
    path
}

/// Run on a WAV with the local clock and collect stdout lines.
fn run_wav(wav: &Path, extra: &[&str]) -> (Vec<String>, String, bool) {
    let output = Command::new(SIDECAR)
        .args(["--model"])
        .arg(model_path())
        .args(["--language", "ja", "--clock", "local", "--input-wav"])
        .arg(wav)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("run sidecar");
    let lines = String::from_utf8(output.stdout)
        .expect("stdout is text")
        .lines()
        .map(str::to_owned)
        .collect();
    (
        lines,
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

fn parse_edges(lines: &[String]) -> Vec<(SpeechEdge, u64, Option<f64>)> {
    lines
        .iter()
        .flat_map(|line| {
            let batch = SpeechObservationBatch::from_json_bytes(line.as_bytes())
                .unwrap_or_else(|e| panic!("line is not a valid batch: {e}: {line}"));
            assert_eq!(batch.provider_id, "silero-vad-sidecar");
            assert_eq!(batch.language, "ja");
            assert!(batch.provider_version.starts_with("silero-vad-v6.2.3"));
            batch.observations
        })
        .map(|o| match o {
            SpeechObservation::SpeechActivity(a) => (a.edge, a.observed_at_ms, a.confidence),
            SpeechObservation::TurnEndScore(_) => panic!("the sidecar emits no turn-end scores"),
        })
        .collect()
}

#[test]
fn the_pinned_model_is_verified_and_other_bytes_are_refused() {
    let bytes = std::fs::read(model_path()).unwrap();
    verify_model_bytes(&bytes).unwrap();
    let mut tampered = bytes;
    tampered[1000] ^= 1;
    assert!(verify_model_bytes(&tampered).is_err());

    let bogus = Path::new(env!("CARGO_TARGET_TMPDIR")).join("not-the-model.onnx");
    std::fs::write(&bogus, b"not an onnx model").unwrap();
    let output = Command::new(SIDECAR)
        .args(["--language", "ja", "--clock", "local", "--model"])
        .arg(&bogus)
        .arg("--input-wav")
        .arg(&bogus)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sha256"));
    assert!(output.stdout.is_empty());
}

#[test]
fn digital_silence_emits_nothing() {
    let wav = write_wav("silence.wav", 16_000, 1, &vec![0; 16_000 * 3]);
    let (lines, stderr, ok) = run_wav(&wav, &[]);
    assert!(ok, "{stderr}");
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn a_synthetic_vowel_yields_one_raw_start_then_end_that_conform_to_the_schema() {
    let wav = write_wav("vowel16k.wav", 16_000, 1, &synthetic_vowel_16k());
    let (lines, stderr, ok) = run_wav(&wav, &[]);
    assert!(ok, "{stderr}");
    let edges = parse_edges(&lines);
    // Local clock: observed times are this process's ms since start, so use
    // differences. The vowel starts 500 ms into the signal and lasts 1.5 s.
    assert_eq!(
        edges.iter().map(|e| e.0).collect::<Vec<_>>(),
        vec![SpeechEdge::Start, SpeechEdge::End],
        "{lines:?}"
    );
    let (start, end) = (edges[0].1, edges[1].1);
    assert!(end > start);
    let held = end - start;
    // Silero releases a steady synthetic vowel after a few hundred ms.
    assert!((200..=1_500).contains(&held), "held {held} ms");
    assert!(
        edges[0].2.unwrap() >= 0.5,
        "start confidence {:?}",
        edges[0].2
    );
    assert!(
        edges[1].2.unwrap() < 0.35,
        "end confidence {:?}",
        edges[1].2
    );
    // Every line is one complete JSON batch of at most the contract size.
    assert!(lines.iter().all(|l| l.len() < 16 * 1024));
}

#[test]
fn raw_edges_are_not_debounced_even_for_a_very_short_burst() {
    // 96 ms of the vowel (3 frames) in silence is shorter than any
    // minimum-speech filter; ADR-0005 says it must still be an edge pair.
    let vowel = synthetic_vowel_16k();
    let mut signal = vec![0i16; 8_000];
    signal.extend_from_slice(&vowel[8_000..8_000 + 1_536]);
    signal.extend(vec![0i16; 16_000]);
    let wav = write_wav("blip.wav", 16_000, 1, &signal);
    let (lines, stderr, ok) = run_wav(&wav, &[]);
    assert!(ok, "{stderr}");
    let kinds: Vec<_> = parse_edges(&lines).iter().map(|e| e.0).collect();
    assert_eq!(kinds, vec![SpeechEdge::Start, SpeechEdge::End], "{lines:?}");
}

#[test]
fn stereo_48k_input_is_downmixed_and_resampled_to_the_same_decision() {
    let mono = synthetic_vowel_16k();
    // Naive 3x upsample by sample-and-hold is a crude but deterministic
    // 48 kHz signal; stereo duplicates the channel.
    let mut stereo = Vec::with_capacity(mono.len() * 6);
    for s in &mono {
        for _ in 0..3 {
            stereo.extend([*s, *s]);
        }
    }
    let wav = write_wav("vowel48k-stereo.wav", 48_000, 2, &stereo);
    let (lines, stderr, ok) = run_wav(&wav, &[]);
    assert!(ok, "{stderr}");
    let edges = parse_edges(&lines);
    let kinds: Vec<_> = edges.iter().map(|e| e.0).collect();
    assert_eq!(kinds, vec![SpeechEdge::Start, SpeechEdge::End], "{lines:?}");
    // Same decision as the native 16 kHz file, within two frames, so the
    // resampler adds no material shift or distortion for the VAD.
    let wav16 = write_wav("vowel16k-ref.wav", 16_000, 1, &mono);
    let (lines16, stderr16, ok16) = run_wav(&wav16, &[]);
    assert!(ok16, "{stderr16}");
    let edges16 = parse_edges(&lines16);
    let held = edges[1].1 as i64 - edges[0].1 as i64;
    let held16 = edges16[1].1 as i64 - edges16[0].1 as i64;
    eprintln!("held 48k stereo {held} ms vs 16k {held16} ms");
    assert!((held - held16).abs() <= 64, "held {held} vs {held16}");
}

#[test]
fn realtime_pacing_spaces_edges_like_live_capture() {
    let wav = write_wav(
        "vowel-realtime.wav",
        16_000,
        1,
        &synthetic_vowel_16k()[..16_000 * 3 / 2],
    );
    let began = Instant::now();
    let (lines, stderr, ok) = run_wav(&wav, &["--pace", "realtime"]);
    assert!(ok, "{stderr}");
    assert!(
        began.elapsed() >= Duration::from_millis(900),
        "paced run took {:?}",
        began.elapsed()
    );
    assert!(!lines.is_empty());
}

// ---- clock handshake with the real binary (no model needed: probe mode) ----

struct Probe {
    child: Child,
    lines: mpsc::Receiver<(Instant, String)>,
}

fn spawn_probe(extra: &[&str]) -> Probe {
    let mut child = Command::new(SIDECAR)
        .args(["--language", "ja", "--clock-probe-ms", "20"])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn probe");
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send((Instant::now(), line)).is_err() {
                return;
            }
        }
    });
    Probe { child, lines }
}

fn observed_ms(line: &str) -> u64 {
    let batch = SpeechObservationBatch::from_json_bytes(line.as_bytes()).expect("valid batch");
    batch.observations[0].observed_at_ms()
}

fn send_sync(child: &mut Child, runtime_now_ms: u64) {
    let stdin = child.stdin.as_mut().unwrap();
    stdin
        .write_all(aivtuber_adapters::speech_vad::encode_clock_sync_line(runtime_now_ms).as_bytes())
        .and_then(|()| stdin.flush())
        .expect("write sync");
}

/// The handshake measurement for A-SPEECH-OBS-001: the stamp of an event the
/// sidecar creates "now" versus the runtime time at which the parent receives
/// the line. The difference is the stamp's offset error plus pipe latency, so
/// it upper-bounds the clock mapping error. A negative value would mean the
/// mapping ran ahead of the runtime clock. The parent re-sends the clock every
/// 50 ms during warm-up, as `SpeechSidecar` does: a single line written at
/// spawn is read only once the child has started, so it is that stale.
#[test]
fn clock_handshake_maps_stamps_onto_the_runtime_timeline_within_the_budget() {
    const RUNTIME_BASE_MS: u64 = 1_000_000;
    let mut probe = spawn_probe(&["--duration-secs", "3"]);
    let origin = Instant::now();
    let mut stdin = probe.child.stdin.take().unwrap();
    std::thread::spawn(move || {
        for _ in 0..60 {
            let now = RUNTIME_BASE_MS + origin.elapsed().as_millis() as u64;
            let line = aivtuber_adapters::speech_vad::encode_clock_sync_line(now);
            if stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush())
                .is_err()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let mut lags = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2_000);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok((arrived, line)) = probe.lines.recv_timeout(remaining) else {
            break;
        };
        let runtime_at_arrival =
            RUNTIME_BASE_MS as f64 + arrived.duration_since(origin).as_secs_f64() * 1000.0;
        lags.push((
            arrived.duration_since(origin),
            runtime_at_arrival - observed_ms(&line) as f64,
        ));
    }
    probe.child.kill().ok();
    assert!(lags.len() >= 40, "only {} observations", lags.len());
    // Skip the first 300 ms: the child is still starting and has seen only
    // the backlog.
    let mut settled: Vec<f64> = lags
        .iter()
        .filter(|(at, _)| *at > Duration::from_millis(300))
        .map(|(_, lag)| *lag)
        .collect();
    settled.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (min, p50, max) = (
        settled[0],
        settled[settled.len() / 2],
        *settled.last().unwrap(),
    );
    eprintln!(
        "clock handshake: n={} lag_ms min={min:.1} p50={p50:.1} max={max:.1} (budget A-SPEECH-OBS-001: 50 ms skew)",
        settled.len()
    );
    // 2 ms of rounding and timer granularity below zero, and the 50 ms budget
    // above it (loaded CI hosts; the measured values are far smaller).
    assert!(
        min > -3.0,
        "stamps ran ahead of the runtime clock by {min:.1} ms"
    );
    assert!(p50 < 50.0, "median stamp lag {p50:.1} ms");
}

#[test]
fn a_later_sync_moves_the_timeline_and_the_sidecar_exits_when_stdin_closes() {
    let mut probe = spawn_probe(&[]);
    send_sync(&mut probe.child, 5_000);
    let (_, first) = probe
        .lines
        .recv_timeout(Duration::from_secs(3))
        .expect("first line");
    let before = observed_ms(&first);
    assert!((5_000..5_500).contains(&before), "first stamp {before}");
    // Re-sync with the runtime clock 60 s ahead: later stamps follow it.
    send_sync(&mut probe.child, 65_000);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut moved = false;
    while Instant::now() < deadline && !moved {
        if let Ok((_, line)) = probe.lines.recv_timeout(Duration::from_millis(200)) {
            moved = observed_ms(&line) >= 60_000;
        }
    }
    assert!(moved, "re-sync was not applied");
    // Parent gone: the sidecar must exit by itself.
    drop(probe.child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = probe.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "sidecar did not exit after stdin closed"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
}

#[test]
fn without_a_clock_sync_the_sidecar_refuses_to_stamp_and_exits() {
    let mut child = Command::new(SIDECAR)
        .args([
            "--language",
            "ja",
            "--clock-probe-ms",
            "20",
            "--sync-timeout-ms",
            "300",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let began = Instant::now();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(began.elapsed() < Duration::from_secs(3));
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert!(stdout.is_empty(), "no unstamped observation may be emitted");
}
