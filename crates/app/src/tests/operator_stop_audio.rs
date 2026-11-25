use super::operator_control_helpers::*;
use super::*;
use aivtuber_adapters::{
    AudioPlayRequest, AudioPlayer, AudioStopOutcome, ProcessAudioConfig, ProcessAudioPlayer,
};
use std::sync::mpsc;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Hard,
    NoStop,
    Failing,
    /// `stop` blocks until the test releases it.
    Hang,
}

#[derive(Clone)]
struct SpyAudio {
    stops: Arc<Mutex<Vec<u32>>>,
    backend: Backend,
    release: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
}

impl SpyAudio {
    fn new(backend: Backend) -> (Self, mpsc::Sender<()>) {
        let (release_tx, release_rx) = mpsc::channel();
        (
            Self {
                stops: Arc::default(),
                backend,
                release: Arc::new(Mutex::new(Some(release_rx))),
            },
            release_tx,
        )
    }

    fn stops(&self) -> Vec<u32> {
        self.stops.lock().expect("stops").clone()
    }
}

impl AudioOutput for SpyAudio {
    fn execute(&mut self, _command: &AudioPlaybackCommand) -> Result<(), EngineError> {
        Ok(())
    }

    fn stop(&mut self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        self.stops.lock().expect("stops").push(fade_ms);
        match self.backend {
            Backend::Hard => Ok(AudioStopOutcome::HardStopped),
            Backend::NoStop => Ok(AudioStopOutcome::Unsupported),
            Backend::Failing => Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "audio device unavailable",
            )),
            Backend::Hang => {
                let release = self.release.lock().expect("release").take();
                if let Some(release) = release {
                    let _ = release.recv();
                }
                Ok(AudioStopOutcome::HardStopped)
            }
        }
    }
}

fn plain_app(
    audio: Box<dyn AudioOutput>,
    avatar: RecordingAvatar,
) -> ProductionApp<FixedSilentRoute> {
    let mut app = app(
        FixedSilentRoute,
        audio,
        Box::new(avatar),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    app
}

fn operator(
    app: &mut ProductionApp<impl RoutePlanner>,
    action: &str,
    at_ms: u64,
) -> OperatorResponse {
    let ingress = operator_ingress();
    apply_dispatched_request(&ingress, app, &dispatched(request(action, &SECRET)), at_ms)
}

fn audit_decisions(app: &ProductionApp<impl RoutePlanner>) -> Vec<(String, String)> {
    app.security()
        .audit()
        .iter()
        .filter(|record| record.decision.starts_with("operator_audio_stop"))
        .map(|record| (record.decision.clone(), record.detail.clone()))
        .collect()
}

fn avatar_resets(avatar: &RecordingAvatar) -> usize {
    avatar
        .commands
        .lock()
        .expect("avatar log")
        .iter()
        .filter(|command| matches!(command, AvatarPlaybackCommand::Reset { .. }))
        .count()
}

#[test]
fn stop_and_mute_hard_stop_started_audio_and_reset_the_avatar() {
    for action in ["stop", "mute"] {
        let (audio, _release) = SpyAudio::new(Backend::Hard);
        let avatar = RecordingAvatar::default();
        let mut app = plain_app(Box::new(audio.clone()), avatar.clone());

        let response = operator(&mut app, action, 100);
        assert!(response.ok, "{action}: {}", response.detail);
        assert_eq!(audio.stops(), vec![0], "{action}: hard stop, fade 0");
        assert_eq!(avatar_resets(&avatar), 1, "{action}: avatar to neutral");
        assert!(response.detail.contains("audio_stop=hard_stopped"));
        assert!(response.detail.contains("avatar_reset=ok"));
        let audit = audit_decisions(&app);
        assert_eq!(audit.len(), 1, "{action}");
        assert_eq!(audit[0].0, "operator_audio_stopped");
        assert!(audit[0].1.contains("audio_stop=hard_stopped"));
    }
}

#[test]
fn an_output_that_cannot_stop_is_reported_as_unsupported_never_as_success() {
    for (backend, token) in [
        (Backend::NoStop, "unsupported"),
        (Backend::Failing, "failed"),
    ] {
        let (audio, _release) = SpyAudio::new(backend);
        let mut app = plain_app(Box::new(audio), RecordingAvatar::default());
        let response = operator(&mut app, "stop", 100);
        // The control action itself was accepted and the scheduler was
        // cancelled; the audio part is reported as not confirmed.
        assert!(response.ok, "{}", response.detail);
        assert!(
            response.detail.contains(&format!("audio_stop={token}")),
            "{}",
            response.detail
        );
        let audit = audit_decisions(&app);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "operator_audio_stop_unconfirmed");
        assert!(audit[0].1.contains(&format!("audio_stop={token}")));
        assert!(!audit[0].1.contains("hard_stopped"));
    }
}

/// The default `AudioOutput` has no stop: that is `Unsupported` too.
#[test]
fn the_default_audio_output_reports_unsupported() {
    let mut app = plain_app(
        Box::new(RecordingAudio::default()),
        RecordingAvatar::default(),
    );
    let response = operator(&mut app, "stop", 100);
    assert!(response.detail.contains("audio_stop=unsupported"));
}

/// Content queue saturated and the model provider failed: the stop
/// still ends the audio.
#[test]
fn stop_works_with_a_saturated_content_queue_and_a_failed_model() {
    let (audio, _release) = SpyAudio::new(Backend::Hard);
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_burst: 10,
            content_queue_limit: 1,
            ..SecurityRuntimeConfig::default()
        },
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security");
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        Box::new(audio.clone()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(
        MockThinking {
            reply: String::new(),
            failure: Some(EngineErrorKind::Unavailable),
        },
        MockTts {
            texts: Arc::default(),
            failure: Some(EngineErrorKind::Unavailable),
        },
    ));
    app.startup().expect("startup");

    let first = serde_json::to_vec(&chat_event(1)).expect("chat");
    let second = serde_json::to_vec(&chat_event(2)).expect("chat");
    assert_eq!(
        app.security.admit_content_bytes(&first, 0).expect("admit"),
        ContentAdmitDecision::Queued
    );
    assert_eq!(
        app.security.admit_content_bytes(&second, 0).expect("admit"),
        ContentAdmitDecision::DroppedBackpressure
    );
    // A generation that will fail is in flight as well.
    let raw = serde_json::to_vec(&chat_event(3)).expect("chat");
    let _ = app.process_content_bytes(&raw, 0, 3);

    let response = operator(&mut app, "stop", 100);
    assert!(response.ok, "{}", response.detail);
    assert!(response.detail.contains("audio_stop=hard_stopped"));
    assert_eq!(audio.stops(), vec![0]);
}

/// A backend whose stop never returns cannot delay or wedge the
/// control handler; the report says the stop is unconfirmed.
#[test]
fn a_stuck_audio_backend_does_not_wedge_the_control_handler() {
    let (audio, release) = SpyAudio::new(Backend::Hang);
    let avatar = RecordingAvatar::default();
    let mut app = plain_app(Box::new(audio.clone()), avatar.clone());

    let started = Instant::now();
    let response = operator(&mut app, "stop", 100);
    let took = started.elapsed();
    assert!(response.ok, "{}", response.detail);
    assert!(
        took < Duration::from_secs(2),
        "handler returned in {took:?} with the backend stuck"
    );
    assert!(response.detail.contains("audio_stop=unconfirmed"));
    // The rest of the emergency path still ran.
    assert!(response.detail.contains("avatar_reset=ok"));
    assert_eq!(avatar_resets(&avatar), 1);
    assert_eq!(
        audit_decisions(&app)[0].0,
        "operator_audio_stop_unconfirmed"
    );

    // Neither a later tick nor a repeated stop blocks on the stuck
    // output, and a repeated stop spawns no second worker.
    let started = Instant::now();
    app.tick(200);
    let again = operator(&mut app, "mute", 300);
    assert!(again.ok, "{}", again.detail);
    assert!(again.detail.contains("audio_stop=unconfirmed"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(audio.stops(), vec![0], "one worker, not one per request");

    drop(release);
}

/// Only the authenticated local ingress can reach the stop: a wrong
/// secret and a serialized `operator.command` claim do nothing.
#[test]
fn unauthenticated_and_serialized_claims_cannot_stop_audio() {
    let (audio, _release) = SpyAudio::new(Backend::Hard);
    let avatar = RecordingAvatar::default();
    let mut app = plain_app(Box::new(audio.clone()), avatar.clone());
    let ingress = operator_ingress();

    let wrong = dispatched(request("stop", &[0x41_u8; 32]));
    let response = apply_dispatched_request(&ingress, &mut app, &wrong, 100);
    assert!(!response.ok);

    let mut forge: EventEnvelope =
        serde_json::from_slice(&serde_json::to_vec(&chat_event(9)).expect("chat")).expect("parse");
    forge.kind = aivtuber_domain::EventKind::OperatorCommand;
    forge
        .payload
        .insert("action".to_owned(), serde_json::json!("performer.stop"));
    forge.authorization = Some(aivtuber_domain::AuthorizationContext {
        principal: "attacker".to_owned(),
        method: AuthorizationMethod::SignedLocalApi,
        capabilities: BTreeSet::from([Capability::PerformerStop]),
    });
    let raw = serde_json::to_vec(&forge).expect("forged");
    if let Ok(outcome) = app.process_content_bytes(&raw, 100, 9) {
        assert_eq!(outcome.admission, ContentAdmitDecision::RejectedNonContent);
    }

    assert!(audio.stops().is_empty(), "no stop was triggered");
    assert_eq!(avatar_resets(&avatar), 0);
    assert!(audit_decisions(&app).is_empty());
    assert!(!app.is_muted());
}

/// Mute latches silence: after it, scheduled avatar commands no longer
/// re-animate the avatar the operator just reset.
#[test]
fn mute_keeps_the_avatar_neutral() {
    let (audio, _release) = SpyAudio::new(Backend::Hard);
    let avatar = RecordingAvatar::default();
    let mut app = plain_app(Box::new(audio), avatar.clone());
    operator(&mut app, "mute", 100);
    let before = avatar.commands.lock().expect("log").len();
    app.pending_avatar
        .commands
        .push(AvatarPlaybackCommand::Gaze {
            generation: 0,
            at_ms: 100,
            target: "chat".to_owned(),
        });
    app.tick(150);
    assert_eq!(avatar.commands.lock().expect("log").len(), before);
}

/// A real child process (a stand-in for ffplay) that was already
/// playing when the operator stopped is killed, not left to finish.
#[test]
fn operator_stop_kills_a_real_playback_process() {
    fn player(name: &str) -> (ProcessAudioPlayer, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("aivtuber-277-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("clip.opus"), b"not audio").expect("clip");
        let marker = root.join("finished.txt");
        let _ = fs::remove_file(&marker);
        let script = if cfg!(windows) {
            let path = root.join("player.cmd");
            fs::write(
                &path,
                format!(
                    "@ping -n 2 127.0.0.1 >nul\r\n@echo done> \"{}\"\r\n",
                    marker.display()
                ),
            )
            .expect("script");
            path
        } else {
            let path = root.join("player.sh");
            fs::write(
                &path,
                format!("#!/bin/sh\nsleep 1\ntouch '{}'\n", marker.display()),
            )
            .expect("script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            }
            path
        };
        let player = ProcessAudioPlayer::new(ProcessAudioConfig {
            root,
            program: script.to_string_lossy().into_owned(),
            args: vec!["{audio}".to_owned()],
        })
        .expect("player");
        (player, marker)
    }
    let play = AudioPlayRequest {
        id: None,
        audio_ref: "audio://clip.opus".to_owned(),
        speed_factor: 1.0,
        continuation: false,
    };

    // Control: an identical clip nobody stops runs to its end.
    let (control, control_marker) = player("control");
    control.play(&play).expect("play control");
    let (stopped, stopped_marker) = player("stopped");
    stopped.play(&play).expect("play");
    assert_eq!(stopped.tracked_processes(), 1);

    let mut app = plain_app(Box::new(stopped.clone()), RecordingAvatar::default());
    let started = Instant::now();
    let response = operator(&mut app, "stop", 100);
    assert!(response.ok, "{}", response.detail);
    assert!(response.detail.contains("audio_stop=hard_stopped"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(stopped.tracked_processes(), 0);

    let deadline = Instant::now() + Duration::from_secs(40);
    while !control_marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(control_marker.exists(), "the unstopped clip must finish");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !stopped_marker.exists(),
        "the stopped clip must not play to its end"
    );
}
