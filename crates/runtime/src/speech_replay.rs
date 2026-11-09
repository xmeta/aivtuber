//! Deterministic replay of speech observations (issue #92, S1).
//!
//! The scheduler replay path takes `EventEnvelope`s and produces performance
//! plans; speech observations are not events and produce no plan, so they get
//! a sibling fixture runner with the same conventions: a checked-in JSON file
//! under `examples/`, logical time only, a pure run, and expectations that fail
//! loudly. Floor tests can inject observations here with no microphone, audio
//! or model.

use crate::{
    IngressPolicy, SpeechBatchRejection, SpeechObservationConfig, SpeechObservationMetrics,
    SpeechObservationTracker,
};
use aivtuber_domain::SpeechObservationBatch;
use serde::Deserialize;
use std::error::Error;
use std::fmt;

pub const SPEECH_REPLAY_SCHEMA_VERSION: &str = "0.1.0";
/// Bounds a fixture so a hostile or broken file cannot run unbounded.
pub const MAX_SPEECH_REPLAY_STEPS: usize = 4_096;

/// Optional overrides of [`SpeechObservationConfig`]; everything else keeps the
/// production default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechReplayConfig {
    pub max_age_ms: Option<u64>,
    pub turn_end_max_age_ms: Option<u64>,
    pub fallback_silence_ms: Option<u64>,
    pub max_speakers: Option<usize>,
    pub turn_end_languages: Option<Vec<String>>,
}

/// One step at a logical time. Exactly one of `deliver` or `poll`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechReplayStep {
    pub now_ms: u64,
    pub deliver: Option<SpeechObservationBatch>,
    /// Speaker id to poll the fallback signal for.
    pub poll: Option<String>,
    /// For `deliver`: the outcome name of each observation, in order.
    pub expect_outcomes: Option<Vec<String>>,
    /// For `poll`: the fallback signal name (`FallbackTurnSignal::name`).
    pub expect_signal: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechReplayFixture {
    pub schema_version: String,
    pub name: String,
    #[serde(default)]
    pub config: SpeechReplayConfig,
    pub steps: Vec<SpeechReplayStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechReplaySignal {
    pub now_ms: u64,
    pub speaker_id: String,
    pub signal: &'static str,
}

#[derive(Debug, Clone)]
pub struct SpeechReplayReport {
    /// Every poll result, in step order.
    pub signals: Vec<SpeechReplaySignal>,
    pub metrics: SpeechObservationMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechReplayError {
    Fixture(String),
    BatchRejected {
        step: usize,
        rejection: SpeechBatchRejection,
    },
    Mismatch {
        step: usize,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for SpeechReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixture(message) => write!(f, "invalid speech replay fixture: {message}"),
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

impl Error for SpeechReplayError {}

impl SpeechReplayFixture {
    pub fn from_json(text: &str) -> Result<Self, SpeechReplayError> {
        let fixture: Self = serde_json::from_str(text)
            .map_err(|error| SpeechReplayError::Fixture(error.to_string()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), SpeechReplayError> {
        let bad = |message: &str| Err(SpeechReplayError::Fixture(message.to_owned()));
        if self.schema_version != SPEECH_REPLAY_SCHEMA_VERSION {
            return bad("unsupported schema_version");
        }
        if self.steps.is_empty() || self.steps.len() > MAX_SPEECH_REPLAY_STEPS {
            return bad("steps must be 1..=4096");
        }
        let mut previous = 0;
        for step in &self.steps {
            if step.deliver.is_some() == step.poll.is_some() {
                return bad("each step needs exactly one of deliver or poll");
            }
            // Replay ordering is logical time; it never goes backwards.
            if step.now_ms < previous {
                return bad("now_ms must not decrease");
            }
            previous = step.now_ms;
        }
        Ok(())
    }

    fn tracker_config(&self) -> SpeechObservationConfig {
        let mut config = SpeechObservationConfig::default();
        let c = &self.config;
        if let Some(v) = c.max_age_ms {
            config.max_age_ms = v;
        }
        if let Some(v) = c.turn_end_max_age_ms {
            config.turn_end_max_age_ms = v;
        }
        if let Some(v) = c.fallback_silence_ms {
            config.fallback.silence_ms = v;
        }
        if let Some(v) = c.max_speakers {
            config.max_speakers = v;
        }
        if let Some(v) = &c.turn_end_languages {
            config.turn_end_languages.clone_from(v);
        }
        config
    }

    /// Run the fixture under `ingress_policy`. Pure: identical input gives an
    /// identical report.
    pub fn run(
        &self,
        ingress_policy: IngressPolicy,
    ) -> Result<SpeechReplayReport, SpeechReplayError> {
        let mut tracker = SpeechObservationTracker::new(self.tracker_config(), ingress_policy)
            .map_err(|error| SpeechReplayError::Fixture(error.to_string()))?;
        let mut signals = Vec::new();
        for (index, step) in self.steps.iter().enumerate() {
            if let Some(batch) = &step.deliver {
                let report = tracker
                    .ingest_batch(batch, step.now_ms)
                    .map_err(|rejection| SpeechReplayError::BatchRejected {
                        step: index,
                        rejection,
                    })?;
                if let Some(expected) = &step.expect_outcomes {
                    let actual: Vec<&str> = report.outcomes.iter().map(|o| o.name()).collect();
                    if actual != *expected {
                        return Err(SpeechReplayError::Mismatch {
                            step: index,
                            expected: format!("{expected:?}"),
                            actual: format!("{actual:?}"),
                        });
                    }
                }
            }
            if let Some(speaker) = &step.poll {
                let signal = tracker
                    .poll(speaker, step.now_ms)
                    .map_or("no_speech", |view| view.fallback.name());
                if let Some(expected) = &step.expect_signal
                    && expected != signal
                {
                    return Err(SpeechReplayError::Mismatch {
                        step: index,
                        expected: expected.clone(),
                        actual: signal.to_owned(),
                    });
                }
                signals.push(SpeechReplaySignal {
                    now_ms: step.now_ms,
                    speaker_id: speaker.clone(),
                    signal,
                });
            }
        }
        Ok(SpeechReplayReport {
            signals,
            metrics: tracker.metrics(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{SourceClass, TrustLevel};

    const PAUSE_AND_END: &str =
        include_str!("../../../examples/speech-observations/replay/pause-resume-end.json");
    const STALLED_FEED: &str =
        include_str!("../../../examples/speech-observations/replay/stalled-feed.json");

    #[test]
    fn mid_utterance_pause_does_not_fire_and_only_the_true_end_does() {
        let fixture = SpeechReplayFixture::from_json(PAUSE_AND_END).expect("fixture");
        let report = fixture.run(IngressPolicy::default()).expect("replay");
        let fired: Vec<u64> = report
            .signals
            .iter()
            .filter(|s| s.signal == "turn_end_eligible")
            .map(|s| s.now_ms)
            .collect();
        // Pause is 700 ms (< 800): never eligible. The true end is at 3_000,
        // so the first eligible poll is at 3_800 or later and no poll before.
        assert!(!fired.is_empty());
        assert!(fired.iter().all(|t| *t >= 3_800), "fired at {fired:?}");
        let mid_pause: Vec<_> = report
            .signals
            .iter()
            .filter(|s| (1_300..2_000).contains(&s.now_ms))
            .collect();
        assert!(!mid_pause.is_empty());
        assert!(mid_pause.iter().all(|s| s.signal != "turn_end_eligible"));
        assert_eq!(report.metrics.fallback_signals, 1);
        // Deterministic: a second run is identical.
        let again = fixture.run(IngressPolicy::default()).unwrap();
        assert_eq!(report.signals, again.signals);
    }

    #[test]
    fn stalled_feed_rejects_stale_observations_without_freezing_state() {
        let fixture = SpeechReplayFixture::from_json(STALLED_FEED).expect("fixture");
        let report = fixture.run(IngressPolicy::default()).expect("replay");
        assert!(report.metrics.rejected_stale_age >= 1);
        assert_eq!(report.metrics.fallback_signals, 0);
    }

    #[test]
    fn fixture_files_cannot_carry_trust_or_priority() {
        let hostile =
            PAUSE_AND_END.replacen("\"edge\"", "\"trust_level\": \"trusted\", \"edge\"", 1);
        assert!(SpeechReplayFixture::from_json(&hostile).is_err());
    }

    #[test]
    fn a_policy_without_speech_refuses_the_replay() {
        let fixture = SpeechReplayFixture::from_json(PAUSE_AND_END).unwrap();
        let policy = IngressPolicy::new(TrustLevel::Untrusted, &[SourceClass::PublicChat]);
        assert!(matches!(
            fixture.run(policy),
            Err(SpeechReplayError::BatchRejected { step: 1, .. })
        ));
    }

    #[test]
    fn malformed_steps_are_refused() {
        let base =
            |steps: &str| format!(r#"{{"schema_version":"0.1.0","name":"x","steps":[{steps}]}}"#);
        for steps in [
            r#"{"now_ms":1}"#,
            r#"{"now_ms":2,"poll":"a"},{"now_ms":1,"poll":"a"}"#,
            r#"{"now_ms":1,"poll":"a","unknown":1}"#,
        ] {
            assert!(
                SpeechReplayFixture::from_json(&base(steps)).is_err(),
                "{steps}"
            );
        }
    }
}
