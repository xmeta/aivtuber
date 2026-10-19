//! Long-range memory evaluation on the delivered generation context (#105).
//!
//! The retained-memory suite in `aivtuber-adaptation` pins what the store
//! *marks* (staleness, provenance, links, expiry). This suite measures what the
//! runtime actually *delivers*: it calls the same
//! [`retained_memory_context`] the composition root calls, so a measured result
//! cannot drift from the request production assembles.
//!
//! It reports the #105 evaluation vocabulary over the issue's scenario classes
//! — preference change, goal → attempt → result, correction of an old fact
//! (including one that moved topic), two unrelated same-topic events, a
//! contradiction that is not an update, an expiry that keeps the newer linked
//! memory, cross-viewer scope, budget pressure, and retention plateau — as a
//! deterministic JSON document:
//!
//! * `current_fact_correctness` — share of claims the query must receive that
//!   were actually delivered;
//! * `stale_memory_use` — superseded claims delivered as if current (must be 0);
//! * `absent_violations` — claims delivered that the case refuses (other
//!   viewer, expired, or cut by the budget);
//! * `temporal_order_errors` — delivered pairs whose true event chronology
//!   contradicts the delivered order (the store keeps record time, not event
//!   time, so this class is measurable but not fixable by retrieval alone);
//! * `context_items`, `context_bytes`, `within_*_budget`, `truncated_claims` —
//!   the request bounds, measured rather than assumed;
//! * `entries_high_water`, `links_high_water` — retained state against the #51
//!   bounds, including the plateau case where growth stops at the cap.
//!
//! Causal link kinds (`caused_by`, `resulted_in`) are deliberately not measured:
//! the link vocabulary has no causal edge until a real evidence source exists,
//! so there is nothing to count yet (docs/memory-adaptation.adoc section 10).

use aivtuber_adaptation::{ActorPseudonymizer, MemoryEntry, WorkingMemory, WorkingMemoryConfig};
use aivtuber_app::retained_memory_context;
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EventEnvelope, EventKind, LocalControlIngress, MAX_THINKING_CONTEXT_BYTES,
    MAX_THINKING_CONTEXT_ITEMS, MAX_THINKING_TEXT_BYTES, OperatorCommandInput, SecurityPlane,
    SourceClass, TrustLevel,
};
use aivtuber_runtime::{MemoryWriteDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const REPORT_SCHEMA_VERSION: &str = "memory-long-range-eval/v1";
const SOURCE_NAMESPACE: &str = "public-chat";

const ADMIN_SECRET: [u8; 32] = [0x21_u8; 32];

fn memory_admin() -> AuthenticatedControl {
    let ingress = LocalControlIngress::new(
        "long-range-eval",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::MemoryAdmin]),
        ControlSecret::new(ADMIN_SECRET),
    )
    .expect("trusted ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-eval-admin".to_owned(),
                correlation_id: "corr-eval-admin".to_owned(),
                sequence: 1,
                observed_at: "2026-10-20T00:00:00Z".to_owned(),
                action: "memory.admin".to_owned(),
                payload: BTreeMap::new(),
            },
            &ADMIN_SECRET,
        )
        .expect("authenticated")
        .authority()
        .clone()
}

fn event(id: &str, actor: Option<&str>) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: id.to_owned(),
        correlation_id: format!("corr-{id}"),
        sequence: id.bytes().map(u64::from).sum(),
        observed_at: "2026-10-20T00:00:00Z".to_owned(),
        source: SOURCE_NAMESPACE.to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: actor.map(str::to_owned),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            Value::String("what happened next?".to_owned()),
        )]),
    }
}

/// One synthetic claim as the case declares it. `event_at_ms` is ground truth
/// the store does *not* keep — measuring that gap is what the temporal-order
/// metric is for.
struct Claim {
    event_id: &'static str,
    actor: Option<&'static str>,
    text: String,
    topic: Option<&'static str>,
    recorded_at_ms: u64,
    event_at_ms: u64,
    supersedes: Option<&'static str>,
}

fn viewer(
    event_id: &'static str,
    text: impl Into<String>,
    topic: &'static str,
    recorded_at_ms: u64,
    event_at_ms: u64,
) -> Claim {
    Claim {
        event_id,
        actor: Some("viewer-1"),
        text: text.into(),
        topic: Some(topic),
        recorded_at_ms,
        event_at_ms,
        supersedes: None,
    }
}

impl Claim {
    fn actor(mut self, actor: &'static str) -> Self {
        self.actor = Some(actor);
        self
    }

    fn supersedes(mut self, target: &'static str) -> Self {
        self.supersedes = Some(target);
        self
    }
}

struct Query {
    actor: Option<&'static str>,
    topic: Option<&'static str>,
    now_ms: u64,
    /// The request's own event text, charged before any retained claim.
    charged_bytes: usize,
}

struct Case {
    id: &'static str,
    config: WorkingMemoryConfig,
    claims: Vec<Claim>,
    query: Query,
    /// Claims this query must receive: current, in-scope evidence.
    must_deliver: Vec<&'static str>,
    /// Claims that must never reach generation (superseded, another viewer's,
    /// expired, or cut by the budget).
    must_not_deliver: Vec<&'static str>,
    /// The subset of `must_not_deliver` refused because a retained
    /// supersession replaced it.
    stale: Vec<&'static str>,
    /// Ground-truth inversion count this case exhibits; declared so a change
    /// in ordering semantics is visible instead of silently "passing".
    expected_temporal_errors: usize,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    case: &'static str,
    delivered: Vec<&'static str>,
    must_deliver: Vec<&'static str>,
    stale_claims: Vec<&'static str>,
    current_fact_correctness: f64,
    stale_memory_use: usize,
    absent_violations: usize,
    temporal_order_errors: usize,
    context_items: usize,
    claim_bytes: usize,
    request_bytes: usize,
    within_item_budget: bool,
    within_byte_budget: bool,
    truncated_claims: usize,
    entries_high_water: usize,
    links_high_water: usize,
    max_entries: usize,
    max_links: usize,
}

#[derive(Debug, Serialize)]
struct Totals {
    cases: usize,
    current_fact_correctness: f64,
    stale_memory_use: usize,
    absent_violations: usize,
    temporal_order_errors: usize,
    truncated_claims: usize,
    max_context_items: usize,
    max_claim_bytes: usize,
    max_request_bytes: usize,
    max_entries_high_water: usize,
    max_links_high_water: usize,
}

#[derive(Debug, Serialize)]
struct EvalReport {
    schema_version: &'static str,
    context_item_budget: usize,
    context_byte_budget: usize,
    item_byte_budget: usize,
    totals: Totals,
    cases: Vec<CaseReport>,
}

fn run_case(case: &Case) -> CaseReport {
    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let pseudonymizer =
        ActorPseudonymizer::new("long-range-eval-v1", [0x5a; 32]).expect("pseudonymizer");
    let mut memory = WorkingMemory::new(case.config, pseudonymizer).expect("working memory");
    let admin = memory_admin();

    // Every claim enters through the real write gate: a case memory that failed
    // the gate would not be evidence about retrieval.
    for claim in &case.claims {
        let mut envelope = event(claim.event_id, claim.actor);
        envelope
            .payload
            .insert("text".to_owned(), Value::String(claim.text.clone()));
        let (decision, permit) = security.authorize_memory_write(&envelope, Some(&admin));
        assert_ne!(
            decision,
            MemoryWriteDecision::Denied,
            "case {}: claim {} must pass the write gate",
            case.id,
            claim.event_id
        );
        let permit = permit.expect("an allowed write carries a permit");
        match claim.supersedes {
            Some(target) => memory
                .remember_durable_superseding(
                    &permit,
                    &claim.text,
                    claim.topic,
                    claim.recorded_at_ms,
                    &[target],
                )
                .expect("superseding durable write"),
            None => memory
                .remember_durable(&permit, &claim.text, claim.topic, claim.recorded_at_ms)
                .expect("durable write"),
        };
    }

    let context = retained_memory_context(
        &memory,
        SOURCE_NAMESPACE,
        case.query.actor,
        case.query.topic,
        case.query.now_ms,
        case.query.charged_bytes,
    );

    let event_at: BTreeMap<&str, u64> = case
        .claims
        .iter()
        .map(|claim| (claim.event_id, claim.event_at_ms))
        .collect();
    let mut delivered: Vec<&'static str> = Vec::new();
    let mut truncated_claims = 0usize;
    for item in &context {
        let event_id = item
            .event_id
            .as_deref()
            .expect("a retained-memory item carries its source event id");
        let claim_id = case
            .claims
            .iter()
            .find(|claim| claim.event_id == event_id)
            .unwrap_or_else(|| panic!("case {}: delivered unknown claim {event_id:?}", case.id))
            .event_id;
        // The delivered text must be exactly what the runtime admitted. A
        // different string means the request carries a shortened claim, which
        // is the semantic corruption #231 fixed for the store.
        let stored = stored_claim(&memory, event_id);
        if item.text != stored {
            truncated_claims += 1;
        }
        delivered.push(claim_id);
    }

    let mut current_hits = 0usize;
    for required in &case.must_deliver {
        if delivered.contains(required) {
            current_hits += 1;
        }
    }
    let current_fact_correctness = if case.must_deliver.is_empty() {
        1.0
    } else {
        current_hits as f64 / case.must_deliver.len() as f64
    };
    let stale_memory_use = delivered
        .iter()
        .filter(|id| case.stale.contains(id))
        .count();
    let absent_violations = delivered
        .iter()
        .filter(|id| case.must_not_deliver.contains(id))
        .count();
    let temporal_order_errors = delivered
        .windows(2)
        .filter(|pair| event_at[pair[1]] > event_at[pair[0]])
        .count();

    let claim_bytes: usize = context.iter().map(|item| item.text.len()).sum();
    let request_bytes = case.query.charged_bytes.saturating_add(claim_bytes);
    let metrics = memory.retention_metrics();
    let report = CaseReport {
        case: case.id,
        delivered,
        must_deliver: case.must_deliver.clone(),
        stale_claims: case.stale.clone(),
        current_fact_correctness,
        stale_memory_use,
        absent_violations,
        temporal_order_errors,
        context_items: context.len(),
        claim_bytes,
        request_bytes,
        within_item_budget: context.len() <= MAX_THINKING_CONTEXT_ITEMS
            && context
                .iter()
                .all(|item| item.text.len() <= MAX_THINKING_TEXT_BYTES),
        within_byte_budget: request_bytes <= MAX_THINKING_CONTEXT_BYTES,
        truncated_claims,
        entries_high_water: metrics.entries_high_water,
        links_high_water: metrics.links_high_water,
        max_entries: case.config.max_entries,
        max_links: case.config.max_links,
    };

    assert_eq!(
        report.current_fact_correctness, 1.0,
        "case {}: current evidence missing from the delivered context: {:?}",
        case.id, report
    );
    assert_eq!(
        report.stale_memory_use, 0,
        "case {}: a superseded claim reached generation as current: {:?}",
        case.id, report
    );
    assert_eq!(
        report.absent_violations, 0,
        "case {}: a refused claim reached generation: {:?}",
        case.id, report
    );
    assert_eq!(
        report.truncated_claims, 0,
        "case {}: a shortened claim reached generation: {:?}",
        case.id, report
    );
    assert!(
        report.within_item_budget && report.within_byte_budget,
        "case {}: request bounds violated: {:?}",
        case.id,
        report
    );
    assert_eq!(
        report.temporal_order_errors, case.expected_temporal_errors,
        "case {}: the temporal-order measurement changed: {:?}",
        case.id, report
    );
    assert!(
        report.entries_high_water <= report.max_entries
            && report.links_high_water <= report.max_links,
        "case {}: retention exceeded its bounds: {:?}",
        case.id,
        report
    );
    report
}

fn stored_claim<'a>(memory: &'a WorkingMemory, event_id: &str) -> &'a str {
    memory
        .entries()
        .iter()
        .find(|entry: &&MemoryEntry| entry.source.event_id == event_id)
        .map(|entry| entry.normalized_claim.as_str())
        .unwrap_or_else(|| panic!("claim {event_id:?} is not retained"))
}

/// Text of exactly `bytes` bytes, so the budget case spends a known amount.
fn claim_text(prefix: &str, bytes: usize) -> String {
    let mut text = String::with_capacity(bytes);
    text.push_str(prefix);
    while text.len() < bytes {
        text.push('x');
    }
    text
}

fn cases() -> Vec<Case> {
    vec![
        // The issue's important example: dislike → later enjoyment. The old
        // claim is superseded, so it is not evidence of the present.
        Case {
            id: "preference_update",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-dislike",
                    "viewer disliked game x",
                    "game-x",
                    1_000,
                    1_000,
                ),
                viewer(
                    "evt-enjoy",
                    "viewer enjoyed the game x ending",
                    "game-x",
                    2_000,
                    2_000,
                )
                .supersedes("evt-dislike"),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 3_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-enjoy"],
            must_not_deliver: vec!["evt-dislike"],
            stale: vec!["evt-dislike"],
            expected_temporal_errors: 0,
        },
        // A correction that moved topic is still the current evidence for the
        // topic it replaced; being topically unrelated must not cut it.
        Case {
            id: "topic_shifted_correction",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-dislike",
                    "viewer disliked game x",
                    "game-x",
                    1_000,
                    1_000,
                ),
                viewer(
                    "evt-enjoy",
                    "viewer enjoyed the game x ending",
                    "ending-arc",
                    2_000,
                    2_000,
                )
                .supersedes("evt-dislike"),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 3_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-enjoy"],
            must_not_deliver: vec!["evt-dislike"],
            stale: vec!["evt-dislike"],
            expected_temporal_errors: 0,
        },
        // The same correction under the item bound: twelve competing topic
        // matches would push a topic-shifted correction past the limit, so
        // promotion — not luck about a short result list — has to place it.
        Case {
            id: "topic_shifted_correction_under_limit",
            config: WorkingMemoryConfig::default(),
            claims: {
                let mut claims = vec![viewer(
                    "evt-old",
                    "viewer disliked game x",
                    "game-x",
                    500,
                    500,
                )];
                for index in 1..=12u64 {
                    claims.push(viewer(
                        match index {
                            1 => "evt-c1",
                            2 => "evt-c2",
                            3 => "evt-c3",
                            4 => "evt-c4",
                            5 => "evt-c5",
                            6 => "evt-c6",
                            7 => "evt-c7",
                            8 => "evt-c8",
                            9 => "evt-c9",
                            10 => "evt-c10",
                            11 => "evt-c11",
                            _ => "evt-c12",
                        },
                        format!("competing claim {index}"),
                        "game-x",
                        index * 1_000,
                        index * 1_000,
                    ));
                }
                claims.push(
                    viewer(
                        "evt-enjoy",
                        "viewer enjoyed the game x ending",
                        "ending-arc",
                        13_000,
                        13_000,
                    )
                    .supersedes("evt-old"),
                );
                claims
            },
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 14_000,
                charged_bytes: 64,
            },
            must_deliver: vec![
                "evt-enjoy",
                "evt-c12",
                "evt-c11",
                "evt-c10",
                "evt-c9",
                "evt-c8",
                "evt-c7",
                "evt-c6",
                "evt-c5",
                "evt-c4",
                "evt-c3",
                "evt-c2",
            ],
            must_not_deliver: vec!["evt-old", "evt-c1"],
            stale: vec!["evt-old"],
            expected_temporal_errors: 0,
        },
        // goal → attempt → result: the whole timeline is delivered, newest
        // first, with no staleness invented by derivation.
        Case {
            id: "goal_attempt_result",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-goal",
                    "goal: clear the raid tonight",
                    "raid-night",
                    1_000,
                    1_000,
                ),
                viewer(
                    "evt-attempt",
                    "attempt: pulled the second boss",
                    "raid-night",
                    2_000,
                    2_000,
                ),
                viewer(
                    "evt-result",
                    "result: cleared the raid",
                    "raid-night",
                    3_000,
                    3_000,
                ),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("raid-night"),
                now_ms: 4_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-goal", "evt-attempt", "evt-result"],
            must_not_deliver: Vec::new(),
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Recorded out of event order: the store keeps record time, so the
        // delivered order contradicts the true chronology. This is the class
        // the measurement must keep detecting rather than smooth over.
        Case {
            id: "record_time_inverts_event_time",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer("evt-finished", "finished game x", "game-x", 1_000, 900),
                viewer("evt-started", "started game y", "game-y", 1_500, 100),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: None,
                now_ms: 2_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-started", "evt-finished"],
            must_not_deliver: Vec::new(),
            stale: Vec::new(),
            expected_temporal_errors: 1,
        },
        // "A contradiction that is not actually an update": same topic, no
        // declared update evidence, so both claims stay current.
        Case {
            id: "contradiction_without_update",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-claim-a",
                    "viewer prefers game x",
                    "game-x",
                    1_000,
                    1_000,
                ),
                viewer(
                    "evt-claim-b",
                    "viewer prefers another game",
                    "game-x",
                    2_000,
                    2_000,
                ),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 3_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-claim-a", "evt-claim-b"],
            must_not_deliver: Vec::new(),
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Another viewer's same-topic claim is never this viewer's context.
        Case {
            id: "other_viewer_same_topic",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-v1",
                    "viewer one loved the boss fight",
                    "game-x",
                    1_000,
                    1_000,
                ),
                viewer(
                    "evt-v2",
                    "viewer two loved the boss fight",
                    "game-x",
                    2_000,
                    2_000,
                )
                .actor("viewer-2"),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 3_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-v1"],
            must_not_deliver: vec!["evt-v2"],
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Expiry of the old claim leaves the newer linked claim current and
        // carries no trace of the removed one.
        Case {
            id: "expiry_keeps_newer_linked",
            config: WorkingMemoryConfig {
                durable_ttl_ms: 1_000,
                ..WorkingMemoryConfig::default()
            },
            claims: vec![
                viewer("evt-old", "old opinion about game x", "game-x", 100, 100),
                viewer(
                    "evt-new",
                    "current opinion about game x",
                    "game-x",
                    1_000,
                    1_000,
                )
                .supersedes("evt-old"),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 1_600,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-new"],
            must_not_deliver: vec!["evt-old"],
            stale: vec!["evt-old"],
            expected_temporal_errors: 0,
        },
        // Budget pressure: the current fact is delivered, the prefix stops at
        // the budget, and the claim that no longer fits is dropped whole.
        Case {
            id: "budget_pressure_drops_whole",
            config: WorkingMemoryConfig {
                max_claim_bytes: 8 * 1024,
                ..WorkingMemoryConfig::default()
            },
            claims: vec![
                viewer(
                    "evt-current",
                    "current fact: the raid moved to friday",
                    "raid-night",
                    3_000,
                    3_000,
                ),
                viewer(
                    "evt-o1",
                    claim_text("detail one ", 3_000),
                    "raid-night",
                    2_000,
                    2_000,
                ),
                viewer(
                    "evt-o2",
                    claim_text("detail two ", 3_000),
                    "raid-night",
                    1_500,
                    1_500,
                ),
                viewer(
                    "evt-o3",
                    claim_text("detail three ", 3_000),
                    "raid-night",
                    1_000,
                    1_000,
                ),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("raid-night"),
                now_ms: 4_000,
                charged_bytes: 1_000,
            },
            must_deliver: vec!["evt-current", "evt-o1", "evt-o2"],
            must_not_deliver: vec!["evt-o3"],
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // A claim larger than one item's own bound is skipped whole, and the
        // next claim uses the room rather than inheriting a shortened text.
        Case {
            id: "oversized_claim_skipped_whole",
            config: WorkingMemoryConfig {
                max_claim_bytes: 8 * 1024,
                ..WorkingMemoryConfig::default()
            },
            claims: vec![
                viewer(
                    "evt-huge",
                    claim_text("huge claim ", MAX_THINKING_TEXT_BYTES + 128),
                    "raid-night",
                    3_000,
                    3_000,
                ),
                viewer(
                    "evt-current",
                    "current fact: the raid moved to friday",
                    "raid-night",
                    2_000,
                    2_000,
                ),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("raid-night"),
                now_ms: 4_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-current"],
            must_not_deliver: vec!["evt-huge"],
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Retention plateau: eight writes under a four-entry cap stop growing
        // at the cap, and only the retained window is delivered.
        Case {
            id: "retention_plateau",
            config: WorkingMemoryConfig {
                max_entries: 4,
                max_links: 6,
                max_links_per_memory: 4,
                ..WorkingMemoryConfig::default()
            },
            claims: (1..=8)
                .map(|index| {
                    viewer(
                        match index {
                            1 => "evt-p1",
                            2 => "evt-p2",
                            3 => "evt-p3",
                            4 => "evt-p4",
                            5 => "evt-p5",
                            6 => "evt-p6",
                            7 => "evt-p7",
                            _ => "evt-p8",
                        },
                        format!("plateau claim {index}"),
                        "game-x",
                        index as u64 * 1_000,
                        index as u64 * 1_000,
                    )
                })
                .collect(),
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 9_000,
                charged_bytes: 64,
            },
            must_deliver: vec!["evt-p5", "evt-p6", "evt-p7", "evt-p8"],
            must_not_deliver: vec!["evt-p1", "evt-p2", "evt-p3", "evt-p4"],
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Nothing in this viewer's scope at all: the request goes out with no
        // memory context rather than with another viewer's words.
        Case {
            id: "no_scoped_memory",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-other-viewer",
                    "viewer two talked about game x",
                    "game-x",
                    1_000,
                    1_000,
                )
                .actor("viewer-2"),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 2_000,
                charged_bytes: 64,
            },
            must_deliver: Vec::new(),
            must_not_deliver: vec!["evt-other-viewer"],
            stale: Vec::new(),
            expected_temporal_errors: 0,
        },
        // Topic is a *rank*, not a filter: a retained claim on another topic is
        // still delivered behind the topic match. Measured so the precision
        // cost of the current rule is visible rather than inferred.
        Case {
            id: "topic_mismatch_ranks_lower",
            config: WorkingMemoryConfig::default(),
            claims: vec![
                viewer(
                    "evt-other-topic",
                    "viewer talked about game y",
                    "game-y",
                    2_000,
                    2_000,
                ),
                viewer(
                    "evt-this-topic",
                    "viewer talked about game x",
                    "game-x",
                    1_000,
                    1_000,
                ),
            ],
            query: Query {
                actor: Some("viewer-1"),
                topic: Some("game-x"),
                now_ms: 3_000,
                charged_bytes: 64,
            },
            // `retained_memory_context` keeps rank order, so the topic match
            // leads even though the other claim is newer.
            must_deliver: vec!["evt-this-topic", "evt-other-topic"],
            must_not_deliver: Vec::new(),
            stale: Vec::new(),
            expected_temporal_errors: 1,
        },
    ]
}

fn run_all() -> EvalReport {
    let cases: Vec<CaseReport> = cases().iter().map(run_case).collect();
    let case_count = cases.len();
    let mean_correctness = cases
        .iter()
        .map(|case| case.current_fact_correctness)
        .sum::<f64>()
        / case_count as f64;
    let totals = Totals {
        cases: case_count,
        current_fact_correctness: mean_correctness,
        stale_memory_use: cases.iter().map(|case| case.stale_memory_use).sum(),
        absent_violations: cases.iter().map(|case| case.absent_violations).sum(),
        temporal_order_errors: cases.iter().map(|case| case.temporal_order_errors).sum(),
        truncated_claims: cases.iter().map(|case| case.truncated_claims).sum(),
        max_context_items: cases
            .iter()
            .map(|case| case.context_items)
            .max()
            .unwrap_or(0),
        max_claim_bytes: cases.iter().map(|case| case.claim_bytes).max().unwrap_or(0),
        max_request_bytes: cases
            .iter()
            .map(|case| case.request_bytes)
            .max()
            .unwrap_or(0),
        max_entries_high_water: cases
            .iter()
            .map(|case| case.entries_high_water)
            .max()
            .unwrap_or(0),
        max_links_high_water: cases
            .iter()
            .map(|case| case.links_high_water)
            .max()
            .unwrap_or(0),
    };
    EvalReport {
        schema_version: REPORT_SCHEMA_VERSION,
        context_item_budget: MAX_THINKING_CONTEXT_ITEMS,
        context_byte_budget: MAX_THINKING_CONTEXT_BYTES,
        item_byte_budget: MAX_THINKING_TEXT_BYTES,
        totals,
        cases,
    }
}

#[test]
fn long_range_evaluation_measures_the_delivered_generation_context() {
    let report = run_all();
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize the evaluation report")
    );

    // Every case asserts its own expectations; these pin the aggregate.
    assert_eq!(report.totals.stale_memory_use, 0);
    assert_eq!(report.totals.absent_violations, 0);
    assert_eq!(report.totals.truncated_claims, 0);
    assert_eq!(report.totals.current_fact_correctness, 1.0);
    assert!(report.totals.max_request_bytes <= MAX_THINKING_CONTEXT_BYTES);
    assert!(report.totals.max_context_items <= MAX_THINKING_CONTEXT_ITEMS);
    assert!(
        report.totals.temporal_order_errors >= 1,
        "the evaluation must keep detecting the record-time/event-time class"
    );
}

#[test]
fn the_delivered_order_is_topic_rank_then_record_time() {
    let report = run_all();
    let order = |case: &str| -> Vec<&'static str> {
        report
            .cases
            .iter()
            .find(|entry| entry.case == case)
            .unwrap_or_else(|| panic!("case {case} is in the evaluation"))
            .delivered
            .clone()
    };

    // A timeline is delivered newest-record-first, so the result of a
    // goal/attempt/result sequence leads.
    assert_eq!(
        order("goal_attempt_result"),
        vec!["evt-result", "evt-attempt", "evt-goal"]
    );
    // A topic match leads a newer claim on another topic: topic is a rank, not
    // a filter, and the cost of that rule is visible in the delivered list.
    assert_eq!(
        order("topic_mismatch_ranks_lower"),
        vec!["evt-this-topic", "evt-other-topic"]
    );
}

#[test]
fn the_evaluation_report_is_deterministic() {
    // Replay matters more than the numbers: the same synthetic state must
    // produce a byte-identical report, or two runs cannot be compared.
    let first = serde_json::to_string(&run_all()).expect("serialize first run");
    let second = serde_json::to_string(&run_all()).expect("serialize second run");
    assert_eq!(
        first, second,
        "the long-range evaluation must be replayable"
    );
}
