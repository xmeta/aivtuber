//! Issue #165: make #163/#164 shadow comparisons consumable by #58
//! quantitative benchmarking and #59/#151 human review.
//!
//! Three rules shape this module:
//!
//! 1. **Evidence, never authority.** Nothing here can change the active route,
//!    and there is no scalar that ranks a policy. A report states how often two
//!    policies disagreed under one identity. Promoting a shadow policy to
//!    active behaviour remains an explicit, reviewed decision.
//! 2. **Aggregate, never retain.** A report carries counts, identity and case
//!    *references* (`event_id` / `correlation_id`) — never payload, rendered
//!    text or provider responses (section 5 of `docs/shadow-evaluation.adoc`).
//! 3. **Only combine like with like.** Records from different
//!    policy/config/profile identities, or from a different comparison
//!    `schema_version`, are refused rather than silently merged: a merged
//!    number across identities cannot be interpreted.

use crate::AppError;
use crate::shadow::{
    SHADOW_COMPARISON_SCHEMA_VERSION, SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowComparisonRecord,
    ShadowEvaluationOutcome, ShadowFallbackReason, ShadowRouteClass,
};
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
    /// The fallback reason differs, so one side degraded and the other did not.
    FallbackVsSuccess,
    /// The shadow half produced no usable decision, so there is nothing to
    /// compare. Counted separately rather than folded into "agreed".
    ShadowUnusable,
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
            Self::ShadowUnusable => "shadow_unusable",
        }
    }

    /// Every category, in a fixed order, so a report always carries the same
    /// key set regardless of which categories a sample happened to contain.
    pub const ALL: [Self; 6] = [
        Self::SameRouteSameTarget,
        Self::SameRouteDifferentTarget,
        Self::RouteTransition,
        Self::ResponseVsSilent,
        Self::FallbackVsSuccess,
        Self::ShadowUnusable,
    ];

    /// True when the two policies actually disagreed.
    ///
    /// [`Self::ShadowUnusable`] is a failure to compare, not an agreement, so it
    /// is deliberately excluded from the divergence rate.
    pub fn is_divergence(self) -> bool {
        !matches!(self, Self::SameRouteSameTarget | Self::ShadowUnusable)
    }
}

/// Classification of a single comparison record.
///
/// `comparable` is false when the record cannot be classified on the evidence
/// available — a failed shadow evaluation, or a target pair that
/// `compare_target` already refused to compare. Such a record still counts
/// toward the total, with category [`ShadowDivergenceCategory::ShadowUnusable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowDivergenceClassification {
    pub category: ShadowDivergenceCategory,
    pub comparable: bool,
}

/// Classify one comparison record.
///
/// The order of the checks is the contract, and it is deliberately
/// "most specific first": a fallback disagreement is reported as
/// [`ShadowDivergenceCategory::FallbackVsSuccess`] even when the route classes
/// also differ, because the degradation is the operationally interesting half.
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

    let category = if record.fallback_diverged == Some(true) {
        ShadowDivergenceCategory::FallbackVsSuccess
    } else {
        match (record.active.route, shadow_decision.route) {
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
                // `None` here means the target pair was not comparable (for
                // example a deterministic route on one side and an asset on the
                // other). The route classes matched, so this is agreement at the
                // only level the evidence supports.
                _ => ShadowDivergenceCategory::SameRouteSameTarget,
            },
        }
    };

    ShadowDivergenceClassification {
        category,
        comparable: true,
    }
}

/// The identity that two records must share before they may be aggregated.
///
/// #165 makes this boundary explicit instead of implicit: aggregating across
/// identities produces a number nobody can interpret, so the aggregator refuses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowCompatibilityKey {
    pub comparison_schema_version: String,
    pub active_policy_id: String,
    pub active_policy_version: String,
    pub active_config_fingerprint: String,
    pub active_runtime_profile: String,
    pub shadow_policy_id: String,
    pub shadow_policy_version: String,
    pub shadow_config_fingerprint: String,
    pub shadow_runtime_profile: String,
    pub dataset_id: Option<String>,
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
            shadow_policy_id: record.shadow_policy.policy_id.clone(),
            shadow_policy_version: record.shadow_policy.policy_version.clone(),
            shadow_config_fingerprint: record.shadow_policy.config_fingerprint.clone(),
            shadow_runtime_profile: record.shadow_policy.runtime_profile.clone(),
            dataset_id: record.shadow_policy.dataset_id.clone(),
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
/// preference order this evidence does not express. Sorting callers use
/// `category` (which is ordered), not the route fields.
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
    /// The bounded-orchestration config version these samples ran under
    /// (#164). Travels with the report so a stored sample is never
    /// re-interpreted under different bounds.
    pub orchestrator_schema_version: String,
    pub compatibility: ShadowCompatibilityKey,
    pub total_comparisons: u64,
    pub comparable_comparisons: u64,
    pub diverged_comparisons: u64,
    /// Count per category. Every category is present, including zeroes, so two
    /// reports compare field by field.
    pub category_counts: BTreeMap<String, u64>,
    /// `#58`-shaped flat metrics (`shadow.divergence.<category>_count`, plus
    /// `shadow.divergence.rate_pct`).
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
/// Refuses, rather than merges, when the batch is empty (an empty report would
/// read as "no divergence"), when a record's comparison `schema_version` differs
/// from the current one, or when any record's identity differs from the first
/// record's. `case_limit` bounds retained references only, never counts.
pub fn summarize_shadow_divergence(
    records: &[ShadowComparisonRecord],
    case_limit: usize,
) -> Result<ShadowDivergenceReport, AppError> {
    let Some(first) = records.first() else {
        return Err(AppError::Routing(
            "shadow divergence summary requires at least one comparison record (#165)".to_owned(),
        ));
    };

    let compatibility = ShadowCompatibilityKey::from_record(first);
    for record in records {
        if record.schema_version != SHADOW_COMPARISON_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "shadow divergence summary refused: record {} carries comparison schema_version {:?}, current is {:?}; samples from different comparison versions must not be combined (#165)",
                record.event_id, record.schema_version, SHADOW_COMPARISON_SCHEMA_VERSION
            )));
        }
        let key = ShadowCompatibilityKey::from_record(record);
        if key != compatibility {
            return Err(AppError::Routing(format!(
                "shadow divergence summary refused: record {} has identity {key:?}, expected {compatibility:?}; records from different policy/config/profile identities must not be aggregated (#165)",
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
    let metrics = category_counts
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
        .chain(std::iter::once((
            "shadow.divergence.rate_pct".to_owned(),
            ShadowReportMetric {
                // Divergence over *comparable* comparisons only. Dividing by all
                // records would let a broken shadow policy depress the rate and
                // make it look like a good one.
                value: if comparable_comparisons == 0 {
                    0.0
                } else {
                    diverged_total as f64 * 100.0 / comparable_comparisons as f64
                },
                sample_count: comparable_comparisons,
            },
        )))
        .collect();

    Ok(ShadowDivergenceReport {
        schema_version: SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION.to_owned(),
        orchestrator_schema_version: SHADOW_ORCHESTRATOR_SCHEMA_VERSION.to_owned(),
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

/// [`summarize_shadow_divergence`] with [`DEFAULT_DIVERGENT_CASE_LIMIT`].
pub fn summarize_shadow_divergence_default(
    records: &[ShadowComparisonRecord],
) -> Result<ShadowDivergenceReport, AppError> {
    summarize_shadow_divergence(records, DEFAULT_DIVERGENT_CASE_LIMIT)
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
