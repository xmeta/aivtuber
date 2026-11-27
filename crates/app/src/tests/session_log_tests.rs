//! App-level tests of the opt-in session conversation log (issue #463): off by
//! default and creating nothing, one pseudonym per viewer and no raw id in a
//! file, the repeat verdicts and routes it records, the operator-only switch
//! and delete-all, and its absence from the diagnose bundle.

use super::*;
use crate::initiative::InitiativeConfig;
use crate::session_log::{Record, SessionLog, render_summary, summarize};

const SECRET: [u8; 32] = [0x5A; 32];
/// The raw actor id of `chat_event`; it must never reach a file.
const RAW_ACTOR: &str = "viewer:test";
const MARKER: &str = "sentinel-chat-text-463";

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aivtuber-session-log-app-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn app_with_log(dir: &std::path::Path, reply: &str) -> ProductionApp<FixedGenerateRoute> {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: reply.to_owned(),
            failure: None,
        },
        MockTts::default(),
        SecretRedactor::new(["sk-live-secret"]),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_session_log(SessionLog::from_settings(
        dir.join("logs"),
        dir.join("settings.json"),
    ));
    app.startup().expect("startup");
    app
}

fn operator(capability: Capability, action: &str) -> AuthenticatedControlCommand {
    LocalControlIngress::new(
        "local-test",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([capability]),
        ControlSecret::new(SECRET),
    )
    .expect("control ingress")
    .authenticate(
        OperatorCommandInput {
            event_id: format!("evt-{action}"),
            correlation_id: "corr-session-log".to_owned(),
            sequence: 97,
            observed_at: "2026-11-22T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        },
        &SECRET,
    )
    .expect("authenticated command")
}

fn switch(app: &mut ProductionApp<FixedGenerateRoute>, action: &str) -> Result<String, String> {
    let control = operator(Capability::SessionLogControl, action);
    app.configure_session_log(control.authority(), action)
}

fn chat(app: &mut ProductionApp<FixedGenerateRoute>, id: u64, text: &str, at_ms: u64) {
    let mut event = chat_event(id);
    event.payload.remove("intent");
    event
        .payload
        .insert("text".to_owned(), serde_json::json!(text));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, at_ms, id).expect("chat");
    wait_for_generation_completion(app, at_ms);
    app.tick(at_ms + 10_000);
}

fn session_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir.join("logs"))
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    files
}

fn records(dir: &std::path::Path) -> Vec<Record> {
    let files = session_files(dir);
    std::fs::read_to_string(files.last().expect("a session file"))
        .expect("readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("one record per line"))
        .collect()
}

#[test]
fn nothing_is_composed_by_default() {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: "hi".to_owned(),
            failure: None,
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    chat(&mut app, 1, "hello", 1_000);
    assert!(!app.session_log_composed());
    assert!(app.session_log_status().is_none());
    assert!(
        switch(&mut app, "session_log.enable")
            .unwrap_err()
            .contains("not configured")
    );
}

#[test]
fn off_by_default_a_composed_log_records_and_creates_nothing() {
    let dir = temp_dir("off");
    let mut app = app_with_log(&dir, "こんにちは！");
    assert!(!app.session_log_status().expect("composed").enabled);
    chat(&mut app, 1, MARKER, 1_000);
    chat(&mut app, 2, MARKER, 30_000);
    app.session_log().expect("log").flush();
    assert!(!dir.join("logs").exists(), "no directory while off");
    assert!(!dir.join("settings.json").exists());
    let status = app.session_log_status().expect("status");
    assert_eq!((status.lines_written, status.files_kept), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_turn_is_logged_with_a_pseudonym_the_reply_the_route_and_the_repeat_verdict() {
    let dir = temp_dir("turn");
    // The reply greets the viewer by the raw id; the file must not.
    let mut app = app_with_log(&dir, &format!("こんにちは、{RAW_ACTOR} さん！"));
    assert_eq!(
        switch(&mut app, "session_log.enable"),
        Ok("session log on".to_owned())
    );
    chat(
        &mut app,
        1,
        &format!("{RAW_ACTOR} です sk-live-secret"),
        1_000,
    );
    chat(&mut app, 2, "また来たよ", 30_000);
    app.session_log().expect("log").flush();

    let file = std::fs::read_to_string(session_files(&dir).last().expect("file")).expect("read");
    assert!(!file.contains(RAW_ACTOR), "{file}");
    assert!(!file.contains("sk-live-secret"), "{file}");
    assert!(!file.contains("evt-1"), "raw event ids are not written");
    let records = records(&dir);
    assert_eq!(records[0].t, "session");
    let inputs: Vec<_> = records.iter().filter(|r| r.t == "input").collect();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].viewer.as_deref(), Some("viewer-01"));
    assert_eq!(inputs[0].origin.as_deref(), Some("chat"));
    assert_eq!(inputs[0].text.as_deref(), Some("viewer-01 です [REDACTED]"));
    assert_eq!(inputs[1].viewer.as_deref(), Some("viewer-01"));
    let phrases: Vec<_> = records.iter().filter(|r| r.t == "phrase").collect();
    assert!(phrases.len() >= 2, "{phrases:?}");
    assert!(
        phrases
            .iter()
            .all(|p| p.text.as_deref().is_some_and(|t| t.contains("viewer-01")))
    );
    assert!(
        phrases
            .iter()
            .all(|p| p.route.as_deref() == Some("generated"))
    );
    assert_eq!(phrases[0].repeat.as_deref(), Some("none"));
    assert!(
        phrases.iter().any(|p| p.repeat.as_deref() == Some("exact")),
        "the same reply twice is an exact repeat: {phrases:?}"
    );
    assert!(records.iter().any(|r| r.t == "route"));
    assert!(records.iter().any(|r| r.t == "config"));
    // Ordering keys: strictly increasing `seq`, monotonic `logical_ms`.
    let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");

    // The offline reader understands the file.
    let summary = summarize(file.as_bytes());
    assert_eq!(summary.malformed, 0);
    assert_eq!(summary.turns, vec![("chat".to_owned(), 2)]);
    assert!(summary.repeats.iter().any(|r| r.exact >= 1));
    assert_eq!(summary.first_audio_ms.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_switch_needs_the_capability_is_remembered_and_delete_all_removes_the_files() {
    let dir = temp_dir("switch");
    let mut app = app_with_log(&dir, "こんにちは！");
    // Without the capability nothing changes.
    let weak = operator(Capability::PerformerStop, "performer.stop");
    assert!(
        app.configure_session_log(weak.authority(), "session_log.enable")
            .is_err()
    );
    assert!(!app.session_log_status().expect("status").enabled);

    assert!(switch(&mut app, "session_log.enable").is_ok());
    assert!(crate::session_log::read_enabled(&dir.join("settings.json")));
    chat(&mut app, 1, "hello", 1_000);
    app.session_log().expect("log").flush();
    assert_eq!(session_files(&dir).len(), 1);
    assert_eq!(app.session_log_status().expect("status").files_kept, 1);

    assert_eq!(
        switch(&mut app, "session_log.disable"),
        Ok("session log off".to_owned())
    );
    assert!(!crate::session_log::read_enabled(
        &dir.join("settings.json")
    ));
    let lines_before = app.session_log_status().expect("status").lines_written;
    chat(&mut app, 2, "after off", 40_000);
    app.session_log().expect("log").flush();
    assert_eq!(
        app.session_log_status().expect("status").lines_written,
        lines_before,
        "nothing is written once it is off"
    );

    assert_eq!(
        switch(&mut app, "session_log.delete"),
        Ok("session logs deleted".to_owned())
    );
    app.session_log().expect("log").flush();
    assert!(session_files(&dir).is_empty());
    assert_eq!(app.session_log_status().expect("status").files_kept, 0);
    // An unknown action is refused even with the capability.
    let control = operator(Capability::SessionLogControl, "session_log.enable");
    assert!(
        app.configure_session_log(control.authority(), "session_log.say")
            .is_err()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_log_is_not_in_the_diagnose_bundle_or_the_audit() {
    let dir = temp_dir("bundle");
    let mut app = app_with_log(&dir, &format!("{MARKER} reply"));
    switch(&mut app, "session_log.enable").expect("on");
    chat(&mut app, 1, MARKER, 1_000);
    app.session_log().expect("log").flush();
    assert!(
        std::fs::read_to_string(session_files(&dir).last().expect("file"))
            .expect("read")
            .contains(MARKER),
        "the operator chose to keep the text in the file"
    );

    let provenance = SupportBundleProvenance::from_build_env();
    let bundle = SupportBundleBuilder::new("local", "fp-test", provenance).generate(&app, 2_000);
    let json = serde_json::to_string(&bundle).expect("json");
    assert!(!json.contains(MARKER), "{json}");
    assert!(!json.contains("viewer-01"));
    assert!(!json.contains(&dir.display().to_string()), "no location");
    assert!(!json.contains(RAW_ACTOR));
    // Counters are there so a failing writer shows up in a bundle.
    let counters = bundle.resources.session_log.as_ref().expect("counters");
    assert!(counters.enabled && counters.lines_written > 0);
    assert!(counters.location.is_none());
    for record in app.security().audit().iter() {
        assert!(!record.detail.contains(MARKER), "{}", record.detail);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_initiative_is_logged_as_its_own_origin_and_a_refused_repeat_is_noted() {
    let dir = temp_dir("initiative");
    let mut app =
        app_with_log(&dir, "最近どう過ごしてる？").with_initiative(InitiativeConfig::default());
    switch(&mut app, "session_log.enable").expect("on");
    let step = |app: &mut ProductionApp<FixedGenerateRoute>, at_ms: u64| {
        app.service_initiative(at_ms, at_ms);
        if app
            .generation_cancellation_registry()
            .is_some_and(|registry| registry.active_event_id().is_some())
        {
            wait_for_generation_completion(app, at_ms);
        }
    };
    step(&mut app, 45_000);
    app.tick(50_000);
    app.service_initiative(50_000, 1);
    // The same words again are refused before commit.
    step(&mut app, 115_000);
    app.tick(120_000);
    app.service_initiative(120_000, 2);
    app.session_log().expect("log").flush();

    let records = records(&dir);
    let inputs: Vec<_> = records.iter().filter(|r| r.t == "input").collect();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().all(|r| {
        r.origin.as_deref() == Some("initiative") && r.viewer.is_none() && r.text.is_none()
    }));
    assert_eq!(inputs[0].kind.as_deref(), Some("idle_observation"));
    let phrases: Vec<_> = records.iter().filter(|r| r.t == "phrase").collect();
    assert_eq!(phrases.len(), 1, "the repeat was never spoken");
    assert_eq!(phrases[0].origin.as_deref(), Some("initiative"));
    let note = records
        .iter()
        .find(|r| r.t == "note")
        .expect("the refusal is noted");
    assert_eq!(note.kind.as_deref(), Some("initiative_repeat_suppressed"));
    assert_eq!(note.fallback.as_deref(), Some("exact"));
    // #547: the matched phrase is the earlier initiative reply the director
    // showed the model; only its origin, age and flag are logged.
    assert_eq!(note.matched_origin.as_deref(), Some("initiative"));
    assert!(
        note.matched_age_ms
            .is_some_and(|age| age > 0 && age < 900_000)
    );
    assert_eq!(note.matched_shown, Some(true));
    let raw = std::fs::read_to_string(session_files(&dir).last().expect("file")).expect("read");
    let note_line = raw
        .lines()
        .find(|line| line.contains("initiative_repeat_suppressed"))
        .expect("note line");
    assert!(!note_line.contains("\"text\""));
    let file = std::fs::read(session_files(&dir).last().expect("file")).expect("read");
    let summary = summarize(file.as_slice());
    assert_eq!(summary.initiative_spacing_ms, vec![70_000]);
    assert_eq!(summary.repeat_refusals, 1);
    assert_eq!(
        summary.repeat_refusals_by_origin,
        vec![("initiative".to_owned(), 1)]
    );
    assert_eq!(
        summary.repeat_refusals_by_shown,
        vec![("true".to_owned(), 1)]
    );
    let rendered = render_summary(&summary);
    assert!(rendered.contains("repeat_refused matched_origin=initiative count=1"));
    assert!(rendered.contains("repeat_refused matched_shown=true count=1"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_operator_endpoint_that_composed_the_log_holds_the_capability() {
    let dir = temp_dir("endpoint");
    let secret = String::from_utf8(SECRET.to_vec()).expect("ascii secret");
    let run = |options: OperatorEndpointOptions, action: &str| {
        let mut app = app_with_log(&dir, "hi");
        let (server, _receiver) =
            OperatorControlServer::with_options("aivtuber-session-log-test", SECRET, options)
                .expect("server");
        let (respond, _rx) = tokio::sync::oneshot::channel();
        let dispatched = DispatchedOperatorRequest {
            request: OperatorRequest {
                action: action.to_owned(),
                secret: secret.clone(),
                request_id: Some("log-1".to_owned()),
                argument: None,
            },
            respond,
        };
        apply_dispatched_request(&server.ingress(), &mut app, &dispatched, 20)
    };
    let composed = OperatorEndpointOptions {
        session_log_control: true,
        ..OperatorEndpointOptions::default()
    };
    for action in [
        "session_log.enable",
        "session_log.disable",
        "session_log.delete",
    ] {
        assert!(run(composed, action).ok, "{action}");
        let none = run(OperatorEndpointOptions::default(), action);
        assert!(!none.ok, "{action}");
        assert_eq!(none.detail, "authentication failed");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_turn_answered_by_a_fallback_gets_a_fallback_record_and_counts_once() {
    let dir = temp_dir("fallback");
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: String::new(),
            failure: Some(EngineErrorKind::Unavailable),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_session_log(SessionLog::from_settings(
        dir.join("logs"),
        dir.join("settings.json"),
    ));
    app.startup().expect("startup");
    switch(&mut app, "session_log.enable").expect("on");
    chat(&mut app, 1, MARKER, 1_000);
    app.session_log().expect("log").flush();

    let records = records(&dir);
    let fallbacks: Vec<_> = records.iter().filter(|r| r.t == "fallback").collect();
    assert_eq!(fallbacks.len(), 1, "{records:?}");
    assert_eq!(fallbacks[0].fallback.as_deref(), Some("unavailable"));
    assert_eq!(fallbacks[0].kind.as_deref(), Some("visible"));
    assert!(fallbacks[0].text.is_none(), "never viewer text");
    let input = records.iter().find(|r| r.t == "input").expect("input");
    assert_eq!(fallbacks[0].turn, input.turn, "same turn as the input");
    assert!(records.iter().all(|r| r.t != "phrase"));

    // The route and fallback records name one turn: it counts once.
    let text = std::fs::read_to_string(session_files(&dir).last().expect("file")).expect("read");
    assert_eq!(summarize(text.as_bytes()).fallbacks, 1);
    let _ = std::fs::remove_dir_all(&dir);
}
