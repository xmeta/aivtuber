//! Issue #77: shadow moderation evidence integration.
//!
//! The properties under test are the ones where a moderation evidence layer
//! typically goes wrong:
//!
//! 1. **Protected benign cases.** Criticism, teasing and disagreement are not
//!    harassment. A classifier that suppresses them is the harm a recall-only
//!    metric hides, so `benign_suppressed` is counted on its own.
//! 2. **Outliers survive aggregation.** 99 benign messages plus one threat must
//!    leave the threat independently countable — the case #77 calls out
//!    explicitly for #79's lossy crowd aggregation.
//! 3. **Severity and lifecycle are different orderings.** `ban` is the most
//!    destructive action and simultaneously the one that stays human-approved.
//! 4. **Disagreement is retained.** Reviewers who disagree are evidence, so
//!    neither the module nor the schema may collapse them into a consensus.
//! 5. **Evidence never becomes authority.** Nothing here executes a
//!    recommendation, and a report carries no scalar that would rank a policy.

use aivtuber_app::{
    AuthorityEffect, MODERATION_COMPARISON_SCHEMA_VERSION,
    MODERATION_DIVERGENCE_REPORT_SCHEMA_VERSION, ModerationAction, ModerationAmbiguity,
    ModerationCategory, ModerationComparisonRecord, ModerationDivergenceReport,
    ModerationOrchestrationIdentity, ModerationOutcome, ModerationPolicyIdentity,
    ModerationRecommendation, ModerationReview, ShadowOrchestratorConfig, ShadowPolicyIdentity,
    classify_moderation_case, summarize_moderation_divergence,
    summarize_moderation_divergence_default,
};

fn shadow_identity() -> ShadowPolicyIdentity {
    ShadowPolicyIdentity {
        policy_id: "moderation-shadow".to_owned(),
        policy_version: "0.4.0".to_owned(),
        config_fingerprint: "cfg-mod-1".to_owned(),
        runtime_profile: "default".to_owned(),
        dataset_id: Some("mod-dataset-2026-10".to_owned()),
    }
}

fn policy() -> ModerationPolicyIdentity {
    ModerationPolicyIdentity::from_policy(&shadow_identity(), "protocol-1")
        .expect("the fixture policy identity is complete")
}

fn config() -> ShadowOrchestratorConfig {
    ShadowOrchestratorConfig::default()
}

fn review(
    reviewer: &str,
    label: ModerationAction,
    acceptable: &[ModerationAction],
    category: ModerationCategory,
    ambiguity: ModerationAmbiguity,
) -> ModerationReview {
    let mut acceptable_actions = acceptable.to_vec();
    if !acceptable_actions.contains(&label) {
        acceptable_actions.push(label);
    }
    acceptable_actions.sort();
    acceptable_actions.dedup();
    ModerationReview {
        reviewer_id: reviewer.to_owned(),
        label,
        category,
        ambiguity,
        acceptable_actions,
        requires_human_review: matches!(label, ModerationAction::HumanReview),
    }
}

fn record(
    case_id: &str,
    action: ModerationAction,
    category: ModerationCategory,
    reviews: Vec<ModerationReview>,
) -> ModerationComparisonRecord {
    ModerationComparisonRecord {
        schema_version: MODERATION_COMPARISON_SCHEMA_VERSION.to_owned(),
        orchestration: Some(
            ModerationOrchestrationIdentity::from_config(&config()).expect("valid config"),
        ),
        case_id: case_id.to_owned(),
        correlation_id: format!("corr-{case_id}"),
        policy: policy(),
        recommendation: ModerationRecommendation::with_confidence(action, category, 0.82),
        reviews,
    }
}

fn benign_agreement(case_id: &str) -> ModerationComparisonRecord {
    record(
        case_id,
        ModerationAction::Allow,
        ModerationCategory::None,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::None,
            ModerationAmbiguity::Low,
        )],
    )
}

fn summarize(
    records: &[ModerationComparisonRecord],
) -> Result<ModerationDivergenceReport, aivtuber_app::AppError> {
    summarize_moderation_divergence_default(records, &config())
}

// ---------------------------------------------------------------------------
// 1. Protected benign cases
// ---------------------------------------------------------------------------

#[test]
fn criticism_sarcasm_and_disagreement_are_not_violations() {
    // #77's protected cases: the module must be able to *represent* "allow" as
    // a first-class labelled outcome rather than as an absence of a decision.
    assert!(!ModerationCategory::None.is_violation());
    for category in ModerationCategory::ALL {
        if category != ModerationCategory::None {
            assert!(category.is_violation(), "{category:?} must be a violation");
        }
    }
}

#[test]
fn suppressing_a_benign_case_is_counted_as_benign_suppressed() {
    // A reviewer says the case is benign; the policy recommended `ignore`.
    let records = vec![record(
        "evt-criticism",
        ModerationAction::Ignore,
        ModerationCategory::None,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::None,
            ModerationAmbiguity::Low,
        )],
    )];

    let report = summarize(&records).expect("summary");

    assert_eq!(report.benign_suppressed_cases, 1);
    assert_eq!(report.overridden_cases, 1);
    assert_eq!(
        report.metrics["shadow.moderation.benign_suppressed_count"].value, 1.0,
        "benign suppression must be visible as its own metric, not folded into accuracy"
    );
}

/// The policy's own category is *what is being evaluated*, so it cannot decide
/// whether its own error was benign. A policy that labels criticism as
/// harassment would otherwise never appear in `benign_suppressed`.
#[test]
fn benign_suppression_follows_the_reviewer_not_the_policy_category() {
    // The policy says harassment; both reviewers say the case is not abuse.
    let records = vec![record(
        "evt-mislabelled",
        ModerationAction::Delete,
        ModerationCategory::Harassment,
        vec![
            review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
            review(
                "rev-b",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
        ],
    )];

    let report = summarize(&records).expect("summary");

    assert_eq!(
        report.benign_suppressed_cases, 1,
        "the reviewer's label, not the policy's, decides benign suppression"
    );
    assert_eq!(report.category_counts["harassment"], 1);
}

/// One reviewer considering a case benign is not enough when another calls it a
/// real violation: the case is not unambiguously benign, so it is not counted
/// as benign suppression.
#[test]
fn a_split_review_is_not_counted_as_benign_suppression() {
    let records = vec![record(
        "evt-split",
        ModerationAction::Delete,
        ModerationCategory::None,
        vec![
            review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ModerationAmbiguity::MultipleAcceptable,
            ),
            review(
                "rev-b",
                ModerationAction::Delete,
                &[ModerationAction::Delete],
                ModerationCategory::Harassment,
                ModerationAmbiguity::Low,
            ),
        ],
    )];

    let report = summarize(&records).expect("summary");
    assert_eq!(
        report.benign_suppressed_cases, 0,
        "unanimity is required: a split review is not a benign case"
    );
    assert_eq!(report.reviewer_disagreement_cases, 1);
}

#[test]
fn an_allowed_benign_case_is_never_a_suppression() {
    let records = vec![benign_agreement("evt-allowed")];
    let report = summarize(&records).expect("summary");
    assert_eq!(report.benign_suppressed_cases, 0);
    assert_eq!(report.agreed_cases, 1);
}

#[test]
fn a_suppressed_violation_is_not_counted_as_benign_suppression() {
    let records = vec![record(
        "evt-spam",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Spam,
            ModerationAmbiguity::Low,
        )],
    )];

    let report = summarize(&records).expect("summary");
    assert_eq!(
        report.benign_suppressed_cases, 0,
        "a missed spam case is a false negative, not benign suppression"
    );
    assert_eq!(report.overridden_cases, 1);
}

// ---------------------------------------------------------------------------
// 2. Outliers survive aggregation (#77's explicit #79 protection case)
// ---------------------------------------------------------------------------

#[test]
fn one_threat_among_many_benign_messages_stays_independently_countable() {
    let mut records = vec![benign_agreement("evt-bench")];
    for index in 0..98 {
        records.push(benign_agreement(&format!("evt-benign-{index}")));
    }
    // The single outlier: an explicit threat the reviewer agrees should be
    // escalated. It must remain visible even though 99 neighbours are benign.
    records.push(record(
        "evt-threat",
        ModerationAction::HumanReview,
        ModerationCategory::Threat,
        vec![review(
            "rev-a",
            ModerationAction::HumanReview,
            &[ModerationAction::HumanReview, ModerationAction::Ban],
            ModerationCategory::Threat,
            ModerationAmbiguity::Low,
        )],
    ));

    let report = summarize(&records).expect("summary");

    assert_eq!(report.total_cases, 100);
    assert_eq!(
        report.category_counts["threat"], 1,
        "the outlier must survive aggregation as its own countable category"
    );
    assert_eq!(
        report.outcome_counts["threat/human_review"], 1,
        "and it must remain addressable by category and action"
    );
    assert_eq!(report.overridden_cases, 0);
    // 99 benign agreements plus the agreed escalation.
    assert_eq!(report.agreed_cases, 100);
}

#[test]
fn a_benign_crowd_cannot_dilute_a_rejected_outlier() {
    // The converse: if the policy *suppressed* the benign crowd, those are
    // suppressed-benign cases, and the report must say so rather than reporting
    // a clean aggregate.
    let mut records = Vec::new();
    for index in 0..50 {
        records.push(record(
            &format!("evt-crowd-{index}"),
            ModerationAction::Ignore,
            ModerationCategory::None,
            vec![review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            )],
        ));
    }
    records.push(record(
        "evt-scam",
        ModerationAction::Allow,
        ModerationCategory::Scam,
        vec![review(
            "rev-a",
            ModerationAction::HumanReview,
            &[ModerationAction::HumanReview],
            ModerationCategory::Scam,
            ModerationAmbiguity::Low,
        )],
    ));

    let report = summarize(&records).expect("summary");

    assert_eq!(report.benign_suppressed_cases, 50);
    assert_eq!(report.overridden_cases, 51);
    assert_eq!(
        report.metrics["shadow.moderation.benign_suppressed_count"].value,
        50.0
    );
}

// ---------------------------------------------------------------------------
// 3. Severity vs lifecycle ordering
// ---------------------------------------------------------------------------

#[test]
fn severity_and_lifecycle_are_independent_orderings() {
    // Declaration order is the automation lifecycle: allow -> ignore -> delete
    // -> timeout -> ban, with human review alongside. Destructiveness is
    // ranked separately, and `human_review` is *not* destructive because it is
    // the escape hatch that keeps `ban` human-approved.
    let lifecycle = [
        ModerationAction::Allow,
        ModerationAction::Ignore,
        ModerationAction::Delete,
        ModerationAction::Timeout,
        ModerationAction::Ban,
    ];
    assert!(ModerationAction::Allow < ModerationAction::Ignore);
    assert!(ModerationAction::Ignore < ModerationAction::Delete);
    assert!(ModerationAction::Delete < ModerationAction::Timeout);
    assert!(ModerationAction::Timeout < ModerationAction::Ban);
    assert_eq!(lifecycle.len(), 5);

    assert!(!ModerationAction::Allow.is_destructive());
    assert!(!ModerationAction::Ignore.is_destructive());
    assert!(ModerationAction::Delete.is_destructive());
    assert!(ModerationAction::Timeout.is_destructive());
    assert!(ModerationAction::Ban.is_destructive());
    assert!(
        !ModerationAction::HumanReview.is_destructive(),
        "escalating to a human is not itself a destructive act"
    );
}

#[test]
fn a_rejected_destructive_recommendation_is_counted_separately() {
    let records = vec![record(
        "evt-ban",
        ModerationAction::Ban,
        ModerationCategory::Harassment,
        vec![review(
            "rev-a",
            ModerationAction::Ignore,
            &[ModerationAction::Ignore, ModerationAction::Delete],
            ModerationCategory::Harassment,
            ModerationAmbiguity::Low,
        )],
    )];

    let report = summarize(&records).expect("summary");
    assert_eq!(report.destructive_overridden_cases, 1);
    assert_eq!(report.overridden_cases, 1);
}

// ---------------------------------------------------------------------------
// 4. Disagreement and ambiguity are retained
// ---------------------------------------------------------------------------

#[test]
fn reviewer_disagreement_is_retained_not_collapsed() {
    // rev-a accepts ignore, rev-b insists on delete. The policy recommended
    // delete: rev-a overrode, and the two reviewers also disagree with each
    // other. Both facts must survive.
    let records = vec![record(
        "evt-disagree",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![
            review(
                "rev-a",
                ModerationAction::Ignore,
                &[ModerationAction::Ignore],
                ModerationCategory::Spam,
                ModerationAmbiguity::MultipleAcceptable,
            ),
            review(
                "rev-b",
                ModerationAction::Delete,
                &[ModerationAction::Delete],
                ModerationCategory::Spam,
                ModerationAmbiguity::Low,
            ),
        ],
    )];

    let report = summarize(&records).expect("summary");
    assert_eq!(report.reviewer_disagreement_cases, 1);
    assert_eq!(report.overridden_cases, 1);
    assert_eq!(
        report.divergent_cases[0].outcome,
        ModerationOutcome::Overridden
    );
}

#[test]
fn a_case_with_multiple_acceptable_actions_is_not_scored_as_an_error() {
    // The policy recommended `ignore`, which the reviewer listed as acceptable
    // even though their own label was `delete`.
    let records = vec![record(
        "evt-ambiguous",
        ModerationAction::Ignore,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Delete,
            &[ModerationAction::Delete, ModerationAction::Ignore],
            ModerationCategory::Spam,
            ModerationAmbiguity::MultipleAcceptable,
        )],
    )];

    let (outcome, disagreement) = classify_moderation_case(&records[0]);
    assert_eq!(
        outcome,
        ModerationOutcome::Agreed,
        "an action inside the acceptable set is not an override"
    );
    assert!(!disagreement);

    let report = summarize(&records).expect("summary");
    assert_eq!(report.overridden_cases, 0);
    assert_eq!(report.agreed_cases, 1);
}

#[test]
fn insufficient_context_is_a_first_class_ambiguity_not_disagreement() {
    for ambiguity in [
        ModerationAmbiguity::Low,
        ModerationAmbiguity::MultipleAcceptable,
        ModerationAmbiguity::InsufficientContext,
    ] {
        let one = record(
            "evt-amb",
            ModerationAction::Allow,
            ModerationCategory::None,
            vec![review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ambiguity,
            )],
        );
        let (_, disagreement) = classify_moderation_case(&one);
        assert!(!disagreement, "one reviewer cannot disagree with itself");
    }
}

#[test]
fn an_unreviewed_case_is_refused_rather_than_scored_as_agreement() {
    let mut unreviewed = benign_agreement("evt-unreviewed");
    unreviewed.reviews.clear();
    let error = summarize(&[unreviewed]).expect_err("an unreviewed case has no evidence");
    assert!(
        error.to_string().contains("no reviewer label"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// 5. Refuse rather than merge
// ---------------------------------------------------------------------------

#[test]
fn an_empty_batch_is_refused_rather_than_reported_as_no_disagreement() {
    summarize(&[]).expect_err("an empty report would read as 'no problems'");
}

#[test]
fn records_from_different_bounds_cannot_be_reported_as_one_batch() {
    let slower = ShadowOrchestratorConfig {
        deadline_ms: 900,
        ..ShadowOrchestratorConfig::default()
    };
    let mut other = benign_agreement("evt-other-bounds");
    other.orchestration =
        Some(ModerationOrchestrationIdentity::from_config(&slower).expect("valid config"));

    let error = summarize(&[benign_agreement("evt-1"), other])
        .expect_err("a mixed-bounds batch must not be summarised under one config");
    let message = error.to_string();
    assert!(
        message.contains("different bounds"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("evt-other-bounds"),
        "the offending case must be named: {message}"
    );
}

#[test]
fn a_config_that_matches_only_some_records_is_refused() {
    let slower = ShadowOrchestratorConfig {
        deadline_ms: 900,
        ..ShadowOrchestratorConfig::default()
    };
    let mut other = benign_agreement("evt-2");
    other.orchestration =
        Some(ModerationOrchestrationIdentity::from_config(&slower).expect("valid config"));
    let records = vec![benign_agreement("evt-1"), other];

    summarize(&records).expect_err("bounds must agree across the whole batch");
    summarize_moderation_divergence_default(&records, &slower)
        .expect_err("and the other config is refused too");
}

#[test]
fn an_unstamped_record_is_refused() {
    let mut record = benign_agreement("evt-1");
    record.orchestration = None;
    let error = summarize(&[record]).expect_err("unattributed evidence must be refused");
    assert!(
        error.to_string().contains("no orchestration identity"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_a_different_reviewer_protocol_are_refused() {
    let mut other = benign_agreement("evt-2");
    other.policy =
        ModerationPolicyIdentity::from_policy(&shadow_identity(), "protocol-2").expect("valid");
    let error = summarize(&[benign_agreement("evt-1"), other])
        .expect_err("rates across reviewer protocols cannot be compared");
    assert!(
        error.to_string().contains("reviewer protocols"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_a_different_policy_version_are_refused() {
    let mut other = benign_agreement("evt-2");
    other.policy = ModerationPolicyIdentity::from_policy(
        &ShadowPolicyIdentity {
            policy_version: "0.5.0".to_owned(),
            ..shadow_identity()
        },
        "protocol-1",
    )
    .expect("valid");
    summarize(&[benign_agreement("evt-1"), other])
        .expect_err("rates across policy versions cannot be compared");
}

#[test]
fn records_from_a_different_comparison_schema_version_are_refused() {
    let mut other = benign_agreement("evt-2");
    other.schema_version = "0.0.9".to_owned();
    let error = summarize(&[benign_agreement("evt-1"), other])
        .expect_err("samples from different comparison versions must not combine");
    assert!(
        error.to_string().contains("different comparison versions"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_incomplete_policy_identity_is_refused_before_any_report() {
    let error = ModerationPolicyIdentity::from_policy(
        &ShadowPolicyIdentity {
            policy_id: "  ".to_owned(),
            ..shadow_identity()
        },
        "protocol-1",
    )
    .expect_err("a blank policy id is not an identity");
    assert!(
        error.to_string().contains("policy_id"),
        "unexpected error: {error}"
    );

    ModerationPolicyIdentity::from_policy(&shadow_identity(), " ")
        .expect_err("a blank reviewer protocol version is not an identity");
}

#[test]
fn an_invalid_config_is_refused_before_any_report_is_built() {
    let invalid = ShadowOrchestratorConfig {
        deadline_ms: 0,
        ..ShadowOrchestratorConfig::default()
    };
    let error = summarize_moderation_divergence_default(&[benign_agreement("evt-1")], &invalid)
        .expect_err("a config the runtime would reject must be refused");
    assert!(
        error.to_string().contains("deadline_ms"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// 6. Evidence never becomes authority
// ---------------------------------------------------------------------------

#[test]
fn a_report_carries_no_authority_and_no_ranking_scalar() {
    let report = summarize(&[benign_agreement("evt-1")]).expect("summary");
    assert_eq!(
        report.authority_effect,
        AuthorityEffect::OfflineEvidenceOnly
    );

    let encoded = serde_json::to_string(&report).expect("serializes");
    for forbidden in [
        "promote",
        "promotion",
        "auto_apply",
        "active_policy",
        "toxicity_score",
        "trust_score",
        "threshold",
    ] {
        assert!(
            !encoded.to_lowercase().contains(forbidden),
            "a moderation report must not carry {forbidden}: {encoded}"
        );
    }
}

#[test]
fn recommendations_cannot_execute_anything() {
    // The recommendation is a value. Aggregating or serializing it must not
    // reach the runtime, and its severity is never used as a gate.
    let recommendation = ModerationRecommendation::with_confidence(
        ModerationAction::Ban,
        ModerationCategory::Threat,
        0.99,
    );
    let encoded = serde_json::to_string(&recommendation).expect("serializes");
    assert!(encoded.contains("\"ban\""));
    assert!(encoded.contains("\"threat\""));
    // Confidence is retained for analysis and is deliberately not a threshold.
    assert!((recommendation.confidence() - 0.99).abs() < 1e-9);
}

#[test]
fn confidence_is_clamped_rather_than_trusted() {
    let over = ModerationRecommendation::with_confidence(
        ModerationAction::Delete,
        ModerationCategory::Spam,
        4.2,
    );
    assert!(over.confidence() <= 1.0);
    let under = ModerationRecommendation::with_confidence(
        ModerationAction::Delete,
        ModerationCategory::Spam,
        -1.0,
    );
    assert!(under.confidence() >= 0.0);
}

// ---------------------------------------------------------------------------
// 7. Aggregation semantics and the privacy boundary
// ---------------------------------------------------------------------------

#[test]
fn aggregation_counts_every_category_even_when_absent() {
    let report = summarize(&[benign_agreement("evt-1")]).expect("summary");
    for category in ModerationCategory::ALL {
        assert!(
            report.category_counts.contains_key(category.as_str()),
            "missing category {}",
            category.as_str()
        );
    }
    assert_eq!(report.total_cases, 1);
    assert_eq!(report.comparable_cases, 1);
    assert_eq!(report.category_counts["none"], 1);
}

#[test]
fn the_override_rate_uses_comparable_cases_as_its_denominator() {
    let records = vec![
        benign_agreement("evt-1"),
        record(
            "evt-2",
            ModerationAction::Delete,
            ModerationCategory::Spam,
            vec![review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::Spam,
                ModerationAmbiguity::Low,
            )],
        ),
    ];
    let report = summarize(&records).expect("summary");

    assert_eq!(report.overridden_cases, 1);
    assert_eq!(report.comparable_cases, 2);
    let rate = report.metrics["shadow.moderation.override_rate_pct"];
    assert!((rate.value - 50.0).abs() < 1e-9);
    assert_eq!(
        rate.sample_count, 2,
        "a rate without its denominator is not interpretable"
    );
}

#[test]
fn truncating_references_never_truncates_counts() {
    let records: Vec<ModerationComparisonRecord> = (0..5)
        .map(|index| {
            record(
                &format!("evt-{index}"),
                ModerationAction::Delete,
                ModerationCategory::Spam,
                vec![review(
                    "rev-a",
                    ModerationAction::Allow,
                    &[ModerationAction::Allow],
                    ModerationCategory::Spam,
                    ModerationAmbiguity::Low,
                )],
            )
        })
        .collect();

    let report = summarize_moderation_divergence(&records, &config(), 2).expect("summary");

    assert_eq!(report.divergent_cases.len(), 2, "references are capped");
    assert_eq!(report.overridden_cases, 5, "counts stay complete");
    assert_eq!(report.total_cases, 5);
    assert!(report.divergent_cases_truncated);

    let full = summarize_moderation_divergence_default(&records, &config()).expect("summary");
    assert!(!full.divergent_cases_truncated);
    assert_eq!(full.divergent_cases.len(), 5);
}

#[test]
fn serialized_evidence_never_contains_message_text() {
    let records = vec![record(
        "evt-1",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Spam,
            ModerationAmbiguity::Low,
        )],
    )];
    let report = summarize(&records).expect("summary");
    let encoded = serde_json::to_string(&report).expect("serializes");
    assert_no_content_fields(&serde_json::from_str(&encoded).expect("re-parses"));
}

/// Assert that no object key in the encoded evidence can name message content.
///
/// Structural rather than a substring scan: `authority_effect` contains
/// "author", so only the actual field names are inspected. A content leak would
/// arrive as an extra key, and `deny_unknown_fields` on every struct in the
/// module is what prevents one from being added from outside.
fn assert_no_content_fields(value: &serde_json::Value) {
    const FORBIDDEN: [&str; 9] = [
        "message",
        "message_text",
        "body",
        "text",
        "transcript",
        "payload",
        "author",
        "author_id",
        "username",
    ];
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    !FORBIDDEN.contains(&key.as_str()),
                    "moderation evidence must not carry a {key} field"
                );
                assert_no_content_fields(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                assert_no_content_fields(item);
            }
        }
        _ => {}
    }
}

#[test]
fn a_case_reference_carries_identifiers_and_labels_only() {
    let records = vec![record(
        "evt-1",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Spam,
            ModerationAmbiguity::Low,
        )],
    )];
    let report = summarize(&records).expect("summary");
    let reference = &report.divergent_cases[0];

    assert_eq!(reference.case_id, "evt-1");
    assert_eq!(reference.correlation_id, "corr-evt-1");
    assert_eq!(reference.recommended_action, ModerationAction::Delete);
    assert_eq!(reference.recommended_category, ModerationCategory::Spam);
    assert_eq!(reference.outcome, ModerationOutcome::Overridden);
    assert!(
        reference.severity >= 3,
        "delete must rank above the non-destructive actions"
    );
}

#[test]
fn a_report_round_trips_through_json() {
    let records = vec![
        benign_agreement("evt-1"),
        record(
            "evt-2",
            ModerationAction::Delete,
            ModerationCategory::Spam,
            vec![review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::Spam,
                ModerationAmbiguity::Low,
            )],
        ),
    ];
    let report = summarize(&records).expect("summary");
    let encoded = serde_json::to_string(&report).expect("serializes");
    let decoded: ModerationDivergenceReport = serde_json::from_str(&encoded).expect("deserializes");
    assert_eq!(decoded, report);
}

#[test]
fn a_report_carries_the_versions_a_stored_sample_needs() {
    let report = summarize(&[benign_agreement("evt-1")]).expect("summary");
    assert_eq!(
        report.schema_version,
        MODERATION_DIVERGENCE_REPORT_SCHEMA_VERSION
    );
    assert_eq!(report.policy, policy());
    assert_eq!(report.policy.policy_id(), "moderation-shadow");
    assert_eq!(report.policy.policy_version(), "0.4.0");
    assert_eq!(
        report.orchestration,
        ModerationOrchestrationIdentity::from_config(&config()).expect("valid config")
    );
}

#[test]
fn different_bounds_produce_different_moderation_identities() {
    let base = config();
    let fingerprint = |config: &ShadowOrchestratorConfig| {
        ModerationOrchestrationIdentity::from_config(config)
            .expect("valid config")
            .bounds_fingerprint()
            .to_owned()
    };

    let busier = ShadowOrchestratorConfig {
        max_concurrent: base.max_concurrent + 1,
        ..base.clone()
    };
    assert_ne!(
        fingerprint(&base),
        fingerprint(&busier),
        "a different bound changes which evidence a run produces"
    );
    assert_eq!(fingerprint(&base), fingerprint(&base.clone()));
}

#[test]
fn a_different_runtime_profile_cannot_collapse_into_one_identity() {
    let profile_a = ShadowPolicyIdentity {
        runtime_profile: "profile-a".to_owned(),
        ..shadow_identity()
    };
    let profile_b = ShadowPolicyIdentity {
        runtime_profile: "profile-b".to_owned(),
        ..shadow_identity()
    };

    let a = ModerationPolicyIdentity::from_policy(&profile_a, "protocol-1").expect("identity");
    let b = ModerationPolicyIdentity::from_policy(&profile_b, "protocol-1").expect("identity");

    assert_ne!(
        a, b,
        "a runtime profile changes which evidence a run produces, so it is part of the identity"
    );
    assert_eq!(
        a.runtime_profile(),
        "profile-a",
        "the identity records the profile it was derived from"
    );
}

#[test]
fn records_from_different_runtime_profiles_cannot_be_reported_as_one_batch() {
    let other_profile = record(
        "case-other-profile",
        ModerationAction::Allow,
        ModerationCategory::None,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::None,
            ModerationAmbiguity::Low,
        )],
    );
    let mut foreign = other_profile.clone();
    foreign.policy = ModerationPolicyIdentity::from_policy(
        &ShadowPolicyIdentity {
            runtime_profile: "profile-b".to_owned(),
            ..shadow_identity()
        },
        "protocol-1",
    )
    .expect("identity");

    let records = vec![benign_agreement("case-a"), foreign];
    let error = summarize(&records).expect_err("a foreign runtime profile must be refused");

    let message = error.to_string();
    assert!(
        message.contains("runtime_profile") || message.contains("policy"),
        "the refusal names the identity that differs, got: {message}"
    );
}

#[test]
fn reviewer_disagreement_does_not_depend_on_submission_order() {
    // The reviewer pair the review used: A labels allow but tolerates ignore,
    // B labels ignore and tolerates only ignore. Only A's view is satisfied by
    // B, so the pair disagrees - but a left-to-right scan sees only A's view
    // and would call it agreement.
    let build = |order: [&str; 2]| {
        vec![
            review(
                order[0],
                ModerationAction::Allow,
                &[ModerationAction::Allow, ModerationAction::Ignore],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
            review(
                order[1],
                ModerationAction::Ignore,
                &[ModerationAction::Ignore],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
        ]
    };

    let forward = record(
        "case-order-a",
        ModerationAction::Allow,
        ModerationCategory::None,
        build(["rev-a", "rev-b"]),
    );
    let reversed = record(
        "case-order-a",
        ModerationAction::Allow,
        ModerationCategory::None,
        build(["rev-b", "rev-a"]),
    );

    let (_, forward_disagrees) = classify_moderation_case(&forward);
    let (_, reversed_disagrees) = classify_moderation_case(&reversed);
    assert!(
        forward_disagrees,
        "B never accepts A's label, so they disagree"
    );
    assert_eq!(
        forward_disagrees, reversed_disagrees,
        "submission order must not decide whether reviewers disagree"
    );

    let summarized = summarize(&[forward, reversed]).expect("summary");
    assert_eq!(
        summarized.reviewer_disagreement_cases, 2,
        "both permutations are counted as disagreement"
    );
}

#[test]
fn reviewers_who_disagree_on_category_disagree_even_when_they_agree_on_action() {
    let reviews = vec![
        review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::None,
            ModerationAmbiguity::Low,
        ),
        review(
            "rev-b",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Harassment,
            ModerationAmbiguity::Low,
        ),
    ];
    let disagreement = record(
        "case-category-split",
        ModerationAction::Allow,
        ModerationCategory::None,
        reviews,
    );

    let (_, disagrees) = classify_moderation_case(&disagreement);
    assert!(
        disagrees,
        "two reviewers who call the same action acceptable but cannot agree on \
         whether it is a violation have not settled what the label means"
    );
}

#[test]
fn an_agreement_is_never_retained_as_a_divergent_reference() {
    let records = vec![benign_agreement("case-agreed-01")];

    let report = summarize(&records).expect("summary");

    assert_eq!(report.agreed_cases, 1);
    assert_eq!(report.overridden_cases, 0);
    assert!(
        report.divergent_cases.is_empty(),
        "an agreement is not a divergence and must not be published as a reference, got {:?}",
        report.divergent_cases
    );
    assert!(
        !report.divergent_cases_truncated,
        "nothing diverged, so nothing was truncated"
    );
}

#[test]
fn agreements_cannot_displace_a_real_divergence_from_the_reference_list() {
    // `case_limit` bounds retained references. If agreements also occupied
    // slots, a batch of agreements ahead of the divergence would report
    // `truncated=false` while quietly dropping the only case worth review.
    let mut records = Vec::new();
    for index in 0..4 {
        records.push(benign_agreement(&format!("case-agreed-{index}")));
    }
    records.push(record(
        "case-real-divergence",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Spam,
            ModerationAmbiguity::Low,
        )],
    ));

    let report = summarize_moderation_divergence(&records, &config(), 2).expect("summary");

    assert_eq!(
        report.overridden_cases, 1,
        "the one override is still counted"
    );
    assert_eq!(
        report.divergent_cases.len(),
        1,
        "only the divergence takes a reference slot"
    );
    assert_eq!(report.divergent_cases[0].case_id, "case-real-divergence");
    assert!(
        !report.divergent_cases_truncated,
        "the single divergence was retained, so nothing was truncated"
    );
}

#[test]
fn a_disagreement_case_is_never_counted_as_agreed() {
    let override_only = record(
        "case-override-only",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![review(
            "rev-a",
            ModerationAction::Allow,
            &[ModerationAction::Allow],
            ModerationCategory::Spam,
            ModerationAmbiguity::Low,
        )],
    );
    let disagreement_only = record(
        "case-disagreement-only",
        ModerationAction::Ignore,
        ModerationCategory::None,
        vec![
            review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow, ModerationAction::Ignore],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
            review(
                "rev-b",
                ModerationAction::Ignore,
                &[ModerationAction::Ignore],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
        ],
    );
    // Overridden *and* split between reviewers: the worst case, and the one
    // that must appear in both counters rather than being folded into either.
    let override_and_disagreement = record(
        "case-override-and-disagreement",
        ModerationAction::Delete,
        ModerationCategory::Spam,
        vec![
            review(
                "rev-a",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::None,
                ModerationAmbiguity::Low,
            ),
            review(
                "rev-b",
                ModerationAction::Allow,
                &[ModerationAction::Allow],
                ModerationCategory::Harassment,
                ModerationAmbiguity::Low,
            ),
        ],
    );

    let report = summarize(&[
        override_only,
        disagreement_only,
        override_and_disagreement,
        benign_agreement("case-settled"),
    ])
    .expect("summary");

    assert_eq!(report.total_cases, 4);
    assert_eq!(
        report.agreed_cases, 1,
        "only the case the reviewers settled is agreed"
    );
    assert_eq!(report.overridden_cases, 2);
    assert_eq!(report.reviewer_disagreement_cases, 2);
    assert_eq!(
        report.divergent_cases.len(),
        2,
        "both overrides are retained as references, and neither agreement nor \
         disagreement takes a slot"
    );
    assert!(!report.divergent_cases_truncated);
}

/// Load a checked-in fixture and rebuild comparison records from its cases.
fn records_from_fixture(
    doc: &serde_json::Value,
    config: &ShadowOrchestratorConfig,
) -> Vec<ModerationComparisonRecord> {
    let orchestration = ModerationOrchestrationIdentity::from_config(config).expect("valid config");
    let policy = doc["policy"].clone();
    doc["cases"]
        .as_array()
        .expect("a fixture carries cases")
        .iter()
        .map(|case| {
            let mut case = case.clone();
            case["schema_version"] = serde_json::json!(MODERATION_COMPARISON_SCHEMA_VERSION);
            case["orchestration"] = serde_json::to_value(&orchestration).expect("serializable");
            case["policy"] = policy.clone();
            serde_json::from_value(case).expect("a valid fixture case deserializes")
        })
        .collect()
}

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/evaluation/moderation-evaluation")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {name}: {error}"));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parsing {name}: {error}"))
}

/// Counters the summarizer derives, paired with the fixture field that claims
/// them. Kept as a list so a new counter cannot be added to one side alone.
fn counters(report: &ModerationDivergenceReport) -> Vec<(&'static str, u64)> {
    vec![
        ("total_cases", report.total_cases),
        ("comparable_cases", report.comparable_cases),
        ("diverged_cases", report.diverged_cases),
        ("agreed_cases", report.agreed_cases),
        ("overridden_cases", report.overridden_cases),
        (
            "reviewer_disagreement_cases",
            report.reviewer_disagreement_cases,
        ),
        ("benign_suppressed_cases", report.benign_suppressed_cases),
        (
            "destructive_overridden_cases",
            report.destructive_overridden_cases,
        ),
    ]
}

#[test]
fn every_valid_fixture_states_exactly_what_the_summarizer_computes() {
    // `scripts/validate.mjs` recomputes these counters in JavaScript. That
    // mirror is only trustworthy if it agrees with the Rust that produces the
    // report, so the checked-in fixtures are held to both: the summarizer runs
    // over the same cases here, and the validator requires the stated numbers
    // to equal the recomputed ones. Drift between the two implementations
    // breaks one of them.
    for name in [
        "benign-criticism.json",
        "outlier-in-crowd.json",
        "quoted-abuse.json",
        "reviewer-disagreement.json",
    ] {
        let doc = fixture(name);
        let records = records_from_fixture(&doc, &config());
        let report = summarize(&records).expect("summary");

        for (field, computed) in counters(&report) {
            let stated = doc["aggregate"][field]
                .as_u64()
                .unwrap_or_else(|| panic!("{name}: aggregate.{field} is not a count"));
            assert_eq!(
                stated, computed,
                "{name}: aggregate.{field} disagrees with the summarizer"
            );
        }

        let refs = doc["aggregate"]["divergent_cases"]
            .as_array()
            .expect("the fixture lists retained references");
        assert_eq!(
            refs.len(),
            report.divergent_cases.len(),
            "{name}: the retained reference count disagrees with the summarizer"
        );
        for (reference, expected) in refs.iter().zip(&report.divergent_cases) {
            assert_eq!(reference["case_id"], serde_json::json!(expected.case_id));
            assert_eq!(
                reference["outcome"],
                serde_json::to_value(expected.outcome).expect("outcome serializes"),
            );
        }
    }
}

#[test]
fn the_under_reported_fixtures_disagree_with_the_summarizer() {
    // These two are rejected by the validator because they erase exactly the
    // harms #77 exists to expose. Assert the Rust side computes the other
    // number, so the corpus is not merely asserting a rule nobody implements.
    for (name, field, erased) in [
        (
            "inconsistent/under-reported-disagreement.json",
            "reviewer_disagreement_cases",
            2u64,
        ),
        (
            "inconsistent/under-reported-benign-suppression.json",
            "benign_suppressed_cases",
            2u64,
        ),
    ] {
        let doc = fixture(name);
        let records = records_from_fixture(&doc, &config());
        let report = summarize(&records).expect("summary");
        let computed = counters(&report)
            .into_iter()
            .find(|(key, _)| *key == field)
            .map(|(_, value)| value)
            .expect("the counter exists");
        let stated = doc["aggregate"][field]
            .as_u64()
            .unwrap_or_else(|| panic!("{name}: aggregate.{field} is not a count"));

        assert_eq!(
            computed, erased,
            "{name}: the cases imply {erased}, so the fixture is not under-reporting"
        );
        assert_ne!(
            stated, computed,
            "{name}: the fixture claims {stated} where the summarizer computes {computed}"
        );
    }
}
