use crate::shadow_orchestrator::{ShadowCompletion, ShadowExecutionSnapshot, ShadowOrchestrator};
use crate::{AppError, PlaybackRoute, RoutePlanner};
use aivtuber_domain::{EventEnvelope, EventKind, FallbackReason};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

pub const SHADOW_COMPARISON_SCHEMA_VERSION: &str = "0.1.0";

/// Version of the bounded orchestration configuration (#164).
///
/// Sampling and filter decisions are evidence: a comparison record is only
/// interpretable alongside the config that produced it. The version travels
/// with the config so a stored sample cannot be silently re-interpreted under
/// different bounds.
pub const SHADOW_ORCHESTRATOR_SCHEMA_VERSION: &str = "0.1.0";

/// Bounded production-compatible orchestration settings for shadow
/// evaluation (#164).
///
/// Every bound is explicit and validated. There is no "unbounded" default: an
/// operator who enables shadow must state how much work may be outstanding and
/// how long a single evaluation may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowOrchestratorConfig {
    pub schema_version: String,
    /// Master switch. Shadow work is never admitted while false.
    pub enabled: bool,
    /// Deterministic admission rate in parts per 10,000 (`0..=10000`).
    pub sample_rate_per_10k: u32,
    /// Seed for deterministic admission sampling.
    pub seed: u64,
    /// Event kinds eligible for shadow evaluation. Empty means every kind.
    pub event_kinds: Vec<EventKind>,
    /// Maximum shadow jobs executing concurrently across the worker pool.
    pub max_concurrent: usize,
    /// Maximum shadow jobs waiting behind the executing workers.
    pub queue_capacity: usize,
    /// Per-event shadow deadline in milliseconds.
    pub deadline_ms: u64,
    /// Whether an optional live provider bridge may be consulted at all.
    /// Defaults to false so PR CI and default deployments make no billable
    /// provider call from the shadow path.
    pub allow_provider_calls: bool,
    /// Whether shadow work is admitted through the #69 budget ledger. Defaults
    /// to false so shadow usage cannot silently double provider spend.
    pub charge_shadow_to_budget: bool,
}

impl Default for ShadowOrchestratorConfig {
    fn default() -> Self {
        Self {
            schema_version: SHADOW_ORCHESTRATOR_SCHEMA_VERSION.to_owned(),
            enabled: false,
            sample_rate_per_10k: 1_000,
            seed: 0,
            event_kinds: Vec::new(),
            max_concurrent: 1,
            queue_capacity: 8,
            deadline_ms: 250,
            allow_provider_calls: false,
            charge_shadow_to_budget: false,
        }
    }
}

impl ShadowOrchestratorConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != SHADOW_ORCHESTRATOR_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "unsupported shadow orchestrator schema_version: {}",
                self.schema_version
            )));
        }
        if self.sample_rate_per_10k > 10_000 {
            return Err(AppError::Routing(
                "shadow sample_rate_per_10k must be between 0 and 10000".to_owned(),
            ));
        }
        if self.max_concurrent == 0 {
            return Err(AppError::Routing(
                "shadow max_concurrent must be greater than zero".to_owned(),
            ));
        }
        if self.queue_capacity == 0 {
            return Err(AppError::Routing(
                "shadow queue_capacity must be greater than zero".to_owned(),
            ));
        }
        if self.deadline_ms == 0 {
            return Err(AppError::Routing(
                "shadow deadline_ms must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }

    /// Deterministic admission bucket for one event.
    ///
    /// FNV-1a over the seed and event id. Deliberately independent of wall
    /// clock, thread identity, and evaluation order: the same trace and seed
    /// must admit exactly the same events on every run, which is what makes a
    /// sampled comparison reproducible.
    pub fn sample_bucket(&self, event_id: &str) -> u32 {
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = FNV_OFFSET_BASIS;
        for byte in self
            .seed
            .to_le_bytes()
            .into_iter()
            .chain(event_id.as_bytes().iter().copied())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        (hash % 10_000) as u32
    }

    /// Whether this event may become shadow work at all, and why not.
    ///
    /// Returns `None` when the event is eligible.
    pub fn skip_reason(&self, event: &EventEnvelope) -> Option<ShadowSkipReason> {
        if !self.enabled {
            return Some(ShadowSkipReason::Disabled);
        }
        if !self.event_kinds.is_empty() && !self.event_kinds.contains(&event.kind) {
            return Some(ShadowSkipReason::Filtered);
        }
        if self.sample_bucket(&event.event_id) >= self.sample_rate_per_10k {
            return Some(ShadowSkipReason::NotSampled);
        }
        None
    }
}

/// Why an event produced no shadow work.
///
/// These are admission outcomes, not evaluation outcomes: a skipped event was
/// never evaluated, so it must not be reported as a shadow planner failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowSkipReason {
    Disabled,
    Filtered,
    NotSampled,
    Saturated,
    ShuttingDown,
    BudgetDenied,
}

/// Result of attempting to submit one event for shadow evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ShadowSubmission {
    Admitted { job_id: u64 },
    Skipped { reason: ShadowSkipReason },
}

/// Optional live/offline provider bridge for shadow evaluation (#164).
///
/// Deliberately narrower than the production provider interfaces: a shadow
/// provider can only be asked for a non-executable `ShadowDecision`, and it is
/// never consulted while `allow_provider_calls` is false.
pub trait ShadowProvider: Send + Sync {
    fn evaluate(&self, input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError>;
}

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
    /// The per-event shadow deadline elapsed; any partial result is discarded.
    DeadlineExceeded,
    /// The job was superseded by newer source work or shutdown began before it
    /// produced a usable decision.
    Cancelled,
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

/// Where and how the shadow half of the wrapper runs.
///
/// `Inline` is the #163 behavior: the shadow policy is evaluated
/// synchronously on the composition thread. `Bounded` is #164: the policy is
/// handed to a bounded worker pool and never evaluated inline. Both return the
/// same active route, so the mode is an orchestration choice, not an output
/// choice.
enum ShadowExecution<S>
where
    S: ShadowPolicy + 'static,
{
    Inline(S),
    Bounded(Box<ShadowOrchestrator<S>>),
}

pub struct ShadowingRoutePlanner<A, S>
where
    S: ShadowPolicy + 'static,
{
    active: A,
    shadow: ShadowExecution<S>,
    active_identity: ShadowPolicyIdentity,
    shadow_identity: ShadowPolicyIdentity,
    last_comparison: Option<ShadowComparisonRecord>,
}

impl<A, S> ShadowingRoutePlanner<A, S>
where
    S: ShadowPolicy + 'static,
{
    /// Synchronous (#163) shadow evaluation. Retained as the default so an
    /// unconfigured deployment behaves exactly as it did before #164.
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
            shadow: ShadowExecution::Inline(shadow),
            active_identity,
            shadow_identity,
            last_comparison: None,
        })
    }

    /// Bounded (#164) shadow evaluation through a worker pool.
    pub fn with_orchestrator(
        active: A,
        shadow: S,
        active_identity: ShadowPolicyIdentity,
        shadow_identity: ShadowPolicyIdentity,
        provider: Option<Arc<dyn ShadowProvider>>,
        config: ShadowOrchestratorConfig,
    ) -> Result<Self, AppError>
    where
        S: Clone,
    {
        active_identity.validate()?;
        shadow_identity.validate()?;
        let orchestrator = ShadowOrchestrator::new(shadow, provider, config).map_err(|error| {
            AppError::Routing(format!(
                "shadow orchestrator configuration invalid: {error}"
            ))
        })?;
        Ok(Self {
            active,
            shadow: ShadowExecution::Bounded(Box::new(orchestrator)),
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

    /// Outcome of the most recent shadow submission attempt.
    pub fn last_shadow_submission(&self) -> Option<&ShadowSubmission> {
        match &self.shadow {
            ShadowExecution::Inline(_) => None,
            ShadowExecution::Bounded(orchestrator) => orchestrator.last_submission(),
        }
    }

    /// Whether shadow work is charged to the #69 budget ledger.
    pub fn shadow_charges_budget(&self) -> bool {
        matches!(&self.shadow, ShadowExecution::Bounded(orchestrator) if orchestrator.charges_budget())
    }

    /// Record that the #69 budget governor refused a submitted shadow job.
    pub fn note_shadow_budget_denied(&mut self) {
        if let ShadowExecution::Bounded(orchestrator) = &mut self.shadow {
            orchestrator.note_budget_denied();
        }
    }

    /// Bounded shadow orchestration snapshot, or `None` in synchronous mode.
    pub fn shadow_execution_snapshot(&self) -> Option<ShadowExecutionSnapshot> {
        match &self.shadow {
            ShadowExecution::Inline(_) => None,
            ShadowExecution::Bounded(orchestrator) => Some(orchestrator.snapshot()),
        }
    }

    /// Take completions published by the bounded pool since the last drain.
    /// Never affects the active route.
    pub fn drain_shadow_comparisons(&mut self) -> Vec<ShadowCompletion> {
        match &mut self.shadow {
            ShadowExecution::Inline(_) => Vec::new(),
            ShadowExecution::Bounded(orchestrator) => orchestrator.drain_comparisons(),
        }
    }

    /// Cancel outstanding shadow work and join every worker.
    ///
    /// Blocking until the join completes is the point: #164 requires that no
    /// shadow work is still running once shutdown returns.
    pub fn shutdown_shadow(&mut self) {
        if let ShadowExecution::Bounded(orchestrator) = &mut self.shadow {
            orchestrator.shutdown();
        }
    }
}

impl<A, S> RoutePlanner for ShadowingRoutePlanner<A, S>
where
    A: RoutePlanner,
    S: ShadowPolicy + 'static,
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

        match &mut self.shadow {
            // Bounded mode (#164): the composition thread only hands the work
            // off. Nothing about this call depends on the shadow outcome, so the
            // active route is returned without waiting for it.
            ShadowExecution::Bounded(orchestrator) => {
                orchestrator.submit(
                    event,
                    remaining_budget,
                    &self.active_identity,
                    &self.shadow_identity,
                    active_decision,
                );
                return Ok(active_route);
            }
            ShadowExecution::Inline(shadow) => {
                let shadow_outcome = match shadow.evaluate(&ShadowPolicyInput {
                    event,
                    remaining_budget,
                }) {
                    Ok(decision) => ShadowEvaluationOutcome::Evaluated { decision },
                    Err(_) => ShadowEvaluationOutcome::Failed {
                        reason: ShadowEvaluationFailure::PlannerError,
                    },
                };

                let (route_diverged, target_diverged, fallback_diverged) = match &shadow_outcome {
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
                    shadow: shadow_outcome,
                    route_diverged,
                    target_diverged,
                    fallback_diverged,
                });
            }
        }

        Ok(active_route)
    }

    fn decision_record(&self) -> Option<&aivtuber_reflex::DecisionReplayRecord> {
        self.active.decision_record()
    }

    fn template_fallback(&self) -> Option<crate::TemplateFallback> {
        self.active.template_fallback()
    }

    fn drain_shadow_comparisons(&mut self) -> Vec<ShadowCompletion> {
        self.drain_shadow_comparisons()
    }

    fn shadow_execution_snapshot(&self) -> Option<ShadowExecutionSnapshot> {
        self.shadow_execution_snapshot()
    }

    fn last_shadow_submission(&self) -> Option<&ShadowSubmission> {
        self.last_shadow_submission()
    }

    fn shadow_charges_budget(&self) -> bool {
        self.shadow_charges_budget()
    }

    fn note_shadow_budget_denied(&mut self) {
        self.note_shadow_budget_denied();
    }

    fn shutdown_shadow(&mut self) {
        self.shutdown_shadow();
    }
}

pub(crate) fn compare_target(active: &ShadowDecision, shadow: &ShadowDecision) -> Option<bool> {
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    #[derive(Clone)]
    struct FixedActive(PlaybackRoute);

    impl RoutePlanner for FixedActive {
        fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Clone)]
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

    // ---- #164 bounded orchestration -------------------------------------

    fn bounded_config() -> ShadowOrchestratorConfig {
        ShadowOrchestratorConfig {
            enabled: true,
            sample_rate_per_10k: 10_000,
            ..ShadowOrchestratorConfig::default()
        }
    }

    fn event_with_id(event_id: &str) -> EventEnvelope {
        let mut event = event();
        event.event_id = event_id.to_owned();
        event
    }

    /// Test gate that lets a test observe how many shadow evaluations are
    /// executing and then releases them on demand.
    ///
    /// A `Barrier` would deadlock here: barrier parties are counted per
    /// *arrival*, and one job can arrive more than once, so a test that also
    /// waits on the barrier would never trip it.
    #[derive(Debug)]
    struct EvalGate {
        entered: Mutex<usize>,
        entered_signal: Condvar,
        open: Mutex<bool>,
        open_signal: Condvar,
    }

    impl EvalGate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: Mutex::new(0),
                entered_signal: Condvar::new(),
                open: Mutex::new(false),
                open_signal: Condvar::new(),
            })
        }

        fn arrive(&self) {
            let mut entered = self.entered.lock().expect("gate entered lock");
            *entered += 1;
            self.entered_signal.notify_all();
        }
        fn wait_for_arrivals(&self, expected: usize) {
            let mut entered = self.entered.lock().expect("gate entered lock");
            while *entered < expected {
                entered = self
                    .entered_signal
                    .wait(entered)
                    .expect("gate entered wait");
            }
        }

        fn release(&self) {
            let mut open = self.open.lock().expect("gate open lock");
            *open = true;
            self.open_signal.notify_all();
        }

        fn await_release(&self) {
            let mut open = self.open.lock().expect("gate open lock");
            while !*open {
                open = self.open_signal.wait(open).expect("gate open wait");
            }
        }
    }

    /// Holds every evaluation until the test releases it, so saturation,
    /// cancellation, and deadlines are observable without sleeping in the
    /// assertions themselves.
    #[derive(Clone)]
    struct GatedShadowPolicy {
        decision: ShadowDecision,
        gate: Arc<EvalGate>,
    }

    impl ShadowPolicy for GatedShadowPolicy {
        fn evaluate(&mut self, input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
            // The input is still consumed so the compiler proves the policy
            // receives the same bounded authority as #163.
            let _ = input.event.kind;
            self.gate.arrive();
            self.gate.await_release();
            Ok(self.decision.clone())
        }
    }

    /// Sleeps past any plausible deadline so the per-event deadline, rather than
    /// an explicit test gate, is what ends the evaluation.
    #[derive(Clone)]
    struct SleepingShadowPolicy {
        decision: ShadowDecision,
        sleep_ms: u64,
    }

    impl ShadowPolicy for SleepingShadowPolicy {
        fn evaluate(&mut self, _input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            Ok(self.decision.clone())
        }
    }

    /// Deadlines for gated tests are deliberately far longer than any realistic
    /// queue delay, so a test can never hang waiting for a policy that the
    /// deadline legitimately refused to start.
    fn gated_deadline() -> ShadowOrchestratorConfig {
        ShadowOrchestratorConfig {
            deadline_ms: 30_000,
            ..bounded_config()
        }
    }

    /// Bounded poll for the orchestrator to go quiescent, collecting every
    /// completion it publishes.
    fn drain_until_quiescent<P>(
        planner: &mut ShadowingRoutePlanner<P, GatedShadowPolicy>,
    ) -> Vec<ShadowCompletion> {
        let mut collected = Vec::new();
        for _ in 0..2_000 {
            collected.extend(planner.drain_shadow_comparisons());
            if let Some(snapshot) = planner.shadow_execution_snapshot()
                && snapshot.is_quiescent()
            {
                return collected;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        collected
    }

    fn drain_with_spy<P>(
        planner: &mut ShadowingRoutePlanner<P, FixedShadow>,
    ) -> Vec<ShadowCompletion> {
        let mut collected = Vec::new();
        for _ in 0..2_000 {
            collected.extend(planner.drain_shadow_comparisons());
            if let Some(snapshot) = planner.shadow_execution_snapshot()
                && snapshot.is_quiescent()
            {
                return collected;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        collected
    }

    fn gated_planner(
        gate: &Arc<EvalGate>,
        config: ShadowOrchestratorConfig,
    ) -> ShadowingRoutePlanner<FixedActive, GatedShadowPolicy> {
        ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            GatedShadowPolicy {
                decision: ShadowDecision::silent(),
                gate: Arc::clone(gate),
            },
            identity("active"),
            identity("shadow"),
            None,
            config,
        )
        .expect("planner")
    }

    #[test]
    fn config_is_versioned_and_rejects_unknown_schema() {
        let config = bounded_config();
        assert_eq!(config.schema_version, SHADOW_ORCHESTRATOR_SCHEMA_VERSION);
        let json = serde_json::to_string(&config).expect("serialize config");
        assert!(json.contains("\"schema_version\":\"0.1.0\""));
        assert!(serde_json::from_str::<ShadowOrchestratorConfig>(&json).is_ok());

        let mut unknown = config.clone();
        unknown.schema_version = "9.9.9".to_owned();
        assert!(unknown.validate().is_err());

        // Every bound is validated rather than silently clamped.
        for invalid in [
            ShadowOrchestratorConfig {
                sample_rate_per_10k: 10_001,
                ..config.clone()
            },
            ShadowOrchestratorConfig {
                max_concurrent: 0,
                ..config.clone()
            },
            ShadowOrchestratorConfig {
                queue_capacity: 0,
                ..config.clone()
            },
            ShadowOrchestratorConfig {
                deadline_ms: 0,
                ..config.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn sampling_is_deterministic_for_the_same_seed_and_trace() {
        let config = ShadowOrchestratorConfig {
            enabled: true,
            sample_rate_per_10k: 5_000,
            seed: 42,
            ..ShadowOrchestratorConfig::default()
        };
        let ids: Vec<String> = (0..64).map(|index| format!("evt-{index:04}")).collect();

        let first: Vec<Option<ShadowSkipReason>> = ids
            .iter()
            .map(|id| config.skip_reason(&event_with_id(id)))
            .collect();
        let second: Vec<Option<ShadowSkipReason>> = ids
            .iter()
            .map(|id| config.skip_reason(&event_with_id(id)))
            .collect();
        assert_eq!(
            first, second,
            "the same seed and trace must sample identically"
        );
        assert!(
            first.iter().any(Option::is_none) && first.iter().any(Option::is_some),
            "a 50% rate must both admit and reject across 64 events"
        );

        let off = ShadowOrchestratorConfig {
            sample_rate_per_10k: 0,
            ..config.clone()
        };
        assert!(
            ids.iter()
                .all(|id| off.skip_reason(&event_with_id(id)).is_some())
        );

        let all = ShadowOrchestratorConfig {
            sample_rate_per_10k: 10_000,
            ..config
        };
        assert!(
            ids.iter()
                .all(|id| all.skip_reason(&event_with_id(id)).is_none())
        );

        // A different seed must be able to produce a different sample.
        let reseeded = ShadowOrchestratorConfig {
            seed: 43,
            ..ShadowOrchestratorConfig::default()
        };
        let reseeded_decisions: Vec<Option<ShadowSkipReason>> = ids
            .iter()
            .map(|id| reseeded.skip_reason(&event_with_id(id)))
            .collect();
        let original = ShadowOrchestratorConfig {
            enabled: true,
            sample_rate_per_10k: 5_000,
            seed: 42,
            ..ShadowOrchestratorConfig::default()
        };
        let original_decisions: Vec<Option<ShadowSkipReason>> = ids
            .iter()
            .map(|id| original.skip_reason(&event_with_id(id)))
            .collect();
        assert_ne!(reseeded_decisions, original_decisions);
    }

    #[test]
    fn kind_filter_excludes_non_listed_kinds_and_disabled_admits_nothing() {
        let filtered = ShadowOrchestratorConfig {
            enabled: true,
            sample_rate_per_10k: 10_000,
            event_kinds: vec![EventKind::ChatDonation],
            ..ShadowOrchestratorConfig::default()
        };
        assert_eq!(
            filtered.skip_reason(&event_with_id("evt-chat")),
            Some(ShadowSkipReason::Filtered)
        );
        let mut donation = event_with_id("evt-donation");
        donation.kind = EventKind::ChatDonation;
        assert_eq!(filtered.skip_reason(&donation), None);

        let disabled = ShadowOrchestratorConfig {
            enabled: false,
            sample_rate_per_10k: 10_000,
            event_kinds: Vec::new(),
            ..ShadowOrchestratorConfig::default()
        };
        assert_eq!(
            disabled.skip_reason(&event_with_id("evt-any")),
            Some(ShadowSkipReason::Disabled)
        );
    }

    #[test]
    fn concurrency_bound_is_enforced_and_saturation_never_blocks_the_active_route() {
        let gate = EvalGate::new();
        let mut planner = gated_planner(
            &gate,
            ShadowOrchestratorConfig {
                max_concurrent: 2,
                queue_capacity: 1,
                ..gated_deadline()
            },
        );

        // Admit one job, wait until a worker has actually claimed it, then admit
        // the second and wait for *that* arrival. Each wait names the exact
        // arrival count it needs: a wait that only re-checks an already-satisfied
        // condition would return while the job is still queued, and the next
        // submit would then saturate a queue that is momentarily full.
        planner.route(&event_with_id("evt-a")).expect("route a");
        gate.wait_for_arrivals(1);
        planner.route(&event_with_id("evt-b")).expect("route b");
        gate.wait_for_arrivals(2);

        // Both workers are now executing, so the queue is empty: this job takes
        // the single queue slot and the next one has nowhere to go.
        assert_eq!(
            planner.route(&event_with_id("evt-c")).expect("route c"),
            PlaybackRoute::Silent,
            "a queued shadow job must still return the active route immediately"
        );
        assert_eq!(
            planner.route(&event_with_id("evt-d")).expect("route d"),
            PlaybackRoute::Silent
        );
        assert_eq!(
            planner.last_shadow_submission(),
            Some(&ShadowSubmission::Skipped {
                reason: ShadowSkipReason::Saturated
            })
        );

        let snapshot = planner.shadow_execution_snapshot().expect("snapshot");
        assert!(
            snapshot.in_flight_high_water <= 2,
            "concurrency bound exceeded: {}",
            snapshot.in_flight_high_water
        );
        assert_eq!(snapshot.dropped_saturated, 1);

        gate.release();
        planner.shutdown_shadow();
    }
    #[test]
    fn deadline_exceeded_discards_the_result_instead_of_publishing_it() {
        // A 1ms deadline against a policy that sleeps far longer: the
        // evaluation necessarily outlives its own deadline.
        let mut planner = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            SleepingShadowPolicy {
                decision: ShadowDecision::deterministic(),
                sleep_ms: 200,
            },
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                deadline_ms: 1,
                ..bounded_config()
            },
        )
        .expect("planner");

        assert_eq!(
            planner.route(&event_with_id("evt-slow")).expect("route"),
            PlaybackRoute::Silent
        );

        let mut record = None;
        for _ in 0..2_000 {
            for completion in planner.drain_shadow_comparisons() {
                if let Some(candidate) = completion.record {
                    record = Some(candidate);
                }
            }
            if record.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let record = record.expect("deadline record");
        assert_eq!(
            record.shadow,
            ShadowEvaluationOutcome::Failed {
                reason: ShadowEvaluationFailure::DeadlineExceeded
            },
            "a decision produced after the deadline must be discarded"
        );
        assert_eq!(record.route_diverged, None);
        planner.shutdown_shadow();
    }

    #[test]
    fn newer_shadow_work_supersedes_the_previous_evaluation() {
        let gate = EvalGate::new();
        let mut planner = gated_planner(&gate, gated_deadline());

        planner
            .route(&event_with_id("evt-superseded"))
            .expect("route");
        gate.wait_for_arrivals(1);
        // Admitting newer work cancels the in-flight token, so the stale
        // evaluation must finish without publishing evidence.
        planner.route(&event_with_id("evt-current")).expect("route");
        gate.release();

        let drained = drain_until_quiescent(&mut planner);
        assert!(
            drained.iter().any(|completion| completion.record.is_none()),
            "superseded work must be reported finished without evidence"
        );
        let current = drained
            .iter()
            .filter_map(|completion| completion.record.as_ref())
            .find(|record| record.event_id == "evt-current")
            .expect("current comparison");
        assert_eq!(
            current.shadow,
            ShadowEvaluationOutcome::Evaluated {
                decision: ShadowDecision::silent()
            }
        );
        planner.shutdown_shadow();
    }

    #[test]
    fn shutdown_joins_every_worker_and_leaves_no_shadow_work_running() {
        let gate = EvalGate::new();
        let mut planner = gated_planner(&gate, gated_deadline());

        planner
            .route(&event_with_id("evt-shutdown"))
            .expect("route");
        gate.wait_for_arrivals(1);
        gate.release();
        // Drain before shutdown: a completion sitting in the mailbox is
        // undrained evidence, not work still running.
        let _ = drain_until_quiescent(&mut planner);

        planner.shutdown_shadow();
        let snapshot = planner.shadow_execution_snapshot().expect("snapshot");
        assert!(snapshot.shutting_down);
        assert_eq!(snapshot.worker_running, 0, "every worker must be joined");
        assert!(snapshot.is_quiescent());

        // Nothing new may be admitted after shutdown.
        assert_eq!(
            planner.route(&event_with_id("evt-after")).expect("route"),
            PlaybackRoute::Silent
        );
        assert_eq!(
            planner.last_shadow_submission(),
            Some(&ShadowSubmission::Skipped {
                reason: ShadowSkipReason::ShuttingDown
            })
        );
    }

    #[test]
    fn bounded_mode_preserves_the_exact_active_route_and_keeps_payloads_out_of_evidence() {
        let active_route = PlaybackRoute::AssetIdentity {
            asset_id: "reaction.agree.01".to_owned(),
            asset_identity: "asset-sha".to_owned(),
        };
        let mut planner = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(active_route.clone()),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
            None,
            bounded_config(),
        )
        .expect("planner");

        assert_eq!(
            planner.route(&event()).expect("route"),
            active_route,
            "bounded orchestration must not change the active route"
        );

        let drained = drain_with_spy(&mut planner);
        let record = drained
            .into_iter()
            .find_map(|completion| completion.record)
            .expect("comparison record");
        assert_eq!(record.route_diverged, Some(true));
        assert_eq!(record.active.route, ShadowRouteClass::SemanticReuse);

        let json = serde_json::to_string(&record).expect("serialize");
        assert!(!json.contains("private payload"));
        assert!(!json.contains("sensitive.intent.value"));
        assert!(!json.contains("private detail"));
        planner.shutdown_shadow();
    }

    #[derive(Default)]
    struct CountingProvider {
        calls: AtomicUsize,
    }

    impl ShadowProvider for CountingProvider {
        fn evaluate(&self, input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Answer something different from the fixed shadow policy so the
            // provider path is provably the one that produced the decision.
            if input.event.payload.contains_key("intent") {
                Ok(ShadowDecision::deterministic())
            } else {
                Ok(ShadowDecision::silent())
            }
        }
    }

    #[test]
    fn provider_calls_are_disabled_by_default_and_optional() {
        let provider = Arc::new(CountingProvider::default());
        let spy = Arc::clone(&provider);

        // Disabled: a configured provider must never be consulted.
        let mut disabled = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
            Some(provider.clone()),
            bounded_config(),
        )
        .expect("planner");
        disabled.route(&event()).expect("route");
        let drained = drain_with_spy(&mut disabled);
        assert_eq!(spy.calls.load(Ordering::SeqCst), 0);
        let snapshot = disabled.shadow_execution_snapshot().expect("snapshot");
        assert_eq!(snapshot.provider_calls, 0);
        assert_eq!(snapshot.provider_calls_blocked, 1);
        assert!(!drained.is_empty());
        disabled.shutdown_shadow();

        // Explicitly enabled: the mock provider is consulted instead of the
        // local policy. No network path exists in either case.
        let enabled_provider = Arc::new(CountingProvider::default());
        let enabled_spy = Arc::clone(&enabled_provider);
        let mut enabled = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
            Some(enabled_provider),
            ShadowOrchestratorConfig {
                allow_provider_calls: true,
                ..bounded_config()
            },
        )
        .expect("planner");
        enabled.route(&event()).expect("route");
        let drained = drain_with_spy(&mut enabled);
        assert_eq!(enabled_spy.calls.load(Ordering::SeqCst), 1);
        let record = drained
            .into_iter()
            .find_map(|completion| completion.record)
            .expect("provider comparison");
        assert_eq!(
            record.shadow,
            ShadowEvaluationOutcome::Evaluated {
                decision: ShadowDecision::deterministic()
            },
            "the provider, not the local policy, must produce the decision"
        );
        assert_eq!(
            enabled
                .shadow_execution_snapshot()
                .expect("snapshot")
                .provider_calls,
            1
        );
        enabled.shutdown_shadow();
    }

    #[test]
    fn shadow_budget_charging_is_opt_in() {
        assert!(!ShadowOrchestratorConfig::default().charge_shadow_to_budget);
        assert!(
            ShadowOrchestratorConfig {
                charge_shadow_to_budget: true,
                ..ShadowOrchestratorConfig::default()
            }
            .validate()
            .is_ok()
        );

        let free = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
            None,
            bounded_config(),
        )
        .expect("planner");
        assert!(!free.shadow_charges_budget());

        let charged = ShadowingRoutePlanner::with_orchestrator(
            FixedActive(PlaybackRoute::Silent),
            FixedShadow(Ok(ShadowDecision::silent())),
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                charge_shadow_to_budget: true,
                ..bounded_config()
            },
        )
        .expect("planner");
        assert!(charged.shadow_charges_budget());
    }
}
