//! App-level tests of the bounded idle initiative (issue #87 S1): it speaks
//! once when no chat has arrived, through a typed bounded request; chat,
//! speech and the operator preempt it through the existing paths; a failed
//! generation falls back to a visual gesture only and is never retried; the
//! same inputs give the same decisions.

use super::*;
use crate::initiative::{
    Decision, EVENT_ID_PREFIX, IDLE_GESTURE_PRESET, InitiativeConfig, Outcome, Preemption,
    Suppression,
};
use aivtuber_domain::{InitiativeKind, ThinkingRequest};
use aivtuber_scheduler::Status;

/// Records every request, blocks an initiative request on a gate when asked,
/// and fails initiative requests when asked. Chat requests always answer.
#[derive(Clone, Default)]
struct InitiativeThinking {
    requests: Arc<Mutex<Vec<ThinkingRequest>>>,
    gate: Option<Arc<BlockingGate>>,
    fail_initiative: Option<EngineErrorKind>,
    /// Say something different each time (the default repeats one phrase).
    distinct: bool,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

const DISTINCT_PHRASES: [&str; 8] = [
    "最近どう過ごしてる？",
    "今日は何を食べようかな",
    "窓の外の雲がきれいだよ",
    "好きな音楽を教えてほしいな",
    "ちょっと伸びをしようっと",
    "明日の天気は晴れるかなあ",
    "コーヒーが飲みたくなってきた",
    "みんなの趣味って何だろう",
];

impl ThinkingEngine for InitiativeThinking {
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let initiative = request.context.initiative.is_some();
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let distinct = self.distinct;
        Box::pin(async move {
            if initiative {
                if let Some(kind) = self.fail_initiative {
                    return Err(EngineError::new(kind, "initiative thinking failure"));
                }
                if let Some(gate) = &self.gate {
                    gate.entered.store(true, Ordering::Release);
                    let released = gate.released.lock().expect("gate");
                    let (_released, _wait) = gate
                        .wake
                        .wait_timeout_while(released, Duration::from_secs(3), |released| !*released)
                        .expect("gate wait");
                }
            }
            Ok(GeneratedReply {
                text: if initiative && distinct {
                    DISTINCT_PHRASES[call % DISTINCT_PHRASES.len()].to_owned()
                } else if initiative {
                    "最近どう過ごしてる？".to_owned()
                } else {
                    "こんにちは！".to_owned()
                },
                usage: None,
            })
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "initiative-thinking".to_owned(),
            model_alias: Some("initiative".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

struct Rig {
    app: ProductionApp<FixedGenerateRoute>,
    thinking: InitiativeThinking,
    audio: RecordingAudio,
    avatar: RecordingAvatar,
}

fn rig_with(thinking: InitiativeThinking, config: InitiativeConfig) -> Rig {
    let audio = RecordingAudio::default();
    let avatar = RecordingAvatar::default();
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            // A cached spoken reaction exists for the neutral family: an
            // initiative must never fall back to it.
            fallback_variant_group: Some("reaction.neutral".to_owned()),
        },
        thinking.clone(),
        MockTts::default(),
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
        audio.clone(),
        avatar.clone(),
    )
    .with_initiative(config);
    app.startup().expect("startup");
    Rig {
        app,
        thinking,
        audio,
        avatar,
    }
}

fn rig() -> Rig {
    rig_with(InitiativeThinking::default(), InitiativeConfig::default())
}

fn initiative_requests(thinking: &InitiativeThinking) -> Vec<ThinkingRequest> {
    thinking
        .requests
        .lock()
        .expect("requests")
        .iter()
        .filter(|request| request.context.initiative.is_some())
        .cloned()
        .collect()
}

fn gestures(avatar: &RecordingAvatar) -> usize {
    avatar
        .commands
        .lock()
        .expect("avatar log")
        .iter()
        .filter(|command| {
            matches!(command, AvatarPlaybackCommand::Gesture { preset, .. }
                if preset == IDLE_GESTURE_PRESET)
        })
        .count()
}

fn chat_at(app: &mut ProductionApp<FixedGenerateRoute>, id: u64, at_ms: u64) {
    let mut event = chat_event(id);
    event.payload.remove("intent");
    event
        .payload
        .insert("text".to_owned(), serde_json::json!("こんばんは"));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, at_ms, id).expect("chat");
}

fn initiative_items(app: &ProductionApp<FixedGenerateRoute>) -> Vec<(Status, String)> {
    app.performer()
        .scheduler()
        .items()
        .iter()
        .filter(|item| item.plan.event_id.starts_with(EVENT_ID_PREFIX))
        .map(|item| (item.status, item.plan.event_id.clone()))
        .collect()
}

fn operator(capability: Capability, action: &str) -> AuthenticatedControlCommand {
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
            correlation_id: "corr-initiative".to_owned(),
            sequence: 97,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )
    .expect("authenticated command")
}

/// Run the initiative at `at_ms`, then let a started generation finish.
fn step(rig: &mut Rig, at_ms: u64) {
    rig.app.service_initiative(at_ms, at_ms);
    if rig
        .app
        .generation_cancellation_registry()
        .is_some_and(|registry| registry.active_event_id().is_some())
    {
        wait_for_generation_completion(&mut rig.app, at_ms);
    }
}

#[test]
fn nothing_is_composed_by_default_and_the_performer_stays_silent() {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        InitiativeThinking::default(),
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    app.service_initiative(600_000, 1);
    assert!(app.initiative_status().is_none());
    assert!(app.performer().scheduler().items().is_empty());
}

#[test]
fn idle_initiative_speaks_through_a_typed_bounded_request() {
    let mut rig = rig();
    step(&mut rig, 44_000);
    assert!(initiative_requests(&rig.thinking).is_empty(), "too early");

    step(&mut rig, 45_000);
    let requests = initiative_requests(&rig.thinking);
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    request.validate().expect("the request validates");
    assert_eq!(
        request.context.initiative.as_ref().map(|c| c.kind),
        Some(InitiativeKind::IdleObservation)
    );
    assert_eq!(
        request.input.text,
        crate::initiative::input_text(InitiativeKind::IdleObservation)
    );
    assert_eq!(
        request.input.event_kind,
        Some(aivtuber_domain::EventKind::TimerTick)
    );
    assert_eq!(request.input.trust_level, TrustLevel::Trusted);
    assert!(request.curated_context.len() <= crate::initiative::SUMMARY_COUNT * 2);

    // It is one ordinary Background performer item.
    let item = rig
        .app
        .performer()
        .scheduler()
        .items()
        .iter()
        .find(|item| item.plan.event_id.starts_with(EVENT_ID_PREFIX))
        .expect("the initiative was scheduled");
    assert_eq!(item.plan.priority, aivtuber_scheduler::Priority::Background);

    // Played out, it is counted as completed and remembered for repeats.
    rig.app.tick(50_000);
    rig.app.service_initiative(50_000, 1);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.fired, status.completed, status.failed), (1, 1, 0));
    assert!(!status.pending);
    assert_eq!(
        rig.app
            .initiative_director()
            .expect("director")
            .recent_texts()
            .collect::<Vec<_>>(),
        vec!["最近どう過ごしてる？"]
    );
    // The memory writer and recent-reply panel treat it as the performer's own.
    assert!(!rig.audio.commands.lock().expect("audio").is_empty());
}

#[test]
fn the_next_request_carries_the_last_replies_and_earlier_initiatives_as_data() {
    let mut rig = rig();
    chat_at(&mut rig.app, 1, 1_000);
    wait_for_generation_completion(&mut rig.app, 1_000);
    rig.app.tick(10_000);
    step(&mut rig, 60_000);
    rig.app.tick(70_000);
    step(&mut rig, 130_000);
    let requests = initiative_requests(&rig.thinking);
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    second.validate().expect("valid");
    assert_eq!(
        second.context.initiative.as_ref().map(|c| c.kind),
        Some(InitiativeKind::AudienceQuestion)
    );
    let texts: Vec<&str> = second
        .curated_context
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert!(
        texts.iter().any(|t| t.starts_with("recent_reply: ")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.starts_with("recent_initiative: ")),
        "{texts:?}"
    );
    // The viewer's own words are not in the request.
    assert!(texts.iter().all(|t| !t.contains("こんばんは")));
}

#[test]
fn chat_arrival_cancels_a_generating_initiative_and_chat_is_not_held() {
    let gate = Arc::new(BlockingGate::default());
    let mut rig = rig_with(
        InitiativeThinking {
            gate: Some(Arc::clone(&gate)),
            ..InitiativeThinking::default()
        },
        InitiativeConfig::default(),
    );
    rig.app.service_initiative(45_000, 1);
    gate.wait_until_entered();
    assert!(rig.app.initiative_status().expect("status").pending);
    assert!(matches!(
        rig.app.reply_availability(46_000),
        ReplyAvailability::Busy(BusyCause::Generating)
    ));

    chat_at(&mut rig.app, 1, 46_000);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.preempted_chat, status.pending), (1, false));
    gate.release();
    wait_for_generation_completion(&mut rig.app, 47_000);
    // The cancelled initiative never spoke, and chat was answered.
    assert!(initiative_items(&rig.app).is_empty());
    assert!(
        rig.app
            .recent_replies()
            .items()
            .all(|reply| !reply.event_id.starts_with(EVENT_ID_PREFIX))
    );
    assert_eq!(rig.app.recent_replies().len(), 1);
    // The chat restarted the idle clock.
    rig.app.service_initiative(50_000, 2);
    assert_eq!(rig.app.initiative_status().expect("status").fired, 1);
}

#[test]
fn chat_arrival_stops_initiative_speech_that_is_already_playing() {
    let mut rig = rig();
    step(&mut rig, 45_000);
    rig.app.tick(45_100);
    let before = initiative_items(&rig.app);
    assert!(
        before
            .iter()
            .any(|(status, _)| matches!(status, Status::Queued | Status::Playing)),
        "{before:?}"
    );

    chat_at(&mut rig.app, 1, 45_200);
    assert!(
        initiative_items(&rig.app)
            .iter()
            .all(|(status, _)| !matches!(status, Status::Queued | Status::Playing)),
        "the initiative must not keep speaking over chat"
    );
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.preempted_chat, status.pending), (1, false));
}

#[test]
fn speech_arrival_preempts_like_chat_but_is_counted_as_speech() {
    let mut rig = rig();
    step(&mut rig, 45_000);
    rig.app
        .note_content_arrival(aivtuber_domain::EventKind::SpeechInput, 45_300);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.preempted_speech, status.preempted_chat), (1, 0));
    assert!(
        initiative_items(&rig.app)
            .iter()
            .all(|(status, _)| !matches!(status, Status::Queued | Status::Playing))
    );
}

#[test]
fn a_human_taking_the_floor_cancels_a_generating_initiative() {
    let gate = Arc::new(BlockingGate::default());
    let thinking = InitiativeThinking {
        gate: Some(Arc::clone(&gate)),
        ..InitiativeThinking::default()
    };
    let mut rig = rig_with(thinking, InitiativeConfig::default());
    rig.app = rig
        .app
        .with_speech_observations(
            aivtuber_runtime::SpeechObservationTracker::new(
                aivtuber_runtime::SpeechObservationConfig::default(),
                aivtuber_runtime::IngressPolicy::default(),
            )
            .expect("observation tracker"),
        )
        .with_speech_transcripts(
            aivtuber_runtime::SpeechTranscriptTracker::new(
                aivtuber_runtime::SpeechTranscriptConfig::default(),
                aivtuber_runtime::IngressPolicy::default(),
            )
            .expect("transcript tracker"),
        )
        .with_speech_floor(aivtuber_domain::FloorConfig::default())
        .expect("speech floor");
    rig.app.service_initiative(45_000, 1);
    gate.wait_until_entered();

    let bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": "0.1.0",
        "provider_id": "vad",
        "provider_version": "1",
        "language": "ja",
        "observations": [{
            "kind": "speech_activity",
            "speaker_id": "host",
            "edge": "start",
            "observed_at_ms": 45_500
        }]
    }))
    .expect("vad json");
    rig.app
        .speech_observations_mut()
        .expect("observations")
        .ingest_bytes(&bytes, 45_500)
        .expect("ingest vad");
    rig.app.service_initiative(45_600, 2);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.preempted_speech, status.pending), (1, false));
    gate.release();
    wait_for_generation_completion(&mut rig.app, 46_000);
    assert!(initiative_items(&rig.app).is_empty());
    // While the floor is held the director does not fire again; it counts why.
    assert_eq!(status.fired, 1);
    assert_eq!(
        rig.app
            .initiative_director()
            .expect("director")
            .metrics()
            .suppressed_by(Suppression::FloorHeld),
        1
    );
}

#[test]
fn a_failed_generation_plays_the_idle_gesture_only_and_is_not_retried() {
    let mut rig = rig_with(
        InitiativeThinking {
            fail_initiative: Some(EngineErrorKind::Unavailable),
            ..InitiativeThinking::default()
        },
        InitiativeConfig::default(),
    );
    step(&mut rig, 45_000);
    rig.app.tick(46_000);
    rig.app.service_initiative(46_000, 1);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.fired, status.failed, status.completed), (1, 1, 0));
    assert_eq!(gestures(&rig.avatar), 1, "one idle gesture");
    assert!(
        rig.audio.commands.lock().expect("audio").is_empty(),
        "a failed initiative never plays a spoken fallback"
    );
    assert!(initiative_items(&rig.app).is_empty());

    // No retry loop: the budget stays charged, so even at the next tick and
    // every tick of the next minute nothing more is requested.
    for at_ms in (47_000..105_000).step_by(1_000) {
        step(&mut rig, at_ms);
    }
    assert_eq!(initiative_requests(&rig.thinking).len(), 1);
    assert_eq!(
        rig.app
            .initiative_director()
            .expect("director")
            .metrics()
            .suppressed_by(Suppression::BudgetGap),
        1
    );
}

#[test]
fn without_a_generative_runtime_the_fallback_is_the_gesture_alone() {
    let avatar = RecordingAvatar::default();
    let mut app = app(
        FixedAssetRoute("reaction.agree.01"),
        Box::new(RecordingAudio::default()),
        Box::new(avatar.clone()),
        Box::new(NoopStreamOutput),
    )
    .with_initiative(InitiativeConfig::default());
    app.startup().expect("startup");
    app.service_initiative(45_000, 1);
    let status = app.initiative_status().expect("status");
    assert_eq!((status.fired, status.failed), (1, 1));
    assert_eq!(gestures(&avatar), 1);
    assert!(app.performer().scheduler().items().is_empty());
}

#[test]
fn mute_suppresses_the_initiative_and_an_operator_stop_holds_it() {
    let mut rig = rig();
    rig.app
        .handle_control_reporting(
            &operator(Capability::PerformerMute, "performer.mute"),
            1_000,
        )
        .expect("mute");
    step(&mut rig, 100_000);
    assert!(initiative_requests(&rig.thinking).is_empty());
    assert_eq!(
        rig.app
            .initiative_director()
            .expect("director")
            .metrics()
            .suppressed_by(Suppression::Muted),
        1
    );

    // A stop (on a fresh rig) holds the initiative for a while even though the
    // idle time has passed again.
    let mut rig = self::rig();
    rig.app
        .handle_control_reporting(
            &operator(Capability::PerformerStop, "performer.stop"),
            10_000,
        )
        .expect("stop");
    step(&mut rig, 100_000);
    assert!(initiative_requests(&rig.thinking).is_empty());
    assert_eq!(
        rig.app
            .initiative_director()
            .expect("director")
            .metrics()
            .suppressed_by(Suppression::OperatorStopped),
        1
    );
    step(&mut rig, 10_000 + crate::initiative::STOP_HOLD_MS);
    assert_eq!(initiative_requests(&rig.thinking).len(), 1);
}

#[test]
fn an_operator_stop_cancels_a_pending_initiative() {
    let gate = Arc::new(BlockingGate::default());
    let mut rig = rig_with(
        InitiativeThinking {
            gate: Some(Arc::clone(&gate)),
            ..InitiativeThinking::default()
        },
        InitiativeConfig::default(),
    );
    rig.app.service_initiative(45_000, 1);
    gate.wait_until_entered();
    rig.app
        .handle_control_reporting(
            &operator(Capability::PerformerStop, "performer.stop"),
            46_000,
        )
        .expect("stop");
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.preempted_stop, status.pending), (1, false));
    gate.release();
    wait_for_generation_completion(&mut rig.app, 47_000);
    assert!(initiative_items(&rig.app).is_empty());
}

#[test]
fn the_toggle_needs_the_capability_and_switches_the_initiative() {
    let mut rig = rig();
    // Without the capability nothing changes.
    let weak = operator(Capability::PerformerStop, "performer.stop");
    assert!(
        rig.app
            .configure_initiative(weak.authority(), "initiative.disable", 1_000)
            .is_err()
    );
    assert!(rig.app.initiative_status().expect("status").enabled);

    let control = operator(Capability::InitiativeControl, "initiative.disable");
    assert_eq!(
        rig.app
            .configure_initiative(control.authority(), "initiative.disable", 1_000),
        Ok("initiative off".to_owned())
    );
    step(&mut rig, 200_000);
    assert!(initiative_requests(&rig.thinking).is_empty());
    assert_eq!(
        rig.app
            .initiative_status()
            .expect("status")
            .suppressed_total,
        0
    );

    let control = operator(Capability::InitiativeControl, "initiative.enable");
    assert_eq!(
        rig.app
            .configure_initiative(control.authority(), "initiative.enable", 201_000),
        Ok("initiative on".to_owned())
    );
    step(&mut rig, 202_000);
    assert_eq!(initiative_requests(&rig.thinking).len(), 1);
    // An unknown action is refused.
    assert!(
        rig.app
            .configure_initiative(control.authority(), "initiative.say", 1)
            .is_err()
    );
}

#[test]
fn five_quiet_minutes_give_at_least_three_initiatives_and_none_overlaps_a_reply() {
    let mut rig = rig_with(
        InitiativeThinking {
            distinct: true,
            ..InitiativeThinking::default()
        },
        InitiativeConfig::default(),
    );
    let mut max_live_audio = 0;
    for second in 0..300_u64 {
        let at_ms = second * 1_000;
        step(&mut rig, at_ms);
        rig.app.tick(at_ms);
        let live = rig
            .app
            .performer()
            .scheduler()
            .items()
            .iter()
            .filter(|item| {
                matches!(item.status, Status::Queued | Status::Playing)
                    && item
                        .plan
                        .channels
                        .contains(&aivtuber_scheduler::BlendChannel::Audio)
            })
            .count();
        max_live_audio = max_live_audio.max(live);
    }
    let status = rig.app.initiative_status().expect("status");
    assert!(status.fired >= 3, "{status:?}");
    assert_eq!(status.failed, 0);
    assert_eq!(max_live_audio, 1, "initiatives never overlap one another");
    assert!(status.fired <= 6, "the budget holds: {status:?}");
}

#[test]
fn an_initiative_never_fires_over_a_chat_reply_in_progress() {
    let mut rig = rig();
    // A chat reply just started playing; the initiative idle clock is older
    // than the threshold only because chat restarted it just now.
    chat_at(&mut rig.app, 1, 100_000);
    wait_for_generation_completion(&mut rig.app, 100_000);
    for at_ms in (100_000..140_000).step_by(500) {
        rig.app.tick(at_ms);
        rig.app.service_initiative(at_ms, at_ms);
    }
    assert!(initiative_requests(&rig.thinking).is_empty());
    // 45 s after the chat, once the reply is long over, it may speak.
    step(&mut rig, 145_000);
    assert_eq!(initiative_requests(&rig.thinking).len(), 1);
}

/// Replay determinism at the host level: the same timed script gives the same
/// decision log, counters and request sequence on two fresh apps.
#[test]
fn the_same_script_replays_to_the_same_decisions_and_requests() {
    fn run() -> (
        Vec<(u64, String)>,
        crate::initiative::InitiativeMetrics,
        Vec<InitiativeKind>,
    ) {
        let mut rig = rig();
        for second in 0..400_u64 {
            let at_ms = second * 1_000;
            if second == 100 {
                chat_at(&mut rig.app, 1, at_ms);
                wait_for_generation_completion(&mut rig.app, at_ms);
            }
            step(&mut rig, at_ms);
            rig.app.tick(at_ms);
        }
        let director = rig.app.initiative_director().expect("director");
        let log = director
            .log()
            .map(|record| (record.at_ms, format!("{:?}", record.outcome)))
            .collect();
        let kinds = initiative_requests(&rig.thinking)
            .iter()
            .filter_map(|request| request.context.initiative.as_ref().map(|c| c.kind))
            .collect();
        (log, director.metrics(), kinds)
    }
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    assert!(a.1.fired >= 4, "{:?}", a.1);
    assert!(
        a.0.iter().any(|(_, outcome)| outcome.starts_with("Fired")),
        "{:?}",
        a.0
    );
    // Silence the unused imports a replay-only run does not touch.
    let _ = (Decision::Wait, Outcome::Completed, Preemption::Chat);
}

#[test]
fn a_repeated_initiative_phrase_is_refused_and_falls_back_to_the_gesture() {
    let mut rig = rig();
    step(&mut rig, 45_000);
    rig.app.tick(50_000);
    rig.app.service_initiative(50_000, 1);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.fired, status.completed, status.failed), (1, 1, 0));
    assert_eq!(gestures(&rig.avatar), 0);
    let repeats = rig.app.repeat_status();
    assert_eq!((repeats.phrases_spoken, repeats.exact_repeats), (1, 0));
    assert_eq!(repeats.by_origin[1].1, 1, "counted as an initiative phrase");

    // The stub says the same thing again. It is refused before commit: the
    // idle gesture plays, nothing is scheduled, and nothing is regenerated.
    step(&mut rig, 115_000);
    rig.app.tick(120_000);
    rig.app.service_initiative(120_000, 2);
    let status = rig.app.initiative_status().expect("status");
    assert_eq!((status.fired, status.completed, status.failed), (2, 1, 1));
    assert!(!status.pending);
    assert_eq!(gestures(&rig.avatar), 1);
    assert_eq!(initiative_requests(&rig.thinking).len(), 2);
    assert!(
        initiative_items(&rig.app)
            .iter()
            .all(|(_, id)| id == "initiative-000001"),
        "the repeat was never scheduled"
    );
    let repeats = rig.app.repeat_status();
    assert_eq!(repeats.suppressed_repeat, 1);
    // Nothing repeated was spoken.
    assert_eq!((repeats.phrases_spoken, repeats.exact_repeats), (1, 0));
}

#[test]
fn chat_replies_are_measured_but_never_refused_for_repeating() {
    let mut rig = rig();
    for id in 1..=2_u64 {
        let at_ms = id * 20_000;
        chat_at(&mut rig.app, id, at_ms);
        wait_for_generation_completion(&mut rig.app, at_ms);
        rig.app.tick(at_ms + 10_000);
    }
    let repeats = rig.app.repeat_status();
    assert_eq!(repeats.suppressed_repeat, 0);
    assert_eq!(repeats.by_origin[0], ("chat_reply".to_owned(), 2, 1, 0));
    assert_eq!(rig.app.recent_replies().len(), 2);
}
