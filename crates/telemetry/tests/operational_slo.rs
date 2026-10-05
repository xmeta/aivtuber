//! Operational SLO evaluation contract (issue #71).
//!
//! These tests pin the three properties the module claims rather than its
//! arithmetic: absence of evidence is never a pass, a zero-tolerance invariant
//! never acquires an error budget, and an experimental plane never moves the
//! active user-facing numbers.

use aivtuber_domain::InteractionDeadlineClass;
use aivtuber_telemetry::{
    AttributionSummary, BaselineEvidence, BenchmarkReport, CacheLevel, ComparisonMode,
    EventObservation, EvidenceSource, FailureOrigin, MissAttribution,
    MissAttribution as Attribution, ObjectiveKind, ReproducibilityMetadata, RouteClass,
    STREAM_HOUR_MS, SloAction, SloError, SloEvaluationConfig, SloIndicatorResult, SloProvenance,
    SloReport, SloStatus, SloTarget, SloTargets, SloWindow, TrafficPlane, WindowKind,
    attribution_of, catalog, evaluate,
};

const CALIBRATION_SOURCE: &str = "04-full-generative.json (dataset=slo-fixture, commit=deadbeef)";

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

    // The target-free percentiles are the evidence a threshold is chosen from,
    // so a reviewer never has to invent the number.
    let calibration = evaluated
        .latency_calibration
        .iter()
        .find(|point| point.window == "session" && point.field == "event_to_first_audio_ms")
        .expect("audio calibration point");
    assert_eq!(calibration.samples, 40);
    assert_eq!(calibration.p50_ms, 21);
    assert_eq!(calibration.max_ms, 41);
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
