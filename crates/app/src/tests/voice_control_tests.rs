//! The live voice settings end to end (issue #360).
//!
//! Real generation pipeline, real `NormalizedHttpTtsAdapter`, real in-process
//! Aivis bridge and swap; the engines are doubles on loopback: a tone
//! `local-tts-sidecar` for the local voice, a fake Aivis Cloud API for the
//! cloud one (no key, no network beyond 127.0.0.1). Thinking is a scripted
//! stub behind the real preview override. Every operator action goes through
//! `LocalControlIngress` like the panel and `aivtuberctl` do.

use super::local_thinking_fallback_tests::{
    LocalRig, assert_visible_fallback, local_rig_with_engines, submit_free_text_line,
    wait_until_quiescent,
};
use super::*;
use aivtuber_adapters::speech_tts::ToneSynthesizer;
use aivtuber_adapters::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
use aivtuber_adapters::speech_tts::server::{DEFAULT_PATH, ServerConfig, ServerHandle, TtsServer};
use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig, SecretString};
use std::io::{Read, Write};

const MODEL: &str = "a59cb814-0083-4369-8542-f51a29e72af7";
const SPEAKER: &str = "e756b8e4-b606-4e15-99b1-3f9c6a1b2317";
const KEY: &str = "sk-e2e-SENTINEL-KEY-0123456789";
const CREDIT: &str = "AivisSpeech: まお (SENTINEL-CREDIT)";
const SECRET: [u8; 32] = [0x5A; 32];
const MODEL_REPLY: &str = "モデルの返事です。聞こえていますか。";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-360e-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("local-audio")).expect("scratch dir");
    dir
}

fn voice_ingress(capabilities: &[Capability]) -> LocalControlIngress {
    LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        capabilities.iter().copied().collect(),
        ControlSecret::new(SECRET),
    )
    .expect("ingress")
}

struct Setup {
    rig: LocalRig,
    api: FakeAivisCloud,
    adapter: NormalizedHttpTtsAdapter,
    ingress: LocalControlIngress,
    root: PathBuf,
    now_ms: u64,
    seed: u64,
    line: u64,
    _local: ServerHandle,
}

impl Setup {
    fn new(behaviour: FakeAivisBehaviour, env_key: Option<&str>, mode: PhraseMode) -> Self {
        let root = scratch("rig");
        let local = TtsServer::bind(
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), root.join("local-audio")),
            Arc::new(ToneSynthesizer::default()),
        )
        .expect("local sidecar double")
        .spawn();
        let adapter = NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: format!("http://{}{DEFAULT_PATH}", local.local_addr()),
                backend_name: "local-tts-sidecar".to_owned(),
                model_alias: Some("supertonic".to_owned()),
                model_version: None,
                voice_model: Some("example-voice-v1".to_owned()),
                viseme_mapping: Some("ja-5vowel-v1".to_owned()),
                timeout: Duration::from_secs(10),
            },
            None,
        )
        .expect("adapter");
        let api = FakeAivisCloud::start(behaviour);
        let slot = PreviewSlot::default();
        let controller = VoiceController::new(
            VoiceEnvironment {
                paths: VoicePaths {
                    settings: root.join("local-voice.json"),
                    key_file: root.join("local-aivis-key.txt"),
                },
                env_key: env_key.map(SecretString::new),
                endpoint: api.endpoint(),
                synth_timeout: Duration::from_secs(5),
                amplitude_visemes: false,
                audio_root: root.join("audio"),
                pack_root: root.clone(),
            },
            adapter.clone(),
            Arc::new(AtomicBool::new(false)),
            slot.clone(),
            aivtuber_domain::VoiceSettings::default(),
        );
        let thinking = PreviewThinking::new(
            Arc::new(MockThinking {
                reply: MODEL_REPLY.to_owned(),
                failure: None,
            }),
            slot,
        );
        let mut rig = local_rig_with_engines(thinking, adapter.clone(), mode);
        rig.app = rig.app.with_voice_controller(controller);
        Self {
            rig,
            api,
            adapter,
            ingress: voice_ingress(&[
                Capability::PerformerStop,
                Capability::PerformerMute,
                Capability::VoiceConfigure,
            ]),
            root,
            now_ms: 0,
            seed: 100,
            line: 100,
            _local: local,
        }
    }

    fn act(&mut self, action: &str, argument: Option<&str>) -> OperatorResponse {
        self.now_ms += 10;
        apply_operator_action(
            &self.ingress,
            &mut self.rig.app,
            &format!("e2e-{}", self.now_ms),
            action,
            argument,
            &SECRET,
            self.now_ms,
        )
    }

    fn apply(&mut self, patch: &str) -> OperatorResponse {
        self.act("voice.apply", Some(patch))
    }

    /// Advance the logical clock until generation is quiescent, then far
    /// enough that everything scheduled has been dispatched.
    fn run_to_quiescence(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            self.now_ms += 10;
            self.rig.app.tick(self.now_ms);
            let snapshot = self
                .rig
                .app
                .generation_execution_snapshot()
                .expect("generation execution snapshot");
            if snapshot.is_quiescent() {
                break;
            }
            assert!(Instant::now() < deadline, "no quiescence: {snapshot:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..200 {
            self.now_ms += 10;
            self.rig.app.tick(self.now_ms);
        }
    }

    /// A free-text viewer line, played out on a clock that only moves forward.
    fn say(&mut self) {
        self.line += 1;
        let mut event = chat_event(self.line);
        event.payload = BTreeMap::from([
            ("text".to_owned(), json!("昨日もまた負けてたよねｗ")),
            ("language".to_owned(), json!("ja")),
        ]);
        let raw = serde_json::to_vec(&event).expect("event json");
        self.rig
            .app
            .process_content_bytes(&raw, self.now_ms, self.line)
            .expect("line");
        self.run_to_quiescence();
    }

    /// Submit the preview line the operator queued, and play it out.
    fn preview_out(&mut self) {
        self.rig.app.service_voice(self.now_ms, &mut self.seed);
        self.run_to_quiescence();
    }

    fn enable_patch() -> String {
        format!(
            r#"{{"backend":"aivis","model_uuid":"{MODEL}","model_name":"まお","speaker_uuid":"{SPEAKER}","style_id":2,"credit":"{CREDIT}","confirm_egress":true}}"#
        )
    }

    fn synth_requests(&self) -> Vec<serde_json::Value> {
        self.api
            .requests()
            .iter()
            .filter(|r| r.route() == "/v1/tts/synthesize")
            .map(|r| serde_json::from_slice(&r.body).expect("synthesis body"))
            .collect()
    }

    fn audit(&self) -> Vec<(String, String)> {
        self.rig
            .app
            .security()
            .audit()
            .iter()
            .map(|r| (r.decision.clone(), r.detail.clone()))
            .collect()
    }

    fn played(&self) -> usize {
        self.rig.audio.lock().expect("audio log").len()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.rig.app.shutdown_voice();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_reply_follows_the_voice_the_operator_chose_live_and_back_to_local() {
    for mode in super::local_thinking_fallback_tests::both_modes() {
        let mut s = Setup::new(FakeAivisBehaviour::default(), None, mode);
        // Local: nothing leaves the machine.
        s.say();
        assert!(s.played() > 0, "the local voice speaks");
        assert!(s.api.requests().is_empty());
        let local_plays = s.played();

        // No key yet: refused with a reason, still local.
        let refused = s.apply(&Setup::enable_patch());
        assert!(
            !refused.ok && refused.detail.contains("API key"),
            "{refused:?}"
        );
        let set = s.act("voice.key.set", Some(KEY));
        assert!(set.ok, "{set:?}");
        let on = s.apply(&Setup::enable_patch());
        assert!(on.ok, "{on:?}");
        s.say();
        let spoken = s.api.spoken_texts();
        assert!(!spoken.is_empty(), "the reply is synthesized by Aivis");
        assert!(spoken.join("").contains("聞こえていますか"), "{spoken:?}");
        assert!(s.played() > local_plays, "and played");
        let requests = s.synth_requests();
        assert_eq!(requests[0]["style_id"], 2);
        assert_eq!(requests[0]["model_uuid"], MODEL);
        let aivis_calls = requests.len();
        // Provenance of the next asset names the Aivis voice.
        assert_eq!(s.adapter.target().backend_name, "aivis-cloud");

        // A style change and a tuning change apply to the very next reply.
        let tuned = s.apply(r#"{"style_id":4,"speaking_rate":1.5}"#);
        assert!(tuned.ok, "{tuned:?}");
        s.say();
        let requests = s.synth_requests();
        assert!(
            requests.len() > aivis_calls,
            "same text, other voice: synthesized again"
        );
        let last = requests.last().unwrap();
        assert_eq!(
            (last["style_id"].as_u64(), last["speaking_rate"].as_f64()),
            (Some(4), Some(1.5))
        );

        // Back to local: speech continues, nothing more is sent.
        let sent = s.api.requests().len();
        let off = s.apply(r#"{"backend":"local"}"#);
        assert!(off.ok, "{off:?}");
        let plays = s.played();
        s.say();
        assert!(s.played() > plays, "the local voice speaks again");
        assert_eq!(s.api.requests().len(), sent, "no more egress");
        let status = OperatorStatusSnapshot::from_app(&s.rig.app, 1_000)
            .voice
            .expect("voice status");
        assert_eq!(status.backend, "local");
        assert_eq!(status.style_id, Some(4), "the choice is remembered");
        let counts = s.rig.app.generation_fallback_counts();
        assert!(
            counts.by_reason.is_empty(),
            "no fallback along the way: {counts:?}"
        );

        // Every change is an audited operator action; none shows the key or
        // the credit line.
        let audit = s.audit();
        let voice: Vec<_> = audit
            .iter()
            .filter(|(_, d)| d.starts_with("action=voice."))
            .collect();
        assert!(
            voice
                .iter()
                .any(|(decision, d)| decision == "operator_rejected" && d.contains("API key")),
            "{voice:?}"
        );
        assert!(
            voice
                .iter()
                .any(|(decision, d)| decision == "operator_accepted"
                    && d.starts_with("action=voice.key.set")),
            "{voice:?}"
        );
        assert!(
            voice
                .iter()
                .filter(|(_, d)| d.starts_with("action=voice.apply"))
                .count()
                >= 4
        );
        for (_, detail) in &audit {
            assert!(!detail.contains("SENTINEL"), "{detail}");
        }
    }
}

#[test]
fn a_rejected_key_degrades_to_the_visible_fallback_and_is_shown() {
    let mut s = Setup::new(
        FakeAivisBehaviour {
            accept_key: Some(KEY.to_owned()),
            ..FakeAivisBehaviour::default()
        },
        None,
        PhraseMode::Buffered,
    );
    assert!(s.act("voice.key.set", Some("a-wrong-key")).ok);
    assert!(s.apply(&Setup::enable_patch()).ok);
    submit_free_text_line(&mut s.rig, 7);
    wait_until_quiescent(&mut s.rig.app);
    // The reply degrades like any TTS failure; the performer is not frozen.
    assert_visible_fallback(&mut s.rig, "unavailable");
    s.now_ms = 2_000;
    let mut seed = 0;
    // The key check (on its own thread) reports rejected without blocking.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        s.rig.app.service_voice(s.now_ms, &mut seed);
        let status = s.rig.app.voice_status().expect("status");
        if status.key.check != "checking" {
            break status;
        }
        assert!(Instant::now() < deadline, "{status:?}");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.key.check, "rejected");
    let error = status.last_error.expect("the failure is shown");
    assert!(
        error.contains("HTTP 401") && !error.contains("wrong"),
        "{error}"
    );
    // Fixing the key fixes the next reply, with no restart.
    assert!(s.act("voice.key.set", Some(KEY)).ok);
    let before = s.played();
    s.say();
    assert!(s.played() > before, "the corrected key speaks");
    assert!(s.rig.app.voice_status().unwrap().last_error.is_none());
}

#[test]
fn the_preview_speaks_the_fixed_sentence_through_the_normal_path_and_cannot_be_forged() {
    let mut s = Setup::new(
        FakeAivisBehaviour::default(),
        Some(KEY),
        PhraseMode::Buffered,
    );
    assert!(s.apply(&Setup::enable_patch()).ok);
    let queued = s.act("voice.preview", None);
    assert!(queued.ok, "{queued:?}");
    let again = s.act("voice.preview", None);
    assert!(
        !again.ok && again.detail.contains("already waiting"),
        "{again:?}"
    );
    s.preview_out();
    let spoken = s.api.spoken_texts().join("");
    assert!(spoken.contains("この声で配信をお届けしますね"), "{spoken}");
    assert!(
        !spoken.contains("聞こえていますか"),
        "the model was not asked: {spoken}"
    );
    assert!(s.played() > 0, "audio reached the output path");
    assert!(
        s.audit()
            .iter()
            .any(|(d, detail)| d == "operator_accepted"
                && detail.starts_with("action=voice.preview")),
        "{:?}",
        s.audit()
    );
    // It was one-shot and tied to the operator's line: a viewer who types the
    // same sentence gets the model.
    let before = s.api.spoken_texts().len();
    s.line += 1;
    let mut event = chat_event(s.line);
    event.payload = BTreeMap::from([(
        "text".to_owned(),
        json!(aivtuber_domain::VOICE_PREVIEW_TEXT),
    )]);
    let raw = serde_json::to_vec(&event).unwrap();
    s.rig
        .app
        .process_content_bytes(&raw, s.now_ms, s.line)
        .expect("line");
    s.run_to_quiescence();
    let spoken = s.api.spoken_texts();
    assert!(spoken.len() > before);
    assert!(
        spoken[before..].join("").contains("聞こえていますか"),
        "{spoken:?}"
    );
}

#[test]
fn mute_and_stop_apply_to_the_preview_like_to_any_utterance() {
    let mut s = Setup::new(
        FakeAivisBehaviour::default(),
        Some(KEY),
        PhraseMode::Buffered,
    );
    assert!(s.apply(&Setup::enable_patch()).ok);
    assert!(s.act("mute", None).ok);
    assert!(s.act("voice.preview", None).ok);
    s.preview_out();
    assert_eq!(s.played(), 0, "a muted performer plays nothing");
}

#[test]
fn the_operator_endpoint_never_takes_the_key_and_a_missing_capability_changes_nothing() {
    let mut s = Setup::new(FakeAivisBehaviour::default(), None, PhraseMode::Buffered);
    // The TCP endpoint (aivtuberctl) has no key action: a key must not travel
    // in a command line or a request log.
    let secret = String::from_utf8(SECRET.to_vec()).unwrap();
    let response = apply_operator_request(
        &s.ingress,
        &mut s.rig.app,
        &OperatorRequest {
            action: "voice.key.set".to_owned(),
            secret: secret.clone(),
            request_id: Some("tcp-1".to_owned()),
            argument: Some(KEY.to_owned()),
        },
        10,
    );
    assert!(
        !response.ok && response.detail.contains("unknown action"),
        "{response:?}"
    );
    assert!(!s.rig.app.voice_status().unwrap().key.set);
    assert!(!s.root.join("local-aivis-key.txt").exists());
    // The other voice actions are available there (aivtuberctl parity).
    let response = apply_operator_request(
        &s.ingress,
        &mut s.rig.app,
        &OperatorRequest {
            action: "voice.apply".to_owned(),
            secret: secret.clone(),
            request_id: Some("tcp-2".to_owned()),
            argument: Some(r#"{"pitch":0.25}"#.to_owned()),
        },
        20,
    );
    assert!(response.ok, "{response:?}");
    assert_eq!(s.rig.app.voice_status().unwrap().tuning.pitch, 0.25);
    // An ingress that does not hold `voice.configure` cannot mint it.
    let weak = voice_ingress(&[Capability::PerformerStop, Capability::PerformerMute]);
    let response = apply_operator_action(
        &weak,
        &mut s.rig.app,
        "weak-1",
        "voice.apply",
        Some(r#"{"pitch":-0.5}"#),
        &SECRET,
        30,
    );
    assert!(
        !response.ok && response.detail.contains("authentication failed"),
        "{response:?}"
    );
    assert_eq!(s.rig.app.voice_status().unwrap().tuning.pitch, 0.25);
    // A wrong secret changes nothing either.
    let response = apply_operator_action(
        &s.ingress,
        &mut s.rig.app,
        "wrong-1",
        "voice.apply",
        Some(r#"{"pitch":-0.5}"#),
        &[0x11; 32],
        40,
    );
    assert!(!response.ok);
    assert_eq!(s.rig.app.voice_status().unwrap().tuning.pitch, 0.25);
}

/// One HTTP exchange to the panel on a worker thread while this thread plays
/// the composition loop.
fn call(
    service: &mut PanelService,
    app: &mut ProductionApp<LocalRoutePlanner>,
    port: u16,
    request: String,
) -> (u16, String) {
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap();
        (
            status,
            text.split_once("\r\n\r\n")
                .map_or(String::new(), |p| p.1.to_owned()),
        )
    });
    let started = Instant::now();
    let mut now_ms = 1_000;
    let mut seed = 0;
    while !client.is_finished() && started.elapsed() < Duration::from_secs(8) {
        service.service(app, now_ms, &mut seed);
        app.service_voice(now_ms, &mut seed);
        now_ms += 10;
        std::thread::sleep(Duration::from_millis(5));
    }
    client.join().unwrap()
}

#[test]
fn the_key_entered_in_the_panel_reaches_the_file_and_no_other_output() {
    let mut s = Setup::new(
        FakeAivisBehaviour {
            accept_key: Some(KEY.to_owned()),
            ..FakeAivisBehaviour::default()
        },
        None,
        PhraseMode::Buffered,
    );
    let mut panel = PanelService::start(
        &PanelSettings {
            port: 0,
            page_dir: None,
            explicit: true,
        },
        Arc::new(voice_ingress(&[
            Capability::PerformerStop,
            Capability::PerformerMute,
            Capability::VoiceConfigure,
        ])),
        &SECRET,
        None,
        Some(s.api.endpoint()),
        None,
    )
    .expect("panel");
    let url = panel.launch_url();
    let (origin, token) = url.split_once("/#token=").expect("fragment url");
    let port: u16 = origin.rsplit(':').next().unwrap().parse().unwrap();
    let post = |path: &str, body: &str| {
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\n\
             X-Panel-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let get_state = || {
        format!(
            "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Panel-Token: {token}\r\n\r\n"
        )
    };
    let mut outputs: Vec<String> = Vec::new();

    let (status, body) = call(
        &mut panel,
        &mut s.rig.app,
        port,
        post("/api/voice/key", &format!("{{\"key\":\"{KEY}\"}}")),
    );
    assert_eq!(status, 200, "{body}");
    outputs.push(body);
    let (status, body) = call(
        &mut panel,
        &mut s.rig.app,
        port,
        post("/api/voice", &Setup::enable_patch()),
    );
    assert_eq!(status, 200, "{body}");
    outputs.push(body);
    // Let the key check finish, then read what the page would read.
    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        let (status, body) = call(&mut panel, &mut s.rig.app, port, get_state());
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        if value["status"]["voice"]["key"]["check"] == "valid" {
            break (body, value);
        }
        assert!(Instant::now() < deadline, "{body}");
        std::thread::sleep(Duration::from_millis(60));
    };
    outputs.push(state.0);
    let voice = &state.1["status"]["voice"];
    assert_eq!(voice["backend"], "aivis");
    assert_eq!(voice["key"]["set"], true);
    assert_eq!(voice["key"]["source"], "file");
    assert_eq!(voice["model_uuid"], MODEL);
    assert_eq!(voice["style_id"], 2);
    assert_eq!(voice["credit"], CREDIT);
    // Speak once so synthesis logs and errors exist too.
    s.say();
    let (_, body) = call(&mut panel, &mut s.rig.app, port, get_state());
    outputs.push(body);
    let snapshot = OperatorStatusSnapshot::from_app(&s.rig.app, 9_000);
    outputs.push(serde_json::to_string(&snapshot).unwrap());
    outputs.push(format!("{:?}", snapshot));
    outputs.push(format!("{panel:?}"));
    for (_, detail) in s.audit() {
        outputs.push(detail);
    }
    for (index, output) in outputs.iter().enumerate() {
        assert!(
            !output.contains("SENTINEL-KEY"),
            "output {index} holds the key: {output}"
        );
    }
    // The key lives in exactly one local file, and nowhere in the settings.
    assert!(
        std::fs::read_to_string(s.root.join("local-aivis-key.txt"))
            .unwrap()
            .contains("SENTINEL-KEY")
    );
    let settings = std::fs::read_to_string(s.root.join("local-voice.json")).unwrap();
    assert!(
        !settings.contains("SENTINEL-KEY") && settings.contains(MODEL),
        "{settings}"
    );
    // Removing it through the panel ends the Aivis voice.
    let (status, body) = call(
        &mut panel,
        &mut s.rig.app,
        port,
        post("/api/voice/key/remove", "{}"),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("local voice"), "{body}");
    assert!(!s.root.join("local-aivis-key.txt").exists());
    assert_eq!(s.rig.app.voice_status().unwrap().backend, "local");
}

#[test]
fn without_voice_settings_the_actions_are_refused_and_nothing_is_composed() {
    let mut s = Setup::new(FakeAivisBehaviour::default(), None, PhraseMode::Buffered);
    // A bare app (no controller): the capability may exist but the state does not.
    let mut bare = local_rig_with_engines(
        MockThinking {
            reply: MODEL_REPLY.to_owned(),
            failure: None,
        },
        s.adapter.clone(),
        PhraseMode::Buffered,
    );
    assert!(bare.app.voice_status().is_none() && !bare.app.voice_configured());
    let response = apply_operator_action(
        &s.ingress,
        &mut bare.app,
        "bare-1",
        "voice.apply",
        Some("{}"),
        &SECRET,
        10,
    );
    assert!(
        !response.ok && response.detail.contains("not configured"),
        "{response:?}"
    );
    assert!(
        OperatorStatusSnapshot::from_app(&bare.app, 0)
            .voice
            .is_none(),
        "no voice block in the status"
    );
    let _ = &mut s;
}
