//! Local operation panel: a small loopback HTTP surface next to the avatar
//! renderer server (issue #335, docs/security-threat-model.adoc section 15).
//!
//! It reuses the renderer server's HTTP plumbing (head peeking, constant-time
//! compare, entropy token) and adds only a static page and four JSON
//! endpoints. It holds **no authority**: it validates the HTTP envelope and
//! forwards typed requests to the composition thread through bounded queues.
//! The composition thread (`aivtuber-app`) turns a stop or mute into the same
//! operator request `aivtuberctl` sends, so verification, capability minting
//! and audit are the ones that already exist; a chat line becomes an ordinary
//! untrusted content envelope there.
//!
//! Network rules, each with a negative test below:
//!
//! * binds 127.0.0.1 only, and there is no bind-address option;
//! * the `Host` header must equal the loopback `host:port` (DNS rebinding);
//! * state-changing requests must carry `Origin` equal to the panel origin
//!   (CSRF) and `Content-Type: application/json` (no form posts);
//! * every API call needs the per-launch secret in the `X-Panel-Token` header,
//!   compared in constant time. A query string is never read for it; the page
//!   receives the secret once in the URL fragment, which no server sees;
//! * no CORS header is ever emitted and `OPTIONS` is refused;
//! * request heads, bodies, chat text, queues and published state are bounded.

use super::server::{
    ConnGuard, HEAD_DEADLINE, MAX_CONNECTIONS, POLL, content_type, ct_eq, header_value, mint_token,
    peek_head, resolve,
};
use aivtuber_domain::{EngineError, EngineErrorKind};
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest accepted request body (the chat JSON object).
pub const MAX_PANEL_BODY_BYTES: usize = 4096;
/// Largest accepted chat text, in characters.
pub const MAX_PANEL_CHAT_CHARS: usize = 500;
/// Largest published state document; a bigger one is refused, not truncated.
pub const MAX_PANEL_STATE_BYTES: usize = 64 * 1024;
const CONTROL_QUEUE: usize = 4;
const CHAT_QUEUE: usize = 8;
const REPLY_WAIT: Duration = Duration::from_secs(3);
const TOKEN_HEADER: &str = "x-panel-token";
/// Authenticated API calls per second (burst, then sustained).
const API_BURST: f64 = 40.0;
const API_PER_SECOND: f64 = 20.0;
const PAGE: &str = "panel.html";

#[derive(Clone)]
pub struct LocalPanelConfig {
    /// TCP port on 127.0.0.1. `0` picks an ephemeral port (tests).
    pub port: u16,
    /// Directory holding `panel.html` and `assets/` (the renderer build).
    pub page_dir: Option<PathBuf>,
}

impl fmt::Debug for LocalPanelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalPanelConfig")
            .field("port", &self.port)
            .field("page_dir", &self.page_dir)
            .finish()
    }
}

/// What the page asked for. Typed: nothing here carries authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelRequest {
    Chat(String),
    Stop,
    Mute,
}

/// The composition thread's answer, relayed to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelReply {
    pub ok: bool,
    pub detail: String,
    /// The event id a chat line was given, so the page can pair the reply.
    pub id: Option<String>,
}

/// One queued request with the slot its answer goes to.
pub struct PanelCommand {
    pub request: PanelRequest,
    reply: SyncSender<PanelReply>,
}

impl PanelCommand {
    pub fn respond(self, ok: bool, detail: impl Into<String>) {
        self.send(ok, detail.into(), None);
    }

    pub fn respond_with_id(self, ok: bool, detail: impl Into<String>, id: impl Into<String>) {
        self.send(ok, detail.into(), Some(id.into()));
    }

    fn send(self, ok: bool, detail: String, id: Option<String>) {
        // The page may have given up; there is nobody to tell.
        let _ = self.reply.try_send(PanelReply { ok, detail, id });
    }
}

/// Counters of what the HTTP layer refused. No request content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PanelMetrics {
    pub accepted: u64,
    pub rejected_host: u64,
    pub rejected_origin: u64,
    pub rejected_auth: u64,
    pub rejected_request: u64,
    pub rate_limited: u64,
    pub queue_full: u64,
}

#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    rejected_host: AtomicU64,
    rejected_origin: AtomicU64,
    rejected_auth: AtomicU64,
    rejected_request: AtomicU64,
    rate_limited: AtomicU64,
    queue_full: AtomicU64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Shared {
    token: String,
    host: String,
    origin: String,
    page_dir: Option<PathBuf>,
    state: Mutex<String>,
    bucket: Mutex<Bucket>,
    control_tx: SyncSender<PanelCommand>,
    chat_tx: SyncSender<PanelCommand>,
    counters: Counters,
    shutdown: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn count(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn allow_api(&self) -> bool {
        let mut bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let refill = now.duration_since(bucket.last).as_secs_f64() * API_PER_SECOND;
        bucket.tokens = (bucket.tokens + refill).min(API_BURST);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The composition thread's end: drain requests, publish state.
pub struct PanelInbox {
    shared: Arc<Shared>,
    control_rx: Mutex<Receiver<PanelCommand>>,
    chat_rx: Mutex<Receiver<PanelCommand>>,
}

impl PanelInbox {
    /// Stop and mute requests. Their own queue: chat cannot fill it.
    pub fn drain_control(&self, max: usize) -> Vec<PanelCommand> {
        self.control_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    pub fn drain_chat(&self, max: usize) -> Vec<PanelCommand> {
        self.chat_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Replace the document `GET /api/state` serves. Refused (and `false`)
    /// above [`MAX_PANEL_STATE_BYTES`]; the previous document stays.
    pub fn publish_state(&self, json: String) -> bool {
        if json.len() > MAX_PANEL_STATE_BYTES {
            return false;
        }
        *self.shared.state.lock().unwrap_or_else(|e| e.into_inner()) = json;
        true
    }

    pub fn metrics(&self) -> PanelMetrics {
        let c = &self.shared.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        PanelMetrics {
            accepted: get(&c.accepted),
            rejected_host: get(&c.rejected_host),
            rejected_origin: get(&c.rejected_origin),
            rejected_auth: get(&c.rejected_auth),
            rejected_request: get(&c.rejected_request),
            rate_limited: get(&c.rate_limited),
            queue_full: get(&c.queue_full),
        }
    }
}

pub struct LocalPanelServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
}

impl fmt::Debug for LocalPanelServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalPanelServer")
            .field("addr", &self.addr)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl LocalPanelServer {
    /// Binds 127.0.0.1 only. There is deliberately no bind-address option.
    pub fn start(config: LocalPanelConfig) -> Result<(Self, PanelInbox), EngineError> {
        let io_err = |what: &str, error: std::io::Error| {
            EngineError::new(
                EngineErrorKind::Unavailable,
                format!("local panel {what}: {error}"),
            )
        };
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, config.port))
            .map_err(|e| io_err("bind", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| io_err("listener", e))?;
        let addr = listener.local_addr().map_err(|e| io_err("addr", e))?;
        let page_dir = match &config.page_dir {
            Some(dir) => Some(fs::canonicalize(dir).map_err(|e| io_err("page directory", e))?),
            None => None,
        };
        let (control_tx, control_rx) = sync_channel(CONTROL_QUEUE);
        let (chat_tx, chat_rx) = sync_channel(CHAT_QUEUE);
        let shared = Arc::new(Shared {
            token: mint_token()?,
            host: format!("127.0.0.1:{}", addr.port()),
            origin: format!("http://127.0.0.1:{}", addr.port()),
            page_dir,
            state: Mutex::new("{}".to_owned()),
            bucket: Mutex::new(Bucket {
                tokens: API_BURST,
                last: Instant::now(),
            }),
            control_tx,
            chat_tx,
            counters: Counters::default(),
            shutdown: std::sync::atomic::AtomicBool::new(false),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("local-panel-accept".to_owned())
                .spawn(move || accept_loop(&listener, &shared))
                .map_err(|e| io_err("thread", e))?
        };
        let inbox = PanelInbox {
            shared: Arc::clone(&shared),
            control_rx: Mutex::new(control_rx),
            chat_rx: Mutex::new(chat_rx),
        };
        Ok((
            Self {
                shared,
                addr,
                accept: Some(accept),
            },
            inbox,
        ))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The per-launch secret. Callers must not log it.
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// URL to show the operator. The secret is in the fragment, so it is not
    /// sent in any request line, log or `Referer`; the page keeps it in memory
    /// and strips it from the address bar. Show it, never log it.
    pub fn launch_url(&self) -> String {
        format!("{}/#token={}", self.shared.origin, self.shared.token)
    }
}

impl Drop for LocalPanelServer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    let active = Arc::new(AtomicUsize::new(0));
    while !shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    continue; // drop the socket
                }
                let guard = ConnGuard(Arc::clone(&active));
                let shared = Arc::clone(shared);
                let spawned = thread::Builder::new()
                    .name("local-panel-conn".to_owned())
                    .spawn(move || {
                        let _guard = guard;
                        handle_connection(stream, &shared);
                    });
                drop(spawned);
            }
            Err(_) => thread::sleep(POLL),
        }
    }
}

struct Response {
    status: &'static str,
    ctype: &'static str,
    body: Vec<u8>,
    csp: bool,
}

fn plain(status: &'static str) -> Response {
    Response {
        status,
        ctype: "text/plain; charset=utf-8",
        body: Vec::new(),
        csp: false,
    }
}

fn json(status: &'static str, ok: bool, detail: &str) -> Response {
    json_with_id(status, ok, detail, None)
}

fn json_with_id(status: &'static str, ok: bool, detail: &str, id: Option<&str>) -> Response {
    let body = serde_json::json!({ "ok": ok, "detail": detail, "id": id }).to_string();
    Response {
        status,
        ctype: "application/json",
        body: body.into_bytes(),
        csp: false,
    }
}

fn write_response(stream: &mut TcpStream, response: &Response, head_only: bool) {
    // No `Access-Control-*` header is ever written: cross-origin pages cannot
    // read a response, and the custom token header forces a preflight that
    // `OPTIONS` below refuses.
    let csp = if response.csp {
        "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
         connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Frame-Options: DENY\r\nCross-Origin-Resource-Policy: same-origin\r\n{csp}\r\n",
        response.status,
        response.ctype,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    if !head_only {
        let _ = stream.write_all(&response.body);
    }
    let _ = stream.flush();
}

fn handle_connection(mut stream: TcpStream, shared: &Arc<Shared>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(HEAD_DEADLINE));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Some((head_len, request_line)) = peek_head(&stream) else {
        return;
    };
    let mut head = vec![0_u8; head_len];
    if stream.read_exact(&mut head).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    // The query string is never interpreted: the secret cannot arrive there.
    let path = target.split('?').next().unwrap_or("");
    let head_only = method == "HEAD";
    let response = route(&mut stream, shared, &head, method, path);
    write_response(&mut stream, &response, head_only);
}

fn route(
    stream: &mut TcpStream,
    shared: &Shared,
    head: &[u8],
    method: &str,
    path: &str,
) -> Response {
    let header = |name: &str| header_value(head, name);
    // Host first, for every route: a rebinding page reaches this socket under
    // its own name, never under the loopback host:port.
    if header("host").as_deref() != Some(shared.host.as_str()) {
        shared.count(&shared.counters.rejected_host);
        return plain("403 Forbidden");
    }
    let is_api = path.starts_with("/api/");
    if !is_api {
        if method != "GET" && method != "HEAD" {
            return plain("405 Method Not Allowed");
        }
        return serve_page(shared, path);
    }
    // Content-Length and Transfer-Encoding are read once, before anything is
    // trusted, so every API response can state whether a body is pending.
    let content_length = header("content-length");
    let state_changing = matches!(
        (method, path),
        ("POST", "/api/chat" | "/api/stop" | "/api/mute")
    );
    let readable = method == "GET" && path == "/api/state";
    if !state_changing && !readable {
        // Includes OPTIONS (CORS preflight): refused without CORS headers.
        return if matches!(path, "/api/chat" | "/api/stop" | "/api/mute" | "/api/state") {
            plain("405 Method Not Allowed")
        } else {
            plain("404 Not Found")
        };
    }
    if state_changing && header("origin").as_deref() != Some(shared.origin.as_str()) {
        shared.count(&shared.counters.rejected_origin);
        return plain("403 Forbidden");
    }
    let authenticated =
        header(TOKEN_HEADER).is_some_and(|presented| ct_eq(&presented, &shared.token));
    if !authenticated {
        shared.count(&shared.counters.rejected_auth);
        return plain("401 Unauthorized");
    }
    if !shared.allow_api() {
        shared.count(&shared.counters.rate_limited);
        return json("429 Too Many Requests", false, "rate limited");
    }
    if readable {
        let state = shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        return Response {
            status: "200 OK",
            ctype: "application/json",
            body: state.into_bytes(),
            csp: false,
        };
    }
    // State-changing from here. Stop and mute take no body but must still be a
    // JSON POST so a plain form post cannot reach them.
    if !header("content-type")
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
    {
        shared.count(&shared.counters.rejected_request);
        return json(
            "415 Unsupported Media Type",
            false,
            "content type must be application/json",
        );
    }
    if header("transfer-encoding").is_some() {
        shared.count(&shared.counters.rejected_request);
        return json("400 Bad Request", false, "chunked bodies are not accepted");
    }
    let Some(length) = content_length
        .as_deref()
        .and_then(|v| v.parse::<usize>().ok())
    else {
        shared.count(&shared.counters.rejected_request);
        return json("411 Length Required", false, "content-length required");
    };
    if length > MAX_PANEL_BODY_BYTES {
        shared.count(&shared.counters.rejected_request);
        return json("413 Payload Too Large", false, "body too large");
    }
    let mut body = vec![0_u8; length];
    if stream.read_exact(&mut body).is_err() {
        shared.count(&shared.counters.rejected_request);
        return json("400 Bad Request", false, "incomplete body");
    }
    let (request, queue) = match path {
        "/api/stop" => (PanelRequest::Stop, &shared.control_tx),
        "/api/mute" => (PanelRequest::Mute, &shared.control_tx),
        _ => match chat_text(&body) {
            Ok(text) => (PanelRequest::Chat(text), &shared.chat_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
    };
    forward(shared, queue, request)
}

/// Parses `{"text": "..."}`; the text is trimmed and bounded.
fn chat_text(body: &[u8]) -> Result<String, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        text: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err(("400 Bad Request", "expected {\"text\": string}"));
    };
    let text = parsed.text.trim();
    if text.is_empty() {
        return Err(("400 Bad Request", "empty text"));
    }
    if text.chars().count() > MAX_PANEL_CHAT_CHARS {
        return Err(("413 Payload Too Large", "text too long"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(("400 Bad Request", "control characters are not accepted"));
    }
    Ok(text.to_owned())
}

fn forward(shared: &Shared, queue: &SyncSender<PanelCommand>, request: PanelRequest) -> Response {
    let (reply, answer) = sync_channel(1);
    match queue.try_send(PanelCommand { request, reply }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            shared.count(&shared.counters.queue_full);
            return json("503 Service Unavailable", false, "busy, try again");
        }
        Err(TrySendError::Disconnected(_)) => {
            return json("503 Service Unavailable", false, "runtime is shutting down");
        }
    }
    shared.count(&shared.counters.accepted);
    match answer.recv_timeout(REPLY_WAIT) {
        Ok(reply) => json_with_id(
            if reply.ok { "200 OK" } else { "409 Conflict" },
            reply.ok,
            &reply.detail,
            reply.id.as_deref(),
        ),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            json("504 Gateway Timeout", false, "no answer from the runtime")
        }
    }
}

/// Only the panel page and its bundled assets; nothing else in the directory
/// (the avatar page, a model) is reachable through this port.
fn serve_page(shared: &Shared, path: &str) -> Response {
    let relative = match path {
        "/" | "/panel.html" => PAGE,
        p if p.starts_with("/assets/") => p.trim_start_matches('/'),
        _ => return plain("404 Not Found"),
    };
    let Some(file) = resolve(shared.page_dir.as_deref(), relative) else {
        return plain("404 Not Found");
    };
    match fs::read(&file) {
        Ok(body) => Response {
            status: "200 OK",
            ctype: content_type(&file),
            body,
            csp: relative == PAGE,
        },
        Err(error) if error.kind() == ErrorKind::NotFound => plain("404 Not Found"),
        Err(_) => plain("404 Not Found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        server: LocalPanelServer,
        inbox: PanelInbox,
        host: String,
        origin: String,
    }

    fn fixture() -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("aivtuber-panel-test-{}", mint_token().unwrap()));
        fs::create_dir_all(dir.join("assets")).unwrap();
        fs::write(dir.join("panel.html"), "<html>panel</html>").unwrap();
        fs::write(dir.join("index.html"), "<html>avatar</html>").unwrap();
        fs::write(dir.join("assets").join("a.js"), "1").unwrap();
        let (server, inbox) = LocalPanelServer::start(LocalPanelConfig {
            port: 0,
            page_dir: Some(dir),
        })
        .unwrap();
        let port = server.local_addr().port();
        Fixture {
            server,
            inbox,
            host: format!("127.0.0.1:{port}"),
            origin: format!("http://127.0.0.1:{port}"),
        }
    }

    struct Raw {
        status: u16,
        head: String,
        body: String,
    }

    fn send(f: &Fixture, request: &str) -> Raw {
        let mut stream = TcpStream::connect(f.server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
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
        } = LocalPanelConfig {
            port: 0,
            page_dir: None,
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
                &format!(
                    "GET /api/state HTTP/1.1\r\nHost: {host}\r\nX-Panel-Token: {token}\r\n\r\n"
                ),
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
        let mut limited = 0;
        for _ in 0..(API_BURST as usize + 30) {
            let r = get(&f, "/api/state", &format!("X-Panel-Token: {token}\r\n"));
            if r.status == 429 {
                limited += 1;
            }
        }
        assert!(limited > 0);
        assert!(f.inbox.metrics().rate_limited > 0);
    }
}
