//! Supervisor for the local speech-activity sidecar (#92 S2).
//!
//! The sidecar (`speech-vad-sidecar`, crates/adapters) is a child process the
//! app spawns only when configured. Its stdout carries `speech-observation`
//! batch lines into the S1 `SpeechObservationTracker`; this is a separate pipe
//! from the stdin content ingress, so a signal never rides the content queue.
//! Its stdin carries the runtime clock (`{"clock_sync":{"runtime_now_ms":N}}`)
//! at start and on a bounded schedule, so the sidecar stamps observations on
//! this process's monotonic timeline (A-SPEECH-OBS-001).
//!
//! Failure policy: nothing here blocks the scheduler or audio. The reader runs
//! on its own thread behind a bounded drop-newest queue, per-poll work is
//! capped, and a child that exits, closes stdout or sends only invalid batches
//! degrades to "no speech observations" and is counted once as provider
//! unavailable. There is no restart: a restarted sidecar would need a
//! deliberate operator decision, and the fallback keeps working without it.

use crate::raw_ingress::{RawIngressConfig, RawIngressMetrics, pump_bounded_records};
use aivtuber_adapters::speech_vad::encode_clock_sync_line;
use aivtuber_domain::MAX_SPEECH_OBSERVATION_BATCH_BYTES;
use aivtuber_runtime::{SpeechBatchReport, SpeechObservationTracker};
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// Most child arguments accepted from configuration.
pub const MAX_SPEECH_SIDECAR_ARGS: usize = 32;
/// A sync line sits in the pipe until the child has started and loaded its
/// model, so the first ones are stale by the child's startup time (measured
/// 18 ms to the first line with no model; a model load adds hundreds). The
/// child keeps the freshest offset it has seen, so for the first
/// `CLOCK_WARMUP` the clock is re-sent every `CLOCK_WARMUP_INTERVAL` and the
/// backlog it drains at start ends with a fresh line. A full pipe just parks
/// the writer thread, so this is bounded.
pub const CLOCK_WARMUP: Duration = Duration::from_secs(10);
pub const CLOCK_WARMUP_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechSidecarConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// How often the runtime clock is re-sent. Bounded: one short line per
    /// interval, never faster than 250 ms.
    pub resync_interval: Duration,
    /// Lines buffered between the reader thread and the runtime loop. Beyond
    /// it the newest line is dropped and counted.
    pub queue_capacity: usize,
    /// Lines ingested per `poll`, so a flooding child cannot starve the loop.
    pub max_lines_per_poll: usize,
    /// Consecutive rejected lines after which the child is considered
    /// misbehaving and is stopped.
    pub max_consecutive_invalid: u32,
}

impl SpeechSidecarConfig {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            resync_interval: Duration::from_secs(5),
            queue_capacity: 64,
            max_lines_per_poll: 64,
            max_consecutive_invalid: 32,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.program.as_os_str().is_empty() {
            return Err("speech sidecar program is empty".to_owned());
        }
        if self.args.len() > MAX_SPEECH_SIDECAR_ARGS {
            return Err(format!(
                "speech sidecar takes at most {MAX_SPEECH_SIDECAR_ARGS} arguments"
            ));
        }
        if !(Duration::from_millis(250)..=Duration::from_secs(60)).contains(&self.resync_interval) {
            return Err("speech sidecar resync interval must be 250 ms to 60 s".to_owned());
        }
        if !(1..=4096).contains(&self.queue_capacity)
            || !(1..=1024).contains(&self.max_lines_per_poll)
            || self.max_consecutive_invalid == 0
        {
            return Err("speech sidecar queue or poll bounds are out of range".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechSidecarState {
    Running,
    /// The child is gone or was stopped; no further observations arrive.
    Degraded(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechSidecarStats {
    pub batches_ingested: u64,
    /// Lines the tracker refused (invalid, oversize, or ingress policy).
    pub lines_rejected: u64,
    pub clock_syncs_sent: u64,
    pub lines_dropped_queue_full: u64,
    pub lines_dropped_oversize: u64,
}

pub struct SpeechSidecar {
    child: Child,
    lines: Receiver<Vec<u8>>,
    ingress: RawIngressMetrics,
    stop: Arc<AtomicBool>,
    syncs_sent: Arc<AtomicU64>,
    state: SpeechSidecarState,
    consecutive_invalid: u32,
    stats: SpeechSidecarStats,
    config: SpeechSidecarConfig,
}

impl SpeechSidecar {
    /// Spawn the child with stdin and stdout piped and write the first clock
    /// sync immediately. `started` is the instant the runtime's `now_ms`
    /// counts from, so the sidecar's stamps land on the runtime timeline.
    /// `AIVTUBER_*` variables are removed from the child's environment: it has
    /// no use for the app's credentials or configuration.
    pub fn spawn(config: SpeechSidecarConfig, started: Instant) -> io::Result<Self> {
        config
            .validate()
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AIVTUBER_") {
                command.env_remove(key);
            }
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (sender, lines) = mpsc::sync_channel::<Vec<u8>>(config.queue_capacity);
        let ingress = RawIngressMetrics::default();
        let reader_metrics = ingress.clone();
        let raw = RawIngressConfig {
            max_record_bytes: MAX_SPEECH_OBSERVATION_BATCH_BYTES,
            queue_capacity: config.queue_capacity,
        };
        thread::spawn(move || {
            let _ = pump_bounded_records(BufReader::new(stdout), &sender, raw, &reader_metrics);
        });

        let stop = Arc::new(AtomicBool::new(false));
        let syncs_sent = Arc::new(AtomicU64::new(0));
        {
            let (stop, syncs, interval) =
                (stop.clone(), syncs_sent.clone(), config.resync_interval);
            thread::spawn(move || clock_writer(stdin, started, interval, &stop, &syncs));
        }
        Ok(Self {
            child,
            lines,
            ingress,
            stop,
            syncs_sent,
            state: SpeechSidecarState::Running,
            consecutive_invalid: 0,
            stats: SpeechSidecarStats::default(),
            config,
        })
    }

    pub fn state(&self) -> &SpeechSidecarState {
        &self.state
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self.state, SpeechSidecarState::Degraded(_))
    }

    pub fn stats(&self) -> SpeechSidecarStats {
        let ingress = self.ingress.snapshot();
        SpeechSidecarStats {
            clock_syncs_sent: self.syncs_sent.load(Ordering::Relaxed),
            lines_dropped_queue_full: ingress.dropped_queue_full,
            lines_dropped_oversize: ingress.dropped_oversize,
            ..self.stats
        }
    }

    /// Drain at most `max_lines_per_poll` queued lines into the tracker.
    /// Never blocks. `observe` sees each ingested batch's raw line and report.
    pub fn poll(
        &mut self,
        tracker: &mut SpeechObservationTracker,
        now_ms: u64,
        mut observe: impl FnMut(&[u8], &SpeechBatchReport),
    ) {
        if self.is_degraded() {
            return;
        }
        for _ in 0..self.config.max_lines_per_poll {
            match self.lines.try_recv() {
                Ok(line) => {
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    match tracker.ingest_bytes(&line, now_ms) {
                        Ok(report) => {
                            self.stats.batches_ingested += 1;
                            self.consecutive_invalid = 0;
                            observe(&line, &report);
                        }
                        Err(_) => {
                            self.stats.lines_rejected += 1;
                            self.consecutive_invalid += 1;
                            if self.consecutive_invalid >= self.config.max_consecutive_invalid {
                                self.degrade(
                                    tracker,
                                    "sidecar sent only rejected batches; stopped".to_owned(),
                                );
                                return;
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // The reader saw EOF: the child closed stdout, normally by
                    // exiting. Everything it sent has been drained above.
                    let reason = match self.child.try_wait() {
                        Ok(Some(status)) => format!("sidecar exited ({status})"),
                        _ => "sidecar closed its output".to_owned(),
                    };
                    self.degrade(tracker, reason);
                    return;
                }
            }
        }
    }

    fn degrade(&mut self, tracker: &mut SpeechObservationTracker, reason: String) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        tracker.record_provider_unavailable();
        eprintln!("speech_sidecar: degraded: {reason}; no speech observations");
        self.state = SpeechSidecarState::Degraded(reason);
    }

    /// Stop the child without counting it as a failure (orderly shutdown).
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SpeechSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Writes the runtime clock to the child's stdin: immediately, every
/// `CLOCK_WARMUP_INTERVAL` during the warm-up, then every `interval`. It is its own thread because a child that stops reading
/// stdin would otherwise block the writer once the pipe fills; here that only
/// parks this thread.
fn clock_writer(
    mut stdin: impl Write,
    started: Instant,
    interval: Duration,
    stop: &AtomicBool,
    sent: &AtomicU64,
) {
    let warm_until = Instant::now() + CLOCK_WARMUP;
    while !stop.load(Ordering::Relaxed) {
        let line = encode_clock_sync_line(elapsed_ms(started));
        if stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return;
        }
        sent.fetch_add(1, Ordering::Relaxed);
        let pause = if Instant::now() < warm_until {
            CLOCK_WARMUP_INTERVAL
        } else {
            interval
        };
        let wake = Instant::now() + pause;
        while Instant::now() < wake && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_bounds_are_enforced() {
        let ok = SpeechSidecarConfig::new("sidecar", vec!["--x".into()]);
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.program = PathBuf::new();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.args = vec!["a".into(); MAX_SPEECH_SIDECAR_ARGS + 1];
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.resync_interval = Duration::from_millis(10);
        assert!(bad.validate().is_err());
        let mut bad = ok;
        bad.max_consecutive_invalid = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn the_clock_writer_sends_immediately_then_at_the_warmup_cadence_and_stops() {
        struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let handle = {
            let (b, s, n) = (bytes.clone(), stop.clone(), sent.clone());
            thread::spawn(move || {
                clock_writer(Shared(b), Instant::now(), Duration::from_millis(60), &s, &n)
            })
        };
        thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!((3..=8).contains(&lines.len()), "{lines:?}");
        let times: Vec<u64> = lines
            .iter()
            .map(|l| aivtuber_adapters::speech_vad::parse_clock_sync_line(l.as_bytes()).unwrap())
            .collect();
        assert!(times[0] < 50, "first sync is immediate: {times:?}");
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(sent.load(Ordering::Relaxed) as usize, lines.len());
    }
}
