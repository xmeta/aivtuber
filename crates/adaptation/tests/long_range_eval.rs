//! Long-range memory evaluation (issue #105) — synthetic, privacy-safe cases
//! in the spirit of LoCoMo/THEANINE, deterministic by construction.
//!
//! This is the baseline half of the mandated comparison ("compare current
//! retrieval against one external sidecar on synthetic
//! update/supersession/deletion cases", docs/implementation-accelerator.adoc).
//! Every memory is written through the real `SecurityRuntime` memory-write
//! gate before it reaches `WorkingMemory`, and the assertions pin what current
//! retrieval measurably does and does not provide:
//!
//! * an update case marks the superseded claim stale through an explicit
//!   supersession link while the whole timeline stays retrievable (the #105
//!   link structure; the original no-signal baseline flipped);
//! * same-topic derivation never invents staleness — a contradiction without
//!   update evidence stays current;
//! * retrieval order follows *record* time, not *event* time — the
//!   temporal-order error class;
//! * expired/evicted memory never leaks while newer memory survives, and
//!   every link touching it is pruned with it;
//! * the viewer's timeline ranks ahead of irrelevant same-topic memory and
//!   results stay node/byte bounded;
//! * provenance class survives retrieval unchanged and is never promoted,
//!   and memory that failed the gate cannot enter at all.
//!
//! Link creation, bounds, and determinism are measured here too: per-write
//! budgets, the store cap owned by the unified retention policy (#51), and
//! byte-identical links for identical inputs.

use aivtuber_adaptation::{
    ActorPseudonymizer, LinkedMemory, MEMORY_EPISTEMIC_VOCAB_VERSION, MemoryEpistemicClass,
    MemoryGateDecision, MemoryLinkKind, MemoryQuery, RetentionClass, WorkingMemory,
    WorkingMemoryConfig,
};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EventEnvelope, EventKind, LocalControlIngress, OperatorCommandInput, SecurityPlane,
    SourceClass, TrustLevel,
};
use aivtuber_runtime::{MemoryWriteDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const ADMIN_SECRET: [u8; 32] = [11_u8; 32];

/// Authenticated `memory.admin` authority, minted the production way
/// (local ingress authentication), so viewer-class claims can pass the gate
/// exactly as they would in a deployment.
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
                observed_at: "2026-10-06T00:00:00Z".to_owned(),
                action: "memory.admin".to_owned(),
                payload: BTreeMap::new(),
            },
            &ADMIN_SECRET,
        )
        .expect("authenticated")
        .authority()
        .clone()
}

fn event(
    id: &str,
    source_class: SourceClass,
    trust_level: TrustLevel,
    actor: Option<&str>,
) -> EventEnvelope {
    let (plane, kind, source) = match source_class {
        SourceClass::System => (
            SecurityPlane::System,
            EventKind::SystemHealth,
            "system-health",
        ),
        _ => (
            SecurityPlane::Content,
            EventKind::ChatMessage,
            "public-chat",
        ),
    };
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: id.to_owned(),
        correlation_id: format!("corr-{id}"),
        sequence: id.bytes().map(u64::from).sum(),
        observed_at: "2026-10-06T00:00:00Z".to_owned(),
        source: source.to_owned(),
        source_class,
        plane,
        trust_level,
        kind,
        actor_id: actor.map(str::to_owned),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([("text".to_owned(), Value::String("remember this".to_owned()))]),
    }
}

/// One synthetic memory as the case declares it. `event_at_ms` is ground
/// truth the store does *not* keep — measuring that gap is the point of the
/// temporal-order case.
struct Memory {
    id: &'static str,
    source_class: SourceClass,
    trust: TrustLevel,
    actor: Option<&'static str>,
    claim: &'static str,
    topic: Option<&'static str>,
    recorded_at_ms: u64,
    event_at_ms: u64,
    /// Explicit update evidence: the retained memory this claim replaces.
    /// Declared by the case (as the future writer would), never inferred from
    /// topic similarity alone.
    supersedes: Option<&'static str>,
}

struct Eval {
    security: SecurityRuntime,
    memory: WorkingMemory,
    admin: AuthenticatedControl,
}

impl Eval {
    fn new(config: WorkingMemoryConfig) -> Self {
        let security = SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security runtime");
        let pseudonymizer = ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("pseudonymizer");
        let memory = WorkingMemory::new(config, pseudonymizer).expect("working memory");
        Self {
            security,
            memory,
            admin: memory_admin(),
        }
    }

    /// Write one memory through the real gate: system sources authorize as
    /// themselves, viewer-class claims only through authenticated
    /// `memory.admin` authority. A denied event panics — a case memory that
    /// failed the gate must never be silently stored.
    fn remember(&mut self, spec: &Memory) {
        // The event carries the claim as its own text: a case that wants to
        // test an unbound claim must build the event differently, because a
        // permitted event that never carried the claim is not evidence for it.
        let mut event = event(spec.id, spec.source_class, spec.trust, spec.actor);
        event
            .payload
            .insert("text".to_owned(), Value::String(spec.claim.to_owned()));
        let authority = (spec.source_class != SourceClass::System).then_some(&self.admin);
        let (decision, permit) = self.security.authorize_memory_write(&event, authority);
        assert_ne!(
            decision,
            MemoryWriteDecision::Denied,
            "case memory {:?} must pass the gate",
            spec.id
        );
        let permit = permit.expect("allowed write carries a permit");
        let written = match spec.supersedes {
            Some(target) => self.memory.remember_durable_superseding(
                &permit,
                spec.claim,
                spec.topic,
                spec.recorded_at_ms,
                &[target],
            ),
            None => {
                self.memory
                    .remember_durable(&permit, spec.claim, spec.topic, spec.recorded_at_ms)
            }
        };
        written.expect("durable entry");
    }

    fn query(
        &self,
        actor: Option<&str>,
        topic: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Vec<&aivtuber_adaptation::MemoryEntry> {
        self.memory.relevant(
            MemoryQuery {
                source_namespace: actor.map(|_| "public-chat"),
                actor_id: actor,
                topic,
                limit,
            },
            now_ms,
        )
    }

    fn query_linked(
        &self,
        actor: Option<&str>,
        topic: Option<&str>,
        limit: usize,
        now_ms: u64,
    ) -> Vec<LinkedMemory<'_>> {
        self.memory.relevant_linked(
            MemoryQuery {
                source_namespace: actor.map(|_| "public-chat"),
                actor_id: actor,
                topic,
                limit,
            },
            now_ms,
        )
    }
}

fn viewer(id: &'static str, claim: &'static str, topic: &'static str, at_ms: u64) -> Memory {
    Memory {
        id,
        source_class: SourceClass::PublicChat,
        trust: TrustLevel::Untrusted,
        actor: Some("viewer-1"),
        claim,
        topic: Some(topic),
        recorded_at_ms: at_ms,
        event_at_ms: at_ms,
        supersedes: None,
    }
}

#[test]
fn an_update_case_marks_the_superseded_claim_stale_and_keeps_the_whole_timeline() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    // The issue's important example: A — the viewer disliked game X; later,
    // B — finished game X and enjoyed the ending. The writer declares B's
    // update evidence through the permit path (the case owns that evidence,
    // exactly as the future conversation writer would).
    let old = viewer("evt-dislike", "viewer disliked game x", "game-x", 1_000);
    let mut new = viewer(
        "evt-enjoy",
        "viewer enjoyed the game x ending",
        "game-x",
        2_000,
    );
    new.supersedes = Some("evt-dislike");
    eval.remember(&old);
    eval.remember(&new);

    let results = eval.query_linked(Some("viewer-1"), Some("game-x"), 10, 3_000);
    // The whole timeline is retrievable, newest first — supersession marks a
    // claim, it never hides it.
    assert_eq!(results.len(), 2, "both claims are part of the timeline");
    assert!(results[0].entry.normalized_claim.contains("enjoyed"));
    assert!(results[1].entry.normalized_claim.contains("disliked"));

    // The link-aware signal: the old claim is stale because a *retained*
    // memory explicitly supersedes it, the current claim is not, and the
    // supersession edge itself is visible in the bounded link view.
    assert!(
        results[1].stale,
        "the superseded claim is marked stale, not silently dropped"
    );
    assert!(!results[0].stale, "the current claim is not stale");
    assert!(
        results[0].links.iter().any(|link| {
            link.kind == MemoryLinkKind::Supersedes
                && link.from == results[0].entry.memory_id
                && link.to == results[1].entry.memory_id
        }),
        "the supersession edge is visible: {:?}",
        results[0].links
    );

    // Both artifacts serialize with the link-aware fields — the flip of the
    // original baseline, which pinned their absence.
    let keys: BTreeSet<String> = results
        .iter()
        .filter_map(|linked| serde_json::to_value(linked).ok())
        .filter_map(|value| {
            value.as_object().map(|object| {
                object
                    .keys()
                    .map(|key| key.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect();
    assert!(
        keys.iter().any(|key| key.contains("stale")) && keys.iter().any(|key| key.contains("link")),
        "link-aware retrieval carries staleness and links: {keys:?}"
    );
    // Provenance is untouched by linkage (threat model §8): the edge never
    // promotes or rewrites the claim's class.
    assert_eq!(results[0].entry.retention, results[1].entry.retention);
    assert_eq!(
        results[0].entry.write_decision,
        results[1].entry.write_decision
    );
    eprintln!("metric: supersession_signal=present, stale_marked=1, timeline_returned=2");
}

#[test]
fn a_contradiction_without_update_evidence_stays_current() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    // "A contradiction that is not actually an update": two same-topic
    // claims with no declared update evidence. The deterministic same-topic
    // spine may link them temporally, but linkage alone must never invent
    // staleness (threat model §8: graph linkage is not authority).
    let first = viewer("evt-claim-a", "viewer prefers game x", "game-x", 1_000);
    let second = viewer(
        "evt-claim-b",
        "viewer prefers another game",
        "game-x",
        2_000,
    );
    eval.remember(&first);
    eval.remember(&second);

    let results = eval.query_linked(Some("viewer-1"), Some("game-x"), 10, 3_000);
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|linked| !linked.stale),
        "same-topic derivation alone never marks a claim stale"
    );
    assert!(
        results.iter().all(|linked| linked
            .links
            .iter()
            .any(|link| link.kind == MemoryLinkKind::TemporalBefore)),
        "the deterministic temporal spine still links the timeline"
    );
}

#[test]
fn retrieval_order_follows_record_time_not_event_time() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    // Ground-truth event chronology: started game Y (day 1) before finished
    // game X (day 2). The older event was recalled into memory *later*, so
    // record time contradicts event time.
    let mut started = viewer("evt-start-y", "started playing game y", "game-y", 1_500);
    started.event_at_ms = 100;
    let mut finished = viewer("evt-finish-x", "finished game x", "game-x", 1_000);
    finished.event_at_ms = 900;
    eval.remember(&started);
    eval.remember(&finished);

    let results = eval.query(None, None, 10, 2_000);
    let order: Vec<u64> = results
        .iter()
        .map(|entry| match entry.normalized_claim.as_str() {
            "started playing game y" => 100,
            "finished game x" => 900,
            other => panic!("unexpected claim {other:?}"),
        })
        .collect();
    // Retrieval ranks by record time: the day-1 event comes back after the
    // day-2 event, contradicting the event chronology.
    assert_eq!(order, vec![100, 900], "record-time order is inverted");
    let temporal_order_errors = usize::from(order[0] < order[1]);
    assert!(
        temporal_order_errors >= 1,
        "the baseline must detect the temporal-order failure class"
    );
    eprintln!("metric: temporal_order_errors={temporal_order_errors}");
}

#[test]
fn expired_memory_never_leaks_while_newer_memory_survives() {
    let mut eval = Eval::new(WorkingMemoryConfig {
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    let mut old = viewer("evt-old", "old opinion about game x", "game-x", 100);
    old.event_at_ms = 100;
    let new = viewer("evt-new", "current opinion about game x", "game-x", 1_500);
    eval.remember(&old);

    // Past the old entry's TTL: filtered from retrieval even before
    // compaction removes it.
    let results = eval.query(Some("viewer-1"), Some("game-x"), 10, 1_400);
    assert!(results.is_empty(), "expired memory is not retrievable");

    let record = eval.memory.compact(1_400);
    assert_eq!(record.expired_removed, 1);
    // Deletion removes the content itself, not just the reference: the store
    // no longer serializes the expired claim anywhere.
    let serialized = serde_json::to_string(eval.memory.entries()).expect("serialize store");
    assert!(
        !serialized.contains("old opinion about game x"),
        "expired content is gone from the store"
    );

    eval.remember(&new);
    let results = eval.query(Some("viewer-1"), Some("game-x"), 10, 1_600);
    assert_eq!(results.len(), 1, "newer memory survives expiry");
    assert_eq!(results[0].retention, RetentionClass::Durable);
}

#[test]
fn viewer_timeline_ranks_ahead_of_irrelevant_same_topic_memory_and_stays_bounded() {
    let mut eval = Eval::new(WorkingMemoryConfig {
        max_entries: 8,
        ..WorkingMemoryConfig::default()
    });
    let relevant_new = viewer("evt-a", "viewer1 loved the boss fight", "game-x", 3_000);
    let relevant_old = viewer("evt-c", "viewer1 disliked the tutorial", "game-x", 1_000);
    let mut irrelevant = viewer("evt-b", "viewer2 loved the game", "game-x", 2_000);
    irrelevant.actor = Some("viewer-2");
    eval.remember(&relevant_new);
    eval.remember(&relevant_old);
    eval.remember(&irrelevant);

    // Unbounded-ish query: the irrelevant same-topic memory is returned too,
    // so timeline precision is 2/3 — measured, not hidden.
    let all = eval.query(Some("viewer-1"), Some("game-x"), 10, 4_000);
    assert_eq!(all.len(), 3);
    let viewer1_pseudonym = ActorPseudonymizer::new("test-v1", [0x42; 32])
        .expect("pseudonymizer")
        .pseudonymize("public-chat", "viewer-1");
    let viewer1 = all
        .iter()
        .filter(|entry| {
            entry.source.pseudonymous_actor_id.as_deref() == Some(viewer1_pseudonym.as_str())
        })
        .count();
    assert_eq!(viewer1, 2, "precision: 2 of 3 results are the viewer's");
    assert_eq!(
        all[0].normalized_claim, "viewer1 loved the boss fight",
        "viewer match outranks topic-only match, newest first"
    );
    eprintln!("metric: timeline_precision=2/3");

    // The node/byte budget is enforced by the query itself.
    let bounded = eval.query(Some("viewer-1"), Some("game-x"), 1, 4_000);
    assert_eq!(bounded.len(), 1, "node bound respected");
    assert!(
        bounded
            .iter()
            .all(|entry| entry.normalized_claim.len() <= eval.memory.config().max_claim_bytes)
    );
}

#[test]
fn provenance_class_survives_retrieval_and_gate_denied_memory_never_enters() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    // A viewer claim (public chat, untrusted, allowed only via memory.admin)
    // and a system observation (allowed as system source).
    let mut claim = viewer(
        "evt-claim",
        "viewer claims the author is x",
        "game-x",
        1_000,
    );
    claim.trust = TrustLevel::Untrusted;
    eval.remember(&claim);
    let observation = Memory {
        id: "evt-system",
        source_class: SourceClass::System,
        trust: TrustLevel::Trusted,
        actor: None,
        claim: "system observed a stable stream",
        topic: Some("health"),
        recorded_at_ms: 1_100,
        event_at_ms: 1_100,
        supersedes: None,
    };
    eval.remember(&observation);

    let results = eval.query(None, None, 10, 2_000);
    assert_eq!(results.len(), 2);
    let viewer_claim = results
        .iter()
        .find(|entry| entry.source.event_id == "evt-claim")
        .expect("viewer claim retrievable");
    assert_eq!(viewer_claim.source.source_class, SourceClass::PublicChat);
    assert_eq!(viewer_claim.source.trust_level, TrustLevel::Untrusted);
    assert_eq!(
        viewer_claim.write_decision,
        MemoryGateDecision::AllowedMemoryAdmin
    );
    let system = results
        .iter()
        .find(|entry| entry.source.event_id == "evt-system")
        .expect("system observation retrievable");
    assert_eq!(system.source.source_class, SourceClass::System);
    assert_eq!(
        system.write_decision,
        MemoryGateDecision::AllowedSystemSource
    );

    // Provenance, not the writing authority, decides the epistemic class: the
    // admin permit made the viewer claim *storable*, it did not make it a
    // fact, and it must survive retrieval as a claim (#105 §3.1).
    assert_eq!(
        viewer_claim.epistemic_class,
        MemoryEpistemicClass::ViewerClaim,
        "an authenticated write of public content is still a viewer claim"
    );
    assert_eq!(
        system.epistemic_class,
        MemoryEpistemicClass::VerifiedFact,
        "a trusted system observation is the only verified fact"
    );
    let view = serde_json::to_value(
        eval.memory
            .relevant_linked(
                MemoryQuery {
                    source_namespace: None,
                    actor_id: None,
                    topic: Some("health"),
                    limit: 4,
                },
                2_000,
            )
            .first()
            .expect("system entry view"),
    )
    .expect("serialize the retrieval view");
    assert_eq!(
        view["entry"]["epistemic_class"],
        Value::String("verified_fact".to_owned()),
        "the class is serialized with the retrieved entry: {view}"
    );
    assert_eq!(
        view["epistemic_version"],
        Value::String(MEMORY_EPISTEMIC_VOCAB_VERSION.to_owned()),
        "the view names the epistemic vocabulary rather than implying it"
    );

    // The gate is the only way in: public chat without authority is denied
    // and cannot reach the store.
    let before = eval.memory.len();
    let denied_event = event(
        "evt-denied",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-1"),
    );
    let (decision, permit) = eval.security.authorize_memory_write(&denied_event, None);
    assert_eq!(decision, MemoryWriteDecision::Denied);
    assert!(permit.is_none(), "no permit, no durable write");
    assert_eq!(eval.memory.len(), before);
}

#[test]
fn deleting_a_memory_removes_every_link_that_touched_it() {
    // Capacity eviction stands in for any deletion path: the superseded
    // memory leaves the store, and nothing about it may survive anywhere —
    // neither its content nor its id inside an edge (threat model §8: an edge
    // must not become a covert archive of removed content).
    let mut eval = Eval::new(WorkingMemoryConfig {
        max_entries: 1,
        ..WorkingMemoryConfig::default()
    });
    let old = viewer("evt-superseded", "private old opinion", "game-x", 1_000);
    eval.remember(&old);
    let mut new = viewer("evt-current", "current opinion", "game-x", 2_000);
    new.supersedes = Some("evt-superseded");
    eval.remember(&new);

    assert_eq!(eval.memory.len(), 1, "capacity evicted the old claim");
    assert!(
        eval.memory.links().is_empty(),
        "both edges died with their endpoint"
    );
    let store = serde_json::to_string(eval.memory.entries()).expect("serialize entries");
    let links = serde_json::to_string(&eval.memory.link_snapshot(3_000)).expect("serialize links");
    // The replay artifact names its own edge vocabulary, so a future
    // vocabulary change is identifiable instead of silently reinterpreted.
    assert!(
        links.contains(aivtuber_adaptation::MEMORY_LINK_VOCAB_VERSION),
        "the serialized link store carries its vocabulary version: {links}"
    );
    for serialized in [store, links] {
        assert!(
            !serialized.contains("private old opinion"),
            "removed content is unreachable"
        );
        assert!(
            !serialized.contains("evt-superseded"),
            "no dangling reference to the removed memory"
        );
    }
    // The survivor is not stale: nothing *retained* supersedes it anymore.
    let results = eval.query_linked(Some("viewer-1"), Some("game-x"), 10, 3_000);
    assert_eq!(results.len(), 1);
    assert!(!results[0].stale);
    assert!(results[0].links.is_empty());
}

#[test]
fn link_creation_respects_the_per_write_budget_and_the_store_cap() {
    let mut eval = Eval::new(WorkingMemoryConfig {
        max_links: 3,
        max_links_per_memory: 2,
        ..WorkingMemoryConfig::default()
    });
    // Four same-topic writes: 0 + 1 + 2 + 2 links created = 5, so the store
    // cap of 3 evicts oldest-first, deterministically.
    eval.remember(&viewer("evt-b0", "claim b0", "game-x", 1_000));
    eval.remember(&viewer("evt-b1", "claim b1", "game-x", 1_100));
    eval.remember(&viewer("evt-b2", "claim b2", "game-x", 1_200));
    eval.remember(&viewer("evt-b3", "claim b3", "game-x", 1_300));
    assert_eq!(
        eval.memory.links().len(),
        3,
        "store cap enforced: {:?}",
        eval.memory.links()
    );
    let metrics = eval.memory.retention_metrics();
    assert_eq!(metrics.links, 3);
    assert_eq!(metrics.links_high_water, 3);

    // Per-write budget: one write may not declare more supersessions than
    // `max_links_per_memory`, and the refusal stores nothing.
    let attempt = event(
        "evt-overflow",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-1"),
    );
    let (decision, permit) = eval
        .security
        .authorize_memory_write(&attempt, Some(&eval.admin));
    assert_ne!(decision, MemoryWriteDecision::Denied);
    let permit = permit.expect("allowed write carries a permit");
    let error = eval
        .memory
        .remember_durable_superseding(
            &permit,
            "claim overflow",
            Some("game-x"),
            1_400,
            &["evt-b0", "evt-b1", "evt-b2"],
        )
        .expect_err("the budget bounds one write's link creation");
    assert!(
        error.to_string().contains("per-memory link budget"),
        "{error}"
    );
    assert_eq!(eval.memory.len(), 4, "a refused write stores nothing");
    assert!(
        eval.memory
            .entries()
            .iter()
            .all(|entry| entry.source.event_id != "evt-overflow")
    );
}

#[test]
fn a_supersession_target_that_is_not_retained_durable_is_refused() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    eval.remember(&viewer("evt-keep", "kept claim", "game-x", 1_000));
    // Working-only state (#80 territory) never becomes a link endpoint: it is
    // not retained episodic memory, so nothing can supersede it.
    eval.memory
        .remember_working(
            &event(
                "evt-working",
                SourceClass::PublicChat,
                TrustLevel::Untrusted,
                Some("viewer-1"),
            ),
            "active thread claim",
            Some("game-x"),
            500,
        )
        .expect("working entry");
    assert!(
        eval.memory.links().is_empty(),
        "working memory never enters the graph"
    );

    let attempt = event(
        "evt-update",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-1"),
    );
    for target in ["evt-working", "evt-unknown"] {
        let (decision, permit) = eval
            .security
            .authorize_memory_write(&attempt, Some(&eval.admin));
        assert_ne!(decision, MemoryWriteDecision::Denied);
        let permit = permit.expect("allowed write carries a permit");
        let error = eval
            .memory
            .remember_durable_superseding(
                &permit,
                "updated claim",
                Some("game-x"),
                2_000,
                &[target],
            )
            .expect_err("only retained durable memories are link endpoints");
        assert!(
            error.to_string().contains("retained durable memory"),
            "{error}"
        );
    }
    // Refused writes stored nothing: the durable claim and the working entry
    // are the only memories here.
    assert_eq!(eval.memory.len(), 2);
    assert!(
        eval.memory
            .entries()
            .iter()
            .all(|entry| entry.source.event_id != "evt-update")
    );
    assert!(eval.memory.links().is_empty());
}

#[test]
fn identical_inputs_produce_identical_links_and_entries() {
    fn build() -> WorkingMemory {
        let mut eval = Eval::new(WorkingMemoryConfig::default());
        eval.remember(&viewer("evt-d1", "claim one", "game-x", 1_000));
        let mut update = viewer("evt-d2", "claim two", "game-x", 2_000);
        update.supersedes = Some("evt-d1");
        eval.remember(&update);
        eval.remember(&viewer("evt-d3", "other topic claim", "game-y", 3_000));
        eval.memory
    }

    let left = build();
    let right = build();
    assert_eq!(left.entries(), right.entries(), "replay-stable store");
    assert_eq!(left.links(), right.links(), "replay-stable link decisions");
    assert!(!left.links().is_empty(), "the fixture produced links");
}
