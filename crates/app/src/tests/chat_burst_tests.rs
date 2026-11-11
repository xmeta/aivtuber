//! App-level tests of live chat-burst selection and deferred replies (issue
//! #75 S1, #78 S2). The falsifier replays the #78 100x burst through
//! `process_content_bytes` against a performer that is busy most of the time.

use super::*;
use aivtuber_domain::{StreamScenario, generate_scenario_trace, scenario_instant_ms};
use aivtuber_reflex::BurstConfig;
use aivtuber_runtime::IngressPolicy;
use aivtuber_scheduler::DeferredConfig;

const ASSETS: [&str; 6] = [
    "reaction.agree.01",
    "reaction.agree.02",
    "reaction.neutral.01",
    "reaction.neutral.02",
    "reaction.surprise.01",
    "reaction.surprise.02",
];

/// Cycles through distinct assets so a refusal is never the per-asset cooldown.
#[derive(Clone, Default)]
struct CyclingRoute {
    next: usize,
}

impl RoutePlanner for CyclingRoute {
    fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        let asset = ASSETS[self.next % ASSETS.len()];
        self.next += 1;
        Ok(PlaybackRoute::AssetId(asset.to_owned()))
    }
}

fn command(capability: Capability, action: &str) -> AuthenticatedControlCommand {
    let secret = [0x47_u8; 32];
    LocalControlIngress::new(
        "local-test",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([capability]),
        ControlSecret::new(secret),
    )
    .expect("control ingress")
    .authenticate(
        OperatorCommandInput {
            event_id: format!("evt-{action}"),
            correlation_id: "corr-chat-burst".to_owned(),
            sequence: 97,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )
    .expect("authenticated command")
}

fn burst_app(config: ChatBurstConfig) -> ProductionApp<CyclingRoute> {
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            // The burst falsifier pushes ~100 events a second from many
            // sources; the per-source limiter is not what is under test.
            content_rate_per_second: 1_000_000.0,
            content_burst: 1_000_000,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
    )
    .expect("security runtime");
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        CyclingRoute::default(),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_chat_burst(config)
    .expect("chat burst config");
    app.startup().expect("startup");
    app
}

fn config() -> ChatBurstConfig {
    ChatBurstConfig::default()
}

fn chat(id: u64, actor: &str, text: &str) -> EventEnvelope {
    let mut event = chat_event(id);
    event.actor_id = Some(actor.to_owned());
    event
        .payload
        .insert("text".to_owned(), serde_json::json!(text));
    event.payload.remove("intent");
    event
}

fn send(
    app: &mut ProductionApp<CyclingRoute>,
    event: &EventEnvelope,
    at_ms: u64,
) -> ContentProcessOutcome {
    let raw = serde_json::to_vec(event).expect("event json");
    app.process_content_bytes(&raw, at_ms, at_ms ^ 0x55)
        .expect("content")
}

fn outcomes(app: &mut ProductionApp<CyclingRoute>) -> Vec<(String, ChatOutcome)> {
    app.take_chat_burst_records(usize::MAX)
        .into_iter()
        .map(|record| (record.event_id, record.outcome))
        .collect()
}

#[test]
fn without_the_flag_nothing_changes() {
    let mut app = app(
        FixedAssetRoute("reaction.agree.01"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    assert!(app.chat_burst_metrics().is_none());
    let outcome = send_plain(&mut app, &chat_event(1), 0);
    assert!(outcome.playback.is_some(), "per-event path replies at once");
    assert!(app.take_chat_burst_records(8).is_empty());
    assert!(app.take_deferred_reply_reports(8).is_empty());
    assert!(app.retention_snapshot().chat_burst.is_none());
    // Calling the loop hook without the lane is a no-op.
    app.service_chat_burst(100, 1);
}

fn send_plain(
    app: &mut ProductionApp<FixedAssetRoute>,
    event: &EventEnvelope,
    at_ms: u64,
) -> ContentProcessOutcome {
    let raw = serde_json::to_vec(event).expect("event json");
    app.process_content_bytes(&raw, at_ms, 3).expect("content")
}

#[test]
fn quiet_chat_replies_at_once_and_a_busy_performer_defers_the_next_reply() {
    let mut app = burst_app(config());
    let first = send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let playback = first.playback.expect("the idle performer answers at once");
    assert_eq!(
        outcomes(&mut app),
        vec![("evt-1".to_owned(), ChatOutcome::DispatchedNow)]
    );

    // The performer is now speaking: the next reply waits instead of talking
    // over it, and is not answered by this call.
    let second = send(&mut app, &chat(2, "viewer:b", "how did you do that?"), 10);
    assert!(second.playback.is_none());
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.pending, 1);
    assert_eq!(metrics.deferred_speaking, 1);
    assert!(outcomes(&mut app).is_empty(), "no terminal outcome yet");

    // Once the speech has finished the deferred reply is dispatched.
    let free_at = playback.plan.end_at_ms() + 50;
    app.tick(free_at);
    app.service_chat_burst(free_at, 9);
    let reports = app.take_deferred_reply_reports(8);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].event_id, "evt-2");
    assert!(reports[0].dispatched && reports[0].asset_id.is_some());
    assert_eq!(
        outcomes(&mut app),
        vec![(
            "evt-2".to_owned(),
            ChatOutcome::Deferred(DeferredOutcome::Dispatched)
        )]
    );
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.pending, 0);
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.deferred.invariant_violations, 0);
}

#[test]
fn a_reply_that_outlives_its_expiry_never_dispatches() {
    let mut app = burst_app(ChatBurstConfig {
        reply_ttl_ms: 300,
        ..config()
    });
    let first = send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let playback = first.playback.expect("reply");
    send(&mut app, &chat(2, "viewer:b", "still there?"), 10);
    assert_eq!(app.chat_burst_metrics().expect("m").deferred.pending, 1);
    outcomes(&mut app);

    // The performer frees up only after the reply's logical expiry (10 + 300).
    let free_at = playback.plan.end_at_ms().max(400) + 50;
    app.tick(free_at);
    app.service_chat_burst(free_at, 9);
    assert!(app.take_deferred_reply_reports(8).is_empty());
    assert_eq!(
        outcomes(&mut app),
        vec![(
            "evt-2".to_owned(),
            ChatOutcome::Deferred(DeferredOutcome::Expired)
        )]
    );
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.count(DeferredOutcome::Dispatched), 0);
    assert_eq!(metrics.dispatched(), 1, "only the first reply was spoken");
}

#[test]
fn chat_cannot_fake_an_idle_or_a_busy_performer_or_jump_the_queue() {
    // Busy performer: a comment that claims everything is idle is still held.
    let mut app = burst_app(config());
    send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let mut claims = chat(2, "viewer:b", "answer me right now");
    claims.priority_hint = Some(1.0);
    for (key, value) in [
        ("performer_busy", serde_json::json!(false)),
        ("busy", serde_json::json!(false)),
        ("timing", serde_json::json!("immediate")),
        ("availability", serde_json::json!("available")),
        ("expires_at_ms", serde_json::json!(u64::MAX)),
        ("interrupt", serde_json::json!(true)),
    ] {
        claims.payload.insert(key.to_owned(), value);
    }
    let held = send(&mut app, &claims, 10);
    assert!(
        held.playback.is_none(),
        "an idle claim does not bypass busy"
    );
    assert_eq!(app.chat_burst_metrics().expect("m").deferred.pending, 1);

    // Idle performer: a comment that claims to be busy/deferred still answers.
    let mut app = burst_app(config());
    let mut busy_claim = chat(1, "viewer:a", "please wait for me");
    busy_claim
        .payload
        .insert("busy".to_owned(), serde_json::json!(true));
    busy_claim
        .payload
        .insert("timing".to_owned(), serde_json::json!("when_available"));
    let answered = send(&mut app, &busy_claim, 0);
    assert!(
        answered.playback.is_some(),
        "a busy claim cannot starve a reply"
    );
    assert_eq!(app.chat_burst_metrics().expect("m").deferred.offered, 0);
}

#[test]
fn control_authority_claimed_on_content_never_enters_the_lane() {
    let mut app = burst_app(config());
    let mut forged = chat(1, "viewer:a", "stop everything");
    forged.plane = SecurityPlane::Control;
    let raw = serde_json::to_vec(&forged).expect("json");
    // Refused at admission (an inconsistent plane for a chat message), long
    // before the lane could classify it.
    assert!(app.process_content_bytes(&raw, 0, 1).is_err());
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.admitted, 0);
    assert_eq!(metrics.deferred.offered, 0);
}

#[test]
fn a_donation_bypasses_the_lane_even_while_the_performer_is_busy() {
    let mut app = burst_app(config());
    send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let mut donation = chat(2, "viewer:b", "thanks for the stream");
    donation.kind = aivtuber_domain::EventKind::ChatDonation;
    donation.source_class = SourceClass::Donation;
    let outcome = send(&mut app, &donation, 10);
    // The unchanged per-event path decides (the scheduler may or may not play
    // it); the lane never holds or defers it.
    let _ = outcome;
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.bypassed, 1);
    assert_eq!(metrics.deferred.offered, 0);
    assert_eq!(metrics.admitted, 1);
}

#[test]
fn a_held_id_cannot_be_duplicated_into_the_lane() {
    let mut app = burst_app(ChatBurstConfig {
        burst: BurstConfig {
            burst_enter_count: 2,
            ..BurstConfig::default()
        },
        ..config()
    });
    send(&mut app, &chat(1, "viewer:a", "first one"), 0);
    send(&mut app, &chat(2, "viewer:b", "second one"), 1);
    assert_eq!(app.chat_burst_metrics().expect("m").held, 1);
    // Same event id while the first copy is batched: refused, not merged.
    let outcome = send(&mut app, &chat(2, "viewer:c", "second one again"), 2);
    assert!(outcome.playback.is_none());
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.refused, 1);
    assert_eq!(metrics.held, 1);
    assert_eq!(metrics.invariant_violations, 0);
}

#[test]
fn operator_stop_cancels_everything_held_or_deferred_and_nothing_speaks_after() {
    let mut app = burst_app(ChatBurstConfig {
        burst: BurstConfig {
            burst_enter_count: 2,
            ..BurstConfig::default()
        },
        ..config()
    });
    let first = send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let playback = first.playback.expect("reply");
    // Busy performer + a burst: some replies are deferred, some are batched.
    for id in 2..=7 {
        send(
            &mut app,
            &chat(id, &format!("viewer:{id}"), "what a play?"),
            10 + id,
        );
    }
    let before = app.chat_burst_metrics().expect("metrics");
    assert!(before.deferred.pending + before.held > 0, "state to cancel");

    let (outcome, _) = app
        .handle_control_reporting(&command(Capability::PerformerStop, "performer.stop"), 30)
        .expect("operator stop");
    assert!(matches!(outcome, ControlOutcome::Stopped { .. }));
    let after = app.chat_burst_metrics().expect("metrics");
    assert_eq!(after.deferred.pending, 0, "stop cancels the inbox at once");
    assert_eq!(
        after.deferred.count(DeferredOutcome::OperatorCancelled) as usize,
        before.deferred.pending
    );

    // Past the old performance and past the batch window: nothing held before
    // the stop may speak, and every comment is accounted exactly once.
    let late = playback.plan.end_at_ms() + 10_000;
    app.tick(late);
    app.service_chat_burst(late, 3);
    assert!(app.take_deferred_reply_reports(16).is_empty());
    app.shutdown(late + 1);
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.held, 0);
    assert_eq!(metrics.deferred.pending, 0);
    assert_eq!(metrics.accounted(), metrics.admitted);
    assert_eq!(metrics.deferred.count(DeferredOutcome::Dispatched), 0);
    assert_eq!(metrics.invariant_violations, 0);
}

#[test]
fn muted_output_holds_replies_until_they_expire() {
    let mut app = burst_app(config());
    app.handle_control_reporting(&command(Capability::PerformerMute, "performer.mute"), 0)
        .expect("mute");
    let outcome = send(&mut app, &chat(1, "viewer:a", "anyone there?"), 10);
    assert!(outcome.playback.is_none());
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred_muted, 1);
    app.service_chat_burst(10 + config().reply_ttl_ms + 1, 2);
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.count(DeferredOutcome::Expired), 1);
    assert_eq!(metrics.dispatched(), 0);
}

#[test]
fn an_erased_actor_never_gets_a_deferred_reply() {
    let mut app = burst_app(config());
    let first = send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let playback = first.playback.expect("reply");
    send(&mut app, &chat(2, "viewer:erasing", "remember me?"), 10);
    send(
        &mut app,
        &chat(3, "viewer:staying", "what is this game?"),
        20,
    );
    assert_eq!(app.chat_burst_metrics().expect("m").deferred.pending, 2);
    outcomes(&mut app);

    // The viewer's own erase command is consumed before the lane.
    send(&mut app, &chat(4, "viewer:erasing", "!forgetme"), 30);
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.pending, 1);
    assert_eq!(metrics.deferred.count(DeferredOutcome::Erased), 1);
    assert_eq!(
        outcomes(&mut app),
        vec![(
            "evt-2".to_owned(),
            ChatOutcome::Deferred(DeferredOutcome::Erased)
        )]
    );

    let free_at = playback.plan.end_at_ms() + 50;
    app.tick(free_at);
    app.service_chat_burst(free_at, 4);
    let reports = app.take_deferred_reply_reports(8);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].event_id, "evt-3", "only the other viewer");
}

#[test]
fn an_erasing_actor_in_a_batch_is_accounted_as_erased() {
    let mut app = burst_app(ChatBurstConfig {
        burst: BurstConfig {
            burst_enter_count: 2,
            max_candidates: 8,
            ..BurstConfig::default()
        },
        ..config()
    });
    send(&mut app, &chat(1, "viewer:a", "first"), 0);
    send(&mut app, &chat(2, "viewer:erasing", "batched one"), 1);
    send(&mut app, &chat(3, "viewer:b", "batched two"), 2);
    assert_eq!(app.chat_burst_metrics().expect("m").held, 2);
    send(&mut app, &chat(4, "viewer:erasing", "!forgetme"), 3);
    // Flush the window: the erased comment must resolve to Erased, never speak.
    app.service_chat_burst(10_000, 5);
    let records = outcomes(&mut app);
    assert!(
        records.contains(&("evt-2".to_owned(), ChatOutcome::ErasedBeforeSelection)),
        "records: {records:?}"
    );
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.held, 0);
}

#[test]
fn the_security_gate_is_rerun_before_a_deferred_reply_dispatches() {
    let mut app = burst_app(config());
    let first = send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    let playback = first.playback.expect("reply");
    send(&mut app, &chat(2, "viewer:b", "what is going on?"), 10);
    assert_eq!(app.chat_burst_metrics().expect("m").deferred.pending, 1);
    outcomes(&mut app);

    // The operator tightens the ingress ceiling while the reply waits: public
    // chat is no longer admitted, so the held reply must be refused, not sent.
    app.security.set_ingress_policy(IngressPolicy::new(
        TrustLevel::Trusted,
        &[SourceClass::Operator],
    ));
    let free_at = playback.plan.end_at_ms() + 50;
    app.tick(free_at);
    app.service_chat_burst(free_at, 6);
    assert!(app.take_deferred_reply_reports(8).is_empty());
    assert_eq!(
        outcomes(&mut app),
        vec![(
            "evt-2".to_owned(),
            ChatOutcome::Deferred(DeferredOutcome::RevalidationRefused)
        )]
    );
}

#[test]
fn a_burst_selects_a_bounded_top_k_and_every_other_comment_is_accounted() {
    let mut app = burst_app(ChatBurstConfig {
        burst: BurstConfig {
            burst_enter_count: 2,
            max_candidates: 12,
            top_k: 2,
            max_per_actor: 1,
            ..BurstConfig::default()
        },
        ..config()
    });
    // 14 distinct viewers inside one window; one asks a question.
    for id in 1..=14_u64 {
        let text = if id == 9 {
            "who made this game?"
        } else {
            "lol"
        };
        send(&mut app, &chat(id, &format!("viewer:{id}"), text), id);
    }
    app.service_chat_burst(5_000, 1);
    let records = outcomes(&mut app);
    let mut seen = BTreeMap::new();
    for (id, _) in &records {
        *seen.entry(id.clone()).or_insert(0_u32) += 1;
    }
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert!(seen.values().all(|count| *count == 1), "one outcome each");
    // The quiet first comment plus the batched ones; whatever is not yet
    // decided is deferred behind the busy performer.
    assert_eq!(
        metrics.accounted() + metrics.deferred.pending as u64,
        metrics.admitted
    );
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.burst_invariant_violations, 0);
    // Selection is Top-K: far fewer replies than comments.
    assert!(metrics.deferred.offered + metrics.terminal[0] <= 6);
}

fn burst_100x() -> Vec<EventEnvelope> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/scenarios/chat-burst-10x.json");
    let mut scenario: StreamScenario =
        serde_json::from_slice(&std::fs::read(path).expect("scenario fixture")).expect("parse");
    scenario.scenario_id = "chat-burst-100x".to_owned();
    scenario.phases[0].events_per_minute = 60.0;
    scenario.phases[1].events_per_minute = 6000.0;
    scenario.phases[1].duration_ms = 20_000;
    scenario.stream_duration_ms = 80_000;
    generate_scenario_trace(&scenario)
        .expect("100x trace")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Issue #75/#78 falsifier: the 100x chat burst, with a performer that is busy
/// most of the time, leaves no admitted comment without a terminal outcome,
/// never dispatches an expired reply, and never delays operator control.
#[test]
fn the_100x_burst_under_a_busy_performer_is_fully_accounted() {
    let metrics = falsify_burst(
        ChatBurstConfig {
            reply_ttl_ms: 4_000,
            ..config()
        },
        Some(400),
    );
    assert!(
        metrics.deferred.count(DeferredOutcome::OperatorCancelled) > 0,
        "stops cancelled deferred replies"
    );
}

/// The same burst without operator stops and with a short time-to-live, so
/// waiting replies run into their expiry instead of being cancelled.
#[test]
fn the_100x_burst_expires_stale_replies_instead_of_answering_late() {
    let metrics = falsify_burst(
        ChatBurstConfig {
            reply_ttl_ms: 1_000,
            ..config()
        },
        None,
    );
    assert!(
        metrics.deferred.count(DeferredOutcome::Expired) + metrics.terminal[9] > 0,
        "stale replies were expired, not spoken"
    );
}

fn falsify_burst(cfg: ChatBurstConfig, stop_every: Option<usize>) -> ChatBurstMetrics {
    let trace = burst_100x();
    let mut app = burst_app(cfg);
    let mut terminals: BTreeMap<String, Vec<ChatOutcome>> = BTreeMap::new();
    let mut dispatch_at: BTreeMap<String, u64> = BTreeMap::new();
    let mut arrival_at: BTreeMap<String, u64> = BTreeMap::new();
    let mut operator_checks = 0;
    let mut stops = 0;
    let mut last_ms = 0;

    let drain = |app: &mut ProductionApp<CyclingRoute>,
                 terminals: &mut BTreeMap<String, Vec<ChatOutcome>>,
                 dispatch_at: &mut BTreeMap<String, u64>| {
        for record in app.take_chat_burst_records(usize::MAX) {
            if matches!(
                record.outcome,
                ChatOutcome::DispatchedNow | ChatOutcome::Deferred(DeferredOutcome::Dispatched)
            ) {
                dispatch_at.insert(record.event_id.clone(), record.at_ms);
            }
            terminals
                .entry(record.event_id)
                .or_default()
                .push(record.outcome);
        }
        let _ = app.take_deferred_reply_reports(usize::MAX);
    };

    for (index, event) in trace.iter().enumerate() {
        let now = scenario_instant_ms(&event.observed_at).expect("instant");
        last_ms = last_ms.max(now);
        arrival_at.insert(event.event_id.clone(), now);
        let raw = serde_json::to_vec(event).expect("json");
        let _ = app.process_content_bytes(&raw, now, index as u64);
        app.service_chat_burst(now, index as u64);
        drain(&mut app, &mut terminals, &mut dispatch_at);

        // Operator emergency control arrives mid-burst. It takes effect
        // synchronously, however deep the inbox is, and is never queued
        // behind content or deferred replies.
        if stop_every.is_some_and(|every| index % every == every - 1) {
            let before = app.chat_burst_metrics().expect("metrics");
            let started = Instant::now();
            let (outcome, _) = app
                .handle_control_reporting(
                    &command(Capability::PerformerStop, "performer.stop"),
                    now,
                )
                .expect("operator stop");
            assert!(matches!(outcome, ControlOutcome::Stopped { .. }));
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "operator stop must not wait on content paths"
            );
            let after = app.chat_burst_metrics().expect("metrics");
            assert_eq!(after.deferred.pending, 0, "stop empties the inbox now");
            assert_eq!(
                after.deferred.count(DeferredOutcome::OperatorCancelled),
                before.deferred.count(DeferredOutcome::OperatorCancelled)
                    + before.deferred.pending as u64
            );
            stops += 1;
            operator_checks += 1;
            drain(&mut app, &mut terminals, &mut dispatch_at);
        }
    }

    // Let every remaining window flush and every wait expire, then shut down.
    let end = last_ms + 120_000;
    app.tick(end);
    app.service_chat_burst(end, 99);
    drain(&mut app, &mut terminals, &mut dispatch_at);
    app.shutdown(end + 1);
    drain(&mut app, &mut terminals, &mut dispatch_at);

    let metrics = app.chat_burst_metrics().expect("metrics");
    if stop_every.is_some() {
        assert!(
            operator_checks >= 3 && stops >= 3,
            "operator control was exercised"
        );
    }
    assert!(
        metrics.admitted > 500,
        "a real burst was admitted: {}",
        metrics.admitted
    );

    // 1. Every admitted comment has exactly one terminal outcome.
    assert_eq!(metrics.held, 0);
    assert_eq!(metrics.deferred.pending, 0);
    assert_eq!(metrics.deferred.in_flight, 0);
    assert_eq!(
        metrics.accounted(),
        metrics.admitted,
        "terminal outcomes must equal admitted comments"
    );
    assert_eq!(metrics.records_dropped, 0);
    let recorded: u64 = terminals
        .values()
        .map(|outcomes| outcomes.len() as u64)
        .sum();
    assert_eq!(recorded, metrics.admitted);
    assert!(
        terminals.values().all(|outcomes| outcomes.len() == 1),
        "an event with two terminal outcomes: {:?}",
        terminals.iter().find(|(_, o)| o.len() != 1)
    );
    assert_eq!(metrics.invariant_violations, 0);
    assert_eq!(metrics.burst_invariant_violations, 0);
    assert_eq!(metrics.deferred.invariant_violations, 0);

    // 2. No expired reply ever dispatches: the logical wait of every
    //    dispatched deferred reply is below its time-to-live, and no reply
    //    dispatched later than window + ttl after its comment arrived.
    assert!(metrics.deferred.wait_ms_max < cfg.reply_ttl_ms);
    for (id, at) in &dispatch_at {
        let arrived = arrival_at[id];
        assert!(
            at - arrived < cfg.reply_ttl_ms,
            "{id} dispatched {} ms after arrival (ttl {})",
            at - arrived,
            cfg.reply_ttl_ms
        );
    }

    // 3. The burst stayed bounded and the deferred inbox was actually used.
    assert!(metrics.deferred.pending_high_water <= cfg.deferred.capacity);
    assert!(
        metrics.held_high_water <= cfg.burst.max_candidates * (cfg.burst.max_ready_batches + 1)
    );
    assert!(
        metrics.deferred.offered > 0,
        "the busy performer deferred replies"
    );
    assert!(metrics.dispatched() > 0, "and still answered some comments");
    assert!(
        metrics.dispatched() < metrics.admitted / 4,
        "a burst is answered selectively, not comment by comment"
    );
    metrics
}

#[test]
fn the_same_burst_replays_to_identical_outcomes() {
    let run = || {
        let trace = burst_100x();
        let mut app = burst_app(config());
        let mut log = Vec::new();
        for (index, event) in trace.iter().take(600).enumerate() {
            let now = scenario_instant_ms(&event.observed_at).expect("instant");
            let raw = serde_json::to_vec(event).expect("json");
            let _ = app.process_content_bytes(&raw, now, index as u64);
            app.service_chat_burst(now, index as u64);
            log.extend(app.take_chat_burst_records(usize::MAX));
        }
        serde_json::to_vec(&log).expect("json")
    };
    assert_eq!(run(), run(), "outcome records are replay-deterministic");
}

#[test]
fn deferred_inbox_capacity_and_actor_caps_bound_a_flood_from_one_viewer() {
    let mut app = burst_app(ChatBurstConfig {
        deferred: DeferredConfig {
            capacity: 4,
            per_actor_cap: 1,
            max_ttl_ms: 60_000,
        },
        burst: BurstConfig {
            burst_enter_count: 64,
            ..BurstConfig::default()
        },
        ..config()
    });
    send(&mut app, &chat(1, "viewer:a", "hello there"), 0);
    for id in 2..=30_u64 {
        send(
            &mut app,
            &chat(id, "viewer:spammer", "buy my stuff?"),
            10 + id,
        );
    }
    let metrics = app.chat_burst_metrics().expect("metrics");
    assert_eq!(metrics.deferred.pending, 1, "one held reply per viewer");
    assert_eq!(
        metrics.deferred.count(DeferredOutcome::RejectedActorCap),
        28
    );
    assert_eq!(metrics.invariant_violations, 0);
}

// ---------------------------------------------------------------------------
// #79 S1: audience-signal aggregation through the live burst path.
// ---------------------------------------------------------------------------

fn audience_config() -> ChatBurstConfig {
    ChatBurstConfig {
        burst: BurstConfig {
            burst_enter_count: 2,
            top_k: 8,
            max_per_actor: 2,
            ..BurstConfig::default()
        },
        audience: Some(aivtuber_reflex::AudienceConfig::default()),
        ..config()
    }
}

/// One repeated-topic cluster (laughter from five viewers) plus three
/// outliers: a threat, a scam link and a genuine question.
fn audience_trace() -> Vec<(u64, EventEnvelope)> {
    [
        ("viewer:0", "hello everyone"),
        ("viewer:1", "草"),
        ("viewer:2", "ww"),
        ("viewer:3", "i will find you and kill you"),
        ("viewer:4", "wwww"),
        (
            "viewer:5",
            "free bitcoin giveaway http://scam.example/claim",
        ),
        ("viewer:6", "草草"),
        ("viewer:7", "how did you beat that boss?"),
        ("viewer:8", "lol"),
    ]
    .iter()
    .enumerate()
    .map(|(index, (actor, text))| (index as u64 * 40, chat(index as u64 + 1, actor, text)))
    .collect()
}

struct AudienceRun {
    terminals: BTreeMap<String, Vec<ChatOutcome>>,
    signals: Vec<aivtuber_reflex::AudienceSignal>,
    metrics: ChatBurstMetrics,
}

fn run_audience(cfg: ChatBurstConfig) -> AudienceRun {
    let mut app = burst_app(cfg);
    let mut terminals: BTreeMap<String, Vec<ChatOutcome>> = BTreeMap::new();
    let mut signals = Vec::new();
    let mut drain = |app: &mut ProductionApp<CyclingRoute>| {
        for record in app.take_chat_burst_records(usize::MAX) {
            terminals
                .entry(record.event_id)
                .or_default()
                .push(record.outcome);
        }
        signals.extend(app.take_audience_signals(usize::MAX));
        let _ = app.take_deferred_reply_reports(usize::MAX);
    };
    for (at, event) in audience_trace() {
        send(&mut app, &event, at);
        app.service_chat_burst(at, at);
        drain(&mut app);
    }
    // Flush the window, let every wait expire, then shut down.
    for end in [5_000, 200_000] {
        app.tick(end);
        app.service_chat_burst(end, end);
        drain(&mut app);
    }
    app.shutdown(200_001);
    drain(&mut app);
    let metrics = app.chat_burst_metrics().expect("metrics");
    AudienceRun {
        terminals,
        signals,
        metrics,
    }
}

#[test]
fn audience_cluster_is_aggregated_while_outliers_keep_their_own_outcome() {
    let run = run_audience(audience_config());
    assert_eq!(run.signals.len(), 1, "one repeated-topic cluster");
    let signal = &run.signals[0];
    assert_eq!((signal.count, signal.unique_actors_estimate), (5, 5));
    // evt-2 is the first laughter comment: the carrier of the crowd.
    assert_eq!(signal.representative_event_ids[0], "evt-2");

    let represented: Vec<&str> = run
        .terminals
        .iter()
        .filter(|(_, outcomes)| outcomes == &&vec![ChatOutcome::RepresentedByAggregate])
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(represented, ["evt-3", "evt-5", "evt-7", "evt-9"]);
    // Outliers (threat, scam, question) are individual candidates.
    for id in ["evt-4", "evt-6", "evt-8"] {
        assert!(
            !run.terminals[id].contains(&ChatOutcome::RepresentedByAggregate),
            "{id} must not be absorbed"
        );
    }
    // Exact accounting: every comment that entered the lane ended exactly once.
    assert_eq!(run.terminals.len(), 9);
    assert!(run.terminals.values().all(|outcomes| outcomes.len() == 1));
    let metrics = run.metrics;
    assert_eq!(metrics.admitted, 9);
    assert_eq!(metrics.accounted(), metrics.admitted);
    assert_eq!(metrics.terminal[10], 4);
    assert_eq!(metrics.audience_members_represented, 4);
    assert_eq!(metrics.audience_signals, 1);
    assert_eq!(metrics.audience_guarded, 2, "threat and scam link guarded");
    assert_eq!(metrics.held, 0);
    assert_eq!(
        metrics.invariant_violations + metrics.burst_invariant_violations,
        0
    );
}

#[test]
fn audience_aggregation_replays_deterministically() {
    let encode = |run: &AudienceRun| {
        let terminals = serde_json::to_vec(&run.terminals).expect("terminals");
        let signals: Vec<String> = run.signals.iter().map(|s| format!("{s:?}")).collect();
        (terminals, signals)
    };
    let first = run_audience(audience_config());
    let second = run_audience(audience_config());
    assert_eq!(encode(&first), encode(&second));
    assert_eq!(first.metrics.terminal, second.metrics.terminal);
}

#[test]
fn audience_is_off_by_default_and_changes_nothing() {
    let off = run_audience(ChatBurstConfig {
        audience: None,
        ..audience_config()
    });
    assert!(off.signals.is_empty());
    assert_eq!(off.metrics.terminal[10], 0);
    assert_eq!(off.metrics.audience_signals, 0);
    assert!(
        off.terminals
            .values()
            .all(|outcomes| !outcomes.contains(&ChatOutcome::RepresentedByAggregate))
    );
    assert_eq!(off.metrics.accounted(), off.metrics.admitted);
}

#[test]
fn audience_config_is_validated_never_clamped() {
    let bad = ChatBurstConfig {
        audience: Some(aivtuber_reflex::AudienceConfig {
            min_cluster_size: 1,
            min_unique_actors: 1,
        }),
        ..config()
    };
    assert!(bad.validate().is_err());
}

/// #79: the same 100x burst with audience aggregation on keeps the #75/#78
/// guarantees (exact accounting, no late reply, operator stop independent).
#[test]
fn the_100x_burst_with_audience_aggregation_is_fully_accounted() {
    let metrics = falsify_burst(
        ChatBurstConfig {
            reply_ttl_ms: 4_000,
            audience: Some(aivtuber_reflex::AudienceConfig::default()),
            ..config()
        },
        Some(400),
    );
    assert_eq!(
        metrics.terminal[10], metrics.audience_members_represented,
        "represented members are accounted as one more terminal category"
    );
    // The scenario events carry an intent but no free text, so there is
    // nothing to absorb: the guard keeps text-less comments individual.
    assert_eq!(metrics.audience_signals, 0);
    assert!(metrics.audience_guarded > 0);
}
