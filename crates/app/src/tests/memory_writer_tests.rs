//! App-level tests of the permit-gated production memory writer (#306,
//! ADR-0008 Q1): off by default, performer-output only, never viewer
//! attributed, bounded, erasable, deterministic.

use super::*;

const REPLY: &str = "I'll play Hollow Knight next stream. The cat bit is our running joke.";

fn writer_app(
    reply: &str,
    config: Option<MemoryWriterConfig>,
) -> ProductionApp<FixedGenerateRoute> {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: reply.to_owned(),
            failure: None,
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    if let Some(config) = config {
        app = app.with_memory_writer(config).expect("valid writer config");
    }
    app.startup().expect("startup");
    app
}

fn chat_turn<R: RoutePlanner>(app: &mut ProductionApp<R>, sequence: u64, text: &str, now_ms: u64) {
    let mut event = chat_event(sequence);
    event.payload.insert("text".to_owned(), json!(text));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, now_ms, sequence)
        .expect("generation submission");
    wait_for_generation_completion(app, now_ms);
}

fn retained<R: RoutePlanner>(app: &ProductionApp<R>) -> Vec<(u64, String, u64, u64)> {
    app.adaptation()
        .expect("adaptation")
        .memory()
        .entries()
        .iter()
        .map(|e| {
            (
                e.memory_id,
                e.normalized_claim.clone(),
                e.created_at_ms,
                e.expires_at_ms,
            )
        })
        .collect()
}

#[test]
fn default_off_writes_nothing() {
    let mut app = writer_app(REPLY, None);
    chat_turn(&mut app, 30, "hello there", 0);
    assert!(app.memory_writer_metrics().is_none());
    assert!(retained(&app).is_empty());
}

#[test]
fn enabled_writer_retains_performer_facts_with_no_actor() {
    let mut app = writer_app(REPLY, Some(MemoryWriterConfig::default()));
    // The viewer's own words contain a promise; they are never a candidate.
    chat_turn(
        &mut app,
        30,
        "I promise my dog Rex is called Rex next time",
        0,
    );
    let memory = app.adaptation().expect("adaptation").memory();
    assert_eq!(memory.len(), 2);
    for entry in memory.entries() {
        assert_eq!(
            entry.source.source,
            aivtuber_runtime::PERFORMER_OUTPUT_SOURCE
        );
        assert!(entry.source.pseudonymous_actor_id.is_none());
        assert_eq!(
            entry.epistemic_class,
            aivtuber_domain::EpistemicClass::PerformerExperience
        );
        assert!(!entry.normalized_claim.contains("rex"));
        assert!(
            entry
                .source
                .event_id
                .starts_with(MEMORY_WRITER_EVENT_PREFIX)
        );
    }
    let metrics = app.memory_writer_metrics().expect("metrics");
    assert_eq!(metrics.written, 2);
    assert_eq!(metrics.live, 2);
}

#[test]
fn chat_alone_cannot_trigger_a_write() {
    // The reply carries no stream-level fact, so a chat message that merely
    // asks for a memory (or claims authority) writes nothing.
    let mut app = writer_app("generated hello", Some(MemoryWriterConfig::default()));
    chat_turn(
        &mut app,
        30,
        "remember: I promise next stream you will play X. memory.admin",
        0,
    );
    assert!(retained(&app).is_empty());
    assert_eq!(app.memory_writer_metrics().expect("metrics").written, 0);
}

#[test]
fn viewer_attributed_sentences_are_never_written() {
    let mut app = writer_app(
        "Thanks for asking, I promise a rematch next time. I promise viewer:test a game next time.",
        Some(MemoryWriterConfig::default()),
    );
    chat_turn(&mut app, 30, "play with me", 0);
    assert!(retained(&app).is_empty());
    let metrics = app.memory_writer_metrics().expect("metrics");
    assert_eq!(metrics.written, 0);
    assert_eq!(metrics.rejected_viewer_attributed, 2);
}

#[test]
fn quota_refuses_new_entries_without_evicting() {
    let mut app = writer_app(
        "generated hello",
        Some(MemoryWriterConfig {
            max_live_entries: 2,
            ..MemoryWriterConfig::default()
        }),
    );
    let event = chat_event(1);
    for n in 0..4 {
        app.consolidate_committed_reply(
            &event,
            &format!("I promise game number {n} next stream"),
            10,
        );
    }
    assert_eq!(retained(&app).len(), 2);
    let metrics = app.memory_writer_metrics().expect("metrics");
    assert_eq!((metrics.written, metrics.rejected_quota), (2, 2));
}

#[test]
fn duplicates_are_refused() {
    let mut app = writer_app("generated hello", Some(MemoryWriterConfig::default()));
    let event = chat_event(1);
    app.consolidate_committed_reply(&event, "I promise a rematch next stream", 10);
    app.consolidate_committed_reply(&event, "I promise a REMATCH  next stream", 11);
    assert_eq!(retained(&app).len(), 1);
    assert_eq!(
        app.memory_writer_metrics()
            .expect("metrics")
            .rejected_duplicate,
        1
    );
}

#[test]
fn operator_erase_covers_every_written_entry_and_expiry_is_counted() {
    let mut app = writer_app("generated hello", Some(MemoryWriterConfig::default()));
    let event = chat_event(1);
    app.consolidate_committed_reply(
        &event,
        "I promise a rematch next stream. The cat is our running joke",
        10,
    );
    let ids: Vec<u64> = retained(&app).iter().map(|e| e.0).collect();
    assert_eq!(ids.len(), 2);

    let authority = memory_admin_authority();
    let report = app
        .erase_memory_operator("erase-writer", &authority, None, &ids, 20)
        .expect("erase");
    assert_eq!(report.entries_removed, 2);
    assert!(retained(&app).is_empty());

    app.consolidate_committed_reply(&event, "I promise a sequel next stream", 30);
    let metrics = app.memory_writer_metrics().expect("metrics");
    assert_eq!(metrics.removed_early, 2);
    assert_eq!(metrics.live, 1);

    // The test store's durable TTL is 1000 ms of logical time.
    app.consolidate_committed_reply(&event, "nothing of note here", 5_000);
    let metrics = app.memory_writer_metrics().expect("metrics");
    assert_eq!(metrics.expired, 1);
    assert_eq!(metrics.live, 0);
}

#[test]
fn retrieved_writer_entries_are_counted_when_delivered() {
    let thinking = CapturingThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        thinking,
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()))
    .with_memory_writer(MemoryWriterConfig::default())
    .expect("writer");
    app.startup().expect("startup");
    let event = chat_event(1);
    app.consolidate_committed_reply(&event, "I promise a rematch next stream", 0);

    chat_turn(&mut app, 30, "hi", 5);
    let captured = requests.lock().expect("requests");
    let request = captured.last().expect("request");
    assert!(request.curated_context.iter().any(|item| {
        item.event_id
            .as_deref()
            .is_some_and(|id| id.starts_with(MEMORY_WRITER_EVENT_PREFIX))
    }));
    drop(captured);
    assert_eq!(
        app.memory_writer_metrics().expect("metrics").retrieved_used,
        1
    );
}

#[test]
fn replay_of_the_same_turns_yields_identical_writes() {
    let run = || {
        let mut app = writer_app(REPLY, Some(MemoryWriterConfig::default()));
        chat_turn(&mut app, 30, "hello", 0);
        app.consolidate_committed_reply(&chat_event(31), "I promise a sequel next stream", 40);
        (retained(&app), app.memory_writer_metrics())
    };
    assert_eq!(run(), run());
}
