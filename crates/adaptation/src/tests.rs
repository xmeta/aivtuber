use super::*;
use aivtuber_asset_store::{RuntimeCompatibility, load_asset_file};
use aivtuber_domain::{EVENT_SCHEMA_VERSION, EventKind, SecurityPlane, SourceClass, TrustLevel};
use aivtuber_runtime::{SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn event(
    id: &str,
    source_class: SourceClass,
    trust_level: TrustLevel,
    actor_id: Option<&str>,
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
        observed_at: "2026-09-25T00:00:00Z".to_owned(),
        source: source.to_owned(),
        source_class,
        plane,
        trust_level,
        kind,
        actor_id: actor_id.map(str::to_owned),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([("text".to_owned(), Value::String("remember this".to_owned()))]),
    }
}

fn security() -> SecurityRuntime {
    SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig::default(),
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime")
}

fn test_pseudonymizer() -> ActorPseudonymizer {
    ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("test pseudonymizer")
}

fn test_memory(config: WorkingMemoryConfig) -> WorkingMemory {
    WorkingMemory::new(config, test_pseudonymizer()).expect("working memory")
}

#[test]
fn link_bounds_must_be_positive() {
    let error = WorkingMemory::new(
        WorkingMemoryConfig {
            max_links: 0,
            ..WorkingMemoryConfig::default()
        },
        test_pseudonymizer(),
    )
    .expect_err("a zero link cap is no bound at all");
    assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
    let error = WorkingMemory::new(
        WorkingMemoryConfig {
            max_links_per_memory: 0,
            ..WorkingMemoryConfig::default()
        },
        test_pseudonymizer(),
    )
    .expect_err("a zero per-write budget is no bound at all");
    assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
}

#[test]
fn link_store_bound_must_cover_one_maximum_write() {
    // A store that cannot hold one maximum-size write would drop an
    // explicit supersession the very same write just had accepted, which
    // is not historical edge eviction — it is a lost caller-owned fact.
    let error = WorkingMemory::new(
        WorkingMemoryConfig {
            max_links: 1,
            max_links_per_memory: 2,
            ..WorkingMemoryConfig::default()
        },
        test_pseudonymizer(),
    )
    .expect_err("max_links must cover one maximum-size write");
    assert!(matches!(error, AdaptationError::InvalidConfiguration(_)));
}

/// Mint a permit and perform one durable write inside the same call: the
/// permit is a non-serializable capability for that gate pass, so it
/// never outlives the write it authorizes.
fn durable_write(
    memory: &mut WorkingMemory,
    security: &mut SecurityRuntime,
    id: &str,
    claim: &str,
    topic: Option<&str>,
    now_ms: u64,
) {
    durable_write_superseding(memory, security, id, claim, topic, now_ms, &[])
        .expect("durable remember");
}

fn durable_write_superseding(
    memory: &mut WorkingMemory,
    security: &mut SecurityRuntime,
    id: &str,
    claim: &str,
    topic: Option<&str>,
    now_ms: u64,
    supersedes: &[&str],
) -> Result<u64, AdaptationError> {
    // The event carries the claim as its own text, so the write is
    // genuinely bound to the permitted event (the tested case for a
    // verified fact). Unbound writes are covered separately.
    let mut source = event(id, SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(claim.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("system permit");
    memory
        .remember_durable_superseding(&permit, claim, topic, now_ms, supersedes)
        .map(|entry| entry.memory_id)
}

#[test]
fn generation_context_scopes_to_the_speaker_and_shared_memory() {
    // #105: a reply may use the speaker's own claims and actorless shared
    // memory, never a third party's claim — the failure mode that would
    // turn another viewer's sentence into this reply's "context".
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-shared",
        "the boss is weak to fire",
        Some("game-x"),
        1_000,
    );
    let own = event(
        "evt-own",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer:a"),
    );
    memory
        .remember_working(&own, "i like fire magic", Some("game-x"), 1_000)
        .expect("own claim");
    let other = event(
        "evt-other",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer:b"),
    );
    memory
        .remember_working(&other, "i hate fire magic", Some("game-x"), 1_000)
        .expect("other claim");

    let texts = |entries: Vec<&MemoryEntry>| {
        entries
            .iter()
            .map(|entry| entry.normalized_claim.clone())
            .collect::<Vec<_>>()
    };

    let scoped =
        texts(memory.context_for("public-chat", Some("viewer:a"), Some("game-x"), 8, 1_001));
    assert!(
        scoped.contains(&"the boss is weak to fire".to_owned()),
        "{scoped:?}"
    );
    assert!(
        scoped.contains(&"i like fire magic".to_owned()),
        "{scoped:?}"
    );
    assert!(
        !scoped.contains(&"i hate fire magic".to_owned()),
        "another actor's claim is out of scope: {scoped:?}"
    );
    // Equal record times break on the node id, newest first — the same
    // insertion-order timeline `relevant` uses, so the ordering is a
    // property of the retained state rather than of this call.
    assert_eq!(scoped[0], "i like fire magic");

    // An actorless event has no speaker to scope to: shared memory only.
    let shared_only = texts(memory.context_for("public-chat", None, Some("game-x"), 8, 1_001));
    assert_eq!(shared_only, vec!["the boss is weak to fire".to_owned()]);

    // The caller's limit bounds the result, and nothing survives expiry.
    assert_eq!(
        memory
            .context_for("public-chat", Some("viewer:a"), Some("game-x"), 1, 1_001)
            .len(),
        1
    );
    assert!(
        memory
            .context_for("public-chat", Some("viewer:a"), Some("game-x"), 8, u64::MAX)
            .is_empty()
    );
}

#[test]
fn generation_context_excludes_unattributable_and_private_memory() {
    // #105 review: "carries no actor" is not "safe to share". A public
    // claim nobody is attributable for, and an operator's private record,
    // are both valid actorless envelopes — and neither may reach another
    // party's reply.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let anonymous = event(
        "evt-anon",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        None,
    );
    memory
        .remember_working(
            &anonymous,
            "somebody hates this game",
            Some("game-x"),
            1_000,
        )
        .expect("anonymous public claim retained");
    let mut operator = event("evt-op", SourceClass::Operator, TrustLevel::Trusted, None);
    operator.kind = EventKind::OperatorCommand;
    operator.plane = SecurityPlane::Control;
    memory
        .remember_working(
            &operator,
            "the operator's private note",
            Some("game-x"),
            1_000,
        )
        .expect("private operator memory retained");
    durable_write(
        &mut memory,
        &mut security,
        "evt-shared",
        "the boss is weak to fire",
        Some("game-x"),
        1_000,
    );

    let viewer = memory
        .context_for("public-chat", Some("viewer:a"), Some("game-x"), 8, 1_001)
        .iter()
        .map(|entry| entry.normalized_claim.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        viewer,
        vec!["the boss is weak to fire".to_owned()],
        "only runtime-owned knowledge is shared; the anonymous claim and the private \
         record stay out"
    );
    let anonymous_event = memory
        .context_for("public-chat", None, Some("game-x"), 8, 1_001)
        .iter()
        .map(|entry| entry.normalized_claim.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        anonymous_event,
        vec!["the boss is weak to fire".to_owned()],
        "an actorless current event gets the shared set, not unattributable claims"
    );
}

#[test]
fn generation_context_replaces_a_superseded_claim_with_its_correction() {
    // #105 review: the context must not hand generation a claim an explicit
    // supersession already replaced — even when the correction moved to a
    // different topic, so that a tight budget cannot keep the old fact and
    // drop its replacement.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-old",
        "the boss is weak to fire",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-new",
        "the boss is weak to ice",
        Some("reload.boss"),
        1_001,
        &["evt-old"],
    )
    .expect("correction retained");

    let context = memory
        .context_for("system-health", None, Some("game-x"), 8, 1_002)
        .iter()
        .map(|entry| entry.normalized_claim.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        context,
        vec!["the boss is weak to ice".to_owned()],
        "the superseded claim is gone and its correction rides the match"
    );
    // The correction is what the *unrelated* topic query sees too, so the
    // pair can never present as two equally current facts.
    let unrelated = memory
        .context_for("system-health", None, Some("reload.boss"), 8, 1_002)
        .iter()
        .map(|entry| entry.normalized_claim.clone())
        .collect::<Vec<_>>();
    assert_eq!(unrelated, vec!["the boss is weak to ice".to_owned()]);
}

fn unfiltered_query(limit: usize) -> MemoryQuery<'static> {
    MemoryQuery {
        source_namespace: None,
        actor_id: None,
        topic: None,
        limit,
    }
}

fn retained_node(memory: &WorkingMemory, event_id: &str) -> u64 {
    memory
        .entries()
        .iter()
        .find(|entry| entry.source.event_id == event_id)
        .map(|entry| entry.memory_id)
        .expect("retained node")
}

/// A durable write from a system source that still carries an actor, so a
/// retrieval query can scope by that actor the way a viewer-scoped
/// lookup does.
fn durable_write_actor(
    memory: &mut WorkingMemory,
    security: &mut SecurityRuntime,
    id: &str,
    topic: Option<&str>,
    now_ms: u64,
    actor: &str,
    supersedes: &[&str],
) -> u64 {
    let claim = format!("claim from {id}");
    let mut source = event(id, SourceClass::System, TrustLevel::Trusted, Some(actor));
    source
        .payload
        .insert("text".to_owned(), Value::String(claim.clone()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("system permit");
    memory
        .remember_durable_superseding(&permit, &claim, topic, now_ms, supersedes)
        .expect("durable remember")
        .memory_id
}

#[test]
fn an_actor_scoped_query_never_expands_into_another_actors_claim() {
    // The derived temporal spine connects every durable entry that shares
    // a topic, so an unscoped hop would hand viewer 1's query a claim
    // viewer 2 wrote — unrelated viewer memory in retrieved context.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write_actor(
        &mut memory,
        &mut security,
        "evt-one",
        Some("game-x"),
        1_000,
        "viewer-1",
        &[],
    );
    durable_write_actor(
        &mut memory,
        &mut security,
        "evt-two",
        Some("game-x"),
        2_000,
        "viewer-2",
        &[],
    );
    durable_write_actor(
        &mut memory,
        &mut security,
        "evt-update",
        Some("game-x"),
        3_000,
        "viewer-2",
        &["evt-one"],
    );
    assert!(
        !memory.links().is_empty(),
        "the fixture produced both a temporal and a cross-actor supersession edge"
    );

    let scoped = memory.relevant_linked(
        MemoryQuery {
            source_namespace: Some("system-health"),
            actor_id: Some("viewer-1"),
            topic: None,
            limit: 10,
        },
        3_500,
    );
    assert_eq!(
        scoped.len(),
        1,
        "only the scoped actor's own claim is retrieved: {:?}",
        scoped
            .iter()
            .map(|item| item.entry.source.event_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(scoped[0].entry.source.event_id, "evt-one");
    // Scope bounds *content*, not verdicts or edge metadata: the edge
    // carries node ids only, and the staleness verdict still comes from
    // the full retained edge set.
    assert!(
        scoped[0].stale,
        "the cross-actor supersession still marks it"
    );
    assert!(
        !scoped[0].links.is_empty(),
        "the bounded view still names the relations"
    );

    // Without an actor scope the same edges are all reachable: the hop
    // only ever narrows to what the query asked for.
    let unscoped = memory.relevant_linked(unfiltered_query(10), 3_500);
    assert_eq!(unscoped.len(), 3, "every live node is retrievable unscoped");
}

#[test]
fn an_actor_and_topic_query_never_returns_another_actors_direct_match() {
    // With an actor *and* a topic supplied, `relevant` admits entries
    // matching either signal, so another viewer's same-topic claim ranks
    // as a direct match — the hop's actor check never sees it, and only
    // `relevant_linked`'s own re-filter can refuse it (#105 review
    // round 8).
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let own = event(
        "evt-own",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-a"),
    );
    let foreign = event(
        "evt-foreign",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-b"),
    );
    memory
        .remember_working(&own, "likes rust", Some("coding"), 10)
        .expect("own");
    memory
        .remember_working(&foreign, "also likes rust", Some("coding"), 20)
        .expect("foreign");

    let scoped = memory.relevant_linked(
        MemoryQuery {
            source_namespace: Some("public-chat"),
            actor_id: Some("viewer-a"),
            topic: Some("coding"),
            limit: 10,
        },
        30,
    );
    assert_eq!(
        scoped.len(),
        1,
        "the other viewer's same-topic claim must not arrive as context: {:?}",
        scoped
            .iter()
            .map(|item| item.entry.source.event_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(scoped[0].entry.source.event_id, "evt-own");

    // The same topic without an actor scope still admits both entries:
    // the removal comes from the actor scope, not from narrowing what a
    // topic match means.
    let unscoped = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("coding"),
            limit: 10,
        },
        30,
    );
    assert_eq!(unscoped.len(), 2, "both claims stay topic-retrievable");
}

#[test]
fn a_historical_query_never_reads_evidence_from_the_future() {
    // Round-10: durable A at 1000 (topic "old"), durable B at 2000
    // superseding A (topic "new"). A query at 1500 predates B: A must
    // come back current — no future edge in its view, no future node
    // through the hop — and B itself must be invisible before its own
    // creation time. After the supersession the verdict flips.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "old claim",
        Some("old"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "new claim",
        Some("new"),
        2_000,
        &["evt-a"],
    )
    .expect("b supersedes a");

    let past = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("old"),
            limit: 10,
        },
        1_500,
    );
    assert_eq!(
        past.len(),
        1,
        "B does not exist at 1500: {:?}",
        past.iter()
            .map(|item| item.entry.source.event_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(past[0].entry.source.event_id, "evt-a");
    assert!(
        !past[0].stale,
        "an edge recorded at 2000 cannot mark A stale at 1500"
    );
    assert!(
        past[0].links.iter().all(|link| link.created_at_ms <= 1_500),
        "the view never shows a relation from the future: {:?}",
        past[0].links
    );

    // B is invisible as a direct match before its creation time too.
    let future_direct = memory.relevant(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("new"),
            limit: 10,
        },
        1_500,
    );
    assert!(
        future_direct.is_empty(),
        "a claim recorded at 2000 must not be readable at 1500"
    );

    // After the supersession exists in time, the verdict flips: staleness
    // is drawn from the retained edge set visible at the read timestamp.
    let after = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("old"),
            limit: 10,
        },
        2_500,
    );
    let a = after
        .iter()
        .find(|item| item.entry.source.event_id == "evt-a")
        .expect("A is still retained");
    assert!(
        a.stale,
        "the supersession at 2000 is visible at 2500, so A is stale"
    );
    assert!(
        a.links
            .iter()
            .any(|link| link.created_at_ms == 2_000 && link.kind == MemoryLinkKind::Supersedes),
        "the now-visible supersession edge is in the view: {:?}",
        a.links
    );
}

#[test]
fn equal_time_retrieval_and_temporal_edges_agree_on_the_same_order() {
    // Round-11: two durable writes at one millisecond. The `TemporalBefore`
    // edge derives its order from insertion (the retained timeline), so
    // newest-first retrieval must show the later insert first — ordering
    // by arbitrary event ids could disagree with the edge (#105 review
    // round 11).
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    // Insertion order deliberately disagrees with event-id order:
    // `evt-a` is written first even though `evt-z` sorts after it.
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "first write",
        Some("same"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-z",
        "second write",
        Some("same"),
        1_000,
    );

    let results = memory.relevant_linked(unfiltered_query(10), 2_000);
    let ids: Vec<&str> = results
        .iter()
        .map(|item| item.entry.source.event_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["evt-z", "evt-a"],
        "equal-time retrieval follows insertion order, not event ids"
    );
    let edge = results[0]
        .links
        .iter()
        .find(|link| link.kind == MemoryLinkKind::TemporalBefore)
        .expect("the same-tick writes are linked");
    assert_eq!(
        edge.from, results[1].entry.memory_id,
        "the edge declares the earlier insert as predecessor"
    );
    assert_eq!(
        edge.to, results[0].entry.memory_id,
        "retrieval leads with the edge's successor: the two views agree"
    );
}

#[test]
fn a_snapshot_never_archives_an_edge_to_an_expired_endpoint() {
    // Threat model §8: an edge must not become a covert archive of
    // content retention removed. After `evt-a` expires without an
    // explicit compaction, the physical store still holds the edge —
    // but the persisted snapshot at a later read time must not
    // (#105 review round 11).
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        durable_ttl_ms: 1_500,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
    );
    assert_eq!(memory.links().len(), 1, "one temporal edge between them");

    let before = memory.link_snapshot(2_400);
    assert_eq!(before.links.len(), 1, "both endpoints live at 2400");
    assert_eq!(
        before.vocab_version, MEMORY_LINK_VOCAB_VERSION,
        "the envelope still versions its data"
    );

    // `evt-a` expired at 2_500; compaction has not run, so only the
    // snapshot filter can keep the edge out of the replay artifact.
    assert_eq!(
        memory.links().len(),
        1,
        "the physical store is unchanged — nothing was compacted"
    );
    let after = memory.link_snapshot(3_000);
    assert!(
        after.links.is_empty(),
        "no edge outlives its expired endpoint in the artifact: {:?}",
        after.links
    );
    assert_eq!(after.vocab_version, MEMORY_LINK_VOCAB_VERSION);
}

#[test]
fn the_per_entry_cap_never_hides_the_newest_supersession() {
    // Round-11: with a per-entry cap of one, two supersessions of the
    // same claim must surface the *latest* correction — response
    // generation consumes this view, and truncating to the oldest would
    // hand it obsolete context (#105 review round 11).
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-old",
        "old claim",
        Some("old-topic"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-first",
        "first correction",
        Some("first-topic"),
        2_000,
        &["evt-old"],
    )
    .expect("first supersession");
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-latest",
        "latest correction",
        Some("latest-topic"),
        3_000,
        &["evt-old"],
    )
    .expect("second supersession");
    assert_eq!(memory.links().len(), 2, "both corrections are retained");

    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("old-topic"),
            limit: 3,
        },
        3_500,
    );
    let ids: Vec<&str> = results
        .iter()
        .map(|item| item.entry.source.event_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["evt-old", "evt-latest"],
        "the latest correction is reachable within the node/link budgets, not the oldest"
    );
    assert!(
        results[0].stale,
        "the claim is superseded — verdict unchanged"
    );
    assert_eq!(
        results[0].links.len(),
        1,
        "the cap of one still applies: {:?}",
        results[0].links
    );
}

#[test]
fn same_time_supersessions_lead_with_the_newest_insertion() {
    // Round-12: two corrections that supersede the same target at the
    // *same* record time must still lead with the later insertion when
    // the per-entry cap exposes only one edge. `relevant` and
    // `TemporalBefore` already treat the larger `memory_id` as newest for
    // equal time, so the bounded view must use the same tie-break or it
    // can hand response generation the older same-tick correction.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-old",
        "old claim",
        Some("old-topic"),
        1_000,
    );
    // Both corrections carry record time 2_000; `evt-first` is inserted
    // first, so it holds the smaller `memory_id`.
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-first",
        "first correction",
        Some("first-topic"),
        2_000,
        &["evt-old"],
    )
    .expect("first same-tick supersession");
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-latest",
        "latest correction",
        Some("latest-topic"),
        2_000,
        &["evt-old"],
    )
    .expect("second same-tick supersession");
    assert_eq!(memory.links().len(), 2, "both corrections are retained");

    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("old-topic"),
            limit: 3,
        },
        2_500,
    );
    let ids: Vec<&str> = results
        .iter()
        .map(|item| item.entry.source.event_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["evt-old", "evt-latest"],
        "equal-time supersessions break newest-first, matching retrieval's own tie-break"
    );
    assert!(
        results[0].stale,
        "the claim is superseded — verdict unchanged"
    );
    assert_eq!(
        results[0].links[0].from, results[1].entry.memory_id,
        "the cap of one surfaces the later insertion's edge: {:?}",
        results[0].links
    );
}

#[test]
fn an_actor_filter_without_a_namespace_fails_closed() {
    // Round-11: `source_namespace.zip(actor_id)` drops an actor filter
    // whose namespace is missing, which would silently widen the query
    // to every actor. An evaluable partial scope returns no matches
    // instead (#105 review round 11).
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write_actor(
        &mut memory,
        &mut security,
        "evt-a",
        Some("topic"),
        1_000,
        "viewer-a",
        &[],
    );
    durable_write_actor(
        &mut memory,
        &mut security,
        "evt-b",
        Some("topic"),
        2_000,
        "viewer-b",
        &[],
    );

    let partial = MemoryQuery {
        source_namespace: None,
        actor_id: Some("viewer-a"),
        topic: Some("topic"),
        limit: 10,
    };
    assert!(
        memory.relevant(partial.clone(), 3_000).is_empty(),
        "relevant refuses a filter it cannot evaluate"
    );
    assert!(
        memory.relevant_linked(partial, 3_000).is_empty(),
        "relevant_linked refuses the same partial scope"
    );

    // With the namespace the scope is evaluable and isolates actors.
    let scoped = memory.relevant_linked(
        MemoryQuery {
            source_namespace: Some("system-health"),
            actor_id: Some("viewer-a"),
            topic: Some("topic"),
            limit: 10,
        },
        3_000,
    );
    let ids: Vec<&str> = scoped
        .iter()
        .map(|item| item.entry.source.event_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["evt-a"],
        "the evaluable scope returns only that actor's memory"
    );
}

#[test]
fn a_supersession_target_recorded_after_the_write_is_refused() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-later",
        "claim recorded later",
        Some("game-x"),
        2_000,
    );

    let error = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-backfill",
        "backfilled claim",
        Some("game-x"),
        1_000,
        &["evt-later"],
    )
    .expect_err("a claim cannot replace evidence from its own future");
    assert!(error.to_string().contains("recorded after"), "{error}");
    assert_eq!(memory.len(), 1, "the refused write stored nothing");
    assert!(
        memory.links().is_empty(),
        "and minted no edge that would mark the later claim stale"
    );

    // The same declaration in logical order is accepted, so the rule is
    // direction and not a blanket ban on backfills.
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-update",
        "an update",
        Some("game-y"),
        3_000,
        &["evt-later"],
    )
    .expect("a later claim may replace an earlier one");
    assert_eq!(
        memory.links().len(),
        1,
        "the accepted supersession is stored"
    );
}

#[test]
fn the_store_bound_sheds_derived_edges_before_explicit_supersession() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 1,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        2_000,
        &["evt-a"],
    )
    .expect("supersession accepted");
    assert_eq!(memory.links().len(), 1, "the store sits at its bound");

    // The next derived edge must not evict the caller's evidence: doing
    // so would resurrect a replaced claim while both nodes are live.
    durable_write(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c",
        Some("game-x"),
        3_000,
    );
    assert_eq!(
        memory.links().front().map(|link| link.kind),
        Some(MemoryLinkKind::Supersedes),
        "the bound sheds derived context first: {:?}",
        memory.links()
    );

    let results = memory.relevant_linked(unfiltered_query(10), 3_500);
    let a = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-a")
        .expect("claim a is still retained");
    assert!(
        a.stale,
        "the superseded claim stays marked while both endpoints are live"
    );
}

#[test]
fn a_supersession_write_that_would_evict_earlier_evidence_is_refused() {
    // The re-review reproduction: with both limits at 1 the store cannot
    // hold two explicit edges, and evicting `B -> A` made A current again
    // while B — the memory that replaced it — was still alive. The bound
    // now refuses the *later* write instead of forgetting the earlier
    // evidence, and it refuses it before anything is stored, so the
    // caller learns the evidence was not accepted.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 1,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        2_000,
        &["evt-a"],
    )
    .expect("the first supersession fits the bound");
    assert_eq!(memory.links().len(), 1);

    let error = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c replaces b",
        Some("game-x"),
        3_000,
        &["evt-b"],
    )
    .expect_err("evidence that would displace earlier evidence is refused");
    assert!(error.to_string().contains("explicit evidence"), "{error}");
    assert_eq!(memory.len(), 2, "the refused write stored nothing");
    assert_eq!(memory.links().len(), 1, "and mangled nothing");

    let results = memory.relevant_linked(unfiltered_query(10), 3_500);
    assert_eq!(results.len(), 2, "claim c was never accepted");
    let a = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-a")
        .expect("claim a is still retained");
    assert!(
        a.stale,
        "A was explicitly superseded and is still retained: {:?}",
        memory.links()
    );
    let b = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-b")
        .expect("claim b is still retained");
    assert!(
        !b.stale,
        "C's evidence was refused, so nothing retained supersedes B"
    );
}

#[test]
fn a_superseding_write_is_not_blocked_by_an_edge_that_its_own_compaction_expires() {
    // Round-3 expiry reproduction: the retained B -> A edge has an
    // endpoint already expired at the write timestamp but not yet
    // physically compacted. It is dead evidence — not a claim held
    // against a still-retained node — so counting it would refuse a
    // supersession that fits once this write's own compaction runs, and
    // a valid update would wait for an unrelated write to free
    // logically dead capacity.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 1,
        max_links_per_memory: 1,
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        100,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        200,
        &["evt-a"],
    )
    .expect("the first supersession fits the bound");
    assert_eq!(memory.links().len(), 1);

    // A expired at 1_100 while B (created at 200) stays live until
    // 1_200, so at 1_150 the B -> A edge is dead but not compacted.
    let written = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c replaces b",
        Some("game-x"),
        1_150,
        &["evt-b"],
    )
    .expect("an edge that expires with this write's own compaction does not block the write");

    assert_eq!(
        memory.links().len(),
        1,
        "one live edge remains: {:?}",
        memory.links()
    );
    assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
    assert_eq!(memory.links()[0].from, written);
    assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-b"));
    assert!(
        memory
            .entries()
            .iter()
            .all(|entry| entry.source.event_id != "evt-a"),
        "this write's compaction physically removed the expired endpoint"
    );

    let results = memory.relevant_linked(unfiltered_query(10), 1_150);
    assert_eq!(results.len(), 2, "A is expired, B and C are retained");
    let b = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-b")
        .expect("claim b is still retained");
    assert!(b.stale, "C -> B marks the replaced claim stale");
    let c = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-c")
        .expect("claim c was accepted");
    assert!(!c.stale, "nothing retained supersedes C");
}

#[test]
fn a_superseding_write_is_not_blocked_by_edges_its_own_insertion_evicts() {
    // Round-3 capacity reproduction: with two node slots the insertion
    // of C evicts A, which prunes the dead B -> A edge within the same
    // write, so C -> B exactly fits the one-edge store. The earlier
    // evidence went with the node retention already removed — no
    // still-retained claim loses its marker — so refusing C would only
    // make a valid update wait for an unrelated write to free the slot.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 2,
        max_links: 1,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        2_000,
        &["evt-a"],
    )
    .expect("the first supersession fits the bound");
    assert_eq!(memory.links().len(), 1);

    let written = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c replaces b",
        Some("game-x"),
        3_000,
        &["evt-b"],
    )
    .expect("an edge that this insertion evicts does not block the write");

    assert_eq!(memory.len(), 2, "the insertion evicted A");
    assert!(
        memory
            .entries()
            .iter()
            .all(|entry| entry.source.event_id != "evt-a"),
        "A is capacity-evicted: {:?}",
        memory.entries()
    );
    assert_eq!(
        memory.links().len(),
        1,
        "the one-edge store keeps C -> B: {:?}",
        memory.links()
    );
    assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
    assert_eq!(memory.links()[0].from, written);
    assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-b"));

    let results = memory.relevant_linked(unfiltered_query(10), 3_500);
    let b = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-b")
        .expect("claim b is still retained");
    assert!(b.stale, "C -> B marks the replaced claim stale");
    let c = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-c")
        .expect("claim c was accepted");
    assert!(!c.stale, "nothing retained supersedes C");
}

#[test]
fn a_supersession_target_this_insertion_evicts_does_not_consume_link_capacity() {
    // Round-4 capacity reproduction: three node slots hold X, A, B and
    // the one-edge store holds the retained B -> A evidence. C
    // supersedes X — the node this insertion capacity-evicts — so the
    // fresh C -> X edge is pruned by the same write's compaction and
    // the store ends exactly where it started. Refusing C would hold a
    // write hostage to a slot its own insertion frees; the evicted
    // target leaves no retained claim that could resurrect.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 3,
        max_links: 1,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-x",
        "claim x",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        2_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        3_000,
        &["evt-a"],
    )
    .expect("the first supersession fits the bound");
    assert_eq!(memory.links().len(), 1);

    let written = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c replaces x",
        Some("game-x"),
        4_000,
        &["evt-x"],
    )
    .expect("a target this insertion evicts does not consume link capacity");

    assert_eq!(memory.len(), 3, "the insertion evicted X");
    assert!(
        memory
            .entries()
            .iter()
            .all(|entry| entry.source.event_id != "evt-x"),
        "X is capacity-evicted: {:?}",
        memory.entries()
    );
    assert_eq!(
        memory.links().len(),
        1,
        "C -> X was pruned with X; B -> A remains: {:?}",
        memory.links()
    );
    assert_eq!(memory.links()[0].kind, MemoryLinkKind::Supersedes);
    assert_eq!(memory.links()[0].from, retained_node(&memory, "evt-b"));
    assert_eq!(memory.links()[0].to, retained_node(&memory, "evt-a"));
    assert!(
        memory.links().iter().all(|link| link.from != written),
        "no edge survives from the new node when its only target was evicted"
    );

    let results = memory.relevant_linked(unfiltered_query(10), 4_500);
    let a = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-a")
        .expect("claim a is still retained");
    assert!(a.stale, "B -> A survives the write untouched");
    let c = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-c")
        .expect("claim c was accepted");
    assert!(!c.stale, "nothing retained supersedes C");
}

#[test]
fn an_explicit_supersession_chain_survives_derived_edge_pressure() {
    // The same `A <- B <- C` shape at a bound that can hold the chain:
    // the store is then only ever allowed to shed derived edges, so both
    // explicitly superseded nodes keep their verdict while all three are
    // live — the assertion the re-review asked for under edge pressure.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 8,
        max_links: 2,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        2_000,
        &["evt-a"],
    )
    .expect("first link fits");
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c replaces b",
        Some("game-x"),
        3_000,
        &["evt-b"],
    )
    .expect("the chain fits the bound");
    assert_eq!(memory.links().len(), 2, "the chain fills the bound");

    // Derived pressure: every later same-topic write wants a temporal
    // edge the bound must shed instead of touching the chain.
    for (id, at_ms) in [("evt-d", 4_000_u64), ("evt-e", 5_000_u64)] {
        durable_write(
            &mut memory,
            &mut security,
            id,
            "a later claim",
            Some("game-x"),
            at_ms,
        );
    }
    assert_eq!(
        memory
            .links()
            .iter()
            .filter(|link| link.kind == MemoryLinkKind::Supersedes)
            .count(),
        2,
        "both explicit edges survived derived pressure: {:?}",
        memory.links()
    );

    let results = memory.relevant_linked(unfiltered_query(10), 5_500);
    assert_eq!(results.len(), 5, "every node is retained");
    for id in ["evt-a", "evt-b"] {
        let item = results
            .iter()
            .find(|result| result.entry.source.event_id == id)
            .expect("retained");
        assert!(
            item.stale,
            "{id} was explicitly superseded and is still retained: {:?}",
            memory.links()
        );
    }
    let c = results
        .iter()
        .find(|result| result.entry.source.event_id == "evt-c")
        .expect("retained");
    assert!(!c.stale, "nothing retained supersedes C");
}

#[test]
fn repeated_supersession_targets_are_counted_once_against_the_write_budget() {
    for cap in [2_usize, 3] {
        let mut security = security();
        let mut memory = test_memory(WorkingMemoryConfig {
            max_links: cap + 2,
            max_links_per_memory: cap,
            ..WorkingMemoryConfig::default()
        });
        durable_write(
            &mut memory,
            &mut security,
            "evt-a",
            "claim a",
            Some("game-x"),
            1_000,
        );
        durable_write(
            &mut memory,
            &mut security,
            "evt-b",
            "claim b",
            Some("game-x"),
            2_000,
        );

        // Three declarations, two unique targets: the raw count must not
        // refuse the write, and the duplicate must not eat a budget slot
        // the temporal spine could have used.
        let written = durable_write_superseding(
            &mut memory,
            &mut security,
            "evt-d",
            "claim d",
            Some("game-x"),
            3_000,
            &["evt-a", "evt-a", "evt-b"],
        )
        .unwrap_or_else(|error| panic!("cap {cap}: duplicate names are one target: {error}"));
        let supersessions = memory
            .links()
            .iter()
            .filter(|link| link.from == written && link.kind == MemoryLinkKind::Supersedes)
            .count();
        assert_eq!(supersessions, 2, "cap {cap}: one edge per unique target");
        let temporal = memory
            .links()
            .iter()
            .filter(|link| link.to == written && link.kind == MemoryLinkKind::TemporalBefore)
            .count();
        assert_eq!(
            temporal,
            usize::from(cap >= 3),
            "cap {cap}: whatever the duplicates did not spend stays available to the spine"
        );
    }
}

#[test]
fn supersession_declarations_are_bounded_before_deduplication() {
    // Round-6: the dedup pass used to allocate capacity for — and walk —
    // the entire raw slice before applying the per-write budget, so a
    // flood of restated ids could force unbounded allocation and CPU
    // despite producing a single edge. The input is now bounded before
    // any allocation, while ordinary restatement inside that bound keeps
    // its old meaning: duplicates still count once.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 4,
        max_links_per_memory: 2,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    let links_before = memory.links().len();

    // Bound: max_links_per_memory (2) × SUPERSESSION_DECLARATION_FACTOR.
    let flood = vec!["evt-a"; 4_096];
    let error = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
        &flood,
    )
    .expect_err("an oversized declaration list is refused before the dedup scan");
    assert!(
        error.to_string().contains("declarations"),
        "the refusal names the unbounded input: {error}"
    );
    assert_eq!(memory.len(), 1, "the refused write stored nothing");
    assert_eq!(memory.links().len(), links_before, "and mangled nothing");

    // Inside the bound, restatement keeps its old meaning: duplicates
    // count once rather than erroring or spending budget.
    let written = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b replaces a",
        Some("game-x"),
        2_000,
        &["evt-a", "evt-a", "evt-a"],
    )
    .expect("restatement inside the bound is still one target");
    let supersessions = memory
        .links()
        .iter()
        .filter(|link| link.from == written && link.kind == MemoryLinkKind::Supersedes)
        .count();
    assert_eq!(supersessions, 1, "three names, one edge");
}

#[test]
fn a_superseding_claim_brings_its_linked_target_back_within_the_query_limit() {
    // The cross-topic update: the query matches only the new claim, and
    // the single-hop timeline is only consumable if the claim it replaces
    // comes back with it — otherwise the caller must bypass the API and
    // scan the store for the endpoint id the view merely names.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-old-topic",
        "viewer disliked the game",
        Some("game-x"),
        1_000,
    );
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-new-topic",
        "viewer finished the game",
        Some("game-y"),
        2_000,
        &["evt-old-topic"],
    )
    .expect("supersession accepted");

    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("game-y"),
            limit: 10,
        },
        3_000,
    );
    assert_eq!(
        results.len(),
        2,
        "the matched claim plus the retained claim it supersedes"
    );
    assert_eq!(results[0].entry.source.event_id, "evt-new-topic");
    assert_eq!(results[1].entry.source.event_id, "evt-old-topic");
    assert!(!results[0].stale, "the current claim is not stale");
    assert!(
        results[1].stale,
        "the replaced claim comes back marked stale"
    );

    // The hop is bounded by the caller's limit and never displaces a
    // direct match.
    let bounded = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("game-y"),
            limit: 1,
        },
        3_000,
    );
    assert_eq!(bounded.len(), 1, "the query limit is still the bound");
    assert_eq!(bounded[0].entry.source.event_id, "evt-new-topic");
}

#[test]
fn an_expired_endpoint_never_appears_in_a_returned_link_view() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        durable_ttl_ms: 1_500,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
    );
    assert_eq!(memory.links().len(), 1, "one temporal edge between them");

    // `evt-a` expired at 2_500 while compaction has not run: retrieval
    // already treats it as deleted, so relation metadata about it is a
    // dangling edge to a node that no longer exists.
    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("game-x"),
            limit: 10,
        },
        3_000,
    );
    assert_eq!(results.len(), 1, "only the live claim is retrievable");
    assert!(
        results[0].links.is_empty(),
        "no dangling relation to the expired endpoint"
    );
    assert!(
        !results[0].stale,
        "an expired node cannot keep a claim marked"
    );
    assert_eq!(
        memory.links().len(),
        1,
        "physical removal stays compaction's job"
    );
}

#[test]
fn serialized_link_artifacts_carry_the_vocabulary_version() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
    );

    let snapshot =
        serde_json::to_value(memory.link_snapshot(3_000)).expect("serialize the link envelope");
    assert_eq!(
        snapshot["vocab_version"],
        Value::String(MEMORY_LINK_VOCAB_VERSION.to_owned())
    );
    assert_eq!(snapshot["links"].as_array().map(Vec::len), Some(1));

    let results = memory.relevant_linked(unfiltered_query(10), 3_000);
    let view = serde_json::to_value(&results[0]).expect("serialize the retrieval view");
    assert_eq!(
        view["vocab_version"],
        Value::String(MEMORY_LINK_VOCAB_VERSION.to_owned()),
        "a replay can identify the vocabulary instead of inferring it: {view}"
    );
    assert_eq!(
        view["epistemic_version"],
        Value::String(MEMORY_EPISTEMIC_VOCAB_VERSION.to_owned()),
        "the same view names the epistemic vocabulary it was classified under"
    );
    assert_eq!(
        view["entry"]["epistemic_class"],
        Value::String("verified_fact".to_owned()),
        "and carries the class the entry was written with: {view}"
    );
}

#[test]
fn the_epistemic_class_is_a_pure_function_of_trusted_metadata() {
    // #105 §3.1: the class comes from the source class and trust level the
    // security runtime already established — never from claim text,
    // retrieval rank, model output, or linkage. `VerifiedFact` has exactly
    // one route in, so nothing a viewer says can be promoted by repetition.
    use SourceClass::*;
    use TrustLevel::*;
    let cases = [
        ((System, Trusted, true), MemoryEpistemicClass::VerifiedFact),
        // A trusted permit for a claim the event never carried is not
        // verified evidence — binding is required, not just provenance.
        ((System, Trusted, false), MemoryEpistemicClass::Observation),
        // The gate admits a semi-trusted system write; it is not a fact.
        (
            (System, SemiTrusted, true),
            MemoryEpistemicClass::Observation,
        ),
        ((System, Untrusted, true), MemoryEpistemicClass::Observation),
        // Public content is a viewer assertion at any nominal trust.
        (
            (PublicChat, Untrusted, true),
            MemoryEpistemicClass::ViewerClaim,
        ),
        (
            (PublicChat, Trusted, true),
            MemoryEpistemicClass::ViewerClaim,
        ),
        (
            (Donation, SemiTrusted, false),
            MemoryEpistemicClass::ViewerClaim,
        ),
        (
            (Speech, Trusted, true),
            MemoryEpistemicClass::PerformerExperience,
        ),
        (
            (Speech, SemiTrusted, true),
            MemoryEpistemicClass::Observation,
        ),
        // An authenticated operator asserts an opinion, not a fact.
        (
            (Operator, Trusted, true),
            MemoryEpistemicClass::PersonaOpinion,
        ),
        ((Game, Untrusted, true), MemoryEpistemicClass::Observation),
        (
            (Stream, SemiTrusted, true),
            MemoryEpistemicClass::Observation,
        ),
        ((Timer, Trusted, true), MemoryEpistemicClass::Observation),
    ];
    for ((source_class, trust_level, bound), expected) in cases {
        let class = MemoryEpistemicClass::derive(source_class, trust_level, bound);
        assert_eq!(
            class, expected,
            "{source_class:?}/{trust_level:?}/bound={bound}"
        );
        assert_eq!(
            class,
            MemoryEpistemicClass::derive(source_class, trust_level, bound),
            "derivation is deterministic"
        );
    }
    assert_eq!(
        cases
            .iter()
            .filter(|(_, class)| *class == MemoryEpistemicClass::VerifiedFact)
            .count(),
        1,
        "exactly one route reaches VerifiedFact: trusted, system, and bound"
    );
}

#[test]
fn a_trusted_permit_for_an_unrelated_claim_is_not_a_verified_fact() {
    // #105 review round 2: a permit proves the *event* was authorized, not
    // the content of a sentence supplied next to it. A trusted system
    // health event that never asserted this claim must not launder it into
    // a verified fact.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let mut source = event("evt-health", SourceClass::System, TrustLevel::Trusted, None);
    source.payload.insert(
        "text".to_owned(),
        Value::String("stream is healthy".to_owned()),
    );
    let (decision, permit) = security.authorize_memory_write(&source, None);
    assert_eq!(decision, MemoryWriteDecision::AllowedSystemSource);
    let permit = permit.expect("trusted system permit");

    let unrelated = memory
        .remember_durable(
            &permit,
            "viewer asserts the game author is X",
            Some("lore"),
            1_000,
        )
        .expect("durable write");
    assert_eq!(
        unrelated.epistemic_class,
        MemoryEpistemicClass::Observation,
        "an unrelated sentence through a trusted permit is not verified evidence"
    );

    // The event's own claim *is* the verified evidence.
    let bound = memory
        .remember_durable(&permit, "stream is healthy", Some("health"), 2_000)
        .expect("durable write of the event's own claim");
    assert_eq!(
        bound.epistemic_class,
        MemoryEpistemicClass::VerifiedFact,
        "the permitted event's own claim is the verified fact"
    );
}

#[test]
fn claim_binding_survives_normalization_but_not_an_appended_sentence() {
    // #105 review round 3: the event's own claim must not be downgraded
    // just because storage normalizes case and whitespace, and appended
    // content must not inherit the event's provenance.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let mut source = event("evt-health", SourceClass::System, TrustLevel::Trusted, None);
    source.payload.insert(
        "text".to_owned(),
        Value::String("Stream   Is Healthy".to_owned()),
    );
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("trusted system permit");

    let bound = memory
        .remember_durable(&permit, "Stream   Is Healthy", Some("health"), 1_000)
        .expect("durable write");
    assert_eq!(
        bound.epistemic_class,
        MemoryEpistemicClass::VerifiedFact,
        "the event's own claim stays verified under case/whitespace normalization"
    );

    let appended = memory
        .remember_durable(
            &permit,
            "Stream   Is Healthy and the author is X",
            Some("lore"),
            2_000,
        )
        .expect("durable write");
    assert_eq!(
        appended.epistemic_class,
        MemoryEpistemicClass::Observation,
        "content appended to the event's claim is not verified evidence"
    );
}

#[test]
fn binding_is_judged_on_full_text_and_overlong_verified_facts_are_rejected() {
    // #105 review round 3 + round 4: binding must not be tested against
    // the truncated stored text, and a verified fact whose own claim
    // exceeds the storage budget must be rejected rather than stored
    // truncated — truncation could drop the negation or qualifier that
    // bounded it.
    let config = WorkingMemoryConfig {
        max_claim_bytes: 20,
        ..WorkingMemoryConfig::default()
    };
    let mut security = security();
    let mut memory = test_memory(config);
    let event_text = "stream is healthy today";
    let mut source = event("evt-health", SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(event_text.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("trusted system permit");

    // Round 4: the event's own claim is recognized as bound (it reaches
    // VerifiedFact) and, being overlong, is refused — not stored
    // truncated. If binding were judged on the truncated copy instead,
    // this overlong write would slip through as an unverified class and
    // be stored, so the rejection is the discriminator.
    let error = memory
        .remember_durable(&permit, event_text, Some("health"), 1_000)
        .expect_err("an overlong verified fact must be rejected, not truncated");
    assert!(
        matches!(error, AdaptationError::InvalidInput(_)),
        "typed rejection: {error}"
    );
    assert!(
        memory.entries().is_empty(),
        "a rejected write stores nothing"
    );

    // Weaker classes keep the truncation semantics: an appended sentence
    // is an Observation, and it is stored truncated rather than refused.
    let extended = format!("{event_text} and the author is X");
    let appended = memory
        .remember_durable(&permit, &extended, Some("lore"), 2_000)
        .expect("durable write");
    assert_eq!(
        appended.epistemic_class,
        MemoryEpistemicClass::Observation,
        "content appended to the event's claim is not verified evidence"
    );
    assert!(
        appended.normalized_claim.len() < extended.len(),
        "the weaker class keeps the bounded truncation: {}",
        appended.normalized_claim
    );
}

#[test]
fn a_negation_beyond_the_byte_limit_is_rejected_not_truncated() {
    // #105 review round 4: a qualifier/negation that falls beyond the
    // storage byte bound must cause a typed rejection of the verified
    // fact, never a truncated stored copy whose meaning flipped.
    let config = WorkingMemoryConfig {
        max_claim_bytes: 20,
        ..WorkingMemoryConfig::default()
    };
    let mut security = security();
    let mut memory = test_memory(config);
    // 31 bytes normalized: truncating at 20 would drop "not verified".
    let event_text = "the game author is not verified";
    assert!(
        normalize(event_text).len() > 20,
        "the case really is overlong"
    );
    let mut source = event("evt-lore", SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(event_text.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("trusted system permit");

    let error = memory
        .remember_durable(&permit, event_text, Some("lore"), 1_000)
        .expect_err("the negation beyond the byte limit must reject the write");
    assert!(
        matches!(error, AdaptationError::InvalidInput(_)),
        "typed rejection: {error}"
    );
    assert!(
        memory.entries().is_empty(),
        "nothing is stored when the verified fact is rejected"
    );

    // The same claim through the same permit is accepted once the budget
    // holds it in full: the rejection is the byte bound, not the content.
    let generous = test_memory(WorkingMemoryConfig {
        max_claim_bytes: 64,
        ..WorkingMemoryConfig::default()
    });
    let stored = {
        let mut memory = generous;
        let stored = memory
            .remember_durable(&permit, event_text, Some("lore"), 1_000)
            .expect("a complete verified fact is stored");
        assert_eq!(
            stored.epistemic_class,
            MemoryEpistemicClass::VerifiedFact,
            "the complete claim is the verified fact"
        );
        assert_eq!(
            stored.normalized_claim, "the game author is not verified",
            "the negation survives intact"
        );
        stored.normalized_claim.clone()
    };
    assert_eq!(stored, "the game author is not verified");

    // The working-memory path enforces the same complete-or-reject rule.
    let mut working = test_memory(config);
    let error = working
        .remember_working(&source, event_text, Some("lore"), 1_000)
        .expect_err("an overlong verified fact is rejected in working memory too");
    assert!(matches!(error, AdaptationError::InvalidInput(_)));
    assert!(working.entries().is_empty());
}

#[test]
fn the_issue_231_counterexample_cannot_be_stored_as_a_verified_fact() {
    // #231's exact reproduction. At a 17-byte budget the prefix itself is
    // a complete-looking proposition ("server is healthy"), so a
    // truncating store would certify the opposite of the claim's own tail
    // ("... but this statement is false") — the retained proposition would
    // not be the one the runtime observed.
    let config = WorkingMemoryConfig {
        max_claim_bytes: 17,
        ..WorkingMemoryConfig::default()
    };
    let mut security = security();
    let mut memory = test_memory(config);
    let event_text = "server is healthy but this statement is false";
    assert_eq!(
        normalize("server is healthy").len(),
        17,
        "the counterexample truncates exactly at the byte bound"
    );
    assert!(
        normalize(event_text).len() > 17,
        "the claim really is overlong"
    );
    let mut source = event("evt-health", SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(event_text.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("trusted system permit");

    let error = memory
        .remember_durable(&permit, event_text, Some("health"), 1_000)
        .expect_err("the counterexample must not be stored truncated");
    assert!(
        matches!(error, AdaptationError::InvalidInput(_)),
        "typed rejection: {error}"
    );
    // #231 acceptance: a refused write inserts nothing at all — no entry,
    // no link, and no burned node id.
    assert!(memory.entries().is_empty(), "no entry is inserted");
    assert!(memory.links().is_empty(), "no link is inserted");
    assert_eq!(memory.len(), 0, "the store is untouched");

    // The high-water mark proves "no partial insertion": the first
    // accepted write still takes the first node id, so the refused write
    // consumed nothing a replay would miss.
    let complete = "server is healthy";
    let mut second = event(
        "evt-health-2",
        SourceClass::System,
        TrustLevel::Trusted,
        None,
    );
    second
        .payload
        .insert("text".to_owned(), Value::String(complete.to_owned()));
    let (_, permit) = security.authorize_memory_write(&second, None);
    let permit = permit.expect("trusted system permit");
    let stored = memory
        .remember_durable(&permit, complete, Some("health"), 2_000)
        .expect("a complete trusted claim at the byte bound is stored");
    assert_eq!(
        stored.memory_id, 1,
        "the refused write did not consume a node id"
    );
    assert_eq!(stored.epistemic_class, MemoryEpistemicClass::VerifiedFact);
    assert_eq!(stored.normalized_claim, complete);

    // The complete proposition is retained once the budget holds it in
    // full, so the rejection is the byte bound and not the qualification.
    let mut generous = test_memory(WorkingMemoryConfig {
        max_claim_bytes: 64,
        ..WorkingMemoryConfig::default()
    });
    let mut third = event(
        "evt-health-3",
        SourceClass::System,
        TrustLevel::Trusted,
        None,
    );
    third
        .payload
        .insert("text".to_owned(), Value::String(event_text.to_owned()));
    let (_, permit) = security.authorize_memory_write(&third, None);
    let permit = permit.expect("trusted system permit");
    let stored = generous
        .remember_durable(&permit, event_text, Some("health"), 3_000)
        .expect("a complete verified fact is stored");
    assert_eq!(stored.normalized_claim, event_text);
    assert_eq!(stored.epistemic_class, MemoryEpistemicClass::VerifiedFact);
}

#[test]
fn the_verified_fact_bound_is_measured_in_bytes_across_a_multi_byte_claim() {
    // #231 acceptance: the bound the invariant is judged against is the
    // same byte bound the truncation used, including where bytes and
    // characters disagree. A claim with fewer than `max_claim_bytes`
    // characters but more bytes must still be refused, an accepted one
    // must be stored whole (never cut inside a character), and a weaker
    // class must still truncate only on a character boundary.
    let claim = "作者は未確認である";
    let claim_bytes = normalize(claim).len();
    let claim_chars = normalize(claim).chars().count();
    assert!(
        claim_bytes > 24 && claim_chars < 24,
        "byte and character counts must disagree: {claim_bytes} bytes, {claim_chars} characters"
    );
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_claim_bytes: 24,
        ..WorkingMemoryConfig::default()
    });
    let mut source = event("evt-lore", SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(claim.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("trusted system permit");

    let error = memory
        .remember_durable(&permit, claim, Some("lore"), 1_000)
        .expect_err("the byte bound, not the character count, rejects this claim");
    assert!(
        matches!(error, AdaptationError::InvalidInput(_)),
        "typed rejection: {error}"
    );
    assert!(memory.entries().is_empty(), "nothing is stored");

    // Fits in bytes: the retained text is the input, intact.
    let mut generous = test_memory(WorkingMemoryConfig {
        max_claim_bytes: 64,
        ..WorkingMemoryConfig::default()
    });
    let stored = generous
        .remember_durable(&permit, claim, Some("lore"), 1_000)
        .expect("a claim the budget holds in full is stored");
    assert_eq!(
        stored.normalized_claim, claim,
        "no character is split or dropped"
    );
    assert_eq!(stored.epistemic_class, MemoryEpistemicClass::VerifiedFact);

    // A weaker class keeps the truncation semantics, and the cut still
    // lands on a character boundary of the same normalized text.
    let appended = format!("{claim} and the stream is calm");
    let mut truncating = test_memory(WorkingMemoryConfig {
        max_claim_bytes: 24,
        ..WorkingMemoryConfig::default()
    });
    let observation = truncating
        .remember_durable(&permit, &appended, Some("lore"), 2_000)
        .expect("a weaker class is still stored truncated");
    assert_eq!(
        observation.epistemic_class,
        MemoryEpistemicClass::Observation,
        "appended content is not verified evidence"
    );
    assert!(observation.normalized_claim.len() <= 24);
    assert!(
        observation.normalized_claim.len() < normalize(&appended).len(),
        "this case really is truncated: {}",
        observation.normalized_claim
    );
    assert!(
        normalize(&appended).starts_with(&observation.normalized_claim),
        "the cut lands on a character boundary: {}",
        observation.normalized_claim
    );
}

#[test]
fn every_entry_persists_the_epistemic_vocabulary_it_was_classified_under() {
    // #105 review round 2: a class is worthless to a replay without the
    // vocabulary it was derived under. The version is stored per entry, not
    // stamped from the current process constant, so an older artifact stays
    // identifiable instead of being presented as derived under the current
    // vocabulary.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );

    let entries = serde_json::to_value(memory.entries()).expect("serialize entries");
    assert_eq!(
        entries[0]["epistemic_version"],
        Value::String(MEMORY_EPISTEMIC_VOCAB_VERSION.to_owned()),
        "each serialized entry names the vocabulary its class was derived under: {entries}"
    );
    assert_eq!(
        entries[0]["epistemic_class"],
        Value::String("verified_fact".to_owned()),
        "the bound claim is the verified fact: {entries}"
    );

    let view = memory.relevant_linked(unfiltered_query(4), 1_500);
    assert_eq!(
        view[0].epistemic_version,
        view[0].entry.epistemic_version.as_str(),
        "the view reports the entry's stored version rather than the process constant"
    );
}

#[test]
fn a_semi_trusted_system_write_is_admitted_but_is_not_a_verified_fact() {
    // `SecurityRuntime::authorize_memory_write` admits a semi-trusted
    // system-plane source, so classification is deliberately stricter than
    // the gate: a semi-trusted observation is admitted but stays an
    // observation, or the store could mint facts the runtime never
    // verified (#105 §3.1).
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    // Bind the claim, so the only reason this is not a fact is the trust
    // level — not a missing claim binding.
    let mut source = event(
        "evt-semi",
        SourceClass::System,
        TrustLevel::SemiTrusted,
        None,
    );
    source.payload.insert(
        "text".to_owned(),
        Value::String("semi-trusted observation".to_owned()),
    );
    let (decision, permit) = security.authorize_memory_write(&source, None);
    assert_eq!(decision, MemoryWriteDecision::AllowedSystemSource);
    let permit = permit.expect("the gate admits a semi-trusted system write");
    let stored = memory
        .remember_durable(&permit, "semi-trusted observation", Some("health"), 1_000)
        .expect("durable write");
    assert_eq!(
        stored.epistemic_class,
        MemoryEpistemicClass::Observation,
        "admitted by the gate, still not a verified fact"
    );
}

#[test]
fn public_content_stays_a_viewer_claim_through_retrieval() {
    // A viewer assertion in working memory must come back as a viewer
    // claim: no ranking, repetition, or linkage may promote it, and a
    // content-plane write mints no links that could carry an upgrade.
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let public = event(
        "evt-chat",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-1"),
    );
    memory
        .remember_working(&public, "the author is x", Some("lore"), 1_000)
        .expect("public working write");
    let results = memory.relevant(unfiltered_query(4), 1_500);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].epistemic_class,
        MemoryEpistemicClass::ViewerClaim,
        "retrieval returns the class it was written with"
    );
    assert!(
        memory.links().is_empty(),
        "a public claim has no linkage through which to self-promote"
    );
}

#[test]
fn temporal_edges_follow_record_time_not_insertion_order() {
    // Backfill: the second write carries an *earlier* record time than
    // the first. Insertion order would derive `TemporalBefore` from the
    // 2_000 record to the 1_000 one — an edge that contradicts the record
    // times `relevant` ranks by.
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-2000",
        "claim recorded at 2000",
        Some("game-x"),
        2_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-1000",
        "claim recorded at 1000",
        Some("game-x"),
        1_000,
    );
    assert!(
        memory.links().is_empty(),
        "a later record is not an earlier record's predecessor: {:?}",
        memory.links()
    );

    durable_write(
        &mut memory,
        &mut security,
        "evt-1500",
        "claim recorded at 1500",
        Some("game-x"),
        1_500,
    );
    let older = retained_node(&memory, "evt-1000");
    let middle = retained_node(&memory, "evt-1500");
    let newer = retained_node(&memory, "evt-2000");
    let edges: Vec<(u64, u64)> = memory
        .links()
        .iter()
        .map(|link| (link.from, link.to))
        .collect();
    assert_eq!(
        edges,
        vec![(older, middle)],
        "only the genuinely older record is a predecessor"
    );
    assert!(!edges.contains(&(newer, middle)));
}

#[test]
fn staleness_is_decided_from_the_full_edge_set_not_the_capped_view() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links_per_memory: 2,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c",
        Some("game-x"),
        3_000,
    );
    // evt-a now carries two temporal edges — its whole view budget — so
    // the later explicit supersession is the third incident edge.
    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-d",
        "claim d",
        Some("game-x"),
        4_000,
        &["evt-a"],
    )
    .expect("supersession accepted");

    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("game-x"),
            limit: 10,
        },
        4_500,
    );
    let a = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-a")
        .expect("evt-a retained");
    assert!(
        a.stale,
        "a supersession that fell past the view cap still marks its target stale"
    );
    assert!(a.links.len() <= 2, "the view stays capped");
    assert_eq!(
        a.links.first().map(|link| link.kind),
        Some(MemoryLinkKind::Supersedes),
        "semantically decisive edges lead the bounded view: {:?}",
        a.links
    );
    assert_eq!(a.links[0].to, a.entry.memory_id);
}

#[test]
fn an_ambiguous_supersession_target_is_refused_and_stores_nothing() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-same",
        "first claim",
        None,
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-same",
        "second unrelated claim",
        None,
        2_000,
    );
    assert_eq!(memory.len(), 2, "duplicate retained ids stay allowed");
    let copies: Vec<u64> = memory
        .entries()
        .iter()
        .map(|entry| entry.memory_id)
        .collect();
    assert_ne!(copies[0], copies[1], "each retained copy is its own node");
    let links_before = memory.links().len();

    let error = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-update",
        "an update",
        None,
        3_000,
        &["evt-same"],
    )
    .expect_err("one source event id retained twice must not be guessed onto a node");
    assert!(matches!(error, AdaptationError::InvalidInput(_)));
    assert_eq!(memory.len(), 2, "a refused write stores nothing");
    assert_eq!(
        memory.links().len(),
        links_before,
        "a refused write mints no links"
    );
    let results = memory.relevant_linked(unfiltered_query(10), 3_500);
    assert!(
        results.iter().all(|item| !item.stale),
        "neither duplicate may be marked stale by a refused write"
    );
}

#[allow(clippy::too_many_arguments)]
fn durable_write_with_evidence(
    memory: &mut WorkingMemory,
    security: &mut SecurityRuntime,
    id: &str,
    claim: &str,
    topic: Option<&str>,
    now_ms: u64,
    supersedes: &[&str],
    caused_by: &[&str],
) -> Result<u64, AdaptationError> {
    let mut source = event(id, SourceClass::System, TrustLevel::Trusted, None);
    source
        .payload
        .insert("text".to_owned(), Value::String(claim.to_owned()));
    let (_, permit) = security.authorize_memory_write(&source, None);
    let permit = permit.expect("system permit");
    memory
        .remember_durable_with_evidence(
            &permit,
            claim,
            topic,
            now_ms,
            MemoryEvidence {
                supersedes,
                caused_by,
            },
        )
        .map(|entry| entry.memory_id)
}

#[test]
fn a_causal_declaration_links_the_new_claim_to_its_cause() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-goal",
        "the viewer wants to finish game x",
        None,
        1_000,
    );
    let goal = retained_node(&memory, "evt-goal");
    let attempt = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-attempt",
        "the viewer tried the final boss again",
        None,
        2_000,
        &[],
        &["evt-goal"],
    )
    .expect("a declared cause is accepted");

    let links = memory.links();
    assert_eq!(links.len(), 1, "one declaration mints one edge: {links:?}");
    assert_eq!(links[0].from, attempt);
    assert_eq!(links[0].to, goal);
    assert_eq!(links[0].kind, MemoryLinkKind::CausedBy);
    assert_eq!(links[0].created_at_ms, 2_000);

    // The edge is visible from both endpoints through the retrieval view,
    // so a response can say what an outcome followed from as well as what
    // it led to.
    let results = memory.relevant_linked(unfiltered_query(8), 3_000);
    let from_attempt = results
        .iter()
        .find(|item| item.entry.memory_id == attempt)
        .expect("the attempt is retrievable");
    assert_eq!(from_attempt.links[0].kind, MemoryLinkKind::CausedBy);
    assert_eq!(from_attempt.links[0].to, goal);
    assert_eq!(
        from_attempt.vocab_version, MEMORY_LINK_VOCAB_VERSION,
        "the view names the vocabulary that shaped it"
    );
}

#[test]
fn a_causal_target_that_is_not_retained_refuses_the_write() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    let error = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-result",
        "the attempt worked",
        None,
        2_000,
        &[],
        &["evt-never-written"],
    )
    .expect_err("an unknown cause is refused");
    assert!(
        matches!(&error, AdaptationError::InvalidInput(message)
            if message.starts_with("causal target must be")),
        "{error}"
    );
    assert_eq!(memory.len(), 0, "a refused write stores nothing");
    assert!(memory.links().is_empty(), "a refused write mints no links");
}

#[test]
fn a_causal_target_from_the_future_is_refused() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-cause",
        "the later claim",
        None,
        3_000,
    );
    let error = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-effect",
        "the earlier claim",
        None,
        2_000,
        &[],
        &["evt-cause"],
    )
    .expect_err("a claim cannot be caused by evidence from its own future");
    assert!(
        matches!(&error, AdaptationError::InvalidInput(message)
            if message.starts_with("causal target was recorded after")),
        "{error}"
    );
    assert_eq!(memory.len(), 1);
    assert!(memory.links().is_empty());
}

#[test]
fn supersession_and_causation_share_the_per_write_budget() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links_per_memory: 2,
        ..WorkingMemoryConfig::default()
    });
    for (index, id) in ["evt-a", "evt-b", "evt-c"].iter().enumerate() {
        durable_write(
            &mut memory,
            &mut security,
            id,
            "a retained claim",
            None,
            1_000 + index as u64,
        );
    }

    let error = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-d",
        "an update with two declared causes",
        None,
        2_000,
        &["evt-a"],
        &["evt-b", "evt-c"],
    )
    .expect_err("three explicit edges exceed a budget of two");
    assert!(
        matches!(&error, AdaptationError::InvalidInput(message)
            if message.contains("explicit relations than the per-memory link budget")),
        "{error}"
    );
    assert_eq!(memory.len(), 3, "a refused write stores nothing");
    assert!(memory.links().is_empty(), "a refused write mints no links");

    // Two declarations across the two kinds fit the one budget, in the
    // order the write listed them: supersession first, then causation.
    let updater = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-e",
        "an update caused by one earlier claim",
        None,
        2_500,
        &["evt-a"],
        &["evt-b"],
    )
    .expect("two declarations fit");
    let kinds: Vec<MemoryLinkKind> = memory
        .links()
        .iter()
        .filter(|link| link.from == updater)
        .map(|link| link.kind)
        .collect();
    assert_eq!(
        kinds,
        [MemoryLinkKind::Supersedes, MemoryLinkKind::CausedBy],
        "explicit evidence is emitted supersession first, then causation"
    );
}

#[test]
fn causal_evidence_is_never_shed_to_make_room() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 2,
        max_links_per_memory: 1,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-cause",
        "the claim everything follows from",
        None,
        1_000,
    );
    for (index, id) in ["evt-1", "evt-2"].iter().enumerate() {
        durable_write_with_evidence(
            &mut memory,
            &mut security,
            id,
            "an outcome",
            None,
            2_000 + index as u64,
            &[],
            &["evt-cause"],
        )
        .expect("the store can hold both explicit edges");
    }
    assert_eq!(
        memory
            .links()
            .iter()
            .filter(|link| link.kind == MemoryLinkKind::CausedBy)
            .count(),
        2,
        "both causal edges are the caller's evidence"
    );

    let error = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-3",
        "a third outcome",
        None,
        3_000,
        &[],
        &["evt-cause"],
    )
    .expect_err("a third explicit edge would displace evidence");
    assert!(
        matches!(&error, AdaptationError::InvalidInput(message)
            if message.contains("cannot hold this write's explicit evidence")),
        "{error}"
    );
    assert_eq!(
        memory
            .links()
            .iter()
            .filter(|link| link.kind == MemoryLinkKind::CausedBy)
            .count(),
        2,
        "the refused write left the retained causal evidence alone"
    );
}

#[test]
fn an_expired_cause_prunes_its_causal_edge() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-cause",
        "the cause",
        None,
        0,
    );
    durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-effect",
        "the effect",
        None,
        500,
        &[],
        &["evt-cause"],
    )
    .expect("the cause is live at the write");
    assert_eq!(memory.links().len(), 1);

    durable_write(
        &mut memory,
        &mut security,
        "evt-unrelated",
        "an unrelated write triggers compaction",
        None,
        2_000,
    );
    assert!(
        memory.links().is_empty(),
        "an expired endpoint takes its edge with it: {:?}",
        memory.links()
    );
}

#[test]
fn the_link_view_leads_with_explicit_evidence() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig::default());
    durable_write(
        &mut memory,
        &mut security,
        "evt-old",
        "the claim that gets replaced",
        Some("game-x"),
        1_000,
    );
    let updater = durable_write_with_evidence(
        &mut memory,
        &mut security,
        "evt-new",
        "the claim that replaces it",
        Some("game-x"),
        2_000,
        &["evt-old"],
        &["evt-old"],
    )
    .expect("both relations are accepted");

    let view = memory.link_view(
        updater,
        &memory
            .entries()
            .iter()
            .map(|entry| entry.memory_id)
            .collect(),
        WorkingMemoryConfig::default().max_links_per_memory,
        3_000,
    );
    let kinds: Vec<MemoryLinkKind> = view.1.iter().map(|link| link.kind).collect();
    assert_eq!(
        kinds,
        [
            MemoryLinkKind::Supersedes,
            MemoryLinkKind::CausedBy,
            MemoryLinkKind::TemporalBefore,
        ],
        "explicit evidence leads the capped view, the derived spine trails it: {kinds:?}"
    );
}

#[test]
fn the_vocabulary_version_names_the_causal_kind() {
    assert_eq!(MEMORY_LINK_VOCAB_VERSION, "memory-link-v2");
    let memory = test_memory(WorkingMemoryConfig::default());
    assert_eq!(memory.link_snapshot(0).vocab_version, "memory-link-v2");
}

#[test]
fn an_expired_supersession_target_is_refused_and_stores_nothing() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    durable_write(&mut memory, &mut security, "evt-old", "old claim", None, 0);
    assert_eq!(
        memory.len(),
        1,
        "expired but not yet compacted: still physically present"
    );

    let error = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-update",
        "an update",
        None,
        2_500,
        &["evt-old"],
    )
    .expect_err(
        "an expired memory is already deleted for retrieval and cannot authorize an update \
         relation",
    );
    assert!(matches!(error, AdaptationError::InvalidInput(_)));
    assert_eq!(memory.len(), 1, "a refused write stores nothing");
    assert!(memory.links().is_empty(), "a refused write mints no links");
}

#[test]
fn an_accepted_write_keeps_every_explicit_supersession_it_declared() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        max_links: 2,
        max_links_per_memory: 2,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-a",
        "claim a",
        Some("game-x"),
        1_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-b",
        "claim b",
        Some("game-x"),
        2_000,
    );
    durable_write(
        &mut memory,
        &mut security,
        "evt-c",
        "claim c",
        Some("game-x"),
        3_000,
    );
    assert_eq!(
        memory.links().len(),
        2,
        "the store is at its cap from historical edges only"
    );

    let update_id = durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-d",
        "claim d",
        Some("game-x"),
        4_000,
        &["evt-a", "evt-b"],
    )
    .expect("write accepted");

    let declared = memory
        .links()
        .iter()
        .filter(|link| link.from == update_id && link.kind == MemoryLinkKind::Supersedes)
        .count();
    assert_eq!(
        declared, 2,
        "every explicit supersession of one accepted write survives it while its endpoints \
         remain retained"
    );
    assert!(
        memory
            .entries()
            .iter()
            .any(|entry| entry.source.event_id == "evt-a")
            && memory
                .entries()
                .iter()
                .any(|entry| entry.source.event_id == "evt-b"),
        "endpoints retained"
    );
    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("game-x"),
            limit: 10,
        },
        4_500,
    );
    for superseded in ["evt-a", "evt-b"] {
        assert!(
            results
                .iter()
                .find(|item| item.entry.source.event_id == superseded)
                .map(|item| item.stale)
                .unwrap_or_else(|| panic!("{superseded} retained")),
            "{superseded} is reported stale"
        );
    }
}

#[test]
fn deletion_prunes_by_node_identity_not_by_a_shared_source_event_id() {
    let mut security = security();
    let mut memory = test_memory(WorkingMemoryConfig {
        durable_ttl_ms: 1_000,
        working_ttl_ms: 10_000_000,
        ..WorkingMemoryConfig::default()
    });
    durable_write(
        &mut memory,
        &mut security,
        "evt-x",
        "durable copy",
        Some("topic-x"),
        0,
    );
    // The same source event id legitimately exists again as working
    // memory (the scheduler may replay one); it is a different node.
    let working = event(
        "evt-x",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-1"),
    );
    memory
        .remember_working(&working, "working copy", Some("topic-x"), 0)
        .expect("working copy");
    let durable_id = memory
        .entries()
        .iter()
        .find(|entry| {
            entry.source.event_id == "evt-x" && entry.retention == RetentionClass::Durable
        })
        .map(|entry| entry.memory_id)
        .expect("durable copy");
    let working_id = memory
        .entries()
        .iter()
        .find(|entry| entry.retention == RetentionClass::Working)
        .map(|entry| entry.memory_id)
        .expect("working copy");
    assert_ne!(durable_id, working_id);

    durable_write_superseding(
        &mut memory,
        &mut security,
        "evt-update",
        "an update",
        Some("topic-x"),
        100,
        &["evt-x"],
    )
    .expect("unique live durable target");
    assert_eq!(
        memory.links().len(),
        2,
        "one supersession plus one temporal spine edge to the durable node"
    );

    // Expire the *durable* node while the working copy with the same
    // source event id survives: the edges must go with the node they
    // pointed at, not stay alive behind the shared id.
    memory.compact(1_050);
    assert!(
        memory
            .entries()
            .iter()
            .any(|entry| entry.memory_id == working_id),
        "the working copy is still retained"
    );
    assert!(
        memory.links().is_empty(),
        "an edge may not outlive its node through a shared source event id"
    );
    let results = memory.relevant_linked(
        MemoryQuery {
            source_namespace: None,
            actor_id: None,
            topic: Some("topic-x"),
            limit: 10,
        },
        1_050,
    );
    let survivor = results
        .iter()
        .find(|item| item.entry.source.event_id == "evt-x")
        .expect("working copy retained");
    assert!(
        !survivor.stale,
        "the surviving copy is a different node and must not inherit the deleted node's \
         staleness"
    );
}

#[test]
fn keyed_actor_pseudonyms_are_deterministic_domain_separated_and_full_length() {
    let pseudonymizer = ActorPseudonymizer::new("v7", [0x11; 32]).expect("pseudonymizer");
    let first = pseudonymizer.pseudonymize("youtube.chat", "viewer-123");
    let repeated = pseudonymizer.pseudonymize("youtube.chat", "viewer-123");
    let other_source = pseudonymizer.pseudonymize("twitch.chat", "viewer-123");

    assert_eq!(first, repeated);
    assert_ne!(first, other_source);
    assert!(first.starts_with("actor:v7:"));
    assert_eq!(first.len(), "actor:v7:".len() + 64);
}

#[test]
fn pseudonym_key_and_version_rotation_break_historical_linkability() {
    let v1 = ActorPseudonymizer::new("v1", [0x21; 32]).expect("v1");
    let rotated_key = ActorPseudonymizer::new("v1", [0x22; 32]).expect("rotated key");
    let v2 = ActorPseudonymizer::new("v2", [0x21; 32]).expect("v2");
    let baseline = v1.pseudonymize("public-chat", "viewer-a");

    assert_ne!(
        baseline,
        rotated_key.pseudonymize("public-chat", "viewer-a")
    );
    assert_ne!(baseline, v2.pseudonymize("public-chat", "viewer-a"));
    assert!(
        v2.pseudonymize("public-chat", "viewer-a")
            .starts_with("actor:v2:")
    );
}

#[test]
fn pseudonym_key_is_required_nonzero_and_redacted_from_debug() {
    assert!(ActorPseudonymizer::new("v1", [0; 32]).is_err());
    let pseudonymizer = ActorPseudonymizer::new("v1", [0xab; 32]).expect("pseudonymizer");
    let debug = format!("{pseudonymizer:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("abababab"));
}

fn generated_fixture() -> PerformanceAsset {
    load_asset_file(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/performance-assets/valid/generated-dynamic.json"),
    )
    .expect("generated fixture")
}

fn generated_runtime() -> RuntimeCompatibility {
    RuntimeCompatibility {
        compiler_version: "0.1.0".to_owned(),
        voice_model: Some("voice-ja-v2".to_owned()),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        viseme_mapping: Some("ja-5vowel-v2".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aivtuber-adaptation-{name}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn retained_adaptation_state_plateaus_under_logical_churn() {
    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 2,
        working_ttl_ms: 10_000,
        durable_ttl_ms: 10_000,
        max_compaction_records: 3,
        ..WorkingMemoryConfig::default()
    });
    let chat = event(
        "evt-retention",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer:retention"),
    );
    for index in 0_u64..20 {
        memory
            .remember_working(&chat, &format!("claim-{index}"), Some("retention"), index)
            .expect("remember churn");
    }
    let memory_metrics = memory.retention_metrics();
    assert_eq!(memory_metrics.entries, 2);
    assert_eq!(memory_metrics.entries_high_water, 2);
    assert_eq!(memory_metrics.compactions, 3);
    assert_eq!(memory_metrics.compactions_high_water, 3);
    assert!(memory_metrics.compactions_evicted > 0);

    let mut engine = AdaptationEngine::with_retention(
        PromotionPolicy::default(),
        "bounded-retention-v1",
        42,
        AdaptationRetentionConfig {
            max_feedback_assets: 3,
            max_recent_groups: 2,
            max_decisions: 2,
        },
    )
    .expect("bounded adaptation engine");
    for index in 0_u64..10 {
        let mut asset = generated_fixture();
        asset.id = format!("dynamic.retention.{index}");
        asset.intent = format!("retention.intent.{index}");
        asset.variant_group = Some(format!("retention.group.{index}"));
        engine.record_use(&asset);
        engine.record_quality(&asset.id, true);
        engine.evaluate(&asset).expect("evaluate churn asset");
    }

    let metrics = engine.retention_metrics();
    assert_eq!(metrics.feedback_assets, 3);
    assert_eq!(metrics.feedback_assets_high_water, 3);
    assert!(metrics.feedback_assets_evicted > 0);
    assert_eq!(metrics.recent_groups, 2);
    assert_eq!(metrics.recent_groups_high_water, 2);
    assert!(metrics.recent_groups_evicted > 0);
    assert_eq!(metrics.decisions, 2);
    assert_eq!(metrics.decisions_high_water, 2);
    assert_eq!(metrics.decisions_evicted, 8);
}

#[test]
fn public_chat_cannot_mint_durable_memory_and_working_memory_is_pseudonymous() {
    let chat = event(
        "evt-chat",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-real-id"),
    );
    let mut security = security();
    let (decision, permit) = security.authorize_memory_write(&chat, None);
    assert_eq!(decision, MemoryWriteDecision::Denied);
    assert!(permit.is_none());

    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 2,
        working_ttl_ms: 100,
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    let stored = memory
        .remember_working(&chat, "  Likes   Rust  ", Some(" Coding "), 10)
        .expect("working entry");
    assert_eq!(stored.retention, RetentionClass::Working);
    assert_eq!(stored.write_decision, MemoryGateDecision::WorkingOnly);
    assert_eq!(stored.normalized_claim, "likes rust");
    assert_eq!(stored.topic.as_deref(), Some("coding"));
    assert_ne!(
        stored.source.pseudonymous_actor_id.as_deref(),
        Some("viewer-real-id")
    );
    assert!(
        stored
            .source
            .pseudonymous_actor_id
            .as_deref()
            .is_some_and(|value| value.starts_with("actor:test-v1:"))
    );
    let serialized = serde_json::to_string(stored).expect("serialize memory entry");
    assert!(!serialized.contains("viewer-real-id"));
    assert_eq!(stored.expires_at_ms, 110);

    let compacted = memory.compact(110);
    assert_eq!(compacted.expired_removed, 1);
    assert!(memory.is_empty());
}

#[test]
fn durable_memory_requires_gate_permit_and_preserves_gate_metadata() {
    let system = event("evt-system", SourceClass::System, TrustLevel::Trusted, None);
    let mut security = security();
    let (decision, permit) = security.authorize_memory_write(&system, None);
    assert_eq!(decision, MemoryWriteDecision::AllowedSystemSource);
    let permit = permit.expect("system permit");

    let mut memory = test_memory(WorkingMemoryConfig {
        max_entries: 4,
        working_ttl_ms: 100,
        durable_ttl_ms: 1_000,
        ..WorkingMemoryConfig::default()
    });
    let stored = memory
        .remember_durable(&permit, "stream is healthy", Some("health"), 50)
        .expect("durable entry");
    assert_eq!(stored.retention, RetentionClass::Durable);
    assert_eq!(
        stored.write_decision,
        MemoryGateDecision::AllowedSystemSource
    );
    assert_eq!(stored.source.event_id, "evt-system");
    assert_eq!(stored.source.source_class, SourceClass::System);
    assert_eq!(stored.source.trust_level, TrustLevel::Trusted);
    assert_eq!(stored.expires_at_ms, 1_050);
}

#[test]
fn working_memory_compaction_enforces_capacity_deterministically() {
    let config = WorkingMemoryConfig {
        max_entries: 2,
        working_ttl_ms: 10_000,
        durable_ttl_ms: 20_000,
        ..WorkingMemoryConfig::default()
    };
    let mut first = test_memory(config);
    let mut second = test_memory(config);

    for memory in [&mut first, &mut second] {
        for index in 1..=3 {
            let event = event(
                &format!("evt-{index}"),
                SourceClass::PublicChat,
                TrustLevel::Untrusted,
                Some("viewer"),
            );
            memory
                .remember_working(&event, &format!("claim {index}"), Some("topic"), index)
                .expect("remember");
        }
    }

    assert_eq!(first.len(), 2);
    assert_eq!(first.entries(), second.entries());
    assert_eq!(
        first
            .entries()
            .iter()
            .map(|entry| entry.source.event_id.as_str())
            .collect::<Vec<_>>(),
        vec!["evt-2", "evt-3"]
    );
    assert_eq!(
        first
            .compactions()
            .last()
            .expect("capacity compaction")
            .capacity_removed,
        1
    );
}

#[test]
fn viewer_and_topic_relevance_is_deterministic_without_raw_actor_identity() {
    let mut memory = test_memory(WorkingMemoryConfig {
        ..WorkingMemoryConfig::default()
    });
    let a = event(
        "evt-a",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-a"),
    );
    let b = event(
        "evt-b",
        SourceClass::PublicChat,
        TrustLevel::Untrusted,
        Some("viewer-b"),
    );
    memory
        .remember_working(&a, "likes rust", Some("coding"), 10)
        .expect("a");
    memory
        .remember_working(&b, "likes music", Some("music"), 20)
        .expect("b");

    let result = memory.relevant(
        MemoryQuery {
            source_namespace: Some("public-chat"),
            actor_id: Some("viewer-a"),
            topic: Some("coding"),
            limit: 2,
        },
        30,
    );
    assert_eq!(result[0].source.event_id, "evt-a");
    assert!(
        memory
            .entries()
            .iter()
            .all(|entry| entry.source.pseudonymous_actor_id.as_deref() != Some("viewer-a"))
    );
}

#[test]
fn promotion_and_invalidation_follow_explicit_frequency_quality_policy() {
    let dir = TestDir::new("promotion");
    let asset = generated_fixture();
    let id = asset.id.clone();
    let provenance = asset.provenance.clone();
    let mut store = AssetStore::new(dir.path(), generated_runtime());
    store
        .insert_hot(asset.clone())
        .expect("hot generated asset");

    let policy = PromotionPolicy {
        min_uses: 2,
        min_quality_labels: 2,
        min_quality_ratio: 0.75,
        invalidate_after_negative_labels: 2,
        recent_variant_window: 1,
    };
    let mut adaptation = AdaptationEngine::new(policy, "promotion-v1", 42).expect("adaptation");

    adaptation.record_use(&asset);
    adaptation.record_quality(&id, true);
    assert_eq!(
        adaptation.apply(&mut store, &id).expect("keep hot"),
        AppliedAdaptation::KeptHot
    );

    adaptation.record_use(&asset);
    adaptation.record_quality(&id, true);
    let promoted = adaptation.apply(&mut store, &id).expect("promote");
    let path = match promoted {
        AppliedAdaptation::Promoted(path) => path,
        other => panic!("expected promotion, got {other:?}"),
    };
    let persisted = load_asset_file(&path).expect("persisted");
    assert_eq!(persisted.provenance, provenance);
    assert!(persisted.compatibility.check(store.runtime()).is_usable());

    adaptation.record_quality(&id, false);
    adaptation.record_quality(&id, false);
    assert_eq!(
        adaptation.apply(&mut store, &id).expect("invalidate"),
        AppliedAdaptation::Invalidated
    );
    assert!(!path.exists());
    assert!(store.hot_get(&id).is_none());

    let decisions = adaptation.decisions();
    assert_eq!(
        decisions
            .iter()
            .map(|record| record.decision)
            .collect::<Vec<_>>(),
        vec![
            AdaptationDecisionKind::KeepHot,
            AdaptationDecisionKind::Promote,
            AdaptationDecisionKind::Invalidate
        ]
    );
    assert!(
        decisions
            .iter()
            .all(|record| record.policy_version == "promotion-v1")
    );
    assert!(decisions.iter().all(|record| record.seed == 42));
}

#[test]
fn adaptation_replay_and_anti_repetition_are_seed_deterministic() {
    let mut first =
        AdaptationEngine::new(PromotionPolicy::default(), "policy-v1", 4242).expect("first");
    let mut second =
        AdaptationEngine::new(PromotionPolicy::default(), "policy-v1", 4242).expect("second");

    let mut a = generated_fixture();
    a.id = "dynamic.variant.a".to_owned();
    a.variant_group = Some("dynamic.reply".to_owned());
    let mut b = a.clone();
    b.id = "dynamic.variant.b".to_owned();
    let candidates = vec![a.clone(), b.clone()];

    first.record_use(&a);
    second.record_use(&a);
    first.record_quality(&a.id, true);
    second.record_quality(&a.id, true);

    let first_decision = first.evaluate(&a).expect("decision");
    let second_decision = second.evaluate(&a).expect("decision");
    assert_eq!(first_decision, second_decision);
    assert_eq!(
        first_decision.to_json_bytes().expect("serialize decision"),
        second_decision.to_json_bytes().expect("serialize decision")
    );
    let selected_first = first
        .select_variant("dynamic.reply", &candidates, 9)
        .expect("variant");
    let selected_second = second
        .select_variant("dynamic.reply", &candidates, 9)
        .expect("variant");
    assert_eq!(selected_first.id, selected_second.id);
    assert_eq!(selected_first.id, "dynamic.variant.b");
}

// Issue #146: the adaptation-side byte-cap truncation applied to memory
// claims and topics, which come from untrusted input. This is a separate
// implementation from `crates/app/src/routing.rs::truncate_utf8` (it
// clamps with `.min(value.len())` and guards `end > 0`), so it gets its own
// property coverage rather than assuming the other one behaves the same.
#[test]
fn truncate_utf8_respects_the_byte_cap_across_unicode_widths() {
    for value in [
        "abcdef",
        "aあいう",
        "a\u{0301}bc",
        "a🎉🎊bc",
        "a\u{10FFFF}b",
    ] {
        for cap in 0..=(value.len() + 3) {
            let truncated = truncate_utf8(value, cap);
            assert!(
                truncated.len() <= cap,
                "cap {cap} produced {} bytes from a {}-byte input",
                truncated.len(),
                value.len()
            );
            assert!(
                std::str::from_utf8(truncated.as_bytes()).is_ok(),
                "cap {cap} produced invalid UTF-8 for {value:?}"
            );
            assert!(
                value.starts_with(&truncated),
                "cap {cap} returned {truncated:?}, which is not a prefix of {value:?}"
            );
        }
    }
}

#[test]
fn truncate_utf8_returns_short_inputs_verbatim() {
    for value in ["", "a", "hello", "こんにちは", "🎉"] {
        // Only caps that admit the whole input; a cap below the byte length
        // must still truncate, including cap 0.
        for cap in value.len()..=(value.len() + 2) {
            assert_eq!(truncate_utf8(value, cap), value, "cap {cap} for {value:?}");
        }
    }
}

/// Keeps the longest valid prefix rather than merely *a* valid prefix, and
/// pins the `cap == 0` case that the `end > 0` guard exists to handle.
#[test]
fn truncate_utf8_keeps_the_longest_valid_prefix() {
    let value = "aあい";
    assert_eq!(value.len(), 7);
    assert_eq!(truncate_utf8(value, 0), "");
    assert_eq!(truncate_utf8(value, 1), "a");
    assert_eq!(truncate_utf8(value, 2), "a");
    assert_eq!(truncate_utf8(value, 3), "a");
    assert_eq!(truncate_utf8(value, 4), "aあ");
    assert_eq!(truncate_utf8(value, 5), "aあ");
    assert_eq!(truncate_utf8(value, 6), "aあ");
    assert_eq!(truncate_utf8(value, 7), value);
    assert_eq!(truncate_utf8(value, 99), value);
}
