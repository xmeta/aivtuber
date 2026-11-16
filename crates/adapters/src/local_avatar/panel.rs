//! Local operation panel: a small loopback HTTP surface next to the avatar
//! renderer server (issue #335, docs/security-threat-model.adoc section 15).
//!
//! It reuses the renderer server's HTTP plumbing (head peeking, constant-time
//! compare, entropy token) and adds only a static page and a few JSON
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
//! * request heads, bodies, chat text, queues and published state are bounded;
//! * the voice settings (#360) add `/api/voice*`: the typed patch, the API key
//!   (a body field, held in a redacted type, never echoed), key removal and
//!   check, the preview, and a model search that this thread proxies to the
//!   public Aivis Cloud list (so the page stays same-origin) under a deadline,
//!   one at a time, with a bounded typed answer. The voice requests have their
//!   own queue so a slider drag can never delay stop or mute;
//! * the stream-chat connections (#361) add `/api/ingress*`: connect or
//!   disconnect one platform (a strictly parsed Twitch login or YouTube video,
//!   both normalized here once), and the YouTube API key (the same redacted
//!   body field as the Aivis key). They have their own queue too, and nothing
//!   in the request can name a program, an argument, a trust level or a source
//!   class: the composition thread chooses all of those;
//! * the avatar choice (#362) adds `/api/avatar/*`: the model file upload (the
//!   one body that is not small JSON: raw bytes, `application/octet-stream`,
//!   declared length at most 64 MiB and checked before anything is written,
//!   streamed to a staging file under a name this thread generates, hashed as
//!   it arrives and inspected as a VRM before the composition thread is even
//!   asked; one upload at a time) and the confirm / select / delete / discard
//!   requests, each naming a model by its content hash only. They have their
//!   own queue. The client never supplies a path or a file name.

use super::server::{
    ConnGuard, HEAD_DEADLINE, MAX_CONNECTIONS, POLL, content_type, ct_eq, header_value, mint_token,
    peek_head, resolve,
};
use super::vrm::{self, VrmError};
use crate::speech_tts::aivis_models::{self, ModelSearchQuery, ModelSort};
use aivtuber_domain::{EngineError, EngineErrorKind};
use sha2::{Digest, Sha256};
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
const VOICE_QUEUE: usize = 4;
const INGRESS_QUEUE: usize = 4;
const AVATAR_QUEUE: usize = 4;
const CHAT_QUEUE: usize = 8;
/// Longest accepted API key, in characters (Aivis keys are about 40).
pub const MAX_PANEL_KEY_CHARS: usize = 256;
/// One model search may take this long, end to end.
const SEARCH_DEADLINE: Duration = Duration::from_secs(6);
/// Every state-changing endpoint (all are `POST`).
const STATE_CHANGING: [&str; 17] = [
    "/api/chat",
    "/api/stop",
    "/api/mute",
    "/api/model",
    "/api/voice",
    "/api/voice/key",
    "/api/voice/key/remove",
    "/api/voice/key/check",
    "/api/voice/preview",
    "/api/ingress/connect",
    "/api/ingress/disconnect",
    "/api/ingress/key",
    "/api/ingress/key/remove",
    "/api/avatar/confirm",
    "/api/avatar/select",
    "/api/avatar/delete",
    "/api/avatar/discard",
];
const SEARCH_PATH: &str = "/api/voice/search";
/// The model upload (#362): a raw body, so it is not in [`STATE_CHANGING`]'s
/// small-JSON handling; it exists only when a staging directory is configured.
const UPLOAD_PATH: &str = "/api/avatar/upload";
/// An upload that sends nothing for this long is dropped.
const UPLOAD_IDLE: Duration = Duration::from_secs(10);
/// An upload that is still running after this long is dropped.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(120);
const UPLOAD_CHUNK: usize = 64 * 1024;
/// How long an unread upload body is drained after a refusal, at most, and how
/// long a silent client is waited for.
const DRAIN_DEADLINE: Duration = Duration::from_secs(3);
const DRAIN_IDLE: Duration = Duration::from_millis(100);
/// Most files cleared from the staging directory before an upload.
const STAGING_SWEEP_LIMIT: usize = 64;
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
    /// Origin of the Aivis Cloud API the model search is proxied to (#360).
    /// `None` leaves the search route off (404).
    pub voice_search_endpoint: Option<String>,
    /// Directory the avatar upload is staged in (#362), owned by the app's
    /// avatar store. `None` leaves the upload route off (404). Everything in it
    /// is deleted before each upload.
    pub avatar_staging_dir: Option<PathBuf>,
}

impl fmt::Debug for LocalPanelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalPanelConfig")
            .field("port", &self.port)
            .field("page_dir", &self.page_dir)
            .field("voice_search_endpoint", &self.voice_search_endpoint)
            .field("avatar_staging_dir", &self.avatar_staging_dir)
            .finish()
    }
}

/// A secret the page sent (the Aivis API key). Redacted in `Debug`, compared
/// by value for tests only; the one reader is [`Self::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct PanelSecret(String);

impl PanelSecret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PanelSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PanelSecret([REDACTED])")
    }
}

/// What the page asked for. Typed: nothing here carries authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelRequest {
    Chat(String),
    Stop,
    Mute,
    /// Switch the local Thinking model (#357). Only the alias, already checked
    /// for shape; whether it is allowed is decided on the composition thread.
    SelectModel(String),
    /// A voice settings patch (#360), the JSON text of a `VoicePatch` that the
    /// HTTP layer already parsed once. The composition thread decides.
    VoiceApply(String),
    /// Store the Aivis API key (#360).
    VoiceKeySet(PanelSecret),
    VoiceKeyRemove,
    VoiceKeyCheck,
    /// Speak the fixed preview sentence (#360).
    VoicePreview,
    /// Start reading one platform's chat (#361). `target` is already the
    /// normalized Twitch login or YouTube video id; the composition thread
    /// parses it again before it spawns anything.
    IngressConnect {
        platform: IngressPlatform,
        target: String,
    },
    IngressDisconnect(IngressPlatform),
    /// Store the YouTube Data API key (#361).
    IngressKeySet(PanelSecret),
    IngressKeyRemove,
    /// A model file was uploaded and staged (#362). Only the SHA-256 (64
    /// lowercase hex) of the staged file, which is named after it in the
    /// staging directory; the HTTP layer already checked it is a VRM.
    AvatarUpload(String),
    /// Make the pending candidate with this hash the active model (#362).
    AvatarConfirm(String),
    /// Switch to a stored model by hash, or to the sample (`sample`).
    AvatarSelect(String),
    /// Delete a stored model by hash.
    AvatarDelete(String),
    /// Drop the pending candidate.
    AvatarDiscard,
}

/// The two chat platforms the panel can connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressPlatform {
    Twitch,
    Youtube,
}

impl IngressPlatform {
    pub fn name(self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::Youtube => "youtube",
        }
    }
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
    voice_tx: SyncSender<PanelCommand>,
    ingress_tx: SyncSender<PanelCommand>,
    avatar_tx: SyncSender<PanelCommand>,
    chat_tx: SyncSender<PanelCommand>,
    voice_search_endpoint: Option<String>,
    search_busy: std::sync::atomic::AtomicBool,
    avatar_staging_dir: Option<PathBuf>,
    upload_busy: std::sync::atomic::AtomicBool,
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
    voice_rx: Mutex<Receiver<PanelCommand>>,
    ingress_rx: Mutex<Receiver<PanelCommand>>,
    avatar_rx: Mutex<Receiver<PanelCommand>>,
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

    /// Voice settings requests (#360). Their own queue, so they can never sit
    /// in front of a stop or a mute.
    pub fn drain_voice(&self, max: usize) -> Vec<PanelCommand> {
        self.voice_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Stream-chat connection requests (#361). Their own queue, so a connect
    /// can never sit in front of a stop or a mute.
    pub fn drain_ingress(&self, max: usize) -> Vec<PanelCommand> {
        self.ingress_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Avatar choice requests (#362). Their own queue, so a model upload or
    /// switch can never sit in front of a stop or a mute.
    pub fn drain_avatar(&self, max: usize) -> Vec<PanelCommand> {
        self.avatar_rx
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
        let (voice_tx, voice_rx) = sync_channel(VOICE_QUEUE);
        let (ingress_tx, ingress_rx) = sync_channel(INGRESS_QUEUE);
        let (avatar_tx, avatar_rx) = sync_channel(AVATAR_QUEUE);
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
            voice_tx,
            ingress_tx,
            avatar_tx,
            chat_tx,
            voice_search_endpoint: config.voice_search_endpoint.clone(),
            search_busy: std::sync::atomic::AtomicBool::new(false),
            avatar_staging_dir: config.avatar_staging_dir.clone(),
            upload_busy: std::sync::atomic::AtomicBool::new(false),
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
            voice_rx: Mutex::new(voice_rx),
            ingress_rx: Mutex::new(ingress_rx),
            avatar_rx: Mutex::new(avatar_rx),
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
    if method == "POST" && path == UPLOAD_PATH {
        discard_unread(&mut stream);
    }
}

/// After the answer to an upload that was refused early, reads (and drops) what
/// the client is still sending, bounded in bytes and time, so closing the socket
/// does not reset the connection under the answer the client has not read yet.
/// The write side is closed first: the client sees the whole answer and the end
/// of it, then its own body drains into nothing.
fn discard_unread(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(DRAIN_IDLE));
    let started = Instant::now();
    let mut total = 0_u64;
    let mut sink = vec![0_u8; UPLOAD_CHUNK];
    while total <= vrm::MAX_VRM_BYTES && started.elapsed() < DRAIN_DEADLINE {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n as u64,
        }
    }
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
    // The search is a POST like the rest (a body, an Origin check) but is
    // answered here, not by the composition thread, and only when configured.
    let searching =
        method == "POST" && path == SEARCH_PATH && shared.voice_search_endpoint.is_some();
    let uploading = method == "POST" && path == UPLOAD_PATH && shared.avatar_staging_dir.is_some();
    let state_changing =
        searching || uploading || (method == "POST" && STATE_CHANGING.contains(&path));
    let readable = method == "GET" && path == "/api/state";
    if !state_changing && !readable {
        // Includes OPTIONS (CORS preflight): refused without CORS headers.
        let known = path == "/api/state"
            || STATE_CHANGING.contains(&path)
            || (path == UPLOAD_PATH && shared.avatar_staging_dir.is_some())
            || (path == SEARCH_PATH && shared.voice_search_endpoint.is_some());
        return if known {
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
    if uploading {
        return upload(stream, shared, &header);
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
    if searching {
        return search(shared, &body);
    }
    let (request, queue) = match path {
        "/api/stop" => (PanelRequest::Stop, &shared.control_tx),
        "/api/mute" => (PanelRequest::Mute, &shared.control_tx),
        "/api/voice/preview" => (PanelRequest::VoicePreview, &shared.voice_tx),
        "/api/voice/key/remove" => (PanelRequest::VoiceKeyRemove, &shared.voice_tx),
        "/api/voice/key/check" => (PanelRequest::VoiceKeyCheck, &shared.voice_tx),
        "/api/ingress/key/remove" => (PanelRequest::IngressKeyRemove, &shared.ingress_tx),
        "/api/avatar/discard" => match empty_object(&body) {
            Ok(()) => (PanelRequest::AvatarDiscard, &shared.avatar_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/avatar/confirm" | "/api/avatar/select" | "/api/avatar/delete" => {
            match avatar_id(&body) {
                Ok(id) => (
                    match path {
                        "/api/avatar/confirm" => PanelRequest::AvatarConfirm(id),
                        "/api/avatar/select" => PanelRequest::AvatarSelect(id),
                        _ => PanelRequest::AvatarDelete(id),
                    },
                    &shared.avatar_tx,
                ),
                Err((status, detail)) => {
                    shared.count(&shared.counters.rejected_request);
                    return json(status, false, detail);
                }
            }
        }
        "/api/ingress/key" => match key_body(&body) {
            Ok(key) => (PanelRequest::IngressKeySet(key), &shared.ingress_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/ingress/connect" => match ingress_connect(&body) {
            Ok(request) => (request, &shared.ingress_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/ingress/disconnect" => match ingress_disconnect(&body) {
            Ok(request) => (request, &shared.ingress_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/voice/key" => match key_body(&body) {
            Ok(key) => (PanelRequest::VoiceKeySet(key), &shared.voice_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/voice" => match voice_patch(&body) {
            Ok(patch) => (PanelRequest::VoiceApply(patch), &shared.voice_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/model" => match model_alias(&body) {
            Ok(alias) => (PanelRequest::SelectModel(alias), &shared.control_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
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

/// Parses `{}`: the discard request names nothing.
fn empty_object(body: &[u8]) -> Result<(), (&'static str, &'static str)> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(map)) if map.is_empty() => Ok(()),
        _ => Err(("400 Bad Request", "expected {}")),
    }
}

/// Parses `{"id": "sample" | <hash or hash prefix>}`. An avatar model is named
/// by content hash only (8 to 64 lowercase hex characters) or `sample`: no
/// path, no file name, nothing the file system could resolve differently.
fn avatar_id(body: &[u8]) -> Result<String, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        id: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err(("400 Bad Request", "expected {\"id\": string}"));
    };
    if parsed.id == "sample" || is_hash_prefix(&parsed.id) {
        Ok(parsed.id)
    } else {
        Err(("400 Bad Request", "not a model id"))
    }
}

/// 8 to 64 lowercase hex characters: a content hash or a prefix of one.
pub fn is_hash_prefix(text: &str) -> bool {
    (8..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Clears the upload-in-progress flag however the upload ends.
struct UploadGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn upload_error(error: VrmError) -> (&'static str, String) {
    let status = match error {
        VrmError::TooLarge => "413 Payload Too Large",
        VrmError::NotGlb
        | VrmError::UnsupportedVersion
        | VrmError::NotVrm
        | VrmError::MissingMeta => "415 Unsupported Media Type",
        _ => "400 Bad Request",
    };
    (status, format!("{}: {error}", error.code()))
}

/// `POST /api/avatar/upload` (#362): the raw bytes of one model.
///
/// Order matters: the declared length is checked before anything is read or
/// written, the first twelve bytes are checked against it before the file is
/// created, the body is read in bounded chunks into a staging file whose name
/// this function made up (a random name, then the content hash) and hashed as it
/// arrives (never held whole), and the finished file is inspected as a VRM
/// before the composition thread is asked to take it. Any failure removes the
/// staging file.
fn upload(
    stream: &mut TcpStream,
    shared: &Shared,
    head_header: &dyn Fn(&str) -> Option<String>,
) -> Response {
    let reject = |status: &'static str, detail: &str| {
        shared.count(&shared.counters.rejected_request);
        json(status, false, detail)
    };
    let octet_stream = head_header("content-type").is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/octet-stream"))
    });
    if !octet_stream {
        return reject(
            "415 Unsupported Media Type",
            "content type must be application/octet-stream",
        );
    }
    if head_header("transfer-encoding").is_some() {
        return reject("400 Bad Request", "chunked bodies are not accepted");
    }
    let Some(length) = head_header("content-length").and_then(|v| v.parse::<u64>().ok()) else {
        return reject("411 Length Required", "content-length required");
    };
    let Some(dir) = shared.avatar_staging_dir.as_deref() else {
        return plain("404 Not Found");
    };
    // Before a single byte of the body is read.
    if length > vrm::MAX_VRM_BYTES {
        let (status, detail) = upload_error(VrmError::TooLarge);
        return reject(status, &detail);
    }
    if shared
        .upload_busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        shared.count(&shared.counters.rate_limited);
        return json(
            "429 Too Many Requests",
            false,
            "an upload is already running",
        );
    }
    let _busy = UploadGuard(&shared.upload_busy);
    let mut prefix = [0_u8; 12];
    if length < 20 {
        let (status, detail) = upload_error(VrmError::Truncated);
        return reject(status, &detail);
    }
    let _ = stream.set_read_timeout(Some(UPLOAD_IDLE));
    if stream.read_exact(&mut prefix).is_err() {
        return reject("400 Bad Request", "incomplete body");
    }
    if let Err(error) = vrm::check_prefix(&prefix, length) {
        let (status, detail) = upload_error(error);
        return reject(status, &detail);
    }
    shared.count(&shared.counters.accepted);
    match stage_upload(stream, dir, &prefix, length) {
        Ok(hash) => {
            let response = forward(
                shared,
                &shared.avatar_tx,
                PanelRequest::AvatarUpload(hash.clone()),
            );
            if !response.status.starts_with("200") {
                // Refused or never picked up: do not leave the file behind.
                let _ = fs::remove_file(dir.join(format!("{hash}.vrm")));
            }
            response
        }
        Err((status, detail)) => reject(status, &detail),
    }
}

/// Streams the rest of the body (after `prefix`) into the staging directory,
/// returning the SHA-256 of the staged, inspected file, which is renamed to
/// `<hash>.vrm`.
fn stage_upload(
    stream: &mut TcpStream,
    dir: &std::path::Path,
    prefix: &[u8; 12],
    length: u64,
) -> Result<String, (&'static str, String)> {
    let io_error = |_: std::io::Error| {
        (
            "500 Internal Server Error",
            "could not stage the file".to_owned(),
        )
    };
    // One upload at a time, so the directory only ever holds this one.
    sweep(dir);
    fs::create_dir_all(dir).map_err(io_error)?;
    let temp = dir.join(format!(
        "up-{}.part",
        mint_token()
            .map_err(|_| ("500 Internal Server Error", "no entropy".to_owned()))?
            .get(..16)
            .unwrap_or("0")
    ));
    let result = (|| {
        let mut file = fs::File::create(&temp).map_err(io_error)?;
        let mut hasher = Sha256::new();
        file.write_all(prefix).map_err(io_error)?;
        hasher.update(prefix);
        let started = Instant::now();
        let mut remaining = length - prefix.len() as u64;
        let mut chunk = vec![0_u8; UPLOAD_CHUNK];
        while remaining > 0 {
            if started.elapsed() > UPLOAD_DEADLINE {
                return Err(("408 Request Timeout", "the upload took too long".to_owned()));
            }
            let want = chunk
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            match stream.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => {
                    return Err(("400 Bad Request", "incomplete body".to_owned()));
                }
                Ok(n) => {
                    file.write_all(&chunk[..n]).map_err(io_error)?;
                    hasher.update(&chunk[..n]);
                    remaining -= n as u64;
                }
            }
        }
        file.flush().map_err(io_error)?;
        drop(file);
        vrm::inspect_file(&temp).map_err(upload_error)?;
        let hash: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let staged = dir.join(format!("{hash}.vrm"));
        let _ = fs::remove_file(&staged);
        fs::rename(&temp, &staged).map_err(io_error)?;
        Ok(hash)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Deletes what an earlier upload left in the staging directory (bounded).
fn sweep(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(STAGING_SWEEP_LIMIT) {
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The body of `POST /api/voice`: a `VoicePatch`, parsed here once for the
/// status code and forwarded as the text the composition thread parses again.
fn voice_patch(body: &[u8]) -> Result<String, (&'static str, &'static str)> {
    let Ok(text) = std::str::from_utf8(body) else {
        return Err(("400 Bad Request", "not a voice settings request"));
    };
    match aivtuber_domain::VoicePatch::parse(text) {
        Ok(_) => Ok(text.to_owned()),
        Err(aivtuber_domain::VoiceSettingsError::PatchTooLarge) => {
            Err(("413 Payload Too Large", "request too large"))
        }
        Err(_) => Err(("400 Bad Request", "not a voice settings request")),
    }
}

fn ingress_platform(name: &str) -> Option<IngressPlatform> {
    match name {
        "twitch" => Some(IngressPlatform::Twitch),
        "youtube" => Some(IngressPlatform::Youtube),
        _ => None,
    }
}

/// Parses `{"platform": "twitch"|"youtube", "target": "..."}`. The target is
/// parsed strictly here (a login, a video id, or a watch/live/short link) and
/// only the normalized value goes on.
fn ingress_connect(body: &[u8]) -> Result<PanelRequest, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        platform: String,
        target: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err((
            "400 Bad Request",
            "expected {\"platform\": string, \"target\": string}",
        ));
    };
    let Some(platform) = ingress_platform(&parsed.platform) else {
        return Err(("400 Bad Request", "platform must be twitch or youtube"));
    };
    let target = match platform {
        IngressPlatform::Twitch => crate::parse_twitch_login(&parsed.target),
        IngressPlatform::Youtube => crate::parse_youtube_video(&parsed.target),
    }
    .map_err(|detail| ("400 Bad Request", detail))?;
    Ok(PanelRequest::IngressConnect { platform, target })
}

/// Parses `{"platform": "twitch"|"youtube"}`.
fn ingress_disconnect(body: &[u8]) -> Result<PanelRequest, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        platform: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err(("400 Bad Request", "expected {\"platform\": string}"));
    };
    ingress_platform(&parsed.platform)
        .map(PanelRequest::IngressDisconnect)
        .ok_or(("400 Bad Request", "platform must be twitch or youtube"))
}

/// Parses `{"key": "..."}`. The key is trimmed and bounded and must have no
/// whitespace inside or control characters; it never appears in a response.
fn key_body(body: &[u8]) -> Result<PanelSecret, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        key: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err(("400 Bad Request", "expected {\"key\": string}"));
    };
    let key = parsed.key.trim();
    if key.is_empty() || key.chars().count() > MAX_PANEL_KEY_CHARS {
        return Err(("400 Bad Request", "the key is empty or too long"));
    }
    if key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err((
            "400 Bad Request",
            "the key contains whitespace or control characters",
        ));
    }
    Ok(PanelSecret(key.to_owned()))
}

/// Parses `{"keyword": "...", "sort": "...", "page": n}` (all optional).
fn search_query(body: &[u8]) -> Result<ModelSearchQuery, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        #[serde(default)]
        keyword: String,
        #[serde(default = "default_sort")]
        sort: ModelSort,
        #[serde(default = "default_page")]
        page: u32,
    }
    fn default_sort() -> ModelSort {
        ModelSort::Download
    }
    fn default_page() -> u32 {
        1
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err((
            "400 Bad Request",
            "expected {\"keyword\", \"sort\", \"page\"}",
        ));
    };
    ModelSearchQuery::new(&parsed.keyword, parsed.sort, parsed.page)
        .map_err(|_| ("400 Bad Request", "not a valid search"))
}

/// The model search, answered on this connection's own thread: one at a time
/// (a second one is refused, not queued), under [`SEARCH_DEADLINE`], and the
/// answer is the typed page, never the service's body.
fn search(shared: &Shared, body: &[u8]) -> Response {
    let query = match search_query(body) {
        Ok(query) => query,
        Err((status, detail)) => {
            shared.count(&shared.counters.rejected_request);
            return json(status, false, detail);
        }
    };
    let Some(endpoint) = shared.voice_search_endpoint.as_deref() else {
        return plain("404 Not Found");
    };
    if shared
        .search_busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        shared.count(&shared.counters.rate_limited);
        return json(
            "429 Too Many Requests",
            false,
            "a search is already running",
        );
    }
    shared.count(&shared.counters.accepted);
    let result = aivis_models::search_models(endpoint, &query, SEARCH_DEADLINE);
    shared.search_busy.store(false, Ordering::Release);
    match result {
        Ok(page) => Response {
            status: "200 OK",
            ctype: "application/json",
            body: serde_json::json!({ "ok": true, "detail": "", "id": null, "page": page })
                .to_string()
                .into_bytes(),
            csp: false,
        },
        Err(error) => json("502 Bad Gateway", false, &error.to_string()),
    }
}

/// Parses `{"model": "..."}`. Only the shape of the alias is checked here
/// (bounded, a-z 0-9 . _ -); the allowlist lives with the composition thread.
fn model_alias(body: &[u8]) -> Result<String, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        model: String,
    }
    let Ok(parsed) = serde_json::from_slice::<Body>(body) else {
        return Err(("400 Bad Request", "expected {\"model\": string}"));
    };
    if !aivtuber_domain::is_valid_model_alias(&parsed.model) {
        return Err(("400 Bad Request", "not a model alias"));
    }
    Ok(parsed.model)
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
        staging: PathBuf,
    }

    fn fixture() -> Fixture {
        fixture_with_search(None)
    }

    fn fixture_with_search(search_endpoint: Option<String>) -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("aivtuber-panel-test-{}", mint_token().unwrap()));
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
        let long = format!("{{\"model\":\"{}\"}}", "a".repeat(65));
        for body in [
            "",
            "{}",
            "{\"model\":\"C:\\x.gguf\"}",
            "{\"model\":\"../x\"}",
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
                include_bytes!("../../tests/fixtures/aivis-model-search.json").to_vec(),
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

    fn post_bytes(
        f: &Fixture,
        headers: &[(&str, &str)],
        declared: Option<u64>,
        body: &[u8],
    ) -> Raw {
        let mut request =
            format!("POST {UPLOAD_PATH} HTTP/1.1\r\nHost: {}\r\n", f.host).into_bytes();
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
}
