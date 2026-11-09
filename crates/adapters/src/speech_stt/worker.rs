//! STT worker core (issue #92, S3b): segments in, transcript revisions out.
//!
//! Engine-neutral and free of any model: the recognizer is a [`Decoder`], so
//! everything here (partial cadence, drop-when-behind, final-always-wins, the
//! hard segment cap, the decode deadline, the heartbeat, text bounds) is
//! tested with a fake decoder and no microphone or model.
//!
//! Threads: a reader parses the audio pipe, one decoder thread owns the
//! recognizer, and the coordinator (this function's caller thread) owns all
//! state and all output. The decoder never runs on the reader or the
//! coordinator, so a slow decode delays text, never the heartbeat or the
//! input. A decode that exceeds the deadline cannot be cancelled (the engine
//! offers no cancellation), so the worker exits with
//! [`EXIT_DECODE_DEADLINE`] and the supervisor degrades to no transcripts.
//!
//! Privacy: audio lives only in the open segment's bounded buffer (at most
//! `max_segment_ms`) and in the decode job copies, is never written anywhere,
//! and `Debug` output shows lengths only. Text is the engine's output
//! unmodified (decision Q9); a text that violates the wire bounds is counted
//! and replaced by an empty final rather than rewritten.

use crate::speech_stt::protocol::{Frame, FrameReader, ProtocolError};
use aivtuber_domain::{
    SPEECH_TRANSCRIPT_SCHEMA_VERSION, SpeechTranscript, SpeechTranscriptBatch, TranscriptEngine,
    TranscriptKind,
};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

/// Input sample rate of every decode.
pub const WORKER_SAMPLE_RATE_HZ: usize = 16_000;
/// Exit code when a decode exceeded its deadline.
pub const EXIT_DECODE_DEADLINE: i32 = 3;
/// Exit code when the audio pipe was malformed.
pub const EXIT_PROTOCOL: i32 = 4;
/// Exit code when the recognizer could not be built or failed to decode.
pub const EXIT_DECODER: i32 = 5;
/// Finals waiting behind the decode in flight (bounded queue of 1-2 segments).
const FINAL_QUEUE_CAPACITY: usize = 2;
const INPUT_QUEUE_CAPACITY: usize = 256;
/// Coordinator wake-up cadence: bounds deadline and heartbeat jitter.
const TICK: Duration = Duration::from_millis(25);

/// A blocking offline recognizer. `samples` are 16 kHz mono in [-1, 1].
pub trait Decoder: Send {
    fn decode(&mut self, samples: &[f32]) -> Result<String, String>;
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub provider_id: String,
    pub provider_version: String,
    pub language: String,
    pub speaker_id: String,
    pub engine: TranscriptEngine,
    /// Re-decode the open segment at most every this much new audio.
    pub partial_step_ms: u64,
    /// Hard cap per segment; the worker force-finalizes at it.
    pub max_segment_ms: u64,
    /// A decode running longer than this ends the worker.
    pub decode_deadline: Duration,
    pub heartbeat_interval: Duration,
}

impl WorkerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(100..=5_000).contains(&self.partial_step_ms) {
            return Err("partial step must be 100..=5000 ms".to_owned());
        }
        if !(1_000..=30_000).contains(&self.max_segment_ms) {
            return Err("max segment must be 1000..=30000 ms".to_owned());
        }
        if self.decode_deadline < Duration::from_millis(100)
            || self.decode_deadline > Duration::from_secs(120)
        {
            return Err("decode deadline must be 100 ms to 120 s".to_owned());
        }
        if self.heartbeat_interval < Duration::from_millis(50)
            || self.heartbeat_interval > Duration::from_secs(30)
        {
            return Err("heartbeat interval must be 50 ms to 30 s".to_owned());
        }
        Ok(())
    }
}

/// How the worker ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    /// The audio pipe closed cleanly; everything queued was drained.
    InputClosed,
    DecodeDeadline,
    Protocol(String),
    Decoder(String),
    /// The output pipe closed: the parent is gone.
    OutputClosed,
}

impl WorkerExit {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InputClosed | Self::OutputClosed => 0,
            Self::DecodeDeadline => EXIT_DECODE_DEADLINE,
            Self::Protocol(_) => EXIT_PROTOCOL,
            Self::Decoder(_) => EXIT_DECODER,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStats {
    pub segments_started: u64,
    pub partials_emitted: u64,
    pub finals_emitted: u64,
    /// Partial re-decodes not started because the decoder was busy.
    pub partials_dropped_busy: u64,
    /// Partial results discarded because their segment had ended or the
    /// text had not changed.
    pub partials_discarded: u64,
    /// Finals dropped because the bounded final queue was full.
    pub finals_dropped_queue_full: u64,
    pub segments_force_finalized: u64,
    pub audio_samples_dropped: u64,
    pub texts_rejected: u64,
    pub max_decode_ms: u64,
}

enum Event {
    Frame(Frame),
    InputClosed,
    ProtocolError(ProtocolError),
}

struct Job {
    segment_id: u64,
    kind: TranscriptKind,
    revision: u32,
    start_ms: u64,
    samples: Vec<f32>,
}

// Audio of a person: lengths only.
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("segment_id", &self.segment_id)
            .field("kind", &self.kind)
            .field("revision", &self.revision)
            .field("samples", &self.samples.len())
            .finish()
    }
}

struct JobResult {
    job: Job,
    text: Result<String, String>,
    decode_ms: u64,
}

/// A final waiting for the decoder. `samples == None` closes the segment with
/// an empty final without decoding (the sidecar said the segment is not worth
/// transcribing).
struct FinalJob {
    segment_id: u64,
    revision: u32,
    start_ms: u64,
    samples: Option<Vec<f32>>,
}

struct Segment {
    id: u64,
    start_ms: u64,
    samples: Vec<f32>,
    /// Audio length at the last partial request.
    last_request_samples: usize,
    last_drop_counted_at: Option<usize>,
    next_revision: u32,
    last_partial_text: Option<String>,
    /// The final was already queued (cap reached); later audio is dropped
    /// until END.
    finalized: bool,
}

struct Inflight {
    started: Instant,
}

/// Run the worker until the audio pipe closes or a fatal condition. `output`
/// receives one JSON line per transcript revision and heartbeat lines.
/// `build_decoder` runs on the decoder thread (model load happens there) and
/// the first heartbeat is written only after it succeeds, so a heartbeat means
/// "ready and alive".
pub fn run_worker<D, R, W>(
    input: R,
    mut output: W,
    build_decoder: impl FnOnce() -> Result<D, String> + Send + 'static,
    config: &WorkerConfig,
) -> (WorkerExit, WorkerStats)
where
    D: Decoder + 'static,
    R: Read + Send + 'static,
    W: Write,
{
    let mut stats = WorkerStats::default();
    let (event_tx, event_rx) = sync_channel::<Event>(INPUT_QUEUE_CAPACITY);
    let (job_tx, job_rx) = sync_channel::<Job>(1);
    let (result_tx, result_rx) = sync_channel::<JobResult>(1);
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);

    thread::spawn(move || {
        let mut decoder = match build_decoder() {
            Ok(decoder) => {
                let _ = ready_tx.send(Ok(()));
                decoder
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        while let Ok(job) = job_rx.recv() {
            let started = Instant::now();
            let text = decoder.decode(&job.samples);
            let decode_ms = started.elapsed().as_millis() as u64;
            if result_tx
                .send(JobResult {
                    job,
                    text,
                    decode_ms,
                })
                .is_err()
            {
                return;
            }
        }
    });

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return (WorkerExit::Decoder(error), stats),
        Err(_) => {
            return (
                WorkerExit::Decoder("decoder thread ended before it was ready".to_owned()),
                stats,
            );
        }
    }

    thread::spawn(move || {
        let mut reader = FrameReader::new(input);
        loop {
            let event = match reader.next_frame() {
                Ok(Some(frame)) => Event::Frame(frame),
                Ok(None) => Event::InputClosed,
                Err(error) => Event::ProtocolError(error),
            };
            let last = !matches!(event, Event::Frame(_));
            if event_tx.send(event).is_err() || last {
                return;
            }
        }
    });

    let mut core = Core {
        config,
        output: &mut output,
        job_tx,
        result_rx,
        segment: None,
        inflight: None,
        finals: VecDeque::new(),
        stats: &mut stats,
        heartbeat_seq: 0,
        next_heartbeat: Instant::now(),
    };
    let exit = core.run(&event_rx);
    (exit, stats)
}

struct Core<'a, W: Write> {
    config: &'a WorkerConfig,
    output: &'a mut W,
    job_tx: SyncSender<Job>,
    result_rx: Receiver<JobResult>,
    segment: Option<Segment>,
    inflight: Option<Inflight>,
    finals: VecDeque<FinalJob>,
    stats: &'a mut WorkerStats,
    heartbeat_seq: u64,
    next_heartbeat: Instant,
}

impl<W: Write> Core<'_, W> {
    fn step_samples(&self) -> usize {
        (self.config.partial_step_ms as usize * WORKER_SAMPLE_RATE_HZ) / 1000
    }

    fn max_samples(&self) -> usize {
        (self.config.max_segment_ms as usize * WORKER_SAMPLE_RATE_HZ) / 1000
    }

    fn run(&mut self, events: &Receiver<Event>) -> WorkerExit {
        let mut input_closed = false;
        loop {
            if let Some(exit) = self.housekeeping() {
                return exit;
            }
            if input_closed {
                // Drain what is queued, then stop.
                if self.inflight.is_none() && self.finals.is_empty() {
                    return WorkerExit::InputClosed;
                }
                match self.result_rx.recv_timeout(TICK) {
                    Ok(result) => {
                        if let Some(exit) = self.on_result(result) {
                            return exit;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return WorkerExit::Decoder("decoder thread ended".to_owned());
                    }
                }
                continue;
            }
            match events.recv_timeout(TICK) {
                Ok(Event::Frame(frame)) => {
                    if let Some(exit) = self.on_frame(frame) {
                        return exit;
                    }
                }
                Ok(Event::InputClosed) | Err(RecvTimeoutError::Disconnected) => {
                    // The parent closed the pipe: close the open segment so
                    // its text is not lost, then drain.
                    if let Some(exit) = self.end_segment(false) {
                        return exit;
                    }
                    input_closed = true;
                }
                Ok(Event::ProtocolError(error)) => return WorkerExit::Protocol(error.to_string()),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Per-tick work: collect a finished decode, enforce the deadline, start
    /// queued finals and write the heartbeat.
    fn housekeeping(&mut self) -> Option<WorkerExit> {
        while let Ok(result) = self.result_rx.try_recv() {
            if let Some(exit) = self.on_result(result) {
                return Some(exit);
            }
        }
        if let Some(inflight) = &self.inflight
            && inflight.started.elapsed() > self.config.decode_deadline
        {
            return Some(WorkerExit::DecodeDeadline);
        }
        if let Some(exit) = self.pump_finals() {
            return Some(exit);
        }
        if Instant::now() >= self.next_heartbeat {
            let line = format!(
                "{{\"heartbeat\":{{\"seq\":{},\"decoder_busy\":{}}}}}\n",
                self.heartbeat_seq,
                self.inflight.is_some()
            );
            self.heartbeat_seq += 1;
            self.next_heartbeat = Instant::now() + self.config.heartbeat_interval;
            if self.write(&line).is_err() {
                return Some(WorkerExit::OutputClosed);
            }
        }
        None
    }

    fn on_frame(&mut self, frame: Frame) -> Option<WorkerExit> {
        match frame {
            Frame::Start {
                segment_id,
                start_ms,
            } => {
                // A START while a segment is open closes it first: the final
                // must not be lost.
                if let Some(exit) = self.end_segment(false) {
                    return Some(exit);
                }
                self.stats.segments_started += 1;
                self.segment = Some(Segment {
                    id: segment_id,
                    start_ms,
                    samples: Vec::new(),
                    last_request_samples: 0,
                    last_drop_counted_at: None,
                    next_revision: 0,
                    last_partial_text: None,
                    finalized: false,
                });
                None
            }
            Frame::Audio(samples) => self.on_audio(&samples),
            Frame::End {
                segment_id,
                skip_decode,
            } => {
                if self.segment.as_ref().is_some_and(|s| s.id == segment_id) {
                    self.end_segment(skip_decode)
                } else {
                    // END for a segment that is not open (already closed by
                    // a START or the cap): nothing to do.
                    self.segment = self.segment.take().filter(|s| s.id != segment_id);
                    None
                }
            }
        }
    }

    fn on_audio(&mut self, samples: &[f32]) -> Option<WorkerExit> {
        let max = self.max_samples();
        let Some(segment) = self.segment.as_mut() else {
            self.stats.audio_samples_dropped += samples.len() as u64;
            return None;
        };
        if segment.finalized {
            self.stats.audio_samples_dropped += samples.len() as u64;
            return None;
        }
        let room = max.saturating_sub(segment.samples.len());
        let take = samples.len().min(room);
        segment.samples.extend_from_slice(&samples[..take]);
        self.stats.audio_samples_dropped += (samples.len() - take) as u64;
        if segment.samples.len() >= max {
            // Hard cap: force the final now; the rest of this segment's audio
            // is dropped until its END.
            self.stats.segments_force_finalized += 1;
            return self.end_segment_keep_open();
        }
        self.maybe_request_partial();
        None
    }

    /// Ask for a partial re-decode when enough new audio arrived and the
    /// decoder is free. Never queued: if the decoder is behind, the request
    /// is dropped and retried with newer audio.
    fn maybe_request_partial(&mut self) {
        let step = self.step_samples();
        let Some(segment) = self.segment.as_mut() else {
            return;
        };
        if segment.samples.len() < segment.last_request_samples + step {
            return;
        }
        if self.inflight.is_some() || !self.finals.is_empty() {
            if segment.last_drop_counted_at != Some(segment.last_request_samples) {
                segment.last_drop_counted_at = Some(segment.last_request_samples);
                self.stats.partials_dropped_busy += 1;
            }
            return;
        }
        segment.last_request_samples = segment.samples.len();
        let job = Job {
            segment_id: segment.id,
            kind: TranscriptKind::Partial,
            revision: 0,
            start_ms: segment.start_ms,
            samples: segment.samples.clone(),
        };
        self.inflight = Some(Inflight {
            started: Instant::now(),
        });
        // Capacity 1 and the decoder is idle, so this cannot block.
        let _ = self.job_tx.try_send(job);
    }

    /// Close the open segment: queue its final (or an empty one) and forget it.
    fn end_segment(&mut self, skip_decode: bool) -> Option<WorkerExit> {
        let segment = self.segment.take()?;
        self.queue_final(segment, skip_decode);
        None
    }

    /// Cap reached: queue the final but keep the segment so its remaining
    /// audio is dropped until END.
    fn end_segment_keep_open(&mut self) -> Option<WorkerExit> {
        let segment = self.segment.as_mut()?;
        segment.finalized = true;
        let revision = segment.next_revision;
        let job = FinalJob {
            segment_id: segment.id,
            revision,
            start_ms: segment.start_ms,
            samples: Some(std::mem::take(&mut segment.samples)),
        };
        self.push_final(job);
        None
    }

    fn queue_final(&mut self, segment: Segment, skip_decode: bool) {
        if segment.finalized {
            return;
        }
        let revision = segment.next_revision;
        let empty = skip_decode || segment.samples.is_empty();
        let job = FinalJob {
            segment_id: segment.id,
            revision,
            start_ms: segment.start_ms,
            samples: if empty { None } else { Some(segment.samples) },
        };
        self.push_final(job);
    }

    fn push_final(&mut self, job: FinalJob) {
        if self.finals.len() >= FINAL_QUEUE_CAPACITY {
            self.stats.finals_dropped_queue_full += 1;
            eprintln!(
                "speech-stt-worker: final for segment {} dropped: decoder is behind",
                job.segment_id
            );
            return;
        }
        self.finals.push_back(job);
    }

    /// Start the next queued final when the decoder is free; skip jobs are
    /// emitted without touching the decoder.
    fn pump_finals(&mut self) -> Option<WorkerExit> {
        while self.inflight.is_none() {
            let job = self.finals.pop_front()?;
            match job.samples {
                None => {
                    if let Err(exit) = self.emit(
                        job.segment_id,
                        job.revision,
                        TranscriptKind::Final,
                        job.start_ms,
                        0,
                        String::new(),
                    ) {
                        return Some(exit);
                    }
                }
                Some(samples) => {
                    self.inflight = Some(Inflight {
                        started: Instant::now(),
                    });
                    let _ = self.job_tx.try_send(Job {
                        segment_id: job.segment_id,
                        kind: TranscriptKind::Final,
                        revision: job.revision,
                        start_ms: job.start_ms,
                        samples,
                    });
                }
            }
        }
        None
    }

    fn on_result(&mut self, result: JobResult) -> Option<WorkerExit> {
        self.inflight = None;
        self.stats.max_decode_ms = self.stats.max_decode_ms.max(result.decode_ms);
        let JobResult { job, text, .. } = result;
        let text = match text {
            Ok(text) => text,
            Err(error) => return Some(WorkerExit::Decoder(error)),
        };
        let covered_samples = job.samples.len();
        match job.kind {
            TranscriptKind::Final => {
                if let Err(exit) = self.emit(
                    job.segment_id,
                    job.revision,
                    TranscriptKind::Final,
                    job.start_ms,
                    covered_samples,
                    text,
                ) {
                    return Some(exit);
                }
                None
            }
            TranscriptKind::Partial => {
                // Only the open segment's partials are worth anything: a
                // segment that ended has a final queued that supersedes them.
                let open_revision = self
                    .segment
                    .as_mut()
                    .filter(|s| s.id == job.segment_id && !s.finalized)
                    .map(|s| {
                        let changed = s.last_partial_text.as_deref() != Some(text.as_str());
                        (s, changed)
                    });
                let Some((segment, changed)) = open_revision else {
                    self.stats.partials_discarded += 1;
                    return None;
                };
                if text.is_empty() || !changed {
                    self.stats.partials_discarded += 1;
                    return None;
                }
                let revision = segment.next_revision;
                segment.next_revision += 1;
                segment.last_partial_text = Some(text.clone());
                if let Err(exit) = self.emit(
                    job.segment_id,
                    revision,
                    TranscriptKind::Partial,
                    job.start_ms,
                    covered_samples,
                    text,
                ) {
                    return Some(exit);
                }
                None
            }
        }
    }

    /// Build, validate and write one transcript line.
    fn emit(
        &mut self,
        segment_id: u64,
        revision: u32,
        kind: TranscriptKind,
        start_ms: u64,
        covered_samples: usize,
        text: String,
    ) -> Result<(), WorkerExit> {
        let covered_audio_ms = (covered_samples * 1000 / WORKER_SAMPLE_RATE_HZ) as u64;
        let mut transcript = SpeechTranscript {
            speaker_id: self.config.speaker_id.clone(),
            segment_id,
            revision,
            kind,
            text,
            covered_audio_ms,
            observed_at_ms: start_ms.saturating_add(covered_audio_ms),
            complete: kind == TranscriptKind::Final,
        };
        let build = |transcript: SpeechTranscript| SpeechTranscriptBatch {
            schema_version: SPEECH_TRANSCRIPT_SCHEMA_VERSION.to_owned(),
            provider_id: self.config.provider_id.clone(),
            provider_version: self.config.provider_version.clone(),
            language: self.config.language.clone(),
            engine: self.config.engine.clone(),
            transcripts: vec![transcript],
        };
        let mut batch = build(transcript.clone());
        if batch.validate().is_err() {
            // The engine's text breaks the wire bounds. Never rewrite it
            // (Q9): drop a partial, and close a final with no text.
            self.stats.texts_rejected += 1;
            if kind == TranscriptKind::Partial {
                return Ok(());
            }
            transcript.text = String::new();
            batch = build(transcript);
            if batch.validate().is_err() {
                return Ok(());
            }
        }
        match kind {
            TranscriptKind::Partial => self.stats.partials_emitted += 1,
            TranscriptKind::Final => self.stats.finals_emitted += 1,
        }
        let mut line =
            serde_json::to_string(&batch).map_err(|e| WorkerExit::Decoder(e.to_string()))?;
        line.push('\n');
        self.write(&line).map_err(|()| WorkerExit::OutputClosed)
    }

    fn write(&mut self, line: &str) -> Result<(), ()> {
        self.output
            .write_all(line.as_bytes())
            .and_then(|()| self.output.flush())
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_stt::protocol::{encode_audio, encode_end, encode_start};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Shared {
        fn transcripts(&self) -> Vec<SpeechTranscript> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .filter(|l| l.contains("\"transcripts\""))
                .map(|l| {
                    SpeechTranscriptBatch::from_json_bytes(l.as_bytes())
                        .expect("valid line")
                        .transcripts
                        .remove(0)
                })
                .collect()
        }
        fn heartbeats(&self) -> usize {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .filter(|l| l.contains("\"heartbeat\""))
                .count()
        }
    }

    /// Decodes to `n<samples>` after a configurable delay; counts calls.
    struct Fake {
        delay: Duration,
        calls: Arc<Mutex<Vec<usize>>>,
        text: Box<dyn FnMut(usize) -> Result<String, String> + Send>,
    }
    impl Decoder for Fake {
        fn decode(&mut self, samples: &[f32]) -> Result<String, String> {
            self.calls.lock().unwrap().push(samples.len());
            thread::sleep(self.delay);
            (self.text)(samples.len())
        }
    }

    fn config() -> WorkerConfig {
        WorkerConfig {
            provider_id: "speech-stt-worker".to_owned(),
            provider_version: "test-1".to_owned(),
            language: "ja".to_owned(),
            speaker_id: "mic-local".to_owned(),
            engine: TranscriptEngine {
                model_id: "fake:1".to_owned(),
                sha256: "0".repeat(64),
            },
            partial_step_ms: 500,
            max_segment_ms: 30_000,
            decode_deadline: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(50),
        }
    }

    fn run(
        frames: Vec<Vec<u8>>,
        config: &WorkerConfig,
        decoder: Fake,
    ) -> (WorkerExit, WorkerStats, Shared) {
        let out = Shared::default();
        let input = Cursor::new(frames.concat());
        let (exit, stats) = run_worker(input, out.clone(), move || Ok(decoder), config);
        (exit, stats, out)
    }

    fn fake(delay_ms: u64) -> (Fake, Arc<Mutex<Vec<usize>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Fake {
                delay: Duration::from_millis(delay_ms),
                calls: calls.clone(),
                text: Box::new(|n| Ok(format!("n{n}"))),
            },
            calls,
        )
    }

    fn audio_ms(ms: usize) -> Vec<Vec<u8>> {
        // 10 ms frames, like the sidecar.
        (0..ms / 10)
            .map(|_| encode_audio(&[0.1; WORKER_SAMPLE_RATE_HZ / 100]))
            .collect()
    }

    #[test]
    fn a_segment_yields_revisions_then_one_final_that_supersedes_them() {
        let (decoder, _) = fake(0);
        let mut frames = vec![encode_start(1, 1_000)];
        frames.extend(audio_ms(2_000));
        frames.push(encode_end(1, false));
        let (exit, stats, out) = run(frames, &config(), decoder);
        assert_eq!(exit, WorkerExit::InputClosed);
        let t = out.transcripts();
        let finals: Vec<_> = t
            .iter()
            .filter(|x| x.kind == TranscriptKind::Final)
            .collect();
        assert_eq!(finals.len(), 1);
        let last = t.last().unwrap();
        assert_eq!(last.kind, TranscriptKind::Final, "the final is last");
        assert!(last.complete && !last.text.is_empty());
        assert_eq!(last.covered_audio_ms, 2_000);
        // Timeline: segment start plus covered audio, on the sidecar's clock.
        assert_eq!(last.observed_at_ms, 1_000 + 2_000);
        // Revisions strictly increase.
        assert!(t.windows(2).all(|w| w[0].revision < w[1].revision));
        assert!(t.iter().all(|x| x.speaker_id == "mic-local"));
        assert!(stats.partials_emitted >= 1);
        assert!(
            t.iter()
                .filter(|x| x.kind == TranscriptKind::Partial)
                .all(|x| !x.complete)
        );
    }

    #[test]
    fn partials_are_at_most_one_per_step_of_new_audio() {
        let (decoder, calls) = fake(0);
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(2_100));
        frames.push(encode_end(1, true));
        let (_, _, _) = run(frames, &config(), decoder);
        // 2.1 s of audio at one decode per 0.5 s: at most 4 partials (the
        // exact count depends on when the decoder was free).
        assert!(
            calls.lock().unwrap().len() <= 4,
            "{:?}",
            calls.lock().unwrap()
        );
    }

    #[test]
    fn a_slow_decoder_drops_partial_requests_and_the_final_still_arrives() {
        let (decoder, calls) = fake(300);
        let mut frames = vec![encode_start(9, 0)];
        frames.extend(audio_ms(3_000));
        frames.push(encode_end(9, false));
        let (exit, stats, out) = run(frames, &config(), decoder);
        assert_eq!(exit, WorkerExit::InputClosed);
        let t = out.transcripts();
        assert_eq!(t.last().unwrap().kind, TranscriptKind::Final);
        assert!(stats.partials_dropped_busy >= 1, "{stats:?}");
        // The final decoded the whole segment.
        assert_eq!(*calls.lock().unwrap().last().unwrap(), 3_000 * 16);
    }

    #[test]
    fn unchanged_text_is_not_re_emitted_as_a_partial() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let decoder = Fake {
            delay: Duration::ZERO,
            calls,
            text: Box::new(|_| Ok("同じ".to_owned())),
        };
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(2_000));
        frames.push(encode_end(1, false));
        let (_, stats, out) = run(frames, &config(), decoder);
        let partials = out
            .transcripts()
            .into_iter()
            .filter(|x| x.kind == TranscriptKind::Partial)
            .count();
        assert_eq!(partials, 1);
        assert!(stats.partials_discarded >= 1);
    }

    #[test]
    fn a_skipped_segment_closes_with_an_empty_final_and_no_decode() {
        let (decoder, calls) = fake(0);
        let frames = vec![
            encode_start(4, 77),
            encode_audio(&[0.1; 160]),
            encode_end(4, true),
        ];
        let (_, _, out) = run(frames, &config(), decoder);
        let t = out.transcripts();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].kind, t[0].text.as_str()), (TranscriptKind::Final, ""));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_segment_cap_force_finalizes_and_drops_the_rest() {
        let (decoder, _) = fake(0);
        let mut cfg = config();
        cfg.max_segment_ms = 1_000;
        let mut frames = vec![encode_start(2, 0)];
        frames.extend(audio_ms(2_500));
        frames.push(encode_end(2, false));
        let (_, stats, out) = run(frames, &cfg, decoder);
        let t = out.transcripts();
        let finals: Vec<_> = t
            .iter()
            .filter(|x| x.kind == TranscriptKind::Final)
            .collect();
        assert_eq!(finals.len(), 1, "one final even though END came later");
        assert_eq!(finals[0].covered_audio_ms, 1_000);
        assert_eq!(stats.segments_force_finalized, 1);
        assert_eq!(stats.audio_samples_dropped, 1_500 * 16);
    }

    #[test]
    fn a_start_while_a_segment_is_open_finalizes_the_old_one_first() {
        let (decoder, _) = fake(0);
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(600));
        frames.push(encode_start(2, 5_000));
        frames.extend(audio_ms(300));
        frames.push(encode_end(2, false));
        let (_, _, out) = run(frames, &config(), decoder);
        let finals: Vec<_> = out
            .transcripts()
            .into_iter()
            .filter(|x| x.kind == TranscriptKind::Final)
            .collect();
        assert_eq!(
            finals.iter().map(|f| f.segment_id).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn audio_outside_a_segment_is_dropped_and_counted() {
        let (decoder, calls) = fake(0);
        let (_, stats, out) = run(audio_ms(100), &config(), decoder);
        assert!(out.transcripts().is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(stats.audio_samples_dropped, 100 * 16);
    }

    #[test]
    fn a_decode_past_the_deadline_ends_the_worker() {
        let (decoder, _) = fake(1_500);
        let mut cfg = config();
        cfg.decode_deadline = Duration::from_millis(200);
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(700));
        // Keep the pipe open past the deadline: a reader that never sends
        // END or closes.
        struct Hang(Cursor<Vec<u8>>);
        impl Read for Hang {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.read(buf)?;
                if n == 0 {
                    thread::sleep(Duration::from_secs(3));
                }
                Ok(n)
            }
        }
        let out = Shared::default();
        let (exit, _) = run_worker(
            Hang(Cursor::new(frames.concat())),
            out,
            move || Ok(decoder),
            &cfg,
        );
        assert_eq!(exit, WorkerExit::DecodeDeadline);
        assert_eq!(exit.exit_code(), EXIT_DECODE_DEADLINE);
    }

    #[test]
    fn heartbeats_flow_while_the_decoder_is_busy() {
        let (decoder, _) = fake(400);
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(600));
        frames.push(encode_end(1, false));
        let (_, _, out) = run(frames, &config(), decoder);
        // 50 ms interval over a run of at least 400 ms of decoding.
        assert!(out.heartbeats() >= 4, "{}", out.heartbeats());
    }

    #[test]
    fn decoder_failures_and_protocol_errors_are_distinct_exits() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let failing = Fake {
            delay: Duration::ZERO,
            calls,
            text: Box::new(|_| Err("boom".to_owned())),
        };
        let mut frames = vec![encode_start(1, 0)];
        frames.extend(audio_ms(100));
        frames.push(encode_end(1, false));
        let (exit, _, _) = run(frames, &config(), failing);
        assert_eq!(exit, WorkerExit::Decoder("boom".to_owned()));

        let (decoder, _) = fake(0);
        let (exit, _, _) = run(vec![vec![0x7f, 0, 0, 0, 0]], &config(), decoder);
        assert!(matches!(exit, WorkerExit::Protocol(_)));
        assert_eq!(exit.exit_code(), EXIT_PROTOCOL);

        let out = Shared::default();
        let (exit, _) = run_worker(
            Cursor::new(Vec::new()),
            out,
            || Err::<Fake, _>("no model".to_owned()),
            &config(),
        );
        assert_eq!(exit, WorkerExit::Decoder("no model".to_owned()));
    }

    #[test]
    fn engine_text_is_emitted_unmodified_and_out_of_bounds_text_is_not_rewritten() {
        // Spaces, punctuation and mixed width survive verbatim.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let decoder = Fake {
            delay: Duration::ZERO,
            calls,
            text: Box::new(|_| Ok(" ｱｲｳ、Ｈello 123！ ".to_owned())),
        };
        let frames = vec![
            encode_start(1, 0),
            encode_audio(&[0.1; 1_600]),
            encode_end(1, false),
        ];
        let (_, _, out) = run(frames, &config(), decoder);
        assert_eq!(
            out.transcripts().last().unwrap().text,
            " ｱｲｳ、Ｈello 123！ "
        );

        // 1001 characters: counted, replaced by an empty final, never cut.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let decoder = Fake {
            delay: Duration::ZERO,
            calls,
            text: Box::new(|_| Ok("あ".repeat(1_001))),
        };
        let frames = vec![
            encode_start(1, 0),
            encode_audio(&[0.1; 1_600]),
            encode_end(1, false),
        ];
        let (_, stats, out) = run(frames, &config(), decoder);
        let t = out.transcripts();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].text, "");
        assert_eq!(stats.texts_rejected, 1);
    }

    #[test]
    fn configuration_bounds_are_enforced() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.max_segment_ms = 30_001;
        assert!(c.validate().is_err());
        let mut c = config();
        c.partial_step_ms = 10;
        assert!(c.validate().is_err());
    }
}
