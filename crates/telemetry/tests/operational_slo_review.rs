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
    EvidenceSource, IndicatorUnit, ReproducibilityMetadata, RouteClass, STREAM_HOUR_MS,
    SloEvaluationConfig, SloIndicatorResult, SloReport, SloStatus, SloTarget, SloTargets,
    SloVerdict, WindowKind, attribution_of, catalog, evaluate,
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
