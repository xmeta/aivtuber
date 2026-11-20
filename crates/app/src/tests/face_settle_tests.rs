//! Face settle after a reply (issue #447): the app-level behavior on logical
//! time with recording outputs. The step arithmetic is covered in
//! `face_settle.rs`.

use super::*;
use aivtuber_scheduler::{BlendChannel, PlannedPerformance, Priority};

const BASELINE: &str = "speaking.neutral";

fn control(action: &str, capability: Capability) -> AuthenticatedControlCommand {
    LocalControlIngress::new(
        "local-test",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([capability]),
        ControlSecret::new([0x45_u8; 32]),
    )
    .expect("control ingress")
    .authenticate(
        OperatorCommandInput {
            event_id: format!("evt-{action}"),
            correlation_id: format!("corr-{action}"),
            sequence: 99,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        },
        &[0x45_u8; 32],
    )
    .expect("authenticated control")
}

fn rig() -> (ProductionApp<FixedSilentRoute>, RecordingAvatar) {
    let avatar = RecordingAvatar::default();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(avatar.clone()),
        Box::new(NoopStreamOutput),
    )
    .with_face_settle(BASELINE, 0.4);
    app.startup().expect("startup");
    (app, avatar)
}

/// A reply on the audio and face channels with its face command.
fn reply(
    app: &mut ProductionApp<FixedSilentRoute>,
    start_ms: u64,
    duration_ms: u64,
    preset: &str,
    intensity: f64,
) -> PlannedPerformance {
    let plan = PlannedPerformance {
        event_id: format!("evt-reply-{start_ms}"),
        asset_id: format!("asset.reply-{start_ms}"),
        priority: Priority::Conversation,
        interruptible: true,
        interrupt_points_ms: vec![duration_ms],
        start_at_ms: start_ms,
        duration_ms,
        generation: 0,
        exclusive: false,
        channels: BTreeSet::from([BlendChannel::Audio, BlendChannel::Face]),
    };
    let planned = app
        .performer
        .scheduler_mut()
        .schedule(plan)
        .expect("schedule reply");
    app.pending_avatar
        .commands
        .push(AvatarPlaybackCommand::Expression {
            generation: planned.generation,
            at_ms: planned.start_at_ms,
            preset: preset.to_owned(),
            intensity,
        });
    planned
}

fn run(app: &mut ProductionApp<FixedSilentRoute>, from_ms: u64, to_ms: u64) {
    for now in (from_ms..=to_ms).step_by(100) {
        app.tick(now);
    }
}

/// `(kind, preset, intensity, at_ms)` of every command the avatar received.
fn log(avatar: &RecordingAvatar) -> Vec<(String, String, f64, u64)> {
    avatar
        .commands
        .lock()
        .expect("avatar log")
        .iter()
        .map(|command| match command {
            AvatarPlaybackCommand::Expression {
                preset,
                intensity,
                at_ms,
                ..
            } => ("expression".to_owned(), preset.clone(), *intensity, *at_ms),
            AvatarPlaybackCommand::Reset { at_ms, .. } => {
                ("reset".to_owned(), String::new(), 0.0, *at_ms)
            }
            other => ("other".to_owned(), String::new(), 0.0, other.at_ms()),
        })
        .collect()
}

/// Expressions after the reply's own first one.
fn settle_steps(avatar: &RecordingAvatar) -> Vec<(String, f64, u64)> {
    log(avatar)
        .into_iter()
        .filter(|(kind, _, _, _)| kind == "expression")
        .skip(1)
        .map(|(_, preset, intensity, at_ms)| (preset, intensity, at_ms))
        .collect()
}

#[test]
fn the_face_steps_down_to_half_then_baseline_after_the_reply() {
    let (mut app, avatar) = rig();
    reply(&mut app, 0, 2_000, "smile_bright", 0.6);
    run(&mut app, 0, 8_000);
    assert_eq!(
        settle_steps(&avatar),
        vec![
            ("smile_bright".to_owned(), 0.3, 3_500),
            (BASELINE.to_owned(), 0.4, 6_000),
        ]
    );
    // Settled for good: nothing more is ever sent.
    run(&mut app, 8_100, 30_000);
    assert_eq!(settle_steps(&avatar).len(), 2);
}

#[test]
fn the_settle_leaves_the_scheduler_and_its_budgets_alone() {
    let (mut app, _avatar) = rig();
    reply(&mut app, 0, 2_000, "smile_bright", 0.6);
    run(&mut app, 0, 2_100);
    let snapshot = |app: &ProductionApp<FixedSilentRoute>| {
        let scheduler = app.performer.scheduler();
        (
            scheduler.last_terminal_at(),
            scheduler.cooldowns().clone(),
            scheduler.current_generation(),
            scheduler.history().count(),
        )
    };
    let before = snapshot(&app);
    run(&mut app, 2_200, 8_000);
    let after = snapshot(&app);
    assert_eq!(before.0, after.0, "reaction spacing anchor untouched");
    assert_eq!(before.2, after.2, "no generation consumed");
    assert_eq!(before.3, after.3, "no scheduler item created");
    assert_eq!(app.scheduler_item_count(), 0);
}

#[test]
fn a_new_reply_wins_immediately_and_is_not_delayed() {
    let (mut app, avatar) = rig();
    reply(&mut app, 0, 2_000, "smile_bright", 0.6);
    run(&mut app, 0, 4_000);
    assert_eq!(settle_steps(&avatar).len(), 1, "half step only so far");
    // A reply starts between the half step and the baseline step.
    let next = reply(&mut app, 4_000, 20_000, "surprise_bright", 0.5);
    assert_eq!(next.start_at_ms, 4_000, "the settle never delays speech");
    run(&mut app, 4_100, 30_000);
    let steps = settle_steps(&avatar);
    // The old reply's baseline step (6_000) never fires; the new reply
    // settles from its own end (24_000).
    assert!(
        steps
            .iter()
            .filter(|(preset, _, _)| preset == BASELINE)
            .all(|(_, _, at_ms)| *at_ms >= 24_000 + 1_500),
        "{steps:?}"
    );
}

#[test]
fn stop_and_mute_cancel_a_pending_settle() {
    for (action, capability) in [
        ("performer.stop", Capability::PerformerStop),
        ("performer.mute", Capability::PerformerMute),
    ] {
        let (mut app, avatar) = rig();
        reply(&mut app, 0, 2_000, "smile_bright", 0.6);
        run(&mut app, 0, 2_500);
        app.handle_control(&control(action, capability), 2_600)
            .expect(action);
        run(&mut app, 2_700, 12_000);
        let commands = log(&avatar);
        let reset_at = commands
            .iter()
            .position(|command| command.0 == "reset")
            .expect("reset");
        assert!(
            commands[reset_at + 1..].is_empty(),
            "{action}: nothing after the reset: {commands:?}"
        );
    }
}

#[test]
fn no_settle_when_the_face_is_already_baseline() {
    let (mut app, avatar) = rig();
    reply(&mut app, 0, 2_000, BASELINE, 0.4);
    run(&mut app, 0, 12_000);
    assert!(settle_steps(&avatar).is_empty());
}

#[test]
fn without_the_feature_the_face_is_held() {
    let avatar = RecordingAvatar::default();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(avatar.clone()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    reply(&mut app, 0, 2_000, "smile_bright", 0.6);
    run(&mut app, 0, 12_000);
    assert!(settle_steps(&avatar).is_empty());
}

#[test]
fn an_output_without_intensity_gets_only_the_baseline() {
    #[derive(Clone, Default)]
    struct Hotkeys(RecordingAvatar);
    impl AvatarOutput for Hotkeys {
        fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
            self.0.execute(command)
        }
        fn graded_expression(&self) -> bool {
            false
        }
    }
    let output = Hotkeys::default();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(output.clone()),
        Box::new(NoopStreamOutput),
    )
    .with_face_settle(BASELINE, 0.4);
    app.startup().expect("startup");
    reply(&mut app, 0, 2_000, "smile_bright", 0.6);
    run(&mut app, 0, 8_000);
    assert_eq!(
        settle_steps(&output.0),
        vec![(BASELINE.to_owned(), 0.4, 6_000)]
    );
}

#[test]
fn replay_is_deterministic_whatever_the_tick_granularity() {
    let play = |step: usize| {
        let (mut app, avatar) = rig();
        reply(&mut app, 0, 2_000, "smile_bright", 0.6);
        for now in (0..=9_000).step_by(step) {
            app.tick(now);
        }
        log(&avatar)
    };
    assert_eq!(play(100), play(100));
    assert_eq!(play(100), play(250));
}
