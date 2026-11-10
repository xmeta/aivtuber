//! Deterministic replay of perception observations (issue #90, S1a, ADR-0007
//! D-8).
//!
//! Replay never calls OBS or a model. Recorded observations (the wire form, no
//! runtime-assigned fields) are fed through the same ingress as live ones, so
//! staleness, expiry and rejection behave identically, on logical time only.
//! A fixture is a checked-in JSON file under `examples/perception-observations/replay`.
//!
//! The runner never mints authority: the caller passes the
//! [`AuthenticatedControl`] that plays the operator for `enable` and
//! `disable` steps, exactly as the live composition root would.

use crate::{
    HigherRankedScene, PerceptionAvailability, PerceptionBatchRejection, PerceptionConfig,
    PerceptionGateError, PerceptionMetrics, PerceptionTracker, SceneFieldSource,
};
use aivtuber_domain::{
    AuthenticatedControl, PerceptionObservationBatch, PerceptionSceneClass, PrivacyClass,
    ThinkingContent,
};
use aivtuber_telemetry::SecretRedactor;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub const PERCEPTION_REPLAY_SCHEMA_VERSION: &str = "0.1.0";
/// Bounds a fixture so a hostile or broken file cannot run unbounded.
pub const MAX_PERCEPTION_REPLAY_STEPS: usize = 4_096;

/// Operator allowlist and secrets for the run. Everything else keeps the
/// production default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionReplayConfig {
    /// Allowlisted alias to privacy floor. Required for any `enable` to pass.
    #[serde(default)]
    pub allowlist: BTreeMap<String, PrivacyClass>,
    pub max_age_ms: Option<u64>,
    pub max_enabled_sources: Option<usize>,
    /// Secret strings the redactor must remove. Synthetic values only.
    #[serde(default)]
    pub secrets: Vec<String>,
}

/// Scene fields a higher-ranked source supplied at a poll.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionReplayHigher {
    pub scene_class: Option<PerceptionSceneClass>,
    pub labels: Option<Vec<String>>,
}

/// One step at a logical time. Exactly one of `enable`, `disable`, `deliver`
/// or `poll`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionReplayStep {
    pub now_ms: u64,
    pub enable: Option<String>,
    pub disable: Option<String>,
    pub deliver: Option<PerceptionObservationBatch>,
    /// Alias to resolve the scene for and build the Thinking item from.
    pub poll: Option<String>,
    pub higher: Option<PerceptionReplayHigher>,
    /// For `deliver`: the outcome name of each observation, in order.
    pub expect_outcomes: Option<Vec<String>>,
    /// For `poll`: the resolved scene class source name.
    pub expect_scene_source: Option<String>,
    /// For `poll`: whether a Thinking caption item is expected.
    pub expect_thinking_item: Option<bool>,
    /// For `poll`: the availability name.
    pub expect_availability: Option<String>,
    /// For `poll`: whether a perceived scene disagreed with the higher source.
    pub expect_scene_disagreement: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionReplayFixture {
    pub schema_version: String,
    pub name: String,
    #[serde(default)]
    pub config: PerceptionReplayConfig,
    pub steps: Vec<PerceptionReplayStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PerceptionReplayPoll {
    pub now_ms: u64,
    pub scene_class: PerceptionSceneClass,
    pub scene_class_source: SceneFieldSource,
    pub scene_disagreement: bool,
    pub availability: PerceptionAvailability,
    pub thinking_item: Option<ThinkingContent>,
}

#[derive(Debug, Clone)]
pub struct PerceptionReplayReport {
    /// Every poll result, in step order.
    pub polls: Vec<PerceptionReplayPoll>,
    pub metrics: PerceptionMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerceptionReplayError {
    Fixture(String),
    BatchRejected {
        step: usize,
        rejection: PerceptionBatchRejection,
    },
    Gate {
        step: usize,
        error: PerceptionGateError,
    },
    Mismatch {
        step: usize,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for PerceptionReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixture(message) => write!(f, "invalid perception replay fixture: {message}"),
            Self::BatchRejected { step, rejection } => {
                write!(f, "step {step}: batch rejected: {rejection:?}")
            }
            Self::Gate { step, error } => write!(f, "step {step}: gate refused: {error:?}"),
            Self::Mismatch {
                step,
                expected,
                actual,
            } => write!(f, "step {step}: expected {expected}, got {actual}"),
        }
    }
}

impl Error for PerceptionReplayError {}

impl PerceptionReplayFixture {
    pub fn from_json(text: &str) -> Result<Self, PerceptionReplayError> {
        let fixture: Self = serde_json::from_str(text)
            .map_err(|error| PerceptionReplayError::Fixture(error.to_string()))?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> Result<(), PerceptionReplayError> {
        let bad = |message: &str| Err(PerceptionReplayError::Fixture(message.to_owned()));
        if self.schema_version != PERCEPTION_REPLAY_SCHEMA_VERSION {
            return bad("unsupported schema_version");
        }
        if self.steps.is_empty() || self.steps.len() > MAX_PERCEPTION_REPLAY_STEPS {
            return bad("steps must be 1..=4096");
        }
        let mut previous = 0;
        for step in &self.steps {
            let actions = [
                step.enable.is_some(),
                step.disable.is_some(),
                step.deliver.is_some(),
                step.poll.is_some(),
            ];
            if actions.iter().filter(|set| **set).count() != 1 {
                return bad("each step needs exactly one of enable, disable, deliver or poll");
            }
            // Replay ordering is logical time; it never goes backwards.
            if step.now_ms < previous {
                return bad("now_ms must not decrease");
            }
            previous = step.now_ms;
        }
        Ok(())
    }

    fn tracker_config(&self) -> PerceptionConfig {
        let mut config = PerceptionConfig {
            source_privacy_floors: self.config.allowlist.clone(),
            ..PerceptionConfig::default()
        };
        if let Some(v) = self.config.max_age_ms {
            config.max_age_ms = v;
        }
        if let Some(v) = self.config.max_enabled_sources {
            config.max_enabled_sources = v;
        }
        config
    }

    /// Run the fixture. Pure: identical input gives an identical report.
    pub fn run(
        &self,
        operator: &AuthenticatedControl,
    ) -> Result<PerceptionReplayReport, PerceptionReplayError> {
        let mut tracker = PerceptionTracker::new(
            self.tracker_config(),
            SecretRedactor::new(self.config.secrets.iter().cloned()),
        )
        .map_err(|error| PerceptionReplayError::Fixture(error.to_string()))?;
        let mut polls = Vec::new();
        for (index, step) in self.steps.iter().enumerate() {
            let gate = |step: usize, error| PerceptionReplayError::Gate { step, error };
            if let Some(alias) = &step.enable {
                tracker
                    .enable_source(operator, alias, step.now_ms)
                    .map_err(|error| gate(index, error))?;
            }
            if let Some(alias) = &step.disable {
                tracker
                    .disable_source(operator, alias)
                    .map_err(|error| gate(index, error))?;
            }
            if let Some(batch) = &step.deliver {
                let report = tracker
                    .ingest_batch(batch, step.now_ms)
                    .map_err(|rejection| PerceptionReplayError::BatchRejected {
                        step: index,
                        rejection,
                    })?;
                if let Some(expected) = &step.expect_outcomes {
                    let actual: Vec<&str> = report.outcomes.iter().map(|o| o.name()).collect();
                    if actual != *expected {
                        return Err(PerceptionReplayError::Mismatch {
                            step: index,
                            expected: format!("{expected:?}"),
                            actual: format!("{actual:?}"),
                        });
                    }
                }
            }
            if let Some(alias) = &step.poll {
                let higher = step.higher.clone().unwrap_or_default();
                let resolution = tracker.resolve_scene(
                    alias,
                    &HigherRankedScene {
                        scene_class: higher.scene_class,
                        labels: higher.labels,
                    },
                    step.now_ms,
                );
                let poll = PerceptionReplayPoll {
                    now_ms: step.now_ms,
                    scene_class: resolution.scene_class,
                    scene_class_source: resolution.scene_class_source,
                    scene_disagreement: resolution.scene_disagreement,
                    availability: tracker.availability(step.now_ms),
                    thinking_item: tracker.thinking_caption_item(step.now_ms),
                };
                let mismatch = |expected: String, actual: String| {
                    Err(PerceptionReplayError::Mismatch {
                        step: index,
                        expected,
                        actual,
                    })
                };
                if let Some(expected) = &step.expect_scene_source
                    && expected != poll.scene_class_source.name()
                {
                    return mismatch(expected.clone(), poll.scene_class_source.name().to_owned());
                }
                if let Some(expected) = step.expect_thinking_item
                    && expected != poll.thinking_item.is_some()
                {
                    return mismatch(
                        expected.to_string(),
                        poll.thinking_item.is_some().to_string(),
                    );
                }
                if let Some(expected) = &step.expect_availability
                    && expected != poll.availability.name()
                {
                    return mismatch(expected.clone(), poll.availability.name().to_owned());
                }
                if let Some(expected) = step.expect_scene_disagreement
                    && expected != poll.scene_disagreement
                {
                    return mismatch(expected.to_string(), poll.scene_disagreement.to_string());
                }
                polls.push(poll);
            }
        }
        Ok(PerceptionReplayReport {
            polls,
            metrics: tracker.metrics(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        AuthorizationMethod, Capability, ControlSecret, LocalControlIngress, OperatorCommandInput,
        ThinkingContextSource,
    };
    use std::collections::BTreeSet;

    const SCENE_AND_CAPTION: &str =
        include_str!("../../../examples/perception-observations/replay/scene-and-caption.json");
    const HOSTILE_CAPTION: &str =
        include_str!("../../../examples/perception-observations/replay/hostile-caption.json");
    const DISABLED_AND_STALE: &str =
        include_str!("../../../examples/perception-observations/replay/disabled-and-stale.json");

    fn operator() -> AuthenticatedControl {
        let secret = [0x31_u8; 32];
        LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::ObsControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress")
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-replay".to_owned(),
                correlation_id: "corr-replay".to_owned(),
                sequence: 1,
                observed_at: "2026-11-10T00:00:00Z".to_owned(),
                action: "obs.control".to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .expect("authenticated")
        .authority()
        .clone()
    }

    #[test]
    fn recorded_scene_and_caption_replay_deterministically() {
        let fixture = PerceptionReplayFixture::from_json(SCENE_AND_CAPTION).expect("fixture");
        let operator = operator();
        let first = fixture.run(&operator).expect("replay");
        let second = fixture.run(&operator).expect("replay again");
        assert_eq!(first.polls, second.polls);
        assert_eq!(first.metrics, second.metrics);
        assert!(first.polls.iter().any(|poll| poll.thinking_item.is_some()));
        assert!(first.metrics.scene_disagreements >= 1);
    }

    #[test]
    fn hostile_captions_surface_only_as_bounded_untrusted_items() {
        let fixture = PerceptionReplayFixture::from_json(HOSTILE_CAPTION).expect("fixture");
        let report = fixture.run(&operator()).expect("replay");
        let item = report
            .polls
            .iter()
            .find_map(|poll| poll.thinking_item.as_ref())
            .expect("an item is offered");
        assert_eq!(item.source, ThinkingContextSource::ScreenObservation);
        assert!(item.text.len() <= 512);
        assert_eq!(report.metrics.rejected_total(), 0);
    }

    #[test]
    fn disabled_expired_and_stale_observations_reject_in_replay() {
        let fixture = PerceptionReplayFixture::from_json(DISABLED_AND_STALE).expect("fixture");
        let report = fixture.run(&operator()).expect("replay");
        assert!(report.metrics.rejected_disabled >= 1);
        assert!(report.metrics.rejected_expired >= 1);
        assert!(report.metrics.rejected_stale_age >= 1);
        assert!(report.metrics.rejected_out_of_order >= 1);
    }

    #[test]
    fn a_fixture_without_the_operator_capability_cannot_enable_anything() {
        let secret = [0x32_u8; 32];
        let weak = LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::PerformerMute]),
            ControlSecret::new(secret),
        )
        .unwrap()
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-weak".to_owned(),
                correlation_id: "corr-weak".to_owned(),
                sequence: 1,
                observed_at: "2026-11-10T00:00:00Z".to_owned(),
                action: "performer.mute".to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .unwrap()
        .authority()
        .clone();
        let fixture = PerceptionReplayFixture::from_json(SCENE_AND_CAPTION).unwrap();
        assert!(matches!(
            fixture.run(&weak),
            Err(PerceptionReplayError::Gate {
                error: PerceptionGateError::Unauthorized,
                ..
            })
        ));
    }

    #[test]
    fn fixture_files_cannot_carry_trust_or_priority() {
        let hostile = SCENE_AND_CAPTION.replacen(
            "\"observation_id\"",
            "\"trust_level\": \"trusted\", \"observation_id\"",
            1,
        );
        assert_ne!(hostile, SCENE_AND_CAPTION);
        assert!(PerceptionReplayFixture::from_json(&hostile).is_err());
    }

    #[test]
    fn malformed_steps_are_refused() {
        let base =
            |steps: &str| format!(r#"{{"schema_version":"0.1.0","name":"x","steps":[{steps}]}}"#);
        for steps in [
            r#"{"now_ms":1}"#,
            r#"{"now_ms":2,"poll":"a"},{"now_ms":1,"poll":"a"}"#,
            r#"{"now_ms":1,"poll":"a","enable":"a"}"#,
            r#"{"now_ms":1,"poll":"a","unknown":1}"#,
        ] {
            assert!(
                PerceptionReplayFixture::from_json(&base(steps)).is_err(),
                "{steps}"
            );
        }
    }
}
