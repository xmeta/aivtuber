//! Deterministic replay of speech transcripts (issue #92, S3b).
//!
//! Sibling of `speech_replay`: a checked-in JSON file under
//! `examples/speech-transcripts/replay`, logical time only, a pure run and
//! expectations that fail loudly. Transcript consumers (floor policy, the
//! normalizer path) can be tested here with no microphone, audio or model.

use crate::{
    IngressPolicy, SpeechTranscriptConfig, SpeechTranscriptMetrics, SpeechTranscriptTracker,
    SurfacedFinal, TranscriptBatchRejection,
};
use aivtuber_domain::SpeechTranscriptBatch;
use serde::Deserialize;
use std::error::Error;
use std::fmt;

pub const SPEECH_TRANSCRIPT_REPLAY_SCHEMA_VERSION: &str = "0.1.0";
/// Bounds a fixture so a hostile or broken file cannot run unbounded.
pub const MAX_SPEECH_TRANSCRIPT_REPLAY_STEPS: usize = 4_096;
/// Finals drained by one `take_finals` step.
const REPLAY_TAKE_LIMIT: usize = 256;

/// Optional overrides of [`SpeechTranscriptConfig`]; everything else keeps the
/// production default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscriptReplayConfig {
    pub partial_max_age_ms: Option<u64>,
    pub final_max_age_ms: Option<u64>,
    pub max_speakers: Option<usize>,
    pub floor_hold_ms: Option<u64>,
    pub max_pending_finals: Option<usize>,
}

/// One step at a logical time. Exactly one of `deliver`, `poll_floor` or
/// `take_finals`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscriptReplayStep {
    pub now_ms: u64,
    pub deliver: Option<SpeechTranscriptBatch>,
    /// Speaker id to read the partial-derived floor hold for.
    pub poll_floor: Option<String>,
    /// Drain the surfaced finals.
    pub take_finals: Option<bool>,
    /// For `deliver`: the outcome name of each transcript, in order.
    pub expect_outcomes: Option<Vec<String>>,
    /// For `poll_floor`: `FloorHold::name` (`none`, `holding`, `stale`).
    pub expect_floor: Option<String>,
    /// For `take_finals`: the exact texts surfaced, oldest first.
    pub expect_finals: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscriptReplayFixture {
    pub schema_version: String,
    pub name: String,
    #[serde(default)]
    pub config: SpeechTranscriptReplayConfig,
    pub steps: Vec<SpeechTranscriptReplayStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechTranscriptReplayFloor {
    pub now_ms: u64,
    pub speaker_id: String,
    pub floor: &'static str,
}

#[derive(Debug, Clone)]
pub struct SpeechTranscriptReplayReport {
    /// Every floor poll, in step order.
    pub floors: Vec<SpeechTranscriptReplayFloor>,
    /// Every surfaced final, in the order they were taken.
    pub finals: Vec<SurfacedFinal>,
    pub metrics: SpeechTranscriptMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechTranscriptReplayError {
    Fixture(String),
    BatchRejected {
        step: usize,
        rejection: TranscriptBatchRejection,
    },
    Mismatch {
        step: usize,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for SpeechTranscriptReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixture(message) => {
                write!(f, "invalid speech transcript replay fixture: {message}")
            }
            Self::BatchRejected { step, rejection } => {
                write!(f, "step {step}: batch rejected: {rejection:?}")
            }
            Self::Mismatch {
                step,
                expected,
                actual,
            } => write!(f, "step {step}: expected {expected}, got {actual}"),
        }
    }
}

impl Error for SpeechTranscriptReplayError {}

impl SpeechTranscriptReplayFixture {
    pub fn from_json(text: &str) -> Result<Self, SpeechTranscriptReplayError> {
        let fixture: Self = serde_json::from_str(text)
            .map_err(|error| SpeechTranscriptReplayError::Fixture(error.to_string()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), SpeechTranscriptReplayError> {
        let bad = |message: &str| Err(SpeechTranscriptReplayError::Fixture(message.to_owned()));
        if self.schema_version != SPEECH_TRANSCRIPT_REPLAY_SCHEMA_VERSION {
            return bad("unsupported schema_version");
        }
        if self.steps.is_empty() || self.steps.len() > MAX_SPEECH_TRANSCRIPT_REPLAY_STEPS {
            return bad("steps must be 1..=4096");
        }
        let mut previous = 0;
        for step in &self.steps {
            let actions = usize::from(step.deliver.is_some())
                + usize::from(step.poll_floor.is_some())
                + usize::from(step.take_finals == Some(true));
            if actions != 1 || step.take_finals == Some(false) {
                return bad("each step needs exactly one of deliver, poll_floor or take_finals");
            }
            // Replay ordering is logical time; it never goes backwards.
            if step.now_ms < previous {
                return bad("now_ms must not decrease");
            }
            previous = step.now_ms;
        }
        Ok(())
    }

    fn tracker_config(&self) -> SpeechTranscriptConfig {
        let mut config = SpeechTranscriptConfig::default();
        let c = &self.config;
        if let Some(v) = c.partial_max_age_ms {
            config.partial_max_age_ms = v;
        }
        if let Some(v) = c.final_max_age_ms {
            config.final_max_age_ms = v;
        }
        if let Some(v) = c.max_speakers {
            config.max_speakers = v;
        }
        if let Some(v) = c.floor_hold_ms {
            config.floor_hold_ms = v;
        }
        if let Some(v) = c.max_pending_finals {
            config.max_pending_finals = v;
        }
        config
    }

    /// Run the fixture under `ingress_policy`. Pure: identical input gives an
    /// identical report.
    pub fn run(
        &self,
        ingress_policy: IngressPolicy,
    ) -> Result<SpeechTranscriptReplayReport, SpeechTranscriptReplayError> {
        let mut tracker = SpeechTranscriptTracker::new(self.tracker_config(), ingress_policy)
            .map_err(|error| SpeechTranscriptReplayError::Fixture(error.to_string()))?;
        let mut floors = Vec::new();
        let mut finals = Vec::new();
        let mismatch = |step: usize, expected: String, actual: String| {
            Err(SpeechTranscriptReplayError::Mismatch {
                step,
                expected,
                actual,
            })
        };
        for (index, step) in self.steps.iter().enumerate() {
            if let Some(batch) = &step.deliver {
                let report = tracker
                    .ingest_batch(batch, step.now_ms)
                    .map_err(|rejection| SpeechTranscriptReplayError::BatchRejected {
                        step: index,
                        rejection,
                    })?;
                if let Some(expected) = &step.expect_outcomes {
                    let actual: Vec<&str> = report.outcomes.iter().map(|o| o.name()).collect();
                    if actual != *expected {
                        return mismatch(index, format!("{expected:?}"), format!("{actual:?}"));
                    }
                }
            }
            if let Some(speaker) = &step.poll_floor {
                let floor = tracker.floor_hold(speaker, step.now_ms).name();
                if let Some(expected) = &step.expect_floor
                    && expected != floor
                {
                    return mismatch(index, expected.clone(), floor.to_owned());
                }
                floors.push(SpeechTranscriptReplayFloor {
                    now_ms: step.now_ms,
                    speaker_id: speaker.clone(),
                    floor,
                });
            }
            if step.take_finals == Some(true) {
                let taken = tracker.take_finals(REPLAY_TAKE_LIMIT);
                if let Some(expected) = &step.expect_finals {
                    let actual: Vec<&str> = taken.iter().map(|f| f.text.as_str()).collect();
                    if actual != *expected {
                        return mismatch(index, format!("{expected:?}"), format!("{actual:?}"));
                    }
                }
                finals.extend(taken);
            }
        }
        Ok(SpeechTranscriptReplayReport {
            floors,
            finals,
            metrics: tracker.metrics(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{SecurityPlane, SourceClass, TrustLevel};

    const REVISIONS_THEN_FINAL: &str =
        include_str!("../../../examples/speech-transcripts/replay/revisions-then-final.json");
    const DISORDER_AND_STALE: &str =
        include_str!("../../../examples/speech-transcripts/replay/disorder-and-stale.json");
    const HOSTILE_TEXT: &str =
        include_str!("../../../examples/speech-transcripts/replay/hostile-text.json");

    #[test]
    fn partials_hold_the_floor_and_only_the_final_surfaces() {
        let fixture =
            SpeechTranscriptReplayFixture::from_json(REVISIONS_THEN_FINAL).expect("fixture");
        let report = fixture.run(IngressPolicy::default()).expect("replay");
        assert_eq!(report.finals.len(), 1);
        assert_eq!(
            report.finals[0].provenance.source_class,
            SourceClass::Speech
        );
        assert_eq!(report.finals[0].provenance.plane, SecurityPlane::Content);
        assert_eq!(
            report.finals[0].provenance.trust_level,
            TrustLevel::Untrusted
        );
        assert!(report.floors.iter().any(|f| f.floor == "holding"));
        assert_eq!(report.floors.last().map(|f| f.floor), Some("none"));
        assert!(report.metrics.partials_accepted >= 3);
        // Deterministic: a second run is identical.
        let again = fixture.run(IngressPolicy::default()).unwrap();
        assert_eq!(report.floors, again.floors);
        assert_eq!(report.finals, again.finals);
        assert_eq!(report.metrics, again.metrics);
    }

    #[test]
    fn disorder_stale_and_late_revisions_are_rejected_without_surfacing_text() {
        let fixture =
            SpeechTranscriptReplayFixture::from_json(DISORDER_AND_STALE).expect("fixture");
        let report = fixture.run(IngressPolicy::default()).expect("replay");
        let m = report.metrics;
        assert!(m.rejected_stale_revision >= 1);
        assert!(m.rejected_out_of_order >= 1);
        assert!(m.rejected_stale_age >= 1);
        assert!(m.rejected_after_final >= 1);
        assert_eq!(m.finals_surfaced, 1);
    }

    #[test]
    fn a_hostile_transcript_is_surfaced_only_as_untrusted_content() {
        let fixture = SpeechTranscriptReplayFixture::from_json(HOSTILE_TEXT).expect("fixture");
        let report = fixture.run(IngressPolicy::default()).expect("replay");
        assert_eq!(report.finals.len(), 1);
        assert_eq!(
            report.finals[0].provenance.trust_level,
            TrustLevel::Untrusted
        );
        assert_eq!(report.finals[0].provenance.plane, SecurityPlane::Content);
    }

    #[test]
    fn fixture_files_cannot_carry_trust_or_priority() {
        let hostile =
            REVISIONS_THEN_FINAL.replacen("\"kind\"", "\"trust_level\": \"trusted\", \"kind\"", 1);
        assert!(SpeechTranscriptReplayFixture::from_json(&hostile).is_err());
    }

    #[test]
    fn a_policy_without_speech_refuses_the_replay() {
        let fixture = SpeechTranscriptReplayFixture::from_json(REVISIONS_THEN_FINAL).unwrap();
        let policy = IngressPolicy::new(TrustLevel::Untrusted, &[SourceClass::PublicChat]);
        assert!(matches!(
            fixture.run(policy),
            Err(SpeechTranscriptReplayError::BatchRejected { step: 0, .. })
        ));
    }

    #[test]
    fn malformed_steps_are_refused() {
        let base =
            |steps: &str| format!(r#"{{"schema_version":"0.1.0","name":"x","steps":[{steps}]}}"#);
        for steps in [
            r#"{"now_ms":1}"#,
            r#"{"now_ms":2,"poll_floor":"a"},{"now_ms":1,"poll_floor":"a"}"#,
            r#"{"now_ms":1,"poll_floor":"a","unknown":1}"#,
            r#"{"now_ms":1,"poll_floor":"a","take_finals":true}"#,
            r#"{"now_ms":1,"take_finals":false}"#,
        ] {
            assert!(
                SpeechTranscriptReplayFixture::from_json(&base(steps)).is_err(),
                "{steps}"
            );
        }
    }
}
