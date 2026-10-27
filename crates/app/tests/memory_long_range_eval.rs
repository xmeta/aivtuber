//! Fixture-driven long-range memory evaluation (#105).
//!
//! The cases live in `examples/memory-long-range/cases.json` rather than in
//! this file, so the exact same inputs can be handed to an external memory
//! sidecar and both sides can emit a comparable `memory-long-range-eval/v1`
//! report. This test checks the published fixture:
//!
//! * every case measures cleanly (current evidence delivered, no superseded,
//!   refused, or shortened claim, request bounds held, temporal expectations
//!   met, retention inside its bounds);
//! * the report is byte-identical across runs;
//! * the fixture contract refuses a case that could not be compared — an
//!   unknown schema version, a repeated claim id, an expectation naming a
//!   claim that does not exist.

use aivtuber_app::{
    MEMORY_EVAL_FIXTURE_SCHEMA_VERSION, MemoryEvalCase, MemoryEvalFixture, evaluate,
};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/memory-long-range/cases.json")
}

fn published_fixture() -> MemoryEvalFixture {
    let path = fixture_path();
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// The scenario classes this issue mandates. Removing one silently would shrink
/// the comparison, so the published fixture must keep all of them.
const REQUIRED_CASES: [&str; 14] = [
    "preference_update",
    "causal_reason_travels_with_its_effect",
    "topic_shifted_correction",
    "topic_shifted_correction_under_limit",
    "goal_attempt_result",
    "record_time_inverts_event_time",
    "contradiction_without_update",
    "other_viewer_same_topic",
    "expiry_keeps_newer_linked",
    "budget_pressure_drops_whole",
    "oversized_claim_skipped_whole",
    "retention_plateau",
    "no_scoped_memory",
    "topic_mismatch_ranks_lower",
];

#[test]
fn the_published_fixture_measures_cleanly() {
    let fixture = published_fixture();
    assert_eq!(fixture.schema_version, MEMORY_EVAL_FIXTURE_SCHEMA_VERSION);
    for required in REQUIRED_CASES {
        assert!(
            fixture.cases.iter().any(|case| case.id == required),
            "the fixture must keep the {required} case"
        );
    }

    let outcome = evaluate(&fixture).expect("the published fixture evaluates");
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&outcome.report).expect("serialize the evaluation report")
    );
    assert!(
        outcome.violations.is_empty(),
        "the published fixture must measure cleanly:\n{}",
        outcome.violations.join("\n")
    );

    // Aggregate pins on top of the per-case contract.
    let totals = &outcome.report.totals;
    assert_eq!(totals.cases, fixture.cases.len());
    assert_eq!(totals.current_fact_correctness, 1.0);
    assert_eq!(totals.stale_memory_use, 0);
    assert_eq!(totals.absent_violations, 0);
    assert_eq!(totals.truncated_claims, 0);
    assert_eq!(
        totals.causal_spans_delivered, 1,
        "the fixture must keep measuring a delivered cause/effect pair"
    );
    assert!(
        totals.temporal_order_errors >= 1,
        "the fixture must keep detecting the record-time/event-time class"
    );
    assert!(
        outcome
            .report
            .cases
            .iter()
            .all(|case| case.within_item_budget && case.within_byte_budget)
    );
}

#[test]
fn the_evaluation_report_is_deterministic() {
    // Replay matters more than the numbers: the same fixture must produce a
    // byte-identical report, or two runs — and two implementations — cannot be
    // compared.
    let fixture = published_fixture();
    let first = serde_json::to_string(&evaluate(&fixture).expect("first run").report)
        .expect("serialize first run");
    let second = serde_json::to_string(&evaluate(&fixture).expect("second run").report)
        .expect("serialize second run");
    assert_eq!(
        first, second,
        "the long-range evaluation must be replayable"
    );
}

#[test]
fn a_fixture_that_cannot_be_compared_is_refused() {
    let mut fixture = published_fixture();
    fixture.schema_version = "memory-eval-v0".to_owned();
    let error = evaluate(&fixture).expect_err("an unknown schema version is refused");
    assert!(
        error.to_string().contains("memory-eval-v1"),
        "the error names the expected version: {error}"
    );

    let mut fixture = published_fixture();
    let repeated = fixture.cases[1].claims[0].event_id.clone();
    fixture.cases[1].claims[1].event_id = repeated;
    // Repeating a claim id inside one case would silently make two nodes one,
    // so the fixture is refused rather than measured ambiguously.
    let error = evaluate(&fixture).expect_err("a repeated claim id is refused");
    assert!(error.to_string().contains("repeated"), "{error}");

    let mut fixture = published_fixture();
    fixture.cases[0]
        .must_deliver
        .push("evt-does-not-exist".to_owned());
    let error = evaluate(&fixture).expect_err("an unknown expectation is refused");
    assert!(error.to_string().contains("evt-does-not-exist"), "{error}");

    let mut fixture = published_fixture();
    let causal = fixture
        .cases
        .iter_mut()
        .find(|case| case.id == "causal_reason_travels_with_its_effect")
        .expect("the causal case is published");
    causal.claims[1].caused_by = Some("evt-not-a-claim".to_owned());
    let error = evaluate(&fixture).expect_err("an unknown cause is refused");
    assert!(error.to_string().contains("evt-not-a-claim"), "{error}");
}

#[test]
fn a_case_can_be_built_in_memory_for_a_one_off_probe() {
    // The fixture is the shared artifact, but a probe case must not require a
    // file just to ask one question.
    let fixture = MemoryEvalFixture {
        schema_version: MEMORY_EVAL_FIXTURE_SCHEMA_VERSION.to_owned(),
        cases: vec![
            serde_json::from_value::<MemoryEvalCase>(serde_json::json!({
                "id": "one_off",
                "claims": [{
                    "event_id": "evt-one",
                    "actor": "viewer-1",
                    "text": "viewer liked the stream",
                    "topic": "stream",
                    "recorded_at_ms": 1000
                }],
                "query": {
                    "actor": "viewer-1",
                    "topic": "stream",
                    "now_ms": 2000,
                    "charged_bytes": 16
                },
                "must_deliver": ["evt-one"]
            }))
            .expect("probe case"),
        ],
    };
    let outcome = evaluate(&fixture).expect("probe evaluates");
    assert!(outcome.violations.is_empty(), "{:?}", outcome.violations);
    assert_eq!(outcome.report.totals.cases, 1);
}
