//! Chat connections from the panel, at the app level (issue #361).
//!
//! The process supervision is covered by `tests/chat_ingress_supervision.rs`
//! against a sidecar double. Here the controller is fed through its test seam
//! (no child), so what is under test is the app side: the records are admitted
//! through `process_content_bytes` like a stdin line and answered, the counters
//! follow the admission verdicts, and every operator action goes through
//! `LocalControlIngress` with its own capability and an audit record that never
//! holds the key.

use super::local_thinking_fallback_tests::{LocalRig, local_rig_with_engines};
use super::*;
use crate::chat_ingress::{ChatIngressController, ChatIngressEnvironment, ChatPlatform, ChatState};
use std::sync::mpsc::SyncSender;

const KEY: &str = "AIza-e2e-SENTINEL-KEY-0123456789";
const SECRET: [u8; 32] = [0x6B; 32];
const REPLY: &str = "コメントありがとう。";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-361e-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn ingress(capabilities: &[Capability]) -> LocalControlIngress {
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
    ingress: LocalControlIngress,
    twitch: SyncSender<Vec<u8>>,
    root: PathBuf,
    now_ms: u64,
    seed: u64,
}

impl Setup {
    fn new() -> Self {
        let root = scratch("rig");
        let mut controller = ChatIngressController::new(ChatIngressEnvironment::new(
            root.join("local-chat.json"),
            root.join("local-youtube-key.txt"),
            None,
            // No sidecar exists here: nothing can be spawned.
            &root,
            None,
            RawIngressConfig {
                max_record_bytes: 64 * 1024,
                queue_capacity: 256,
            },
        ));
        let twitch = controller.attach_queue_for_test(ChatPlatform::Twitch, "somechannel");
        let mut rig = local_rig_with_engines(
            MockThinking {
                reply: REPLY.to_owned(),
                failure: None,
            },
            MockTts::default(),
            PhraseMode::Buffered,
        );
        rig.app = rig.app.with_chat_ingress(controller);
        Self {
            rig,
            ingress: ingress(&[
                Capability::PerformerStop,
                Capability::PerformerMute,
                Capability::IngressControl,
            ]),
            twitch,
            root,
            now_ms: 0,
            seed: 0,
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

    fn line(&self, n: u64, source_class: &str, trust: &str, source: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": aivtuber_domain::EVENT_SCHEMA_VERSION,
            "event_id": format!("twitch-{n}"),
            "correlation_id": format!("twitch-{n}"),
            "sequence": n,
            "observed_at": "2026-11-13T00:00:00Z",
            "source": source,
            "source_class": source_class,
            "plane": "content",
            "trust_level": trust,
            "kind": "chat.message",
            "actor_id": format!("viewer:twitch-{n}"),
            "priority_hint": null,
            "payload": { "text": "昨日もまた負けてたよねｗ", "language": "ja" },
        }))
        .expect("line")
    }

    fn valid(&self, n: u64) -> Vec<u8> {
        self.line(n, "public_chat", "untrusted", "twitch:somechannel")
    }

    fn service(&mut self) {
        self.now_ms += 10;
        self.rig
            .app
            .service_chat_ingress(self.now_ms, &mut self.seed);
    }

    fn twitch_status(&self) -> crate::chat_ingress::ChatPlatformStatus {
        self.rig.app.chat_ingress_status().expect("composed").twitch
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
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.rig.app.shutdown_chat_ingress();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn chat_records_are_admitted_like_a_stdin_line_and_get_a_reply() {
    let mut s = Setup::new();
    s.twitch.send(s.valid(1)).unwrap();
    s.service();
    assert_eq!(s.twitch_status().messages_accepted, 1);
    assert_eq!(s.twitch_status().messages_dropped, 0);
    // The reply is generated, synthesized and played through the ordinary pipeline.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        s.now_ms += 10;
        s.rig.app.tick(s.now_ms);
        if !s.rig.audio.lock().expect("audio log").is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "no reply was played");
        std::thread::sleep(Duration::from_millis(2));
    }
    let replies: Vec<_> = s.rig.app.recent_replies().items().collect();
    assert!(
        replies.iter().any(|r| r.event_id == "twitch-1"),
        "{replies:?}"
    );
}

#[test]
fn a_record_above_the_panel_source_ceiling_is_dropped_before_admission() {
    let mut s = Setup::new();
    for (n, (class, trust, source)) in [
        ("public_chat", "trusted", "twitch:somechannel"),
        ("operator", "untrusted", "twitch:somechannel"),
        ("system", "trusted", "twitch:somechannel"),
        ("public_chat", "untrusted", "local-panel"),
        ("donation", "untrusted", "twitch:somechannel"),
    ]
    .into_iter()
    .enumerate()
    {
        s.twitch
            .send(s.line(n as u64, class, trust, source))
            .unwrap();
    }
    s.twitch.send(b"not json".to_vec()).unwrap();
    s.service();
    let status = s.twitch_status();
    assert_eq!(status.messages_accepted, 0);
    assert_eq!(status.dropped_refused, 6);
    assert_eq!(status.messages_dropped, 6);
    assert_eq!(s.rig.app.security().ingress_policy_rejected(), 0);
    assert!(s.rig.audio.lock().expect("audio log").is_empty());
}

#[test]
fn the_app_takes_a_bounded_number_of_records_per_turn() {
    let mut s = Setup::new();
    for n in 0..100 {
        s.twitch.send(s.valid(n)).unwrap();
    }
    s.service();
    let after_one_turn = s.twitch_status();
    assert_eq!(
        after_one_turn.messages_accepted + after_one_turn.dropped_refused,
        crate::chat_ingress::CHAT_RECORDS_PER_TICK as u64
    );
}

#[test]
fn every_action_needs_the_capability_and_the_key_is_never_audited_or_returned() {
    let mut s = Setup::new();
    // Without `ingress.control` the ingress cannot mint the capability.
    let weak = ingress(&[Capability::PerformerStop, Capability::PerformerMute]);
    let response = apply_operator_action(
        &weak,
        &mut s.rig.app,
        "weak-1",
        "ingress.connect",
        Some(r#"{"platform":"twitch","target":"somechannel"}"#),
        &SECRET,
        10,
    );
    assert!(
        !response.ok && response.detail.contains("authentication failed"),
        "{response:?}"
    );
    // A wrong secret changes nothing either.
    let response = apply_operator_action(
        &s.ingress,
        &mut s.rig.app,
        "wrong-1",
        "ingress.key.set",
        Some(KEY),
        &[0_u8; 32],
        20,
    );
    assert!(!response.ok);
    assert!(!s.rig.app.chat_ingress_status().unwrap().youtube_key.set);

    // The panel path sets the key: stored, never echoed, never audited.
    let set = s.act("ingress.key.set", Some(KEY));
    assert!(set.ok, "{set:?}");
    assert!(!set.detail.contains(KEY));
    let status = s.rig.app.chat_ingress_status().unwrap();
    assert!(status.youtube_key.set);
    assert_eq!(status.youtube_key.source.as_deref(), Some("file"));
    assert_eq!(
        std::fs::read_to_string(s.root.join("local-youtube-key.txt"))
            .unwrap()
            .trim(),
        KEY
    );
    let mut outputs = vec![
        serde_json::to_string(&status).unwrap(),
        serde_json::to_string(&OperatorStatusSnapshot::from_app(&s.rig.app, 30)).unwrap(),
        format!("{status:?}"),
    ];
    outputs.extend(
        s.audit()
            .into_iter()
            .map(|(d, detail)| format!("{d} {detail}")),
    );
    // A refused connect (the sidecar is not installed in this scratch dir) is
    // audited with the platform and target only.
    let connect = s.act(
        "ingress.connect",
        Some(r#"{"platform":"youtube","target":"https://youtu.be/dQw4w9WgXcQ"}"#),
    );
    assert!(
        !connect.ok && connect.detail.contains("not installed"),
        "{connect:?}"
    );
    outputs.push(format!("{connect:?}"));
    for text in &outputs {
        assert!(!text.contains(KEY), "the key leaked into: {text}");
    }
    assert!(
        s.audit()
            .iter()
            .any(|(d, detail)| d == "operator_rejected"
                && detail.starts_with("action=ingress.connect")),
        "{:?}",
        s.audit()
    );
    assert!(
        s.audit()
            .iter()
            .any(|(d, detail)| d == "operator_accepted"
                && detail.starts_with("action=ingress.key.set")),
        "{:?}",
        s.audit()
    );
    // Removing the key is an audited action too.
    assert!(s.act("ingress.key.remove", None).ok);
    assert!(!s.rig.app.chat_ingress_status().unwrap().youtube_key.set);
    assert!(!s.root.join("local-youtube-key.txt").exists());
}

#[test]
fn the_operator_endpoint_never_takes_the_key_but_has_connect_and_disconnect() {
    let mut s = Setup::new();
    let secret = String::from_utf8(SECRET.to_vec()).unwrap();
    let send = |s: &mut Setup, action: &str, argument: Option<&str>, id: &str| {
        apply_operator_request(
            &s.ingress,
            &mut s.rig.app,
            &OperatorRequest {
                action: action.to_owned(),
                secret: secret.clone(),
                request_id: Some(id.to_owned()),
                argument: argument.map(str::to_owned),
            },
            10,
        )
    };
    // aivtuberctl has no key action: a key must not travel in a command line
    // or a request log.
    let response = send(&mut s, "ingress.key.set", Some(KEY), "tcp-1");
    assert!(
        !response.ok && response.detail.contains("unknown action"),
        "{response:?}"
    );
    assert!(!s.rig.app.chat_ingress_status().unwrap().youtube_key.set);
    // Bad inputs are refused with a reason, whatever the client sent.
    for argument in [
        r#"{"platform":"twitch","target":"a b"}"#,
        r#"{"platform":"twitch","target":"ab"}"#,
        r#"{"platform":"kick","target":"abcdef"}"#,
        r#"{"platform":"twitch","target":"abcdef","program":"cmd.exe"}"#,
        "not json",
    ] {
        let response = send(&mut s, "ingress.connect", Some(argument), "tcp-2");
        assert!(
            !response.ok && response.detail.starts_with("refused: "),
            "{argument}: {response:?}"
        );
    }
    // Disconnect (the attached test connection stands in for a running one).
    let response = send(
        &mut s,
        "ingress.disconnect",
        Some(r#"{"platform":"twitch"}"#),
        "tcp-3",
    );
    assert!(response.ok, "{response:?}");
    assert_eq!(s.twitch_status().state, ChatState::Disconnected);
    // An unknown ingress action is refused.
    let response = send(&mut s, "ingress.reconfigure", None, "tcp-4");
    assert!(!response.ok);
}

#[test]
fn without_a_composed_controller_the_actions_are_refused() {
    let mut rig = local_rig_with_engines(
        MockThinking {
            reply: REPLY.to_owned(),
            failure: None,
        },
        MockTts::default(),
        PhraseMode::Buffered,
    );
    assert!(!rig.app.chat_ingress_configured());
    assert!(rig.app.chat_ingress_status().is_none());
    let response = apply_operator_action(
        &ingress(&[Capability::IngressControl]),
        &mut rig.app,
        "none-1",
        "ingress.connect",
        Some(r#"{"platform":"twitch","target":"somechannel"}"#),
        &SECRET,
        10,
    );
    assert!(
        !response.ok && response.detail.contains("not configured"),
        "{response:?}"
    );
}
