//! Loopback listener: static page directory plus one WebSocket endpoint.
//!
//! Runs on its own threads, outside the frame/motor loop. Plain `std::net`
//! plus the already-vendored `tungstenite`; no async runtime.

use super::protocol::{HostFrame, MAX_FRAME_BYTES, RendererFrame, WireCommand};
use super::{
    DropReason, LocalAvatarConfig, LocalAvatarHandle, LocalAvatarState, MAX_IN_FLIGHT, Shared,
};
use aivtuber_domain::{EngineError, EngineErrorKind};
use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Error as WsError, Message, WebSocket, http};

const MAX_HEAD_BYTES: usize = 8192;
pub(crate) const MAX_CONNECTIONS: usize = 16;
const MAX_STATIC_BYTES: u64 = 64 * 1024 * 1024;
/// The active VRM model, token required (#362).
pub(crate) const ACTIVE_MODEL_PATH: &str = "/avatar/model.vrm";
/// The active model's id, token required (#362).
pub(crate) const ACTIVE_MODEL_ID_PATH: &str = "/avatar/active";
pub(crate) const HEAD_DEADLINE: Duration = Duration::from_secs(2);
pub(crate) const POLL: Duration = Duration::from_millis(10);

pub struct LocalAvatarServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
}

impl fmt::Debug for LocalAvatarServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalAvatarServer")
            .field("addr", &self.addr)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl LocalAvatarServer {
    /// Binds 127.0.0.1 only and starts serving. There is deliberately no
    /// bind-address option: remote exposure is out of scope for this adapter.
    pub fn start(config: LocalAvatarConfig) -> Result<Self, EngineError> {
        let io_err = |what: &str, error: std::io::Error| {
            EngineError::new(
                EngineErrorKind::Unavailable,
                format!("local avatar {what}: {error}"),
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
        let mut config = config;
        config.page_dir = page_dir;
        let shared = Arc::new(Shared::new(config, mint_token()?, addr.port()));
        let accept = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("local-avatar-accept".to_owned())
                .spawn(move || accept_loop(&listener, &shared))
                .map_err(|e| io_err("thread", e))?
        };
        Ok(Self {
            shared,
            addr,
            accept: Some(accept),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn handle(&self) -> LocalAvatarHandle {
        LocalAvatarHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The per-launch secret. Callers must not log it.
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// URL to give to the OBS Browser Source / local browser. Contains the
    /// secret: show it to the operator, never log it.
    pub fn launch_url(&self) -> String {
        format!("{}/?token={}", self.shared.origin, self.shared.token)
    }
}

impl Drop for LocalAvatarServer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

pub(crate) fn mint_token() -> Result<String, EngineError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        EngineError::new(
            EngineErrorKind::Unavailable,
            format!("OS entropy unavailable for avatar token: {error}"),
        )
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

pub(crate) struct ConnGuard(pub(crate) Arc<AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
                    .name("local-avatar-conn".to_owned())
                    .spawn(move || {
                        let _guard = guard;
                        handle_connection(stream, &shared);
                    });
                drop(spawned);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => thread::sleep(POLL),
        }
    }
}

fn handle_connection(stream: TcpStream, shared: &Arc<Shared>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(HEAD_DEADLINE));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Some((head_len, request_line)) = peek_head(&stream) else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/ws" {
        websocket(stream, shared);
    } else {
        // Consume exactly the head we peeked, then answer statically.
        let mut sink = vec![0_u8; head_len];
        let mut stream = stream;
        if stream.read_exact(&mut sink).is_ok() {
            let host_ok = header_value(&sink, "host").is_some_and(|h| h == shared.host);
            serve_static(&mut stream, shared, method, path, query, host_ok);
        }
    }
}

/// Peeks (without consuming) until the head terminator; returns its length
/// and the request line.
pub(crate) fn peek_head(stream: &TcpStream) -> Option<(usize, String)> {
    let deadline = Instant::now() + HEAD_DEADLINE;
    let mut buf = vec![0_u8; MAX_HEAD_BYTES];
    let mut last = 0;
    loop {
        let n = stream.peek(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(pos) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            let line_end = buf[..n].windows(2).position(|w| w == b"\r\n")?;
            let line = String::from_utf8_lossy(&buf[..line_end]).into_owned();
            return Some((pos + 4, line));
        }
        if n == buf.len() || Instant::now() > deadline {
            return None;
        }
        if n == last {
            thread::sleep(Duration::from_millis(5));
        }
        last = n;
    }
}

pub(crate) fn header_value(head: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    text.lines().skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], head_only: bool) {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    if !head_only {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

fn serve_static(
    stream: &mut TcpStream,
    shared: &Shared,
    method: &str,
    path: &str,
    query: &str,
    host_ok: bool,
) {
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        return respond(stream, "405 Method Not Allowed", "text/plain", b"", false);
    }
    if !host_ok {
        return respond(stream, "403 Forbidden", "text/plain", b"", head_only);
    }
    // The operation panel (#335) is a second page of the same build, served
    // only by its own server: this one serves the avatar entry and nothing of
    // the panel's.
    if path == "/panel.html" || path.starts_with("/assets/panel-") {
        return respond(stream, "404 Not Found", "text/plain", b"", head_only);
    }
    if path == ACTIVE_MODEL_PATH || path == ACTIVE_MODEL_ID_PATH {
        return serve_active_model(stream, shared, path, query, head_only);
    }
    let Some(file) = resolve(shared.config.page_dir.as_deref(), path) else {
        return respond(stream, "404 Not Found", "text/plain", b"", head_only);
    };
    match fs::read(&file) {
        Ok(body) => {
            let ctype = content_type(&file);
            let csp = page_csp(shared);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
                 Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
                 Content-Security-Policy: {csp}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            if !head_only {
                let _ = stream.write_all(&body);
            }
            let _ = stream.flush();
        }
        Err(_) => respond(stream, "404 Not Found", "text/plain", b"", head_only),
    }
}

/// The page may talk to nothing but this origin (no CDN, no third-party
/// egress). `blob:` in connect-src is required because three.js decodes
/// embedded model textures by fetching a blob: URL the page itself created from
/// the model bytes; it names no other host (observed under the CSP in the S3
/// smoke test).
fn page_csp(shared: &Shared) -> String {
    format!(
        "default-src 'self'; connect-src 'self' blob: ws://{}; img-src 'self' data: blob:; \
         script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'",
        shared.host
    )
}

/// The active VRM model (#362), served only to a caller that presents the
/// per-launch secret (the avatar page already holds it): the model bytes at
/// [`ACTIVE_MODEL_PATH`] with their id in a header, and the bare id at
/// [`ACTIVE_MODEL_ID_PATH`] for the page's reload poll. The file is chosen by
/// the composition root, never by the request.
fn serve_active_model(
    stream: &mut TcpStream,
    shared: &Shared,
    path: &str,
    query: &str,
    head_only: bool,
) {
    let token_ok = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .is_some_and(|t| ct_eq(t, &shared.token));
    if !token_ok {
        return respond(stream, "403 Forbidden", "text/plain", b"", head_only);
    }
    // One lock: the id and the file always belong together.
    let (file, id) = {
        let model = shared.model.lock().unwrap_or_else(|e| e.into_inner());
        // `fallback=sample`: the page could not decode the active model and
        // asks for the shipped sample, still labelled with the active id so it
        // does not mistake the fallback for a change and reload in a loop.
        let sample = query.split('&').any(|kv| kv == "fallback=sample");
        let file = match &model.path {
            Some(path) if !sample => Some(path.clone()),
            _ => resolve(shared.config.page_dir.as_deref(), "model.vrm"),
        };
        (file, model.id.clone())
    };
    if path == ACTIVE_MODEL_ID_PATH {
        let body = serde_json::json!({ "id": id }).to_string();
        return respond(
            stream,
            "200 OK",
            "application/json",
            body.as_bytes(),
            head_only,
        );
    }
    let opened = file.and_then(|file| {
        let handle = fs::File::open(file).ok()?;
        let len = handle.metadata().ok()?.len();
        (len <= MAX_STATIC_BYTES).then_some((handle, len))
    });
    let Some((handle, len)) = opened else {
        return respond(stream, "404 Not Found", "text/plain", b"", head_only);
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: model/gltf-binary\r\nContent-Length: {len}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Avatar-Model-Id: {id}\r\nContent-Security-Policy: {}\r\n\r\n",
        page_csp(shared)
    );
    let _ = stream.write_all(header.as_bytes());
    if !head_only {
        // Streamed, not read whole: the file may be 64 MiB.
        let _ = std::io::copy(&mut handle.take(len), stream);
    }
    let _ = stream.flush();
}

/// Maps a URL path to a file strictly inside the page directory.
pub(crate) fn resolve(root: Option<&Path>, path: &str) -> Option<PathBuf> {
    let root = root?;
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if !rel
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/'))
    {
        return None;
    }
    let rel = Path::new(rel);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = fs::canonicalize(root.join(rel)).ok()?;
    let meta = fs::metadata(&full).ok()?;
    (full.starts_with(root) && meta.is_file() && meta.len() <= MAX_STATIC_BYTES).then_some(full)
}

pub(crate) fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "vrm" | "glb" => "model/gltf-binary",
        _ => "application/octet-stream",
    }
}

fn deny(status: u16) -> ErrorResponse {
    http::Response::builder()
        .status(status)
        .body(None)
        .expect("static error response")
}

fn websocket(stream: TcpStream, shared: &Arc<Shared>) {
    let claimed = Cell::new(false);
    // The closure's `Result` type is dictated by tungstenite's `Callback`.
    #[allow(clippy::result_large_err)]
    let callback = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let headers = request.headers();
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let token_ok = request
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
            .is_some_and(|t| ct_eq(t, &shared.token));
        // Order matters: authenticate first so an unauthenticated peer cannot
        // probe whether a renderer is connected (409 vs 403).
        if text("origin") != Some(shared.origin.as_str())
            || text("host") != Some(shared.host.as_str())
            || !token_ok
        {
            return Err(deny(403));
        }
        let mut inner = shared.lock();
        if inner.session_active {
            return Err(deny(409));
        }
        inner.session_active = true;
        claimed.set(true);
        Ok(response)
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES));
    match tungstenite::accept_hdr_with_config(stream, callback, Some(config)) {
        Ok(ws) => run_session(shared, ws),
        Err(_) => {
            if claimed.get() {
                shared.lock().session_active = false;
            }
        }
    }
}

struct Session {
    epoch: Option<String>,
    ready: bool,
    seq: u64,
    in_flight: VecDeque<u64>,
}

fn run_session(shared: &Arc<Shared>, mut ws: WebSocket<TcpStream>) {
    let _ = ws.get_mut().set_read_timeout(Some(POLL));
    let _ = ws.get_mut().set_write_timeout(Some(Duration::from_secs(1)));
    {
        let mut inner = shared.lock();
        // Anything queued belongs to a previous connection/epoch: never replay.
        inner.purge(DropReason::StaleEpoch);
        inner.health.state = LocalAvatarState::Handshaking;
        if inner.sessions_seen > 0 {
            inner.health.reconnects += 1;
        }
        inner.sessions_seen += 1;
    }
    let mut session = Session {
        epoch: None,
        ready: false,
        seq: 0,
        in_flight: VecDeque::new(),
    };
    while !shared.shutdown.load(Ordering::SeqCst) {
        match ws.read() {
            Ok(Message::Text(text)) => {
                if on_frame(shared, &mut ws, &mut session, text.as_str()).is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Binary(_)) => {
                shared.lock().health.protocol_errors += 1;
                break;
            }
            Ok(_) => {}
            Err(WsError::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
        if session.ready && pump(shared, &mut ws, &mut session).is_err() {
            break;
        }
    }
    let _ = ws.close(None);
    let mut inner = shared.lock();
    inner.purge(DropReason::StaleEpoch);
    inner.health.state = LocalAvatarState::Disconnected;
    inner.renderer_model = None;
    inner.session_active = false;
}

/// `Err` means "close the connection".
fn on_frame(
    shared: &Shared,
    ws: &mut WebSocket<TcpStream>,
    session: &mut Session,
    text: &str,
) -> Result<(), ()> {
    let Ok(frame) = RendererFrame::parse(text) else {
        shared.lock().health.protocol_errors += 1;
        return Err(());
    };
    match frame {
        RendererFrame::Hello {
            epoch, model_hash, ..
        } => {
            if session.epoch.is_some() {
                shared.lock().health.protocol_errors += 1;
                return Err(());
            }
            session.epoch = Some(epoch);
            shared.lock().renderer_model = Some(model_hash);
        }
        RendererFrame::Ready { epoch } => {
            if session.ready || session.epoch.as_deref() != Some(epoch.as_str()) {
                shared.lock().health.protocol_errors += 1;
                return Err(());
            }
            session.ready = true;
            shared.lock().health.state = LocalAvatarState::Ready;
            // First frame of the epoch: snap to a known state.
            send(shared, ws, session, WireCommand::Reset {})?;
        }
        RendererFrame::Ack { epoch, seq } => settle(shared, session, &epoch, seq, None),
        RendererFrame::Reject { epoch, seq, reason } => {
            settle(shared, session, &epoch, seq, Some(reason));
        }
    }
    Ok(())
}

fn settle(
    shared: &Shared,
    session: &mut Session,
    epoch: &str,
    seq: u64,
    reject: Option<super::RejectReason>,
) {
    let mut inner = shared.lock();
    let known = session.epoch.as_deref() == Some(epoch)
        && session
            .in_flight
            .iter()
            .position(|s| *s == seq)
            .map(|i| session.in_flight.remove(i))
            .is_some();
    if !known {
        inner.health.stray_acks += 1;
    } else if let Some(reason) = reject {
        *inner.health.rejected.entry(reason).or_insert(0) += 1;
    } else {
        inner.health.acked += 1;
    }
}

fn send(
    shared: &Shared,
    ws: &mut WebSocket<TcpStream>,
    session: &mut Session,
    cmd: WireCommand,
) -> Result<(), ()> {
    let epoch = session.epoch.clone().ok_or(())?;
    session.seq += 1;
    let json = HostFrame::new(&epoch, session.seq, cmd).to_json();
    ws.send(Message::text(json)).map_err(|_| ())?;
    session.in_flight.push_back(session.seq);
    shared.lock().health.sent += 1;
    Ok(())
}

/// Sends queued commands, dropping the ones whose window has passed.
fn pump(shared: &Shared, ws: &mut WebSocket<TcpStream>, session: &mut Session) -> Result<(), ()> {
    loop {
        let next = {
            let mut inner = shared.lock();
            if session.in_flight.len() >= MAX_IN_FLIGHT {
                inner.health.state = LocalAvatarState::Degraded;
                return Ok(());
            }
            inner.health.state = LocalAvatarState::Ready;
            match inner.queue.pop_front() {
                None => return Ok(()),
                Some(queued) if queued.enqueued.elapsed() > shared.config.max_command_age => {
                    inner.drop_one(DropReason::Expired);
                    continue;
                }
                Some(queued) => queued.cmd,
            }
        };
        send(shared, ws, session, next)?;
    }
}

impl LocalAvatarConfig {
    pub fn with_page_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.page_dir = Some(dir.into());
        self
    }
}
