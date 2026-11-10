//! RAM-only targeted erase (#305, ADR-0008 S1) — synthetic conformance cases.
//!
//! Every permit is minted by the real `SecurityRuntime` (operator: authenticated
//! `memory.admin`; viewer: the requester's own chat envelope), and every
//! memory is written through the real memory-write gate.

use aivtuber_adaptation::{
    ActorPseudonymizer, ERASURE_VOCAB_VERSION, ErasureOutcome, ErasureRemnant, ErasureRequester,
    ErasureScopeKind, ErasureState, ErasureStatus, MAX_ERASE_RATE_ENTRIES, MAX_ERASURE_REPORTS,
    MemoryQuery, VIEWER_ERASE_MIN_INTERVAL_MS, WorkingMemory, WorkingMemoryConfig,
};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EventEnvelope, EventKind, LocalControlIngress, OperatorCommandInput, SecurityPlane,
    SourceClass, TrustLevel,
};
use aivtuber_runtime::{
    MemoryErasePermit, SecurityRuntime, SecurityRuntimeConfig, VIEWER_ERASE_COMMAND,
    ViewerEraseDecision,
};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const SECRET: [u8; 32] = [11_u8; 32];
const NS: &str = "public-chat";

fn admin() -> AuthenticatedControl {
    let ingress = LocalControlIngress::new(
        "erasure-eval",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::MemoryAdmin]),
        ControlSecret::new(SECRET),
    )
    .expect("ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-erasure-admin".to_owned(),
                correlation_id: "corr-erasure-admin".to_owned(),
                sequence: 1,
                observed_at: "2026-11-10T00:00:00Z".to_owned(),
                action: "memory.admin".to_owned(),
                payload: BTreeMap::new(),
            },
            &SECRET,
        )
        .expect("authenticated")
        .authority()
        .clone()
}

fn chat(id: &str, actor: Option<&str>, text: &str) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: id.to_owned(),
        correlation_id: format!("corr-{id}"),
        sequence: id.bytes().map(u64::from).sum(),
        observed_at: "2026-11-10T00:00:00Z".to_owned(),
        source: NS.to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: actor.map(str::to_owned),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([("text".to_owned(), Value::String(text.to_owned()))]),
    }
}

struct Harness {
    security: SecurityRuntime,
    memory: WorkingMemory,
    admin: AuthenticatedControl,
}

impl Harness {
    fn new() -> Self {
        let security = SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security");
        let memory = WorkingMemory::new(
            WorkingMemoryConfig::default(),
            ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("pseudonymizer"),
        )
        .expect("memory");
        Self {
            security,
            memory,
            admin: admin(),
        }
    }

    /// A durable viewer claim through the real gate (authenticated admin).
    fn remember(&mut self, id: &str, actor: &str, claim: &str, topic: &str, at_ms: u64) -> u64 {
        let event = chat(id, Some(actor), claim);
        let (_, permit) = self
            .security
            .authorize_memory_write(&event, Some(&self.admin));
        self.memory
            .remember_durable(&permit.expect("permit"), claim, Some(topic), at_ms)
            .expect("durable")
            .memory_id
    }

    fn visible(&self, actor: &str, now_ms: u64) -> usize {
        self.memory
            .relevant(
                MemoryQuery {
                    source_namespace: Some(NS),
                    actor_id: Some(actor),
                    topic: None,
                    limit: 32,
                },
                now_ms,
            )
            .len()
    }

    fn operator_permit(
        &mut self,
        request: &str,
        actor: Option<&str>,
        ids: &[u64],
    ) -> MemoryErasePermit {
        self.security
            .authorize_operator_memory_erase(
                request,
                &self.admin,
                actor.map(|actor| (NS, actor)),
                ids,
            )
            .expect("operator permit")
    }

    fn erase_op(
        &mut self,
        request: &str,
        actor: Option<&str>,
        ids: &[u64],
        at_ms: u64,
    ) -> aivtuber_adaptation::ErasureReport {
        let permit = self.operator_permit(request, actor, ids);
        self.memory.erase(&permit, at_ms).expect("erase")
    }

    fn viewer_permit(&mut self, event_id: &str, actor: &str) -> MemoryErasePermit {
        let event = chat(event_id, Some(actor), VIEWER_ERASE_COMMAND);
        match self.security.authorize_viewer_memory_erase(&event) {
            ViewerEraseDecision::Permitted(permit) => permit,
            other => panic!("expected a permit, got {other:?}"),
        }
    }
}

fn seeded() -> Harness {
    let mut harness = Harness::new();
    harness.remember("a-1", "viewer-a", "a likes Rust", "lang", 10);
    harness.remember("a-2", "viewer-a", "a plays on Linux", "lang", 20);
    harness.remember("b-1", "viewer-b", "b likes Go", "lang", 30);
    harness
}

#[test]
fn read_visibility_is_revoked_before_anything_is_removed() {
    let mut h = seeded();
    assert_eq!(h.visible("viewer-a", 100), 2);
    let before_b = h.visible("viewer-b", 100);
    let permit = h.operator_permit("req-1", Some("viewer-a"), &[]);

    let status = h.memory.begin_erase(&permit, 100).expect("begin");
    assert_eq!(status, ErasureStatus::Pending(ErasureState::ReadRevoked));
    // Canonical entries are still physically present...
    assert_eq!(h.memory.len(), 3);
    // ...but no read path returns the revoked scope.
    assert_eq!(h.visible("viewer-a", 100), 0);
    let context = h
        .memory
        .context_for(NS, Some("viewer-a"), Some("lang"), 8, 100);
    assert!(
        context
            .iter()
            .all(|entry| entry.source.event_id.starts_with("b-")),
        "context_for must not return revoked claims: {context:?}"
    );
    let linked = h.memory.relevant_linked(
        MemoryQuery {
            source_namespace: Some(NS),
            actor_id: Some("viewer-a"),
            topic: Some("lang"),
            limit: 8,
        },
        100,
    );
    assert!(
        linked
            .iter()
            .all(|item| item.entry.source.event_id.starts_with("b-"))
    );
    assert!(
        h.memory
            .link_snapshot(100)
            .links
            .iter()
            .all(|link| link.from != 1 && link.to != 1 && link.from != 2 && link.to != 2),
        "edges touching revoked nodes are not visible"
    );
    // Another actor's visibility is untouched.
    assert_eq!(h.visible("viewer-b", 100), before_b);
    assert_eq!(h.memory.retention_metrics().erasures_pending, 1);
}

#[test]
fn every_state_fault_resumes_idempotently_with_reads_closed() {
    // A fault injected after each state leaves the scope revoked, and resuming
    // (from a repeated begin with the same request id) reaches the same
    // report as an uninterrupted run.
    let mut reference = seeded();
    let permit = reference.operator_permit("req", Some("viewer-a"), &[]);
    let expected = reference.memory.erase(&permit, 500).expect("erase");

    for steps in 0..=5_usize {
        let mut h = seeded();
        let permit = h.operator_permit("req", Some("viewer-a"), &[]);
        h.memory.begin_erase(&permit, 500).expect("begin");
        for _ in 0..steps {
            h.memory.advance_erase("req", 500).expect("advance");
        }
        assert_eq!(h.visible("viewer-a", 600), 0, "closed after {steps} steps");
        // "Process resumes": the same request is re-submitted and finished.
        let resumed = h.memory.erase(&permit, 500).expect("resume");
        assert_eq!(resumed, expected, "resumed from step {steps}");
        assert!(h.memory.pending_erasure_ids().is_empty());
        assert_eq!(h.memory.len(), 1, "only viewer-b's entry remains");
    }
}

#[test]
fn erase_is_idempotent_and_an_unknown_scope_is_a_reported_noop() {
    let mut h = seeded();
    let first = h.erase_op("req-1", Some("viewer-a"), &[], 100);
    assert_eq!(first.outcome, ErasureOutcome::Erased);
    assert_eq!(first.entries_removed, 2);

    // Same request id again: the retained report, no second transaction.
    let permit = h.operator_permit("req-1", Some("viewer-a"), &[]);
    assert_eq!(h.memory.erase(&permit, 999).expect("replay"), first);

    // A fresh request for the same scope removes nothing.
    let again = h.erase_op("req-2", Some("viewer-a"), &[], 200);
    assert_eq!(again.outcome, ErasureOutcome::NoOp);
    assert_eq!((again.entries_removed, again.links_removed), (0, 0));
    assert!(again.verified);

    // An actor that never wrote is a reported no-op as well.
    let unknown = h.erase_op("req-3", Some("nobody"), &[], 300);
    assert_eq!(unknown.outcome, ErasureOutcome::NoOp);
    assert_eq!(h.memory.len(), 1);
}

#[test]
fn erase_leaves_no_entry_edge_or_context_for_the_scope() {
    let mut h = seeded();
    let report = h.erase_op("req", Some("viewer-a"), &[], 100);
    assert_eq!(report.outcome, ErasureOutcome::Erased);
    assert!(
        report.links_removed >= 1,
        "the temporal edge a-1 -> a-2 is pruned"
    );
    for entry in h.memory.entries() {
        assert!(entry.source.event_id.starts_with("b-"));
    }
    let alive: BTreeSet<u64> = h.memory.entries().iter().map(|e| e.memory_id).collect();
    for link in h.memory.links() {
        assert!(alive.contains(&link.from) && alive.contains(&link.to));
    }
    assert!(
        h.memory
            .context_for(NS, Some("viewer-a"), Some("lang"), 8, 100)
            .is_empty()
    );
    // A new write after the erase is a new, readable claim.
    h.remember("a-3", "viewer-a", "a is back", "lang", 150);
    assert_eq!(h.visible("viewer-a", 200), 1);
}

#[test]
fn viewer_can_erase_only_their_own_pseudonym() {
    let mut h = seeded();
    // viewer-b asks; the scope comes from the envelope, not the text.
    let permit = h.viewer_permit("evt-forget-b", "viewer-b");
    let report = h.memory.erase(&permit, 100).expect("erase");
    assert_eq!(report.requester, ErasureRequester::Viewer);
    assert_eq!(report.scope_kind, ErasureScopeKind::Actor);
    assert_eq!(report.entries_removed, 1);
    assert_eq!(h.visible("viewer-a", 200), 2, "viewer-a is untouched");
    assert_eq!(h.visible("viewer-b", 200), 0);

    // The same raw actor id in another namespace is a different pseudonym.
    let mut other_ns = chat("a-x", Some("viewer-a"), "other platform");
    other_ns.source = "other-platform".to_owned();
    let (_, write) = h.security.authorize_memory_write(&other_ns, Some(&h.admin));
    h.memory
        .remember_durable(&write.expect("permit"), "other platform", Some("lang"), 120)
        .expect("durable");
    let permit = h.viewer_permit("evt-forget-a", "viewer-a");
    let report = h.memory.erase(&permit, 130).expect("erase");
    assert_eq!(report.entries_removed, 2, "only the public-chat namespace");
    assert_eq!(h.memory.len(), 1);
    assert_eq!(h.memory.entries()[0].source.source, "other-platform");
}

#[test]
fn viewer_requests_are_rate_limited_per_pseudonym_on_logical_time() {
    let mut h = seeded();
    let p1 = h.viewer_permit("evt-1", "viewer-a");
    let first = h.memory.erase(&p1, 1_000).expect("erase");
    assert_eq!(first.outcome, ErasureOutcome::Erased);

    h.remember("a-3", "viewer-a", "a again", "lang", 1_500);
    let p2 = h.viewer_permit("evt-2", "viewer-a");
    let limited = h
        .memory
        .erase(&p2, 1_000 + VIEWER_ERASE_MIN_INTERVAL_MS - 1)
        .expect("erase");
    assert_eq!(limited.outcome, ErasureOutcome::RateLimited);
    assert_eq!(limited.entries_removed, 0);
    assert!(!limited.verified);
    assert_eq!(
        h.visible("viewer-a", 2_000),
        1,
        "a limited request changes nothing"
    );
    assert!(h.memory.pending_erasure_ids().is_empty());

    // Another pseudonym is independent.
    let pb = h.viewer_permit("evt-3", "viewer-b");
    assert_eq!(
        h.memory.erase(&pb, 1_001).expect("erase").outcome,
        ErasureOutcome::Erased
    );

    // After the window the same viewer is admitted again.
    let p3 = h.viewer_permit("evt-4", "viewer-a");
    let allowed = h
        .memory
        .erase(&p3, 1_000 + VIEWER_ERASE_MIN_INTERVAL_MS)
        .expect("erase");
    assert_eq!(allowed.outcome, ErasureOutcome::Erased);
    assert_eq!(h.memory.retention_metrics().erasures_rate_limited, 1);
}

#[test]
fn operator_erase_is_not_viewer_rate_limited_and_can_target_entry_ids() {
    let mut h = seeded();
    let ids: Vec<u64> = h
        .memory
        .entries()
        .iter()
        .map(|entry| entry.memory_id)
        .collect();
    let by_id = h.erase_op("req-1", None, &ids[..1], 100);
    assert_eq!(by_id.requester, ErasureRequester::Operator);
    assert_eq!(by_id.scope_kind, ErasureScopeKind::MemoryIds);
    assert_eq!(by_id.entries_removed, 1);
    // Back-to-back operator requests are not limited by the viewer limiter.
    for n in 0..3 {
        let request = format!("req-actor-{n}");
        h.erase_op(&request, Some("viewer-a"), &[], 100);
    }
    assert_eq!(h.memory.retention_metrics().erasures_rate_limited, 0);

    let both = h.erase_op("req-both", Some("viewer-b"), &[999], 100);
    assert_eq!(both.scope_kind, ErasureScopeKind::ActorAndMemoryIds);
    assert_eq!(h.memory.len(), 0);
}

#[test]
fn chat_cannot_mint_an_erase_or_a_write_permit_for_anyone_else() {
    let mut h = Harness::new();
    // A viewer line that tries to name another actor is just content.
    let mut sneaky = chat("evt-sneaky", Some("viewer-a"), "!forgetme viewer-b");
    assert_eq!(
        h.security.authorize_viewer_memory_erase(&sneaky),
        ViewerEraseDecision::NotACommand
    );
    sneaky.payload.insert(
        "text".to_owned(),
        Value::String(VIEWER_ERASE_COMMAND.to_owned()),
    );
    // Operator erase needs the authenticated capability; a stop-only control
    // cannot mint one.
    let ingress = LocalControlIngress::new(
        "stop-only",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::PerformerStop]),
        ControlSecret::new(SECRET),
    )
    .expect("ingress");
    let stop = ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-stop".to_owned(),
                correlation_id: "corr-stop".to_owned(),
                sequence: 2,
                observed_at: "2026-11-10T00:00:00Z".to_owned(),
                action: "performer.stop".to_owned(),
                payload: BTreeMap::new(),
            },
            &SECRET,
        )
        .expect("authenticated");
    assert!(
        h.security
            .authorize_operator_memory_erase("req", stop.authority(), Some((NS, "viewer-a")), &[])
            .is_err()
    );
    // And the viewer command never yields a durable-write permit.
    let (decision, permit) = h.security.authorize_memory_write(&sneaky, None);
    assert!(permit.is_none(), "{decision:?}");
}

#[test]
fn report_lists_counts_and_every_unprovable_remnant_without_content() {
    let mut h = seeded();
    let report = h.erase_op("req", Some("viewer-a"), &[], 777);
    assert_eq!(report.vocab_version, ERASURE_VOCAB_VERSION);
    assert_eq!(report.completed_at_ms, 777);
    assert_eq!(report.entries_removed, 2);
    assert_eq!((report.caches_purged, report.proposals_purged), (0, 0));
    assert!(report.verified);
    assert_eq!(report.unprovable_remnants, ErasureRemnant::ALL.to_vec());
    for required in [
        ErasureRemnant::SidecarCopies,
        ErasureRemnant::HindsightRetireOnly,
        ErasureRemnant::GeneratedAssets,
        ErasureRemnant::ExternalBackupsAndProcessResidue,
    ] {
        assert!(report.unprovable_remnants.contains(&required));
    }

    let json = serde_json::to_string(&report).expect("serializes");
    for leaked in ["viewer-a", "likes Rust", "Linux", "actor:test-v1"] {
        assert!(!json.contains(leaked), "report leaks {leaked:?}: {json}");
    }
    let back: aivtuber_adaptation::ErasureReport = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back, report);
}

#[test]
fn erasure_is_deterministic_and_bounded() {
    let run = || {
        let mut h = seeded();
        let mut reports = Vec::new();
        reports.push(h.erase_op("r1", Some("viewer-a"), &[], 10));
        reports.push({
            let p = h.viewer_permit("v1", "viewer-b");
            h.memory.erase(&p, 20).expect("erase")
        });
        (reports, h.memory.retention_metrics())
    };
    assert_eq!(run(), run());

    // Reports and the rate table never grow past their bounds.
    let mut h = Harness::new();
    for n in 0..(MAX_ERASURE_REPORTS + 5) {
        let permit = h.operator_permit(&format!("r-{n}"), Some("nobody"), &[]);
        h.memory.erase(&permit, n as u64).expect("erase");
    }
    assert_eq!(h.memory.erasure_reports().len(), MAX_ERASURE_REPORTS);

    let mut h = Harness::new();
    for n in 0..MAX_ERASE_RATE_ENTRIES {
        let permit = h.viewer_permit(&format!("v-{n}"), &format!("viewer-{n}"));
        assert_eq!(
            h.memory.erase(&permit, 1).expect("erase").outcome,
            ErasureOutcome::NoOp
        );
    }
    // Table full inside the window: refuse rather than grow.
    let permit = h.viewer_permit("v-over", "viewer-over");
    assert_eq!(
        h.memory.erase(&permit, 2).expect("erase").outcome,
        ErasureOutcome::RateLimited
    );
    // Once the window has passed, aged-out records make room.
    let permit = h.viewer_permit("v-later", "viewer-over");
    assert_eq!(
        h.memory
            .erase(&permit, 1 + VIEWER_ERASE_MIN_INTERVAL_MS)
            .expect("erase")
            .outcome,
        ErasureOutcome::NoOp
    );
}

#[test]
fn erase_telemetry_counts_completed_removed_and_pending() {
    let mut h = seeded();
    let permit = h.operator_permit("req", Some("viewer-a"), &[]);
    h.memory.begin_erase(&permit, 1).expect("begin");
    assert_eq!(h.memory.retention_metrics().erasures_pending, 1);
    h.memory.erase(&permit, 1).expect("finish");
    let metrics = h.memory.retention_metrics();
    assert_eq!(metrics.erasures_pending, 0);
    assert_eq!(metrics.erasures_completed, 1);
    assert_eq!(metrics.erasure_entries_removed, 2);
    assert!(metrics.erasure_links_removed >= 1);
}
