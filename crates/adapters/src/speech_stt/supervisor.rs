//! Supervisor of the `speech-stt-worker` child, owned by the speech sidecar
//! (issue #92, S3b).
//!
//! The sidecar spawns the worker as a separate process so that a crash, an
//! out-of-memory kill or a hang in a 0.5-1 GB model cannot take the VAD edges
//! down with it (decision Q3). Failure policy, the same as the S2 sidecar
//! has toward the app and with no automatic restart:
//!
//! * the worker exits (the decode-deadline exit code is distinguished),
//! * it closes its output, or stops reading audio,
//! * 32 consecutive output lines are not valid transcripts,
//! * no heartbeat arrives within the timeout (this closes the hung-child
//!   limitation S2 recorded, for this child),
//! * it cannot be started (including a pinned engine library mismatch).
//!
//! The first failure is final: the worker is killed, one
//! [`SttStatus::Unavailable`] line goes to the app, and the feeder is refused
//! (`ready()` is false) so the VAD carries on with no transcripts.

use crate::speech_stt::feeder::FrameSink;
use crate::speech_stt::pins::verify_engine_libs;
use crate::speech_stt::worker::EXIT_DECODE_DEADLINE;
use crate::speech_stt::{SttStatus, SttUnavailableReason, encode_stt_status_line};
use crate::speech_vad::LineHandle;
use aivtuber_domain::{MAX_SPEECH_TRANSCRIPT_BATCH_BYTES, SpeechTranscriptBatch};
use serde::Deserialize;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Output line cap: a transcript batch plus its JSON framing.
const MAX_LINE_BYTES: usize = MAX_SPEECH_TRANSCRIPT_BATCH_BYTES + 1_024;
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct SttWorkerConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// How long the worker may take to load its model and send the first
    /// heartbeat (model load is 2-10 s, EXP-0002 6.1; the pin check reads
    /// every model file first).
    pub startup_timeout: Duration,
    /// Longest silence between heartbeats once running.
    pub heartbeat_timeout: Duration,
    /// Encoded frames queued toward the worker. 10 ms of audio per frame in
    /// steady state; beyond it the segment is damaged (see the feeder).
    pub frame_queue: usize,
    pub max_consecutive_invalid: u32,
    /// Check the engine library pins beside the program before spawning it.
    /// On in production; a test double has no such libraries.
    pub verify_engine_libs: bool,
}

impl SttWorkerConfig {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            startup_timeout: Duration::from_secs(60),
            heartbeat_timeout: Duration::from_secs(5),
            frame_queue: 512,
            max_consecutive_invalid: 32,
            verify_engine_libs: true,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.program.as_os_str().is_empty() {
            return Err("stt worker program is empty".to_owned());
        }
        if self.args.len() > 32 {
            return Err("stt worker takes at most 32 arguments".to_owned());
        }
        if !(1..=4096).contains(&self.frame_queue) || self.max_consecutive_invalid == 0 {
            return Err("stt worker queue bounds are out of range".to_owned());
        }
        if self.heartbeat_timeout < Duration::from_millis(200)
            || self.startup_timeout < self.heartbeat_timeout
        {
            return Err("stt worker timeouts are out of range".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SttWorkerStats {
    pub transcript_lines_relayed: u64,
    pub invalid_lines: u64,
    pub heartbeats: u64,
    pub frames_dropped_queue_full: u64,
}

struct Shared {
    started: Instant,
    ready: AtomicBool,
    /// Orderly shutdown in progress: the worker ending is not a failure.
    stopping: AtomicBool,
    failed: Mutex<Option<SttUnavailableReason>>,
    last_heartbeat: Mutex<Instant>,
    relayed: AtomicU64,
    invalid: AtomicU64,
    heartbeats: AtomicU64,
    dropped: AtomicU64,
    child: Mutex<Child>,
    out: LineHandle,
}

impl Shared {
    fn is_failed(&self) -> bool {
        self.failed.lock().expect("failed").is_some()
    }

    /// Record the first failure, kill the child, tell the app once.
    fn fail(&self, reason: SttUnavailableReason) {
        if self.stopping.load(Ordering::Relaxed) {
            return;
        }
        {
            let mut failed = self.failed.lock().expect("failed");
            if failed.is_some() {
                return;
            }
            *failed = Some(reason);
        }
        let _ = self.child.lock().expect("child").kill();
        eprintln!(
            "speech-vad-sidecar: stt worker unavailable: {}; no transcripts",
            reason.name()
        );
        self.out
            .send(encode_stt_status_line(SttStatus::Unavailable { reason }));
    }
}

pub struct SttWorker {
    shared: Arc<Shared>,
    frames: Option<SyncSender<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

#[derive(Deserialize)]
struct HeartbeatLine {
    #[allow(dead_code)]
    heartbeat: serde_json::Value,
}

impl SttWorker {
    /// Spawn the worker. `out` carries relayed transcript lines and status
    /// lines to the app. A start failure is reported as an `unavailable`
    /// status as well as an error, so the app learns of it either way.
    pub fn spawn(config: SttWorkerConfig, out: LineHandle) -> Result<Self, String> {
        config.validate()?;
        let announce = |reason: SttUnavailableReason, message: String| {
            out.send(encode_stt_status_line(SttStatus::Unavailable { reason }));
            Err(message)
        };
        if config.verify_engine_libs {
            let dir = config
                .program
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            if let Err(error) = verify_engine_libs(&dir) {
                return announce(
                    SttUnavailableReason::WorkerFailedToStart,
                    format!("stt engine libraries: {error}"),
                );
            }
        }
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // The worker has no use for the app's credentials or configuration.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AIVTUBER_") {
                command.env_remove(key);
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return announce(
                    SttUnavailableReason::WorkerFailedToStart,
                    format!("stt worker {}: {error}", config.program.display()),
                );
            }
        };
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let now = Instant::now();
        let shared = Arc::new(Shared {
            started: now,
            ready: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            failed: Mutex::new(None),
            last_heartbeat: Mutex::new(now),
            relayed: AtomicU64::new(0),
            invalid: AtomicU64::new(0),
            heartbeats: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            child: Mutex::new(child),
            out,
        });
        let (frames, frame_rx) = sync_channel::<Vec<u8>>(config.frame_queue);
        {
            let shared = shared.clone();
            thread::spawn(move || write_frames(stdin, &frame_rx, &shared));
        }
        let reader = {
            let shared = shared.clone();
            let max_invalid = config.max_consecutive_invalid;
            thread::spawn(move || read_output(BufReader::new(stdout), &shared, max_invalid))
        };
        {
            let shared = shared.clone();
            let (startup, heartbeat) = (config.startup_timeout, config.heartbeat_timeout);
            thread::spawn(move || watchdog(&shared, startup, heartbeat));
        }
        Ok(Self {
            shared,
            frames: Some(frames),
            reader: Some(reader),
        })
    }

    pub fn is_failed(&self) -> bool {
        self.shared.is_failed()
    }

    pub fn failure(&self) -> Option<SttUnavailableReason> {
        *self.shared.failed.lock().expect("failed")
    }

    pub fn stats(&self) -> SttWorkerStats {
        SttWorkerStats {
            transcript_lines_relayed: self.shared.relayed.load(Ordering::Relaxed),
            invalid_lines: self.shared.invalid.load(Ordering::Relaxed),
            heartbeats: self.shared.heartbeats.load(Ordering::Relaxed),
            frames_dropped_queue_full: self.shared.dropped.load(Ordering::Relaxed),
        }
    }

    /// Orderly stop (not a failure): close the audio pipe so the worker
    /// finishes its open segment, give it `grace` to exit, then kill it.
    pub fn shutdown(&mut self, grace: Duration) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        self.frames = None;
        let deadline = Instant::now() + grace;
        loop {
            let done = matches!(
                self.shared.child.lock().expect("child").try_wait(),
                Ok(Some(_)) | Err(_)
            );
            if done || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        {
            let mut child = self.shared.child.lock().expect("child");
            let _ = child.kill();
            let _ = child.wait();
        }
        // The child is gone, so its stdout ends and the reader has relayed
        // every line the worker wrote.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl FrameSink for SttWorker {
    fn ready(&self) -> bool {
        self.shared.ready.load(Ordering::Relaxed) && !self.shared.is_failed()
    }

    fn offer(&mut self, frame: Vec<u8>) -> bool {
        if self.shared.is_failed() {
            return false;
        }
        let Some(sender) = &self.frames else {
            return false;
        };
        match sender.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

impl Drop for SttWorker {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        self.frames = None;
        let mut child = self.shared.child.lock().expect("child");
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn write_frames(mut stdin: impl Write, frames: &Receiver<Vec<u8>>, shared: &Shared) {
    for frame in frames {
        if stdin
            .write_all(&frame)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            shared.fail(SttUnavailableReason::WorkerStdinClosed);
            return;
        }
    }
}

fn read_output<R: Read>(mut reader: BufReader<R>, shared: &Shared, max_invalid: u32) {
    let mut line = Vec::with_capacity(4096);
    let mut consecutive_invalid = 0_u32;
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES) {
            Ok(LineRead::Eof) => break,
            Err(_) => break,
            Ok(LineRead::Oversize) => {
                shared.invalid.fetch_add(1, Ordering::Relaxed);
                consecutive_invalid += 1;
            }
            Ok(LineRead::Line) => {
                let text = trim(&line);
                if text.is_empty() {
                    continue;
                }
                if serde_json::from_slice::<HeartbeatLine>(text).is_ok() {
                    shared.heartbeats.fetch_add(1, Ordering::Relaxed);
                    *shared.last_heartbeat.lock().expect("heartbeat") = Instant::now();
                    if !shared.ready.swap(true, Ordering::Relaxed) {
                        shared.out.send(encode_stt_status_line(SttStatus::Ready));
                    }
                    continue;
                }
                if SpeechTranscriptBatch::from_json_bytes(text).is_ok() {
                    consecutive_invalid = 0;
                    shared.relayed.fetch_add(1, Ordering::Relaxed);
                    let mut relay = String::from_utf8_lossy(text).into_owned();
                    relay.push('\n');
                    shared.out.send(relay);
                    continue;
                }
                shared.invalid.fetch_add(1, Ordering::Relaxed);
                consecutive_invalid += 1;
            }
        }
        if consecutive_invalid >= max_invalid {
            shared.fail(SttUnavailableReason::InvalidLines);
            return;
        }
    }
    // The worker's stdout ended: it exited or closed its output. The watchdog
    // names an exit; this names the rest.
    let exited = shared.child.lock().expect("child").try_wait();
    shared.fail(match exited {
        Ok(Some(status)) if status.code() == Some(EXIT_DECODE_DEADLINE) => {
            SttUnavailableReason::DecodeDeadlineExceeded
        }
        Ok(Some(_)) => SttUnavailableReason::WorkerExited,
        _ => SttUnavailableReason::WorkerClosedOutput,
    });
}

fn watchdog(shared: &Shared, startup: Duration, heartbeat: Duration) {
    loop {
        thread::sleep(WATCHDOG_INTERVAL);
        if shared.is_failed() {
            return;
        }
        let exited = shared.child.lock().expect("child").try_wait();
        if let Ok(Some(status)) = exited {
            // Let the reader drain what the worker wrote before it died; it
            // reports the same reason when it reaches end of file.
            thread::sleep(Duration::from_millis(100));
            shared.fail(if status.code() == Some(EXIT_DECODE_DEADLINE) {
                SttUnavailableReason::DecodeDeadlineExceeded
            } else {
                SttUnavailableReason::WorkerExited
            });
            return;
        }
        let silent_for = if shared.ready.load(Ordering::Relaxed) {
            shared.last_heartbeat.lock().expect("heartbeat").elapsed()
        } else {
            shared.started.elapsed()
        };
        let limit = if shared.ready.load(Ordering::Relaxed) {
            heartbeat
        } else {
            startup
        };
        if silent_for > limit {
            shared.fail(SttUnavailableReason::HeartbeatMissing);
            return;
        }
    }
}

enum LineRead {
    Line,
    Oversize,
    Eof,
}

/// Read one line keeping at most `max + 1` bytes: an endless line costs time,
/// not memory.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> io::Result<LineRead> {
    line.clear();
    let read = (&mut *reader)
        .take(max as u64 + 1)
        .read_until(b'\n', line)?;
    if read == 0 {
        return Ok(LineRead::Eof);
    }
    if line.last() != Some(&b'\n') && line.len() > max {
        // Discard through the delimiter without keeping it.
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                return Ok(LineRead::Eof);
            }
            let newline = buffer.iter().position(|b| *b == b'\n');
            let used = newline.map_or(buffer.len(), |i| i + 1);
            reader.consume(used);
            if newline.is_some() {
                break;
            }
        }
        line.clear();
        return Ok(LineRead::Oversize);
    }
    Ok(LineRead::Line)
}

fn trim(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\n' | b'\r' | b' ' | b'\t') {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_lines_cap_memory_and_resume_after_an_oversize_line() {
        let mut data = vec![b'x'; 10_000];
        data.extend_from_slice(b"\nok\n");
        let mut reader = BufReader::new(data.as_slice());
        let mut line = Vec::new();
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 100).unwrap(),
            LineRead::Oversize
        ));
        assert!(line.len() <= 101);
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 100).unwrap(),
            LineRead::Line
        ));
        assert_eq!(trim(&line), b"ok");
        assert!(matches!(
            read_bounded_line(&mut reader, &mut line, 100).unwrap(),
            LineRead::Eof
        ));
    }

    #[test]
    fn configuration_bounds_are_enforced() {
        let ok = SttWorkerConfig::new("worker", vec![]);
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.program = PathBuf::new();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.args = vec!["a".into(); 33];
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.heartbeat_timeout = Duration::from_millis(10);
        assert!(bad.validate().is_err());
        let mut bad = ok;
        bad.max_consecutive_invalid = 0;
        assert!(bad.validate().is_err());
    }
}
