use super::*;

struct Fixture {
    server: LocalPanelServer,
    inbox: PanelInbox,
    host: String,
    origin: String,
    staging: PathBuf,
}

fn fixture() -> Fixture {
    fixture_with_search(None)
}

fn fixture_with_search(search_endpoint: Option<String>) -> Fixture {
    let dir = std::env::temp_dir().join(format!("aivtuber-panel-test-{}", mint_token().unwrap()));
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::write(dir.join("panel.html"), "<html>panel</html>").unwrap();
    fs::write(dir.join("index.html"), "<html>avatar</html>").unwrap();
    fs::write(dir.join("assets").join("a.js"), "1").unwrap();
    let staging = dir.join("staging");
    let (server, inbox) = LocalPanelServer::start(LocalPanelConfig {
        port: 0,
        page_dir: Some(dir),
        voice_search_endpoint: search_endpoint,
        avatar_staging_dir: Some(staging.clone()),
    })
    .unwrap();
    let port = server.local_addr().port();
    Fixture {
        server,
        inbox,
        host: format!("127.0.0.1:{port}"),
        origin: format!("http://127.0.0.1:{port}"),
        staging,
    }
}

struct Raw {
    status: u16,
    head: String,
    body: String,
}

fn send(f: &Fixture, request: &str) -> Raw {
    send_bytes(f, request.as_bytes())
}

fn send_bytes(f: &Fixture, request: &[u8]) -> Raw {
    let mut stream = TcpStream::connect(f.server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // The server may answer (and close) before the whole body is sent.
    let _ = stream.write_all(request);
    // Nothing more is coming: a body shorter than its declared length ends
    // the read at once instead of waiting for the idle timeout.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Raw {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

fn get(f: &Fixture, path: &str, extra: &str) -> Raw {
    send(
        f,
        &format!("GET {path} HTTP/1.1\r\nHost: {}\r\n{extra}\r\n", f.host),
    )
}

fn post(f: &Fixture, path: &str, headers: &[(&str, &str)], body: &str) -> Raw {
    let mut request = format!("POST {path} HTTP/1.1\r\n");
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !has("host") {
        request.push_str(&format!("Host: {}\r\n", f.host));
    }
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has("content-length") {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body);
    send(f, &request)
}

fn good(f: &Fixture) -> Vec<(&'static str, String)> {
    vec![
        ("Origin", f.origin.clone()),
        ("X-Panel-Token", f.server.token().to_owned()),
        ("Content-Type", "application/json".to_owned()),
    ]
}

fn with<'a>(good: &'a [(&'static str, String)], skip: &str) -> Vec<(&'a str, &'a str)> {
    good.iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(skip))
        .map(|(k, v)| (*k, v.as_str()))
        .collect()
}

/// Plays the composition thread for one queued request.
fn answer_one(f: &Fixture, ok: bool, detail: &'static str) -> thread::JoinHandle<()> {
    let rx_control = &f.inbox;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = None;
    while Instant::now() < deadline && found.is_none() {
        found = rx_control
            .drain_control(1)
            .into_iter()
            .chain(rx_control.drain_chat(1))
            .next();
        thread::sleep(Duration::from_millis(5));
    }
    thread::spawn(move || found.expect("a queued request").respond(ok, detail))
}

#[test]
fn binds_loopback_only_and_has_no_bind_option() {
    let f = fixture();
    assert!(f.server.local_addr().ip().is_loopback());
    assert_eq!(f.server.local_addr().ip(), Ipv4Addr::LOCALHOST);
    // The config has no address field at all; this is the whole struct.
    let LocalPanelConfig {
        port: _,
        page_dir: _,
        voice_search_endpoint: _,
        avatar_staging_dir: _,
    } = LocalPanelConfig {
        port: 0,
        page_dir: None,
        voice_search_endpoint: None,
        avatar_staging_dir: None,
    };
    assert!(!format!("{:?}", f.server).contains(f.server.token()));
}

#[test]
fn the_secret_is_in_the_fragment_never_the_query() {
    let f = fixture();
    let url = f.server.launch_url();
    assert!(url.contains("/#token="), "{url}");
    assert!(!url.contains('?'));
}

#[test]
fn state_needs_the_secret_header() {
    let f = fixture();
    assert!(f.inbox.publish_state("{\"ok\":1}".to_owned()));
    let token = f.server.token().to_owned();
    let ok = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body, "{\"ok\":1}");
    // Missing.
    assert_eq!(get(&f, "/api/state", "").status, 401);
    // Wrong, same length and different length.
    let same_len = "0".repeat(token.len());
    assert_eq!(
        get(&f, "/api/state", &format!("X-Panel-Token: {same_len}\r\n")).status,
        401
    );
    assert_eq!(get(&f, "/api/state", "X-Panel-Token: nope\r\n").status, 401);
    // A query string is never read for the secret.
    assert_eq!(
        get(&f, &format!("/api/state?token={token}"), "").status,
        401
    );
    assert_eq!(
        get(&f, &format!("/api/state?x-panel-token={token}"), "").status,
        401
    );
    assert_eq!(f.inbox.metrics().rejected_auth, 5);
}

#[test]
fn a_foreign_host_is_refused_everywhere() {
    let f = fixture();
    let token = f.server.token().to_owned();
    for host in ["evil.example:80", "localhost", "127.0.0.1", "127.0.0.1:1"] {
        let page = send(&f, &format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert_eq!(page.status, 403, "page for {host}");
        let api = send(
            &f,
            &format!("GET /api/state HTTP/1.1\r\nHost: {host}\r\nX-Panel-Token: {token}\r\n\r\n"),
        );
        assert_eq!(api.status, 403, "api for {host}");
    }
    let no_host = send(&f, "GET / HTTP/1.1\r\n\r\n");
    assert_eq!(no_host.status, 403);
    assert_eq!(f.inbox.metrics().rejected_host, 9);
}

#[test]
fn a_foreign_or_missing_origin_cannot_change_state() {
    let f = fixture();
    let good = good(&f);
    for origin in [
        "http://evil.example",
        "http://127.0.0.1:1",
        "null",
        "http://localhost",
    ] {
        let mut headers = with(&good, "origin");
        headers.push(("Origin", origin));
        let r = post(&f, "/api/stop", &headers, "");
        assert_eq!(r.status, 403, "origin {origin}");
    }
    let missing = post(&f, "/api/mute", &with(&good, "origin"), "");
    assert_eq!(missing.status, 403);
    assert!(
        f.inbox.drain_control(8).is_empty(),
        "nothing may reach the queue"
    );
    assert_eq!(f.inbox.metrics().rejected_origin, 5);
}

#[test]
fn a_state_change_needs_the_secret_even_with_the_right_origin() {
    let f = fixture();
    let good = good(&f);
    let r = post(&f, "/api/stop", &with(&good, "x-panel-token"), "");
    assert_eq!(r.status, 401);
    let mut headers = with(&good, "x-panel-token");
    headers.push(("X-Panel-Token", "wrong"));
    assert_eq!(post(&f, "/api/mute", &headers, "").status, 401);
    assert!(f.inbox.drain_control(8).is_empty());
}

#[test]
fn a_form_post_cannot_reach_a_state_change() {
    let f = fixture();
    let good = good(&f);
    let mut headers = with(&good, "content-type");
    headers.push(("Content-Type", "application/x-www-form-urlencoded"));
    assert_eq!(post(&f, "/api/stop", &headers, "").status, 415);
    assert_eq!(
        post(&f, "/api/stop", &with(&good, "content-type"), "").status,
        415
    );
    assert!(f.inbox.drain_control(8).is_empty());
}

#[test]
fn no_cors_headers_and_no_preflight() {
    let f = fixture();
    let token = f.server.token().to_owned();
    let preflight = send(
        &f,
        &format!(
            "OPTIONS /api/stop HTTP/1.1\r\nHost: {}\r\nOrigin: http://evil.example\r\n\
             Access-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: x-panel-token\r\n\r\n",
            f.host
        ),
    );
    assert_eq!(preflight.status, 405);
    let responses = [
        preflight,
        get(
            &f,
            "/api/state",
            &format!("X-Panel-Token: {token}\r\nOrigin: http://evil.example\r\n"),
        ),
        get(&f, "/", "Origin: http://evil.example\r\n"),
        get(&f, "/api/state", ""),
    ];
    for r in &responses {
        assert!(!r.head.contains("access-control-"), "{}", r.head);
    }
}

#[test]
fn oversized_bodies_and_text_are_refused() {
    let f = fixture();
    let good = good(&f);
    let declared = (MAX_PANEL_BODY_BYTES + 1).to_string();
    let mut headers = with(&good, "content-length");
    headers.push(("Content-Length", declared.as_str()));
    assert_eq!(post(&f, "/api/chat", &headers, "{}").status, 413);
    // Within the body cap but over the text cap.
    let long = format!("{{\"text\":\"{}\"}}", "あ".repeat(MAX_PANEL_CHAT_CHARS + 1));
    assert!(long.len() <= MAX_PANEL_BODY_BYTES);
    assert_eq!(
        post(&f, "/api/chat", &with(&good, "none"), &long).status,
        413
    );
    assert!(f.inbox.drain_chat(8).is_empty());
    // Chunked bodies are not parsed at all.
    let chunked = [("Transfer-Encoding", "chunked")];
    let mut headers = with(&good, "none");
    headers.extend(chunked);
    assert_eq!(post(&f, "/api/chat", &headers, "0\r\n\r\n").status, 400);
    // Malformed, empty, extra fields, control characters.
    for body in [
        "",
        "not json",
        "{\"text\":\"   \"}",
        "{\"text\":\"a\",\"kind\":\"operator.command\"}",
        "{\"text\":\"a\\u0000b\"}",
    ] {
        assert_eq!(
            post(&f, "/api/chat", &with(&good, "none"), body).status,
            400,
            "{body}"
        );
    }
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn a_chat_line_and_a_stop_reach_their_queues_and_get_the_answer() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let chat = thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/chat", &headers, "{\"text\":\" こんにちは \"}"));
        let join = answer_one(&f, true, "queued");
        join.join().unwrap();
        handle.join().unwrap()
    });
    assert_eq!(chat.status, 200, "{}", chat.body);
    assert!(chat.body.contains("\"ok\":true") && chat.body.contains("queued"));
    let stop = thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/stop", &headers, ""));
        // A stop never waits behind chat: it has its own queue.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_control(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].request, PanelRequest::Stop);
        commands
            .into_iter()
            .next()
            .unwrap()
            .respond(false, "rejected: x");
        handle.join().unwrap()
    });
    assert_eq!(stop.status, 409);
    assert!(stop.body.contains("\"ok\":false"));
}

#[test]
fn a_shutdown_needs_the_token_origin_and_an_empty_object_then_reaches_the_control_queue() {
    let f = fixture();
    let good = good(&f);
    // No token, a foreign origin and a non-empty body never reach the queue.
    assert_eq!(
        post(&f, "/api/shutdown", &with(&good, "x-panel-token"), "{}").status,
        401
    );
    let mut foreign = with(&good, "origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(post(&f, "/api/shutdown", &foreign, "{}").status, 403);
    let headers = with(&good, "none");
    assert_eq!(post(&f, "/api/shutdown", &headers, "").status, 400);
    assert_eq!(
        post(&f, "/api/shutdown", &headers, "{\"now\":true}").status,
        400
    );
    assert!(f.inbox.drain_control(8).is_empty());
    let accepted = thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/shutdown", &headers, "{}"));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_control(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].request, PanelRequest::Shutdown);
        commands
            .into_iter()
            .next()
            .unwrap()
            .respond(true, "shutting down");
        handle.join().unwrap()
    });
    assert_eq!(accepted.status, 200, "{}", accepted.body);
}

#[test]
fn a_model_request_is_shape_checked_authenticated_and_goes_to_the_control_queue() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    // Foreign origin and missing secret never reach the queue.
    let mut foreign = with(&good, "origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(
        post(&f, "/api/model", &foreign, "{\"model\":\"a\"}").status,
        403
    );
    assert_eq!(
        post(
            &f,
            "/api/model",
            &with(&good, "x-panel-token"),
            "{\"model\":\"a\"}"
        )
        .status,
        401
    );
    // A path, markup, an oversized alias, extra fields or a missing field.
    let long = format!("{{\"model\":\"{}\"}}", "a".repeat(97));
    for body in [
        "",
        "{}",
        "{\"model\":\"C:\\x.gguf\"}",
        "{\"model\":\"../x\"}",
        "{\"model\":\"a//b\"}",
        "{\"model\":\"a:b/c\"}",
        "{\"model\":\"A\"}",
        "{\"model\":\"a\",\"path\":\"/x\"}",
        long.as_str(),
    ] {
        assert_eq!(post(&f, "/api/model", &headers, body).status, 400, "{body}");
    }
    assert!(f.inbox.drain_control(8).is_empty());
    let answer = thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/model", &headers, "{\"model\":\"qwen3-4b\"}"));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_control(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].request,
            PanelRequest::SelectModel("qwen3-4b".to_owned())
        );
        commands
            .into_iter()
            .next()
            .unwrap()
            .respond(true, "switching");
        handle.join().unwrap()
    });
    assert_eq!(answer.status, 200, "{}", answer.body);
    // GET is not a state change and is refused with 405.
    assert_eq!(get(&f, "/api/model", "").status, 405);
}

#[test]
fn the_chat_text_reaches_the_queue_trimmed() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/chat", &headers, "{\"text\":\"  hi  \"}"));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_chat(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands[0].request, PanelRequest::Chat("hi".to_owned()));
        commands.into_iter().next().unwrap().respond(true, "ok");
        handle.join().unwrap();
    });
}

#[test]
fn a_full_queue_answers_busy_and_never_blocks_control() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    // Nobody drains: fill the chat queue from parallel clients, each
    // waits for its own answer, so run them on threads.
    let mut statuses = Vec::new();
    thread::scope(|s| {
        let handles: Vec<_> = (0..CHAT_QUEUE + 3)
            .map(|_| s.spawn(|| post(&f, "/api/chat", &headers, "{\"text\":\"x\"}").status))
            .collect();
        for h in handles {
            statuses.push(h.join().unwrap());
        }
    });
    assert!(statuses.contains(&503), "{statuses:?}");
    assert!(f.inbox.metrics().queue_full >= 1);
    // The control queue is independent of the chat queue.
    let stop = thread::scope(|s| {
        let handle = s.spawn(|| post(&f, "/api/mute", &headers, ""));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_control(4);
            thread::sleep(Duration::from_millis(5));
        }
        commands
            .into_iter()
            .next()
            .expect("queued")
            .respond(true, "muted");
        handle.join().unwrap()
    });
    assert_eq!(stop.status, 200);
}

#[test]
fn the_page_serves_only_the_panel_files() {
    let f = fixture();
    assert_eq!(get(&f, "/", "").status, 200);
    let page = get(&f, "/panel.html", "");
    assert_eq!(page.status, 200);
    assert!(page.head.contains("content-security-policy"));
    assert!(page.head.contains("frame-ancestors 'none'"));
    assert_eq!(get(&f, "/assets/a.js", "").status, 200);
    // The avatar page and anything else in the directory is not served.
    assert_eq!(get(&f, "/index.html", "").status, 404);
    assert_eq!(get(&f, "/../index.html", "").status, 404);
    assert_eq!(get(&f, "/assets/../index.html", "").status, 404);
    assert_eq!(get(&f, "/model.vrm", "").status, 404);
    assert_eq!(
        send(
            &f,
            &format!(
                "POST / HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\n\r\n",
                f.host
            )
        )
        .status,
        405
    );
}

#[test]
fn published_state_is_bounded() {
    let f = fixture();
    assert!(!f.inbox.publish_state("x".repeat(MAX_PANEL_STATE_BYTES + 1)));
    assert!(f.inbox.publish_state("{}".to_owned()));
}

#[test]
fn authenticated_calls_are_rate_limited() {
    let f = fixture();
    let token = f.server.token().to_owned();
    // Not timing based: the bucket is emptied far below zero, so no refill
    // can make a request pass however slowly the machine runs.
    f.server
        .shared
        .bucket
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tokens = -1.0e9;
    let r = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert_eq!(r.status, 429);
    assert!(f.inbox.metrics().rate_limited > 0);
    // And a full bucket lets the burst through.
    {
        let mut bucket = f
            .server
            .shared
            .bucket
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        bucket.tokens = API_BURST;
        bucket.last = Instant::now();
    }
    let r = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert_eq!(r.status, 200);
}

// ---- voice settings (#360) ----

const SENTINEL_KEY: &str = "sk-panel-SENTINEL-KEY-0123456789";

/// Posts and plays the composition thread: answers the one request that
/// reaches the voice queue with `detail`, returns what the page saw and
/// the request that was queued.
fn voice_round_trip(
    f: &Fixture,
    path: &str,
    body: &str,
    detail: &'static str,
) -> (Raw, PanelRequest) {
    let good = good(f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(f, path, &headers, body));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_voice(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1, "{path}");
        let command = commands.into_iter().next().unwrap();
        let request = command.request.clone();
        command.respond(true, detail);
        (handle.join().unwrap(), request)
    })
}

#[test]
fn voice_requests_are_authenticated_same_origin_and_use_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    let body = "{\"backend\":\"local\"}";
    for path in [
        "/api/voice",
        "/api/voice/key",
        "/api/voice/key/remove",
        "/api/voice/key/check",
        "/api/voice/preview",
    ] {
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        let mut form = with(&good, "content-type");
        form.push(("Content-Type", "application/x-www-form-urlencoded"));
        assert_eq!(post(&f, path, &form, body).status, 415, "{path}");
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    // A voice request never sits in the stop/mute queue.
    let (answer, request) = voice_round_trip(&f, "/api/voice/preview", "{}", "speaking");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(request, PanelRequest::VoicePreview);
    assert!(f.inbox.drain_control(8).is_empty());
    for (path, want) in [
        ("/api/voice/key/remove", PanelRequest::VoiceKeyRemove),
        ("/api/voice/key/check", PanelRequest::VoiceKeyCheck),
    ] {
        let (answer, request) = voice_round_trip(&f, path, "{}", "ok");
        assert_eq!(answer.status, 200, "{path}");
        assert_eq!(request, want);
    }
}

#[test]
fn a_voice_patch_is_shape_checked_before_it_reaches_the_queue() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let long = format!("{{\"credit\":\"{}\"}}", "x".repeat(2100));
    for body in [
        "",
        "[]",
        "{\"unknown\":1}",
        "{\"api_key\":\"sk-x\"}",
        "{\"backend\":\"cloud\"}",
        "{\"pitch\":\"high\"}",
        long.as_str(),
    ] {
        let status = post(&f, "/api/voice", &headers, body).status;
        assert!(status == 400 || status == 413, "{body:?} {status}");
    }
    assert!(f.inbox.drain_voice(8).is_empty());
    let patch = "{\"backend\":\"aivis\",\"style_id\":3,\"confirm_egress\":true}";
    let (answer, request) = voice_round_trip(&f, "/api/voice", patch, "applied");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(request, PanelRequest::VoiceApply(patch.to_owned()));
}

#[test]
fn the_key_is_a_body_field_held_redacted_and_never_echoed() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    for body in [
        "{}".to_owned(),
        "{\"key\":\"\"}".to_owned(),
        "{\"key\":\"has space\"}".to_owned(),
        "{\"key\":\"line\\nbreak\"}".to_owned(),
        format!("{{\"key\":\"{}\"}}", "k".repeat(MAX_PANEL_KEY_CHARS + 1)),
        format!("{{\"key\":\"{SENTINEL_KEY}\",\"also\":1}}"),
    ] {
        let answer = post(&f, "/api/voice/key", &headers, &body);
        assert_eq!(answer.status, 400, "{body}");
        assert!(!answer.body.contains(SENTINEL_KEY));
    }
    assert!(f.inbox.drain_voice(8).is_empty());
    let body = format!("{{\"key\":\"  {SENTINEL_KEY}  \"}}");
    let (answer, request) = voice_round_trip(&f, "/api/voice/key", &body, "key set");
    assert_eq!(answer.status, 200);
    assert!(!answer.body.contains(SENTINEL_KEY) && !answer.head.contains("sentinel"));
    let PanelRequest::VoiceKeySet(key) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(key.expose(), SENTINEL_KEY, "trimmed");
    assert!(!format!("{request:?}").contains(SENTINEL_KEY));
    assert!(!format!("{key:?}").contains("SENTINEL"));
}

// ---- stream chat connections (#361) ----

/// Posts and plays the composition thread for one request on the ingress
/// queue, as [`voice_round_trip`] does for the voice queue.
fn ingress_round_trip(
    f: &Fixture,
    path: &str,
    body: &str,
    detail: &'static str,
) -> (Raw, PanelRequest) {
    let good = good(f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(f, path, &headers, body));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_ingress(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1, "{path}");
        let command = commands.into_iter().next().unwrap();
        let request = command.request.clone();
        command.respond(true, detail);
        (handle.join().unwrap(), request)
    })
}

#[test]
fn ingress_requests_are_authenticated_same_origin_and_use_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    let body = "{\"platform\":\"twitch\",\"target\":\"somechannel\"}";
    for path in [
        "/api/ingress/connect",
        "/api/ingress/disconnect",
        "/api/ingress/key",
        "/api/ingress/key/remove",
    ] {
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        let mut form = with(&good, "content-type");
        form.push(("Content-Type", "application/x-www-form-urlencoded"));
        assert_eq!(post(&f, path, &form, body).status, 415, "{path}");
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    assert!(f.inbox.drain_ingress(8).is_empty(), "nothing got through");
    // A connect never sits in the stop/mute or voice queue.
    let (answer, request) = ingress_round_trip(&f, "/api/ingress/connect", body, "connecting");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        request,
        PanelRequest::IngressConnect {
            platform: IngressPlatform::Twitch,
            target: "somechannel".to_owned()
        }
    );
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    let (answer, request) = ingress_round_trip(
        &f,
        "/api/ingress/disconnect",
        "{\"platform\":\"youtube\"}",
        "ok",
    );
    assert_eq!(answer.status, 200);
    assert_eq!(
        request,
        PanelRequest::IngressDisconnect(IngressPlatform::Youtube)
    );
    let (answer, request) = ingress_round_trip(&f, "/api/ingress/key/remove", "{}", "ok");
    assert_eq!(answer.status, 200);
    assert_eq!(request, PanelRequest::IngressKeyRemove);
}

// ---- voice input (#400) ----

fn speech_input_round_trip(f: &Fixture, body: &str) -> (Raw, PanelRequest) {
    speech_input_round_trip_at(f, "/api/speech-input", body)
}

fn speech_input_round_trip_at(f: &Fixture, path: &str, body: &str) -> (Raw, PanelRequest) {
    let good = good(f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(f, path, &headers, body));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_speech_input(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        let command = commands.into_iter().next().unwrap();
        let request = command.request.clone();
        command.respond(true, "ok");
        (handle.join().unwrap(), request)
    })
}

#[test]
fn speech_input_requests_are_authenticated_same_origin_strict_and_on_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    let path = "/api/speech-input";
    let body = "{\"enabled\":true}";
    let mut foreign = with(&good, "origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(post(&f, path, &foreign, body).status, 403);
    assert_eq!(
        post(&f, path, &with(&good, "x-panel-token"), body).status,
        401
    );
    let mut form = with(&good, "content-type");
    form.push(("Content-Type", "application/x-www-form-urlencoded"));
    assert_eq!(post(&f, path, &form, body).status, 415);
    assert_eq!(get(&f, path, "").status, 405);
    // Only a boolean `enabled`: nothing else can ride along.
    let headers = with(&good, "none");
    for bad in [
        "",
        "[]",
        "{}",
        "{\"enabled\":\"true\"}",
        "{\"enabled\":1}",
        "{\"enabled\":null}",
        "{\"enabled\":true,\"device\":\"mic\"}",
        "{\"enabled\":true,\"program\":\"cmd.exe\"}",
        "{\"enabled\":true}{\"enabled\":false}",
    ] {
        assert_eq!(post(&f, path, &headers, bad).status, 400, "{bad:?}");
    }
    assert!(
        f.inbox.drain_speech_input(8).is_empty(),
        "nothing got through"
    );
    let (answer, request) = speech_input_round_trip(&f, body);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(request, PanelRequest::SpeechInput { enabled: true });
    let (_, request) = speech_input_round_trip(&f, "{\"enabled\":false}");
    assert_eq!(request, PanelRequest::SpeechInput { enabled: false });
    // Never in the stop/mute, voice, chat-connection or chat queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_ingress(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

// ---- idle initiative (#87) ----

#[test]
fn initiative_requests_are_authenticated_same_origin_strict_and_on_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    let path = "/api/initiative";
    let body = "{\"enabled\":true}";
    let mut foreign = with(&good, "origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(post(&f, path, &foreign, body).status, 403);
    assert_eq!(
        post(&f, path, &with(&good, "x-panel-token"), body).status,
        401
    );
    assert_eq!(get(&f, path, "").status, 405);
    let headers = with(&good, "none");
    for bad in [
        "",
        "{}",
        "{\"enabled\":\"true\"}",
        "{\"enabled\":true,\"kind\":\"audience_question\"}",
        "{\"enabled\":true,\"text\":\"say this\"}",
    ] {
        assert_eq!(post(&f, path, &headers, bad).status, 400, "{bad:?}");
    }
    assert!(
        f.inbox.drain_initiative(8).is_empty(),
        "nothing got through"
    );
    for (body, enabled) in [("{\"enabled\":true}", true), ("{\"enabled\":false}", false)] {
        let answer = thread::scope(|s| {
            let handle = s.spawn(|| post(&f, path, &headers, body));
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut commands = Vec::new();
            while commands.is_empty() && Instant::now() < deadline {
                commands = f.inbox.drain_initiative(4);
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(commands.len(), 1);
            let command = commands.into_iter().next().unwrap();
            assert_eq!(command.request, PanelRequest::Initiative { enabled });
            command.respond(true, "ok");
            handle.join().unwrap()
        });
        assert_eq!(answer.status, 200, "{}", answer.body);
    }
    // Never in the stop/mute, voice input, voice or chat queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_speech_input(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

// ---- session conversation log (#463) ----

#[test]
fn session_log_requests_are_authenticated_same_origin_strict_and_on_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    let body = "{\"enabled\":true}";
    for path in ["/api/session-log", "/api/session-log/delete"] {
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    let headers = with(&good, "none");
    for bad in [
        "",
        "{}",
        "{\"enabled\":\"true\"}",
        "{\"enabled\":true,\"path\":\"/tmp/x\"}",
    ] {
        assert_eq!(
            post(&f, "/api/session-log", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    for bad in ["", "[]", "{\"all\":true}", "{\"path\":\"/\"}"] {
        assert_eq!(
            post(&f, "/api/session-log/delete", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    assert!(
        f.inbox.drain_session_log(8).is_empty(),
        "nothing got through"
    );
    for (path, body, expected) in [
        (
            "/api/session-log",
            "{\"enabled\":true}",
            PanelRequest::SessionLog { enabled: true },
        ),
        (
            "/api/session-log",
            "{\"enabled\":false}",
            PanelRequest::SessionLog { enabled: false },
        ),
        (
            "/api/session-log/delete",
            "{}",
            PanelRequest::SessionLogDelete,
        ),
    ] {
        let answer = thread::scope(|s| {
            let handle = s.spawn(|| post(&f, path, &headers, body));
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut commands = Vec::new();
            while commands.is_empty() && Instant::now() < deadline {
                commands = f.inbox.drain_session_log(4);
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(commands.len(), 1);
            let command = commands.into_iter().next().unwrap();
            assert_eq!(command.request, expected);
            command.respond(true, "ok");
            handle.join().unwrap()
        });
        assert_eq!(answer.status, 200, "{}", answer.body);
    }
    // Never in the stop/mute, voice input, initiative, voice or chat queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_speech_input(8).is_empty());
    assert!(f.inbox.drain_initiative(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

// ---- character sheet (#401) ----

/// Posts `body` to a persona path and answers the one queued command with
/// `reply`; returns the HTTP answer and the request the queue saw.
fn persona_round_trip(
    f: &Fixture,
    path: &str,
    body: &str,
    text: Option<&str>,
) -> (Raw, PanelRequest) {
    let good = good(f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(f, path, &headers, body));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_persona(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        let command = commands.into_iter().next().unwrap();
        let request = command.request.clone();
        command.respond_with_text(true, "ok", text.map(str::to_owned));
        (handle.join().unwrap(), request)
    })
}

#[test]
fn persona_requests_are_authenticated_same_origin_strict_and_on_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    for path in ["/api/persona/set", "/api/persona/get"] {
        let body = if path.ends_with("set") {
            "{\"text\":\"x\"}"
        } else {
            "{}"
        };
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        let mut form = with(&good, "content-type");
        form.push(("Content-Type", "application/x-www-form-urlencoded"));
        assert_eq!(post(&f, path, &form, body).status, 415, "{path}");
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    // Only a string `text`: nothing else can ride along, and a bad body
    // never reaches the queue.
    let headers = with(&good, "none");
    for bad in [
        "",
        "[]",
        "{}",
        "{\"text\":1}",
        "{\"text\":null}",
        "{\"text\":\"x\",\"capability\":\"persona.configure\"}",
        "{\"text\":\"x\"}{\"text\":\"y\"}",
    ] {
        assert_eq!(
            post(&f, "/api/persona/set", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    for bad in ["", "[]", "{\"text\":\"x\"}"] {
        assert_eq!(
            post(&f, "/api/persona/get", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    assert!(f.inbox.drain_persona(8).is_empty(), "nothing got through");

    let (answer, request) = persona_round_trip(
        &f,
        "/api/persona/set",
        "{\"text\":\"名前: ミコト\\n\"}",
        None,
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let PanelRequest::PersonaSet(text) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(text.expose(), "名前: ミコト\n");
    // The text is redacted in Debug, and the set answer carries none.
    assert!(!format!("{request:?}").contains("ミコト"));
    assert!(!answer.body.contains("ミコト") && !answer.body.contains("\"text\""));
    let (_, request) = persona_round_trip(&f, "/api/persona/set", "{\"text\":\"\"}", None);
    let PanelRequest::PersonaSet(text) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(text.expose(), "");

    // Never in the stop/mute, voice, chat-connection, speech or chat queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_ingress(8).is_empty());
    assert!(f.inbox.drain_speech_input(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn a_persona_get_answer_carries_the_text_and_only_that_answer_does() {
    let f = fixture();
    let (answer, request) = persona_round_trip(
        &f,
        "/api/persona/get",
        "{}",
        Some("名前: ミコト\n\"一人称\""),
    );
    assert_eq!(request, PanelRequest::PersonaGet);
    assert_eq!(answer.status, 200, "{}", answer.body);
    let value: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["text"], "名前: ミコト\n\"一人称\"");
    // The state document the page polls never holds it.
    let token = f.server.token().to_owned();
    let state = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert!(!state.body.contains("ミコト"));
}

#[test]
fn the_persona_body_is_bounded_before_it_is_read_and_must_be_valid_utf8() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    // A body larger than the persona bound is refused unread.
    let declared = (MAX_PANEL_PERSONA_BODY_BYTES + 1).to_string();
    let mut long = headers.clone();
    long.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/persona/set", &long, "{").status, 413);
    // Every other body keeps the small bound.
    let declared = (MAX_PANEL_BODY_BYTES + 1).to_string();
    let mut other = headers.clone();
    other.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/chat", &other, "{").status, 413);
    // A body above the old bound but within the persona one is read.
    let big = format!(
        "{{\"text\":\"{}\"}}",
        "a".repeat(MAX_PANEL_BODY_BYTES + 100)
    );
    let (answer, request) = persona_round_trip(&f, "/api/persona/set", &big, None);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(matches!(request, PanelRequest::PersonaSet(_)));
    // Bytes that are not UTF-8 never reach the queue.
    let mut raw = format!(
        "POST /api/persona/set HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nX-Panel-Token: {}\r\n\
         Content-Type: application/json\r\nContent-Length: 13\r\n\r\n",
        f.host,
        f.origin,
        f.server.token()
    )
    .into_bytes();
    raw.extend_from_slice(b"{\"text\":\"\xFF\xFE\"}");
    assert_eq!(send_bytes(&f, &raw).status, 400);
    assert!(f.inbox.drain_persona(8).is_empty());
}

// ---- operator topic list (#87 S2) ----

#[test]
fn topics_requests_are_authenticated_same_origin_strict_and_share_the_persona_queue() {
    let f = fixture();
    let good = good(&f);
    for path in ["/api/topics/set", "/api/topics/get"] {
        let body = if path.ends_with("set") {
            "{\"text\":\"x\"}"
        } else {
            "{}"
        };
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        let mut form = with(&good, "content-type");
        form.push(("Content-Type", "application/x-www-form-urlencoded"));
        assert_eq!(post(&f, path, &form, body).status, 415, "{path}");
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    // Only a string `text`: nothing else can ride along, and a bad body
    // never reaches the queue.
    let headers = with(&good, "none");
    for bad in [
        "",
        "[]",
        "{}",
        "{\"text\":1}",
        "{\"text\":\"x\",\"capability\":\"persona.configure\"}",
        "{\"text\":\"x\"}{\"text\":\"y\"}",
    ] {
        assert_eq!(
            post(&f, "/api/topics/set", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    for bad in ["", "[]", "{\"text\":\"x\"}"] {
        assert_eq!(
            post(&f, "/api/topics/get", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    assert!(f.inbox.drain_persona(8).is_empty(), "nothing got through");

    let (answer, request) = persona_round_trip(
        &f,
        "/api/topics/set",
        "{\"text\":\"好きな食べ物\\n天気\"}",
        None,
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let PanelRequest::TopicsSet(text) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(text.expose(), "好きな食べ物\n天気");
    // The text is redacted in Debug, and the set answer carries none.
    assert!(!format!("{request:?}").contains("好きな"));
    assert!(!answer.body.contains("好きな") && !answer.body.contains("\"text\""));
    // The get answer carries the text, and only that answer does.
    let (answer, request) =
        persona_round_trip(&f, "/api/topics/get", "{}", Some("好きな食べ物\n天気"));
    assert_eq!(request, PanelRequest::TopicsGet);
    let value: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(value["text"], "好きな食べ物\n天気");
    let token = f.server.token().to_owned();
    let state = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert!(!state.body.contains("好きな"));

    // Never in the stop/mute, voice, chat-connection, speech or chat queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_ingress(8).is_empty());
    assert!(f.inbox.drain_speech_input(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn the_topics_body_has_its_own_bound_checked_before_it_is_read() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let declared = (MAX_PANEL_TOPICS_BODY_BYTES + 1).to_string();
    let mut long = headers.clone();
    long.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/topics/set", &long, "{").status, 413);
    // Above the persona bound but within the topics one is read.
    let big = format!(
        "{{\"text\":\"{}\"}}",
        "a".repeat(MAX_PANEL_PERSONA_BODY_BYTES + 100)
    );
    let (answer, request) = persona_round_trip(&f, "/api/topics/set", &big, None);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(matches!(request, PanelRequest::TopicsSet(_)));
    // The persona bound is untouched by it.
    let declared = (MAX_PANEL_PERSONA_BODY_BYTES + 1).to_string();
    let mut other = headers.clone();
    other.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/persona/set", &other, "{").status, 413);
}

// ---- memory seed (#402) ----

/// Posts `body` to a memory-seed path and answers the one queued command
/// with `text`; returns the HTTP answer and the request the queue saw.
fn memory_seed_round_trip(
    f: &Fixture,
    path: &str,
    body: &str,
    text: Option<&str>,
) -> (Raw, PanelRequest) {
    let good = good(f);
    let headers = with(&good, "none");
    thread::scope(|s| {
        let handle = s.spawn(|| post(f, path, &headers, body));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut commands = Vec::new();
        while commands.is_empty() && Instant::now() < deadline {
            commands = f.inbox.drain_memory_seed(4);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(commands.len(), 1);
        let command = commands.into_iter().next().unwrap();
        let request = command.request.clone();
        command.respond_with_text(true, "ok", text.map(str::to_owned));
        (handle.join().unwrap(), request)
    })
}

const SEED_DOC: &str = "{\"schema_version\":\"0.1.0\",\"seed_id\":\"s\",\"revision\":\"1\",\"entries\":[{\"kind\":\"persona-fact\",\"claim\":\"ミコトは猫が好き\"}]}";

#[test]
fn memory_seed_requests_are_authenticated_same_origin_strict_and_on_their_own_queue() {
    let f = fixture();
    let good = good(&f);
    for path in ["/api/memory-seed/set", "/api/memory-seed/get"] {
        let body = if path.ends_with("set") {
            SEED_DOC
        } else {
            "{}"
        };
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        let mut form = with(&good, "content-type");
        form.push(("Content-Type", "application/x-www-form-urlencoded"));
        assert_eq!(post(&f, path, &form, body).status, 415, "{path}");
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    let headers = with(&good, "none");
    for bad in ["", "[]", "\"x\"", "7", "null", "{\"a\":", "{}{}"] {
        assert_eq!(
            post(&f, "/api/memory-seed/set", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    for bad in ["", "[]", SEED_DOC] {
        assert_eq!(
            post(&f, "/api/memory-seed/get", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    assert!(
        f.inbox.drain_memory_seed(8).is_empty(),
        "nothing got through"
    );

    let (answer, request) = memory_seed_round_trip(&f, "/api/memory-seed/set", SEED_DOC, None);
    assert_eq!(answer.status, 200, "{}", answer.body);
    let PanelRequest::MemorySeedSet(document) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(document.expose(), SEED_DOC);
    // The claim is redacted in Debug, and the set answer carries none.
    assert!(!format!("{request:?}").contains("ミコト"));
    assert!(!answer.body.contains("ミコト") && !answer.body.contains("\"text\""));

    // Never in the other queues.
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_ingress(8).is_empty());
    assert!(f.inbox.drain_speech_input(8).is_empty());
    assert!(f.inbox.drain_persona(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn a_memory_seed_get_answer_carries_the_document_and_only_that_answer_does() {
    let f = fixture();
    let (answer, request) =
        memory_seed_round_trip(&f, "/api/memory-seed/get", "{}", Some(SEED_DOC));
    assert_eq!(request, PanelRequest::MemorySeedGet);
    assert_eq!(answer.status, 200, "{}", answer.body);
    let value: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["text"], SEED_DOC);
    let token = f.server.token().to_owned();
    let state = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
    assert!(!state.body.contains("ミコト"));
}

#[test]
fn the_memory_seed_body_is_bounded_before_it_is_read_and_must_be_valid_utf8() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let declared = (MAX_PANEL_MEMORY_SEED_BODY_BYTES + 1).to_string();
    let mut long = headers.clone();
    long.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/memory-seed/set", &long, "{").status, 413);
    // The persona bound is untouched by it.
    let declared = (MAX_PANEL_PERSONA_BODY_BYTES + 1).to_string();
    let mut other = headers.clone();
    other.push(("Content-Length", &declared));
    assert_eq!(post(&f, "/api/persona/set", &other, "{").status, 413);
    // A document above the persona bound but within the seed one is read.
    let big = format!(
        "{{\"pad\":\"{}\"}}",
        "a".repeat(MAX_PANEL_PERSONA_BODY_BYTES + 100)
    );
    let (answer, request) = memory_seed_round_trip(&f, "/api/memory-seed/set", &big, None);
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(matches!(request, PanelRequest::MemorySeedSet(_)));
    // Bytes that are not UTF-8 never reach the queue.
    let mut raw = format!(
        "POST /api/memory-seed/set HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nX-Panel-Token: {}\r\n\
         Content-Type: application/json\r\nContent-Length: 10\r\n\r\n",
        f.host,
        f.origin,
        f.server.token()
    )
    .into_bytes();
    raw.extend_from_slice(b"{\"a\":\"\xFF\xFE\"}");
    assert_eq!(send_bytes(&f, &raw).status, 400);
    assert!(f.inbox.drain_memory_seed(8).is_empty());
}

#[test]
fn speech_input_device_and_duplex_are_strict_authenticated_and_on_the_same_queue() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    for path in ["/api/speech-input/device", "/api/speech-input/duplex"] {
        let body = if path.ends_with("device") {
            "{\"device\":null}"
        } else {
            "{\"duplex\":\"full\"}"
        };
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, body).status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), body).status,
            401,
            "{path}"
        );
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    let long = "x".repeat(129);
    let too_long = format!("{{\"device\":\"{long}\"}}");
    for bad in [
        "",
        "{}",
        "{\"device\":1}",
        "{\"device\":\"\"}",
        "{\"device\":\"a\nb\"}",
        "{\"device\":\"mic\",\"enabled\":true}",
        "{\"device\":\"mic\",\"program\":\"cmd.exe\"}",
        too_long.as_str(),
    ] {
        assert_eq!(
            post(&f, "/api/speech-input/device", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    for bad in [
        "",
        "{}",
        "{\"duplex\":\"both\"}",
        "{\"duplex\":true}",
        "{\"duplex\":\"half\",\"device\":\"mic\"}",
    ] {
        assert_eq!(
            post(&f, "/api/speech-input/duplex", &headers, bad).status,
            400,
            "{bad:?}"
        );
    }
    // Nothing but the enabled body reaches the old endpoint either.
    assert_eq!(
        post(&f, "/api/speech-input", &headers, "{\"duplex\":\"half\"}").status,
        400
    );
    assert!(
        f.inbox.drain_speech_input(8).is_empty(),
        "nothing got through"
    );
    let (answer, request) = speech_input_round_trip_at(
        &f,
        "/api/speech-input/device",
        "{\"device\":\"USB Mic (Yamaha)\"}",
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(
        request,
        PanelRequest::SpeechInputDevice(Some("USB Mic (Yamaha)".to_owned()))
    );
    let (_, request) =
        speech_input_round_trip_at(&f, "/api/speech-input/device", "{\"device\":null}");
    assert_eq!(request, PanelRequest::SpeechInputDevice(None));
    let (_, request) =
        speech_input_round_trip_at(&f, "/api/speech-input/duplex", "{\"duplex\":\"full\"}");
    assert_eq!(request, PanelRequest::SpeechInputDuplex { full: true });
    let (_, request) =
        speech_input_round_trip_at(&f, "/api/speech-input/duplex", "{\"duplex\":\"half\"}");
    assert_eq!(request, PanelRequest::SpeechInputDuplex { full: false });
    assert!(f.inbox.drain_control(8).is_empty());
    assert!(f.inbox.drain_voice(8).is_empty());
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn a_connect_target_is_normalized_and_bad_input_never_reaches_the_queue() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    for body in [
        "",
        "[]",
        "{}",
        "{\"platform\":\"twitch\"}",
        "{\"platform\":\"kick\",\"target\":\"abcdef\"}",
        "{\"platform\":\"twitch\",\"target\":\"ab\"}",
        "{\"platform\":\"twitch\",\"target\":\"#channel\"}",
        "{\"platform\":\"twitch\",\"target\":\"a b\"}",
        "{\"platform\":\"twitch\",\"target\":\"abc\",\"program\":\"cmd.exe\"}",
        "{\"platform\":\"youtube\",\"target\":\"short\"}",
        "{\"platform\":\"youtube\",\"target\":\"https://evil.example/watch?v=dQw4w9WgXcQ\"}",
        "{\"platform\":\"youtube\",\"target\":\"dQw4w9WgXcQ; calc\"}",
        "{\"platform\":\"disconnect-me\"}",
    ] {
        let status = post(&f, "/api/ingress/connect", &headers, body).status;
        assert_eq!(status, 400, "{body:?}");
    }
    assert_eq!(
        post(
            &f,
            "/api/ingress/disconnect",
            &headers,
            "{\"platform\":\"kick\"}"
        )
        .status,
        400
    );
    assert_eq!(
        post(
            &f,
            "/api/ingress/disconnect",
            &headers,
            "{\"platform\":\"twitch\",\"x\":1}"
        )
        .status,
        400
    );
    assert!(f.inbox.drain_ingress(8).is_empty());
    // Normalized before it is queued: a link becomes the video id, a
    // channel is lowercased.
    let (_, request) = ingress_round_trip(
        &f,
        "/api/ingress/connect",
        "{\"platform\":\"youtube\",\"target\":\" https://youtu.be/dQw4w9WgXcQ?si=x \"}",
        "ok",
    );
    assert_eq!(
        request,
        PanelRequest::IngressConnect {
            platform: IngressPlatform::Youtube,
            target: "dQw4w9WgXcQ".to_owned()
        }
    );
    let (_, request) = ingress_round_trip(
        &f,
        "/api/ingress/connect",
        "{\"platform\":\"twitch\",\"target\":\" Some_Channel \"}",
        "ok",
    );
    assert_eq!(
        request,
        PanelRequest::IngressConnect {
            platform: IngressPlatform::Twitch,
            target: "some_channel".to_owned()
        }
    );
}

#[test]
fn the_youtube_key_is_a_redacted_body_field_never_echoed() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    for body in [
        "{}".to_owned(),
        "{\"key\":\"\"}".to_owned(),
        "{\"key\":\"has space\"}".to_owned(),
        format!("{{\"key\":\"{}\"}}", "k".repeat(MAX_PANEL_KEY_CHARS + 1)),
        format!("{{\"key\":\"{SENTINEL_KEY}\",\"also\":1}}"),
    ] {
        let answer = post(&f, "/api/ingress/key", &headers, &body);
        assert_eq!(answer.status, 400, "{body}");
        assert!(!answer.body.contains(SENTINEL_KEY));
    }
    assert!(f.inbox.drain_ingress(8).is_empty());
    let body = format!("{{\"key\":\"  {SENTINEL_KEY}  \"}}");
    let (answer, request) = ingress_round_trip(&f, "/api/ingress/key", &body, "key set");
    assert_eq!(answer.status, 200);
    assert!(!answer.body.contains(SENTINEL_KEY));
    let PanelRequest::IngressKeySet(key) = &request else {
        panic!("{request:?}");
    };
    assert_eq!(key.expose(), SENTINEL_KEY, "trimmed");
    assert!(!format!("{request:?}").contains(SENTINEL_KEY));
}

fn search_fixture() -> (Fixture, crate::speech_tts::aivis_cloud_fake::FakeAivisCloud) {
    use crate::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    let api = FakeAivisCloud::start(FakeAivisBehaviour {
        search_body: Some(
            include_bytes!("../../../tests/fixtures/aivis-model-search.json").to_vec(),
        ),
        ..FakeAivisBehaviour::default()
    });
    (fixture_with_search(Some(api.endpoint())), api)
}

#[test]
fn the_search_is_proxied_typed_and_sends_no_credential() {
    let (f, api) = search_fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let answer = post(
        &f,
        "/api/voice/search",
        &headers,
        "{\"keyword\":\"まお\",\"sort\":\"like\",\"page\":2}",
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    let value: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["page"]["page"], 2);
    assert_eq!(value["page"]["models"].as_array().unwrap().len(), 3);
    assert_eq!(value["page"]["models"][0]["name"], "まお");
    // Only the typed DTO crossed: no wire field the page does not need.
    for absent in ["icon_url", "audio_url", "handle"] {
        assert!(!answer.body.contains(absent), "{absent}");
    }
    let requests = api.requests();
    assert_eq!(requests[0].query_param("keyword").as_deref(), Some("まお"));
    assert_eq!(requests[0].query_param("sort").as_deref(), Some("like"));
    assert!(requests[0].header("authorization").is_none());
    // Defaults, and the search never touches the composition queues.
    let answer = post(&f, "/api/voice/search", &headers, "{}");
    assert_eq!(answer.status, 200);
    assert!(f.inbox.drain_voice(8).is_empty() && f.inbox.drain_control(8).is_empty());
}

#[test]
fn the_search_is_validated_authenticated_bounded_and_one_at_a_time() {
    let (f, _api) = search_fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let mut foreign = with(&good, "origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(post(&f, "/api/voice/search", &foreign, "{}").status, 403);
    assert_eq!(
        post(&f, "/api/voice/search", &with(&good, "x-panel-token"), "{}").status,
        401
    );
    assert_eq!(get(&f, "/api/voice/search", "").status, 405);
    let long = format!("{{\"keyword\":\"{}\"}}", "a".repeat(65));
    for body in [
        "{\"sort\":\"price\"}",
        "{\"page\":0}",
        "{\"page\":101}",
        "{\"x\":1}",
        "{\"keyword\":\"a\\nb\"}",
        long.as_str(),
    ] {
        assert_eq!(
            post(&f, "/api/voice/search", &headers, body).status,
            400,
            "{body}"
        );
    }
    // Off unless configured: the route does not exist.
    let plain_panel = fixture();
    let good2 = self::good(&plain_panel);
    assert_eq!(
        post(
            &plain_panel,
            "/api/voice/search",
            &with(&good2, "none"),
            "{}"
        )
        .status,
        404
    );
    // One at a time: a held-open search makes the second one a 429.
    use crate::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    let slow = FakeAivisCloud::start(FakeAivisBehaviour {
        delay: Duration::from_millis(700),
        ..FakeAivisBehaviour::default()
    });
    let g = fixture_with_search(Some(slow.endpoint()));
    let good3 = self::good(&g);
    let headers3 = with(&good3, "none");
    let statuses = thread::scope(|s| {
        let first = s.spawn(|| post(&g, "/api/voice/search", &headers3, "{}").status);
        thread::sleep(Duration::from_millis(200));
        let second = post(&g, "/api/voice/search", &headers3, "{}").status;
        (first.join().unwrap(), second)
    });
    assert_eq!(statuses, (200, 429));
    // The slot is free again afterwards.
    assert_eq!(post(&g, "/api/voice/search", &headers3, "{}").status, 200);
}

#[test]
fn an_unreachable_or_failing_service_is_a_502_with_a_fixed_message() {
    use crate::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    let down = FakeAivisCloud::start(FakeAivisBehaviour {
        search_status: 503,
        ..FakeAivisBehaviour::default()
    });
    let f = fixture_with_search(Some(down.endpoint()));
    let good = good(&f);
    let answer = post(&f, "/api/voice/search", &with(&good, "none"), "{}");
    assert_eq!(answer.status, 502);
    assert!(answer.body.contains("HTTP 503"), "{}", answer.body);
    let closed = fixture_with_search(Some("http://127.0.0.1:9".to_owned()));
    let good = self::good(&closed);
    let answer = post(&closed, "/api/voice/search", &with(&good, "none"), "{}");
    assert_eq!(answer.status, 502);
    assert!(answer.body.contains("unreachable"), "{}", answer.body);
}

// ---- avatar choice (#362) ----

const VRM_JSON: &str = r#"{"extensions":{"VRMC_vrm":{"meta":{"name":"Test","authors":["A"],
    "avatarPermission":"everyone","commercialUsage":"corporation"}}}}"#;

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn post_bytes(f: &Fixture, headers: &[(&str, &str)], declared: Option<u64>, body: &[u8]) -> Raw {
    let mut request = format!("POST {UPLOAD_PATH} HTTP/1.1\r\nHost: {}\r\n", f.host).into_bytes();
    for (k, v) in headers {
        request.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    let declared = declared.unwrap_or(body.len() as u64);
    request.extend_from_slice(format!("Content-Length: {declared}\r\n\r\n").as_bytes());
    request.extend_from_slice(body);
    send_bytes(f, &request)
}

fn upload_headers(f: &Fixture) -> Vec<(&'static str, String)> {
    vec![
        ("Origin", f.origin.clone()),
        ("X-Panel-Token", f.server.token().to_owned()),
        ("Content-Type", "application/octet-stream".to_owned()),
    ]
}

fn as_pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn staging_is_empty(f: &Fixture) -> bool {
    fs::read_dir(&f.staging).map_or(true, |mut entries| entries.next().is_none())
}

/// Waits for the one queued avatar request (the composition thread's side).
fn take_avatar(f: &Fixture) -> PanelCommand {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(command) = f.inbox.drain_avatar(1).into_iter().next() {
            return command;
        }
        assert!(Instant::now() < deadline, "no avatar request arrived");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_upload_is_staged_hashed_and_only_its_hash_is_forwarded() {
    let f = fixture();
    let model = crate::local_avatar::vrm::test_model(VRM_JSON);
    let hash = sha256_hex(&model);
    let headers = upload_headers(&f);
    let response = thread::scope(|s| {
        let handle = s.spawn(|| post_bytes(&f, &as_pairs(&headers), None, &model));
        let command = take_avatar(&f);
        assert_eq!(command.request, PanelRequest::AvatarUpload(hash.clone()));
        // The staged file is named after its hash and holds the bytes.
        assert_eq!(
            fs::read(f.staging.join(format!("{hash}.vrm"))).unwrap(),
            model
        );
        command.respond(true, "candidate ready");
        handle.join().unwrap()
    });
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(response.body.contains("candidate ready"));
    // No temporary file is left next to it.
    let names: Vec<_> = fs::read_dir(&f.staging)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [format!("{hash}.vrm")]);
}

#[test]
fn a_refused_upload_leaves_nothing_staged() {
    let f = fixture();
    let model = crate::local_avatar::vrm::test_model(VRM_JSON);
    let headers = upload_headers(&f);
    let pairs = as_pairs(&headers);
    // The composition thread cannot take it: the file is removed again.
    let response = thread::scope(|s| {
        let handle = s.spawn(|| post_bytes(&f, &pairs, None, &model));
        take_avatar(&f).respond(false, "avatar store is full");
        handle.join().unwrap()
    });
    assert_eq!(response.status, 409);
    assert!(staging_is_empty(&f));
}

#[test]
fn bad_uploads_get_typed_refusals_and_never_reach_the_queue() {
    let f = fixture();
    let headers = upload_headers(&f);
    let pairs = as_pairs(&headers);
    let valid = crate::local_avatar::vrm::test_model(VRM_JSON);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.resize(4096, 0);
    let mut cut = valid.clone();
    cut.truncate(cut.len() - 100);
    let mut lying = valid.clone();
    lying[12..16].copy_from_slice(&(1_u32 << 30).to_le_bytes());
    let plain_glb = crate::local_avatar::vrm::test_model(r#"{"asset":{"version":"2.0"}}"#);
    let mut not_utf8 = crate::local_avatar::vrm::test_model(VRM_JSON);
    not_utf8[20] = 0xFF;
    let mut huge_json = valid.clone();
    huge_json[12..16].copy_from_slice(&(vrm::MAX_JSON_BYTES as u32 + 4).to_le_bytes());
    let cases: [(&str, &[u8], u16, &str); 7] = [
        ("renamed png", &png[..], 415, "not_glb"),
        ("truncated glb", &cut[..], 400, "length_mismatch"),
        ("lying json chunk", &lying[..], 400, "chunk_out_of_bounds"),
        ("glb without VRM", &plain_glb[..], 415, "not_vrm"),
        (
            "json that is not utf-8",
            &not_utf8[..],
            400,
            "json_not_utf8",
        ),
        (
            "json chunk length over the cap",
            &huge_json[..],
            400,
            "chunk_out_of_bounds",
        ),
        ("too short", &b"glTF"[..], 400, "truncated"),
    ];
    for (name, body, status, code) in cases {
        let r = post_bytes(&f, &pairs, None, body);
        assert_eq!(r.status, status, "{name}: {}", r.body);
        assert!(r.body.contains(code), "{name}: {}", r.body);
    }
    // A body longer than the glTF header says: the header is the lie.
    let r = post_bytes(&f, &pairs, Some(valid.len() as u64 + 10), &valid);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("length_mismatch"), "{}", r.body);
    // A consistent header and declared length, but the client stops
    // sending early: refused once it does, not after the idle timeout, and
    // the partial file is removed.
    let r = post_bytes(
        &f,
        &pairs,
        Some(valid.len() as u64),
        &valid[..valid.len() - 10],
    );
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("incomplete body"), "{}", r.body);
    assert!(f.inbox.drain_avatar(8).is_empty());
    assert!(staging_is_empty(&f), "no partial file may remain");
}

#[test]
fn an_oversized_upload_is_refused_before_any_of_it_is_read() {
    let f = fixture();
    let headers = upload_headers(&f);
    let pairs = as_pairs(&headers);
    // Only a header is sent; the declared length is one byte over the cap.
    let r = post_bytes(&f, &pairs, Some(vrm::MAX_VRM_BYTES + 1), b"");
    assert_eq!(r.status, 413, "{}", r.body);
    assert!(r.body.contains("too_large"));
    let r = post_bytes(&f, &pairs, Some(u64::MAX), b"");
    assert_eq!(r.status, 413);
    assert!(f.inbox.drain_avatar(8).is_empty());
    assert!(staging_is_empty(&f));
}

#[test]
fn an_upload_needs_origin_secret_and_the_right_content_type() {
    let f = fixture();
    let model = crate::local_avatar::vrm::test_model(VRM_JSON);
    let headers = upload_headers(&f);
    let without = |name: &str| -> Vec<(&'static str, &str)> {
        headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case(name))
            .map(|(k, v)| (*k, v.as_str()))
            .collect()
    };
    assert_eq!(post_bytes(&f, &without("origin"), None, &model).status, 403);
    let mut foreign = without("origin");
    foreign.push(("Origin", "http://evil.example"));
    assert_eq!(post_bytes(&f, &foreign, None, &model).status, 403);
    assert_eq!(
        post_bytes(&f, &without("x-panel-token"), None, &model).status,
        401
    );
    let mut wrong = without("x-panel-token");
    wrong.push(("X-Panel-Token", "nope"));
    assert_eq!(post_bytes(&f, &wrong, None, &model).status, 401);
    // A form post or a JSON post is not an upload.
    for ctype in [
        "application/json",
        "multipart/form-data; boundary=x",
        "text/plain",
    ] {
        let mut other = without("content-type");
        other.push(("Content-Type", ctype));
        assert_eq!(post_bytes(&f, &other, None, &model).status, 415, "{ctype}");
    }
    assert_eq!(
        post_bytes(&f, &without("content-type"), None, &model).status,
        415
    );
    // Chunked bodies are not parsed.
    let mut chunked = without("none");
    chunked.push(("Transfer-Encoding", "chunked"));
    assert_eq!(post_bytes(&f, &chunked, None, &model).status, 400);
    assert!(f.inbox.drain_avatar(8).is_empty());
    assert!(staging_is_empty(&f));
}

#[test]
fn one_upload_runs_at_a_time() {
    let f = fixture();
    let model = crate::local_avatar::vrm::test_model(VRM_JSON);
    let headers = upload_headers(&f);
    f.server.shared.upload_busy.store(true, Ordering::SeqCst);
    let r = post_bytes(&f, &as_pairs(&headers), None, &model);
    assert_eq!(r.status, 429, "{}", r.body);
    assert!(f.inbox.drain_avatar(8).is_empty());
    f.server.shared.upload_busy.store(false, Ordering::SeqCst);
}

#[test]
fn the_upload_flag_is_released_after_a_refusal() {
    let f = fixture();
    let headers = upload_headers(&f);
    let pairs = as_pairs(&headers);
    assert_eq!(
        post_bytes(&f, &pairs, None, b"not a model at all, but long enough").status,
        415
    );
    assert!(!f.server.shared.upload_busy.load(Ordering::SeqCst));
}

#[test]
fn the_upload_route_does_not_exist_without_a_staging_directory() {
    let (server, inbox) = LocalPanelServer::start(LocalPanelConfig {
        port: 0,
        page_dir: None,
        voice_search_endpoint: None,
        avatar_staging_dir: None,
    })
    .unwrap();
    let port = server.local_addr().port();
    let f = Fixture {
        server,
        inbox,
        host: format!("127.0.0.1:{port}"),
        origin: format!("http://127.0.0.1:{port}"),
        staging: PathBuf::new(),
    };
    let headers = upload_headers(&f);
    let r = post_bytes(&f, &as_pairs(&headers), None, b"anything");
    assert_eq!(r.status, 404);
}

#[test]
fn avatar_requests_name_a_model_by_hash_only() {
    let f = fixture();
    let good = good(&f);
    let headers = with(&good, "none");
    let hash = "ab".repeat(32);
    let cases = [
        (
            "/api/avatar/confirm",
            format!("{{\"id\":\"{hash}\"}}"),
            PanelRequest::AvatarConfirm(hash.clone()),
        ),
        (
            "/api/avatar/select",
            "{\"id\":\"sample\"}".to_owned(),
            PanelRequest::AvatarSelect("sample".to_owned()),
        ),
        (
            "/api/avatar/select",
            "{\"id\":\"abcdef01\"}".to_owned(),
            PanelRequest::AvatarSelect("abcdef01".to_owned()),
        ),
        (
            "/api/avatar/delete",
            format!("{{\"id\":\"{hash}\"}}"),
            PanelRequest::AvatarDelete(hash.clone()),
        ),
        (
            "/api/avatar/discard",
            "{}".to_owned(),
            PanelRequest::AvatarDiscard,
        ),
    ];
    for (path, body, expected) in cases {
        let response = thread::scope(|s| {
            let handle = s.spawn(|| post(&f, path, &headers, &body));
            let command = take_avatar(&f);
            assert_eq!(command.request, expected, "{path}");
            command.respond(true, "done");
            handle.join().unwrap()
        });
        assert_eq!(response.status, 200, "{path}: {}", response.body);
    }
    // Nothing else is accepted as an id: paths, names, upper case, short
    // prefixes, extra fields, other types.
    for body in [
        "{\"id\":\"../../etc/passwd\"}".to_owned(),
        "{\"id\":\"model.vrm\"}".to_owned(),
        "{\"id\":\"C:\\\\x\"}".to_owned(),
        format!("{{\"id\":\"{}\"}}", hash.to_uppercase()),
        "{\"id\":\"abcdef0\"}".to_owned(),
        format!("{{\"id\":\"{}0\"}}", hash),
        "{\"id\":\"\"}".to_owned(),
        "{\"id\":7}".to_owned(),
        "{\"id\":\"sample\",\"path\":\"/x\"}".to_owned(),
        "{}".to_owned(),
        "not json".to_owned(),
    ] {
        for path in [
            "/api/avatar/confirm",
            "/api/avatar/select",
            "/api/avatar/delete",
        ] {
            let r = post(&f, path, &headers, &body);
            assert_eq!(r.status, 400, "{path} {body}: {}", r.body);
        }
    }
    for body in ["{\"id\":\"sample\"}", "not json", "[]"] {
        assert_eq!(
            post(&f, "/api/avatar/discard", &headers, body).status,
            400,
            "{body}"
        );
    }
    assert!(f.inbox.drain_avatar(8).is_empty());
    assert!(
        f.inbox.drain_control(8).is_empty(),
        "never on the stop queue"
    );
    assert!(f.inbox.drain_chat(8).is_empty());
}

#[test]
fn avatar_requests_need_origin_and_secret_like_every_state_change() {
    let f = fixture();
    let good = good(&f);
    for path in [
        "/api/avatar/confirm",
        "/api/avatar/select",
        "/api/avatar/delete",
        "/api/avatar/discard",
    ] {
        let mut foreign = with(&good, "origin");
        foreign.push(("Origin", "http://evil.example"));
        assert_eq!(post(&f, path, &foreign, "{}").status, 403, "{path}");
        assert_eq!(
            post(&f, path, &with(&good, "x-panel-token"), "{}").status,
            401,
            "{path}"
        );
        assert_eq!(get(&f, path, "").status, 405, "{path}");
    }
    assert!(f.inbox.drain_avatar(8).is_empty());
}

#[test]
fn the_hash_shape_check_is_strict() {
    assert!(is_hash_prefix("abcdef01"));
    assert!(is_hash_prefix(&"0".repeat(64)));
    assert!(!is_hash_prefix("abcdef0"));
    assert!(!is_hash_prefix(&"0".repeat(65)));
    assert!(!is_hash_prefix("ABCDEF01"));
    assert!(!is_hash_prefix("abcdefg1"));
    assert!(!is_hash_prefix("abcd ef01"));
}
