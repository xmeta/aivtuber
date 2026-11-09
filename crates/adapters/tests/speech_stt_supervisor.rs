//! Issue #92 S3b: the STT worker supervisor against the worker test double.
//! No microphone, model or engine library is involved.

use aivtuber_adapters::speech_stt::feeder::FrameSink;
use aivtuber_adapters::speech_stt::protocol::{encode_audio, encode_end, encode_start};
use aivtuber_adapters::speech_stt::supervisor::{SttWorker, SttWorkerConfig};
use aivtuber_adapters::speech_stt::{SttStatus, SttUnavailableReason, parse_stt_status_line};
use aivtuber_adapters::speech_vad::LineSink;
use aivtuber_domain::{SpeechTranscriptBatch, TranscriptKind};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DOUBLE: &str = env!("CARGO_BIN_EXE_speech-stt-worker-double");

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<u8>>>);

impl Write for Collector {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Collector {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn statuses(&self) -> Vec<SttStatus> {
        self.lines()
            .iter()
            .filter_map(|l| parse_stt_status_line(l.as_bytes()))
            .collect()
    }
    fn transcripts(&self) -> Vec<aivtuber_domain::SpeechTranscript> {
        self.lines()
            .iter()
            .filter_map(|l| SpeechTranscriptBatch::from_json_bytes(l.as_bytes()).ok())
            .flat_map(|b| b.transcripts)
            .collect()
    }
}

fn config(mode: &str, extra: &[&str]) -> SttWorkerConfig {
    let mut args = vec!["--mode".to_owned(), mode.to_owned()];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut config = SttWorkerConfig::new(DOUBLE, args);
    // The double loads no engine library.
    config.verify_engine_libs = false;
    config.startup_timeout = Duration::from_secs(5);
    config.heartbeat_timeout = Duration::from_millis(800);
    config
}

fn spawn(config: SttWorkerConfig) -> (SttWorker, LineSink, Collector) {
    let collector = Collector::default();
    let sink = LineSink::spawn(collector.clone(), 256);
    let worker = SttWorker::spawn(config, sink.handle().expect("handle")).expect("spawn");
    (worker, sink, collector)
}

fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn unavailable(collector: &Collector) -> Vec<SttUnavailableReason> {
    collector
        .statuses()
        .into_iter()
        .filter_map(|s| match s {
            SttStatus::Unavailable { reason } => Some(reason),
            SttStatus::Ready => None,
        })
        .collect()
}

fn feed_segment(worker: &mut SttWorker, id: u64, ms: usize) {
    assert!(worker.offer(encode_start(id, 1_000)));
    for _ in 0..ms / 10 {
        assert!(worker.offer(encode_audio(&[0.1; 160])));
    }
    assert!(worker.offer(encode_end(id, false)));
}

#[test]
fn a_ready_worker_relays_partials_then_the_final_and_reports_ready_once() {
    let (mut worker, sink, collector) = spawn(config("normal", &[]));
    assert!(wait_for(Duration::from_secs(10), || worker.ready()));
    feed_segment(&mut worker, 7, 1_200);
    assert!(wait_for(Duration::from_secs(10), || {
        collector
            .transcripts()
            .iter()
            .any(|t| t.kind == TranscriptKind::Final)
    }));
    let transcripts = collector.transcripts();
    let last = transcripts.last().unwrap();
    assert_eq!(last.kind, TranscriptKind::Final);
    assert_eq!(last.segment_id, 7);
    assert_eq!(last.covered_audio_ms, 1_200);
    assert_eq!(last.observed_at_ms, 1_000 + 1_200);
    assert!(last.text.starts_with("音声"));
    assert!(
        collector
            .statuses()
            .iter()
            .filter(|s| **s == SttStatus::Ready)
            .count()
            == 1
    );
    assert!(unavailable(&collector).is_empty());
    // Orderly shutdown is not a failure and sends no unavailable status.
    worker.shutdown(Duration::from_secs(3));
    assert!(!worker.is_failed());
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
    assert!(unavailable(&collector).is_empty());
}

#[test]
fn a_worker_that_exits_is_reported_once_and_refused_further_audio() {
    let (mut worker, sink, collector) = spawn(config("exit", &["--after-ms", "300"]));
    assert!(wait_for(Duration::from_secs(10), || worker.is_failed()));
    assert_eq!(worker.failure(), Some(SttUnavailableReason::WorkerExited));
    assert!(!worker.ready());
    assert!(!worker.offer(encode_audio(&[0.0; 16])));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        unavailable(&collector),
        [SttUnavailableReason::WorkerExited],
        "reported exactly once"
    );
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
}

#[test]
fn a_hung_worker_is_caught_by_the_missing_heartbeat() {
    let (worker, sink, collector) = spawn(config("hang", &["--after-ms", "300"]));
    assert!(wait_for(Duration::from_secs(10), || worker.is_failed()));
    assert_eq!(
        worker.failure(),
        Some(SttUnavailableReason::HeartbeatMissing)
    );
    assert_eq!(
        unavailable(&collector),
        [SttUnavailableReason::HeartbeatMissing]
    );
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
}

#[test]
fn a_worker_that_never_loads_is_caught_by_the_startup_timeout() {
    let mut cfg = config("silent", &[]);
    cfg.startup_timeout = Duration::from_millis(900);
    let (worker, sink, collector) = spawn(cfg);
    assert!(!worker.ready());
    assert!(wait_for(Duration::from_secs(10), || worker.is_failed()));
    assert_eq!(
        unavailable(&collector),
        [SttUnavailableReason::HeartbeatMissing]
    );
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
}

#[test]
fn thirty_two_consecutive_invalid_lines_end_the_transcript_path() {
    let (worker, sink, collector) = spawn(config("garbage", &[]));
    assert!(wait_for(Duration::from_secs(10), || worker.is_failed()));
    assert_eq!(worker.failure(), Some(SttUnavailableReason::InvalidLines));
    assert!(worker.stats().invalid_lines >= 32);
    assert_eq!(
        unavailable(&collector),
        [SttUnavailableReason::InvalidLines]
    );
    // Garbage never reached the app.
    assert!(collector.transcripts().is_empty());
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
}

#[test]
fn a_decode_past_the_deadline_is_reported_as_such() {
    let (mut worker, sink, collector) = spawn(config("deadline", &[]));
    assert!(wait_for(Duration::from_secs(10), || worker.ready()));
    // The double's partial step is 500 ms of audio; its fake decode takes 5 s
    // against a 300 ms deadline, so the worker ends itself with the deadline
    // exit code.
    feed_segment(&mut worker, 1, 800);
    assert!(wait_for(Duration::from_secs(10), || worker.is_failed()));
    assert_eq!(
        worker.failure(),
        Some(SttUnavailableReason::DecodeDeadlineExceeded)
    );
    assert_eq!(
        unavailable(&collector),
        [SttUnavailableReason::DecodeDeadlineExceeded]
    );
    drop(worker);
    let _ = sink.finish(Duration::from_secs(1));
}

#[test]
fn a_missing_binary_or_unpinned_libraries_fail_to_start_and_tell_the_app() {
    let collector = Collector::default();
    let sink = LineSink::spawn(collector.clone(), 16);
    let mut cfg = SttWorkerConfig::new("definitely-not-a-real-stt-worker", vec![]);
    cfg.verify_engine_libs = false;
    assert!(SttWorker::spawn(cfg, sink.handle().unwrap()).is_err());

    // With verification on, a directory without the pinned libraries is
    // refused before anything is spawned. (Run the check from a scratch copy:
    // the build directory may hold real engine libraries from a sherpa-stt
    // build.)
    let scratch = std::env::temp_dir().join(format!("aivtuber-stt-nolibs-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let copy = scratch.join(std::path::Path::new(DOUBLE).file_name().unwrap());
    std::fs::copy(DOUBLE, &copy).unwrap();
    let cfg = SttWorkerConfig::new(&copy, vec![]);
    let error = SttWorker::spawn(cfg, sink.handle().unwrap()).err().unwrap();
    assert!(error.contains("engine libraries"), "{error}");
    let _ = std::fs::remove_dir_all(&scratch);
    let _ = sink.finish(Duration::from_secs(1));
    assert_eq!(
        unavailable(&collector),
        [
            SttUnavailableReason::WorkerFailedToStart,
            SttUnavailableReason::WorkerFailedToStart
        ]
    );
}
