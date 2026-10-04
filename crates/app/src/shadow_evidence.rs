//! Issue #165: make #163/#164 shadow comparisons consumable by #58
//! quantitative benchmarking and #59/#151 human review.
//!
//! Four rules shape this module:
//!
//! 1. **Evidence, never authority.** Nothing here can change the active route,
//!    and there is no scalar that ranks a policy. A report states how often two
//!    policies disagreed under one identity. Promoting a shadow policy to
//!    active behaviour remains an explicit, reviewed decision.
//! 2. **Aggregate, never retain.** A report carries counts, identity and case
//!    *references* (`event_id` / `correlation_id`) — never payload, rendered
//!    text or provider responses (section 5 of `docs/shadow-evaluation.adoc`).
//! 3. **Only combine like with like.** Records from different policy/config/
//!    profile identities, dataset ids, comparison schema versions or bounded
//!    orchestration bounds are refused rather than silently merged: a merged
//!    number across identities cannot be interpreted.
//! 4. **An absent comparison is not agreement.** A target pair the runtime
//!    declines to compare, and a shadow evaluation that produced nothing, are
//!    both counted as *not comparable*. Neither may be folded into agreement,
//!    or the divergence rate would silently understate the disagreement it
//!    exists to measure.

use crate::AppError;
use crate::shadow::{
    SHADOW_COMPARISON_SCHEMA_VERSION, SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowComparisonRecord,
    ShadowEvaluationOutcome, ShadowFallbackReason, ShadowOrchestratorConfig, ShadowRouteClass,
};
use crate::shadow_orchestrator::ShadowExecutionSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema version of the aggregated divergence report itself.
///
/// Versioned independently of [`SHADOW_COMPARISON_SCHEMA_VERSION`] so a stored
/// report can be read alongside the records it summarises, and so a change to
/// this shape is visible to consumers instead of silently absorbed.
pub const SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION: &str = "1";

/// Why a pair of decisions was counted the way it was.
///
/// The names are stable and machine-readable: #58 metrics and #151 review
/// sampling both key off them, so a rename is a contract change rather than a
/// refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowDivergenceCategory {
    /// Same route class and the same comparable target: the policies agreed.
    SameRouteSameTarget,
    /// Same route class, different comparable target — a real disagreement
    /// about *what* to play.
    SameRouteDifferentTarget,
    /// Different route class on both sides (neither is silence).
    RouteTransition,
    /// One side responded where the other stayed silent, or vice versa.
    ResponseVsSilent,
    /// Exactly one side carries a fallback reason, the other is a clean
    /// success. Both sides failing is *not* this category — see
    /// [`Self::FallbackReasonMismatch`].
    FallbackVsSuccess,
    /// **Both** sides carry a fallback reason and the reasons differ
    /// (`Timeout` vs `Unavailable`, say). Distinct from
    /// [`Self::FallbackVsSuccess`]: one side degraded while the other succeeded,
    /// here both degraded differently.
    FallbackReasonMismatch,
    /// The shadow half produced no usable decision, so there is nothing to
    /// compare. Counted separately rather than folded into "agreed".
    ShadowUnusable,
    /// The route classes matched but the target pair was *not comparable* — for
    /// example two `Generated` routes, whose content the runtime deliberately
    /// does not retain. This is an absent comparison, not an agreement, and is
    /// excluded from the divergence rate's denominator for that reason.
    TargetIncomparable,
}

impl ShadowDivergenceCategory {
    /// Stable snake_case name, used as the JSON key and the metric suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameRouteSameTarget => "same_route_same_target",
            Self::SameRouteDifferentTarget => "same_route_different_target",
            Self::RouteTransition => "route_transition",
            Self::ResponseVsSilent => "response_vs_silent",
            Self::FallbackVsSuccess => "fallback_vs_success",
            Self::FallbackReasonMismatch => "fallback_reason_mismatch",
            Self::ShadowUnusable => "shadow_unusable",
            Self::TargetIncomparable => "target_incomparable",
        }
    }

    /// Every category, in a fixed order, so a report always carries the same
    /// key set regardless of which categories a sample happened to contain.
    pub const ALL: [Self; 8] = [
        Self::SameRouteSameTarget,
        Self::SameRouteDifferentTarget,
        Self::RouteTransition,
        Self::ResponseVsSilent,
        Self::FallbackVsSuccess,
        Self::FallbackReasonMismatch,
        Self::ShadowUnusable,
        Self::TargetIncomparable,
    ];

    /// True when the two policies actually disagreed.
    ///
    /// [`Self::ShadowUnusable`] and [`Self::TargetIncomparable`] are *failures to
    /// compare*, not agreements, so both are excluded from the divergence rate:
    /// counting either as agreement would let a policy that produces unusable
    /// or uncompareable evidence look better than one that disagrees.
    pub fn is_divergence(self) -> bool {
        !matches!(
            self,
            Self::SameRouteSameTarget | Self::ShadowUnusable | Self::TargetIncomparable
        )
    }

    /// True when the record behind this category took part in the divergence
    /// rate's denominator.
    pub fn is_comparable(self) -> bool {
        !matches!(self, Self::ShadowUnusable | Self::TargetIncomparable)
    }
}

/// Classification of a single comparison record.
///
/// `comparable` is false when the record cannot be classified on the evidence
/// available — a failed shadow evaluation, or a target pair `compare_target`
/// declined to compare. Such a record still counts toward the total, with a
/// non-comparable category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowDivergenceClassification {
    pub category: ShadowDivergenceCategory,
    pub comparable: bool,
}

/// Classify one comparison record.
///
/// Three ordering rules are the contract, and each exists because the looser
/// reading would misstate the report:
///
/// 1. A fallback difference outranks a route difference, because degradation is
///    the operationally interesting half.
/// 2. `fallback_vs_success` means **exactly one** side carries a fallback
///    reason. `build_record` sets `fallback_diverged` from
///    `active.fallback_reason != shadow.fallback_reason`, which is also true
///    when *both* sides failed for different reasons — that case is
///    [`ShadowDivergenceCategory::FallbackReasonMismatch`] instead.
/// 3. A route match with an *incomparable* target pair is
///    [`ShadowDivergenceCategory::TargetIncomparable`], never
///    `same_route_same_target`. `compare_target` returns `None` for pairs like
///    `Generated` vs `Generated`, whose content the runtime does not retain, so
///    agreement there is unevidenced.
pub fn classify_shadow_divergence(
    record: &ShadowComparisonRecord,
) -> ShadowDivergenceClassification {
    let shadow_decision = match &record.shadow {
        ShadowEvaluationOutcome::Failed { .. } => {
            return ShadowDivergenceClassification {
                category: ShadowDivergenceCategory::ShadowUnusable,
                comparable: false,
            };
        }
        ShadowEvaluationOutcome::Evaluated { decision } => decision,
    };

    let active_fallback = record.active.fallback_reason;
    let shadow_fallback = shadow_decision.fallback_reason;

    let category = match (active_fallback, shadow_fallback) {
        (Some(active), Some(shadow)) if active != shadow => {
            ShadowDivergenceCategory::FallbackReasonMismatch
        }
        (Some(_), None) | (None, Some(_)) => ShadowDivergenceCategory::FallbackVsSuccess,
        _ => match (record.active.route, shadow_decision.route) {
            (ShadowRouteClass::Silent, ShadowRouteClass::Silent) => {
                ShadowDivergenceCategory::SameRouteSameTarget
            }
            (ShadowRouteClass::Silent, _) | (_, ShadowRouteClass::Silent) => {
                ShadowDivergenceCategory::ResponseVsSilent
            }
            (active_route, shadow_route) if active_route != shadow_route => {
                ShadowDivergenceCategory::RouteTransition
            }
            _ => match record.target_diverged {
                Some(true) => ShadowDivergenceCategory::SameRouteDifferentTarget,
                Some(false) => ShadowDivergenceCategory::SameRouteSameTarget,
                // The runtime declined to compare these targets. The route
                // classes matched, but agreement is unevidenced.
                None => ShadowDivergenceCategory::TargetIncomparable,
            },
        },
    };

    ShadowDivergenceClassification {
        category,
        comparable: category.is_comparable(),
    }
}

/// The identity that two records must share before they may be aggregated.
///
/// #165 makes this boundary explicit instead of implicit. Both policy
/// identities are held field by field — including *both* `dataset_id`s, since
/// `ShadowPolicyIdentity` defines one per side and they may legitimately differ.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowCompatibilityKey {
    pub comparison_schema_version: String,
    pub active_policy_id: String,
    pub active_policy_version: String,
    pub active_config_fingerprint: String,
    pub active_runtime_profile: String,
    pub active_dataset_id: Option<String>,
    pub shadow_policy_id: String,
    pub shadow_policy_version: String,
    pub shadow_config_fingerprint: String,
    pub shadow_runtime_profile: String,
    pub shadow_dataset_id: Option<String>,
}

impl ShadowCompatibilityKey {
    /// Derive the key from one record. Records agreeing on this key are the
    /// only ones that may be combined.
    pub fn from_record(record: &ShadowComparisonRecord) -> Self {
        Self {
            comparison_schema_version: record.schema_version.clone(),
            active_policy_id: record.active_policy.policy_id.clone(),
            active_policy_version: record.active_policy.policy_version.clone(),
            active_config_fingerprint: record.active_policy.config_fingerprint.clone(),
            active_runtime_profile: record.active_policy.runtime_profile.clone(),
            active_dataset_id: record.active_policy.dataset_id.clone(),
            shadow_policy_id: record.shadow_policy.policy_id.clone(),
            shadow_policy_version: record.shadow_policy.policy_version.clone(),
            shadow_config_fingerprint: record.shadow_policy.config_fingerprint.clone(),
            shadow_runtime_profile: record.shadow_policy.runtime_profile.clone(),
            shadow_dataset_id: record.shadow_policy.dataset_id.clone(),
        }
    }
}

/// The bounded-orchestration identity a batch of records was produced under.
///
/// A `ShadowComparisonRecord` does not carry this, and it must not be
/// reconstructed afterwards: `ShadowOrchestratorConfig` fixes the sample rate,
/// deadline, concurrency, queue capacity and whether provider calls or budget
/// charging were permitted, and two batches differing only in those bounds are
/// not comparable even under an identical policy identity. The caller therefore
/// supplies the config it actually ran with, and the summarizer validates it
/// before copying anything into a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowOrchestrationIdentity {
    pub orchestrator_schema_version: String,
    /// Stable fingerprint of the orchestration bounds (sample rate, seed,
    /// event kinds, max concurrent, queue capacity, deadline, provider-call and
    /// budget flags). Two batches may only be combined when this matches.
    pub bounds_fingerprint: String,
}

impl ShadowOrchestrationIdentity {
    /// Derive the identity from the config the records were actually produced
    /// under. The fingerprint covers every bound that changes which events are
    /// admitted and how long an evaluation may run.
    pub fn from_config(config: &ShadowOrchestratorConfig) -> Self {
        // FNV-1a over a canonical, field-ordered rendering. Deterministic and
        // dependency-free, matching `ShadowOrchestratorConfig::sample_bucket`.
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        fn absorb(mut hash: u64, text: &str) -> u64 {
            for byte in text.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash
        }

        let kinds: Vec<String> = config
            .event_kinds
            .iter()
            .map(|kind| format!("{kind:?}"))
            .collect();
        let canonical = format!(
            "enabled={}|sample_rate_per_10k={}|seed={}|event_kinds={}|max_concurrent={}|queue_capacity={}|deadline_ms={}|allow_provider_calls={}|charge_shadow_to_budget={}",
            config.enabled,
            config.sample_rate_per_10k,
            config.seed,
            kinds.join(","),
            config.max_concurrent,
            config.queue_capacity,
            config.deadline_ms,
            config.allow_provider_calls,
            config.charge_shadow_to_budget,
        );
        let hash = absorb(FNV_OFFSET_BASIS, &canonical);

        Self {
            orchestrator_schema_version: config.schema_version.clone(),
            bounds_fingerprint: format!("{hash:016x}"),
        }
    }

    /// Reject an identity that does not describe the orchestrator this build
    /// actually runs. A report must never claim bounds it cannot vouch for.
    fn validate(&self) -> Result<(), AppError> {
        if self.orchestrator_schema_version != SHADOW_ORCHESTRATOR_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "shadow divergence refused: orchestration schema_version {:?} is not the current {:?} (#165)",
                self.orchestrator_schema_version, SHADOW_ORCHESTRATOR_SCHEMA_VERSION
            )));
        }
        if self.bounds_fingerprint.trim().is_empty() {
            return Err(AppError::Routing(
                "shadow divergence refused: orchestration bounds_fingerprint must not be empty (#165)"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// Optional operational evidence for a batch (#165 scope: provider-call counts
/// and latency attribution "when available").
///
/// Counters are supplied by the caller from a `ShadowExecutionSnapshot` delta
/// taken around the batch, because the snapshot is cumulative for the life of
/// the orchestrator and the records alone cannot say what the shadow path cost.
/// They are attached to the batch as a whole rather than per record, so they
/// carry no identity of their own and cannot be mistaken for per-case evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowOperationalEvidence {
    pub provider_calls: u64,
    pub provider_calls_blocked: u64,
    pub evaluations_completed: u64,
    pub evaluations_deadline_exceeded: u64,
}

impl ShadowOperationalEvidence {
    /// Read a cumulative snapshot delta. Saturating subtraction, so a caller
    /// that passes snapshots out of order yields zeroes rather than a wrap.
    pub fn from_snapshot_delta(before: &SnapshotCounters, after: &SnapshotCounters) -> Self {
        Self {
            provider_calls: after.provider_calls.saturating_sub(before.provider_calls),
            provider_calls_blocked: after
                .provider_calls_blocked
                .saturating_sub(before.provider_calls_blocked),
            evaluations_completed: after.completed.saturating_sub(before.completed),
            evaluations_deadline_exceeded: after
                .deadline_exceeded
                .saturating_sub(before.deadline_exceeded),
        }
    }
}

/// The subset of `ShadowExecutionSnapshot` this module reads.
///
/// Named separately so callers pass the counters explicitly instead of this
/// module reaching into orchestrator internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCounters {
    pub provider_calls: u64,
    pub provider_calls_blocked: u64,
    pub completed: u64,
    pub deadline_exceeded: u64,
}

impl SnapshotCounters {
    /// Read the counters out of a `ShadowExecutionSnapshot`.
    pub fn from_snapshot(snapshot: &ShadowExecutionSnapshot) -> Self {
        Self {
            provider_calls: snapshot.provider_calls,
            provider_calls_blocked: snapshot.provider_calls_blocked,
            completed: snapshot.completed,
            deadline_exceeded: snapshot.deadline_exceeded,
        }
    }
}

/// A reference to one divergent case, safe for offline review sampling.
///
/// `event_id` and `correlation_id` are runtime identifiers, not content: a #151
/// reviewer needs a stable handle to request a replay, not the payload. This
/// struct has no field that can hold one.
///
/// Deliberately not `Ord`: `ShadowRouteClass` and `ShadowFallbackReason` have no
/// ordering of their own, and inventing one here would imply an active/shadow
/// preference order this evidence does not express.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowDivergentCaseRef {
    pub event_id: String,
    pub correlation_id: String,
    pub category: ShadowDivergenceCategory,
    pub active_route: ShadowRouteClass,
    pub shadow_route: ShadowRouteClass,
    pub active_fallback_reason: Option<ShadowFallbackReason>,
    pub shadow_fallback_reason: Option<ShadowFallbackReason>,
}

/// One metric value, shaped like a `benchmark-result` metric entry so #58 can
/// merge it into its `metrics` map without translation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowReportMetric {
    pub value: f64,
    pub sample_count: u64,
}

/// Aggregated divergence evidence for exactly one compatibility key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowDivergenceReport {
    pub schema_version: String,
    /// The bounded-orchestration identity the caller supplied and this
    /// summarizer validated, copied verbatim. Never reconstructed here.
    pub orchestration: ShadowOrchestrationIdentity,
    pub compatibility: ShadowCompatibilityKey,
    pub total_comparisons: u64,
    pub comparable_comparisons: u64,
    pub diverged_comparisons: u64,
    /// Count per category. Every category is present, including zeroes, so two
    /// reports compare field by field.
    pub category_counts: BTreeMap<String, u64>,
    /// `#58`-shaped flat metrics (`shadow.divergence.<category>_count`, plus
    /// `shadow.divergence.rate_pct` and any supplied operational counters).
    pub metrics: BTreeMap<String, ShadowReportMetric>,
    /// Divergent case references in input order, for #151 offline sampling.
    pub divergent_cases: Vec<ShadowDivergentCaseRef>,
    /// True when `divergent_cases` holds fewer references than
    /// `diverged_comparisons`. Counts are never truncated, so a truncated
    /// report still states its true totals.
    pub divergent_cases_truncated: bool,
}

/// How many divergent case references a report retains by default.
pub const DEFAULT_DIVERGENT_CASE_LIMIT: usize = 64;

/// Aggregate comparison records into one report.
///
/// `orchestration` must describe the config these records were actually
/// produced under, and is validated before anything is copied into the report.
/// `operational` is optional and adds provider-call counters as batch-level
/// metrics.
///
/// Refuses, rather than merges, when the batch is empty (an empty report would
/// read as "no divergence"), the orchestration identity is not the current one,
/// a record carries a different comparison `schema_version`, or any record's
/// identity differs from the first record's. `case_limit` bounds retained
/// references only, never counts.
pub fn summarize_shadow_divergence(
    records: &[ShadowComparisonRecord],
    orchestration: &ShadowOrchestrationIdentity,
    case_limit: usize,
) -> Result<ShadowDivergenceReport, AppError> {
    orchestration.validate()?;

    let Some(first) = records.first() else {
        return Err(AppError::Routing(
            "shadow divergence summary requires at least one comparison record (#165)".to_owned(),
        ));
    };

    let compatibility = ShadowCompatibilityKey::from_record(first);
    for record in records {
        if record.schema_version != SHADOW_COMPARISON_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "shadow divergence refused: record {} carries comparison schema_version {:?}, current is {:?}; samples from different comparison versions must not be combined (#165)",
                record.event_id, record.schema_version, SHADOW_COMPARISON_SCHEMA_VERSION
            )));
        }
        let key = ShadowCompatibilityKey::from_record(record);
        if key != compatibility {
            return Err(AppError::Routing(format!(
                "shadow divergence refused: record {} has identity {key:?}, expected {compatibility:?}; records from different policy/config/profile identities must not be aggregated (#165)",
                record.event_id
            )));
        }
    }

    let mut category_counts: BTreeMap<String, u64> = ShadowDivergenceCategory::ALL
        .iter()
        .map(|category| (category.as_str().to_owned(), 0))
        .collect();
    let mut divergent_cases = Vec::new();
    let mut comparable_comparisons = 0u64;
    let mut diverged_comparisons = 0u64;

    for record in records {
        let classification = classify_shadow_divergence(record);
        if let Some(count) = category_counts.get_mut(classification.category.as_str()) {
            *count += 1;
        }

        if !classification.comparable {
            continue;
        }
        comparable_comparisons += 1;

        if !classification.category.is_divergence() {
            continue;
        }
        diverged_comparisons += 1;

        if divergent_cases.len() < case_limit {
            divergent_cases.push(divergent_case_ref(record, classification.category));
        }
    }

    let diverged_total = diverged_comparisons;
    let truncated = diverged_total > divergent_cases.len() as u64;
    let sample_count = records.len() as u64;
    let mut metrics: BTreeMap<String, ShadowReportMetric> = category_counts
        .iter()
        .map(|(category, count)| {
            (
                format!("shadow.divergence.{category}_count"),
                ShadowReportMetric {
                    value: *count as f64,
                    sample_count,
                },
            )
        })
        .collect();
    metrics.insert(
        "shadow.divergence.rate_pct".to_owned(),
        ShadowReportMetric {
            // Divergence over *comparable* comparisons only. Dividing by all
            // records would let a broken or uncompareable shadow policy depress
            // the rate and make it look like a good one.
            value: if comparable_comparisons == 0 {
                0.0
            } else {
                diverged_total as f64 * 100.0 / comparable_comparisons as f64
            },
            sample_count: comparable_comparisons,
        },
    );

    Ok(ShadowDivergenceReport {
        schema_version: SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION.to_owned(),
        orchestration: orchestration.clone(),
        compatibility,
        total_comparisons: records.len() as u64,
        comparable_comparisons,
        diverged_comparisons,
        category_counts,
        metrics,
        divergent_cases,
        divergent_cases_truncated: truncated,
    })
}

/// [`summarize_shadow_divergence`] with [`DEFAULT_DIVERGENT_CASE_LIMIT`] and no
/// operational evidence.
pub fn summarize_shadow_divergence_default(
    records: &[ShadowComparisonRecord],
    orchestration: &ShadowOrchestrationIdentity,
) -> Result<ShadowDivergenceReport, AppError> {
    summarize_shadow_divergence(records, orchestration, DEFAULT_DIVERGENT_CASE_LIMIT)
}

fn divergent_case_ref(
    record: &ShadowComparisonRecord,
    category: ShadowDivergenceCategory,
) -> ShadowDivergentCaseRef {
    let (shadow_route, shadow_fallback_reason) = match &record.shadow {
        ShadowEvaluationOutcome::Evaluated { decision } => {
            (decision.route, decision.fallback_reason)
        }
        // Unreachable in practice: a record with no usable decision is not
        // comparable and never reaches here. Kept total rather than panicking.
        ShadowEvaluationOutcome::Failed { .. } => (record.active.route, None),
    };
    ShadowDivergentCaseRef {
        event_id: record.event_id.clone(),
        correlation_id: record.correlation_id.clone(),
        category,
        active_route: record.active.route,
        shadow_route,
        active_fallback_reason: record.active.fallback_reason,
        shadow_fallback_reason,
    }
}
