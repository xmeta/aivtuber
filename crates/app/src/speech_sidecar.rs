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
//!
//! Transcripts (#92 S3b): the sidecar may also supervise a `speech-stt-worker`
//! child and relay its `speech-transcript` batch lines on the same stdout,
//! plus one `{"stt":{...}}` status line when the transcript path becomes
//! unavailable. Lines are routed by their top-level key (`stt`, `transcripts`,
//! otherwise an observation batch). A failed worker, or 32 consecutive
//! rejected transcript lines, degrades the *transcript path only*, counted
//! once as transcript provider unavailable: the VAD edges and the
//! deterministic floor fallback keep working, and nothing restarts.

use crate::raw_ingress::{RawIngressConfig, RawIngressMetrics, pump_bounded_records};
use aivtuber_adapters::speech_stt::{SttStatus, SttUnavailableReason, parse_stt_status_line};
use aivtuber_adapters::speech_vad::{encode_clock_sync_line, parse_feed_heartbeat_line};
use aivtuber_domain::{MAX_SPEECH_OBSERVATION_BATCH_BYTES, MAX_SPEECH_TRANSCRIPT_BATCH_BYTES};
use aivtuber_runtime::{
    SpeechBatchReport, SpeechObservationTracker, SpeechTranscriptTracker, TranscriptBatchReport,
};
use serde::Deserialize;
use serde::de::IgnoredAny;
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

/// Routing probe: only which top-level keys are present.
#[derive(Deserialize)]
struct LineProbe {
    #[serde(default)]
    stt: Option<IgnoredAny>,
    #[serde(default)]
    transcripts: Option<IgnoredAny>,
    #[serde(default)]
    vad: Option<IgnoredAny>,
}

enum LineKind {
    /// `{"vad":{"heartbeat":{...}}}`: feed liveness (#81 S2).
    FeedHeartbeat,
    Status,
    Transcript,
    Observation,
}

fn classify(line: &[u8]) -> LineKind {
    match serde_json::from_slice::<LineProbe>(line) {
        Ok(LineProbe { stt: Some(_), .. }) => LineKind::Status,
        Ok(LineProbe {
            transcripts: Some(_),
            ..
        }) => LineKind::Transcript,
        Ok(LineProbe { vad: Some(_), .. }) => LineKind::FeedHeartbeat,
        // Anything else, including garbage, is judged by the observation
        // tracker exactly as before S3b.
        _ => LineKind::Observation,
    }
}

/// What the app knows about the transcript path of the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechTranscriptPath {
    /// The sidecar has not reported a transcript worker (none configured, or
    /// still loading its model).
    NotReported,
    /// The worker reported ready (its first heartbeat).
    Ready,
    /// No further transcripts: counted once, never restarted.
    Degraded(SttUnavailableReason),
}
/// A sync line sits in the pipe until the child has started and loaded its
/// model, so the first ones are stale by the child's startup time (measured
/// 18 ms to the first line with no model; a model load adds hundreds). The
/// child keeps the freshest offset it has seen, so for the first
/// `CLOCK_WARMUP` the clock is re-sent every `CLOCK_WARMUP_INTERVAL` and the
/// backlog it drains at start ends with a fresh line. A full pipe just parks
/// the writer thread, so this is bounded.
pub const CLOCK_WARMUP: Duration = Duration::from_secs(10);
pub const CLOCK_WARMUP_INTERVAL: Duration = Duration::from_millis(50);

/// Sidecar arguments that enable the transcript worker, from the app's
/// `AIVTUBER_SPEECH_STT_*` configuration. The sidecar spawns and supervises
/// the worker; the app never starts it directly.
pub fn stt_sidecar_args(
    worker: &str,
    model: &str,
    model_dir: &str,
    threads: Option<u64>,
) -> Vec<String> {
    let mut args = vec![
        "--stt-worker".to_owned(),
        worker.to_owned(),
        "--stt-model".to_owned(),
        model.to_owned(),
        "--stt-model-dir".to_owned(),
        model_dir.to_owned(),
    ];
    if let Some(threads) = threads {
        args.push("--stt-threads".to_owned());
        args.push(threads.to_string());
    }
    args
}

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
    /// Transcript batches the transcript tracker accepted for evaluation.
    pub transcript_batches_ingested: u64,
    /// Transcript lines the tracker refused (invalid, oversize or policy).
    pub transcript_lines_rejected: u64,
    /// Transcript lines that arrived with no transcript tracker composed, or
    /// after the transcript path degraded.
    pub transcript_lines_ignored: u64,
    /// Well-formed feed-liveness beats received (#81 S2).
    pub feed_heartbeats: u64,
}

pub struct SpeechSidecar {
    child: Child,
    lines: Receiver<Vec<u8>>,
    ingress: RawIngressMetrics,
    stop: Arc<AtomicBool>,
    syncs_sent: Arc<AtomicU64>,
    state: SpeechSidecarState,
    consecutive_invalid: u32,
    consecutive_invalid_transcripts: u32,
    transcript_path: SpeechTranscriptPath,
    /// Runtime time at which the newest feed heartbeat was *received*; the
    /// receipt time, not the child's stamp, so a clock-mapping error cannot
    /// hide a stalled feed. `None`: the child never reported one.
    feed_heartbeat_ms: Option<u64>,
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
            max_record_bytes: MAX_SPEECH_OBSERVATION_BATCH_BYTES
                .max(MAX_SPEECH_TRANSCRIPT_BATCH_BYTES),
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
            consecutive_invalid_transcripts: 0,
            transcript_path: SpeechTranscriptPath::NotReported,
            feed_heartbeat_ms: None,
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

    pub fn transcript_path(&self) -> SpeechTranscriptPath {
        self.transcript_path
    }

    /// When the newest feed-liveness beat arrived, on the runtime timeline.
    /// `None` until the child sends one (it only does with
    /// `--feed-heartbeat-ms`). The floor treats `None` as "not reported", not
    /// as "stale".
    pub fn feed_heartbeat_ms(&self) -> Option<u64> {
        self.feed_heartbeat_ms
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
    /// Transcript lines are counted and ignored: use
    /// [`poll_with_transcripts`](Self::poll_with_transcripts) to consume them.
    pub fn poll(
        &mut self,
        tracker: &mut SpeechObservationTracker,
        now_ms: u64,
        observe: impl FnMut(&[u8], &SpeechBatchReport),
    ) {
        self.poll_with_transcripts(tracker, None, now_ms, observe, |_, _| {});
    }

    /// Like [`poll`](Self::poll), and routes transcript batches into
    /// `transcripts` (`observe_transcript` sees each ingested line and its
    /// report). With `None`, transcript lines are counted and ignored.
    pub fn poll_with_transcripts(
        &mut self,
        tracker: &mut SpeechObservationTracker,
        mut transcripts: Option<&mut SpeechTranscriptTracker>,
        now_ms: u64,
        mut observe: impl FnMut(&[u8], &SpeechBatchReport),
        mut observe_transcript: impl FnMut(&[u8], &TranscriptBatchReport),
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
                    match classify(&line) {
                        LineKind::FeedHeartbeat => {
                            // A malformed beat says nothing and changes
                            // nothing; it never counts toward degrading.
                            if parse_feed_heartbeat_line(&line).is_some() {
                                self.feed_heartbeat_ms =
                                    Some(self.feed_heartbeat_ms.map_or(now_ms, |p| p.max(now_ms)));
                                self.stats.feed_heartbeats += 1;
                            } else {
                                self.stats.lines_rejected += 1;
                            }
                            continue;
                        }
                        LineKind::Status => {
                            self.on_status(&line, transcripts.as_deref_mut());
                            continue;
                        }
                        LineKind::Transcript => {
                            self.on_transcript(
                                &line,
                                transcripts.as_deref_mut(),
                                now_ms,
                                &mut observe_transcript,
                            );
                            continue;
                        }
                        LineKind::Observation => {}
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
                                    transcripts.as_deref_mut(),
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
                    // The exit status can land a moment after the pipe
                    // closes; wait at most 50 ms, once, so an exit is named
                    // as one (this runs only on the way to degrading).
                    let status = (0..10).find_map(|_| match self.child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        _ => {
                            thread::sleep(Duration::from_millis(5));
                            None
                        }
                    });
                    let reason = match status {
                        Some(status) => format!("sidecar exited ({status})"),
                        None => "sidecar closed its output".to_owned(),
                    };
                    self.degrade(tracker, transcripts.as_deref_mut(), reason);
                    return;
                }
            }
        }
    }

    /// A health line from the sidecar about its transcript worker.
    fn on_status(&mut self, line: &[u8], transcripts: Option<&mut SpeechTranscriptTracker>) {
        match parse_stt_status_line(line) {
            Some(SttStatus::Ready) => {
                if self.transcript_path == SpeechTranscriptPath::NotReported {
                    self.transcript_path = SpeechTranscriptPath::Ready;
                    eprintln!("speech_sidecar: transcript worker ready");
                }
            }
            Some(SttStatus::Unavailable { reason }) => {
                self.degrade_transcripts(transcripts, reason);
            }
            // A malformed status line says nothing and changes nothing.
            None => self.stats.lines_rejected += 1,
        }
    }

    fn on_transcript(
        &mut self,
        line: &[u8],
        transcripts: Option<&mut SpeechTranscriptTracker>,
        now_ms: u64,
        observe: &mut impl FnMut(&[u8], &TranscriptBatchReport),
    ) {
        let Some(tracker) = transcripts else {
            self.stats.transcript_lines_ignored += 1;
            return;
        };
        if matches!(self.transcript_path, SpeechTranscriptPath::Degraded(_)) {
            self.stats.transcript_lines_ignored += 1;
            return;
        }
        match tracker.ingest_bytes(line, now_ms) {
            Ok(report) => {
                self.stats.transcript_batches_ingested += 1;
                self.consecutive_invalid_transcripts = 0;
                observe(line, &report);
            }
            Err(_) => {
                self.stats.transcript_lines_rejected += 1;
                self.consecutive_invalid_transcripts += 1;
                if self.consecutive_invalid_transcripts >= self.config.max_consecutive_invalid {
                    self.degrade_transcripts(Some(tracker), SttUnavailableReason::InvalidLines);
                }
            }
        }
    }

    /// The transcript path ends here, once; the sidecar keeps running.
    fn degrade_transcripts(
        &mut self,
        transcripts: Option<&mut SpeechTranscriptTracker>,
        reason: SttUnavailableReason,
    ) {
        if matches!(self.transcript_path, SpeechTranscriptPath::Degraded(_)) {
            return;
        }
        self.transcript_path = SpeechTranscriptPath::Degraded(reason);
        if let Some(tracker) = transcripts {
            tracker.record_provider_unavailable();
        }
        eprintln!(
            "speech_sidecar: transcripts unavailable: {}; no transcripts",
            reason.name()
        );
    }

    fn degrade(
        &mut self,
        tracker: &mut SpeechObservationTracker,
        transcripts: Option<&mut SpeechTranscriptTracker>,
        reason: String,
    ) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        tracker.record_provider_unavailable();
        // Without the sidecar there is no worker either.
        if self.transcript_path != SpeechTranscriptPath::NotReported
            || self.stats.transcript_batches_ingested > 0
        {
            self.degrade_transcripts(transcripts, SttUnavailableReason::WorkerExited);
        }
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
    fn stt_arguments_are_plain_tokens_in_a_fixed_order() {
        assert_eq!(
            stt_sidecar_args("w", "k2v2-int8", "m", Some(2)),
            [
                "--stt-worker",
                "w",
                "--stt-model",
                "k2v2-int8",
                "--stt-model-dir",
                "m",
                "--stt-threads",
                "2"
            ]
        );
        assert_eq!(stt_sidecar_args("w", "p", "m", None).len(), 6);
    }

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
