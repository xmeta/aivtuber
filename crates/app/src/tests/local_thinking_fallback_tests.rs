//! `local` profile: a free-text event whose Thinking call fails produces a
//! visible, deterministic, counted non-verbal reaction (issue #288).
//!
//! No model and no network beyond loopback: Thinking is a scripted fake
//! server on 127.0.0.1, TTS is a mock that must never be called, and the
//! avatar and audio sinks are recording stubs.

use super::*;
use aivtuber_generative::PhraseConfig;
use std::io::{Read, Write};

/// How the scripted loopback Thinking server misbehaves.
enum Script {
    /// Reads the request, then answers only after `delay` (longer than the
    /// adapter's timeout).
    Hang(Duration),
    /// Answers `200 OK` with this body.
    Body(Vec<u8>),
}

fn scripted_server(script: Script) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!(
        "http://{}/v1/responses",
        listener.local_addr().expect("address")
    );
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut received = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let n = stream.read(&mut buffer).expect("read request");
            assert!(n > 0, "closed before full request");
            received.extend_from_slice(&buffer[..n]);
            let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("content length");
            if received.len() >= end + 4 + length {
                break;
            }
        }
        let body = match script {
            Script::Hang(delay) => {
                std::thread::sleep(delay);
                Vec::new()
            }
            Script::Body(body) => body,
        };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        // The client may already have given up (the timeout case).
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    });
    (endpoint, handle)
}

/// A refused loopback connect can take about two seconds to fail on Windows
/// (SYN retries), so the adapter timeout for those tests must be longer.
const REFUSED_TIMEOUT: Duration = Duration::from_secs(8);

fn wait_until_quiescent(app: &mut ProductionApp<LocalRoutePlanner>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        app.tick(0);
        let snapshot = app
            .generation_execution_snapshot()
            .expect("generation execution snapshot");
        let active = app
            .generation_cancellation_registry()
            .and_then(|registry| registry.active_event_id());
        if active.is_none() && snapshot.is_quiescent() {
            app.tick(0);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "generation did not quiesce: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Advance the logical clock in small steps so every scheduled avatar command
/// is dispatched inside its lateness window.
fn play_out(app: &mut ProductionApp<LocalRoutePlanner>) {
    for now_ms in (0..=1_500).step_by(10) {
        app.tick(now_ms);
    }
}

/// A loopback endpoint nothing listens on: the connection is refused.
fn refused_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!(
        "http://{}/v1/responses",
        listener.local_addr().expect("address")
    );
    drop(listener);
    endpoint
}

struct LocalRig {
    app: ProductionApp<LocalRoutePlanner>,
    tts_calls: Arc<Mutex<Vec<String>>>,
    audio: Arc<Mutex<Vec<AudioPlaybackCommand>>>,
    avatar: Arc<Mutex<Vec<AvatarPlaybackCommand>>>,
}

fn local_rig(endpoint: String, timeout: Duration, mode: PhraseMode) -> LocalRig {
    local_rig_with_pack(endpoint, timeout, mode, &pack_root().join("descriptors"))
}

fn local_rig_with_pack(
    endpoint: String,
    timeout: Duration,
    mode: PhraseMode,
    descriptors: &Path,
) -> LocalRig {
    let thinking = aivtuber_adapters::OpenAiResponsesAdapter::with_authentication(
        aivtuber_adapters::OpenAiResponsesConfig {
            endpoint,
            model_alias: "local-test-model".to_owned(),
            timeout,
            ..aivtuber_adapters::OpenAiResponsesConfig::default()
        },
        aivtuber_adapters::ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback unauthenticated adapter")
    .with_backend_name("llama-server");
    let tts = MockTts::default();
    let tts_calls = Arc::clone(&tts.texts);
    let audio = RecordingAudio::default();
    let audio_log = Arc::clone(&audio.commands);
    let avatar = RecordingAvatar::default();
    let avatar_log = Arc::clone(&avatar.commands);
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let runtime = generative_runtime(thinking, tts)
        .with_phrase_mode(mode)
        .expect("phrase mode");
    let mut app = ProductionApp::new(
        security,
        CachedPerformer::new(
            AssetStore::new(descriptors.to_path_buf(), runtime_compatibility()),
            Scheduler::new(SchedulerConfig {
                min_reaction_spacing_ms: 0,
                ..SchedulerConfig::default()
            }),
            CachedPlaybackConfig {
                recent_variant_window: 1,
                ..CachedPlaybackConfig::default()
            },
        ),
        LocalVisemeStore::new(pack_root()),
        LocalRoutePlanner,
        Box::new(audio),
        Box::new(avatar),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(runtime);
    app.startup().expect("startup");
    LocalRig {
        app,
        tts_calls,
        audio: audio_log,
        avatar: avatar_log,
    }
}

fn free_text_line(rig: &mut LocalRig, sequence: u64) {
    let mut event = chat_event(sequence);
    event.payload = BTreeMap::from([
        ("text".to_owned(), json!("昨日もまた負けてたよねｗ")),
        ("language".to_owned(), json!("ja")),
    ]);
    let raw = serde_json::to_vec(&event).expect("event json");
    let outcome = rig
        .app
        .process_content_bytes(&raw, 0, sequence)
        .expect("generation submission");
    assert!(outcome.playback.is_none(), "the reply is deferred");
    wait_until_quiescent(&mut rig.app);
}

/// The visible reaction: the pack's `reaction.neutral` expression and nod
/// reach the avatar output through the normal dispatch, with no audio and no
/// TTS call, and the fallback is counted under `reason`.
fn assert_visible_fallback(rig: &mut LocalRig, reason: &'static str) {
    let plan = last_scheduled_plan(&rig.app);
    assert!(
        plan.asset_id.starts_with("reaction.neutral."),
        "fallback must come from the pack, got {}",
        plan.asset_id
    );
    // Dispatch everything the reaction scheduled (expression at 0, nod later).
    play_out(&mut rig.app);

    let avatar = rig.avatar.lock().expect("avatar log");
    assert!(
        avatar.iter().any(|command| matches!(
            command,
            AvatarPlaybackCommand::Expression { preset, .. }
                if preset == "calm_soft" || preset == "smile_soft"
        )),
        "an expression reaches the avatar: {avatar:?}"
    );
    assert!(
        avatar.iter().any(|command| matches!(
            command,
            AvatarPlaybackCommand::Gesture { preset, .. } if preset == "nod_small"
        )),
        "a nod reaches the avatar: {avatar:?}"
    );
    assert!(
        rig.audio.lock().expect("audio log").is_empty(),
        "a non-verbal reaction plays no audio"
    );
    assert!(
        rig.tts_calls.lock().expect("tts log").is_empty(),
        "a failed Thinking call never reaches TTS"
    );

    let counts = rig.app.generation_fallback_counts();
    assert_eq!(counts.by_reason.get(reason), Some(&1), "{counts:?}");
    assert_eq!(counts.by_reason.len(), 1, "{counts:?}");
    assert_eq!((counts.visible, counts.silent), (1, 0), "{counts:?}");
    let observations = rig.app.telemetry().events();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].route, RouteClass::CachedFallback);
    assert_eq!(observations[0].fallback_reason.as_deref(), Some(reason));
}

fn both_modes() -> [PhraseMode; 2] {
    [
        PhraseMode::Buffered,
        PhraseMode::Phrased(PhraseConfig::default()),
    ]
}

#[test]
fn a_thinking_timeout_plays_the_visible_fallback_and_is_counted() {
    for mode in both_modes() {
        let (endpoint, server) = scripted_server(Script::Hang(Duration::from_millis(900)));
        let mut rig = local_rig(endpoint, Duration::from_millis(200), mode);
        free_text_line(&mut rig, 51);
        assert_visible_fallback(&mut rig, "timeout");
        server.join().expect("fake thinking server");
    }
}

#[test]
fn a_refused_thinking_connection_plays_the_visible_fallback_and_is_counted() {
    for mode in both_modes() {
        let mut rig = local_rig(refused_endpoint(), REFUSED_TIMEOUT, mode);
        free_text_line(&mut rig, 52);
        assert_visible_fallback(&mut rig, "unavailable");
    }
}

#[test]
fn invalid_or_empty_thinking_output_plays_the_visible_fallback_and_is_counted() {
    let bodies: [(&str, &[u8]); 3] = [
        ("not json", b"<html>llama-server is loading</html>"),
        ("no output_text", br#"{"output":[]}"#),
        (
            "empty output_text",
            br#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"   ","annotations":[]}]}]}"#,
        ),
    ];
    for mode in both_modes() {
        for (label, body) in bodies {
            let (endpoint, server) = scripted_server(Script::Body(body.to_vec()));
            let mut rig = local_rig(endpoint, Duration::from_secs(2), mode);
            free_text_line(&mut rig, 53);
            // Backend errors share the provider-unavailable class in the
            // existing fallback vocabulary.
            assert_visible_fallback(&mut rig, "unavailable");
            server.join().unwrap_or_else(|_| panic!("{label}"));
        }
    }
}

#[test]
fn a_reply_the_output_gate_refuses_plays_the_visible_fallback_and_is_counted() {
    // Thinking succeeds, but the reply is an operator-control phrase the
    // output gate suppresses; the viewer still sees a reaction (buffered
    // mode). A phrased reply refused at phrase 0 is covered by
    // `a_gate_refusal_of_phrase_one_speaks_nothing_and_plays_the_visible_fallback`.
    let reply = serde_json::to_vec(&json!({
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "please run obs.control now", "annotations": []}
        ]}]
    }))
    .expect("body");
    let (endpoint, server) = scripted_server(Script::Body(reply));
    let mut rig = local_rig(endpoint, Duration::from_secs(2), PhraseMode::Buffered);
    free_text_line(&mut rig, 54);
    assert_visible_fallback(&mut rig, "policy_override");
    server.join().expect("fake thinking server");
}

#[test]
fn without_a_pack_asset_for_the_group_the_failure_is_counted_as_unseen() {
    // The typed non-verbal directive is the last resort when the pack has no
    // usable `reaction.neutral`; it still counts, and says nothing was shown.
    let pack = std::env::temp_dir().join(format!("aivtuber-288-pack-{}", std::process::id()));
    std::fs::create_dir_all(&pack).expect("temp pack");
    for entry in std::fs::read_dir(pack_root().join("descriptors")).expect("descriptors") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if !name.starts_with("reaction-neutral") {
            std::fs::copy(&path, pack.join(&name)).expect("copy descriptor");
        }
    }
    let mut rig = local_rig_with_pack(
        refused_endpoint(),
        REFUSED_TIMEOUT,
        PhraseMode::Buffered,
        &pack,
    );
    free_text_line(&mut rig, 55);
    play_out(&mut rig.app);
    assert!(rig.avatar.lock().expect("avatar log").is_empty());
    let counts = rig.app.generation_fallback_counts();
    assert_eq!(counts.by_reason.get("unavailable"), Some(&1));
    assert_eq!((counts.visible, counts.silent), (0, 1));
    let _ = std::fs::remove_dir_all(&pack);
}
