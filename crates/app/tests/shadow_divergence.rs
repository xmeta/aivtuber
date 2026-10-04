//! Issue #165: shadow divergence evidence integration.
//!
//! #163 and #164 produce `ShadowComparisonRecord`s but left them as per-event
//! evidence with no way for #58 benchmarking or #151 human review to consume
//! them. This test file pins the properties that make such consumption safe:
//!
//! 1. **Stable classification.** Every decision shape lands in exactly one
//!    stable category, and an absent comparison — a failed shadow evaluation or
//!    a target pair the runtime declines to compare — is reported as *not
//!    comparable* rather than folded into agreement.
//! 2. **Explicit compatibility.** Records from different policy/config/profile
//!    identities, dataset ids, comparison `schema_version`s or orchestration
//!    bounds are refused instead of merged.
//! 3. **Evidence, not authority.** A report carries counts and runtime
//!    references only — never payload or generated text — and exposes nothing
//!    that could promote a shadow policy to active behaviour.

use aivtuber_app::{
    SHADOW_COMPARISON_SCHEMA_VERSION, SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION,
    SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowComparisonRecord, ShadowCompatibilityKey,
    ShadowDecision, ShadowDivergenceCategory, ShadowDivergenceReport, ShadowEvaluationFailure,
    ShadowEvaluationOutcome, ShadowFallbackReason, ShadowOperationalEvidence,
    ShadowOrchestrationIdentity, ShadowOrchestratorConfig, ShadowPolicyIdentity, ShadowRouteClass,
    ShadowTargetIdentity, SnapshotCounters, classify_shadow_divergence,
    summarize_shadow_divergence, summarize_shadow_divergence_default,
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

fn active(route: ShadowRouteClass, target: Option<ShadowTargetIdentity>) -> ShadowDecision {
    ShadowDecision {
        route,
        target,
        fallback_reason: None,
    }
}

/// The config a test batch claims to have run under.
fn config() -> ShadowOrchestratorConfig {
    ShadowOrchestratorConfig::default()
}

fn agreeing(event_id: &str) -> ShadowComparisonRecord {
    record(
        event_id,
        active(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
        evaluated(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
        Some(false),
        Some(false),
        Some(false),
    )
}

fn route_transition(event_id: &str) -> ShadowComparisonRecord {
    record(
        event_id,
        active(ShadowRouteClass::Template, Some(template("t1", "1"))),
        evaluated(ShadowRouteClass::Generated, None),
        Some(true),
        Some(true),
        Some(false),
    )
}

fn summarize(
    records: &[ShadowComparisonRecord],
) -> Result<ShadowDivergenceReport, aivtuber_app::AppError> {
    summarize_shadow_divergence_default(records, &config())
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
        active(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
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
        active(ShadowRouteClass::Silent, None),
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
        active(ShadowRouteClass::Generated, None),
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

/// P1: `fallback_diverged` is built from `!=` on the reasons, so it is also true
/// when *both* sides failed for different reasons. Recording that as
/// "one side degraded, the other succeeded" would misstate the report.
#[test]
fn both_sides_failing_differently_is_not_fallback_versus_success() {
    let both_failed = record(
        "evt-5",
        ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(template("t1", "1")),
            fallback_reason: Some(ShadowFallbackReason::Timeout),
        },
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision {
                route: ShadowRouteClass::Template,
                target: Some(template("t1", "1")),
                fallback_reason: Some(ShadowFallbackReason::Unavailable),
            },
        },
        Some(false),
        Some(false),
        Some(true),
    );
    let classification = classify_shadow_divergence(&both_failed);
    assert_eq!(
        classification.category,
        ShadowDivergenceCategory::FallbackReasonMismatch,
        "both sides degraded, so this is not one-side-degraded-vs-success"
    );
    assert_ne!(
        classification.category,
        ShadowDivergenceCategory::FallbackVsSuccess
    );
    assert!(classification.category.is_divergence());
}

#[test]
fn exactly_one_side_falling_back_is_fallback_versus_success() {
    let active_degraded = record(
        "evt-6",
        ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(template("t1", "1")),
            fallback_reason: Some(ShadowFallbackReason::Timeout),
        },
        evaluated(ShadowRouteClass::Template, Some(template("t1", "1"))),
        Some(false),
        Some(false),
        Some(true),
    );
    assert_eq!(
        classify_shadow_divergence(&active_degraded).category,
        ShadowDivergenceCategory::FallbackVsSuccess
    );

    let shadow_degraded = record(
        "evt-7",
        active(ShadowRouteClass::Template, Some(template("t1", "1"))),
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision {
                route: ShadowRouteClass::Template,
                target: Some(template("t1", "1")),
                fallback_reason: Some(ShadowFallbackReason::Timeout),
            },
        },
        Some(false),
        Some(false),
        Some(true),
    );
    assert_eq!(
        classify_shadow_divergence(&shadow_degraded).category,
        ShadowDivergenceCategory::FallbackVsSuccess
    );
}

#[test]
fn the_same_fallback_reason_on_both_sides_is_agreement() {
    let both_timed_out = record(
        "evt-8",
        ShadowDecision {
            route: ShadowRouteClass::Template,
            target: Some(template("t1", "1")),
            fallback_reason: Some(ShadowFallbackReason::Timeout),
        },
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision {
                route: ShadowRouteClass::Template,
                target: Some(template("t1", "1")),
                fallback_reason: Some(ShadowFallbackReason::Timeout),
            },
        },
        Some(false),
        Some(false),
        Some(false),
    );
    assert_eq!(
        classify_shadow_divergence(&both_timed_out).category,
        ShadowDivergenceCategory::SameRouteSameTarget
    );
}

/// P1: `compare_target` returns `None` for pairs whose content the runtime does
/// not retain (`Generated` vs `Generated`). Treating that as agreement counted
/// unevidenced agreement and lowered the divergence rate.
#[test]
fn an_incomparable_target_pair_is_not_agreement() {
    let two_generated = record(
        "evt-9",
        active(ShadowRouteClass::Generated, None),
        evaluated(ShadowRouteClass::Generated, None),
        Some(false),
        None,
        Some(false),
    );
    let classification = classify_shadow_divergence(&two_generated);
    assert_eq!(
        classification.category,
        ShadowDivergenceCategory::TargetIncomparable
    );
    assert!(
        !classification.comparable,
        "an unevidenced comparison must stay out of the rate denominator"
    );
    assert!(!classification.category.is_divergence());
}

#[test]
fn an_incomparable_target_pair_is_excluded_from_the_rate_denominator() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2"), {
        record(
            "evt-3",
            active(ShadowRouteClass::Generated, None),
            evaluated(ShadowRouteClass::Generated, None),
            Some(false),
            None,
            Some(false),
        )
    }];
    let report = summarize(&records).expect("summary");

    assert_eq!(report.total_comparisons, 3);
    assert_eq!(
        report.comparable_comparisons, 2,
        "the incomparable pair must not enter the denominator"
    );
    assert_eq!(report.diverged_comparisons, 1);
    assert_eq!(report.category_counts["target_incomparable"], 1);
    // 1 divergence over 2 comparable comparisons. Counting the incomparable pair
    // as agreement would report 100/3 ≈ 33%.
    assert_eq!(report.metrics["shadow.divergence.rate_pct"].value, 50.0);
    assert_eq!(report.metrics["shadow.divergence.rate_pct"].sample_count, 2);
}

#[test]
fn a_failed_shadow_evaluation_is_unusable_not_agreement() {
    let record = record(
        "evt-10",
        active(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
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
    let error = summarize(&[]).expect_err("empty batch must be refused");
    assert!(
        error.to_string().contains("at least one comparison record"),
        "unexpected error: {error}"
    );
}

#[test]
fn records_from_different_policy_identities_are_refused() {
    let mut second = agreeing("evt-2");
    second.shadow_policy = identity("semantic-reuse", "0.4.0", "cfg-1", "default");
    let error = summarize(&[agreeing("evt-1"), second])
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
    let error = summarize(&[agreeing("evt-1"), second])
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
    let error = summarize(&[agreeing("evt-1"), second])
        .expect_err("a different runtime profile must not be aggregated");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

/// P1: `ShadowPolicyIdentity` defines a `dataset_id` on *each* side. Reading
/// only the shadow one let two batches with different active datasets aggregate
/// into the same key.
#[test]
fn a_different_active_dataset_id_is_refused() {
    let mut first = agreeing("evt-1");
    first.active_policy.dataset_id = Some("dataset-A".to_owned());
    first.shadow_policy.dataset_id = Some("dataset-shared".to_owned());

    let mut second = agreeing("evt-2");
    second.active_policy.dataset_id = Some("dataset-B".to_owned());
    second.shadow_policy.dataset_id = Some("dataset-shared".to_owned());

    let error =
        summarize(&[first, second]).expect_err("a different active dataset must not be aggregated");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_different_shadow_dataset_id_is_refused() {
    let mut second = agreeing("evt-2");
    second.shadow_policy.dataset_id = Some("dataset-B".to_owned());
    let first = agreeing("evt-1");
    let error =
        summarize(&[first, second]).expect_err("a different shadow dataset must not be aggregated");
    assert!(
        error.to_string().contains("must not be aggregated"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_compatibility_key_holds_both_dataset_ids() {
    let mut record = agreeing("evt-1");
    record.active_policy.dataset_id = Some("dataset-A".to_owned());
    record.shadow_policy.dataset_id = Some("dataset-B".to_owned());

    let key = ShadowCompatibilityKey::from_record(&record);
    assert_eq!(key.active_dataset_id.as_deref(), Some("dataset-A"));
    assert_eq!(key.shadow_dataset_id.as_deref(), Some("dataset-B"));
    assert_eq!(
        key.comparison_schema_version,
        SHADOW_COMPARISON_SCHEMA_VERSION
    );
    assert_eq!(key.active_policy_id, "intent-routing");
    assert_eq!(key.shadow_policy_version, "0.3.0");
}

#[test]
fn records_from_a_different_comparison_schema_version_are_refused() {
    let mut second = agreeing("evt-2");
    second.schema_version = "0.0.9".to_owned();
    let error = summarize(&[agreeing("evt-1"), second])
        .expect_err("an older comparison schema must not be combined");
    assert!(
        error.to_string().contains("different comparison versions"),
        "unexpected error: {error}"
    );
}

/// P1: the report must not back-fill the *current* orchestrator version. Two
/// batches that differ only in deadline / sample rate / concurrency are not
/// comparable even under an identical policy identity.
#[test]
fn the_report_copies_the_orchestration_identity_it_was_given() {
    let config = ShadowOrchestratorConfig {
        deadline_ms: 400,
        ..ShadowOrchestratorConfig::default()
    };

    let report =
        summarize_shadow_divergence_default(&[agreeing("evt-1")], &config).expect("summary");

    assert_eq!(
        report.orchestration,
        ShadowOrchestrationIdentity::from_config(&config).expect("identity"),
        "the report must echo the identity derived from the supplied config"
    );
    assert_eq!(
        report.orchestration.orchestrator_schema_version(),
        SHADOW_ORCHESTRATOR_SCHEMA_VERSION
    );
}

#[test]
fn different_orchestration_bounds_produce_different_identities() {
    let base = ShadowOrchestratorConfig::default();
    let fingerprint = |config: &ShadowOrchestratorConfig| {
        ShadowOrchestrationIdentity::from_config(config)
            .expect("valid config")
            .bounds_fingerprint()
            .to_owned()
    };

    let slower = ShadowOrchestratorConfig {
        deadline_ms: base.deadline_ms + 1,
        ..base.clone()
    };
    assert_ne!(
        fingerprint(&base),
        fingerprint(&slower),
        "a different deadline changes which evidence a run produces"
    );

    let busier = ShadowOrchestratorConfig {
        max_concurrent: base.max_concurrent + 1,
        ..base.clone()
    };
    assert_ne!(fingerprint(&base), fingerprint(&busier));

    let billing = ShadowOrchestratorConfig {
        allow_provider_calls: true,
        ..base.clone()
    };
    assert_ne!(
        fingerprint(&base),
        fingerprint(&billing),
        "enabling billable provider calls must change the bounds fingerprint"
    );

    // Identical configs must agree, or stored evidence becomes unreproducible.
    assert_eq!(fingerprint(&base), fingerprint(&base.clone()));
}

#[test]
fn a_config_from_another_schema_version_is_refused() {
    let config = ShadowOrchestratorConfig {
        schema_version: "0.0.9".to_owned(),
        ..ShadowOrchestratorConfig::default()
    };
    let error = summarize_shadow_divergence_default(&[agreeing("evt-1")], &config)
        .expect_err("a foreign orchestration version must be refused");
    assert!(
        error.to_string().contains("schema_version"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_invalid_config_is_refused_before_any_report_is_built() {
    let config = ShadowOrchestratorConfig {
        deadline_ms: 0,
        ..ShadowOrchestratorConfig::default()
    };
    let error = summarize_shadow_divergence_default(&[agreeing("evt-1")], &config)
        .expect_err("a config the runtime would reject must be refused");
    assert!(
        error.to_string().contains("deadline_ms"),
        "unexpected error: {error}"
    );
}

/// P2: the scope asks for provider-call counts "when available".
#[test]
fn provider_call_counters_are_derived_from_a_snapshot_delta() {
    let before = SnapshotCounters {
        provider_calls: 4,
        provider_calls_blocked: 1,
        completed: 3,
        deadline_exceeded: 2,
    };
    let after = SnapshotCounters {
        provider_calls: 7,
        provider_calls_blocked: 1,
        completed: 9,
        deadline_exceeded: 2,
    };

    let delta = ShadowOperationalEvidence::from_snapshot_delta(&before, &after);
    assert_eq!(delta.provider_calls, 3);
    assert_eq!(delta.provider_calls_blocked, 0);
    assert_eq!(delta.evaluations_completed, 6);
    assert_eq!(delta.evaluations_deadline_exceeded, 0);
}

#[test]
fn a_snapshot_delta_saturates_instead_of_wrapping() {
    let later = SnapshotCounters {
        provider_calls: 9,
        provider_calls_blocked: 5,
        completed: 8,
        deadline_exceeded: 1,
    };
    let earlier = SnapshotCounters {
        provider_calls: 4,
        provider_calls_blocked: 1,
        completed: 3,
        deadline_exceeded: 0,
    };

    let delta = ShadowOperationalEvidence::from_snapshot_delta(&later, &earlier);
    assert_eq!(
        delta.provider_calls, 0,
        "out-of-order snapshots must not wrap"
    );
    assert_eq!(delta.evaluations_completed, 0);
}

#[test]
fn aggregation_counts_every_category_even_when_absent() {
    let report = summarize(&[agreeing("evt-1")]).expect("summary");
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
        active(ShadowRouteClass::SemanticReuse, Some(asset("a"))),
        ShadowEvaluationOutcome::Failed {
            reason: ShadowEvaluationFailure::Cancelled,
        },
        None,
        None,
        None,
    );
    let records = vec![route_transition("evt-1"), unusable];
    let report = summarize(&records).expect("summary");

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
    let report = summarize_shadow_divergence(&records, &config(), None, 1).expect("summary");

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
    let report = summarize(&[route_transition("evt-1")]).expect("summary");
    assert!(!report.divergent_cases_truncated);
    assert_eq!(report.divergent_cases.len(), 1);
}

#[test]
fn metrics_follow_the_benchmark_flat_naming_convention() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let report = summarize(&records).expect("summary");
    for (name, metric) in &report.metrics {
        assert!(
            name.starts_with("shadow.divergence.") || name.starts_with("shadow.operations."),
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
    let report = summarize(&[agreeing("evt-1")]).expect("summary");
    assert_eq!(
        report.schema_version,
        SHADOW_DIVERGENCE_REPORT_SCHEMA_VERSION
    );
    assert_eq!(
        report.orchestration.orchestrator_schema_version(),
        SHADOW_ORCHESTRATOR_SCHEMA_VERSION
    );
}

#[test]
fn serialized_evidence_never_contains_payload_or_generated_text() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let report = summarize(&records).expect("summary");
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
    let report: ShadowDivergenceReport = summarize(&records).expect("summary");
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
    let report = summarize(&records).expect("summary");
    let encoded = serde_json::to_string(&report).expect("serializes");
    let decoded: ShadowDivergenceReport = serde_json::from_str(&encoded).expect("deserializes");
    assert_eq!(decoded, report);
}

/// P2: the scope asks for provider-call counts "when available". The type
/// existing without being reachable from the report API is not an export.
#[test]
fn operational_evidence_reaches_the_report_metrics() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let operational = ShadowOperationalEvidence {
        provider_calls: 5,
        provider_calls_blocked: 2,
        evaluations_completed: 7,
        evaluations_deadline_exceeded: 1,
    };

    let report =
        summarize_shadow_divergence(&records, &config(), Some(operational), 8).expect("summary");

    assert_eq!(
        report.metrics["shadow.operations.provider_calls_count"].value,
        5.0
    );
    assert_eq!(
        report.metrics["shadow.operations.provider_calls_blocked_count"].value,
        2.0
    );
    assert_eq!(
        report.metrics["shadow.operations.evaluations_completed_count"].value,
        7.0
    );
    assert_eq!(
        report.metrics["shadow.operations.evaluations_deadline_exceeded_count"].value,
        1.0
    );
    assert_eq!(
        report.metrics["shadow.operations.provider_calls_count"].sample_count, 2,
        "operational counters are measured over the same batch"
    );
}

/// Operational cost must not be confused with disagreement.
#[test]
fn operational_evidence_does_not_distort_the_divergence_rate() {
    let records = vec![route_transition("evt-1"), agreeing("evt-2")];
    let with = summarize_shadow_divergence(
        &records,
        &config(),
        Some(ShadowOperationalEvidence {
            provider_calls: 99,
            ..ShadowOperationalEvidence::default()
        }),
        8,
    )
    .expect("summary");
    let without = summarize(&records).expect("summary");

    assert_eq!(
        with.metrics["shadow.divergence.rate_pct"].value,
        without.metrics["shadow.divergence.rate_pct"].value
    );
    assert_eq!(
        with.diverged_comparisons, without.diverged_comparisons,
        "provider calls describe cost, not disagreement"
    );
}

#[test]
fn no_operational_evidence_means_no_operational_metrics() {
    let report = summarize(&[agreeing("evt-1")]).expect("summary");
    assert!(
        !report
            .metrics
            .keys()
            .any(|name| name.starts_with("shadow.operations.")),
        "callers must opt in to operational counters"
    );
}

/// The aggregation the report publishes must be internally consistent: the
/// diverging categories sum to `diverged_comparisons`, which is what #58 reads.
#[test]
fn diverging_category_counts_sum_to_the_diverged_total() {
    let records = vec![
        route_transition("evt-1"),
        route_transition("evt-2"),
        route_transition("evt-3"),
        agreeing("evt-4"),
    ];
    let report = summarize(&records).expect("summary");

    let diverging_sum: u64 = ShadowDivergenceCategory::ALL
        .iter()
        .filter(|category| category.is_divergence())
        .map(|category| report.category_counts[category.as_str()])
        .sum();
    assert_eq!(diverging_sum, report.diverged_comparisons);
    assert_eq!(report.diverged_comparisons, 3);
}
