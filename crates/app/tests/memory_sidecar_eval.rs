//! The sidecar side of the long-range memory evaluation (#105).
//!
//! The runtime's own measurement is pinned by `memory_long_range_eval.rs`;
//! these tests pin the other half: what the project does with what an external
//! memory service answers. A fake sidecar stands in for the service, so each
//! test states one property of the boundary:
//!
//! * a service that answers inside the scope the project asked for is measured
//!   by the same metrics and bounds as the runtime;
//! * a claim the project retired is refused when a service answers with it
//!   anyway — the project's retention policy wins over the service's index;
//! * a claim of somebody else is refused, however the service matched it;
//! * a claim the project never admitted is refused;
//! * text the service rewrote is a shortened claim, whatever the service
//!   called it;
//! * the request budget applied to a service's answer is the runtime's own.

use aivtuber_app::{
    MemoryEvalCase, MemoryEvalClaim, MemoryEvalFixture, MemoryEvalQuery, RefusalReason, compare,
    evaluate, evaluate_sidecar, measure_case,
};
use aivtuber_domain::{
    EngineError, EngineErrorKind, MAX_SIDECAR_RECALL_ITEMS, MAX_THINKING_CONTEXT_BYTES,
    MemorySidecar, SidecarClaim, SidecarHandle, SidecarRecallItem, SidecarRecallQuery,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// How the fake service answers a recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Every live claim in the scope, in retention order, filtered by the
    /// query's actor scope: what a well-behaved service does.
    Conforming,
    /// Also answer with claims the project retired.
    KeepsRetired,
    /// Ignore the actor scope the request named.
    IgnoresScope,
    /// Answer with its own rendering of the claim text.
    RewritesText,
}

#[derive(Clone)]
struct Stored {
    text: String,
    actor: Option<String>,
    retired: bool,
}

/// A memory service with no ranking of its own: it keeps what it is told and
/// answers with everything it still has, so the tests can state exactly what
/// the project accepted.
struct FakeSidecar {
    answer: Answer,
    /// A claim id the service invents, which the project never admitted.
    unknown: Option<String>,
    scopes: Mutex<BTreeMap<String, Vec<(String, Stored)>>>,
}

impl FakeSidecar {
    fn new(answer: Answer) -> Self {
        Self {
            answer,
            unknown: None,
            scopes: Mutex::new(BTreeMap::new()),
        }
    }

    fn with_unknown(answer: Answer, claim_id: &str) -> Self {
        Self {
            answer,
            unknown: Some(claim_id.to_owned()),
            scopes: Mutex::new(BTreeMap::new()),
        }
    }
}

impl MemorySidecar for FakeSidecar {
    fn open_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.scopes
            .lock()
            .expect("fake sidecar lock")
            .insert(scope.to_owned(), Vec::new());
        Ok(())
    }

    fn retain(&self, scope: &str, claim: &SidecarClaim) -> Result<SidecarHandle, EngineError> {
        claim.validate()?;
        let mut scopes = self.scopes.lock().expect("fake sidecar lock");
        let items = scopes.get_mut(scope).ok_or_else(|| {
            EngineError::new(EngineErrorKind::Backend, "retained outside an open scope")
        })?;
        items.push((
            claim.claim_id.clone(),
            Stored {
                text: claim.text.clone(),
                actor: claim.actor.clone(),
                retired: false,
            },
        ));
        Ok(SidecarHandle::new(claim.claim_id.clone()))
    }

    fn recall(
        &self,
        scope: &str,
        query: &SidecarRecallQuery,
    ) -> Result<Vec<SidecarRecallItem>, EngineError> {
        query.validate()?;
        let scopes = self.scopes.lock().expect("fake sidecar lock");
        let items = scopes.get(scope).ok_or_else(|| {
            EngineError::new(EngineErrorKind::Backend, "recalled from a closed scope")
        })?;
        let mut answered = Vec::new();
        for (claim_id, stored) in items {
            if stored.retired && self.answer != Answer::KeepsRetired {
                continue;
            }
            let in_scope = self.answer == Answer::IgnoresScope
                || query.actor.is_none()
                || stored.actor == query.actor
                || stored.actor.is_none();
            if !in_scope {
                continue;
            }
            answered.push(SidecarRecallItem {
                claim_id: claim_id.clone(),
                text: if self.answer == Answer::RewritesText {
                    format!("summarised: {}", stored.text)
                } else {
                    stored.text.clone()
                },
                actor: stored.actor.clone(),
                topic: None,
                event_at_ms: None,
            });
        }
        if let Some(unknown) = &self.unknown {
            answered.push(SidecarRecallItem {
                claim_id: unknown.clone(),
                text: "a claim nobody admitted".to_owned(),
                actor: query.actor.clone(),
                topic: None,
                event_at_ms: None,
            });
        }
        answered.truncate(query.limit.min(MAX_SIDECAR_RECALL_ITEMS));
        Ok(answered)
    }

    fn retire(&self, scope: &str, handle: &SidecarHandle) -> Result<(), EngineError> {
        let mut scopes = self.scopes.lock().expect("fake sidecar lock");
        let items = scopes.get_mut(scope).ok_or_else(|| {
            EngineError::new(EngineErrorKind::Backend, "retired in a closed scope")
        })?;
        for (claim_id, stored) in items.iter_mut() {
            if claim_id == handle.as_str() {
                stored.retired = true;
            }
        }
        Ok(())
    }

    fn close_scope(&self, scope: &str) -> Result<(), EngineError> {
        self.scopes.lock().expect("fake sidecar lock").remove(scope);
        Ok(())
    }
}

fn claim(
    id: &str,
    actor: Option<&str>,
    topic: Option<&str>,
    recorded_at_ms: u64,
    text: &str,
) -> MemoryEvalClaim {
    MemoryEvalClaim {
        event_id: id.to_owned(),
        actor: actor.map(str::to_owned),
        text: text.to_owned(),
        text_bytes: None,
        topic: topic.map(str::to_owned),
        recorded_at_ms,
        event_at_ms: None,
        supersedes: None,
    }
}

fn query(topic: Option<&str>, now_ms: u64, charged_bytes: usize) -> MemoryEvalQuery {
    MemoryEvalQuery {
        actor: Some("viewer-1".to_owned()),
        topic: topic.map(str::to_owned),
        now_ms,
        charged_bytes,
    }
}

fn case(
    id: &str,
    claims: Vec<MemoryEvalClaim>,
    query: MemoryEvalQuery,
    must_deliver: &[&str],
    must_not_deliver: &[&str],
) -> MemoryEvalCase {
    MemoryEvalCase {
        id: id.to_owned(),
        config: Default::default(),
        claims,
        query,
        must_deliver: must_deliver.iter().map(|id| (*id).to_owned()).collect(),
        must_not_deliver: must_not_deliver.iter().map(|id| (*id).to_owned()).collect(),
        stale: Vec::new(),
        expected_temporal_errors: 0,
        expected_delivery_order: Vec::new(),
    }
}

fn fixture(cases: Vec<MemoryEvalCase>) -> MemoryEvalFixture {
    MemoryEvalFixture {
        schema_version: aivtuber_app::MEMORY_EVAL_FIXTURE_SCHEMA_VERSION.to_owned(),
        cases,
    }
}

fn evaluate_fake(answer: Answer, fixture: &MemoryEvalFixture) -> aivtuber_app::MemoryEvalOutcome {
    evaluate_sidecar(&FakeSidecar::new(answer), fixture, "test").expect("the fake sidecar answers")
}

#[test]
fn a_conforming_service_is_measured_cleanly() {
    let fixture = fixture(vec![case(
        "same_topic",
        vec![
            claim(
                "evt-old",
                Some("viewer-1"),
                Some("game-x"),
                1000,
                "old claim",
            ),
            claim(
                "evt-new",
                Some("viewer-1"),
                Some("game-x"),
                2000,
                "new claim",
            ),
        ],
        query(Some("game-x"), 3000, 64),
        &["evt-old", "evt-new"],
        &[],
    )]);
    let outcome = evaluate_fake(Answer::Conforming, &fixture);
    assert!(
        outcome.violations.is_empty(),
        "a conforming answer measured clean:\n{}",
        outcome.violations.join("\n")
    );
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-old", "evt-new"]);
    assert_eq!(report.current_fact_correctness, 1.0);
    assert_eq!(report.stale_memory_use, 0);
    assert_eq!(report.absent_violations, 0);
    assert_eq!(report.truncated_claims, 0);
    assert!(report.refused_items.is_empty());
    assert!(report.within_item_budget && report.within_byte_budget);
}

#[test]
fn a_retired_claim_the_service_keeps_is_refused() {
    let mut superseding = claim(
        "evt-new",
        Some("viewer-1"),
        Some("game-x"),
        2000,
        "new claim",
    );
    superseding.supersedes = Some("evt-old".to_owned());
    let fixture = fixture(vec![case(
        "superseded",
        vec![
            claim(
                "evt-old",
                Some("viewer-1"),
                Some("game-x"),
                1000,
                "old claim",
            ),
            superseding,
        ],
        query(Some("game-x"), 3000, 64),
        &["evt-new"],
        &["evt-old"],
    )]);
    let outcome = evaluate_fake(Answer::KeepsRetired, &fixture);
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-new"]);
    assert_eq!(
        report.refused_items,
        [aivtuber_app::RefusedItem {
            claim_id: Some("evt-old".to_owned()),
            reason: RefusalReason::Removed,
        }]
    );
    assert!(report.absent_violations == 0);
    assert!(
        outcome
            .violations
            .iter()
            .any(|violation| violation.contains("refused by the project")),
        "a kept removed claim is reported, got:\n{}",
        outcome.violations.join("\n")
    );
}

#[test]
fn a_claim_of_another_scope_is_refused() {
    let fixture = fixture(vec![case(
        "other_viewer",
        vec![
            claim(
                "evt-v1",
                Some("viewer-1"),
                Some("game-x"),
                1000,
                "viewer one",
            ),
            claim(
                "evt-v2",
                Some("viewer-2"),
                Some("game-x"),
                2000,
                "viewer two",
            ),
        ],
        query(Some("game-x"), 3000, 64),
        &["evt-v1"],
        &["evt-v2"],
    )]);
    let outcome = evaluate_fake(Answer::IgnoresScope, &fixture);
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-v1"]);
    assert_eq!(
        report.refused_items,
        [aivtuber_app::RefusedItem {
            claim_id: Some("evt-v2".to_owned()),
            reason: RefusalReason::OutOfScope,
        }]
    );
}

#[test]
fn a_claim_the_project_never_admitted_is_refused() {
    let fixture = fixture(vec![case(
        "unknown",
        vec![claim(
            "evt-known",
            Some("viewer-1"),
            Some("game-x"),
            1000,
            "known",
        )],
        query(Some("game-x"), 3000, 64),
        &["evt-known"],
        &[],
    )]);
    let outcome = evaluate_sidecar(
        &FakeSidecar::with_unknown(Answer::Conforming, "evt-ghost"),
        &fixture,
        "test",
    )
    .expect("the fake sidecar answers");
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-known"]);
    assert_eq!(
        report.refused_items,
        [aivtuber_app::RefusedItem {
            claim_id: Some("evt-ghost".to_owned()),
            reason: RefusalReason::Unknown,
        }]
    );
}

#[test]
fn text_the_service_rewrote_is_a_shortened_claim() {
    let fixture = fixture(vec![case(
        "rewritten",
        vec![claim(
            "evt-1",
            Some("viewer-1"),
            Some("game-x"),
            1000,
            "the viewer liked it",
        )],
        query(Some("game-x"), 3000, 64),
        &["evt-1"],
        &[],
    )]);
    let outcome = evaluate_fake(Answer::RewritesText, &fixture);
    let report = &outcome.report.cases[0];
    assert_eq!(report.truncated_claims, 1);
    assert!(
        outcome
            .violations
            .iter()
            .any(|violation| violation.contains("shortened claim")),
        "a rewritten claim is reported as shortened, got:\n{}",
        outcome.violations.join("\n")
    );
}

#[test]
fn the_request_budget_is_the_runtimes_own() {
    let mut claims = Vec::new();
    for (index, id) in ["evt-b1", "evt-b2", "evt-b3"].iter().enumerate() {
        let mut sized = claim(
            id,
            Some("viewer-1"),
            Some("game-x"),
            1000 + index as u64,
            "seed",
        );
        sized.text_bytes = Some(3_000);
        claims.push(sized);
    }
    let charged = 1_000;
    let fixture = fixture(vec![case(
        "budget",
        claims,
        query(Some("game-x"), 5000, charged),
        &["evt-b1", "evt-b2"],
        &["evt-b3"],
    )]);
    let outcome = evaluate_fake(Answer::Conforming, &fixture);
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-b1", "evt-b2"]);
    assert!(report.request_bytes <= MAX_THINKING_CONTEXT_BYTES);
    assert_eq!(report.request_bytes, charged + 6_000);
    assert!(
        outcome.violations.is_empty(),
        "the runtime's budget is applied to the answer:\n{}",
        outcome.violations.join("\n")
    );

    // The same answer measured by the runtime's own measurement rule keeps
    // exactly what the runner kept, so the two sides of a comparison count
    // the budget the same way.
    let delivery = aivtuber_app::CaseDelivery {
        claims: vec![
            aivtuber_app::DeliveredClaim {
                claim_id: "evt-b1".to_owned(),
                delivered_text: "x".repeat(3_000),
                retained_text: "x".repeat(3_000),
            },
            aivtuber_app::DeliveredClaim {
                claim_id: "evt-b2".to_owned(),
                delivered_text: "x".repeat(3_000),
                retained_text: "x".repeat(3_000),
            },
        ],
        refused: Vec::new(),
    };
    let measured = measure_case(
        &fixture.cases[0],
        &delivery,
        &aivtuber_app::RetentionSnapshot {
            entries_high_water: 3,
            links_high_water: 0,
            max_entries: 256,
            max_links: 64,
        },
    );
    assert_eq!(measured.request_bytes, report.request_bytes);
    assert!(measured.within_byte_budget);
}

#[test]
fn an_expired_claim_the_service_keeps_is_refused() {
    let mut expired = case(
        "expiry",
        vec![
            claim(
                "evt-old",
                Some("viewer-1"),
                Some("game-x"),
                500,
                "old claim",
            ),
            claim(
                "evt-new",
                Some("viewer-1"),
                Some("game-x"),
                1500,
                "new claim",
            ),
        ],
        query(Some("game-x"), 2000, 64),
        &["evt-new"],
        &["evt-old"],
    );
    expired.config.durable_ttl_ms = Some(1000);
    let outcome = evaluate_fake(Answer::KeepsRetired, &fixture(vec![expired]));
    let report = &outcome.report.cases[0];
    assert_eq!(report.delivered, ["evt-new"]);
    assert_eq!(
        report.refused_items,
        [aivtuber_app::RefusedItem {
            claim_id: Some("evt-old".to_owned()),
            reason: RefusalReason::Removed,
        }]
    );
}

/// A candidate whose answer leaks another viewer's claim is rejected by the
/// comparison, even though the project refused the item and delivered exactly
/// what the runtime delivered.
#[test]
fn a_refused_service_item_rejects_the_candidate() {
    let fixture = fixture(vec![case(
        "scope_leak",
        vec![
            claim(
                "evt-v1",
                Some("viewer-1"),
                Some("game-x"),
                1000,
                "viewer one",
            ),
            claim(
                "evt-v2",
                Some("viewer-2"),
                Some("game-x"),
                2000,
                "viewer two",
            ),
        ],
        query(Some("game-x"), 3000, 64),
        &["evt-v1"],
        &["evt-v2"],
    )]);
    let runtime = evaluate(&fixture).expect("the runtime measures the fixture");
    assert!(runtime.violations.is_empty(), "{:?}", runtime.violations);
    let candidate = evaluate_fake(Answer::IgnoresScope, &fixture);
    assert_eq!(candidate.report.totals.absent_violations, 0);
    assert_eq!(candidate.report.totals.refused_items, 1);

    let comparison = compare(&runtime.report, &candidate.report).expect("comparable reports");
    assert!(comparison.is_regression());
    assert!(
        comparison
            .reasons
            .iter()
            .any(|reason| reason.contains("the project refused")),
        "the refusal is the reason, got:\n{}",
        comparison.reasons.join("\n")
    );
}

/// A service that answers exactly like the runtime is not a regression, and
/// wins nothing: the comparison supports keeping the runtime's own store.
#[test]
fn an_equivalent_candidate_is_no_improvement() {
    let fixture = fixture(vec![case(
        "equivalent",
        vec![claim(
            "evt-1",
            Some("viewer-1"),
            Some("game-x"),
            1000,
            "the claim",
        )],
        query(Some("game-x"), 3000, 64),
        &["evt-1"],
        &[],
    )]);
    let runtime = evaluate(&fixture).expect("the runtime measures the fixture");
    let candidate = evaluate_fake(Answer::Conforming, &fixture);
    let comparison = compare(&runtime.report, &candidate.report).expect("comparable reports");
    assert_eq!(
        comparison.verdict,
        aivtuber_app::CandidateVerdict::NoImprovement,
        "{:?}",
        comparison.reasons
    );
    assert!(!comparison.is_regression());
}

/// The runner never hands the service a claim the project did not admit, and
/// the report it produces carries the runtime's schema so the two documents
/// are comparable by construction.
#[test]
fn the_report_carries_the_comparable_schema() {
    let fixture = fixture(vec![case(
        "schema",
        vec![claim(
            "evt-1",
            Some("viewer-1"),
            Some("game-x"),
            1000,
            "the claim",
        )],
        query(Some("game-x"), 3000, 64),
        &["evt-1"],
        &[],
    )]);
    let outcome = evaluate_fake(Answer::Conforming, &fixture);
    assert_eq!(
        outcome.report.schema_version,
        aivtuber_app::MEMORY_EVAL_REPORT_SCHEMA_VERSION
    );
    assert_eq!(
        outcome.report.fixture_schema_version,
        aivtuber_app::MEMORY_EVAL_FIXTURE_SCHEMA_VERSION
    );
    assert_eq!(outcome.report.context_item_budget, 12);
    assert_eq!(
        outcome.report.context_byte_budget,
        MAX_THINKING_CONTEXT_BYTES
    );
}
