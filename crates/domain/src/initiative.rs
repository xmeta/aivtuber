//! Idle-initiative context for a reply request (issue #87 S1).
//!
//! An [`InitiativeContext`] tells a Thinking consumer that no viewer wrote the
//! `input`: the performer decided, in deterministic code, to speak on its own
//! because the stream went quiet. It names one [`InitiativeKind`] from a closed
//! vocabulary and nothing else: no free text, no viewer text and no history.
//!
//! Authority rule: the context is derived data minted by the runtime's
//! initiative director, never from a payload field, and it grants nothing: it
//! cannot reach the operator or control plane, a capability, a permit or a
//! gate. Model output produced under it is data like any other reply.
//!
//! Normative wire form: `schemas/initiative-context.schema.json`. It is the
//! optional `initiative` member of the Reflex context (and so of the Thinking
//! request context) and is omitted when absent, so a request without one is
//! byte-identical to one built before this contract existed.

use crate::DomainValidationError;
use serde::{Deserialize, Serialize};

pub const INITIATIVE_CONTEXT_SCHEMA_VERSION: &str = "0.1.0";

/// Closed vocabulary of what the performer may start on its own. Adding a kind
/// is a reviewed code and schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitiativeKind {
    /// Ask the audience one short question.
    AudienceQuestion,
    /// Say one small thing about the moment, aloud.
    IdleObservation,
    /// Come back to something the performer said earlier.
    RevisitTopic,
}

impl InitiativeKind {
    pub const ALL: [Self; 3] = [
        Self::AudienceQuestion,
        Self::IdleObservation,
        Self::RevisitTopic,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::AudienceQuestion => "audience_question",
            Self::IdleObservation => "idle_observation",
            Self::RevisitTopic => "revisit_topic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitiativeContext {
    pub schema_version: String,
    pub kind: InitiativeKind,
}

impl InitiativeContext {
    pub fn new(kind: InitiativeKind) -> Self {
        Self {
            schema_version: INITIATIVE_CONTEXT_SCHEMA_VERSION.to_owned(),
            kind,
        }
    }

    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != INITIATIVE_CONTEXT_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "initiative.schema_version",
                format!("expected {INITIATIVE_CONTEXT_SCHEMA_VERSION}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_under_its_name() {
        for kind in InitiativeKind::ALL {
            let context = InitiativeContext::new(kind);
            context.validate().expect("valid");
            let value = serde_json::to_value(&context).expect("serializes");
            assert_eq!(value["kind"], kind.name());
            let back: InitiativeContext = serde_json::from_value(value).expect("parses");
            assert_eq!(back, context);
        }
    }

    #[test]
    fn unknown_kinds_fields_and_versions_are_refused() {
        let bad_kind = serde_json::json!({"schema_version": "0.1.0", "kind": "free_text"});
        assert!(serde_json::from_value::<InitiativeContext>(bad_kind).is_err());
        let extra =
            serde_json::json!({"schema_version": "0.1.0", "kind": "idle_observation", "text": "x"});
        assert!(serde_json::from_value::<InitiativeContext>(extra).is_err());
        let mut wrong = InitiativeContext::new(InitiativeKind::IdleObservation);
        wrong.schema_version = "9.9.9".to_owned();
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn the_schema_fixtures_deserialize_and_validate() {
        for text in [
            include_str!("../../../examples/initiative-context/valid/audience-question.json"),
            include_str!("../../../examples/initiative-context/valid/idle-observation.json"),
            include_str!("../../../examples/initiative-context/valid/revisit-topic.json"),
        ] {
            let parsed: InitiativeContext = serde_json::from_str(text).expect("valid fixture");
            parsed.validate().expect("valid fixture validates");
        }
        for text in [
            include_str!("../../../examples/initiative-context/invalid/free-text-field.json"),
            include_str!(
                "../../../examples/initiative-context/invalid/kind-outside-vocabulary.json"
            ),
            include_str!("../../../examples/initiative-context/invalid/missing-kind.json"),
            include_str!("../../../examples/initiative-context/invalid/wrong-version.json"),
        ] {
            let outcome = serde_json::from_str::<InitiativeContext>(text)
                .map_err(|e| e.to_string())
                .and_then(|context| context.validate().map_err(|e| e.to_string()));
            assert!(outcome.is_err(), "invalid fixture must be refused: {text}");
        }
    }
}
