//! Issue #77: make shadow moderation recommendations measurable offline, with
//! zero destructive side effects and no path to automatic promotion.
//!
//! Five rules shape this module, each one a place where an evidence layer can
//! quietly become an authority:
//!
//! 1. **A recommendation is not an action.** [`ModerationRecommendation`] is a
//!    value returned by a *shadow* policy. Nothing here executes it, and no
//!    function in this module can reach the active route. Deletion, timeout and
//!    ban are recommendations with a rank, not operations.
//! 2. **Evidence, never promotion.** A report states how often a shadow policy
//!    disagreed with a reviewer, at what severity, and where reviewers
//!    overrode it. There is no scalar that ranks a policy and no field naming a
//!    policy to switch to. Severity thresholds are deliberately absent: #77
//!    requires them to be calibrated from measured evidence, so inventing
//!    numbers here would be worse than having none.
//! 3. **Refuse rather than merge.** Records from different policy identities,
//!    reviewer identities, comparison schema versions or orchestration bounds
//!    are refused. A rate computed across reviewers or bounds cannot be
//!    interpreted.
//! 4. **Bounds live on the record.** Orchestration identity travels with each
//!    [`ModerationComparisonRecord`] and is cross-checked against the config
//!    the caller supplies, so two batches run under different bounds cannot be
//!    concatenated into one confident report.
//! 5. **Suppression is the asymmetric harm.** Blocking legitimate criticism is
//!    counted explicitly (`benign_suppressed`) rather than folded into generic
//!    accuracy, because a classifier that is silent about benign criticism looks
//!    good on any aggregate that only measures recall of abuse.

use crate::AppError;
use crate::shadow::{ShadowOrchestratorConfig, ShadowPolicyIdentity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema version of the moderation divergence report itself.
///
/// Versioned independently of the comparison record schema so a stored report
/// can be read alongside the records it summarises.
pub const MODERATION_DIVERGENCE_REPORT_SCHEMA_VERSION: &str = "1";

/// Version of the comparison-record schema this module reads and writes.
pub const MODERATION_COMPARISON_SCHEMA_VERSION: &str = "0.1.0";

/// How many divergent case references a report retains by default.
pub const DEFAULT_MODERATION_CASE_LIMIT: usize = 64;

/// What a shadow moderation policy recommended.
///
/// Ordered by *lifecycle*, not by severity: this is the order in which #77
/// permits automation, cheapest-and-safest first. [`Self::severity`] ranks
/// destructiveness independently, because the two orderings are not the same —
/// `ban` is the most severe action but the issue keeps it human-approved by
/// default, so it is last in the automation lifecycle *and* highest in
/// severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationAction {
    /// Take no moderation action; optionally no engagement either.
    Allow,
    /// Suppress rewards/visibility without removing the message.
    Ignore,
    /// Remove the message.
    Delete,
    /// Temporarily mute the author.
    Timeout,
    /// Remove the author permanently.
    Ban,
    /// Route to a human moderator for a decision.
    HumanReview,
}

impl ModerationAction {
    /// Stable snake_case name, used as the JSON key and metric suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ignore => "ignore",
            Self::Delete => "delete",
            Self::Timeout => "timeout",
            Self::Ban => "ban",
            Self::HumanReview => "human_review",
        }
    }

    /// True when acting on this recommendation would destroy or restrict
    /// something the author can observe.
    ///
    /// `HumanReview` is deliberately excluded: escalating to a human is not
    /// itself a destructive act, it is the escape hatch that keeps `ban`
    /// human-approved by default.
    ///
    /// The distinction is the point of the issue: a false positive that
    /// suppresses benign criticism is the harm that a recall-only metric hides.
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Delete | Self::Timeout | Self::Ban)
    }
}

/// Why a case was classified the way it was.
///
/// Deliberately **not** a single toxicity score. #77's non-goals rule out one
/// opaque number as the only decision basis, so the category and the reason are
/// separate fields and both are retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationCategory {
    /// No moderation concern; includes benign criticism, teasing and
    /// disagreement, which are protected cases rather than violations.
    None,
    Spam,
    Flood,
    Harassment,
    Scam,
    HateSpeech,
    Threat,
    SexualContent,
    Impersonation,
}

impl ModerationCategory {
    /// Stable snake_case name, used as the JSON key and metric suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Spam => "spam",
            Self::Flood => "flood",
            Self::Harassment => "harassment",
            Self::Scam => "scam",
            Self::HateSpeech => "hate_speech",
            Self::Threat => "threat",
            Self::SexualContent => "sexual_content",
            Self::Impersonation => "impersonation",
        }
    }

    /// Every category, in a fixed order, so two reports compare field by field.
    pub const ALL: [Self; 9] = [
        Self::None,
        Self::Spam,
        Self::Flood,
        Self::Harassment,
        Self::Scam,
        Self::HateSpeech,
        Self::Threat,
        Self::SexualContent,
        Self::Impersonation,
    ];

    /// True when the category represents an actual policy concern.
    ///
    /// `none` is the protected-benign bucket: cases where a policy said "allow"
    /// and a reviewer agreed are not abuse, and `benign_suppressed` counts the
    /// times that bucket was suppressed anyway.
    pub fn is_violation(self) -> bool {
        self != Self::None
    }
}

/// How certain a reviewer was that the label is the only acceptable one.
///
/// #77 requires that multiple acceptable outcomes be representable, so
/// `multiple_acceptable` and `insufficient_context` are first-class rather than
/// an "unknown" bucket lumped in with disagreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationAmbiguity {
    /// One acceptable outcome.
    Low,
    /// Several outcomes are defensible; a reviewer may pick any of them.
    MultipleAcceptable,
    /// The available context does not settle it.
    InsufficientContext,
}

/// A reviewer's label for one case.
///
/// `acceptable_actions` is a **set**, not a single value, so a reviewer can say
/// "ignore or delete are both fine" without the evidence layer forcing a
/// consensus that the policy does not actually have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationReview {
    /// Opaque reviewer identifier, used only to preserve disagreement.
    ///
    /// Deliberately not a performance score: reviewers disagree, and encoding a
    /// score here would turn "disagreement" into "one reviewer is wrong".
    pub reviewer_id: String,
    pub label: ModerationAction,
    pub category: ModerationCategory,
    pub ambiguity: ModerationAmbiguity,
    /// Every action this reviewer considered acceptable. Non-empty, and always
    /// contains `label` so the two cannot contradict each other.
    pub acceptable_actions: Vec<ModerationAction>,
    /// Whether the case was decided to require a human rather than a policy.
    pub requires_human_review: bool,
}

impl ModerationReview {
    /// True when this reviewer's label falls inside the acceptable set.
    ///
    /// Used to score an agreement without first collapsing a multi-acceptable
    /// case into one "true" answer.
    pub fn accepts(&self, action: ModerationAction) -> bool {
        self.acceptable_actions.contains(&action)
    }

    /// True when the reviewer held that this action would be wrong.
    ///
    /// Only meaningful when the label is *not* in the acceptable set; a reviewer
    /// who marks something ambiguous may still list several actions.
    pub fn rejects(&self, action: ModerationAction) -> bool {
        !self.accepts(action)
    }
}

/// A versioned identity for the shadow moderation policy under evaluation.
///
/// Private fields and a single `from_config` constructor, for the same reason
/// as the shadow orchestration identity (#165): a hand-writable identity would
/// let a report claim a policy version nobody verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationPolicyIdentity {
    policy_id: String,
    policy_version: String,
    config_fingerprint: String,
    reviewer_protocol_version: String,
    dataset_id: Option<String>,
}

impl ModerationPolicyIdentity {
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    pub fn policy_version(&self) -> &str {
        &self.policy_version
    }

    pub fn config_fingerprint(&self) -> &str {
        &self.config_fingerprint
    }

    pub fn reviewer_protocol_version(&self) -> &str {
        &self.reviewer_protocol_version
    }

    pub fn dataset_id(&self) -> Option<&str> {
        self.dataset_id.as_deref()
    }

    /// Derive the identity from the policy identity actually in use plus the
    /// reviewer protocol version, so a rate cannot be compared across protocols.
    pub fn from_policy(
        policy: &ShadowPolicyIdentity,
        reviewer_protocol_version: &str,
    ) -> Result<Self, AppError> {
        // `ShadowPolicyIdentity::validate` is module-private to `shadow`, so
        // the same four fields are checked here rather than reaching into it.
        for (name, value) in [
            ("policy_id", policy.policy_id.as_str()),
            ("policy_version", policy.policy_version.as_str()),
            ("config_fingerprint", policy.config_fingerprint.as_str()),
            ("runtime_profile", policy.runtime_profile.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(AppError::Routing(format!(
                    "shadow moderation identity refused: policy {name} must not be empty (#77)"
                )));
            }
        }
        if reviewer_protocol_version.trim().is_empty() {
            return Err(AppError::Routing(
                "shadow moderation identity refused: reviewer_protocol_version must not be empty (#77)"
                    .to_owned(),
            ));
        }
        Ok(Self {
            policy_id: policy.policy_id.clone(),
            policy_version: policy.policy_version.clone(),
            config_fingerprint: policy.config_fingerprint.clone(),
            reviewer_protocol_version: reviewer_protocol_version.to_owned(),
            dataset_id: policy.dataset_id.clone(),
        })
    }
}

/// The bounded-orchestration identity a moderation batch was produced under.
///
/// Shares the fingerprint construction with [`crate::shadow::ShadowOrchestrationIdentity`]
/// so the two evidence layers agree on what "same bounds" means. `None` on a
/// record means the record has no attributable bounds and must not be summarised
/// under someone else's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationOrchestrationIdentity {
    orchestrator_schema_version: String,
    bounds_fingerprint: String,
}

impl ModerationOrchestrationIdentity {
    pub fn orchestrator_schema_version(&self) -> &str {
        &self.orchestrator_schema_version
    }

    pub fn bounds_fingerprint(&self) -> &str {
        &self.bounds_fingerprint
    }

    /// Derive from a validated config, reusing the shadow fingerprint so a
    /// moderation report and a divergence report describe the same run.
    pub fn from_config(config: &ShadowOrchestratorConfig) -> Result<Self, AppError> {
        let shadow = crate::shadow::ShadowOrchestrationIdentity::from_config(config)?;
        Ok(Self {
            orchestrator_schema_version: shadow.orchestrator_schema_version().to_owned(),
            bounds_fingerprint: shadow.bounds_fingerprint().to_owned(),
        })
    }
}

/// One reviewed moderation case: what the shadow policy recommended and what a
/// reviewer said about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationComparisonRecord {
    pub schema_version: String,
    /// The bounds this record was produced under. `None` marks a record with no
    /// attributable orchestration.
    #[serde(default)]
    pub orchestration: Option<ModerationOrchestrationIdentity>,
    /// Opaque case identifier, not message content.
    pub case_id: String,
    pub correlation_id: String,
    pub policy: ModerationPolicyIdentity,
    pub recommendation: ModerationRecommendation,
    /// Every review of this case, in submission order. Never collapsed: a
    /// disagreement between reviewers is the evidence, so keeping only a
    /// consensus would delete the thing being measured.
    pub reviews: Vec<ModerationReview>,
}

/// A side-effect-free moderation recommendation.
///
/// Nothing in this type executes anything. It is what a shadow policy returns,
/// and it exists only to be scored against reviewer labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationRecommendation {
    pub action: ModerationAction,
    pub category: ModerationCategory,
    /// Policy's own confidence in `0.0..=1.0`, retained for analysis.
    ///
    /// Never used as a decision basis and never thresholded here: #77 forbids
    /// enabling destructive automation from model confidence alone.
    pub confidence_micros: u32,
    pub reason_code: String,
}

impl ModerationRecommendation {
    /// Confidence as a fraction in `0.0..=1.0`.
    pub fn confidence(&self) -> f64 {
        f64::from(self.confidence_micros) / 1_000_000.0
    }

    /// Build from a fraction, rounding to micro-precision.
    pub fn with_confidence(
        action: ModerationAction,
        category: ModerationCategory,
        confidence: f64,
    ) -> Self {
        let clamped = confidence.clamp(0.0, 1.0);
        Self {
            action,
            category,
            confidence_micros: (clamped * 1_000_000.0).round() as u32,
            reason_code: format!("{category:?}_{action:?}").to_lowercase(),
        }
    }
}

/// How one case scored against its reviewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationOutcome {
    /// Every reviewer accepted the recommendation.
    Agreed,
    /// At least one reviewer rejected it.
    Overridden,
    /// Reviewers disagreed with each other, independent of the policy.
    ReviewerDisagreement,
}

/// A reference to one divergent moderation case, safe for offline review.
///
/// Holds identifiers and labels only. This struct has **no field** that can
/// carry message text, so the privacy boundary is structural rather than a
/// review promise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationDivergentCaseRef {
    pub case_id: String,
    pub correlation_id: String,
    pub recommended_action: ModerationAction,
    pub recommended_category: ModerationCategory,
    pub outcome: ModerationOutcome,
    pub severity: u8,
}

/// One metric value, shaped like a `benchmark-result` metric entry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationReportMetric {
    pub value: f64,
    pub sample_count: u64,
}

/// Aggregated moderation evidence for exactly one policy identity and one set of
/// orchestration bounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerationDivergenceReport {
    pub schema_version: String,
    pub orchestration: ModerationOrchestrationIdentity,
    pub policy: ModerationPolicyIdentity,
    pub total_cases: u64,
    pub comparable_cases: u64,
    pub diverged_cases: u64,
    pub agreed_cases: u64,
    pub overridden_cases: u64,
    pub reviewer_disagreement_cases: u64,
    /// Cases labelled benign (`category: none`) that the policy recommended
    /// suppressing. Counted separately from every other error because it is the
    /// harm a recall-only metric hides.
    pub benign_suppressed_cases: u64,
    /// Destructive recommendations that at least one reviewer rejected.
    pub destructive_overridden_cases: u64,
    /// Category count per recommendation. Every category present, including
    /// zeroes, so two reports compare field by field.
    pub category_counts: BTreeMap<String, u64>,
    /// Outcome count per category pair, keyed `<category>/<action>`.
    pub outcome_counts: BTreeMap<String, u64>,
    pub metrics: BTreeMap<String, ModerationReportMetric>,
    pub divergent_cases: Vec<ModerationDivergentCaseRef>,
    pub divergent_cases_truncated: bool,
    /// Always `OfflineEvidenceOnly`. Present as a value rather than an
    /// assumption so a consumer reading the report sees the limit stated, and
    /// typed so it cannot carry anything else.
    pub authority_effect: AuthorityEffect,
}

/// What a moderation report is allowed to authorize: nothing.
///
/// The issue's central boundary, expressed as a type rather than a convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AuthorityEffect {
    /// Evidence only. Cannot authorize execution, promotion, or any runtime
    /// control. The sole variant, so `Default` lands here.
    #[default]
    #[serde(rename = "offline_evidence_only")]
    OfflineEvidenceOnly,
}

/// Score one case against its reviews.
///
/// Three outcomes, and they are not mutually exclusive in the way a naive
/// two-way agreement metric would be: reviewer disagreement is reported
/// independently, because a policy can be "right" and still be unusable if
/// reviewers cannot agree on what right means.
pub fn classify_moderation_case(record: &ModerationComparisonRecord) -> (ModerationOutcome, bool) {
    let rejected: Vec<&ModerationReview> = record
        .reviews
        .iter()
        .filter(|review| review.rejects(record.recommendation.action))
        .collect();

    let mut reviewer_disagreement = false;
    for (index, left) in record.reviews.iter().enumerate() {
        for right in record.reviews.iter().skip(index + 1) {
            if !left.accepts(right.label) {
                reviewer_disagreement = true;
                break;
            }
        }
        if reviewer_disagreement {
            break;
        }
    }

    let outcome = if !rejected.is_empty() {
        ModerationOutcome::Overridden
    } else if reviewer_disagreement {
        ModerationOutcome::ReviewerDisagreement
    } else {
        ModerationOutcome::Agreed
    };
    (outcome, reviewer_disagreement)
}

/// Aggregate moderation comparison records into one report.
///
/// Refuses, rather than merges, when the batch is empty (an empty report would
/// read as "no disagreements"), the config is invalid, a record's stamped
/// orchestration identity is absent or differs from the one `config` describes,
/// a record carries a different comparison `schema_version`, or any record's
/// policy identity differs from the first record's. `case_limit` bounds
/// retained references only, never counts.
pub fn summarize_moderation_divergence(
    records: &[ModerationComparisonRecord],
    config: &ShadowOrchestratorConfig,
    case_limit: usize,
) -> Result<ModerationDivergenceReport, AppError> {
    let orchestration = ModerationOrchestrationIdentity::from_config(config)?;

    let Some(first) = records.first() else {
        return Err(AppError::Routing(
            "moderation divergence summary requires at least one comparison record (#77)"
                .to_owned(),
        ));
    };
    let policy = first.policy.clone();

    let mut total_cases = 0u64;
    let mut comparable_cases = 0u64;
    let mut diverged_cases = 0u64;
    let mut agreed_cases = 0u64;
    let mut overridden_cases = 0u64;
    let mut reviewer_disagreement_cases = 0u64;
    let mut benign_suppressed_cases = 0u64;
    let mut destructive_overridden_cases = 0u64;

    let mut category_counts: BTreeMap<String, u64> = ModerationCategory::ALL
        .iter()
        .map(|category| (category.as_str().to_owned(), 0))
        .collect();
    let mut outcome_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut divergent_cases = Vec::new();

    for record in records {
        if record.schema_version != MODERATION_COMPARISON_SCHEMA_VERSION {
            return Err(AppError::Routing(format!(
                "moderation divergence refused: case {} carries comparison schema_version {:?}, current is {:?}; samples from different comparison versions must not be combined (#77)",
                record.case_id, record.schema_version, MODERATION_COMPARISON_SCHEMA_VERSION
            )));
        }
        match &record.orchestration {
            None => {
                return Err(AppError::Routing(format!(
                    "moderation divergence refused: case {} carries no orchestration identity; unattributed evidence must not be reported under someone else's bounds (#77)",
                    record.case_id
                )));
            }
            Some(stamped) if *stamped != orchestration => {
                return Err(AppError::Routing(format!(
                    "moderation divergence refused: case {} was produced under orchestration bounds {} ({}), but the supplied config describes {} ({}); batches from different bounds must not be combined (#77)",
                    record.case_id,
                    stamped.orchestrator_schema_version(),
                    stamped.bounds_fingerprint(),
                    orchestration.orchestrator_schema_version(),
                    orchestration.bounds_fingerprint(),
                )));
            }
            Some(_) => {}
        }
        if record.policy != policy {
            return Err(AppError::Routing(format!(
                "moderation divergence refused: case {} has policy {:?}, expected {:?}; records from different moderation policies or reviewer protocols must not be aggregated (#77)",
                record.case_id, record.policy, policy
            )));
        }
        if record.reviews.is_empty() {
            return Err(AppError::Routing(format!(
                "moderation divergence refused: case {} has no reviewer label; an unreviewed case cannot be scored (#77)",
                record.case_id
            )));
        }

        total_cases += 1;
        comparable_cases += 1;

        let action = record.recommendation.action;
        let category = record.recommendation.category;
        if let Some(count) = category_counts.get_mut(category.as_str()) {
            *count += 1;
        }

        let (outcome, reviewer_disagreement) = classify_moderation_case(record);
        *outcome_counts
            .entry(format!("{}/{}", category.as_str(), action.as_str()))
            .or_insert(0) += 1;

        if reviewer_disagreement {
            reviewer_disagreement_cases += 1;
        }
        let overrode = outcome == ModerationOutcome::Overridden;
        if overrode {
            overridden_cases += 1;
            diverged_cases += 1;
            if action.is_destructive() {
                destructive_overridden_cases += 1;
            }
            // A suppression of a case every reviewer called benign is the
            // asymmetric harm. Benign-ness is the *reviewers'* call and never
            // the policy's own category: a policy that labels criticism as
            // harassment would otherwise never be counted as suppressing
            // benign content, which is precisely the harm this counter exists
            // to surface.
            if action != ModerationAction::Allow && reviewed_as_benign(record) {
                benign_suppressed_cases += 1;
            }
        } else {
            agreed_cases += 1;
        }

        if divergent_cases.len() < case_limit {
            divergent_cases.push(ModerationDivergentCaseRef {
                case_id: record.case_id.clone(),
                correlation_id: record.correlation_id.clone(),
                recommended_action: action,
                recommended_category: category,
                outcome,
                severity: action_severity(action),
            });
        }
    }

    let truncated = diverged_cases > divergent_cases.len() as u64;
    let sample_count = total_cases;

    let mut metrics: BTreeMap<String, ModerationReportMetric> = category_counts
        .iter()
        .map(|(category, count)| {
            (
                format!("shadow.moderation.{category}_count"),
                ModerationReportMetric {
                    value: *count as f64,
                    sample_count,
                },
            )
        })
        .collect();
    for (name, value) in [
        ("agreed", agreed_cases),
        ("overridden", overridden_cases),
        ("reviewer_disagreement", reviewer_disagreement_cases),
        ("benign_suppressed", benign_suppressed_cases),
        ("destructive_overridden", destructive_overridden_cases),
        ("total", total_cases),
    ] {
        metrics.insert(
            format!("shadow.moderation.{name}_count"),
            ModerationReportMetric {
                value: value as f64,
                sample_count,
            },
        );
    }
    metrics.insert(
        "shadow.moderation.override_rate_pct".to_owned(),
        ModerationReportMetric {
            // Over comparable cases only. Reported with its sample count so a
            // consumer can see how thin the denominator is.
            value: if comparable_cases == 0 {
                0.0
            } else {
                overridden_cases as f64 * 100.0 / comparable_cases as f64
            },
            sample_count: comparable_cases,
        },
    );

    Ok(ModerationDivergenceReport {
        schema_version: MODERATION_DIVERGENCE_REPORT_SCHEMA_VERSION.to_owned(),
        orchestration,
        policy,
        total_cases,
        comparable_cases,
        diverged_cases,
        agreed_cases,
        overridden_cases,
        reviewer_disagreement_cases,
        benign_suppressed_cases,
        destructive_overridden_cases,
        category_counts,
        outcome_counts,
        metrics,
        divergent_cases,
        divergent_cases_truncated: truncated,
        authority_effect: AuthorityEffect::OfflineEvidenceOnly,
    })
}

/// True when every reviewer labelled the case benign.
///
/// Deliberately unanimity rather than a majority: with one reviewer, "the
/// reviewers think this is benign" and "nobody objected" are the same claim, and
/// a majority rule would let a destructive recommendation survive a split
/// review on a case one reviewer considered harmless.
fn reviewed_as_benign(record: &ModerationComparisonRecord) -> bool {
    !record.reviews.is_empty()
        && record
            .reviews
            .iter()
            .all(|review| review.category == ModerationCategory::None)
}

/// [`summarize_moderation_divergence`] with [`DEFAULT_MODERATION_CASE_LIMIT`].
pub fn summarize_moderation_divergence_default(
    records: &[ModerationComparisonRecord],
    config: &ShadowOrchestratorConfig,
) -> Result<ModerationDivergenceReport, AppError> {
    summarize_moderation_divergence(records, config, DEFAULT_MODERATION_CASE_LIMIT)
}

/// Destructiveness rank, independent of the automation lifecycle order.
///
/// `HumanReview` is absent on purpose: escalating to a human is not itself
/// destructive, it is the escape hatch that keeps `ban` human-approved.
fn action_severity(action: ModerationAction) -> u8 {
    match action {
        ModerationAction::Allow => 0,
        ModerationAction::Ignore => 1,
        ModerationAction::HumanReview => 2,
        ModerationAction::Delete => 3,
        ModerationAction::Timeout => 4,
        ModerationAction::Ban => 5,
    }
}
