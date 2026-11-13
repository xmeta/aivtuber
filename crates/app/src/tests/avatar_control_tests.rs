//! The avatar choice at the app level (issue #362).
//!
//! The store itself is covered in `avatar_control.rs` against a real avatar
//! server handle. Here every operator action goes through `LocalControlIngress`
//! with its own capability and leaves an audit record that holds the model name
//! and the content hash, and the status reaches the operator snapshot.

use super::local_thinking_fallback_tests::{LocalRig, local_rig_with_engines};
use super::*;
use crate::avatar_control::{AvatarController, AvatarEnvironment, MAX_STORED_MODELS};
use aivtuber_adapters::{LocalAvatarConfig, LocalAvatarServer};

const SECRET: [u8; 32] = [0x7C; 32];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-362e-{name}-{}-{}",
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

/// A minimal valid VRM 1.0 file named `name`.
fn model(name: &str) -> Vec<u8> {
    let json = format!(
        r#"{{"extensions":{{"VRMC_vrm":{{"meta":{{"name":"{name}","authors":["Tester"],"avatarPermission":"onlyAuthor","commercialUsage":"personalNonProfit","creditNotation":"required"}}}}}}}}"#
    );
    let mut json = json.into_bytes();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let bin = [1_u8, 2, 3, 4];
    let total = 12 + 8 + json.len() + 8 + bin.len();
    let mut out = 0x4654_6C67_u32.to_le_bytes().to_vec();
    out.extend_from_slice(&2_u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534A_u32.to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E_4942_u32.to_le_bytes());
    out.extend_from_slice(&bin);
    out
}

fn hash_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Setup {
    rig: LocalRig,
    ingress: LocalControlIngress,
    env: AvatarEnvironment,
    root: PathBuf,
    now_ms: u64,
    server: LocalAvatarServer,
}

impl Setup {
    fn new(capabilities: &[Capability]) -> Self {
        let root = scratch("rig");
        let page = root.join("page");
        std::fs::create_dir_all(&page).expect("page dir");
        std::fs::write(page.join("model.vrm"), model("Sample")).expect("sample");
        let server = LocalAvatarServer::start(LocalAvatarConfig {
            port: 0,
            page_dir: Some(page.clone()),
            ..LocalAvatarConfig::default()
        })
        .expect("avatar server");
        let env = AvatarEnvironment {
            settings: root.join("local-avatar.json"),
            dir: root.join("avatars"),
            sample: Some(page.join("model.vrm")),
        };
        let controller = AvatarController::new(env.clone(), server.handle()).expect("store");
        let mut rig = local_rig_with_engines(
            MockThinking {
                reply: "こんにちは。".to_owned(),
                failure: None,
            },
            MockTts::default(),
            PhraseMode::Buffered,
        );
        rig.app = rig.app.with_avatar_controller(controller);
        Self {
            rig,
            ingress: ingress(capabilities),
            env,
            root,
            now_ms: 0,
            server,
        }
    }

    fn full() -> Self {
        Self::new(&[
            Capability::PerformerStop,
            Capability::PerformerMute,
            Capability::AvatarConfigure,
        ])
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

    /// Plays the panel's HTTP layer, then asks for the upload as the panel does.
    fn upload(&mut self, bytes: &[u8]) -> (String, OperatorResponse) {
        let hash = hash_of(bytes);
        let dir = self.env.staging_dir();
        std::fs::create_dir_all(&dir).expect("staging");
        std::fs::write(dir.join(format!("{hash}.vrm")), bytes).expect("staged file");
        let response = self.act("avatar.upload", Some(&hash));
        (hash, response)
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
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn upload_confirm_select_delete_go_through_the_ingress_and_are_audited() {
    let mut s = Setup::full();
    let (hash, response) = s.upload(&model("Alice"));
    assert!(response.ok, "{response:?}");
    assert!(response.detail.contains("Alice"), "{response:?}");
    let status = s.rig.app.avatar_status().expect("composed");
    assert_eq!(
        status.pending.as_ref().map(|p| p.id.as_str()),
        Some(hash.as_str())
    );
    // The candidate carries the licence summary and its warnings for the operator.
    let pending = status.pending.unwrap();
    let meta = pending.meta.expect("meta");
    assert_eq!(meta.avatar_permission.as_deref(), Some("onlyAuthor"));
    assert!(
        pending
            .warnings
            .contains(&aivtuber_adapters::VrmWarning::AuthorOnly)
    );
    // Not active until confirmed.
    assert_eq!(s.rig.app.avatar_status().unwrap().active, "sample");
    let response = s.act("avatar.confirm", Some(&hash));
    assert!(response.ok, "{response:?}");
    assert!(
        response.detail.contains("credit: Alice / Tester"),
        "{response:?}"
    );
    assert_eq!(s.rig.app.avatar_status().unwrap().active, hash);
    let response = s.act("avatar.select", Some("sample"));
    assert!(response.ok, "{response:?}");
    let response = s.act("avatar.delete", Some(&hash[..16]));
    assert!(response.ok, "{response:?}");
    assert!(s.rig.app.avatar_status().unwrap().models.is_empty());
    // Every action left an accepted audit record with the name and the hash,
    // and never the file bytes.
    let audit = s.audit();
    for action in [
        "avatar.upload",
        "avatar.confirm",
        "avatar.select",
        "avatar.delete",
    ] {
        assert!(
            audit.iter().any(|(d, detail)| d == "operator_accepted"
                && detail.starts_with(&format!("action={action}"))),
            "{action}: {audit:?}"
        );
    }
    assert!(
        audit
            .iter()
            .any(|(_, detail)| detail.contains("Alice") && detail.contains(&hash)),
        "{audit:?}"
    );
}

#[test]
fn without_the_capability_nothing_changes() {
    let mut s = Setup::new(&[Capability::PerformerStop, Capability::PerformerMute]);
    let (hash, response) = s.upload(&model("Alice"));
    assert!(!response.ok, "{response:?}");
    for (action, argument) in [
        ("avatar.confirm", Some(hash.as_str())),
        ("avatar.select", Some("sample")),
        ("avatar.delete", Some(hash.as_str())),
        ("avatar.discard", None),
    ] {
        let response = s.act(action, argument);
        assert!(!response.ok, "{action}: {response:?}");
    }
    let status = s.rig.app.avatar_status().unwrap();
    assert!(status.pending.is_none() && status.models.is_empty());
    assert_eq!(status.active, "sample");
}

#[test]
fn a_wrong_secret_is_refused_and_audited() {
    let mut s = Setup::full();
    let response = apply_operator_action(
        &s.ingress,
        &mut s.rig.app,
        "bad-1",
        "avatar.select",
        Some("sample"),
        &[0_u8; 32],
        10,
    );
    assert!(
        !response.ok && response.detail == "authentication failed",
        "{response:?}"
    );
    assert!(
        s.audit()
            .iter()
            .any(|(d, detail)| d != "operator_accepted" && detail.contains("avatar.select")),
        "{:?}",
        s.audit()
    );
}

#[test]
fn the_operator_endpoint_has_everything_but_the_upload() {
    let mut s = Setup::full();
    let (hash, response) = s.upload(&model("Alice"));
    assert!(response.ok);
    let secret = String::from_utf8(SECRET.to_vec()).unwrap();
    let send = |s: &mut Setup, action: &str, argument: Option<&str>| {
        apply_operator_request(
            &s.ingress,
            &mut s.rig.app,
            &OperatorRequest {
                action: action.to_owned(),
                secret: secret.clone(),
                request_id: Some("tcp-1".to_owned()),
                argument: argument.map(str::to_owned),
            },
            10,
        )
    };
    // The upload names a file the panel staged; aivtuberctl cannot send it.
    let response = send(&mut s, "avatar.upload", Some(&hash));
    assert!(
        !response.ok && response.detail.contains("unknown action"),
        "{response:?}"
    );
    for (action, argument) in [
        ("avatar.confirm", Some(hash.as_str())),
        ("avatar.select", Some("sample")),
        ("avatar.discard", None),
    ] {
        let response = send(&mut s, action, argument);
        // Discard has nothing left to discard after the confirm.
        assert!(
            response.ok || action == "avatar.discard",
            "{action}: {response:?}"
        );
    }
    assert_eq!(s.rig.app.avatar_status().unwrap().models.len(), 1);
    let response = send(&mut s, "avatar.reconfigure", None);
    assert!(!response.ok);
}

#[test]
fn bad_arguments_are_refused_with_a_reason_and_change_nothing() {
    let mut s = Setup::full();
    for (action, argument) in [
        ("avatar.upload", None),
        ("avatar.upload", Some("../../etc/passwd")),
        ("avatar.upload", Some("C:\\x\\model.vrm")),
        ("avatar.upload", Some(&"ab".repeat(32)[..])),
        ("avatar.confirm", Some("abcdef01")),
        ("avatar.select", Some("../model.vrm")),
        ("avatar.select", Some("Sample")),
        ("avatar.select", None),
        ("avatar.delete", Some("sample")),
        ("avatar.delete", Some("abcdef01")),
        ("avatar.discard", None),
    ] {
        let response = s.act(action, argument);
        assert!(
            !response.ok && response.detail.starts_with("refused: "),
            "{action} {argument:?}: {response:?}"
        );
    }
    let status = s.rig.app.avatar_status().unwrap();
    assert!(status.pending.is_none() && status.models.is_empty());
    assert_eq!(status.active, "sample");
}

#[test]
fn a_model_that_is_not_a_vrm_cannot_become_a_candidate() {
    let mut s = Setup::full();
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(4096, 0);
    let (hash, response) = s.upload(&png);
    assert!(!response.ok, "{response:?}");
    assert!(response.detail.contains("not_glb"), "{response:?}");
    assert!(!s.env.staging_dir().join(format!("{hash}.vrm")).exists());
    assert!(s.rig.app.avatar_status().unwrap().pending.is_none());
}

#[test]
fn the_status_reaches_the_operator_snapshot_and_stays_inside_the_panel_bound() {
    let mut s = Setup::full();
    for i in 0..(MAX_STORED_MODELS + 1) {
        let (hash, response) = s.upload(&model(&format!("Model {i}")));
        assert!(response.ok, "{response:?}");
        if i < MAX_STORED_MODELS {
            assert!(s.act("avatar.confirm", Some(&hash)).ok);
        }
    }
    let snapshot = OperatorStatusSnapshot::from_app(&s.rig.app, 0);
    let avatar = snapshot.avatar_model.as_ref().expect("avatar status");
    assert_eq!(avatar.models.len(), MAX_STORED_MODELS);
    assert!(avatar.pending.is_some());
    // The wire form round-trips (aivtuberctl reads it back).
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(json.len() < 48 * 1024, "{}", json.len());
    let back: OperatorStatusSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back, snapshot);
}

#[test]
fn without_a_composed_store_the_actions_are_refused_and_the_status_is_absent() {
    let mut rig = local_rig_with_engines(
        MockThinking {
            reply: "こんにちは。".to_owned(),
            failure: None,
        },
        MockTts::default(),
        PhraseMode::Buffered,
    );
    assert!(!rig.app.avatar_configured());
    assert!(rig.app.avatar_status().is_none());
    assert!(rig.app.avatar_staging_dir().is_none());
    let response = apply_operator_action(
        &ingress(&[Capability::AvatarConfigure]),
        &mut rig.app,
        "none-1",
        "avatar.select",
        Some("sample"),
        &SECRET,
        10,
    );
    assert!(
        !response.ok && response.detail.contains("not configured"),
        "{response:?}"
    );
}

// ---- the whole path over HTTP: panel upload -> candidate -> confirm -> the
// avatar server serves the new model ----

mod over_http {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    struct Reply {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    /// Sends `request` on a worker thread while this thread plays the
    /// composition loop (`PanelService::service`), as `main` does.
    fn exchange(
        panel: &mut PanelService,
        app: &mut ProductionApp<LocalRoutePlanner>,
        port: u16,
        request: Vec<u8>,
    ) -> Reply {
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let _ = stream.write_all(&request);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
            let split = bytes
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(bytes.len(), |p| p + 4);
            let head = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
            Reply {
                status: head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                head,
                body: bytes[split..].to_vec(),
            }
        });
        let started = Instant::now();
        let mut seed = 0;
        let mut now_ms = 1_000;
        while !client.is_finished() && started.elapsed() < Duration::from_secs(10) {
            panel.service(app, now_ms, &mut seed);
            now_ms += 10;
            std::thread::sleep(Duration::from_millis(2));
        }
        client.join().expect("client")
    }

    fn panel_request(
        port: u16,
        token: &str,
        method: &str,
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> Vec<u8> {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\n\
             X-Panel-Token: {token}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    fn avatar_get(server: &LocalAvatarServer, path_and_query: &str) -> Reply {
        let addr = server.local_addr();
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream
            .write_all(format!("GET {path_and_query} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .expect("write");
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        let split = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(bytes.len(), |p| p + 4);
        let head = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
        Reply {
            status: head
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            head,
            body: bytes[split..].to_vec(),
        }
    }

    #[test]
    fn upload_confirm_and_the_avatar_page_is_served_the_new_model() {
        let mut s = Setup::full();
        let mut panel = PanelService::start(
            &PanelSettings {
                port: 0,
                page_dir: None,
                explicit: true,
            },
            Arc::new(ingress(&[
                Capability::PerformerStop,
                Capability::PerformerMute,
                Capability::AvatarConfigure,
            ])),
            &SECRET,
            None,
            None,
            s.rig.app.avatar_staging_dir(),
        )
        .expect("panel");
        let url = panel.launch_url();
        let (origin, token) = url.split_once("/#token=").expect("fragment url");
        let port: u16 = origin.rsplit(':').next().unwrap().parse().unwrap();
        let (token, avatar_token) = (token.to_owned(), s.server.token().to_owned());

        // Before: the page is served the sample.
        let sample_id = s.rig.app.avatar_status().unwrap().active_id.unwrap();
        let before = avatar_get(&s.server, &format!("/avatar/active?token={avatar_token}"));
        assert_eq!(before.status, 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&before.body).unwrap()["id"],
            sample_id.as_str()
        );

        // Upload over HTTP: a candidate with its licence, not yet active.
        let bytes = model("Alice");
        let hash = hash_of(&bytes);
        let reply = exchange(
            &mut panel,
            &mut s.rig.app,
            port,
            panel_request(
                port,
                &token,
                "POST",
                "/api/avatar/upload",
                "application/octet-stream",
                &bytes,
            ),
        );
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(String::from_utf8_lossy(&reply.body).contains("candidate ready"));
        let state = exchange(
            &mut panel,
            &mut s.rig.app,
            port,
            panel_request(port, &token, "GET", "/api/state", "application/json", b""),
        );
        let state: serde_json::Value = serde_json::from_slice(&state.body).expect("state json");
        let avatar = &state["status"]["avatar_model"];
        assert_eq!(avatar["active"], "sample");
        assert_eq!(avatar["pending"]["id"], hash.as_str());
        assert_eq!(avatar["pending"]["meta"]["avatar_permission"], "onlyAuthor");
        assert_eq!(avatar["pending"]["warnings"][0], "author_only");
        // Still the sample for the page.
        let still = avatar_get(&s.server, &format!("/avatar/active?token={avatar_token}"));
        assert!(String::from_utf8_lossy(&still.body).contains(&sample_id));

        // Confirm over HTTP: active, and the avatar server now serves it.
        let reply = exchange(
            &mut panel,
            &mut s.rig.app,
            port,
            panel_request(
                port,
                &token,
                "POST",
                "/api/avatar/confirm",
                "application/json",
                format!("{{\"id\":\"{hash}\"}}").as_bytes(),
            ),
        );
        assert_eq!(
            reply.status,
            200,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let now = avatar_get(&s.server, &format!("/avatar/active?token={avatar_token}"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&now.body).unwrap()["id"],
            hash.as_str()
        );
        let served = avatar_get(
            &s.server,
            &format!("/avatar/model.vrm?token={avatar_token}"),
        );
        assert_eq!(served.status, 200);
        assert_eq!(served.body, bytes, "the page is served the uploaded file");
        assert!(
            served.head.contains(&format!("x-avatar-model-id: {hash}")),
            "{}",
            served.head
        );
        assert!(served.head.contains("model/gltf-binary"));
        // The fallback asks for the sample but keeps the active id.
        let fallback = avatar_get(
            &s.server,
            &format!("/avatar/model.vrm?token={avatar_token}&fallback=sample"),
        );
        assert_ne!(fallback.body, bytes);
        assert!(
            fallback
                .head
                .contains(&format!("x-avatar-model-id: {hash}"))
        );
        // The model is only for a page that holds the avatar token.
        for path in [
            "/avatar/model.vrm".to_owned(),
            "/avatar/model.vrm?token=wrong".to_owned(),
            format!("/avatar/model.vrm?token={}", "0".repeat(avatar_token.len())),
            "/avatar/active".to_owned(),
            "/avatar/active?token=".to_owned(),
        ] {
            assert_eq!(avatar_get(&s.server, &path).status, 403, "{path}");
        }

        // A file that is not a VRM is refused over HTTP and changes nothing.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.resize(4096, 0);
        let reply = exchange(
            &mut panel,
            &mut s.rig.app,
            port,
            panel_request(
                port,
                &token,
                "POST",
                "/api/avatar/upload",
                "application/octet-stream",
                &png,
            ),
        );
        assert_eq!(
            reply.status,
            415,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(String::from_utf8_lossy(&reply.body).contains("not_glb"));
        assert_eq!(s.rig.app.avatar_status().unwrap().active, hash);
        assert!(s.rig.app.avatar_status().unwrap().pending.is_none());
    }
}
