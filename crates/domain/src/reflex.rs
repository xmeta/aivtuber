use crate::DomainValidationError;
use serde::{Deserialize, Serialize};

pub const REFLEX_SCHEMA_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseRoute {
    Silent,
    Reaction,
    Cached,
    Template,
    Llm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub value: ResponseRoute,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionTarget {
    Camera,
    Chat,
    Game,
    Speaker,
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    None,
    Timeout,
    Unavailable,
    RateLimited,
    Overloaded,
    Authentication,
    InvalidRequest,
    LowConfidence,
    PolicyOverride,
    OperatorOverride,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendIdentity {
    pub name: String,
    pub model_alias: Option<String>,
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflexDecision {
    pub schema_version: String,
    pub route: RouteDecision,
    pub reaction_family: Option<String>,
    pub gesture_family: Option<String>,
    pub attention_target: AttentionTarget,
    pub interrupt_probability: f64,
    pub cache_reuse_probability: f64,
    pub importance: f64,
    pub emotion_intensity: f64,
    pub backend: BackendIdentity,
    pub latency_ms: f64,
    pub fallback_reason: FallbackReason,
}

impl ReflexDecision {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != REFLEX_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {REFLEX_SCHEMA_VERSION}"),
            ));
        }
        if self.backend.name.trim().is_empty() {
            return Err(DomainValidationError::new(
                "backend.name",
                "must not be empty",
            ));
        }
        if let Some(confidence) = self.route.confidence {
            validate_unit_interval("route.confidence", confidence)?;
        }
        validate_unit_interval("interrupt_probability", self.interrupt_probability)?;
        validate_unit_interval("cache_reuse_probability", self.cache_reuse_probability)?;
        validate_unit_interval("importance", self.importance)?;
        validate_unit_interval("emotion_intensity", self.emotion_intensity)?;

        if !self.latency_ms.is_finite() || self.latency_ms < 0.0 {
            return Err(DomainValidationError::new(
                "latency_ms",
                "must be finite and non-negative",
            ));
        }
        Ok(())
    }
}

fn validate_unit_interval(field: &'static str, value: f64) -> Result<(), DomainValidationError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            "must be a finite value in 0..=1",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_decision() -> ReflexDecision {
        ReflexDecision {
            schema_version: REFLEX_SCHEMA_VERSION.to_owned(),
            route: RouteDecision {
                value: ResponseRoute::Cached,
                confidence: Some(0.91),
            },
            reaction_family: Some("embarrassed_laugh".to_owned()),
            gesture_family: Some("head_shake_small".to_owned()),
            attention_target: AttentionTarget::Camera,
            interrupt_probability: 0.08,
            cache_reuse_probability: 0.94,
            importance: 0.37,
            emotion_intensity: 0.64,
            backend: BackendIdentity {
                name: "jev".to_owned(),
                model_alias: Some("jev-latest".to_owned()),
                model_version: None,
            },
            latency_ms: 120.0,
            fallback_reason: FallbackReason::None,
        }
    }

    #[test]
    fn valid_decision_passes() {
        valid_decision().validate().expect("valid reflex decision");
    }

    #[test]
    fn repository_reflex_fixture_matches_domain_contract() {
        let json = include_str!("../../../examples/reflex-decisions/cached-reaction.json");
        let decision: ReflexDecision =
            serde_json::from_str(json).expect("deserialize reflex fixture");
        decision
            .validate()
            .expect("reflex fixture must remain valid");
    }

    #[test]
    fn invalid_probability_is_rejected() {
        let mut decision = valid_decision();
        decision.cache_reuse_probability = 1.01;

        let error = decision.validate().expect_err("probability must fail");
        assert_eq!(error.field(), "cache_reuse_probability");
    }
}
