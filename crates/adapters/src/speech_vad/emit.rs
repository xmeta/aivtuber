use crate::speech_vad::clock::ClockError;
use crate::speech_vad::pipeline::RawEdge;
use aivtuber_domain::{
    MAX_SPEECH_OBSERVATION_BATCH_BYTES, MAX_SPEECH_OBSERVATIONS_PER_BATCH,
    SPEECH_OBSERVATION_SCHEMA_VERSION, SpeechActivityObservation, SpeechObservation,
    SpeechObservationBatch,
};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub struct StamperConfig {
    pub provider_id: String,
    pub provider_version: String,
    /// Declared by the operator, never inferred.
    pub language: String,
    /// Pseudonymous local speaker id (not a name, not a voiceprint).
    pub speaker_id: String,
}

/// Turns raw edges into schema-valid `speech-observation` batch lines. No
/// turn-end scores are produced: Smart Turn is not adopted (EXP-0001).
#[derive(Debug, Clone)]
pub struct ObservationStamper {
    config: StamperConfig,
}

impl ObservationStamper {
    pub fn new(config: StamperConfig) -> Result<Self, String> {
        // Validate the constant fields once, with a throwaway observation.
        let probe = Self::build(
            &config,
            vec![SpeechObservation::SpeechActivity(
                SpeechActivityObservation {
                    speaker_id: config.speaker_id.clone(),
                    edge: aivtuber_domain::SpeechEdge::Start,
                    observed_at_ms: 0,
                    expires_at_ms: None,
                    confidence: None,
                },
            )],
        );
        probe.validate().map_err(|error| error.to_string())?;
        Ok(Self { config })
    }

    /// One JSON line (with trailing newline) per at most 32 edges. `map`
    /// converts a 16 kHz stream position to runtime milliseconds.
    pub fn lines(
        &self,
        edges: &[RawEdge],
        map: impl Fn(u64) -> Result<u64, ClockError>,
    ) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        for group in edges.chunks(MAX_SPEECH_OBSERVATIONS_PER_BATCH) {
            let mut observations = Vec::with_capacity(group.len());
            for edge in group {
                observations.push(SpeechObservation::SpeechActivity(
                    SpeechActivityObservation {
                        speaker_id: self.config.speaker_id.clone(),
                        edge: edge.edge,
                        observed_at_ms: map(edge.sample16).map_err(|e| e.to_string())?,
                        expires_at_ms: None,
                        confidence: edge.confidence.map(round3),
                    },
                ));
            }
            let batch = Self::build(&self.config, observations);
            batch.validate().map_err(|e| e.to_string())?;
            let mut line = serde_json::to_string(&batch).map_err(|e| e.to_string())?;
            if line.len() >= MAX_SPEECH_OBSERVATION_BATCH_BYTES {
                return Err("serialized batch exceeds the contract size".to_owned());
            }
            line.push('\n');
            lines.push(line);
        }
        Ok(lines)
    }

    fn build(
        config: &StamperConfig,
        observations: Vec<SpeechObservation>,
    ) -> SpeechObservationBatch {
        SpeechObservationBatch {
            schema_version: SPEECH_OBSERVATION_SCHEMA_VERSION.to_owned(),
            provider_id: config.provider_id.clone(),
            provider_version: config.provider_version.clone(),
            language: config.language.clone(),
            observations,
        }
    }
}

fn round3(value: f32) -> f64 {
    (f64::from(value) * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkStats {
    pub lines_written: u64,
    pub dropped_queue_full: u64,
    pub closed: bool,
}

#[derive(Default)]
struct SinkShared {
    written: AtomicU64,
    dropped: AtomicU64,
    closed: AtomicBool,
}

/// Bounded, non-blocking line output. A slow or stalled reader costs dropped
/// lines (counted), never an unbounded queue and never a blocked caller. A
/// closed reader (the parent exited) is reported through `is_closed`.
pub struct LineSink {
    sender: Option<SyncSender<String>>,
    shared: Arc<SinkShared>,
    writer: Option<JoinHandle<()>>,
}

impl LineSink {
    pub fn spawn<W: Write + Send + 'static>(mut out: W, capacity: usize) -> Self {
        let (sender, receiver) = sync_channel::<String>(capacity.max(1));
        let shared = Arc::new(SinkShared::default());
        let thread_shared = shared.clone();
        let writer = std::thread::spawn(move || {
            for line in receiver {
                if out
                    .write_all(line.as_bytes())
                    .and_then(|()| out.flush())
                    .is_err()
                {
                    thread_shared.closed.store(true, Ordering::Relaxed);
                    return;
                }
                thread_shared.written.fetch_add(1, Ordering::Relaxed);
            }
        });
        Self {
            sender: Some(sender),
            shared,
            writer: Some(writer),
        }
    }

    /// Drop-newest when full, so already queued edges keep their order.
    pub fn send(&self, line: String) {
        let Some(sender) = &self.sender else { return };
        match sender.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.closed.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> SinkStats {
        SinkStats {
            lines_written: self.shared.written.load(Ordering::Relaxed),
            dropped_queue_full: self.shared.dropped.load(Ordering::Relaxed),
            closed: self.is_closed(),
        }
    }

    /// Flush queued lines and stop the writer. If the reader stalled forever
    /// the writer thread is left detached rather than blocking shutdown.
    pub fn finish(mut self, wait: std::time::Duration) -> SinkStats {
        self.sender = None;
        if let Some(writer) = self.writer.take() {
            let deadline = std::time::Instant::now() + wait;
            while !writer.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if writer.is_finished() {
                let _ = writer.join();
            }
        }
        self.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::SpeechEdge;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    fn stamper() -> ObservationStamper {
        ObservationStamper::new(StamperConfig {
            provider_id: "silero-vad-sidecar".into(),
            provider_version: "silero-vad-v6.2.3-ifless".into(),
            language: "ja".into(),
            speaker_id: "mic-local".into(),
        })
        .unwrap()
    }

    fn edge(kind: SpeechEdge, sample16: u64) -> RawEdge {
        RawEdge {
            edge: kind,
            sample16,
            confidence: Some(0.87654),
        }
    }

    #[test]
    fn lines_are_schema_valid_batches_with_runtime_times() {
        let lines = stamper()
            .lines(
                &[
                    edge(SpeechEdge::Start, 16_000),
                    edge(SpeechEdge::End, 32_000),
                ],
                |s| Ok(5_000 + s / 16),
            )
            .unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with('\n'));
        let batch =
            SpeechObservationBatch::from_json_bytes(lines[0].trim_end().as_bytes()).unwrap();
        assert_eq!(batch.language, "ja");
        let times: Vec<u64> = batch
            .observations
            .iter()
            .map(|o| o.observed_at_ms())
            .collect();
        assert_eq!(times, vec![6_000, 7_000]);
        match &batch.observations[0] {
            SpeechObservation::SpeechActivity(a) => assert_eq!(a.confidence, Some(0.877)),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!lines[0].contains("turn_end_score"));
    }

    #[test]
    fn more_than_one_batch_of_edges_is_split_and_unsynced_clock_fails_closed() {
        let many: Vec<RawEdge> = (0..70).map(|i| edge(SpeechEdge::Start, i * 512)).collect();
        let lines = stamper().lines(&many, |s| Ok(s / 16)).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(
            stamper()
                .lines(&many, |_| Err(ClockError::NotSynced))
                .is_err()
        );
    }

    #[test]
    fn invalid_configuration_is_refused() {
        let mut bad = StamperConfig {
            provider_id: "ok".into(),
            provider_version: "1".into(),
            language: "Japanese!".into(),
            speaker_id: "mic".into(),
        };
        assert!(ObservationStamper::new(bad.clone()).is_err());
        bad.language = "ja".into();
        bad.speaker_id = "has space".into();
        assert!(ObservationStamper::new(bad).is_err());
    }

    struct Gate(mpsc::Receiver<()>, Arc<Mutex<Vec<u8>>>);
    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            self.1.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_is_bounded_when_the_reader_stalls_and_the_caller_never_blocks() {
        let (open_gate, gate) = mpsc::channel();
        let sink_out = Arc::new(Mutex::new(Vec::new()));
        let sink = LineSink::spawn(Gate(gate, sink_out.clone()), 8);
        let started = std::time::Instant::now();
        for i in 0..10_000 {
            sink.send(format!("line {i}\n"));
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        let stats = sink.stats();
        // 8 queued plus at most one in the writer's hands.
        assert!(stats.dropped_queue_full >= 10_000 - 9, "{stats:?}");
        for _ in 0..16 {
            let _ = open_gate.send(());
        }
        let stats = sink.finish(Duration::from_secs(2));
        assert!(stats.lines_written <= 9);
        let text = String::from_utf8(sink_out.lock().unwrap().clone()).unwrap();
        // Drop-newest: the oldest lines are the ones that survive, in order.
        assert!(text.starts_with("line 0\nline 1\n"));
    }

    #[test]
    fn a_closed_reader_is_reported() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = LineSink::spawn(Broken, 4);
        sink.send("x\n".into());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sink.is_closed() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(sink.is_closed());
    }
}
