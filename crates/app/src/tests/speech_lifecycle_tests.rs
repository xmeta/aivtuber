//! App-level tests of the speech lifecycle (issue #222 S2, ADR-0004 D5): the
//! composition root reports output observations and the scheduler owns the
//! truth. No provider type appears in any assertion.

use super::*;
use aivtuber_domain::{
    SpeechCancelReason, SpeechLifecycleEvent, SpeechPhase, SpeechReached, SpeechTimeBasis,
};

fn command(capability: Capability, action: &str) -> AuthenticatedControlCommand {
    let secret = [0x46_u8; 32];
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
            correlation_id: "corr-lifecycle".to_owned(),
            sequence: 98,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )
    .expect("authenticated command")
}

fn speak(
    audio: Box<dyn AudioOutput>,
) -> (
    ProductionApp<FixedAssetRoute>,
    aivtuber_runtime::CachedPlaybackOutcome,
) {
    let mut app = app(
        FixedAssetRoute("reaction.agree.01"),
        audio,
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    let raw = serde_json::to_vec(&chat_event(2)).expect("event json");
    let playback = app
        .process_content_bytes(&raw, 0, 11)
        .expect("production path")
        .playback
        .expect("cached playback");
    (app, playback)
}

fn phases(events: &[SpeechLifecycleEvent]) -> Vec<SpeechPhase> {
    events.iter().map(|event| event.phase).collect()
}

fn drained(app: &mut ProductionApp<FixedAssetRoute>) -> Vec<SpeechLifecycleEvent> {
    let events = app.drain_speech_lifecycle(usize::MAX);
    for event in &events {
        event.validate().expect("valid lifecycle event");
    }
    events
}

#[test]
fn a_spoken_reply_is_finished_with_the_output_accepted_basis_and_a_digest() {
    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    app.tick(playback.plan.start_at_ms + 150);
    app.tick(playback.plan.end_at_ms() + 10);
    let events = drained(&mut app);
    let kinds = phases(&events);
    assert_eq!(kinds.first(), Some(&SpeechPhase::Prepared));
    assert_eq!(kinds[1], SpeechPhase::Committed);
    assert_eq!(kinds[2], SpeechPhase::Started);
    assert_eq!(kinds.last(), Some(&SpeechPhase::Finished));
    assert_eq!(
        events.last().unwrap().basis,
        SpeechTimeBasis::OutputAccepted,
        "the adapter accepted the command; nothing claims it was audible"
    );
    assert_eq!(events[0].speech.asset_id, "reaction.agree.01");
    assert!(events[0].speech.speech_text_digest.is_some());
    assert_eq!(events[0].speech.phrase, 0);
    assert!(events[0].speech.final_phrase);
    let snapshot = app.retention_snapshot();
    assert_eq!(snapshot.speech_lifecycle.finished, 1);
    assert_eq!(snapshot.speech_lifecycle.rejected, 0);
}

#[test]
fn a_failing_audio_output_is_cancelled_never_finished() {
    let (mut app, playback) = speak(Box::new(FailingAudio {
        attempts: Arc::new(AtomicUsize::new(0)),
    }));
    app.tick(playback.plan.start_at_ms + 150);
    app.tick(playback.plan.end_at_ms() + 10);
    let events = drained(&mut app);
    assert!(!phases(&events).contains(&SpeechPhase::Finished));
    assert!(matches!(
        events.last().unwrap().phase,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::OutputFailure,
            reached: SpeechReached::Started,
            ..
        }
    ));
}

#[test]
fn a_command_dispatched_too_late_is_cancelled_as_stale_not_finished() {
    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    // The first tick comes long after the lateness window and after the
    // item's logical end: the timeline alone would call it finished.
    app.tick(playback.plan.end_at_ms() + 1_000_000);
    let events = drained(&mut app);
    assert!(!phases(&events).contains(&SpeechPhase::Finished));
    assert!(matches!(
        events.last().unwrap().phase,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::StaleOrLate,
            ..
        }
    ));
}

#[test]
fn operator_stop_and_shutdown_record_their_reasons_with_the_spoken_duration() {
    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    app.tick(playback.plan.start_at_ms);
    app.handle_control(
        &command(Capability::PerformerStop, "performer.stop"),
        playback.plan.start_at_ms + 100,
    )
    .expect("stop");
    let events = drained(&mut app);
    assert!(matches!(
        events.last().unwrap().phase,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::OperatorStop,
            reached: SpeechReached::Started,
            spoken_ms: Some(100),
        }
    ));

    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    app.tick(playback.plan.start_at_ms);
    app.shutdown(playback.plan.start_at_ms + 50);
    let events = drained(&mut app);
    assert!(matches!(
        events.last().unwrap().phase,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::Shutdown,
            ..
        }
    ));
}

#[test]
fn operator_mute_cancels_the_speech_lifecycle_and_no_finished_follows() {
    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    app.tick(playback.plan.start_at_ms);
    app.handle_control(
        &command(Capability::PerformerMute, "performer.mute"),
        playback.plan.start_at_ms + 20,
    )
    .expect("mute");
    app.tick(playback.plan.end_at_ms() + 10);
    let events = drained(&mut app);
    assert!(!phases(&events).contains(&SpeechPhase::Finished));
    assert!(matches!(
        events.last().unwrap().phase,
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::Muted,
            reached: SpeechReached::Started,
            ..
        }
    ));
}

#[test]
fn the_snapshot_lets_a_consumer_resynchronize_without_provider_events() {
    let (mut app, playback) = speak(Box::new(RecordingAudio::default()));
    app.tick(playback.plan.start_at_ms);
    let snapshot = app.speech_lifecycle_snapshot();
    assert_eq!(snapshot.live.len(), 1);
    assert_eq!(snapshot.live[0].speech.asset_id, "reaction.agree.01");
    assert!(snapshot.next_seq > 1);
}
