//! The section-12 sidecar comparison rule, applied to real reports (#105).
//!
//! The runtime side of every case here is the report the published fixture
//! actually produces, not a hand-written one: the rule is judged against
//! measured numbers. The candidate side is that same report edited into the
//! shape a sidecar would have to produce, so each test states one claim about
//! the rule: what counts as an improvement, what counts as a regression, and
//! what must be refused as not comparable at all.

use aivtuber_app::{
    CandidateVerdict, EvalReport, MemoryCompareError, MemoryEvalFixture, compare, evaluate,
};

fn published_fixture() -> MemoryEvalFixture {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/memory-long-range/cases.json");
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// The baseline: what this runtime actually measures on the published fixture.
fn runtime_report() -> EvalReport {
    let outcome = evaluate(&published_fixture()).expect("the fixture evaluates");
    assert!(
        outcome.violations.is_empty(),
        "baseline must measure clean:\n{}",
        outcome.violations.join("\n")
    );
    outcome.report
}

/// Re-derive every aggregate from the case rows, exactly as the runner does.
///
/// A candidate that changes what it delivered must also change what it
/// declares — that is the point: the comparator checks one against the other,
/// so a test that only edits a case row would be caught by the validator
/// instead of exercising the rule.
fn renormalize(report: &mut EvalReport) {
    let cases = report.cases.len();
    report.totals.cases = cases;
    report.totals.current_fact_correctness = report
        .cases
        .iter()
        .map(|case| case.current_fact_correctness)
        .sum::<f64>()
        / cases as f64;
    report.totals.stale_memory_use = report.cases.iter().map(|case| case.stale_memory_use).sum();
    report.totals.absent_violations = report.cases.iter().map(|case| case.absent_violations).sum();
    report.totals.temporal_order_errors = report
        .cases
        .iter()
        .map(|case| case.temporal_order_errors)
        .sum();
    report.totals.truncated_claims = report.cases.iter().map(|case| case.truncated_claims).sum();
    report.totals.retained_causal_spans = report
        .cases
        .iter()
        .map(|case| case.retained_causal_spans)
        .sum();
    report.totals.max_context_items = report
        .cases
        .iter()
        .map(|case| case.context_items)
        .max()
        .unwrap_or(0);
    report.totals.max_claim_bytes = report
        .cases
        .iter()
        .map(|case| case.claim_bytes)
        .max()
        .unwrap_or(0);
    report.totals.max_request_bytes = report
        .cases
        .iter()
        .map(|case| case.request_bytes)
        .max()
        .unwrap_or(0);
    report.totals.max_entries_high_water = report
        .cases
        .iter()
        .map(|case| case.entries_high_water)
        .max()
        .unwrap_or(0);
    report.totals.max_links_high_water = report
        .cases
        .iter()
        .map(|case| case.links_high_water)
        .max()
        .unwrap_or(0);
}

fn case_mut<'a>(report: &'a mut EvalReport, name: &str) -> &'a mut aivtuber_app::CaseReport {
    report
        .cases
        .iter_mut()
        .find(|case| case.case == name)
        .unwrap_or_else(|| panic!("case {name} is in the published fixture"))
}

/// A candidate that recovers the true chronology where the runtime ranks by
/// record time: the class a timeline-aware sidecar is supposed to win.
fn candidate_with_ordered_timeline() -> EvalReport {
    let mut candidate = runtime_report();
    let mut fixed = 0;
    for name in [
        "record_time_inverts_event_time",
        "topic_mismatch_ranks_lower",
    ] {
        let case = case_mut(&mut candidate, name);
        case.delivered.reverse();
        case.temporal_order_errors = 0;
        fixed += 1;
    }
    assert_eq!(fixed, 2, "the baseline has two temporal errors to fix");
    renormalize(&mut candidate);
    candidate
}

#[test]
fn the_runtime_compared_with_itself_is_not_an_improvement() {
    let runtime = runtime_report();
    let comparison = compare(&published_fixture(), &runtime, &runtime)
        .expect("a report is comparable with itself");
    assert_eq!(comparison.verdict, CandidateVerdict::NoImprovement);
    assert!(!comparison.is_regression());
    assert!(
        comparison
            .reasons
            .iter()
            .any(|reason| reason.contains("temporal_order_errors")),
        "the reason names the dimension nobody won: {:?}",
        comparison.reasons
    );
    assert_eq!(comparison.case_deltas.len(), runtime.cases.len());
}

#[test]
fn a_sidecar_that_recovers_true_temporal_order_is_an_improvement() {
    let runtime = runtime_report();
    let candidate = candidate_with_ordered_timeline();
    let comparison = compare(&published_fixture(), &runtime, &candidate).expect("comparable");
    assert_eq!(comparison.verdict, CandidateVerdict::Improvement);
    assert!(!comparison.is_regression());
    assert!(
        comparison
            .reasons
            .iter()
            .any(|reason| reason.contains("temporal_order_errors") && reason.contains("2 -> 0")),
        "the reason shows the measured win: {:?}",
        comparison.reasons
    );
    let delta = comparison
        .deltas
        .iter()
        .find(|delta| delta.metric == "temporal_order_errors")
        .expect("temporal delta");
    assert!(delta.candidate_better && !delta.candidate_worse);
}

#[test]
fn a_sidecar_that_leaks_a_superseded_claim_is_rejected() {
    let mut candidate = candidate_with_ordered_timeline();
    {
        let case = case_mut(&mut candidate, "preference_update");
        case.delivered.push("evt-dislike".to_owned());
        case.stale_memory_use += 1;
        case.absent_violations += 1;
    }
    renormalize(&mut candidate);

    let runtime = runtime_report();
    let comparison = compare(&published_fixture(), &runtime, &candidate).expect("comparable");
    assert_eq!(comparison.verdict, CandidateVerdict::Rejected);
    assert!(comparison.is_regression());
    assert!(
        comparison
            .reasons
            .iter()
            .any(|reason| reason.contains("stale_memory_use")
                && reason.contains("preference_update")),
        "the reason names the case and the metric: {:?}",
        comparison.reasons
    );
    // Winning a dimension does not rescue a leak: the candidate would have
    // been an improvement on paper while delivering replaced evidence.
    assert!(
        comparison
            .deltas
            .iter()
            .any(|delta| delta.metric == "temporal_order_errors" && delta.candidate_better),
        "the quality win is still visible beside the rejection"
    );
}

#[test]
fn a_sidecar_that_overstays_a_request_bound_is_rejected() {
    let mut candidate = candidate_with_ordered_timeline();
    case_mut(&mut candidate, "budget_pressure_drops_whole").within_byte_budget = false;

    let runtime = runtime_report();
    let comparison = compare(&published_fixture(), &runtime, &candidate).expect("comparable");
    assert_eq!(comparison.verdict, CandidateVerdict::Rejected);
    assert!(
        comparison
            .reasons
            .iter()
            .any(|reason| reason.contains("byte bound")),
        "{:?}",
        comparison.reasons
    );
}

#[test]
fn a_report_that_contradicts_its_own_records_is_incomparable() {
    let runtime = runtime_report();

    // Declared clean while delivering a replaced claim: caught by re-deriving
    // the count from the delivered list, not by trusting the number.
    let mut lying = candidate_with_ordered_timeline();
    {
        let case = case_mut(&mut lying, "preference_update");
        case.delivered.push("evt-dislike".to_owned());
    }
    renormalize(&mut lying);
    let error =
        compare(&published_fixture(), &runtime, &lying).expect_err("a lying report is refused");
    assert!(
        error.to_string().contains("stale_memory_use"),
        "the refusal names the mismatched metric: {error}"
    );

    // A total edited without editing the rows it claims to summarize.
    let mut tampered = runtime_report();
    tampered.totals.current_fact_correctness = 1.5;
    let error = compare(&published_fixture(), &runtime, &tampered)
        .expect_err("an impossible total is refused");
    match error {
        MemoryCompareError::Incomparable(message) => assert!(
            message.contains("totals disagree") || message.contains("correctness"),
            "the refusal explains the inconsistency: {message}"
        ),
    }
}

#[test]
fn a_report_about_a_different_run_is_incomparable() {
    let runtime = runtime_report();

    let mut fewer_cases = runtime_report();
    fewer_cases.cases.pop();
    renormalize(&mut fewer_cases);
    let error =
        compare(&published_fixture(), &runtime, &fewer_cases).expect_err("coverage must match");
    assert!(error.to_string().contains("covers"), "{error}");

    let mut renamed = runtime_report();
    renamed.cases[0].case = "another_suite".to_owned();
    let error =
        compare(&published_fixture(), &runtime, &renamed).expect_err("case identity must match");
    assert!(error.to_string().contains("order differs"), "{error}");

    let mut other_schema = runtime_report();
    other_schema.schema_version = "memory-long-range-eval/v2".to_owned();
    let error =
        compare(&published_fixture(), &runtime, &other_schema).expect_err("one schema or none");
    assert!(
        error.to_string().contains("schema_version"),
        "the refusal names the schema it refused: {error}"
    );

    // Two reports over different fixture documents ask different questions:
    // the case ids may look alike while meaning different claims.
    let mut other_fixture = runtime_report();
    other_fixture.fixture_schema_version = "memory-eval-v0".to_owned();
    let error =
        compare(&published_fixture(), &runtime, &other_fixture).expect_err("one fixture or none");
    assert!(
        error.to_string().contains("fixture_schema_version"),
        "the refusal names the fixture document it refused: {error}"
    );
}

/// The causal row comes from the fixture and the delivered sets, never from a
/// report's own number (#105 review round 13).
///
/// A candidate keeps no project edge, so its own retained-span count is 0 even
/// where it returned both members of the declared pair. The compared number
/// still shows the pair, because it is re-derived rather than read.
#[test]
fn the_causal_row_is_recomputed_from_the_fixture_not_from_a_report() {
    let runtime = runtime_report();
    let mut candidate = runtime_report();
    {
        let case = case_mut(&mut candidate, "causal_reason_travels_with_its_effect");
        assert_eq!(case.retained_causal_spans, 1);
        case.retained_causal_spans = 0;
    }
    renormalize(&mut candidate);

    let comparison = compare(&published_fixture(), &runtime, &candidate).expect("comparable");
    let delta = comparison
        .case_deltas
        .iter()
        .find(|delta| delta.case == "causal_reason_travels_with_its_effect")
        .expect("the causal case is compared");
    assert_eq!(delta.runtime_causal_pairs_delivered, 1);
    assert_eq!(delta.candidate_causal_pairs_delivered, 1);
    let row = comparison
        .deltas
        .iter()
        .find(|delta| delta.metric == "causal_pairs_delivered")
        .expect("the causal row is reported");
    assert_eq!((row.runtime, row.candidate), (1.0, 1.0));
    assert!(
        !row.candidate_better && !row.candidate_worse,
        "a metric the comparison reports, not one it scores"
    );
}

/// A causal count invented beside a delivery that cannot support it is refused
/// rather than shown.
#[test]
fn a_causal_count_the_delivery_cannot_support_is_refused() {
    let runtime = runtime_report();
    let mut candidate = runtime_report();
    {
        // The delivery is untouched; only the count changes. No causal pair is
        // declared for this case, so nothing can support the claim.
        let case = case_mut(&mut candidate, "preference_update");
        assert_eq!(case.retained_causal_spans, 0);
        case.retained_causal_spans = 1;
    }
    renormalize(&mut candidate);

    let error = compare(&published_fixture(), &runtime, &candidate)
        .expect_err("an unsupported causal count is refused");
    assert!(
        error.to_string().contains("retained causal span"),
        "the refusal names the metric: {error}"
    );
}

#[test]
fn the_verdict_round_trips_as_json() {
    let runtime = runtime_report();
    let comparison = compare(
        &published_fixture(),
        &runtime,
        &candidate_with_ordered_timeline(),
    )
    .expect("comparable");
    let json = serde_json::to_string(&comparison).expect("serialize the verdict");
    let back: aivtuber_app::MemoryComparison =
        serde_json::from_str(&json).expect("deserialize the verdict");
    assert_eq!(back, comparison);
    assert_eq!(back.verdict_label(), "improvement");
}
