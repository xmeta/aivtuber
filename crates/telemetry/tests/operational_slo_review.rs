//! Regressions for the review findings on PR #218.
//!
//! Each test names the finding it pins. The point of this file is not coverage
//! for its own sake: findings 2, 3 and 4 were all cases where two pieces of the
//! report could disagree with each other — a numerator that described the wrong
//! direction, a ratio and a miss set computed from different predicates, and a
//! streak counted across window kinds that were never the same series.

use aivtuber_domain::InteractionDeadlineClass;
use aivtuber_telemetry::{
    AttributionSummary, BaselineEvidence, BenchmarkReport, ComparisonMode, EventObservation,
    EventVerdict, EvidenceSource, IndicatorUnit, ObjectiveKind, ReproducibilityMetadata,
    RouteClass, STREAM_HOUR_MS, SloEvaluationConfig, SloIndicatorResult, SloReport, SloStatus,
    SloTarget, SloTargets, SloVerdict, WindowKind, attribution_of, catalog, evaluate,
    overall_verdict,
};

fn observation(route: RouteClass) -> EventObservation {
    EventObservation::new("evt", ComparisonMode::DeterministicSemantic, route)
}

fn report(events: Vec<EventObservation>, stream_duration_ms: u64) -> BenchmarkReport {
    BenchmarkReport::from_events(
        ReproducibilityMetadata {
            dataset_id: "slo-review-fixture".to_owned(),
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
        },
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

fn baseline() -> BaselineEvidence {
    BaselineEvidence {
        value: 0.0,
        source: "slo-review-fixture (dataset=slo-review-fixture, commit=deadbeef)".to_owned(),
        stream_hours: Some(1),
        series: None,
        runs: Vec::new(),
    }
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

fn calibrated(target: f64) -> SloTarget {
    SloTarget {
        target,
        threshold_ms: None,
        baseline: baseline(),
    }
}

fn latency_target(target: f64, threshold_ms: u64) -> SloTarget {
    SloTarget {
        threshold_ms: Some(threshold_ms),
        ..calibrated(target)
    }
}

fn timed(index: u64, audio_ms: u64) -> EventObservation {
    let mut event = observation(RouteClass::Deterministic);
    event.stream_offset_ms = Some(index * 1_000);
    event.event_to_first_audio_ms = Some(audio_ms);
    event
}

// ---------------------------------------------------------------- finding 2

/// The published `numerator` text must describe the conforming subset, because
/// `SloTarget::target` is a *minimum* conforming ratio. An indicator described
/// as a failure rate whose value is a success ratio makes any configured target
/// ambiguous, which is exactly what `slo-catalog-v1` must not ship.
#[test]
fn finding_2_every_measured_indicator_publishes_a_conforming_numerator() {
    for indicator in catalog() {
        if indicator.evidence != EvidenceSource::Measured {
            continue;
        }
        let numerator = indicator.numerator.to_lowercase();
        for failure_phrasing in [
            "rate of",
            "count of",
            "were exhausted",
            "denied admission",
            "produced no user-visible output",
        ] {
            assert!(
                !numerator.contains(failure_phrasing),
                "{}: numerator describes failures ({failure_phrasing:?}) but the value is a \\
                 conforming ratio: {}",
                indicator.id,
                indicator.numerator
            );
        }
        assert!(
            numerator.contains("those"),
            "{}: numerator must be phrased as the conforming subset: {}",
            indicator.id,
            indicator.numerator
        );
        assert_eq!(
            indicator.unit,
            IndicatorUnit::Ratio,
            "{}: every measured objective is a minimum conforming ratio in slo-catalog-v1",
            indicator.id
        );
        assert!(
            !indicator.id.ends_with("_rate") || !indicator.numerator.contains("within"),
            "{}: latency objectives are named `within_target` so the threshold direction is \\
             unambiguous",
            indicator.id
        );
    }
}

/// The value must move the same way the numerator does: silencing more events
/// must lower the speech-presence ratio, not raise an "unintended silence" one.
#[test]
fn finding_2_a_worse_run_reports_a_lower_conforming_ratio() {
    let healthy: Vec<EventObservation> = (0..20_u64).map(|index| timed(index, 10)).collect();
    let degraded: Vec<EventObservation> = (0..20_u64)
        .map(|index| {
            if index >= 15 {
                let mut event = timed(index, 10);
                event.route = RouteClass::Silent;
                event.event_to_first_audio_ms = None;
                event
            } else {
                timed(index, 10)
            }
        })
        .collect();

    let file = targets(&[("availability.speech_presence_rate", calibrated(0.99))]);
    let healthy = evaluate(
        &report(healthy, 20_000),
        &file,
        SloEvaluationConfig::default(),
    )
    .expect("report");
    let degraded = evaluate(
        &report(degraded, 20_000),
        &file,
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let healthy_value = result(&healthy, "availability.speech_presence_rate", "session")
        .value
        .expect("healthy ratio");
    let degraded_value = result(&degraded, "availability.speech_presence_rate", "session")
        .value
        .expect("degraded ratio");
    assert_eq!(healthy_value, 1.0);
    assert!(
        degraded_value < healthy_value,
        "silencing 5 of 20 events must lower the ratio, got {degraded_value} vs {healthy_value}"
    );
}

/// The same direction check for the two other renamed objectives.
#[test]
fn finding_2_deadline_and_budget_objectives_move_the_same_way() {
    // Enough events to clear both indicators' sample floor of 15, with exactly
    // one budget denial.
    let mut events: Vec<EventObservation> = (0..19_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Generated);
            event.stream_offset_ms = Some(index * 1_000);
            event.deadline_class = Some(InteractionDeadlineClass::Conversation);
            event
        })
        .collect();
    let mut denied = observation(RouteClass::Generated);
    denied.stream_offset_ms = Some(19_000);
    denied.deadline_class = Some(InteractionDeadlineClass::Conversation);
    denied.budget_denial_reason = Some("budget_exhausted".to_owned());
    events.push(denied);

    let evaluated = evaluate(
        &report(events, 20_000),
        &targets(&[
            ("reliability.deadline_adherence_rate", calibrated(1.0)),
            (
                "reliability.generative_budget_admission_rate",
                calibrated(1.0),
            ),
        ]),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    // The budget denial did not exhaust a deadline, so it is not a miss of the
    // deadline objective; it is one less admitted event for the budget one.
    assert_eq!(
        result(&evaluated, "reliability.deadline_adherence_rate", "session").value,
        Some(1.0)
    );
    let budget = result(
        &evaluated,
        "reliability.generative_budget_admission_rate",
        "session",
    );
    assert_eq!(budget.value, Some(0.95));
    assert_eq!(budget.eligible, 20);
    assert_eq!(budget.conforming, 19);
    assert_eq!(budget.status, SloStatus::Missed);
}

// ---------------------------------------------------------------- finding 3

/// A met calibrated objective must not carry the whole report to a passing
/// aggregate while a zero-tolerance invariant is still unmeasured.
#[test]
fn finding_3_a_met_objective_cannot_hide_unmeasured_invariant_evidence() {
    let events: Vec<EventObservation> = (0..20_u64).map(|index| timed(index, index)).collect();
    let evaluated = evaluate(
        &report(events, 20_000),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            latency_target(0.95, 100),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.status, SloStatus::Met);
    assert_eq!(
        audio.miss_attribution.unattributed, 0,
        "a met objective has no misses to attribute"
    );

    assert_eq!(
        result(&evaluated, "safety.stale_dispatch_count", "session").status,
        SloStatus::NotYetMeasured
    );
    assert_eq!(
        evaluated.verdict,
        SloVerdict::Incomplete,
        "one met objective must not make the report pass while required evidence is unresolved"
    );
    assert!(evaluated.markdown_summary().contains("INCOMPLETE"));
}

/// The non-passing states must not be reachable as `Ok`, and a breach must still
/// outrank an incomplete report.
#[test]
fn finding_3_breach_outranks_incomplete_and_ok_needs_full_evidence() {
    let healthy: Vec<EventObservation> = (0..20_u64).map(|index| timed(index, 5)).collect();
    let file = targets(&[(
        "availability.event_to_first_audio_within_target",
        latency_target(0.95, 100),
    )]);

    let meeting = evaluate(
        &report(healthy.clone(), 20_000),
        &file,
        SloEvaluationConfig::default(),
    )
    .expect("report");
    assert_eq!(meeting.verdict, SloVerdict::Incomplete);

    // Now push it into breach without touching the invariant evidence.
    let breaching: Vec<EventObservation> = (0..20_u64).map(|index| timed(index, 5_000)).collect();
    let breach = evaluate(
        &report(breaching, 20_000),
        &file,
        SloEvaluationConfig::default(),
    )
    .expect("report");
    assert_eq!(breach.verdict, SloVerdict::Breach);
}

// ---------------------------------------------------------------- finding 4

/// Two hourly misses plus the session aggregate is not a three-window streak.
#[test]
fn finding_4_a_two_hour_run_cannot_manufacture_a_three_window_streak() {
    let events: Vec<EventObservation> = (0..2_u64)
        .flat_map(|hour| {
            (0..20_u64).map(move |index| {
                let mut event = observation(RouteClass::Silent);
                event.stream_offset_ms = Some(hour * STREAM_HOUR_MS + index * 60_000);
                event
            })
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 2 * STREAM_HOUR_MS),
        &targets(&[("availability.speech_presence_rate", calibrated(0.99))]),
        SloEvaluationConfig {
            persistent_miss_windows: 3,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("report");

    assert_eq!(evaluated.verdict, SloVerdict::Breach);
    assert_eq!(
        evaluated
            .windows
            .iter()
            .filter(|window| window.kind == WindowKind::StreamHour)
            .count(),
        2,
        "the fixture must produce exactly two hourly windows"
    );
    assert!(
        evaluated.aar_candidates.is_empty(),
        "two hourly misses plus the session aggregate must not read as a three-window streak: \
         {:?}",
        evaluated.aar_candidates
    );
}

/// The fix narrows the grouping; it does not disable the escalation.
#[test]
fn finding_4_a_real_streak_within_one_series_still_reaches_the_review() {
    let events: Vec<EventObservation> = (0..4_u64)
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
        &targets(&[("availability.speech_presence_rate", calibrated(0.99))]),
        SloEvaluationConfig {
            persistent_miss_windows: 3,
            ..SloEvaluationConfig::default()
        },
    )
    .expect("report");

    let candidate = evaluated
        .aar_candidates
        .iter()
        .find(|candidate| candidate.indicator == "availability.speech_presence_rate")
        .expect("AAR candidate");
    assert_eq!(candidate.window_kind, WindowKind::StreamHour);
    assert!(candidate.consecutive_miss_windows >= 3);
    assert!(candidate.rationale.contains("stream_hour"));
    assert!(
        !candidate.rationale.contains("rolling"),
        "the hourly streak must not be described with rolling-window wording"
    );
}

// ---------------------------------------------------------------- finding 5

/// An event outside an indicator's denominator is not a miss of that indicator,
/// so an unrelated provider failure cannot appear in its miss attribution.
#[test]
fn finding_5_an_out_of_denominator_failure_is_not_a_miss_of_that_indicator() {
    let mut eligible: Vec<EventObservation> = (0..19_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            event.deadline_class = Some(InteractionDeadlineClass::Conversation);
            event
        })
        .collect();
    assert_eq!(eligible.len(), 19);

    let mut unrelated = observation(RouteClass::Deterministic);
    unrelated.stream_offset_ms = Some(19_000);
    unrelated.fallback_reason = Some("unavailable".to_owned());
    unrelated.event_to_first_audio_ms = Some(10);
    assert!(
        unrelated.deadline_class.is_none(),
        "the fixture must rely on this event being outside the deadline denominator"
    );
    eligible.push(unrelated);

    let evaluated = evaluate(
        &report(eligible, 20_000),
        &targets(&[("reliability.deadline_adherence_rate", calibrated(1.0))]),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let deadline = result(&evaluated, "reliability.deadline_adherence_rate", "session");
    assert_eq!(deadline.status, SloStatus::Met);
    assert_eq!(
        deadline.eligible, 19,
        "only the deadline-bearing events are eligible"
    );
    assert_eq!(
        deadline.miss_attribution.external_provider_failures, 0,
        "an event outside the denominator must not be attributed to this objective"
    );
    assert_eq!(deadline.miss_attribution.runtime_handling_failures, 0);

    // It is still attributed at the run level, which is where it belongs.
    assert_eq!(evaluated.failure_attribution.external_provider_failures, 1);
    assert_eq!(evaluated.failure_attribution.runtime_handled, 1);
}

/// An unrecognized declared reason is counted as unattributed instead of being
/// dropped by the tally.
#[test]
fn finding_5_an_unknown_declared_reason_is_counted_as_unattributed() {
    let mut unknown = observation(RouteClass::Silent);
    unknown.fallback_reason = Some("provider_exploded".to_owned());
    let mut policy = observation(RouteClass::Silent);
    policy.fallback_reason = Some("budget_exhausted".to_owned());

    let summary = AttributionSummary::of([&unknown, &policy]);
    assert_eq!(
        summary.unattributed, 1,
        "an unknown declared reason must be counted, not discarded"
    );
    assert_eq!(summary.runtime_policy_decisions, 1);
    assert_eq!(summary.external_provider_failures, 0);
}

/// The tally is complete: every event with a declared reason lands in exactly
/// one bucket.
#[test]
fn finding_5_every_declared_reason_lands_in_exactly_one_bucket() {
    let reasons = [
        ("timeout", true),
        ("unavailable", true),
        ("rate_limited", true),
        ("overloaded", true),
        ("authentication", true),
        ("invalid_request", false),
        ("low_confidence", false),
        ("policy_override", false),
        ("operator_override", false),
        ("budget_exhausted", false),
        ("something_new", false),
    ];
    for (reason, external) in reasons {
        let mut event = observation(RouteClass::CachedFallback);
        event.fallback_reason = Some(reason.to_owned());
        event.event_to_first_audio_ms = Some(10);
        let summary = AttributionSummary::of([&event]);
        let total = summary.external_provider_failures
            + summary.runtime_policy_decisions
            + summary.unattributed;
        assert_eq!(total, 1, "{reason} must be counted exactly once");
        if external {
            assert_eq!(summary.external_provider_failures, 1, "{reason}");
        } else if reason == "something_new" {
            assert_eq!(summary.unattributed, 1, "{reason}");
        } else {
            assert_eq!(summary.runtime_policy_decisions, 1, "{reason}");
        }
        assert_eq!(
            attribution_of(&event),
            if external {
                aivtuber_telemetry::MissAttribution::ExternalProvider
            } else {
                aivtuber_telemetry::MissAttribution::Unattributed
            },
            "{reason}"
        );
    }
}

// ---------------------------------------------------------------- finding 6

/// A latency sample with no calibrated `threshold_ms` is eligible but not
/// scoreable, so it must not enter the miss set — even when it carries a
/// declared external-provider reason, which is exactly the case that used to
/// report `Uncalibrated` in one field and `external_provider_failures: 1` in
/// the next.
#[test]
fn finding_6_an_uncalibrated_latency_sample_is_not_an_slo_miss() {
    let mut event = observation(RouteClass::CachedFallback);
    event.stream_offset_ms = Some(0);
    event.fallback_reason = Some("unavailable".to_owned());
    event.event_to_first_audio_ms = Some(50);

    let evaluated = evaluate(
        &report(vec![event], 1_000),
        &SloTargets::default(),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.status, SloStatus::Uncalibrated);
    assert_eq!(audio.value, None, "an unscored indicator has no ratio");
    assert_eq!(
        (audio.conforming, audio.eligible, audio.unscored),
        (0, 1, 1),
        "the sample is calibration evidence, not a conformance and not a miss"
    );
    assert_eq!(
        audio.miss_attribution.external_provider_failures, 0,
        "an uncalibrated objective has no defined miss to attribute"
    );
    assert_eq!(audio.miss_attribution.runtime_handled, 0);
    assert_eq!(audio.miss_attribution.runtime_handling_failures, 0);
    assert_eq!(audio.miss_attribution.unattributed, 0);
    assert!(
        evaluated
            .error_budgets
            .iter()
            .all(|budget| budget.indicator != audio.id),
        "an undecided objective must not carry a budget"
    );
}

/// The same fixture, calibrated. Once a threshold exists the samples are scored
/// and the miss is real, so the fix must not have disarmed attribution. The
/// window carries enough samples to clear the indicator's sample floor, because
/// an under-sampled window is legitimately undecided and would hide the point.
#[test]
fn finding_6_a_calibrated_threshold_turns_the_same_samples_into_real_misses() {
    let events: Vec<EventObservation> = (0..20_u64)
        .map(|index| {
            let mut event = observation(RouteClass::CachedFallback);
            event.stream_offset_ms = Some(index * 1_000);
            event.fallback_reason = Some("unavailable".to_owned());
            event.event_to_first_audio_ms = Some(50);
            event
        })
        .collect();

    let evaluated = evaluate(
        &report(events, 20_000),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            latency_target(0.95, 10),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.status, SloStatus::Missed);
    assert_eq!(
        (audio.conforming, audio.eligible, audio.unscored),
        (0, 20, 0)
    );
    assert_eq!(audio.miss_attribution.external_provider_failures, 20);
    assert_eq!(audio.miss_attribution.runtime_handled, 20);
    let budget = evaluated
        .error_budgets
        .iter()
        .find(|budget| budget.indicator == audio.id && budget.window == "session")
        .expect("error budget");
    assert_eq!(
        budget.observed_misses, 20,
        "unscored samples are never spent"
    );
    assert_eq!(
        evaluated.verdict,
        SloVerdict::Breach,
        "a scored miss is a breach, which is the state the fix must not erase"
    );
}

/// `misses()` is what the error budget spends, so it has to exclude unscored
/// samples too — otherwise the indicator and the budget disagree about the same
/// window.
#[test]
fn finding_6_unscored_samples_are_never_charged_as_a_spend() {
    // Half the window is measured latency, half carries no latency sample at
    // all. The unscored half must be visible in the denominator and absent
    // from the miss count and the budget.
    let events: Vec<EventObservation> = (0..20_u64)
        .map(|index| {
            let mut event = observation(RouteClass::Deterministic);
            event.stream_offset_ms = Some(index * 1_000);
            if index % 2 == 0 {
                event.event_to_first_audio_ms = Some(index + 1);
            }
            event
        })
        .collect();
    let evaluated = evaluate(
        &report(events, 20_000),
        &SloTargets::default(),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.eligible, 10, "only measured samples are eligible");
    assert_eq!(audio.unscored, 10);
    assert!(
        audio.miss_attribution.external_provider_failures == 0,
        "no window budget is published for an uncalibrated objective"
    );
}

/// Every classifier's four states have to stay distinct. A regression here
/// means a future indicator quietly collapsed "unscoreable" into "miss" again.
#[test]
fn finding_6_the_four_event_verdict_states_are_distinct() {
    assert!(!EventVerdict::Outside.is_eligible());
    assert!(!EventVerdict::Conforms.is_miss());
    assert!(EventVerdict::Misses.is_eligible() && EventVerdict::Misses.is_miss());

    let unscored = EventVerdict::Unscored;
    assert!(
        unscored.is_eligible(),
        "an uncalibrated sample is still denominator evidence"
    );
    assert!(
        !unscored.is_miss(),
        "an uncalibrated sample must never enter the miss set"
    );
}

// ------------------------------------------------------- contract hardening

/// Baseline evidence is the only justification a target ever has, so it is held
/// to the same standard as the target itself.
#[test]
fn hardening_an_impossible_baseline_is_refused() {
    for (value, expected) in [(2.0, "baseline value"), (f64::NAN, "baseline value")] {
        let file = SloTargets {
            targets: [(
                "availability.speech_presence_rate".to_owned(),
                SloTarget {
                    target: 0.99,
                    threshold_ms: None,
                    baseline: BaselineEvidence {
                        value,
                        source: "hardening-fixture".to_owned(),
                        stream_hours: Some(1),
                        series: None,
                        runs: Vec::new(),
                    },
                },
            )]
            .into_iter()
            .collect(),
            ..SloTargets::default()
        };
        let error = file.validate(&catalog()).expect_err("baseline ratio");
        assert!(
            error.to_string().contains(expected),
            "value {value}: {error}"
        );
    }
}

#[test]
fn hardening_a_baseline_covering_no_time_is_refused() {
    let file = SloTargets {
        targets: [(
            "availability.speech_presence_rate".to_owned(),
            SloTarget {
                target: 0.99,
                threshold_ms: None,
                baseline: BaselineEvidence {
                    value: 0.98,
                    source: "hardening-fixture".to_owned(),
                    stream_hours: Some(0),
                    series: None,
                    runs: Vec::new(),
                },
            },
        )]
        .into_iter()
        .collect(),
        ..SloTargets::default()
    };
    let error = file.validate(&catalog()).expect_err("no stream hours");
    assert!(error.to_string().contains("stream hours"), "{error}");
}

#[test]
fn hardening_a_target_file_from_another_report_contract_is_refused() {
    let file = SloTargets {
        schema_version: Some("99".to_owned()),
        ..SloTargets::default()
    };
    let error = file
        .validate(&catalog())
        .expect_err("unknown report schema");
    assert!(error.to_string().contains("report schema"), "{error}");
}

/// An absent `schema_version` is an unversioned draft and stays accepted, so
/// the hardening does not break the checked-in empty target file.
#[test]
fn hardening_an_unversioned_target_file_is_still_accepted() {
    let file = SloTargets {
        schema_version: None,
        ..SloTargets::default()
    };
    file.validate(&catalog()).expect("unversioned draft");
}

// ---------------------------------------------------------------- round 3

/// Build one indicator result so the aggregate rule can be exercised without
/// the catalog's `not_yet_measured` rows masking it.
fn row(id: &str, status: SloStatus) -> SloIndicatorResult {
    SloIndicatorResult {
        id: id.to_owned(),
        kind: ObjectiveKind::Slo,
        unit: IndicatorUnit::Ratio,
        window: "session".to_owned(),
        window_kind: WindowKind::Session,
        denominator: "events".to_owned(),
        numerator: "those inside the objective".to_owned(),
        conforming: 0,
        eligible: 0,
        unscored: 0,
        value: None,
        target: None,
        threshold_ms: None,
        sample_floor: 0,
        status,
        miss_attribution: AttributionSummary::default(),
        actions: Vec::new(),
        missing_evidence: None,
    }
}

/// `InsufficientSamples` is an undecided objective, so it must make the
/// aggregate `Incomplete` rather than being quietly treated as "nothing
/// outstanding". It is listed here explicitly because it is the status most
/// likely to be forgotten when a new one is added.
#[test]
fn round_3_every_undecided_status_keeps_the_aggregate_from_passing() {
    for status in [
        SloStatus::Uncalibrated,
        SloStatus::NotYetMeasured,
        SloStatus::InsufficientSamples,
    ] {
        assert!(status.is_unresolved(), "{status:?} must be unresolved");
        let verdicts = overall_verdict(&[
            row("availability.speech_presence_rate", SloStatus::Met),
            row("reliability.deadline_adherence_rate", status),
        ]);
        assert_eq!(
            verdicts,
            SloVerdict::Incomplete,
            "a met objective plus {status:?} must not read as a pass"
        );
    }

    // The decided and the legitimately-empty statuses are not outstanding.
    for status in [SloStatus::Met, SloStatus::Missed, SloStatus::NoData] {
        assert!(!status.is_unresolved(), "{status:?} must not be unresolved");
    }
    assert_eq!(
        overall_verdict(&[
            row("availability.speech_presence_rate", SloStatus::Met),
            row("reliability.fallback_delivery_rate", SloStatus::NoData),
        ]),
        SloVerdict::Ok
    );
}

/// Precedence is unchanged: a real breach still outranks everything.
#[test]
fn round_3_a_breach_still_outranks_incomplete_evidence() {
    assert_eq!(
        overall_verdict(&[
            row("availability.speech_presence_rate", SloStatus::Missed),
            row(
                "reliability.deadline_adherence_rate",
                SloStatus::NotYetMeasured
            ),
        ]),
        SloVerdict::Breach
    );
    assert_eq!(
        overall_verdict(&[row("availability.speech_presence_rate", SloStatus::NoData)]),
        SloVerdict::Uncalibrated,
        "a report with no data at all is not a pass either"
    );
}

/// The same rule through the real evaluator: an under-sampled calibrated
/// objective is not decided, is not given a budget, and cannot raise the
/// aggregate above `Incomplete`.
#[test]
fn round_3_an_under_sampled_objective_is_not_decided_and_spends_nothing() {
    let events: Vec<EventObservation> = (0..4_u64).map(|index| timed(index, 5)).collect();
    let evaluated = evaluate(
        &report(events, 4_000),
        &targets(&[(
            "availability.event_to_first_audio_within_target",
            latency_target(0.95, 100),
        )]),
        SloEvaluationConfig::default(),
    )
    .expect("report");

    let audio = result(
        &evaluated,
        "availability.event_to_first_audio_within_target",
        "session",
    );
    assert_eq!(audio.status, SloStatus::InsufficientSamples);
    assert!(audio.status.is_unresolved());
    // The observed ratio is still reported — it is the evidence a reviewer
    // needs to judge the sample floor — but it must not decide the objective.
    assert_eq!(audio.value, Some(1.0));
    assert!(!audio.status.is_decided());
    assert!(
        evaluated
            .error_budgets
            .iter()
            .all(|budget| budget.indicator != audio.id),
        "an undecided objective must not carry a budget"
    );
    assert_eq!(evaluated.verdict, SloVerdict::Incomplete);
}
