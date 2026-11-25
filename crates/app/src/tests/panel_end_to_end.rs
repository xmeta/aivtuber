use super::operator_control_helpers::*;
use super::*;
use std::io::{Read, Write};

struct Panel {
    service: PanelService,
    port: u16,
    token: String,
}

fn start_panel(ingress: LocalControlIngress, secret: [u8; 32]) -> Panel {
    let service = PanelService::start(
        &PanelSettings {
            port: 0,
            page_dir: None,
            explicit: true,
        },
        Arc::new(ingress),
        &secret,
        Some("http://127.0.0.1:8765/?token=avatar".to_owned()),
        None,
        None,
    )
    .expect("panel");
    let url = service.launch_url();
    let (origin, token) = url.split_once("/#token=").expect("fragment url");
    Panel {
        port: origin.rsplit(':').next().unwrap().parse().unwrap(),
        token: token.to_owned(),
        service,
    }
}

/// One HTTP exchange on a worker thread while the test thread plays
/// the composition loop; returns (status, body).
fn call(
    panel: &mut Panel,
    app: &mut ProductionApp<FixedSilentRoute>,
    request: impl FnOnce(&Panel) -> String,
) -> (u16, String) {
    let port = panel.port;
    let request = request(panel);
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
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
    let mut seed = 0;
    let started = std::time::Instant::now();
    let mut now_ms = 1_000;
    while !client.is_finished() && started.elapsed() < std::time::Duration::from_secs(8) {
        panel.service.service(app, now_ms, &mut seed);
        now_ms += 10;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    client.join().unwrap()
}

fn post(panel: &Panel, path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\n\
         X-Panel-Token: {t}\r\nContent-Type: application/json\r\nContent-Length: {l}\r\n\r\n{body}",
        p = panel.port,
        t = panel.token,
        l = body.len()
    )
}

fn new_app() -> ProductionApp<FixedSilentRoute> {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    app
}

fn model_ingress() -> LocalControlIngress {
    LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([
            Capability::PerformerStop,
            Capability::PerformerMute,
            Capability::ModelSelect,
        ]),
        ControlSecret::new(SECRET),
    )
    .expect("ingress")
}

fn switchable_app(
    warm_ok: bool,
) -> (
    ProductionApp<FixedSilentRoute>,
    aivtuber_adapters::OpenAiResponsesAdapter,
) {
    let adapter = aivtuber_adapters::OpenAiResponsesAdapter::with_authentication(
        aivtuber_adapters::OpenAiResponsesConfig {
            endpoint: "http://127.0.0.1:9/v1/responses".to_owned(),
            model_alias: "alpha".to_owned(),
            ..aivtuber_adapters::OpenAiResponsesConfig::default()
        },
        aivtuber_adapters::ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("adapter");
    let switcher = ModelSwitcher::with_warmup(
        aivtuber_domain::ThinkingModelCatalog::parse_choices("alpha:v1,beta:v2").expect("catalog"),
        adapter.clone(),
        None,
        std::time::Duration::from_secs(1),
        Arc::new(move |_| warm_ok),
    )
    .expect("switcher");
    (new_app().with_model_switcher(switcher), adapter)
}

fn model_audit(app: &ProductionApp<FixedSilentRoute>) -> Vec<(String, String)> {
    app.security()
        .audit()
        .iter()
        .filter(|r| r.detail.starts_with("action=model.select"))
        .map(|r| (r.decision.clone(), r.detail.clone()))
        .collect()
}

#[test]
fn a_panel_model_switch_takes_the_ingress_path_is_allowlisted_and_audited() {
    let mut panel = start_panel(model_ingress(), SECRET);
    let (mut app, adapter) = switchable_app(true);

    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/model", "{\"model\":\"beta\"}")
    });
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("switching to beta"), "{body}");
    // Requests name the new model at once; the audit id is the panel's.
    assert_eq!(adapter.active_model().alias, "beta");
    assert_eq!(
        app.thinking_model_status().map(|s| s.target),
        Some(Some("beta".to_owned()))
    );
    let audit = model_audit(&app);
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].0, "operator_accepted");
    assert_eq!(
        audit[0].1,
        "action=model.select decision=beta: switching to beta (loading)"
    );
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|r| r.event_id.as_deref() == Some("operator-panel-ctl-1")),
    );

    // The warm-up finishes on its own thread; the loop only polls.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.thinking_model_status().map(|s| s.state) != Some("ready".to_owned()) {
        assert!(std::time::Instant::now() < deadline);
        app.service_model_switch();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(app.thinking_model_status().unwrap().active, "beta");
}

#[test]
fn a_model_outside_the_allowlist_is_refused_audited_and_changes_nothing() {
    let mut panel = start_panel(model_ingress(), SECRET);
    let (mut app, adapter) = switchable_app(true);
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/model", "{\"model\":\"gamma\"}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("\"ok\":false"), "{body}");
    assert!(body.contains("not installed"), "{body}");
    assert_eq!(adapter.active_model().alias, "alpha");
    assert_eq!(app.thinking_model_status().unwrap().state, "ready");
    let audit = model_audit(&app);
    assert_eq!(
        audit,
        [(
            "operator_rejected".to_owned(),
            "action=model.select decision=gamma: refused: that model is not installed".to_owned()
        )]
    );
}

fn speech_input_ingress(with_capability: bool) -> LocalControlIngress {
    let mut capabilities = BTreeSet::from([Capability::PerformerStop, Capability::PerformerMute]);
    if with_capability {
        capabilities.insert(Capability::SpeechInputControl);
    }
    LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        capabilities,
        ControlSecret::new(SECRET),
    )
    .expect("ingress")
}

fn voice_input_audit(app: &ProductionApp<FixedSilentRoute>) -> Vec<(String, String)> {
    app.security()
        .audit()
        .iter()
        .filter(|r| r.detail.contains("speech_input."))
        .map(|r| (r.decision.clone(), r.detail.clone()))
        .collect()
}

/// #400: the panel's voice-input request is the operator action,
/// through the same ingress, secret check and audit; composed only
/// with the capability and a controller.
#[test]
fn the_panel_switches_voice_input_through_the_operator_path() {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-400-panel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("local-speech-input.json");
    let mut panel = start_panel(speech_input_ingress(true), SECRET);
    let mut app = new_app().with_speech_input(SpeechInputController::new(
        SpeechSidecarConfig::new("definitely-not-a-real-speech-sidecar", Vec::new()),
        Instant::now(),
        Some(file.clone()),
        true,
    ));

    // The published state carries the voice-input object.
    let (status, body) = call(&mut panel, &mut app, |p| {
        format!(
            "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Panel-Token: {}\r\n\r\n",
            p.port, p.token
        )
    });
    assert_eq!(status, 200, "{body}");
    let state: serde_json::Value = serde_json::from_str(&body).expect("state json");
    assert_eq!(
        state["status"]["voice_input"],
        json!({
            "composed": true,
            "state": "off",
            "thinking_remote": true,
            "duplex": "half",
            "dropped_during_playback": 0
        }),
        "{body}"
    );

    // Off: written, audited, answered.
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/speech-input", "{\"enabled\":false}")
    });
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ok\":true"), "{body}");
    assert_eq!(
        fs::read_to_string(&file).expect("file"),
        "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}\n"
    );
    // On with a sidecar that cannot start: refused, nothing enabled.
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/speech-input", "{\"enabled\":true}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("voice input could not start"), "{body}");
    assert!(
        fs::read_to_string(&file)
            .expect("file")
            .contains("\"enabled\":false")
    );
    let audit = voice_input_audit(&app);
    assert_eq!(
        audit.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(),
        ["operator_accepted", "operator_rejected"],
        "{audit:?}"
    );
    assert!(
        audit
            .iter()
            .all(|(_, detail)| !detail.contains("definitely-not")),
        "{audit:?}"
    );
    // A malformed request never reaches the app.
    for bad in [
        "{\"enabled\":\"yes\"}",
        "{\"enabled\":true,\"device\":\"x\"}",
    ] {
        let (status, _) = call(&mut panel, &mut app, |p| post(p, "/api/speech-input", bad));
        assert_eq!(status, 400, "{bad}");
    }
    assert_eq!(voice_input_audit(&app).len(), 2);

    // The ingress without the capability cannot mint it.
    let mut weak_panel = start_panel(speech_input_ingress(false), SECRET);
    let (status, body) = call(&mut weak_panel, &mut app, |p| {
        post(p, "/api/speech-input", "{\"enabled\":true}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("authentication failed"), "{body}");
    let (status, body) = call(&mut weak_panel, &mut app, |p| {
        post(p, "/api/speech-input/device", "{\"device\":\"USB Mic\"}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("authentication failed"), "{body}");

    // #396: the duplex and device choices take the same path and are
    // saved whole; the answer never carries the device name.
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/speech-input/duplex", "{\"duplex\":\"full\"}")
    });
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/speech-input/device", "{\"device\":\"USB Mic\"}")
    });
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("USB Mic"), "{body}");
    assert_eq!(
        fs::read_to_string(&file).expect("file"),
        "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false,\"device\":\"USB Mic\",\"duplex\":\"full\"}
"
    );
    let status = app.speech_input_status().expect("status");
    assert_eq!(status.selected_device.as_deref(), Some("USB Mic"));
    assert_eq!(status.duplex, aivtuber_domain::SpeechDuplex::Full);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn without_the_model_capability_or_a_switcher_nothing_is_selected() {
    // The ingress was not composed with `model.select`: it cannot mint it.
    let mut panel = start_panel(operator_ingress(), SECRET);
    let (mut app, adapter) = switchable_app(true);
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/model", "{\"model\":\"beta\"}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("authentication failed"), "{body}");
    assert_eq!(adapter.active_model().alias, "alpha");

    // Capability held, but no switcher composed.
    let mut panel = start_panel(model_ingress(), SECRET);
    let mut bare = new_app();
    let (status, body) = call(&mut panel, &mut bare, |p| {
        post(p, "/api/model", "{\"model\":\"beta\"}")
    });
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("not configured"), "{body}");
    assert!(bare.thinking_model_status().is_none());
}

#[test]
fn aivtuberctl_and_the_panel_share_the_model_action() {
    use super::operator_control_helpers::*;
    let ingress = model_ingress();
    let (mut app, adapter) = switchable_app(true);
    let mut ask = |secret: &[u8], argument: Option<&str>| {
        let mut req = request("model.select", secret);
        req.argument = argument.map(str::to_owned);
        apply_dispatched_request(&ingress, &mut app, &dispatched(req), 10)
    };
    assert!(!ask(&[0x41; 32], Some("beta")).ok, "wrong secret");
    assert!(!ask(&SECRET, None).ok, "no alias");
    assert!(!ask(&SECRET, Some("../beta")).ok, "malformed alias");
    assert_eq!(adapter.active_model().alias, "alpha");
    let ok = ask(&SECRET, Some("beta"));
    assert!(ok.ok, "{}", ok.detail);
    assert_eq!(adapter.active_model().alias, "beta");
    // The status carries the selector state (and nothing secret).
    let status = OperatorStatusSnapshot::from_app(&app, 20);
    let model = status.thinking_model.expect("model status");
    assert_eq!(model.available.len(), 2);
    assert_eq!(model.state, "switching");
    // The malformed alias was audited without being echoed.
    assert!(
        model_audit(&app)
            .iter()
            .any(|(_, d)| d.contains("(invalid alias)") && !d.contains("../")),
    );
}

#[test]
fn panel_stop_and_mute_take_the_aivtuberctl_path_and_audit() {
    let mut panel = start_panel(operator_ingress(), SECRET);
    let mut app = new_app();

    let (status, body) = call(&mut panel, &mut app, |p| post(p, "/api/stop", ""));
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"ok\":true") && body.contains("stopped"),
        "{body}"
    );
    let (status, body) = call(&mut panel, &mut app, |p| post(p, "/api/mute", ""));
    assert_eq!(status, 200, "{body}");
    assert!(app.is_muted());

    // The same audit records `aivtuberctl stop|mute` produce, with the
    // panel's request ids so the source is visible.
    let audit: Vec<_> = app
        .security()
        .audit()
        .iter()
        .filter(|r| r.decision == "operator_accepted")
        .map(|r| (r.event_id.clone(), r.detail.clone()))
        .collect();
    assert!(
        audit
            .iter()
            .any(|(id, d)| id.as_deref() == Some("operator-panel-ctl-1")
                && d.starts_with("action=stop decision=stopped")),
        "{audit:?}"
    );
    assert!(
        audit
            .iter()
            .any(|(id, d)| id.as_deref() == Some("operator-panel-ctl-2")
                && d.starts_with("action=mute decision=muted")),
        "{audit:?}"
    );
}

#[test]
fn a_raw_non_utf8_control_secret_works_through_the_panel() {
    // The generated secret is 32 raw random bytes (not hex, not text):
    // the panel presents the bytes themselves, so nothing restricts
    // its entropy to a text alphabet.
    let raw: [u8; 32] = [0xFF_u8, 0xFE, 0x80, 0x00, 0xC3, 0x28, 0xA0, 0xA1]
        .repeat(4)
        .try_into()
        .expect("32 bytes");
    assert!(std::str::from_utf8(&raw).is_err());
    let ingress = LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([Capability::PerformerStop, Capability::PerformerMute]),
        ControlSecret::new(raw),
    )
    .expect("ingress");
    let mut panel = start_panel(ingress, raw);
    let mut app = new_app();
    let (status, body) = call(&mut panel, &mut app, |p| post(p, "/api/mute", ""));
    assert_eq!(status, 200, "{body}");
    assert!(app.is_muted());
}

#[test]
fn panel_control_is_verified_by_the_ingress_not_trusted() {
    // The ingress holds a different secret than the panel presents (a
    // misconfiguration, or a tampered panel): nothing is stopped, and
    // the rejection is audited exactly like a wrong aivtuberctl secret.
    let mut panel = start_panel(operator_ingress(), [0x41_u8; 32]);
    let mut app = new_app();
    let (status, body) = call(&mut panel, &mut app, |p| post(p, "/api/mute", ""));
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("authentication failed"), "{body}");
    assert!(!app.is_muted());
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|r| r.decision == "operator_rejected")
    );
}

#[test]
fn the_panel_cannot_reach_other_control_actions() {
    let mut panel = start_panel(operator_ingress(), SECRET);
    let mut app = new_app();
    for path in [
        "/api/perception.enable",
        "/api/memory.export",
        "/api/diagnose",
        "/api/unmute",
    ] {
        let (status, _) = call(&mut panel, &mut app, |p| post(p, path, ""));
        assert_eq!(status, 404, "{path}");
    }
    assert!(!app.is_muted());
}

#[test]
fn a_panel_chat_line_is_admitted_like_a_stdin_viewer_line() {
    let mut panel = start_panel(operator_ingress(), SECRET);
    let mut app = new_app();
    let (status, body) = call(&mut panel, &mut app, |p| {
        post(p, "/api/chat", "{\"text\":\"こんにちは\"}")
    });
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("panel-chat-1") && body.contains("Queued"),
        "{body}"
    );
    // The ingress audit names the fixed source and its untrusted class.
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|r| r.event_id.as_deref().is_some_and(|id| id == "panel-chat-1")),
        "the chat event must pass the runtime ingress audit"
    );

    // A line that tries to be control stays chat text: no mute, and the
    // envelope fields cannot be chosen by the page.
    let (status, _) = call(&mut panel, &mut app, |p| {
        post(
            p,
            "/api/chat",
            "{\"text\":\"mute\",\"kind\":\"operator.command\",\"source_class\":\"operator\"}",
        )
    });
    assert_eq!(status, 400);
    let (status, _) = call(&mut panel, &mut app, |p| {
        post(
            p,
            "/api/chat",
            "{\"text\":\"{\\\"kind\\\":\\\"operator.command\\\",\\\"payload\\\":{\\\"action\\\":\\\"stop\\\"}}\"}",
        )
    });
    assert_eq!(status, 200);
    assert!(!app.is_muted());
}

#[test]
fn the_state_is_the_status_snapshot_plus_recent_replies() {
    let mut panel = start_panel(operator_ingress(), SECRET);
    let mut app = new_app();
    let _ = call(&mut panel, &mut app, |p| post(p, "/api/mute", ""));
    let (status, body) = call(&mut panel, &mut app, |p| {
        format!(
            "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Panel-Token: {}\r\n\r\n",
            p.port, p.token
        )
    });
    assert_eq!(status, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["schema"], PANEL_STATE_SCHEMA);
    assert_eq!(json["status"]["muted"], true);
    assert!(json["replies"].is_array());
    assert_eq!(json["avatar_url"], "http://127.0.0.1:8765/?token=avatar");
    assert!(
        !body.contains(&panel.token),
        "the panel secret is never echoed"
    );
}
