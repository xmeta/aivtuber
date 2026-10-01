use crate::{AppError, PlaybackRoute, RoutePlanner};
use aivtuber_domain::{EventEnvelope, FallbackReason};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const SHADOW_COMPARISON_SCHEMA_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowPolicyIdentity {
    pub policy_id: String,
    pub policy_version: String,
    pub config_fingerprint: String,
    pub runtime_profile: String,
    pub dataset_id: Option<String>,
}

impl ShadowPolicyIdentity {
    fn validate(&self) -> Result<(), AppError> {
        for (name, value) in [
            ("policy_id", self.policy_id.as_str()),
            ("policy_version", self.policy_version.as_str()),
            ("config_fingerprint", self.config_fingerprint.as_str()),
            ("runtime_profile", self.runtime_profile.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(AppError::Routing(format!(
                    "shadow {name} must not be empty"
                )));
            }
        }
        if self
            .dataset_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(AppError::Routing(
                "shadow dataset_id must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

pub struct ShadowPolicyInput<'a> {
    pub event: &'a EventEnvelope,
    pub remaining_budget: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowRouteClass {
    Silent,
    Deterministic,
    SemanticReuse,
    Template,
    Generated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShadowTargetIdentity {
    Asset {
        asset_id: String,
        asset_identity: Option<String>,
    },
    Template {
        template_id: String,
        template_version: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowFallbackReason {
    Timeout,
    Unavailable,
    RateLimited,
    Overloaded,
    Authentication,
    InvalidRequest,
    LowConfidence,
    PolicyOverride,
    OperatorOverride,
    BudgetExhausted,
    TemplateMissing,
    TemplateInvalid,
    TemplateSlotLimit,
    TemplateSlotTooLarge,
    TemplateRenderTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowDecision {
    pub route: ShadowRouteClass,
    pub target: Option<ShadowTargetIdentity>,
    pub fallback_reason: Option<ShadowFallbackReason>,
}

impl ShadowDecision {
    pub fn silent() -> Self {
        Self {
            route: ShadowRouteClass::Silent,
            target: None,
            fallback_reason: None,
        }
    }

    pub fn deterministic() -> Self {
        Self {
            route: ShadowRouteClass::Deterministic,
            target: None,
            fallback_reason: None,
        }
    }
}

pub trait ShadowPolicy: Send {
    fn evaluate(&mut self, input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError>;
}

/// Provider-free shadow policy matching the deterministic intent/silent split.
/// It deliberately does not retain the intent value itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct IntentShadowPolicy;

impl ShadowPolicy for IntentShadowPolicy {
    fn evaluate(&mut self, input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
        let has_intent = input
            .event
            .payload
            .get("intent")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        Ok(if has_intent {
            ShadowDecision::deterministic()
        } else {
            ShadowDecision::silent()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowEvaluationFailure {
    PlannerError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ShadowEvaluationOutcome {
    Evaluated { decision: ShadowDecision },
    Failed { reason: ShadowEvaluationFailure },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowComparisonRecord {
    pub schema_version: String,
    pub event_id: String,
    pub correlation_id: String,
    pub active_policy: ShadowPolicyIdentity,
    pub shadow_policy: ShadowPolicyIdentity,
    pub active: ShadowDecision,
    pub shadow: ShadowEvaluationOutcome,
    pub route_diverged: Option<bool>,
    pub target_diverged: Option<bool>,
    pub fallback_diverged: Option<bool>,
}

pub struct ShadowingRoutePlanner<A, S> {
    active: A,
    shadow: S,
    active_identity: ShadowPolicyIdentity,
    shadow_identity: ShadowPolicyIdentity,
    last_comparison: Option<ShadowComparisonRecord>,
}

impl<A, S> ShadowingRoutePlanner<A, S> {
    pub fn new(
        active: A,
        shadow: S,
        active_identity: ShadowPolicyIdentity,
        shadow_identity: ShadowPolicyIdentity,
    ) -> Result<Self, AppError> {
        active_identity.validate()?;
        shadow_identity.validate()?;
        Ok(Self {
            active,
            shadow,
            active_identity,
            shadow_identity,
            last_comparison: None,
        })
    }

    pub fn last_comparison(&self) -> Option<&ShadowComparisonRecord> {
        self.last_comparison.as_ref()
    }

    pub fn active(&self) -> &A {
        &self.active
    }
}

impl<A, S> RoutePlanner for ShadowingRoutePlanner<A, S>
where
    A: RoutePlanner,
    S: ShadowPolicy,
{
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        self.route_with_budget(event, None)
    }

    fn route_with_budget(
        &mut self,
        event: &EventEnvelope,
        remaining_budget: Option<Duration>,
    ) -> Result<PlaybackRoute, AppError> {
        let active_route = self.active.route_with_budget(event, remaining_budget)?;
        let active_decision = summarize_active_route(&active_route, &self.active);
        let shadow = match self.shadow.evaluate(&ShadowPolicyInput {
            event,
            remaining_budget,
        }) {
            Ok(decision) => ShadowEvaluationOutcome::Evaluated { decision },
            Err(_) => ShadowEvaluationOutcome::Failed {
                reason: ShadowEvaluationFailure::PlannerError,
            },
        };

        let (route_diverged, target_diverged, fallback_diverged) = match &shadow {
            ShadowEvaluationOutcome::Evaluated { decision } => (
                Some(active_decision.route != decision.route),
                compare_target(&active_decision, decision),
                Some(active_decision.fallback_reason != decision.fallback_reason),
            ),
            ShadowEvaluationOutcome::Failed { .. } => (None, None, None),
        };

        self.last_comparison = Some(ShadowComparisonRecord {
            schema_version: SHADOW_COMPARISON_SCHEMA_VERSION.to_owned(),
            event_id: event.event_id.clone(),
            correlation_id: event.correlation_id.clone(),
            active_policy: self.active_identity.clone(),
            shadow_policy: self.shadow_identity.clone(),
            active: active_decision,
            shadow,
            route_diverged,
            target_diverged,
            fallback_diverged,
        });

        Ok(active_route)
    }

    fn decision_record(&self) -> Option<&aivtuber_reflex::DecisionReplayRecord> {
        self.active.decision_record()
    }

    fn template_fallback(&self) -> Option<crate::TemplateFallback> {
        self.active.template_fallback()
    }
}

fn compare_target(active: &ShadowDecision, shadow: &ShadowDecision) -> Option<bool> {
    match (&active.target, &shadow.target) {
        (Some(left), Some(right)) => Some(left != right),
        (None, None)
            if active.route == ShadowRouteClass::Silent
                && shadow.route == ShadowRouteClass::Silent =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn summarize_active_route<R: RoutePlanner>(route: &PlaybackRoute, planner: &R) -> ShadowDecision {
    let (route_class, target) = match route {
        PlaybackRoute::Silent => (ShadowRouteClass::Silent, None),
        PlaybackRoute::Intent(_) => (ShadowRouteClass::Deterministic, None),
        PlaybackRoute::AssetId(asset_id) => (
            ShadowRouteClass::SemanticReuse,
            Some(ShadowTargetIdentity::Asset {
                asset_id: asset_id.clone(),
                asset_identity: None,
            }),
        ),
        PlaybackRoute::AssetIdentity {
            asset_id,
            asset_identity,
        } => (
            ShadowRouteClass::SemanticReuse,
            Some(ShadowTargetIdentity::Asset {
                asset_id: asset_id.clone(),
                asset_identity: Some(asset_identity.clone()),
            }),
        ),
        PlaybackRoute::Template {
            template_id,
            composition,
        } => (
            ShadowRouteClass::Template,
            Some(ShadowTargetIdentity::Template {
                template_id: template_id.clone(),
                template_version: composition.template_version.clone(),
            }),
        ),
        PlaybackRoute::Generate(_) => (ShadowRouteClass::Generated, None),
    };

    ShadowDecision {
        route: route_class,
        target,
        fallback_reason: planner
            .template_fallback()
            .map(template_fallback_reason)
            .or_else(|| {
                planner
                    .decision_record()
                    .and_then(|record| fallback_reason(record.policy.fallback_reason))
            }),
    }
}

fn fallback_reason(reason: FallbackReason) -> Option<ShadowFallbackReason> {
    match reason {
        FallbackReason::None => None,
        FallbackReason::Timeout => Some(ShadowFallbackReason::Timeout),
        FallbackReason::Unavailable => Some(ShadowFallbackReason::Unavailable),
        FallbackReason::RateLimited => Some(ShadowFallbackReason::RateLimited),
        FallbackReason::Overloaded => Some(ShadowFallbackReason::Overloaded),
        FallbackReason::Authentication => Some(ShadowFallbackReason::Authentication),
        FallbackReason::InvalidRequest => Some(ShadowFallbackReason::InvalidRequest),
        FallbackReason::LowConfidence => Some(ShadowFallbackReason::LowConfidence),
        FallbackReason::PolicyOverride => Some(ShadowFallbackReason::PolicyOverride),
        FallbackReason::OperatorOverride => Some(ShadowFallbackReason::OperatorOverride),
        FallbackReason::BudgetExhausted => Some(ShadowFallbackReason::BudgetExhausted),
    }
}

fn template_fallback_reason(reason: crate::TemplateFallback) -> ShadowFallbackReason {
    match reason {
        crate::TemplateFallback::MissingTemplate => ShadowFallbackReason::TemplateMissing,
        crate::TemplateFallback::InvalidTemplate => ShadowFallbackReason::TemplateInvalid,
        crate::TemplateFallback::SlotLimitExceeded => ShadowFallbackReason::TemplateSlotLimit,
        crate::TemplateFallback::SlotValueTooLarge => ShadowFallbackReason::TemplateSlotTooLarge,
        crate::TemplateFallback::RenderTooLarge => ShadowFallbackReason::TemplateRenderTooLarge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Clone)]
    struct FixedActive(PlaybackRoute);

    impl RoutePlanner for FixedActive {
        fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
            Ok(self.0.clone())
        }
    }

    struct FixedShadow(Result<ShadowDecision, ()>);

    impl ShadowPolicy for FixedShadow {
        fn evaluate(&mut self, _input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
            self.0
                .clone()
                .map_err(|_| AppError::Routing("shadow failure with private detail".to_owned()))
        }
    }

    fn identity(policy_id: &str) -> ShadowPolicyIdentity {
        ShadowPolicyIdentity {
            policy_id: policy_id.to_owned(),
            policy_version: "v1".to_owned(),
            config_fingerprint: format!("{policy_id}-cfg"),
            runtime_profile: "cached".to_owned(),
            dataset_id: Some("reaction-quality-stage2".to_owned()),
        }
    }

    fn event() -> EventEnvelope {
        serde_json::from_value(json!({
            "schema_version": "0.2.0",
            "event_id": "evt-shadow-001",
            "correlation_id": "corr-shadow-001",
            "sequence": 1,
            "observed_at": "2026-10-01T00:00:00Z",
            "source": "shadow-test",
            "source_class": "public_chat",
            "plane": "content",
            "trust_level": "untrusted",
            "kind": "chat.message",
            "actor_id": "viewer:test",
            "priority_hint": null,
            "payload": {
                "intent": "sensitive.intent.value",
                "text": "private payload must never enter the shadow record"
            }
        }))
        .expect("event fixture")
    }

    #[test]
    fn wrapper_returns_active_route_unchanged_while_recording_divergence() {
        let active_route = PlaybackRoute::AssetIdentity {
            asset_id: "reaction.agree.01".to_owned(),
            asset_identity: "asset-sha".to_owned(),
        };
        let mut planner = ShadowingRoutePlanner::new(
            FixedActive(active_route.clone()),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
        )
        .expect("planner");

        let returned = planner.route(&event()).expect("active route");
        assert_eq!(returned, active_route);

        let record = planner.last_comparison().expect("comparison");
        assert_eq!(record.route_diverged, Some(true));
        assert_eq!(record.target_diverged, None);
        assert_eq!(record.active.route, ShadowRouteClass::SemanticReuse);
    }

    #[test]
    fn shadow_failure_is_observational_and_does_not_fail_active_route() {
        let mut planner = ShadowingRoutePlanner::new(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Err(())),
            identity("active"),
            identity("shadow"),
        )
        .expect("planner");

        assert_eq!(
            planner.route(&event()).expect("active route"),
            PlaybackRoute::Silent
        );
        assert_eq!(
            planner.last_comparison().expect("comparison").shadow,
            ShadowEvaluationOutcome::Failed {
                reason: ShadowEvaluationFailure::PlannerError
            }
        );
    }

    #[test]
    fn comparison_serialization_excludes_event_payload_and_private_shadow_error() {
        let mut planner = ShadowingRoutePlanner::new(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Err(())),
            identity("active"),
            identity("shadow"),
        )
        .expect("planner");
        planner.route(&event()).expect("route");

        let json = serde_json::to_string(planner.last_comparison().expect("comparison"))
            .expect("serialize");
        assert!(!json.contains("private payload"));
        assert!(!json.contains("sensitive.intent.value"));
        assert!(!json.contains("private detail"));
        assert!(json.contains("evt-shadow-001"));
    }

    #[test]
    fn intent_shadow_policy_is_provider_free_and_does_not_retain_intent_value() {
        let input_event = event();
        let mut policy = IntentShadowPolicy;
        let decision = policy
            .evaluate(&ShadowPolicyInput {
                event: &input_event,
                remaining_budget: Some(Duration::from_millis(10)),
            })
            .expect("decision");

        assert_eq!(decision, ShadowDecision::deterministic());
        let json = serde_json::to_string(&decision).expect("serialize");
        assert!(!json.contains("sensitive.intent.value"));
    }

    #[test]
    fn template_comparison_retains_safe_identity_but_not_rendered_text() {
        let active_route = PlaybackRoute::Template {
            template_id: "thanks.donation".to_owned(),
            composition: crate::TemplateComposition {
                template_id: "thanks.donation".to_owned(),
                template_version: "starter-v1".to_owned(),
                text: "private rendered slot value".to_owned(),
            },
        };
        let shadow_decision = ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(ShadowTargetIdentity::Template {
                template_id: "thanks.donation".to_owned(),
                template_version: "starter-v1".to_owned(),
            }),
            fallback_reason: None,
        };
        let mut planner = ShadowingRoutePlanner::new(
            FixedActive(active_route),
            FixedShadow(Ok(shadow_decision)),
            identity("active"),
            identity("shadow"),
        )
        .expect("planner");

        planner.route(&event()).expect("route");
        let record = planner.last_comparison().expect("comparison");
        assert_eq!(record.route_diverged, Some(false));
        assert_eq!(record.target_diverged, Some(false));

        let json = serde_json::to_string(record).expect("serialize");
        assert!(json.contains("thanks.donation"));
        assert!(json.contains("starter-v1"));
        assert!(!json.contains("private rendered slot value"));
    }

    #[test]
    fn invalid_identity_is_rejected_before_shadow_evaluation() {
        let mut invalid = identity("shadow");
        invalid.config_fingerprint.clear();
        let result = ShadowingRoutePlanner::new(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            invalid,
        );
        assert!(result.is_err());
    }
}
