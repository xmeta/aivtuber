//! Operational SLO evaluation contract (issue #71).
//!
//! These tests pin the three properties the module claims rather than its
//! arithmetic: absence of evidence is never a pass, a zero-tolerance invariant
//! never acquires an error budget, and an experimental plane never moves the
//! active user-facing numbers.

use aivtuber_domain::InteractionDeadlineClass;
use aivtuber_telemetry::{
    AttributionSummary, BaselineEvidence, BaselineProposalSet, BaselineRun, BenchmarkReport,
    BenchmarkResult, CacheLevel, ComparisonMode, EventObservation, EvidenceSource, FailureOrigin,
    MIN_BASELINE_RUNS, MissAttribution, MissAttribution as Attribution, ObjectiveKind,
    ReproducibilityMetadata, RouteClass, STREAM_HOUR_MS, SloAction, SloError, SloEvaluationConfig,
    SloIndicatorResult, SloProvenance, SloReport, SloStatus, SloTarget, SloTargets, SloVerdict,
    SloWindow, TrafficPlane, WindowKind, attribution_of, catalog, evaluate,
};

const CALIBRATION_SOURCE: &str = "04-full-generative.json (dataset=slo-fixture, commit=deadbeef)";

/// The #58 compatibility series the fixture runs belong to.
const CALIBRATION_SERIES: &str = "deterministic_semantic|slo-fixture|bench-v1|7";

/// Real-looking calibration evidence for a target that only needs *a* valid
/// baseline to exercise some other rule.
///
/// It deliberately carries the compatibility series and two named runs with
/// revisions: a target has to be able to prove which repeated runs formed its
/// baseline, so a helper that left the manifest empty would exercise exactly
/// the bypass `SloTargets::validate` is supposed to close.
fn calibration_manifest() -> Vec<BaselineRun> {
    vec![
        BaselineRun {
            run_id: "run-a".to_owned(),
            git_commit: "deadbeef".to_owned(),
        },
        BaselineRun {
            run_id: "run-b".to_owned(),
            git_commit: "deadbeef".to_owned(),
        },
    ]
}

fn metadata(dataset: &str, stream_duration_ms: u64) -> ReproducibilityMetadata {
    ReproducibilityMetadata {
        dataset_id: dataset.to_owned(),
        git_commit: "deadbeef".to_owned(),
        rust_toolchain: "rustc 1.98.0".to_owned(),
        bun_toolchain: None,
        config_version: "bench-v1".to_owned(),
        asset_version: "starter-v1".to_owned(),
        index_version: None,
        jev_model: None,
        thinking_model: None,
        tts_model: None,
        seed: 7,
        stream_duration_ms: Some(stream_duration_ms),
    }
}

fn observation(route: RouteClass) -> EventObservation {
    let mut event = EventObservation::new("evt", ComparisonMode::DeterministicSemantic, route);
    event.routing_latency_us = 10;
    event.cache_lookup = true;
    event.cache_hit = true;
    event.cache_level = Some(CacheLevel::Memory);
    event
}

fn report(events: Vec<EventObservation>, stream_duration_ms: u64) -> BenchmarkReport {
    BenchmarkReport::from_events(
        metadata("slo-fixture", stream_duration_ms),
        ComparisonMode::DeterministicSemantic,
        events,
    )
    .expect("benchmark report")
}

fn result<'a>(report: &'a SloReport, id: &str, window: &str) -> &'a SloIndicatorResult {
    report
        .indicators
        .iter()
        .find(|entry| entry.id == id && entry.window == window)
        .unwrap_or_else(|| panic!("missing indicator {id} in window {window}"))
}

fn targets(entries: &[(&str, SloTarget)]) -> SloTargets {
    SloTargets {
        targets: entries
            .iter()
            .map(|(id, target)| ((*id).to_owned(), target.clone()))
            .collect(),
        ..SloTargets::default()
    }
}

fn calibrated(target: f64, baseline: f64) -> SloTarget {
    SloTarget {
        target,
        threshold_ms: None,
        baseline: BaselineEvidence {
            value: baseline,
            source: CALIBRATION_SOURCE.to_owned(),
            stream_hours: Some(1),
            series: Some(CALIBRATION_SERIES.to_owned()),
            runs: calibration_manifest(),
            // At every indicator's sample floor, so the helper represents
            // evidence a target is allowed to cite; the floor itself is
            // pinned by its own regression below.
            eligible: Some(15),
        },
    }
}

fn latency_target(target: f64, baseline: f64, threshold_ms: u64) -> SloTarget {
    SloTarget {
        threshold_ms: Some(threshold_ms),
        ..calibrated(target, baseline)
    }
}

fn evaluate_default(report: &BenchmarkReport) -> SloReport {
    evaluate(
        report,
        &SloTargets::default(),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report")
}

/// Evaluate one report as an identified run. Run identity is what makes two
/// reports two runs, so every calibratable run in these tests carries one.
fn evaluate_run(report: &BenchmarkReport, run_id: &str) -> SloReport {
    evaluate(
        report,
        &SloTargets::default(),
        SloEvaluationConfig {
            run_id: Some(run_id.to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report")
}

#[test]
fn an_empty_denominator_reports_no_data_and_never_a_pass() {
    let evaluated = evaluate_default(&report(vec![observation(RouteClass::Silent)], 60_000));

    let reuse = result(&evaluated, "quality.semantic_reuse_correctness", "session");
    assert_eq!(reuse.status, SloStatus::NoData);
    assert_eq!(reuse.value, None);
    assert_eq!(reuse.eligible, 0);

    // No external failure occurred, so the fallback objective has nothing to
    // say. It must not be reported as a satisfied fallback objective.
    let fallback = result(&evaluated, "reliability.fallback_delivery_rate", "session");
    assert_eq!(fallback.status, SloStatus::NoData);
    // Non-passing: uncalibrated objectives and unmeasured invariants are both
    // outstanding, so the aggregate says so instead of reporting a clean run.
    assert_eq!(
        evaluated.verdict,
        aivtuber_telemetry::SloVerdict::Incomplete
    );
}

#[test]
fn an_uncalibrated_latency_indicator_reports_evidence_but_no_value() {
    let events: Vec<EventObservation> = (1..=40)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.event_to_first_visible_reaction_ms = Some(index);
            event.event_to_first_audio_ms = Some(index + 1);
            event
        })
        .collect();
    let evaluated = evaluate_default(&report(events, 40_000));

    let visible = result(
        &evaluated,
        "availability.event_to_first_visible_within_target",
        "session",
    );
    assert_eq!(visible.status, SloStatus::Uncalibrated);
    assert_eq!(visible.value, None);
    assert_eq!(visible.eligible, 40);
    assert_eq!(
        visible.unscored, 40,
        "with no calibrated threshold every sample is calibration evidence, not a miss"
    );

    // The target-free percentiles are the evidence a threshold is chosen from,
    // so a reviewer never has to invent the number.
    let calibration = evaluated
        .latency_calibration
        .iter()
        .find(|point| point.window == "session" && point.field == "event_to_first_audio_ms")
        .expect("audio calibration point");
    assert_eq!(calibration.samples, 40);
    assert_eq!(calibration.p50_ms, 21);
    assert_eq!(calibration.max_ms, Some(41));
    assert!(
        evaluated
            .uncalibrated
            .contains(&"availability.event_to_first_audio_within_target".to_owned())
    );
}

#[test]
fn a_latency_target_without_a_threshold_is_refused() {
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        calibrated(0.99, 0.99),
    )]);
    let error = file
        .validate(&catalog())
        .expect_err("threshold is required");
    assert!(error.to_string().contains("threshold_ms"), "{error}");
}

#[test]
fn a_target_that_cites_no_measured_artifact_is_refused() {
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        SloTarget {
            threshold_ms: Some(250),
            ..calibrated(0.99, 0.99)
        },
    )]);
    let mut file = file;
    file.targets
        .get_mut("availability.event_to_first_audio_within_target")
        .expect("target")
        .baseline
        .source = "   ".to_owned();
    let error = file.validate(&catalog()).expect_err("baseline is required");
    assert!(error.to_string().contains("calibrated from"), "{error}");
}

#[test]
fn a_zero_tolerance_invariant_cannot_be_given_an_error_budget() {
    let file = targets(&[("safety.stale_dispatch_count", calibrated(0.99, 0.0))]);
    let error = file
        .validate(&catalog())
        .expect_err("invariants are not SLOs");
    assert!(error.to_string().contains("zero-tolerance"), "{error}");
}

#[test]
fn a_target_file_for_another_catalog_version_is_refused() {
    let file = SloTargets {
        catalog_version: "slo-catalog-v0".to_owned(),
        ..SloTargets::default()
    };
    let error = file.validate(&catalog()).expect_err("stale catalog");
    assert!(error.to_string().contains("not comparable"), "{error}");
}

#[test]
fn a_target_naming_an_unknown_indicator_is_refused() {
    let file = targets(&[("availability.made_up", calibrated(0.99, 0.99))]);
    let error = file.validate(&catalog()).expect_err("unknown indicator");
    assert!(
        error.to_string().contains("does not name an indicator"),
        "{error}"
    );
}

#[test]
fn a_miss_spends_the_error_budget_and_names_an_action() {
    let events: Vec<EventObservation> = (1..=20)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.event_to_first_audio_ms = Some(index * 10);
            event
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 20_000),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            latency_target(0.95, 0.0, 25),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.status, SloStatus::Missed);
    assert!(audio.value.is_some_and(|value| value < 0.95));
    assert_eq!(evaluated.verdict, aivtuber_telemetry::SloVerdict::Breach);

    let budget = evaluated
        .error_budgets
        .iter()
        .find(|budget| budget.indicator == audio.id)
        .expect("error budget");
    // 5% of 20 eligible events, spent 12 times over.
    assert_eq!(budget.allowed_misses, 1);
    assert!(budget.observed_misses > 1);
    assert!(budget.exhausted);
    assert!(budget.remaining_misses < 0);
    assert!(audio.actions.contains(&SloAction::RaiseIncident));
}

#[test]
fn a_met_objective_leaves_error_budget_remaining() {
    let events: Vec<EventObservation> = (1..=20)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.event_to_first_audio_ms = Some(index);
            event
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 20_000),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            latency_target(0.95, 0.99, 100),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");

    let budget = &evaluated.error_budgets[0];
    assert_eq!(budget.observed_misses, 0);
    assert_eq!(budget.allowed_misses, 1);
    assert!(!budget.exhausted);
    // The objective itself is met, but the report is not `Ok`: the
    // zero-tolerance invariants are still not measurable from this artifact.
    assert_eq!(
        evaluated.verdict,
        aivtuber_telemetry::SloVerdict::Incomplete
    );
}

#[test]
fn an_external_provider_failure_is_attributed_away_from_the_runtime_when_a_fallback_played() {
    let mut handled = observation(RouteClass::CachedFallback);
    handled.fallback_reason = Some("unavailable".to_owned());
    handled.event_to_first_audio_ms = Some(50);
    let mut unhandled = observation(RouteClass::Silent);
    unhandled.fallback_reason = Some("timeout".to_owned());

    assert_eq!(attribution_of(&handled), Attribution::ExternalProvider);
    assert_eq!(attribution_of(&unhandled), Attribution::RuntimeHandling);

    let summary = AttributionSummary::of([&handled, &unhandled]);
    assert_eq!(summary.external_provider_failures, 2);
    assert_eq!(summary.runtime_handled, 1);
    assert_eq!(summary.runtime_handling_failures, 1);

    // A fallback that delivered keeps the objective satisfied even though the
    // provider was down: degradation quality is what the runtime owns.
    let degraded_only = evaluate(
        &report(vec![handled.clone()], 1_000),
        &targets(&[("reliability.fallback_delivery_rate", calibrated(1.0, 1.0))]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    assert_eq!(
        result(
            &degraded_only,
            "reliability.fallback_delivery_rate",
            "session"
        )
        .status,
        SloStatus::Met
    );

    // One provider failure the runtime did not survive *is* the runtime's miss,
    // and the attribution says which of the two events caused it.
    let mixed = evaluate(
        &report(vec![handled, unhandled], 2_000),
        &targets(&[("reliability.fallback_delivery_rate", calibrated(1.0, 0.5))]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    let fallback = result(&mixed, "reliability.fallback_delivery_rate", "session");
    assert_eq!(fallback.status, SloStatus::Missed);
    assert_eq!(fallback.miss_attribution.runtime_handling_failures, 1);
    assert_eq!(fallback.miss_attribution.runtime_handled, 0);
    assert_eq!(mixed.failure_attribution.runtime_handled, 1);
    assert_eq!(mixed.verdict, aivtuber_telemetry::SloVerdict::Breach);
}

#[test]
fn a_target_calibrated_for_another_series_is_not_applied_to_this_report() {
    // The fixture report belongs to `deterministic_semantic|slo-fixture|bench-v1|7`.
    // A target calibrated on a different compatibility series must not decide
    // this report: cross-series reuse would silently apply — or silently
    // loosen — an objective calibrated for a different workload, exactly
    // what `baseline.series` claims to scope.
    let mut foreign = calibrated(1.0, 1.0);
    foreign.baseline.series = Some("deterministic_semantic|other-dataset|bench-v1|7".to_owned());

    let mut handled = observation(RouteClass::CachedFallback);
    handled.fallback_reason = Some("unavailable".to_owned());
    handled.event_to_first_audio_ms = Some(50);

    let cross = evaluate(
        &report(vec![handled.clone()], 1_000),
        &targets(&[("reliability.fallback_delivery_rate", foreign)]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    let row = result(&cross, "reliability.fallback_delivery_rate", "session");
    assert_eq!(
        row.target, None,
        "a target calibrated for another series must not be applied"
    );
    assert_eq!(
        row.status,
        SloStatus::Uncalibrated,
        "cross-series application must produce neither Met nor Missed"
    );
    assert_eq!(
        row.value,
        Some(1.0),
        "the measured value still publishes as calibration evidence"
    );
    assert!(
        cross
            .limitations
            .iter()
            .any(|line| line.contains("compatibility series")),
        "the report says why the target was not applied: {:?}",
        cross.limitations
    );

    // Positive control: the same target on the report's own series still
    // decides the objective, so the boundary selects on series alone.
    let same = evaluate(
        &report(vec![handled], 1_000),
        &targets(&[("reliability.fallback_delivery_rate", calibrated(1.0, 1.0))]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    let row = result(&same, "reliability.fallback_delivery_rate", "session");
    assert_eq!(row.target, Some(1.0));
    assert_eq!(row.status, SloStatus::Met);
}

#[test]
fn a_baseline_below_the_indicator_sample_floor_is_refused() {
    // Evidence the contract itself would call too thin to *decide* the
    // objective must not be citable as the reason a target exists. The
    // denominator now travels in the evidence precisely so this is checked
    // rather than trusted.
    let mut under_floor = calibrated(0.99, 1.0);
    under_floor.baseline.eligible = Some(4); // speech presence floor is 15

    let file = targets(&[("availability.speech_presence_rate", under_floor.clone())]);
    let error = file
        .validate(&aivtuber_telemetry::catalog())
        .expect_err("below-floor evidence must not justify a target");
    assert!(error.to_string().contains("sample floor"), "{error}");

    // At the floor the same evidence validates, so the boundary is the floor
    // itself and nothing else.
    let at_floor = targets(&[("availability.speech_presence_rate", calibrated(0.99, 1.0))]);
    at_floor
        .validate(&aivtuber_telemetry::catalog())
        .expect("evidence at the sample floor is citable");
}

#[test]
fn a_version_one_target_file_without_the_eligible_denominator_is_refused_by_validation() {
    // Compatibility is part of the contract, not a parser accident. A
    // version-1 artifact written before the denominator existed must still
    // load — if parsing failed first, the file's own `schema_version` could
    // never be read to say which contract it wrote — and it is refused
    // afterwards, in validation, named for what it lacks instead of being
    // reported as an unreadable file.
    let bytes = br#"{
        "schema_version": "1",
        "catalog_version": "slo-catalog-v1",
        "targets": {
            "availability.speech_presence_rate": {
                "target": 0.95,
                "baseline": {
                    "value": 0.97,
                    "source": "run-legacy (dataset=slo-fixture, commit=deadbeef)",
                    "stream_hours": 3,
                    "series": "deterministic_semantic|slo-fixture|bench-v1|7",
                    "runs": [
                        {"run_id": "run-a", "git_commit": "deadbeef"},
                        {"run_id": "run-b", "git_commit": "deadbeef"}
                    ]
                }
            }
        }
    }"#;
    let file = SloTargets::from_json(bytes).expect("a version-1 target file must still parse");
    let error = file
        .validate(&catalog())
        .expect_err("evidence without a denominator cannot justify a target");
    assert!(
        error.to_string().contains("eligible"),
        "the refusal names the missing denominator, not a parse failure: {error}"
    );
}

#[test]
fn series_keys_escape_separators_so_distinct_tuples_cannot_collide() {
    // Under a raw `mode|dataset|config|seed` join, a report over dataset
    // `alpha|beta` + config `gamma` and a target calibrated for dataset
    // `alpha` + config `beta|gamma` produce the *same* key — the exact shape
    // that would let one workload's target decide another's verdict.
    let mut piped = report(
        vec![{
            let mut handled = observation(RouteClass::CachedFallback);
            handled.fallback_reason = Some("unavailable".to_owned());
            handled.event_to_first_audio_ms = Some(50);
            handled
        }],
        1_000,
    );
    piped.metadata.dataset_id = "alpha|beta".to_owned();
    piped.metadata.config_version = "gamma".to_owned();

    // The key the legacy unescaped join would have produced for the *other*
    // tuple: identical text, different workload.
    let mut colliding = calibrated(1.0, 1.0);
    colliding.baseline.series = Some("deterministic_semantic|alpha|beta|gamma|7".to_owned());
    let crossed = evaluate(
        &piped,
        &targets(&[("reliability.fallback_delivery_rate", colliding)]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    let row = result(&crossed, "reliability.fallback_delivery_rate", "session");
    assert_eq!(
        row.target, None,
        "a colliding tuple must not match across the separator"
    );
    assert_eq!(row.status, SloStatus::Uncalibrated);
    assert!(
        crossed
            .limitations
            .iter()
            .any(|line| line.contains("compatibility series")),
        "the report says why: {:?}",
        crossed.limitations
    );

    // The properly escaped key of the report's own tuple does apply, so the
    // escaping widens nothing — it only removes the collision.
    let mut matching = calibrated(1.0, 1.0);
    matching.baseline.series = Some("deterministic_semantic|alpha%7Cbeta|gamma|7".to_owned());
    let own = evaluate(
        &piped,
        &targets(&[("reliability.fallback_delivery_rate", matching)]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");
    let row = result(&own, "reliability.fallback_delivery_rate", "session");
    assert_eq!(row.target, Some(1.0));
    assert_eq!(row.status, SloStatus::Met);
}

#[test]
fn a_runtime_policy_silence_is_not_attributed_to_the_provider() {
    let mut event = observation(RouteClass::Silent);
    event.budget_denial_reason = Some("budget_exhausted".to_owned());
    assert_eq!(
        aivtuber_telemetry::failure_origin("budget_exhausted"),
        FailureOrigin::RuntimePolicy
    );
    assert_eq!(attribution_of(&event), Attribution::Unattributed);
    assert_eq!(
        aivtuber_telemetry::failure_origin("something_new"),
        FailureOrigin::Unknown
    );
}

#[test]
fn a_declared_invariant_is_reported_as_not_yet_measured_with_its_missing_evidence() {
    let evaluated = evaluate_default(&report(vec![observation(RouteClass::Silent)], 60_000));

    let stale = result(&evaluated, "safety.stale_dispatch_count", "session");
    assert_eq!(stale.status, SloStatus::NotYetMeasured);
    assert_eq!(stale.kind, ObjectiveKind::Invariant);
    assert!(
        stale
            .missing_evidence
            .as_deref()
            .is_some_and(|reason| reason.contains("asserts zero"))
    );
    assert!(
        !evaluated
            .error_budgets
            .iter()
            .any(|budget| budget.indicator == "safety.stale_dispatch_count")
    );
    assert!(
        evaluated
            .not_yet_measured
            .iter()
            .any(|entry| entry.starts_with("safety.stale_dispatch_count:"))
    );
}

#[test]
fn an_artifact_without_offsets_falls_back_to_one_session_window_and_says_why() {
    let evaluated = evaluate_default(&report(
        (0..30)
            .map(|_| observation(RouteClass::Deterministic))
            .collect(),
        7_200_000,
    ));

    assert_eq!(evaluated.windows.len(), 1);
    assert_eq!(evaluated.windows[0].kind, WindowKind::Session);
    assert!(
        evaluated
            .limitations
            .iter()
            .any(|limitation| limitation.contains("logical stream offsets"))
    );
}

#[test]
fn stream_hour_windows_partition_the_session_denominator() {
    let events: Vec<EventObservation> = (0..8)
        .flat_map(|hour| {
            (0..20_u64).map(move |index| {
                let mut event = observation(RouteClass::Deterministic);
                event.stream_offset_ms = Some(hour * STREAM_HOUR_MS + index * 60_000);
                event
            })
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 8 * STREAM_HOUR_MS),
        &targets(&[(
            "reliability.generative_budget_admission_rate",
            calibrated(1.0, 1.0),
        )]),
        SloEvaluationConfig {
            rolling_window_hours: 3,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");

    let hours: Vec<&SloWindow> = evaluated
        .windows
        .iter()
        .filter(|window| window.kind == WindowKind::StreamHour)
        .collect();
    assert_eq!(hours.len(), 8);
    assert!(
        hours.iter().all(|window| window.admitted_events == 20),
        "stream-hour windows must partition the session, not overlap"
    );
    let rolling: Vec<&SloWindow> = evaluated
        .windows
        .iter()
        .filter(|window| {
            matches!(
                window.kind,
                WindowKind::RollingStreamHours { window_hours: 3 }
            )
        })
        .collect();
    assert_eq!(rolling.len(), 6);
    assert!(rolling.iter().all(|window| window.admitted_events == 60));
    assert!(
        hours
            .iter()
            .map(|window| window.admitted_events)
            .sum::<u64>()
            == 160,
        "the hour buckets must cover the session exactly once"
    );
}

#[test]
fn a_persistent_run_of_missed_stream_hours_reaches_the_after_action_review() {
    let events: Vec<EventObservation> = (0..4)
        .flat_map(|hour| {
            (0..20_u64).map(move |index| {
                let mut event = observation(RouteClass::Silent);
                event.stream_offset_ms = Some(hour * STREAM_HOUR_MS + index * 60_000);
                event
            })
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 4 * STREAM_HOUR_MS),
        &targets(&[("availability.speech_presence_rate", calibrated(0.99, 1.0))]),
        SloEvaluationConfig {
            persistent_miss_windows: 3,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");

    assert_eq!(evaluated.verdict, aivtuber_telemetry::SloVerdict::Breach);
    let candidate = evaluated
        .aar_candidates
        .iter()
        .find(|candidate| candidate.indicator == "availability.speech_presence_rate")
        .expect("AAR candidate");
    assert!(candidate.consecutive_miss_windows >= 3);
    assert_eq!(candidate.recommended_action, SloAction::RaiseIncident);
    assert!(candidate.rationale.contains("consecutive"));
}

#[test]
fn a_single_missed_window_is_a_breach_but_not_yet_an_incident() {
    let events: Vec<EventObservation> = (0..20_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Silent);
            event.stream_offset_ms = Some(index * 60_000);
            event
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 20 * 60_000),
        &targets(&[("availability.speech_presence_rate", calibrated(0.99, 1.0))]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");

    assert_eq!(evaluated.verdict, aivtuber_telemetry::SloVerdict::Breach);
    assert!(
        evaluated.aar_candidates.is_empty(),
        "one noisy window must not page anyone: {:?}",
        evaluated.aar_candidates
    );
}

#[test]
fn an_experimental_plane_report_is_measured_but_excluded_from_the_active_slo() {
    let evaluated = evaluate(
        &report(
            (0..20)
                .map(|_| {
                    let mut event = observation(RouteClass::Deterministic);
                    event.deadline_class = Some(InteractionDeadlineClass::Conversation);
                    event
                })
                .collect(),
            20_000,
        ),
        &targets(&[("reliability.deadline_adherence_rate", calibrated(1.0, 1.0))]),
        SloEvaluationConfig {
            plane: TrafficPlane::Experimental,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");

    assert!(!evaluated.counts_toward_active_slo);
    assert_eq!(
        evaluated.verdict,
        aivtuber_telemetry::SloVerdict::Incomplete,
        "an experimental plane still reports unresolved evidence honestly"
    );
    assert!(
        evaluated
            .markdown_summary()
            .contains("excluded from the active")
    );
}

#[test]
fn a_scenario_replay_is_labelled_as_such_in_the_report() {
    let evaluated = evaluate(
        &report(vec![observation(RouteClass::Silent)], 60_000),
        &SloTargets::default(),
        SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");

    assert_eq!(evaluated.source.provenance, SloProvenance::ScenarioReplay);
    assert_eq!(evaluated.source.provenance.as_str(), "scenario_replay");
}

#[test]
fn the_report_names_the_artifact_it_was_evaluated_from() {
    let evaluated = evaluate_default(&report(vec![observation(RouteClass::Silent)], 60_000));

    let digest = evaluated
        .source
        .source_digest
        .as_deref()
        .expect("every freshly evaluated report records its source digest");
    assert_eq!(digest.len(), 64, "SHA-256 hex is 64 characters");
    assert!(
        digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "digest is lowercase hex: {digest}"
    );
}

#[test]
fn the_source_digest_is_deterministic_and_ignores_the_evaluation_config() {
    let artifact = report(vec![observation(RouteClass::Silent)], 60_000);
    let first = evaluate_default(&artifact);
    let second = evaluate(
        &artifact,
        &SloTargets::default(),
        SloEvaluationConfig {
            provenance: SloProvenance::ScenarioReplay,
            plane: TrafficPlane::Experimental,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");

    // The digest names the artifact, so it cannot depend on who
    // evaluated it, with which targets, or on which plane.
    assert_eq!(
        first.source.source_digest, second.source.source_digest,
        "one artifact must always produce one digest"
    );
}

#[test]
fn the_source_digest_changes_with_the_artifact_content() {
    let base = report(vec![observation(RouteClass::Silent)], 60_000);
    let base_digest = evaluate_default(&base).source.source_digest;

    // Same metadata, same duration, different observations: another
    // observation of the same revision and series, which is exactly
    // the case the series fields cannot distinguish.
    let other_artifacts = [
        report(vec![observation(RouteClass::Deterministic)], 60_000),
        {
            let mut rerun = metadata("slo-fixture", 60_000);
            rerun.seed = 8;
            BenchmarkReport::from_events(
                rerun,
                ComparisonMode::DeterministicSemantic,
                vec![observation(RouteClass::Silent)],
            )
            .expect("benchmark report")
        },
    ];
    for artifact in other_artifacts {
        let digest = evaluate_default(&artifact).source.source_digest;
        assert_ne!(
            digest, base_digest,
            "a different artifact must produce a different digest"
        );
    }
}

#[test]
fn insufficient_samples_do_not_decide_an_objective() {
    let events: Vec<EventObservation> = (0..5_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.budget_denial_reason = Some("budget_exhausted".to_owned());
            event
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 5_000),
        &targets(&[(
            "reliability.generative_budget_admission_rate",
            calibrated(1.0, 1.0),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("SLO report");

    let denial = result(
        &evaluated,
        "reliability.generative_budget_admission_rate",
        "session",
    );
    assert_eq!(denial.status, SloStatus::InsufficientSamples);
    assert_eq!(
        evaluated.verdict,
        aivtuber_telemetry::SloVerdict::Incomplete
    );
}

#[test]
fn the_catalog_defines_every_indicator_with_a_written_denominator_and_an_action() {
    for indicator in catalog() {
        assert!(!indicator.id.is_empty());
        assert!(
            !indicator.denominator.trim().is_empty(),
            "{} must state its denominator",
            indicator.id
        );
        assert!(
            !indicator.numerator.trim().is_empty(),
            "{} must state its numerator",
            indicator.id
        );
        assert!(
            !indicator.actions.is_empty(),
            "{} must document an action path for a breach",
            indicator.id
        );
        if indicator.evidence == EvidenceSource::Declared {
            assert!(
                indicator.missing_evidence.is_some(),
                "{} is declared without naming the missing evidence",
                indicator.id
            );
        }
    }
}

#[test]
fn a_report_round_trips_through_json() {
    let events: Vec<EventObservation> = (0..20_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event
        })
        .collect();
    let evaluated = evaluate_default(&report(events, 20_000));
    let bytes = evaluated.to_json_pretty().expect("json");
    let parsed: SloReport = serde_json::from_slice(&bytes).expect("round trip");
    assert_eq!(parsed, evaluated);
}

#[test]
fn a_miss_attribution_type_is_exported_for_consumers() {
    // Consumers classify a miss without re-deriving the provider/runtime split.
    let _: MissAttribution = attribution_of(&observation(RouteClass::Silent));
    let error = SloError::new("boom");
    assert_eq!(error.to_string(), "boom");
}

// ---------------------------------------------------------------------------
// Baseline selection from a run history (#58 metrics -> #71 targets)
// ---------------------------------------------------------------------------

fn detection_with_latency(count: u64) -> Vec<EventObservation> {
    (1..=count)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.event_to_first_visible_reaction_ms = Some(index);
            event.event_to_first_audio_ms = Some(index + 1);
            event
        })
        .collect()
}

/// Two structurally different runs of the *same* compatibility series: the
/// smallest history that may produce baseline evidence. The two runs are
/// distinguished by their run identity, as #58 requires.
fn two_run_history() -> (SloReport, SloReport) {
    let run_a = evaluate_run(
        &report(
            (0..20)
                .map(|_| observation(RouteClass::Deterministic))
                .collect(),
            2 * STREAM_HOUR_MS,
        ),
        "run-a",
    );
    let mut run_b_events = (0..10)
        .map(|_| observation(RouteClass::Deterministic))
        .collect::<Vec<_>>();
    run_b_events.extend(silence(10));
    let run_b = evaluate_run(&report(run_b_events, 2 * STREAM_HOUR_MS), "run-b");
    (run_a, run_b)
}

fn silence(count: u64) -> Vec<EventObservation> {
    (0..count)
        .map(|_| observation(RouteClass::Silent))
        .collect()
}

/// Events that each carry a *correct* reuse decision, so they sit inside
/// `quality.semantic_reuse_correctness`'s denominator. A run without them has an
/// empty denominator for that indicator and cannot contribute to its baseline.
fn reuse_decisions(count: u64) -> Vec<EventObservation> {
    (0..count)
        .map(|_| {
            let mut event = observation(RouteClass::Deterministic);
            event.wrong_reuse = Some(false);
            event
        })
        .collect()
}

#[test]
fn a_baseline_pools_the_session_denominator_across_runs() {
    // Run A: 20 conforming. Run B: 10 conforming and 10 unintended silences.
    let (run_a, run_b) = two_run_history();

    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(pooled.revisions, vec!["deadbeef".to_owned()]);
    assert_eq!(
        pooled.series.as_deref(),
        Some("deterministic_semantic|slo-fixture|bench-v1|7"),
        "the #58 compatibility series is named so the boundary is auditable"
    );

    let presence = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .expect("presence proposal");
    assert_eq!(presence.runs, 2);
    assert_eq!(presence.eligible, Some(40));
    assert_eq!(presence.conforming, Some(30));
    let baseline = presence.baseline.as_ref().expect("baseline evidence");
    assert!((baseline.value - 0.75).abs() < 1e-9, "{}", baseline.value);
    assert_eq!(baseline.stream_hours, Some(4));
    // Provenance survives the copy into a target: identities, not counts.
    assert!(baseline.source.contains("slo-baseline"));
    assert!(baseline.source.contains("datasets=slo-fixture"));
    assert!(baseline.source.contains("revisions=deadbeef"));
    assert!(baseline.source.contains("config=bench-v1"));
    assert!(baseline.source.contains("seed=7"));
    assert!(baseline.source.contains("mode=deterministic_semantic"));
    assert!(
        baseline
            .source
            .contains("series=deterministic_semantic|slo-fixture|bench-v1|7")
    );
    // ... and the identity is structured too, so the durable block names the
    // runs and the series without a consumer parsing prose.
    assert_eq!(
        baseline.series.as_deref(),
        Some("deterministic_semantic|slo-fixture|bench-v1|7")
    );
    assert_eq!(
        baseline
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run-a", "run-b"]
    );
}

#[test]
fn a_pooled_baseline_can_justify_a_target_without_being_invented() {
    let (run_a, run_b) = two_run_history();
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    let baseline = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .and_then(|proposal| proposal.baseline.clone())
        .expect("measured baseline");

    // The measured value is 0.75, but the product decision is a *higher* target
    // the runtime must keep holding; the proposal only supplies the evidence.
    let file = targets(&[(
        "availability.speech_presence_rate",
        SloTarget {
            target: 0.7,
            threshold_ms: None,
            baseline,
        },
    )]);
    file.validate(&catalog())
        .expect("a measured baseline is exactly what a target must cite");
}

/// Indicator denominators are per-run: a run with an empty denominator for one
/// indicator produced no evidence for it. If the manifest attached to that
/// indicator's baseline were the whole history's manifest, a single measured
/// run could satisfy the repeated-run floor and pass as calibration evidence.
#[test]
fn an_indicator_measured_in_only_one_run_is_not_target_ready_evidence() {
    // Only `quality.semantic_reuse_correctness` has a denominator that can be
    // empty in one run and populated in another: an event with no reuse
    // decision is outside that indicator's denominator, while a silence event
    // is an eligible miss for speech presence.
    let run_a = evaluate_run(&report(reuse_decisions(10), 2 * STREAM_HOUR_MS), "run-a");
    // No event here carries a reuse decision, so this run contributed nothing
    // to that indicator's baseline.
    let run_b = evaluate_run(&report(silence(10), 2 * STREAM_HOUR_MS), "run-b");
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");

    let reuse = pooled
        .proposals
        .get("quality.semantic_reuse_correctness")
        .expect("the indicator was measured in the history");
    assert_eq!(reuse.runs, 1, "only run-a had an eligible denominator");
    assert_eq!(reuse.eligible, Some(10));
    assert_eq!(reuse.conforming, Some(10));
    assert!(
        reuse.baseline.is_none(),
        "one measured run is a data point, not a baseline: {:?}",
        reuse.baseline
    );

    // The history is still two runs; the shortfall is named, not hidden.
    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(pooled.contributing_runs.len(), 2);
    assert!(
        pooled.limitations.iter().any(|limitation| {
            limitation.contains("quality.semantic_reuse_correctness")
                && limitation.contains("fewer than 2")
        }),
        "the under-sampled indicator is named: {:?}",
        pooled.limitations
    );

    // Non-vacuous: suppressing the one-run indicator did not suppress the rest
    // of the history's calibration.
    let still_calibrated = pooled
        .proposals
        .iter()
        .filter(|(_, proposal)| {
            proposal
                .baseline
                .as_ref()
                .is_some_and(|baseline| baseline.runs.len() == 2)
        })
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    assert!(
        !still_calibrated.is_empty(),
        "indicators both runs measured still publish evidence: {:?}",
        pooled.proposals.keys().collect::<Vec<_>>()
    );
}

/// Coverage is the contributing runs' represented time, not the history's: a
/// long run that contributed nothing to an indicator must not inflate the
/// hours attached to that indicator's evidence.
#[test]
fn evidence_coverage_counts_only_contributing_runs() {
    let run_a = evaluate_run(&report(reuse_decisions(10), 2 * STREAM_HOUR_MS), "run-a");
    let run_b = evaluate_run(&report(reuse_decisions(10), 3 * STREAM_HOUR_MS), "run-b");
    let run_c = evaluate_run(&report(silence(10), 5 * STREAM_HOUR_MS), "run-c");

    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b, run_c]).expect("proposal set");
    let baseline = pooled
        .proposals
        .get("quality.semantic_reuse_correctness")
        .and_then(|proposal| proposal.baseline.clone())
        .expect("two contributing runs");

    assert_eq!(
        baseline
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run-a", "run-b"],
        "run-c has no eligible denominator for this indicator"
    );
    assert_eq!(
        baseline.stream_hours,
        Some(5),
        "2h + 3h of contributing runs, not the history's 10h"
    );
    assert!(
        baseline.source.contains("[run-a,run-b]"),
        "{}",
        baseline.source
    );
    assert!(
        !baseline.source.contains("run-c"),
        "the prose names the same runs as the manifest: {}",
        baseline.source
    );
}

#[test]
fn a_single_run_calibrates_nothing() {
    let (run_a, _) = two_run_history();
    let pooled = BaselineProposalSet::from_reports(&[run_a]).expect("proposal set");
    assert_eq!(pooled.contributing_reports, 1);
    assert!(
        pooled.proposals.is_empty(),
        "one run is a data point, not a baseline: {:?}",
        pooled.proposals
    );
    assert!(
        pooled
            .limitations
            .iter()
            .any(|limitation| limitation.contains("at least 2"))
    );
}

#[test]
fn a_repeated_run_identity_is_one_run_not_repeated_evidence() {
    let (run_a, _) = two_run_history();
    let pooled = BaselineProposalSet::from_reports(&[run_a.clone(), run_a.clone(), run_a])
        .expect("proposal set");
    assert_eq!(pooled.reports, 3);
    assert_eq!(pooled.contributing_reports, 1);
    assert_eq!(pooled.duplicate_reports, 2);
    assert_eq!(pooled.contributing_runs.len(), 1);
    assert!(
        pooled.proposals.is_empty(),
        "a retried run is still one run"
    );
}

#[test]
fn two_independent_runs_with_identical_payloads_are_two_runs() {
    // Deterministic replay makes two independent runs of one revision produce
    // byte-identical *reports*. Only the run identity distinguishes them, and
    // they are two measurements of runner variance (#58), so content equality
    // must never collapse them into one.
    let events = detection_with_latency(20);
    let run_a = evaluate_run(&report(events.clone(), STREAM_HOUR_MS), "nightly-1");
    let run_b = evaluate_run(&report(events, STREAM_HOUR_MS), "nightly-2");

    // The measurement payload is identical; only the run identity differs.
    let mut stripped_a = run_a.clone();
    stripped_a.source.run_id = None;
    let mut stripped_b = run_b.clone();
    stripped_b.source.run_id = None;
    assert_eq!(stripped_a, stripped_b, "the two payloads are identical");

    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(pooled.duplicate_reports, 0);
    let presence = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .expect("two independent runs are calibration evidence");
    assert_eq!(presence.runs, 2);
    assert_eq!(presence.eligible, Some(40), "both runs contribute");
    assert_eq!(
        presence.baseline.as_ref().map(|baseline| baseline.value),
        Some(1.0)
    );
}

#[test]
fn a_report_without_a_run_identity_is_refused_rather_than_guessed() {
    let anonymous = evaluate_default(&report(detection_with_latency(20), STREAM_HOUR_MS));
    let error = BaselineProposalSet::from_reports(&[anonymous])
        .expect_err("identity cannot be inferred from report content");
    assert!(error.to_string().contains("run identity"), "{error}");
}

#[test]
fn a_conflicting_retry_is_refused_identically_in_both_argument_orders() {
    // Two attempts of one run disagree. `run_id` says they are the same logical
    // run; it does not say which attempt is newer, and the order the files are
    // handed over is not provenance. Selecting "the last one supplied" would
    // make durable calibration evidence a function of argv/glob order, so the
    // history fails closed instead — and both orders must fail the same way.
    let original = evaluate_run(
        &report(detection_with_latency(20), STREAM_HOUR_MS),
        "nightly-1",
    );
    let retried = evaluate_run(&report(silence(20), STREAM_HOUR_MS), "nightly-1");
    let independent = evaluate_run(
        &report(detection_with_latency(20), STREAM_HOUR_MS),
        "nightly-2",
    );

    // The two attempts really do carry different measurements.
    assert_ne!(original.indicators, retried.indicators);

    let forward = BaselineProposalSet::from_reports(&[
        original.clone(),
        retried.clone(),
        independent.clone(),
    ])
    .expect_err("a conflicting retry is not poolable");
    let reversed = BaselineProposalSet::from_reports(&[retried, original, independent])
        .expect_err("the same conflict must fail the same way when reversed");
    assert_eq!(
        forward.to_string(),
        reversed.to_string(),
        "the outcome must not depend on argument order"
    );
    assert_eq!(
        forward.to_string().matches("nightly-1").count(),
        1,
        "the refusal names the ambiguous run: {forward}"
    );
    assert!(
        forward.to_string().contains("different measurements"),
        "{forward}"
    );
}

#[test]
fn the_pooled_baseline_names_every_run_and_revision_it_was_pooled_from() {
    let (run_a, mut run_b) = two_run_history();
    run_b.source.git_commit = "cafebabe".to_owned();
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    assert_eq!(
        pooled.revisions,
        vec!["cafebabe".to_owned(), "deadbeef".to_owned()]
    );
    assert_eq!(
        pooled
            .contributing_runs
            .iter()
            .map(|run| (run.run_id.as_str(), run.git_commit.as_str()))
            .collect::<Vec<_>>(),
        vec![("run-a", "deadbeef"), ("run-b", "cafebabe")],
        "a target has to be able to name the runs and revisions behind its baseline"
    );
    let baseline = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .and_then(|proposal| proposal.baseline.clone())
        .expect("baseline");
    assert_eq!(
        baseline.runs, pooled.contributing_runs,
        "the evidence block itself carries the manifest, so copying it out of \
         the proposal keeps the identity"
    );
}

#[test]
fn a_baseline_citing_an_unnamed_run_is_refused() {
    let mut target = calibrated(0.9, 0.95);
    target.baseline.runs = vec![BaselineRun {
        run_id: "  ".to_owned(),
        git_commit: "deadbeef".to_owned(),
    }];
    let file = targets(&[("availability.speech_presence_rate", target)]);
    let error = file
        .validate(&catalog())
        .expect_err("a run that cannot be named is not evidence");
    assert!(error.to_string().contains("empty run identity"), "{error}");
}

/// The bypass the review reproduced: a target whose `baseline` is a
/// hand-written prose string, with no series and no measured runs, used to
/// validate cleanly — a direct route around the whole evidence chain this
/// contract exists to build.
#[test]
fn a_handwritten_target_without_a_run_manifest_is_refused() {
    let bytes = br#"{
        "target": 0.95,
        "baseline": {
            "value": 0.95,
            "source": "handwritten, not a measured artifact",
            "stream_hours": 1,
            "eligible": 15
        }
    }"#;
    let mut file = SloTargets::default();
    file.targets.insert(
        "availability.speech_presence_rate".to_owned(),
        serde_json::from_slice(bytes).expect("target"),
    );
    let error = file
        .validate(&catalog())
        .expect_err("a target must prove which measured runs formed its baseline");
    assert!(
        error.to_string().contains("compatibility series"),
        "a baseline outside a known series is not evidence about a workload: {error}"
    );
}

#[test]
fn a_target_with_no_compatibility_series_is_refused() {
    let mut target = calibrated(0.9, 0.95);
    target.baseline.series = None;
    let file = targets(&[("availability.speech_presence_rate", target)]);
    let error = file
        .validate(&catalog())
        .expect_err("the series is part of the evidence");
    assert!(
        error.to_string().contains("compatibility series"),
        "{error}"
    );
}

#[test]
fn a_target_that_names_no_measured_runs_is_refused() {
    let mut target = calibrated(0.9, 0.95);
    target.baseline.runs.clear();
    let file = targets(&[("availability.speech_presence_rate", target)]);
    let error = file
        .validate(&catalog())
        .expect_err("no named run is no calibration evidence");
    assert!(error.to_string().contains("at least 2"), "{error}");
}

#[test]
fn a_target_whose_run_has_no_revision_is_refused() {
    let mut target = calibrated(0.9, 0.95);
    target.baseline.runs[0].git_commit = "  ".to_owned();
    let file = targets(&[("availability.speech_presence_rate", target)]);
    let error = file
        .validate(&catalog())
        .expect_err("evidence whose revision is unknown is not auditable");
    assert!(error.to_string().contains("without a revision"), "{error}");
}

#[test]
fn a_target_counting_one_run_twice_is_refused() {
    let mut target = calibrated(0.9, 0.95);
    let first = target.baseline.runs[0].run_id.clone();
    target.baseline.runs[1].run_id = first;
    let file = targets(&[("availability.speech_presence_rate", target)]);
    let error = file
        .validate(&catalog())
        .expect_err("a run identity counts one contribution");
    assert!(error.to_string().contains("more than once"), "{error}");
}

#[test]
fn a_history_spanning_two_compatibility_series_is_refused() {
    // Each boundary field of the #58 series key restarts the series.
    let mut broken_on_config = run_a().clone();
    broken_on_config.source.config_version = "bench-v2".to_owned();
    let error = BaselineProposalSet::from_reports(&[run_a(), broken_on_config])
        .expect_err("different config_version is a #58 series boundary");
    assert!(
        error
            .to_string()
            .contains("more than one compatibility series"),
        "{error}"
    );

    let mut broken_on_seed = run_a().clone();
    broken_on_seed.source.seed = 8;
    assert!(
        BaselineProposalSet::from_reports(&[run_a(), broken_on_seed]).is_err(),
        "a different seed is also a series boundary"
    );

    let mut broken_on_dataset = run_a().clone();
    broken_on_dataset.source.dataset_id = "other-dataset".to_owned();
    assert!(
        BaselineProposalSet::from_reports(&[run_a(), broken_on_dataset]).is_err(),
        "a different dataset is also a series boundary"
    );

    let mut broken_on_mode = run_a().clone();
    broken_on_mode.source.mode = ComparisonMode::DeterministicOnly;
    assert!(
        BaselineProposalSet::from_reports(&[run_a(), broken_on_mode]).is_err(),
        "a different mode is also a series boundary"
    );
}

/// One fresh active run per call, so a test never pools the same value twice.
fn run_a() -> SloReport {
    let (run, _) = two_run_history();
    run
}

#[test]
fn a_report_for_another_schema_version_fails_closed() {
    let (run_a, _) = two_run_history();
    let mut future = run_a;
    future.schema_version = "future-schema-v999".to_owned();
    let error = BaselineProposalSet::from_reports(&[future])
        .expect_err("a foreign report contract must not pool");
    assert!(error.to_string().contains("fails closed"), "{error}");
}

#[test]
fn sub_hour_evidence_omits_stream_hours_instead_of_claiming_one() {
    let run_a = evaluate_run(
        &report(
            (0..20)
                .map(|_| observation(RouteClass::Deterministic))
                .collect(),
            2_000,
        ),
        "run-a",
    );
    let run_b = evaluate_run(&report(silence(20), 2_000), "run-b");
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    let baseline = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .and_then(|proposal| proposal.baseline.clone())
        .expect("baseline");
    assert_eq!(
        baseline.stream_hours, None,
        "4 seconds of represented time is not one stream hour"
    );
    assert!(
        pooled
            .limitations
            .iter()
            .any(|limitation| limitation.contains("less than one full stream hour"))
    );
}

#[test]
fn the_minimum_run_floor_is_two_and_exported() {
    assert_eq!(MIN_BASELINE_RUNS, 2);
    const { assert!(MIN_BASELINE_RUNS > 1, "one run must never calibrate") };
}

#[test]
fn an_experimental_report_does_not_move_the_active_baseline() {
    let active = evaluate_run(
        &report(detection_with_latency(20), STREAM_HOUR_MS),
        "active-a",
    );
    let experimental = evaluate(
        &report(detection_with_latency(20), STREAM_HOUR_MS),
        &SloTargets::default(),
        SloEvaluationConfig {
            plane: TrafficPlane::Experimental,
            run_id: Some("shadow-a".to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report");
    // A second active run, because one run calibrates nothing by itself.
    let active_b = evaluate_run(&report(silence(20), STREAM_HOUR_MS), "active-b");

    let pooled =
        BaselineProposalSet::from_reports(&[active, active_b, experimental]).expect("proposal set");
    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(pooled.excluded_experimental_reports, 1);
    let presence = pooled
        .proposals
        .get("availability.speech_presence_rate")
        .expect("presence proposal");
    assert_eq!(
        presence.eligible,
        Some(40),
        "the shadow run must not be pooled: only the two active runs count"
    );
    assert!(
        pooled
            .limitations
            .iter()
            .any(|limitation| limitation.contains("experimental-plane"))
    );
}

#[test]
fn a_history_of_mixed_catalog_versions_is_refused() {
    let mut stale = evaluate_default(&report(detection_with_latency(5), STREAM_HOUR_MS));
    stale.catalog_version = "slo-catalog-v0".to_owned();
    let error = BaselineProposalSet::from_reports(&[stale])
        .expect_err("mixed definitions are not comparable");
    assert!(error.to_string().contains("not comparable"), "{error}");
}

#[test]
fn a_latency_objective_publishes_percentiles_and_never_a_zero_ratio() {
    let latency_run_a = evaluate_run(
        &report(detection_with_latency(40), STREAM_HOUR_MS),
        "latency-run-a",
    );
    let (_, run_b) = two_run_history();
    // Same series (same dataset/config/seed/mode) — run_b varies in content but
    // the metadata helper keeps the series identical, so pool them together.
    let pooled = BaselineProposalSet::from_reports(&[latency_run_a, run_b]).expect("proposal set");

    let audio = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    assert!(
        audio.baseline.is_none(),
        "without a probe boundary there is no measured latency ratio"
    );
    let latency = audio.latency.as_ref().expect("percentiles");
    assert_eq!(latency.samples, 40);
    assert_eq!(latency.field, "event_to_first_audio_ms");
    assert!(
        latency.p95_ms > 0
            && latency.max_ms.expect("a full report records a max") >= latency.p99_ms
    );
}

#[test]
fn the_first_latency_target_is_constructible_from_measured_artifacts_alone() {
    // The calibration cycle: percentiles -> candidate boundary -> measured
    // conforming ratio -> valid target. Every step is measured; nothing is
    // invented.
    let run_a = evaluate_run(
        &report(detection_with_latency(40), STREAM_HOUR_MS),
        "probe-run-a",
    );
    let run_b = evaluate_run(
        &report(detection_with_latency(40), STREAM_HOUR_MS + 1_000),
        "probe-run-b",
    );
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");

    let audio = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    let latency = audio.latency.as_ref().expect("target-free percentiles");
    // 1. A candidate boundary is chosen *from* the measured percentiles.
    let threshold_ms = latency.p95_ms;

    // 2. The boundary is *measured* over the same history via the probe.
    let probe = SloTarget::threshold_probe(threshold_ms);
    let measured_a = evaluate(
        &BenchmarkReport::from_events(
            metadata("slo-fixture", STREAM_HOUR_MS),
            ComparisonMode::DeterministicSemantic,
            detection_with_latency(40),
        )
        .expect("benchmark report"),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            probe.clone(),
        )]),
        SloEvaluationConfig {
            run_id: Some("probe-run-a".to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("probed SLO report");
    let measured_b = evaluate(
        &BenchmarkReport::from_events(
            metadata("slo-fixture", STREAM_HOUR_MS + 1_000),
            ComparisonMode::DeterministicSemantic,
            detection_with_latency(40),
        )
        .expect("benchmark report"),
        &targets(&[("availability.event_to_first_audio_within_target", probe)]),
        SloEvaluationConfig {
            run_id: Some("probe-run-b".to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("probed SLO report");
    let probed = BaselineProposalSet::from_reports(&[measured_a, measured_b]).expect("probed set");

    // 3. The probe result now carries a measured ratio at that boundary.
    let probed_audio = probed
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("probed proposal");
    assert_eq!(probed_audio.threshold_ms, Some(threshold_ms));
    assert_eq!(probed_audio.runs, 2);
    let baseline = probed_audio.baseline.clone().expect("measured ratio");
    assert!((0.0..=1.0).contains(&baseline.value));
    assert!(baseline.source.contains("probe"), "{}", baseline.source);

    // 4. A target calibrated with that measured evidence is valid. This is the
    // step that was impossible before: no invented number anywhere.
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        SloTarget {
            target: 0.9,
            threshold_ms: Some(threshold_ms),
            baseline: baseline.clone(),
        },
    )]);
    file.validate(&catalog())
        .expect("the first latency target must be constructible from measurements");
}

/// The calibration floor is per indicator here too: a probed run with no
/// eligible latency samples at the candidate boundary measured no ratio at that
/// boundary, so it cannot count toward the floor.
#[test]
fn a_latency_boundary_measured_in_only_one_run_is_not_target_ready() {
    let threshold_ms = 50_u64;
    let probe = |events: Vec<EventObservation>, run_id: &str| {
        evaluate(
            &report(events, STREAM_HOUR_MS),
            &targets(&[(
                "availability.event_to_first_audio_within_target",
                SloTarget::threshold_probe(threshold_ms),
            )]),
            SloEvaluationConfig {
                run_id: Some(run_id.to_owned()),
                ..SloEvaluationConfig::default()
            },
        )
        .expect("probed SLO report")
    };
    let measured_a = probe(detection_with_latency(40), "probe-run-a");
    // Silence carries no first-audio sample, so this run has no eligible
    // observation at the candidate boundary.
    let measured_b = probe(silence(10), "probe-run-b");

    let pooled = BaselineProposalSet::from_reports(&[measured_a, measured_b]).expect("probed set");
    let audio = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    assert_eq!(
        audio.threshold_ms,
        Some(threshold_ms),
        "the boundary was still probed, so the candidate is published"
    );
    assert_eq!(audio.runs, 1, "only one run had samples at that boundary");
    assert!(
        audio.baseline.is_none(),
        "a ratio measured in one run is not calibration evidence: {:?}",
        audio.baseline
    );
    assert!(
        pooled.limitations.iter().any(|limitation| {
            limitation.contains("availability.event_to_first_audio_within_target")
                && limitation.contains("fewer than 2")
        }),
        "the under-sampled indicator is named: {:?}",
        pooled.limitations
    );
}

/// The durable latency-calibration cycle.
///
/// A measured run is written to storage and later re-read as a plain artifact;
/// nothing below uses the in-memory value. The stored `SloReport` alone can only
/// supply the candidate boundary — it is not a benchmark report — so the probe
/// reads the retrieved replay report. That is the step the durable history has
/// to keep possible after the trusted main job has finished.
#[test]
fn a_stored_run_can_still_be_probed_and_pooled_after_it_is_retrieved() {
    let directory = std::env::temp_dir().join(format!(
        "aivtuber-slo-retrieval-{}-stored-run",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).expect("scratch directory");
    let run_ids = ["stored-a", "stored-b"];

    // Persist each measured run, then retrieve it from storage.
    let mut retrieved = Vec::new();
    for index in 0..run_ids.len() {
        let measured = report(detection_with_latency(40), 2 * STREAM_HOUR_MS);
        let path = directory.join(format!("{index}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&measured).expect("serialize"),
        )
        .expect("persist replay report");
        let bytes = std::fs::read(&path).expect("retrieve replay report");
        retrieved.push(serde_json::from_slice::<BenchmarkReport>(&bytes).expect("parse"));
    }
    let _ = std::fs::remove_dir_all(&directory);

    // 1. The stored SLO report selects a candidate boundary. It cannot be the
    // probe's input: it is not a replay benchmark report.
    let free: Vec<SloReport> = retrieved
        .iter()
        .zip(run_ids)
        .map(|(run, run_id)| {
            evaluate(
                run,
                &SloTargets::default(),
                SloEvaluationConfig {
                    run_id: Some(run_id.to_owned()),
                    ..SloEvaluationConfig::default()
                },
            )
            .expect("target-free report")
        })
        .collect();
    let audio_percentile = |report: &SloReport| {
        report
            .latency_calibration
            .iter()
            .find(|point| point.window == "session" && point.field == "event_to_first_audio_ms")
            .map(|point| point.p95_ms)
            .expect("target-free audio percentiles")
    };
    let threshold_ms = audio_percentile(&free[0]);
    assert!(
        serde_json::from_slice::<BenchmarkReport>(&serde_json::to_vec(&free[0]).expect("encode"))
            .is_err(),
        "a stored SLO report is not a replay benchmark report, which is why the raw \
         report must be persisted for the probe step"
    );

    // 2. Probe every retrieved run at the chosen boundary.
    let probe_targets = targets(&[(
        "availability.event_to_first_audio_within_target",
        SloTarget::threshold_probe(threshold_ms),
    )]);
    let probed: Vec<SloReport> = retrieved
        .iter()
        .zip(run_ids)
        .map(|(run, run_id)| {
            evaluate(
                run,
                &probe_targets,
                SloEvaluationConfig {
                    run_id: Some(run_id.to_owned()),
                    ..SloEvaluationConfig::default()
                },
            )
            .expect("probed report")
        })
        .collect();

    // 3. Pool them into target-ready evidence.
    let pooled = BaselineProposalSet::from_reports(&probed).expect("probed proposal set");
    let audio = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    assert_eq!(audio.threshold_ms, Some(threshold_ms));
    assert_eq!(
        audio.runs, 2,
        "both stored runs were probed at the boundary"
    );
    let baseline = audio
        .baseline
        .clone()
        .expect("a probed boundary measured in two stored runs is target-ready");
    assert_eq!(baseline.runs.len(), 2);
    assert!(
        (0.0..=1.0).contains(&baseline.value),
        "the ratio is measured, not invented: {}",
        baseline.value
    );

    // 4. And it loads as a target: the first latency target comes entirely from
    // artifacts that survived the run that produced them.
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        SloTarget {
            target: 0.9,
            threshold_ms: Some(threshold_ms),
            baseline,
        },
    )]);
    file.validate(&catalog())
        .expect("a latency target calibrated from retrieved stored artifacts");
}

#[test]
fn a_probe_target_is_never_valid_calibration_evidence() {
    // The probe carries an unmeasured placeholder baseline; the validator must
    // never accept it in a targets file, so a probed ratio cannot be skipped.
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        SloTarget::threshold_probe(250),
    )]);
    let error = file
        .validate(&catalog())
        .expect_err("a probe is not calibration evidence");
    assert!(
        error.to_string().contains("latency probe"),
        "the probe's placeholder source is refused: {error}"
    );
    // But the evaluation path may use it: that is how the boundary is measured.
    file.validate_for_evaluation(&catalog())
        .expect("a probe drives the measuring evaluation");
}

#[test]
fn no_evidence_in_the_history_leaves_an_objective_not_measured() {
    let run_a = evaluate_run(&report(silence(5), 60_000), "run-a");
    let run_b = evaluate_run(&report(silence(6), 60_000), "run-b");
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");

    assert!(
        pooled
            .not_measured
            .contains(&"availability.event_to_first_visible_within_target".to_owned()),
        "a latency objective with zero samples is unmeasured, not a 0% baseline"
    );
    assert!(
        !pooled
            .proposals
            .contains_key("availability.event_to_first_visible_within_target")
    );
}

#[test]
fn a_baseline_proposal_set_round_trips_through_json() {
    let (run_a, run_b) = two_run_history();
    let pooled = BaselineProposalSet::from_reports(&[run_a, run_b]).expect("proposal set");
    let bytes = pooled.to_json_pretty().expect("json");
    let parsed: BaselineProposalSet = serde_json::from_slice(&bytes).expect("round trip");
    assert_eq!(parsed, pooled);
    let summary = pooled.markdown_summary();
    assert!(summary.contains("measured evidence, not a target"));
    assert!(summary.contains("Series:"), "series is shown for audit");
}

// ---------------------------------------------------------------------------
// #58 aggregate history -> baseline selection (issue #71)
// ---------------------------------------------------------------------------

/// A production #58 aggregate row, verbatim from the `benchmark-data` branch
/// (`data/replay-comparison.deterministic_only__starter-replay-comparison-v1__
/// replay-benchmark-v1-top_k-2-reuse_thresh__4242-w6bnyp.jsonl`, first line),
/// pinned so the ingest can never drift from the bytes history actually
/// recorded.
const PRODUCTION_AGGREGATE_ROW: &str = r#"{"schema_version":"1","benchmark_suite":"replay-comparison","mode":"deterministic_only","dataset_id":"starter-replay-comparison-v1","git":{"commit":"039c151696b0ce8faedd27cf0b2d3e956f51222e"},"environment":{"os":"linux","architecture":"x86_64","rust_version":"rustc 1.98.1 (48a229cea 2026-09-01)","bun_version":"1.3.14","cargo_profile":"debug"},"configuration":{"config_version":"replay-benchmark-v1;top_k=2;reuse_threshold=0.85;min_route_confidence=0.5;min_reaction_spacing_ms=0;llm_cost_microunits=0;tts_cost_microunits=0","runtime_profile":"cached","asset_version":"starter-v1/compiler-0.1.0","index_version":"asset-semantic-fnv1a64-b45ae4ea0463f981","retriever_version":"benchmark-semantic-v1","seed":4242,"stream_duration_ms":180000},"metrics":{"cached.first_audio.p50_ms":{"value":83,"sample_count":3},"cached.first_audio.p95_ms":{"value":112,"sample_count":3},"cached.first_audio.p99_ms":{"value":112,"sample_count":3},"cached.first_visible.p95_ms":{"value":33,"sample_count":3},"routing.llm_calls_per_100_events":{"value":0,"sample_count":3},"routing.route_decision.p95_us":{"value":3,"sample_count":3},"semantic.wrong_reuse_rate_pct":{"value":0,"sample_count":0}},"invariants":{"reliability.deterministic_replay_mismatch_count":{"value":0},"reliability.stale_dispatch_count":{"value":0},"reliability.unauthorized_privileged_action_count":{"value":0},"resource.invalid_route_transition_count":{"value":0},"resource.retention_bound_violation_count":{"value":0}},"recording":{"run_id":"run-36719154068","recorded_at":"2026-09-30T13:06:12Z","attempt":1}}"#;

/// A minimal #58 aggregate history row, shaped exactly like the rows the
/// `benchmark-data` recorder appends, with the knobs each pooling rule is
/// tested through.
fn aggregate_row(run_id: &str, commit: &str, seed: u64, p95_ms: u64) -> String {
    serde_json::json!({
        "schema_version": "1",
        "benchmark_suite": "replay-comparison",
        "mode": "deterministic_only",
        "dataset_id": "slo-fixture",
        "git": { "commit": commit },
        "environment": {
            "os": "linux",
            "architecture": "x86_64",
            "rust_version": "rustc 1.98.1 (48a229cea 2026-09-01)",
            "cargo_profile": "debug"
        },
        "configuration": {
            "config_version": "bench-v1",
            "asset_version": "starter-v1",
            "seed": seed,
            "stream_duration_ms": 180000
        },
        "metrics": {
            "cached.first_audio.p50_ms": { "value": p95_ms.saturating_sub(20), "sample_count": 3 },
            "cached.first_audio.p95_ms": { "value": p95_ms, "sample_count": 3 },
            "cached.first_audio.p99_ms": { "value": p95_ms, "sample_count": 3 }
        },
        "invariants": {},
        "recording": { "run_id": run_id, "recorded_at": "2026-09-30T13:06:12Z", "attempt": 1 }
    })
    .to_string()
}

fn aggregate_shell(run_id: &str, commit: &str, seed: u64, p95_ms: u64) -> SloReport {
    let row = BenchmarkResult::from_json(aggregate_row(run_id, commit, seed, p95_ms).as_bytes())
        .expect("aggregate row parses");
    SloReport::from_benchmark_result(&row).expect("shell")
}

#[test]
fn a_production_aggregate_history_row_becomes_a_percentile_only_shell() {
    let row = BenchmarkResult::from_json(PRODUCTION_AGGREGATE_ROW.as_bytes())
        .expect("the bytes history actually recorded parse");
    let shell = SloReport::from_benchmark_result(&row).expect("shell");

    // Percentiles flow through; nothing else claims to be measured: no
    // indicator rows, no windows, no invariant tallies.
    assert!(shell.indicators.is_empty());
    assert!(shell.windows.is_empty());
    assert!(shell.error_budgets.is_empty());
    assert_eq!(shell.latency_calibration.len(), 1);
    let point = &shell.latency_calibration[0];
    assert_eq!(
        point.indicator,
        "availability.event_to_first_audio_within_target"
    );
    assert_eq!(point.window, "session");
    assert_eq!(point.field, "event_to_first_audio_ms");
    assert_eq!((point.p50_ms, point.p95_ms, point.p99_ms), (83, 112, 112));
    assert_eq!(point.samples, 3);
    assert_eq!(
        point.max_ms, None,
        "the aggregate row records no per-run maximum, and none is invented"
    );

    // Identity comes straight from the row, so the usual pooling rules apply.
    assert_eq!(shell.source.run_id.as_deref(), Some("run-36719154068"));
    assert_eq!(
        shell.source.git_commit,
        "039c151696b0ce8faedd27cf0b2d3e956f51222e"
    );
    assert_eq!(shell.source.mode, ComparisonMode::DeterministicOnly);
    assert_eq!(shell.source.dataset_id, "starter-replay-comparison-v1");
    assert_eq!(shell.source.seed, 4242);
    assert_eq!(shell.source.stream_duration_ms, Some(180_000));
    assert!(shell.counts_toward_active_slo);
    assert_eq!(shell.verdict, SloVerdict::Uncalibrated);

    // A shell is a report artifact: it survives serialization unchanged, with
    // the absent maximum simply absent.
    let bytes = serde_json::to_string(&shell).expect("serialize");
    assert!(!bytes.contains("max_ms"));
    let round_tripped: SloReport = serde_json::from_str(&bytes).expect("parse");
    assert_eq!(round_tripped, shell);
}

#[test]
fn aggregate_history_rows_pool_latency_evidence_but_never_a_citable_baseline() {
    let pooled = BaselineProposalSet::from_reports(&[
        aggregate_shell("run-a", "aaaa1111", 7, 100),
        aggregate_shell("run-b", "bbbb2222", 7, 200),
    ])
    .expect("proposal set");

    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(
        pooled.series.as_deref(),
        Some("deterministic_only|slo-fixture|bench-v1|7")
    );
    assert_eq!(
        pooled.revisions,
        vec!["aaaa1111".to_owned(), "bbbb2222".to_owned()]
    );

    let proposal = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    let latency = proposal.latency.as_ref().expect("target-free percentiles");
    assert_eq!(latency.runs, 2);
    assert_eq!(latency.samples, 6);
    assert_eq!(
        latency.p95_ms, 200,
        "the worst observed percentile across runs is the conservative evidence"
    );
    assert_eq!(latency.p99_ms, 200);
    assert_eq!(latency.max_ms, None, "no per-run maximum was recorded");
    assert_eq!(proposal.threshold_ms, None);
    assert_eq!(proposal.eligible, None);
    assert_eq!(proposal.conforming, None);
    assert!(
        proposal.baseline.is_none(),
        "an aggregate history carries no denominators, so it never publishes a citable baseline"
    );

    // Ratio objectives are honestly absent, not silently zero.
    assert!(
        pooled
            .not_measured
            .contains(&"availability.speech_presence_rate".to_owned())
    );

    // The markdown renders the missing maximum as unknown, never as zero.
    let markdown = pooled.markdown_summary();
    assert!(markdown.contains("200 / 200 / -"), "{markdown}");
}

#[test]
fn aggregate_history_rows_from_two_compatibility_series_are_refused() {
    let error = BaselineProposalSet::from_reports(&[
        aggregate_shell("run-a", "aaaa1111", 7, 100),
        aggregate_shell("run-b", "bbbb2222", 9, 100),
    ])
    .expect_err("one series per history");
    assert!(
        error.to_string().contains("compatibility series"),
        "{error}"
    );
}

#[test]
fn an_aggregate_history_row_without_a_run_identity_is_refused() {
    let mut row: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 7, 100)).expect("json");
    row.as_object_mut()
        .expect("object")
        .remove("recording")
        .expect("recording present to remove");
    let row = BenchmarkResult::from_json(row.to_string().as_bytes()).expect("row parses");
    let error = SloReport::from_benchmark_result(&row).expect_err("no identity, no evidence");
    assert!(error.to_string().contains("recording.run_id"), "{error}");
}

#[test]
fn an_aggregate_history_row_without_a_series_dataset_is_refused() {
    let mut row: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 7, 100)).expect("json");
    row.as_object_mut()
        .expect("object")
        .remove("dataset_id")
        .expect("dataset_id present to remove");
    let row = BenchmarkResult::from_json(row.to_string().as_bytes()).expect("row parses");
    let error = SloReport::from_benchmark_result(&row).expect_err("no series, no evidence");
    assert!(error.to_string().contains("dataset_id"), "{error}");
}

#[test]
fn an_aggregate_history_row_from_a_non_operational_mode_is_refused() {
    let mut row: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 7, 100)).expect("json");
    row["mode"] = serde_json::json!("resource_soak");
    let row = BenchmarkResult::from_json(row.to_string().as_bytes()).expect("row parses");
    let error = SloReport::from_benchmark_result(&row).expect_err("outside the operational series");
    assert!(
        error
            .to_string()
            .contains("cannot feed SLO baseline selection"),
        "{error}"
    );
}

/// A full `slo-report` evaluation whose session first-audio percentiles are
/// exactly what `aggregate_row(_, _, 7, p95)` records — three samples ending
/// at `latencies[2]` nearest-rank to p95/p99 — so the report and that row's
/// shell are two views of one measured run (the overlap review of PR #225
/// requires pooling).
fn full_report_for(run_id: &str, latencies: [u64; 3]) -> SloReport {
    let events = latencies
        .iter()
        .enumerate()
        .map(|(index, latency)| {
            let mut event = EventObservation::new(
                format!("evt-{index}"),
                ComparisonMode::DeterministicOnly,
                RouteClass::Deterministic,
            );
            event.routing_latency_us = 10;
            event.cache_lookup = true;
            event.cache_hit = true;
            event.cache_level = Some(CacheLevel::Memory);
            event.stream_offset_ms = Some((index as u64 + 1) * 1_000);
            event.event_to_first_audio_ms = Some(*latency);
            event
        })
        .collect();
    let report = BenchmarkReport::from_events(
        metadata("slo-fixture", 180_000),
        ComparisonMode::DeterministicOnly,
        events,
    )
    .expect("benchmark report");
    evaluate(
        &report,
        &SloTargets::default(),
        SloEvaluationConfig {
            run_id: Some(run_id.to_owned()),
            ..SloEvaluationConfig::default()
        },
    )
    .expect("SLO report")
}

#[test]
fn a_full_report_and_its_aggregate_shell_are_one_run_in_either_order() {
    // run-a exists as both artifacts — the #224 migration overlap — and must
    // pool as one run; run-b is an old aggregate-only run whose percentile
    // evidence must keep contributing alongside it.
    let full = full_report_for("run-a", [60, 80, 100]);
    let shell = aggregate_shell("run-a", "deadbeef", 7, 100);
    let old = aggregate_shell("run-b", "cccc3333", 7, 200);

    for (label, reports) in [
        (
            "shell first",
            vec![shell.clone(), old.clone(), full.clone()],
        ),
        ("full first", vec![full.clone(), shell.clone(), old.clone()]),
    ] {
        let pooled = BaselineProposalSet::from_reports(&reports)
            .unwrap_or_else(|error| panic!("{label}: the mixed history must pool: {error}"));
        assert_eq!(pooled.contributing_reports, 2, "{label}");
        assert_eq!(
            pooled
                .contributing_runs
                .iter()
                .filter(|run| run.run_id == "run-a")
                .count(),
            1,
            "{label}: two artifacts of one run are one run"
        );
        let latency = pooled
            .proposals
            .get("availability.event_to_first_audio_within_target")
            .expect("latency proposal")
            .latency
            .clone()
            .expect("target-free percentiles");
        assert_eq!(latency.runs, 2, "{label}");
        assert_eq!(latency.samples, 6, "{label}");
        assert_eq!(
            latency.p95_ms, 200,
            "{label}: the aggregate-only run still contributes its percentiles"
        );
        assert_eq!(
            latency.max_ms,
            Some(100),
            "{label}: the full report's per-run maximum survives the collapse"
        );
        assert!(
            pooled
                .limitations
                .iter()
                .any(|line| line.contains("aggregate-history shell")),
            "{label}: the collapse is declared: {}",
            pooled.limitations.join("; ")
        );
    }
}

#[test]
fn a_full_report_and_shell_that_disagree_are_refused_in_either_order() {
    let full = full_report_for("run-a", [60, 80, 200]);
    let shell = aggregate_shell("run-a", "deadbeef", 7, 100);
    let independent = aggregate_shell("run-b", "cccc3333", 7, 200);

    let forward =
        BaselineProposalSet::from_reports(&[full.clone(), shell.clone(), independent.clone()])
            .expect_err("a pair that disagrees on percentiles is not one measurement");
    let reversed = BaselineProposalSet::from_reports(&[independent, shell, full])
        .expect_err("the same disagreement must fail when reversed");
    assert_eq!(
        forward.to_string(),
        reversed.to_string(),
        "argument order must not change the outcome"
    );
    assert!(forward.to_string().contains("disagree"), "{forward}");
}

#[test]
fn a_full_report_and_shell_that_disagree_on_duration_are_refused_in_either_order() {
    // Represented duration is a measurement of the run, not cosmetic
    // metadata: a pair contradicting it is two attempts of one identity, not
    // two views of one measurement (re-review of PR #225).
    let full = full_report_for("run-a", [60, 80, 100]);
    let mut shell = aggregate_shell("run-a", "deadbeef", 7, 100);
    shell.source.stream_duration_ms = Some(3_600_000);
    let independent = aggregate_shell("run-b", "cccc3333", 7, 200);

    let forward =
        BaselineProposalSet::from_reports(&[full.clone(), shell.clone(), independent.clone()])
            .expect_err("a duration mismatch is not one measurement");
    let reversed = BaselineProposalSet::from_reports(&[independent, shell, full])
        .expect_err("the same mismatch must fail when reversed");
    assert_eq!(
        forward.to_string(),
        reversed.to_string(),
        "argument order must not change the outcome"
    );
    assert!(
        forward.to_string().contains("stream_duration_ms"),
        "{forward}"
    );
}

#[test]
fn an_aggregate_history_row_from_another_benchmark_suite_is_refused() {
    // Two otherwise-identical rows differing only in benchmark_suite: one is
    // calibration evidence, and the other must not silently pool as the same
    // suite's measurement (review of PR #225).
    let mut other: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 7, 100)).expect("json");
    other["benchmark_suite"] = serde_json::json!("resource-soak");
    let other = BenchmarkResult::from_json(other.to_string().as_bytes()).expect("row parses");
    let error = SloReport::from_benchmark_result(&other).expect_err("one suite per importer");
    assert!(error.to_string().contains("benchmark_suite"), "{error}");
    assert!(error.to_string().contains("replay-comparison"), "{error}");

    let identical =
        BenchmarkResult::from_json(aggregate_row("run-a", "aaaa1111", 7, 100).as_bytes())
            .expect("row parses");
    SloReport::from_benchmark_result(&identical)
        .expect("the identical replay-comparison row still converts");
}

/// One `aggregate_row` mutated before parsing, so each malformed-evidence
/// rule is refused on the row shape it exists to refuse.
fn malformed_row(rotate: impl FnOnce(&mut serde_json::Value)) -> BenchmarkResult {
    let mut row: serde_json::Value =
        serde_json::from_str(&aggregate_row("run-a", "aaaa1111", 7, 100)).expect("json");
    rotate(&mut row);
    BenchmarkResult::from_json(row.to_string().as_bytes()).expect("row parses")
}

#[test]
fn malformed_first_audio_percentiles_are_refused_before_calibration() {
    let refused = |row: BenchmarkResult, expected: &str| {
        let error =
            SloReport::from_benchmark_result(&row).expect_err("malformed evidence is refused");
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in: {error}"
        );
    };

    // A fractional latency must not be rounded into an integer.
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p95_ms"]["value"] = serde_json::json!(100.5)
        }),
        "integral",
    );
    // A percentile with no samples behind it is not calibration evidence.
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p95_ms"]["sample_count"] = serde_json::json!(0)
        }),
        "positive sample count",
    );
    // One run's percentiles describe one sample set.
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p50_ms"]["sample_count"] = serde_json::json!(2)
        }),
        "disagree on sample_count",
    );
    // An unordered triplet must not be silently repaired by sorting.
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p99_ms"]["value"] = serde_json::json!(50)
        }),
        "not ordered",
    );
    // A half-present triplet is not half-dropped either.
    refused(
        malformed_row(|row| {
            row["metrics"]
                .as_object_mut()
                .expect("metrics object")
                .remove("cached.first_audio.p99_ms");
        }),
        "of the three",
    );
    // Values at/above 2^53 are refused outright: a raw JSON integer there
    // can already have been rounded during parsing, and the f64->u64 cast
    // boundary (2^64) sits inside that range (re-reviews of PR #225).
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p95_ms"]["value"] =
                serde_json::json!(18_446_744_073_709_551_616.0_f64);
        }),
        "2^53",
    );
    // The first unsafe *raw JSON integer*: 2^53+1 rounds to 2^53 during
    // deserialization, so the parsed f64 no longer equals the integer the
    // source wrote — refused, never recorded as 9007199254740992.
    refused(
        malformed_row(|row| {
            row["metrics"]["cached.first_audio.p95_ms"]["value"] =
                serde_json::json!(9_007_199_254_740_993_u64);
        }),
        "2^53",
    );
}

#[test]
fn an_identical_aggregate_history_row_is_not_a_second_measurement() {
    let row = BenchmarkResult::from_json(aggregate_row("run-a", "aaaa1111", 7, 100).as_bytes())
        .expect("row parses");
    let shell = SloReport::from_benchmark_result(&row).expect("shell");
    let pooled = BaselineProposalSet::from_reports(&[
        shell.clone(),
        shell,
        aggregate_shell("run-b", "bbbb2222", 7, 100),
    ])
    .expect("proposal set");
    assert_eq!(pooled.contributing_reports, 2);
    assert_eq!(pooled.duplicate_reports, 1);
    let proposal = pooled
        .proposals
        .get("availability.event_to_first_audio_within_target")
        .expect("latency proposal");
    assert_eq!(
        proposal.latency.as_ref().expect("latency").runs,
        2,
        "a double-passed row is not a second measurement"
    );
}
