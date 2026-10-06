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
//! * an update case returns the whole timeline but no supersession signal —
//!   the failure the temporal/causal links of #105 exist to fix;
//! * retrieval order follows *record* time, not *event* time — the
//!   temporal-order error class;
//! * expired memory never leaks while newer memory survives;
//! * the viewer's timeline ranks ahead of irrelevant same-topic memory and
//!   results stay node/byte bounded;
//! * provenance class survives retrieval unchanged and is never promoted,
//!   and memory that failed the gate cannot enter at all.
//!
//! When bounded links land, the same cases gain link-aware expectations;
//! these baselines are the numbers they must improve.

use aivtuber_adaptation::{
    ActorPseudonymizer, MemoryGateDecision, MemoryQuery, RetentionClass, WorkingMemory,
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
        let event = event(spec.id, spec.source_class, spec.trust, spec.actor);
        let authority = (spec.source_class != SourceClass::System).then_some(&self.admin);
        let (decision, permit) = self.security.authorize_memory_write(&event, authority);
        assert_ne!(
            decision,
            MemoryWriteDecision::Denied,
            "case memory {:?} must pass the gate",
            spec.id
        );
        let permit = permit.expect("allowed write carries a permit");
        self.memory
            .remember_durable(&permit, spec.claim, spec.topic, spec.recorded_at_ms)
            .expect("durable entry");
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
    }
}

#[test]
fn an_update_case_returns_the_whole_timeline_but_no_supersession_signal() {
    let mut eval = Eval::new(WorkingMemoryConfig::default());
    // The issue's important example: A — the viewer disliked game X; later,
    // B — finished game X and enjoyed the ending.
    let old = viewer("evt-dislike", "viewer disliked game x", "game-x", 1_000);
    let new = viewer(
        "evt-enjoy",
        "viewer enjoyed the game x ending",
        "game-x",
        2_000,
    );
    eval.remember(&old);
    eval.remember(&new);

    let results = eval.query(Some("viewer-1"), Some("game-x"), 10, 3_000);
    // The whole timeline is retrievable, newest first.
    assert_eq!(results.len(), 2, "both claims are part of the timeline");
    assert!(results[0].normalized_claim.contains("enjoyed"));
    assert!(results[1].normalized_claim.contains("disliked"));

    // Detection: nothing in either result distinguishes the superseded claim
    // from the current one. Serialize both and pin that no staleness,
    // supersession, or relation field exists — if a link structure ever adds
    // one, this baseline flips and the case gains link-aware expectations.
    let keys: BTreeSet<String> = results
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
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
        keys.iter().all(|key| {
            !key.contains("supersed")
                && !key.contains("stale")
                && !key.contains("update")
                && !key.contains("relation")
                && !key.contains("link")
        }),
        "current retrieval carries no supersession signal: {keys:?}"
    );
    assert_eq!(results[0].retention, results[1].retention);
    assert_eq!(results[0].write_decision, results[1].write_decision);
    eprintln!("metric: supersession_signal=none, timeline_returned=2");
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
