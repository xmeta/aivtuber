//! Operational SLO evaluation contract (issue #71).
//!
//! These tests pin the three properties the module claims rather than its
//! arithmetic: absence of evidence is never a pass, a zero-tolerance invariant
//! never acquires an error budget, and an experimental plane never moves the
//! active user-facing numbers.

use aivtuber_domain::InteractionDeadlineClass;
use aivtuber_telemetry::{
    AttributionSummary, BaselineEvidence, BaselineProposalSet, BaselineRun, BenchmarkReport,
    CacheLevel, ComparisonMode, EventObservation, EvidenceSource, FailureOrigin, MIN_BASELINE_RUNS,
    MissAttribution, MissAttribution as Attribution, ObjectiveKind, ReproducibilityMetadata,
    RouteClass, STREAM_HOUR_MS, SloAction, SloError, SloEvaluationConfig, SloIndicatorResult,
    SloProvenance, SloReport, SloStatus, SloTarget, SloTargets, SloWindow, TrafficPlane,
    WindowKind, attribution_of, catalog, evaluate,
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
            "stream_hours": 1
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
    assert!(latency.p95_ms > 0 && latency.max_ms >= latency.p99_ms);
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
