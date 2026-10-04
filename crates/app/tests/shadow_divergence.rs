//! Issue #165: shadow divergence evidence integration.
//!
//! #163 and #164 produce `ShadowComparisonRecord`s but left them as per-event
//! evidence with no way for #58 benchmarking or #151 human review to consume
//! them. This test file pins the three properties that make such consumption
//! safe:
//!
//! 1. **Stable classification.** Every decision shape lands in exactly one
//!    stable category, and a failed shadow evaluation is reported as a failure
//!    to compare rather than folded into agreement.
//! 2. **Explicit compatibility.** Records from different policy/config/profile
//!    identities, or from a different comparison `schema_version`, are refused
//!    instead of merged into a number nobody can interpret.
//! 3. **Evidence, not authority.** A report carries counts and runtime
//!    references only — never payload or generated text — and it exposes
//!    nothing that could promote a shadow policy to active behaviour.

use aivtuber_app::{
    SHADOW_COMPARISON_SCHEMA_VERSION, SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION,
    SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowComparisonRecord, ShadowCompatibilityKey,
    ShadowDecision, ShadowDivergenceCategory, ShadowDivergenceReport, ShadowEvaluationFailure,
    ShadowEvaluationOutcome, ShadowFallbackReason, ShadowPolicyIdentity, ShadowRouteClass,
    ShadowTargetIdentity, classify_shadow_divergence, summarize_shadow_divergence,
    summarize_shadow_divergence_default,
};

fn identity(
    policy_id: &str,
    version: &str,
    fingerprint: &str,
    profile: &str,
) -> ShadowPolicyIdentity {
    ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: version.to_owned(),
        config_fingerprint: fingerprint.to_owned(),
        runtime_profile: profile.to_owned(),
        dataset_id: None,
    }
}

fn asset(asset_id: &str) -> ShadowTargetIdentity {
    ShadowTargetIdentity::Asset {
        asset_id: asset_id.to_owned(),
        asset_identity: None,
    }
}

fn template(id: &str, version: &str) -> ShadowTargetIdentity {
    ShadowTargetIdentity::Template {
        template_id: id.to_owned(),
        template_version: version.to_owned(),
    }
}

fn record(
    event_id: &str,
    active: ShadowDecision,
    shadow: ShadowEvaluationOutcome,
    route_diverged: Option<bool>,
    target_diverged: Option<bool>,
    fallback_diverged: Option<bool>,
) -> ShadowComparisonRecord {
    ShadowComparisonRecord {
        schema_version: SHADOW_COMPARISON_SCHEMA_VERSION.to_owned(),
        event_id: event_id.to_owned(),
        correlation_id: format!("corr-{event_id}"),
        active_policy: identity("intent-routing", "1.0.0", "cfg-1", "default"),
        shadow_policy: identity("semantic-reuse", "0.3.0", "cfg-1", "default"),
        active,
        shadow,
        route_diverged,
        target_diverged,
        fallback_diverged,
    }
}

fn evaluated(
    route: ShadowRouteClass,
    target: Option<ShadowTargetIdentity>,
) -> ShadowEvaluationOutcome {
    ShadowEvaluationOutcome::Evaluated {
        decision: ShadowDecision {
            route,
            target,
            fallback_reason: None,
        },
    }
}

fn agreeing(event_id: &str) -> ShadowComparisonRecord {
    record(
        event_id,
        ShadowDecision {
            route: ShadowRouteClass::SemanticReuse,
            target: Some(asset("a")),
            fallback_reason: None,
        },
        evaluated(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
        Some(false),
        Some(false),
        Some(false),
    )
}

fn route_transition(event_id: &str) -> ShadowComparisonRecord {
    record(
        event_id,
        ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(template("t1", "1")),
            fallback_reason: None,
        },
        evaluated(ShadowRouteClass::Generated, None),
        Some(true),
        Some(true),
        Some(false),
    )
}

#[test]
fn agreement_is_its_own_category() {
    let classification = classify_shadow_divergence(&agreeing("evt-1"));
    assert_eq!(
        classification.category,
        ShadowDivergenceCategory::SameRouteSameTarget
    );
    assert!(classification.comparable);
    assert!(!classification.category.is_divergence());
}

#[test]
fn same_route_different_target_is_a_real_disagreement() {
    let record = record(
        "evt-2",
        ShadowDecision {
            route: ShadowRouteClass::SemanticReuse,
            target: Some(asset("a")),
            fallback_reason: None,
        },
        evaluated(ShadowRouteClass::SemanticReuse, Some(asset("b"))),
        Some(false),
        Some(true),
        Some(false),
    );
    let classification = classify_shadow_divergence(&record);
    assert_eq!(
        classification.category,
        ShadowDivergenceCategory::SameRouteDifferentTarget
    );
    assert!(classification.category.is_divergence());
}

#[test]
fn response_versus_silent_is_its_own_category() {
    let active_silent = record(
        "evt-3",
        ShadowDecision {
            route: ShadowRouteClass::Silent,
            target: None,
            fallback_reason: None,
        },
        evaluated(ShadowRouteClass::Generated, None),
        Some(true),
        None,
        Some(false),
    );
    assert_eq!(
        classify_shadow_divergence(&active_silent).category,
        ShadowDivergenceCategory::ResponseVsSilent
    );

    let mirrored = record(
        "evt-4",
        ShadowDecision {
            route: ShadowRouteClass::Generated,
            target: None,
            fallback_reason: None,
        },
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision::silent(),
        },
        Some(true),
        None,
        Some(false),
    );
    assert_eq!(
        classify_shadow_divergence(&mirrored).category,
        ShadowDivergenceCategory::ResponseVsSilent
    );
}

#[test]
fn fallback_disagreement_wins_over_route_disagreement() {
    // The operationally interesting half is the degradation, so a fallback
    // difference is reported as `fallback_vs_success` even though the route
    // classes also differ.
    let record = record(
        "evt-5",
        ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(template("t1", "1")),
            fallback_reason: None,
        },
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision {
                route: ShadowRouteClass::Generated,
                target: None,
                fallback_reason: Some(ShadowFallbackReason::Timeout),
            },
        },
        Some(true),
        Some(true),
        Some(true),
    );
    assert_eq!(
        classify_shadow_divergence(&record).category,
        ShadowDivergenceCategory::FallbackVsSuccess
    );
}

#[test]
fn a_failed_shadow_evaluation_is_unusable_not_agreement() {
    let record = record(
        "evt-6",
        ShadowDecision {
            route: ShadowRouteClass::SemanticReuse,
            target: Some(asset("a")),
            fallback_reason: None,
        },
        ShadowEvaluationOutcome::Failed {
            reason: ShadowEvaluationFailure::DeadlineExceeded,
        },
        None,
        None,
        None,
    );
    let classification = classify_shadow_divergence(&record);
    assert_eq!(
        classification.category,
        ShadowDivergenceCategory::ShadowUnusable
    );
    assert!(!classification.comparable);
    assert!(
        !classification.category.is_divergence(),
        "an unusable evaluation is a failure to compare, never an agreement"
    );
}

#[test]
fn an_empty_batch_is_refused_rather_than_reported_as_no_divergence() {
    let error = summarize_shadow_divergence(&[], 8).expect_err("empty batch must be refused");
    assert!(
        error.to_string().contains("at least one comparison record"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_different_policy_identities_are_refused() {
    let mut second = agreeing("evt-2");
    second.shadow_policy = identity("semantic-reuse", "0.4.0", "cfg-1", "default");
    let error = summarize_shadow_divergence(&[agreeing("evt-1"), second], 8)
        .expect_err("a policy version bump must not be aggregated silently");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_a_different_config_fingerprint_are_refused() {
    let mut second = agreeing("evt-2");
    second.shadow_policy.config_fingerprint = "cfg-2".to_owned();
    let error = summarize_shadow_divergence(&[agreeing("evt-1"), second], 8)
        .expect_err("a different active config must not be aggregated");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_a_different_runtime_profile_are_refused() {
    let mut second = agreeing("evt-2");
    second.active_policy.runtime_profile = "low-memory".to_owned();
    let error = summarize_shadow_divergence(&[agreeing("evt-1"), second], 8)
        .expect_err("a different runtime profile must not be aggregated");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_a_different_comparison_schema_version_are_refused() {
    let mut second = agreeing("evt-2");
    second.schema_version = "0.0.9".to_owned();
    let error = summarize_shadow_divergence(&[agreeing("evt-1"), second], 8)
        .expect_err("an older comparison schema must not be combined");
    assert!(
        error.to_string().contains("different comparison versions"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_compatibility_key_is_the_one_the_records_carry() {
    let record = agreeing("evt-1");
    let key = ShadowCompatibilityKey::from_record(&record);
    assert_eq!(
        key.comparison_schema_version,
        SHADOW_COMPARISON_SCHEMA_VERSION
    );
    assert_eq!(key.active_policy_id, "intent-routing");
    assert_eq!(key.shadow_policy_version, "0.3.0");
}

#[test]
fn aggregation_counts_every_category_even_when_absent() {
    let report = summarize_shadow_divergence(&[agreeing("evt-1")], 8).expect("summary");
    for category in ShadowDivergenceCategory::ALL {
        assert!(
            report.category_counts.contains_key(category.as_str()),
            "missing category {}",
            category.as_str()
        );
    }
    assert_eq!(report.total_comparisons, 1);
    assert_eq!(report.comparable_comparisons, 1);
    assert_eq!(report.diverged_comparisons, 0);
    assert_eq!(report.category_counts["same_route_same_target"], 1);
}

#[test]
fn the_divergence_rate_excludes_unusable_evaluations_from_its_denominator() {
    let unusable = record(
        "evt-2",
        ShadowDecision {
            route: ShadowRouteClass::SemanticReuse,
            target: Some(asset("a")),
            fallback_reason: None,
        },
        ShadowEvaluationOutcome::Failed {
            reason: ShadowEvaluationFailure::Cancelled,
        },
        None,
        None,
        None,
    );
    let records = vec![route_transition("evt-1"), unusable];
    let report = summarize_shadow_divergence(&records, 8).expect("summary");

    assert_eq!(report.total_comparisons, 2);
    assert_eq!(report.comparable_comparisons, 1);
    assert_eq!(report.diverged_comparisons, 1);
    assert_eq!(report.category_counts["shadow_unusable"], 1);

    // 1 divergence over 1 comparable comparison is 100%. Dividing by all two
    // records would report 50% and let a broken shadow policy look better.
    assert_eq!(report.metrics["shadow.divergence.rate_pct"].value, 100.0);
    assert_eq!(report.metrics["shadow.divergence.rate_pct"].sample_count, 1);
}

#[test]
fn truncating_references_never_truncates_counts() {
    let records = vec![
        route_transition("evt-1"),
        route_transition("evt-2"),
        route_transition("evt-3"),
    ];
    let report = summarize_shadow_divergence(&records, 1).expect("summary");

    assert_eq!(report.diverged_comparisons, 3);
    assert_eq!(report.category_counts["route_transition"], 3);
    assert_eq!(report.divergent_cases.len(), 1);
    assert!(report.divergent_cases_truncated);
    assert_eq!(
        report.metrics["shadow.divergence.route_transition_count"].value,
        3.0
    );
}

#[test]
fn an_untruncated_report_says_so() {
    let report =
        summarize_shadow_divergence_default(&[route_transition("evt-1")]).expect("summary");
    assert!(!report.divergent_cases_truncated);
    assert_eq!(report.divergent_cases.len(), 1);
}

#[test]
fn metrics_follow_the_benchmark_flat_naming_convention() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let report = summarize_shadow_divergence(&records, 8).expect("summary");
    for (name, metric) in &report.metrics {
        assert!(
            name.starts_with("shadow.divergence."),
            "unexpected metric name {name}"
        );
        assert!(
            name.ends_with("_count") || name.ends_with("_pct"),
            "benchmark metric names carry a unit suffix, got {name}"
        );
        assert!(
            metric.sample_count > 0,
            "metric {name} lost its sample count"
        );
    }
    assert_eq!(
        report.metrics["shadow.divergence.route_transition_count"].sample_count,
        2
    );
}

#[test]
fn a_report_carries_the_versions_a_stored_sample_needs() {
    let report = summarize_shadow_divergence_default(&[agreeing("evt-1")]).expect("summary");
    assert_eq!(
        report.schema_version,
        SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION
    );
    assert_eq!(
        report.orchestrator_schema_version,
        SHADOW_ORCHESTRATOR_SCHEMA_VERSION
    );
}

#[test]
fn serialized_evidence_never_contains_payload_or_generated_text() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let report = summarize_shadow_divergence(&records, 8).expect("summary");
    let json = serde_json::to_string(&report).expect("report serializes");

    for forbidden in ["payload", "text", "prompt", "transcript", "authorization"] {
        assert!(
            !json.contains(forbidden),
            "evidence leaked {forbidden}: {json}"
        );
    }
    // The runtime handles a reviewer needs are still present.
    assert!(json.contains("evt-1"));
    assert!(json.contains("corr-evt-1"));
}

#[test]
fn a_report_cannot_promote_a_shadow_policy() {
    // The boundary is structural, not a runtime check: the report exposes no
    // field that names an active policy to switch to, and no score that could
    // be read as a recommendation.
    let records = vec![route_transition("evt-1"), route_transition("evt-2")];
    let report: ShadowDivergenceReport = summarize_shadow_divergence(&records, 8).expect("summary");
    let value = serde_json::to_value(&report).expect("report serializes");
    let object = value.as_object().expect("report is an object");

    for forbidden in [
        "promote",
        "promotion",
        "recommended_policy",
        "new_active_policy",
    ] {
        assert!(
            !object.contains_key(forbidden),
            "report exposes {forbidden}, which could imply promotion"
        );
    }
    // The only policy identities present are the ones already compared.
    let compatibility = object["compatibility"].as_object().expect("compatibility");
    assert_eq!(compatibility["active_policy_id"], "intent-routing");
    assert_eq!(compatibility["shadow_policy_id"], "semantic-reuse");
}

#[test]
fn a_report_round_trips_through_json() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let report = summarize_shadow_divergence(&records, 8).expect("summary");
    let encoded = serde_json::to_string(&report).expect("serializes");
    let decoded: ShadowDivergenceReport = serde_json::from_str(&encoded).expect("deserializes");
    assert_eq!(decoded, report);
}
