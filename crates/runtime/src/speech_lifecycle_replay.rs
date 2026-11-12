//! Recording and deterministic replay of speech lifecycle events (issue #222
//! S3, ADR-0004 D5/Q14).
//!
//! Sibling of `speech_replay` and `speech_transcript_replay`. A
//! [`SpeechLifecycleRecorder`] retains the events the composition root drains
//! from the scheduler, in the log's monotonic `seq` order (never wall-clock
//! order). The retained form is the event itself: identities, phases, logical
//! times, and the SHA-256 digest of the text sent to TTS with the projection
//! identity. The event type has no text field and the recorder re-validates
//! every event against the wire contract, so raw reply or speech text cannot
//! enter a record.
//!
//! [`SpeechLifecycleReplayFixture`] feeds recorded events through the recorder
//! and the scheduler's [`OwnSpeechTracker`] with expectations that fail
//! loudly, and pins the SHA-256 of the retained records so a change in the
//! recorded form is a visible divergence.

use aivtuber_domain::{OwnSpeech, SpeechLifecycleEvent};
use aivtuber_scheduler::{OwnSpeechMetrics, OwnSpeechTracker};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

pub const SPEECH_LIFECYCLE_REPLAY_SCHEMA_VERSION: &str = "0.1.0";
/// Default and largest number of retained records.
pub const DEFAULT_LIFECYCLE_RECORD_CAPACITY: usize = 4_096;
pub const MAX_LIFECYCLE_RECORD_CAPACITY: usize = 65_536;
/// Bounds a fixture so a hostile or broken file cannot run unbounded.
pub const MAX_SPEECH_LIFECYCLE_REPLAY_STEPS: usize = 4_096;

/// Why an event was not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleRecordRejection {
    /// The event does not satisfy the wire contract.
    Invalid,
    /// `seq` is not newer than the newest retained event: replay order is the
    /// log's logical sequence, so an out-of-order event is refused.
    NotMonotonic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleRecorderMetrics {
    pub recorded: u64,
    pub rejected_invalid: u64,
    pub rejected_not_monotonic: u64,
    /// Events whose `seq` skipped ahead (the scheduler's queue dropped some).
    pub gaps: u64,
    /// Oldest records evicted to stay within capacity.
    pub evicted: u64,
    pub retained: usize,
}

/// Bounded, ordered retention of lifecycle events for replay.
#[derive(Debug)]
pub struct SpeechLifecycleRecorder {
    records: VecDeque<SpeechLifecycleEvent>,
    capacity: usize,
    last_seq: Option<u64>,
    metrics: LifecycleRecorderMetrics,
}

impl SpeechLifecycleRecorder {
    /// A recorder keeping the newest `capacity` records (clamped to
    /// `1..=MAX_LIFECYCLE_RECORD_CAPACITY`).
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::new(),
            capacity: capacity.clamp(1, MAX_LIFECYCLE_RECORD_CAPACITY),
            last_seq: None,
            metrics: LifecycleRecorderMetrics::default(),
        }
    }

    pub fn record(&mut self, event: &SpeechLifecycleEvent) -> Result<(), LifecycleRecordRejection> {
        if event.validate().is_err() {
            self.metrics.rejected_invalid += 1;
            return Err(LifecycleRecordRejection::Invalid);
        }
        if let Some(last) = self.last_seq {
            if event.seq <= last {
                self.metrics.rejected_not_monotonic += 1;
                return Err(LifecycleRecordRejection::NotMonotonic);
            }
            if event.seq > last + 1 {
                self.metrics.gaps += 1;
            }
        }
        self.last_seq = Some(event.seq);
        if self.records.len() >= self.capacity {
            self.records.pop_front();
            self.metrics.evicted += 1;
        }
        self.records.push_back(event.clone());
        self.metrics.recorded += 1;
        Ok(())
    }

    pub fn records(&self) -> impl Iterator<Item = &SpeechLifecycleEvent> {
        self.records.iter()
    }

    pub fn metrics(&self) -> LifecycleRecorderMetrics {
        LifecycleRecorderMetrics {
            retained: self.records.len(),
            ..self.metrics
        }
    }

    /// Lowercase-hex SHA-256 over the retained records, one compact JSON line
    /// each. Two runs that produce the same facts in the same order have the
    /// same digest.
    pub fn records_digest(&self) -> String {
        let mut hasher = Sha256::new();
        for record in &self.records {
            // A validated event always serializes.
            if let Ok(line) = serde_json::to_vec(record) {
                hasher.update(&line);
                hasher.update(b"\n");
            }
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// One step: events recorded and observed in order, then expectations.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechLifecycleReplayStep {
    pub events: Vec<SpeechLifecycleEvent>,
    /// `speaking` or `idle`, as the floor sees the performer after the step.
    pub expect_own: Option<String>,
    pub expect_awaiting_next_phrase: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechLifecycleReplayFixture {
    pub schema_version: String,
    pub name: String,
    pub capacity: Option<usize>,
    pub steps: Vec<SpeechLifecycleReplayStep>,
    /// Pinned SHA-256 of the retained records after the last step.
    pub expect_records_digest: Option<String>,
    pub expect_recorded: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SpeechLifecycleReplayReport {
    /// What the floor saw after each step.
    pub own: Vec<OwnSpeech>,
    pub records_digest: String,
    pub recorder: LifecycleRecorderMetrics,
    pub tracker: OwnSpeechMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechLifecycleReplayError {
    Fixture(String),
    Rejected {
        step: usize,
        seq: u64,
        rejection: LifecycleRecordRejection,
    },
    Mismatch {
        step: usize,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for SpeechLifecycleReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixture(message) => write!(f, "invalid lifecycle replay fixture: {message}"),
            Self::Rejected {
                step,
                seq,
                rejection,
            } => write!(f, "step {step}: event seq {seq} rejected: {rejection:?}"),
            Self::Mismatch {
                step,
                expected,
                actual,
            } => write!(f, "step {step}: expected {expected}, got {actual}"),
        }
    }
}

impl Error for SpeechLifecycleReplayError {}

fn own_name(own: OwnSpeech) -> &'static str {
    match own {
        OwnSpeech::Idle => "idle",
        OwnSpeech::Speaking => "speaking",
    }
}

impl SpeechLifecycleReplayFixture {
    pub fn from_json(text: &str) -> Result<Self, SpeechLifecycleReplayError> {
        let fixture: Self = serde_json::from_str(text)
            .map_err(|error| SpeechLifecycleReplayError::Fixture(error.to_string()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), SpeechLifecycleReplayError> {
        let bad = |message: &str| Err(SpeechLifecycleReplayError::Fixture(message.to_owned()));
        if self.schema_version != SPEECH_LIFECYCLE_REPLAY_SCHEMA_VERSION {
            return bad("unsupported schema_version");
        }
        if self.steps.is_empty() || self.steps.len() > MAX_SPEECH_LIFECYCLE_REPLAY_STEPS {
            return bad("steps must be 1..=4096");
        }
        if self
            .capacity
            .is_some_and(|c| c == 0 || c > MAX_LIFECYCLE_RECORD_CAPACITY)
        {
            return bad("capacity must be 1..=65536");
        }
        Ok(())
    }

    pub fn run(&self) -> Result<SpeechLifecycleReplayReport, SpeechLifecycleReplayError> {
        let mut recorder = SpeechLifecycleRecorder::new(
            self.capacity.unwrap_or(DEFAULT_LIFECYCLE_RECORD_CAPACITY),
        );
        let mut tracker = OwnSpeechTracker::new();
        let mut own = Vec::with_capacity(self.steps.len());
        for (step_index, step) in self.steps.iter().enumerate() {
            for event in &step.events {
                recorder.record(event).map_err(|rejection| {
                    SpeechLifecycleReplayError::Rejected {
                        step: step_index,
                        seq: event.seq,
                        rejection,
                    }
                })?;
                tracker.observe(event);
            }
            let now = tracker.own();
            let mismatch = |expected: String, actual: String| {
                Err(SpeechLifecycleReplayError::Mismatch {
                    step: step_index,
                    expected,
                    actual,
                })
            };
            if let Some(expected) = &step.expect_own
                && expected != own_name(now)
            {
                return mismatch(expected.clone(), own_name(now).to_owned());
            }
            if let Some(expected) = step.expect_awaiting_next_phrase
                && expected != tracker.awaiting_next_phrase()
            {
                return mismatch(
                    format!("awaiting_next_phrase={expected}"),
                    format!("awaiting_next_phrase={}", tracker.awaiting_next_phrase()),
                );
            }
            own.push(now);
        }
        let last = self.steps.len() - 1;
        let digest = recorder.records_digest();
        if let Some(expected) = &self.expect_records_digest
            && expected != &digest
        {
            return Err(SpeechLifecycleReplayError::Mismatch {
                step: last,
                expected: expected.clone(),
                actual: digest,
            });
        }
        if let Some(expected) = self.expect_recorded
            && expected != recorder.metrics().recorded
        {
            return Err(SpeechLifecycleReplayError::Mismatch {
                step: last,
                expected: format!("recorded={expected}"),
                actual: format!("recorded={}", recorder.metrics().recorded),
            });
        }
        Ok(SpeechLifecycleReplayReport {
            own,
            records_digest: digest,
            recorder: recorder.metrics(),
            tracker: tracker.metrics(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fixture_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/speech-lifecycle/replay")
    }

    #[test]
    fn every_checked_in_fixture_replays_identically_twice() {
        let mut count = 0;
        for entry in fs::read_dir(fixture_dir()).expect("fixture dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read");
            let fixture = SpeechLifecycleReplayFixture::from_json(&text).expect("fixture");
            assert!(
                fixture.expect_records_digest.is_some(),
                "{path:?} pins a digest"
            );
            let first = fixture.run().unwrap_or_else(|e| panic!("{path:?}: {e}"));
            let second = fixture.run().expect("second run");
            assert_eq!(first.records_digest, second.records_digest, "{path:?}");
            assert_eq!(first.own, second.own, "{path:?}");
            count += 1;
        }
        assert!(count >= 3, "expected the lifecycle fixtures, found {count}");
    }

    fn event(seq: u64, phase: &str) -> SpeechLifecycleEvent {
        let generation = if phase == "prepared" {
            String::new()
        } else {
            r#","generation":1"#.to_owned()
        };
        let json = format!(
            r#"{{"schema_version":"0.1.0","seq":{seq},"at_ms":{seq},"basis":"logical_timeline","speech":{{"event_id":"e","phrase":0,"final_phrase":true,"asset_id":"a"{generation}}},"phase":{{"kind":"{phase}"}}}}"#
        );
        SpeechLifecycleEvent::from_json_bytes(json.as_bytes()).expect("event")
    }

    #[test]
    fn recording_is_ordered_by_seq_and_refuses_disorder() {
        let mut recorder = SpeechLifecycleRecorder::new(8);
        recorder.record(&event(1, "prepared")).expect("1");
        recorder.record(&event(2, "committed")).expect("2");
        assert_eq!(
            recorder.record(&event(2, "started")),
            Err(LifecycleRecordRejection::NotMonotonic)
        );
        assert_eq!(
            recorder.record(&event(1, "started")),
            Err(LifecycleRecordRejection::NotMonotonic)
        );
        recorder.record(&event(5, "started")).expect("gap is kept");
        let m = recorder.metrics();
        assert_eq!((m.recorded, m.rejected_not_monotonic, m.gaps), (3, 2, 1));
    }

    #[test]
    fn retention_is_bounded_and_keeps_the_newest() {
        let mut recorder = SpeechLifecycleRecorder::new(2);
        for seq in 1..=4 {
            recorder.record(&event(seq, "started")).expect("record");
        }
        let seqs: Vec<u64> = recorder.records().map(|r| r.seq).collect();
        assert_eq!(seqs, [3, 4]);
        assert_eq!(recorder.metrics().evicted, 2);
    }

    #[test]
    fn an_invalid_event_is_not_retained() {
        let mut recorder = SpeechLifecycleRecorder::new(4);
        let mut bad = event(1, "prepared");
        bad.speech.speech_text_digest = Some("こんにちは".to_owned());
        assert_eq!(
            recorder.record(&bad),
            Err(LifecycleRecordRejection::Invalid)
        );
        assert_eq!(recorder.metrics().retained, 0);
    }

    #[test]
    fn the_retained_form_carries_a_digest_and_never_text() {
        let mut recorder = SpeechLifecycleRecorder::new(4);
        let mut prepared = event(1, "prepared");
        prepared.speech.speech_text_digest = Some("a".repeat(64));
        prepared.speech.projection = Some("rules-1+lexicon-abc".to_owned());
        recorder.record(&prepared).expect("record");
        let line = serde_json::to_string(recorder.records().next().expect("one")).expect("json");
        assert!(line.contains("speech_text_digest"));
        assert!(!line.contains("\"text\""), "{line}");
    }

    #[test]
    fn malformed_fixtures_are_refused() {
        for text in [
            r#"{"schema_version":"9","name":"x","steps":[{"events":[]}]}"#,
            r#"{"schema_version":"0.1.0","name":"x","steps":[]}"#,
            r#"{"schema_version":"0.1.0","name":"x","steps":[{"events":[],"unknown":1}]}"#,
            r#"{"schema_version":"0.1.0","name":"x","capacity":0,"steps":[{"events":[]}]}"#,
        ] {
            assert!(
                SpeechLifecycleReplayFixture::from_json(text).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn a_diverging_digest_or_expectation_fails_loudly() {
        let text =
            fs::read_to_string(fixture_dir().join("phrased-reply-barge-in.json")).expect("fixture");
        let mut fixture = SpeechLifecycleReplayFixture::from_json(&text).expect("fixture");
        fixture.expect_records_digest = Some("0".repeat(64));
        assert!(matches!(
            fixture.run(),
            Err(SpeechLifecycleReplayError::Mismatch { .. })
        ));
    }
}
