//! Loopback HTTP server for the normalized TTS contract (#220 S2b).
//!
//! One route, hand-rolled over `std::net` (no new crate): `POST <path>` with
//! `Content-Type: application/json` and the body `NormalizedHttpTtsAdapter`
//! sends, `{"text","style","voice_model","viseme_mapping"}`. Success is
//! `200 {"audio_ref","duration_ms"[,"viseme_ref"]}` and the audio is a WAV file
//! under the audio root. Failures are non-200 statuses the adapter already
//! maps: 400/422 invalid request, 503 busy (`Overloaded`), 5xx unavailable.
//!
//! Bounds: header bytes and lines, body bytes, text characters, simultaneous
//! connections, one synthesis at a time (a second one is refused with 503, not
//! queued), a per-request read deadline, a synthesis deadline, and a byte cap on
//! the generated-audio directory. A synthesis that outlives its deadline is
//! answered 504 and its result is discarded; it keeps the busy flag until the
//! engine returns, so a hung engine cannot pile up work.

use super::{
    GenerationSettings, SynthError, Synthesizer, VoiceConfig, content_digest, envelope, wav,
};
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub const DEFAULT_PATH: &str = "/v1/synthesize";
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_HEADER_LINES: usize = 32;
const STYLE_APPLIED_HEADER: &str = "X-TTS-Style-Applied: false\r\n";

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Must be a loopback address.
    pub listen: SocketAddr,
    /// The request path served; anything else is 404.
    pub path: String,
    /// `AIVTUBER_AUDIO_ROOT`: must exist. WAV files go to `<root>/generated/`.
    pub audio_root: PathBuf,
    /// Pack root for amplitude viseme tracks, written to
    /// `<root>/viseme/generated/`. `None`: no `viseme_ref` (no mouth track).
    pub viseme_root: Option<PathBuf>,
    pub max_body_bytes: usize,
    pub max_text_chars: usize,
    pub synth_deadline: Duration,
    /// Per-request read deadline and per-socket timeout.
    pub io_timeout: Duration,
    pub max_connections: usize,
    /// Cap on the WAV files in the generated directory; the oldest are
    /// removed past it.
    pub max_cache_bytes: u64,
    pub voice: VoiceConfig,
}

impl ServerConfig {
    pub fn new(listen: SocketAddr, audio_root: impl Into<PathBuf>) -> Self {
        Self {
            listen,
            path: DEFAULT_PATH.to_owned(),
            audio_root: audio_root.into(),
            viseme_root: None,
            max_body_bytes: 16 * 1024,
            max_text_chars: 500,
            synth_deadline: Duration::from_secs(12),
            io_timeout: Duration::from_secs(5),
            max_connections: 4,
            max_cache_bytes: 256 * 1024 * 1024,
            voice: VoiceConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.listen.ip().is_loopback() {
            return Err(format!(
                "listen address {} is not loopback; the TTS sidecar serves 127.0.0.1/[::1] only",
                self.listen
            ));
        }
        if !self.path.starts_with('/') || self.path.contains(['?', '#', ' ']) {
            return Err("path must start with / and have no query".to_owned());
        }
        if !self.audio_root.is_dir() {
            return Err(format!(
                "audio root {} is not an existing directory",
                self.audio_root.display()
            ));
        }
        if let Some(root) = &self.viseme_root
            && !root.is_dir()
        {
            return Err(format!(
                "viseme root {} is not an existing directory",
                root.display()
            ));
        }
        if !(256..=1024 * 1024).contains(&self.max_body_bytes)
            || !(1..=2_000).contains(&self.max_text_chars)
            || !(1..=64).contains(&self.max_connections)
            || self.synth_deadline.is_zero()
            || self.synth_deadline > Duration::from_secs(120)
            || self.io_timeout.is_zero()
            || self.io_timeout > Duration::from_secs(60)
            || self.max_cache_bytes < 1024 * 1024
        {
            return Err("a server bound is out of range".to_owned());
        }
        self.voice.validate()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerStats {
    pub ok: u64,
    pub cache_hits: u64,
    pub busy_rejected: u64,
    pub connections_rejected: u64,
    pub bad_requests: u64,
    pub deadline_exceeded: u64,
    pub engine_errors: u64,
    /// Requests that carried a `style`; it is never applied.
    pub style_ignored: u64,
}

#[derive(Default)]
struct Counters {
    ok: AtomicU64,
    cache_hits: AtomicU64,
    busy_rejected: AtomicU64,
    connections_rejected: AtomicU64,
    bad_requests: AtomicU64,
    deadline_exceeded: AtomicU64,
    engine_errors: AtomicU64,
    style_ignored: AtomicU64,
}

struct Inner {
    config: ServerConfig,
    synth: Arc<dyn Synthesizer>,
    busy: AtomicBool,
    connections: AtomicUsize,
    counters: Counters,
    tmp_counter: AtomicU64,
}

struct BusyGuard(Arc<Inner>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

struct ConnectionGuard(Arc<Inner>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct TtsServer {
    listener: TcpListener,
    inner: Arc<Inner>,
}

pub struct ServerHandle {
    addr: SocketAddr,
    inner: Arc<Inner>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> ServerStats {
        let c = &self.inner.counters;
        ServerStats {
            ok: c.ok.load(Ordering::Relaxed),
            cache_hits: c.cache_hits.load(Ordering::Relaxed),
            busy_rejected: c.busy_rejected.load(Ordering::Relaxed),
            connections_rejected: c.connections_rejected.load(Ordering::Relaxed),
            bad_requests: c.bad_requests.load(Ordering::Relaxed),
            deadline_exceeded: c.deadline_exceeded.load(Ordering::Relaxed),
            engine_errors: c.engine_errors.load(Ordering::Relaxed),
            style_ignored: c.style_ignored.load(Ordering::Relaxed),
        }
    }

    pub fn stop(mut self) {
        self.shutdown_now();
    }

    fn shutdown_now(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}

impl TtsServer {
    pub fn bind(config: ServerConfig, synth: Arc<dyn Synthesizer>) -> Result<Self, String> {
        config.validate()?;
        let listener = TcpListener::bind(config.listen)
            .map_err(|error| format!("cannot listen on {}: {error}", config.listen))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            listener,
            inner: Arc::new(Inner {
                config,
                synth,
                busy: AtomicBool::new(false),
                connections: AtomicUsize::new(0),
                counters: Counters::default(),
                tmp_counter: AtomicU64::new(0),
            }),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("bound listener")
    }

    /// Serve on a background thread until the handle is stopped or dropped.
    pub fn spawn(self) -> ServerHandle {
        let addr = self.local_addr();
        let shutdown = Arc::new(AtomicBool::new(false));
        let inner = self.inner.clone();
        let flag = shutdown.clone();
        let thread = thread::Builder::new()
            .name("tts-accept".to_owned())
            .spawn(move || self.accept_loop(&flag))
            .expect("spawn accept thread");
        ServerHandle {
            addr,
            inner,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Serve on the calling thread; returns only if `shutdown` is set.
    pub fn serve(self, shutdown: &AtomicBool) {
        self.accept_loop(shutdown);
    }

    fn accept_loop(&self, shutdown: &AtomicBool) {
        while !shutdown.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((stream, _)) => self.dispatch(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn dispatch(&self, stream: TcpStream) {
        let inner = self.inner.clone();
        let previous = inner.connections.fetch_add(1, Ordering::AcqRel);
        let guard = ConnectionGuard(inner.clone());
        if previous >= inner.config.max_connections {
            inner
                .counters
                .connections_rejected
                .fetch_add(1, Ordering::Relaxed);
            // Refuse inline, with no thread: the 503 is a few dozen bytes that
            // fit a fresh socket's send buffer, and the short timeout keeps a
            // hostile client from holding the accept loop. A connection flood
            // therefore costs this loop, never a thread per connection.
            let mut stream = stream;
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
            write_response(
                &mut stream,
                error_response(503, "busy", "too many connections"),
            );
            drop(guard);
            return;
        }
        let spawned = thread::Builder::new()
            .name("tts-conn".to_owned())
            .spawn(move || {
                let _guard = guard;
                handle_connection(&inner, stream);
            });
        if spawned.is_err() {
            // The guard moved into the failed closure and was dropped.
        }
    }
}

struct Response {
    status: u16,
    body: Vec<u8>,
    retry_after: bool,
    style_header: bool,
}

fn error_response(status: u16, code: &str, message: &str) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&json!({"error": code, "message": message})).unwrap_or_default(),
        retry_after: status == 503,
        style_header: false,
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

fn write_response(stream: &mut TcpStream, response: Response) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        response.status,
        reason(response.status),
        response.body.len()
    );
    if response.retry_after {
        head.push_str("Retry-After: 1\r\n");
    }
    if response.style_header {
        head.push_str(STYLE_APPLIED_HEADER);
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

#[derive(Debug)]
struct HttpError(u16, &'static str, &'static str);

impl HttpError {
    fn into_response(self) -> Response {
        error_response(self.0, self.1, self.2)
    }
}

struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn handle_connection(inner: &Arc<Inner>, mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(inner.config.io_timeout));
    let _ = stream.set_write_timeout(Some(inner.config.io_timeout));
    let _ = stream.set_nodelay(true);
    let response = match serve_request(inner, &mut stream) {
        Ok(response) => response,
        Err(error) => {
            inner.counters.bad_requests.fetch_add(1, Ordering::Relaxed);
            error.into_response()
        }
    };
    write_response(&mut stream, response);
}

fn read_error(error: &std::io::Error) -> HttpError {
    match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HttpError(
            408,
            "request_timeout",
            "the request was not received in time",
        ),
        _ => HttpError(400, "bad_request", "the request could not be read"),
    }
}

/// Read up to the blank line that ends the head. Returns the head text and any
/// body bytes that arrived with it.
fn read_head(stream: &mut TcpStream, deadline: Instant) -> Result<(String, Vec<u8>), HttpError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = find_head_end(&buffer) {
            if end > MAX_HEADER_BYTES {
                return Err(HttpError(
                    431,
                    "headers_too_large",
                    "request head too large",
                ));
            }
            let head = String::from_utf8(buffer[..end].to_vec())
                .map_err(|_| HttpError(400, "bad_request", "the request head is not UTF-8"))?;
            return Ok((head, buffer[end + 4..].to_vec()));
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(HttpError(
                431,
                "headers_too_large",
                "request head too large",
            ));
        }
        if Instant::now() >= deadline {
            return Err(HttpError(
                408,
                "request_timeout",
                "the request was not received in time",
            ));
        }
        let n = stream.read(&mut chunk).map_err(|e| read_error(&e))?;
        if n == 0 {
            return Err(HttpError(
                400,
                "bad_request",
                "connection closed before the request ended",
            ));
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_head(text: &str) -> Result<Head, HttpError> {
    // httparse owns the request-line and header grammar: it rejects obsolete
    // line folding, bare CR/LF, spaces in names and control characters in
    // values. The head was already cut at the blank line and bounded by
    // MAX_HEADER_BYTES; the array bounds the header count.
    // httparse accepts a bare LF as a line end; this server does not, so one
    // header value cannot smuggle a second header past a proxy that disagrees.
    let head_bytes = text.as_bytes();
    let bare = head_bytes.iter().enumerate().any(|(i, b)| match b {
        b'\n' => i == 0 || head_bytes[i - 1] != b'\r',
        b'\r' => head_bytes.get(i + 1) != Some(&b'\n'),
        _ => false,
    });
    if bare {
        return Err(HttpError(
            400,
            "bad_request",
            "bare CR or LF in the request head",
        ));
    }
    let mut bytes = head_bytes.to_vec();
    bytes.extend_from_slice(b"\r\n\r\n");
    let mut storage = [httparse::EMPTY_HEADER; MAX_HEADER_LINES];
    let mut request = httparse::Request::new(&mut storage);
    match request.parse(&bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(HttpError(
                431,
                "headers_too_large",
                "request head incomplete",
            ));
        }
        Err(httparse::Error::TooManyHeaders) => {
            return Err(HttpError(431, "headers_too_large", "too many header lines"));
        }
        Err(_) => return Err(HttpError(400, "bad_request", "malformed request head")),
    }
    let (Some(method), Some(path)) = (request.method, request.path) else {
        return Err(HttpError(400, "bad_request", "malformed request line"));
    };
    let mut headers = Vec::new();
    for header in request.headers.iter() {
        let value = std::str::from_utf8(header.value)
            .map_err(|_| HttpError(400, "bad_request", "header value is not UTF-8"))?;
        headers.push((header.name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Head {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
    })
}

/// `Host` must name this machine: a loopback IP literal or `localhost`. This is
/// what stops a DNS-rebinding page from reaching the sidecar.
fn host_is_loopback(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TtsRequest {
    text: String,
    #[serde(default)]
    style: Option<String>,
    // Accepted because the adapter sends them. Never used to choose a voice
    // (configuration only) and never echoed.
    #[serde(default)]
    #[allow(dead_code)]
    voice_model: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    viseme_mapping: Option<String>,
}

/// Remove control characters other than ordinary whitespace (a NUL would also
/// panic the engine's C string conversion) and trim.
fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .to_owned()
}

struct Artifact {
    audio_ref: String,
    duration_ms: u64,
    viseme_ref: Option<String>,
}

fn serve_request(inner: &Arc<Inner>, stream: &mut TcpStream) -> Result<Response, HttpError> {
    let config = &inner.config;
    let read_deadline = Instant::now() + config.io_timeout;
    let (head_text, mut body) = read_head(stream, read_deadline)?;
    let head = parse_head(&head_text)?;

    if head.path != config.path {
        return Err(HttpError(404, "not_found", "unknown path"));
    }
    if head.method != "POST" {
        return Err(HttpError(405, "method_not_allowed", "POST only"));
    }
    match head.header("host") {
        Some(host) if host_is_loopback(host) => {}
        _ => {
            return Err(HttpError(
                400,
                "bad_request",
                "Host must be a loopback name",
            ));
        }
    }
    let content_type = head.header("content-type").unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(HttpError(
            415,
            "unsupported_media_type",
            "application/json only",
        ));
    }
    if head.header("transfer-encoding").is_some() {
        return Err(HttpError(
            400,
            "bad_request",
            "chunked bodies are not accepted",
        ));
    }
    let Some(length) = head.header("content-length") else {
        return Err(HttpError(
            411,
            "length_required",
            "Content-Length is required",
        ));
    };
    let Ok(length) = length.parse::<usize>() else {
        return Err(HttpError(400, "bad_request", "invalid Content-Length"));
    };
    if length > config.max_body_bytes {
        return Err(HttpError(413, "body_too_large", "request body too large"));
    }
    if body.len() > length {
        return Err(HttpError(
            400,
            "bad_request",
            "body longer than Content-Length",
        ));
    }
    let mut chunk = [0_u8; 4096];
    while body.len() < length {
        if Instant::now() >= read_deadline {
            return Err(HttpError(
                408,
                "request_timeout",
                "the request was not received in time",
            ));
        }
        let want = chunk.len().min(length - body.len());
        let n = stream
            .read(&mut chunk[..want])
            .map_err(|e| read_error(&e))?;
        if n == 0 {
            return Err(HttpError(
                400,
                "bad_request",
                "connection closed before the body ended",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let request: TtsRequest = serde_json::from_slice(&body)
        .map_err(|_| HttpError(400, "bad_request", "the body is not a TTS request"))?;
    if request.style.is_some() {
        // Supertonic has no style: it is ignored and reported, never an error
        // (decision Q6).
        inner.counters.style_ignored.fetch_add(1, Ordering::Relaxed);
    }
    let text = sanitize_text(&request.text);
    if text.is_empty() {
        return Err(HttpError(400, "bad_request", "text must not be empty"));
    }
    if text.chars().count() > config.max_text_chars {
        return Err(HttpError(
            422,
            "text_too_long",
            "text exceeds the sidecar limit",
        ));
    }

    let settings = GenerationSettings::for_text(&text, config.voice);
    // One engine per request: the content address and the synthesis below must
    // agree on it even if a switchable front is swapped in between (#360).
    let synth: Arc<dyn Synthesizer> = inner
        .synth
        .snapshot()
        .unwrap_or_else(|| Arc::clone(&inner.synth));
    let digest = content_digest(&text, &settings, synth.bundle_sha256());
    let started = Instant::now();
    if let Some(artifact) = lookup_cache(config, &digest) {
        inner.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        inner.counters.ok.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "local-tts-sidecar: ok chars={} audio_ms={} cache=hit total_ms={} style_applied=false",
            text.chars().count(),
            artifact.duration_ms,
            started.elapsed().as_millis()
        );
        return Ok(success(&artifact));
    }

    if inner
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        inner.counters.busy_rejected.fetch_add(1, Ordering::Relaxed);
        return Ok(error_response(
            503,
            "busy",
            "a synthesis is already running",
        ));
    }
    let guard = BusyGuard(inner.clone());
    let deadline = started + config.synth_deadline;
    let (sender, receiver) = mpsc::channel();
    let worker_inner = inner.clone();
    let char_count = text.chars().count();
    let spawned = thread::Builder::new()
        .name("tts-synth".to_owned())
        .spawn(move || {
            let _guard = guard;
            let result = synthesize_to_files(
                &worker_inner,
                synth.as_ref(),
                &text,
                &settings,
                &digest,
                deadline,
            );
            let _ = sender.send(result);
        });
    if spawned.is_err() {
        return Ok(error_response(
            500,
            "internal",
            "cannot start a synthesis thread",
        ));
    }
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(artifact)) => {
            inner.counters.ok.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "local-tts-sidecar: ok chars={char_count} audio_ms={} cache=miss total_ms={} style_applied=false",
                artifact.duration_ms,
                started.elapsed().as_millis()
            );
            Ok(success(&artifact))
        }
        Ok(Err(SynthError::DeadlineExceeded)) | Err(mpsc::RecvTimeoutError::Timeout) => {
            inner
                .counters
                .deadline_exceeded
                .fetch_add(1, Ordering::Relaxed);
            eprintln!("local-tts-sidecar: deadline_exceeded chars={char_count}");
            Ok(error_response(
                504,
                "deadline_exceeded",
                "synthesis deadline exceeded",
            ))
        }
        Ok(Err(SynthError::Engine(message))) => {
            inner.counters.engine_errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("local-tts-sidecar: engine_error {message}");
            Ok(error_response(500, "engine_error", "the engine failed"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            inner.counters.engine_errors.fetch_add(1, Ordering::Relaxed);
            Ok(error_response(
                500,
                "engine_error",
                "the synthesis thread ended unexpectedly",
            ))
        }
    }
}

fn success(artifact: &Artifact) -> Response {
    let mut body = json!({
        "audio_ref": artifact.audio_ref,
        "duration_ms": artifact.duration_ms,
    });
    if let Some(viseme_ref) = &artifact.viseme_ref {
        body["viseme_ref"] = json!(viseme_ref);
    }
    Response {
        status: 200,
        body: serde_json::to_vec(&body).unwrap_or_default(),
        retry_after: false,
        style_header: true,
    }
}

fn wav_name(digest: &str) -> String {
    format!("{digest}.wav")
}

fn viseme_name(digest: &str) -> String {
    format!("{digest}.json")
}

fn wav_dir(config: &ServerConfig) -> PathBuf {
    config.audio_root.join("generated")
}

fn viseme_dir(config: &ServerConfig) -> Option<PathBuf> {
    config
        .viseme_root
        .as_ref()
        .map(|root| root.join("viseme").join("generated"))
}

fn artifact_for(config: &ServerConfig, digest: &str, duration_ms: u64) -> Artifact {
    Artifact {
        audio_ref: format!("audio://generated/{}", wav_name(digest)),
        duration_ms,
        viseme_ref: config
            .viseme_root
            .as_ref()
            .map(|_| format!("viseme/generated/{}", viseme_name(digest))),
    }
}

/// A previous synthesis of exactly this request, if its files are intact.
fn lookup_cache(config: &ServerConfig, digest: &str) -> Option<Artifact> {
    let path = wav_dir(config).join(wav_name(digest));
    let mut file = fs::File::open(&path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut header = [0_u8; wav::HEADER_LEN];
    file.read_exact(&mut header).ok()?;
    let (_, duration_ms) = wav::parse_header(length, &header)?;
    if let Some(dir) = viseme_dir(config)
        && !dir.join(viseme_name(digest)).is_file()
    {
        return None;
    }
    // Keep a reused file young so the size cap removes older ones first.
    // Best effort: a player may hold the file open.
    drop(file);
    if let Ok(writable) = fs::OpenOptions::new().write(true).open(&path) {
        let _ = writable.set_modified(SystemTime::now());
    }
    Some(artifact_for(config, digest, duration_ms))
}

fn synthesize_to_files(
    inner: &Inner,
    synth: &dyn Synthesizer,
    text: &str,
    settings: &GenerationSettings,
    digest: &str,
    deadline: Instant,
) -> Result<Artifact, SynthError> {
    let config = &inner.config;
    let pcm = synth.synthesize(text, settings, deadline)?;
    if Instant::now() > deadline {
        return Err(SynthError::DeadlineExceeded);
    }
    let bytes = wav::encode_pcm16(&pcm.samples, pcm.sample_rate)
        .ok_or_else(|| SynthError::Engine("engine returned no usable audio".to_owned()))?;
    let duration_ms = wav::duration_ms(pcm.samples.len() as u64, pcm.sample_rate);
    let io = |what: &str, error: std::io::Error| SynthError::Engine(format!("{what}: {error}"));
    // The track first: the audio file's existence is what makes a request a
    // cache hit, so it is published last.
    if let Some(dir) = viseme_dir(config) {
        let track = envelope::amplitude_track_json(&pcm.samples, pcm.sample_rate, duration_ms);
        write_atomic(inner, &dir, &viseme_name(digest), track.as_bytes())
            .map_err(|e| io("viseme track", e))?;
    }
    let dir = wav_dir(config);
    write_atomic(inner, &dir, &wav_name(digest), &bytes).map_err(|e| io("audio file", e))?;
    prune(config, &dir, &wav_name(digest));
    Ok(artifact_for(config, digest, duration_ms))
}

/// Write through a temp file and an atomic rename, so a file at its final name
/// is complete. No fsync: it cost over a second on a busy disk, and a file torn
/// by a crash is not a cache hit (the header must match the length) and is
/// synthesized again.
fn write_atomic(inner: &Inner, dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(
        "{name}.tmp{}-{}",
        std::process::id(),
        inner.tmp_counter.fetch_add(1, Ordering::Relaxed)
    ));
    let result = fs::File::create(&temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&temp, dir.join(name)));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Keep the generated WAV directory under the byte cap by removing the oldest
/// files (never the one just written), with their viseme tracks.
fn prune(config: &ServerConfig, dir: &Path, keep: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "wav" {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            Some((metadata.modified().ok()?, metadata.len(), path))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= config.max_cache_bytes {
        return;
    }
    files.sort_by_key(|(modified, ..)| *modified);
    for (_, len, path) in files {
        if total <= config.max_cache_bytes {
            break;
        }
        if path.file_name().is_some_and(|n| n == keep) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
            if let (Some(dir), Some(stem)) = (viseme_dir(config), path.file_stem()) {
                let _ = fs::remove_file(dir.join(format!("{}.json", stem.to_string_lossy())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_tts::{SwapSynthesizer, ToneSynthesizer};
    use crate::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
    use aivtuber_domain::{EngineErrorKind, SpeechRequest};
    use std::sync::atomic::AtomicUsize;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-tts-server-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(root: &Path) -> ServerConfig {
        let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), root);
        config.io_timeout = Duration::from_millis(800);
        config
    }

    fn start(config: ServerConfig, synth: impl Synthesizer) -> ServerHandle {
        TtsServer::bind(config, Arc::new(synth)).unwrap().spawn()
    }

    struct Reply {
        status: u16,
        headers: String,
        body: String,
    }

    fn raw(addr: SocketAddr, request: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Reply {
            status,
            headers: head.to_owned(),
            body: body.to_owned(),
        }
    }

    fn post_to(addr: SocketAddr, path: &str, extra_headers: &str, body: &str) -> Reply {
        raw(
            addr,
            format!(
                "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
    }

    fn post(addr: SocketAddr, body: &str) -> Reply {
        post_to(addr, DEFAULT_PATH, "", body)
    }

    fn ok_body(text: &str) -> String {
        json!({"text": text, "style": null, "voice_model": "v", "viseme_mapping": "m"}).to_string()
    }

    #[test]
    fn serves_the_normalized_contract_and_writes_a_wav_under_the_audio_root() {
        let root = scratch("ok");
        let server = start(config(&root), ToneSynthesizer::default());
        let reply = post(server.local_addr(), &ok_body("こんにちは"));
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert!(reply.headers.contains("X-TTS-Style-Applied: false"));
        let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let audio_ref = value["audio_ref"].as_str().unwrap();
        assert!(audio_ref.starts_with("audio://generated/") && audio_ref.ends_with(".wav"));
        assert_eq!(value["duration_ms"], 500); // 5 chars at 100 ms
        assert!(value.get("viseme_ref").is_none());
        let path = root
            .join("generated")
            .join(&audio_ref["audio://generated/".len()..]);
        let bytes = fs::read(&path).unwrap();
        let (rate, ms) = wav::parse_header(bytes.len() as u64, &bytes).unwrap();
        assert_eq!((rate, ms), (44_100, 500));
        // The reference resolves through the same rules the player uses.
        assert!(!audio_ref.contains(".."));
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_production_adapter_is_the_client_and_the_contract_is_unchanged() {
        let root = scratch("adapter");
        let server = start(config(&root), ToneSynthesizer::default());
        let adapter = NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: format!("http://{}{DEFAULT_PATH}", server.local_addr()),
                backend_name: "local-tts-sidecar".to_owned(),
                model_alias: Some("supertonic-3-int8".to_owned()),
                model_version: None,
                voice_model: Some("supertonic-3-int8".to_owned()),
                viseme_mapping: Some("ja-5vowel-v1".to_owned()),
                timeout: Duration::from_secs(5),
            },
            None,
        )
        .unwrap();
        let artifact = adapter
            .synthesize_sync(&SpeechRequest {
                text: "生成音声です".to_owned(),
                style: Some("cheerful".to_owned()),
            })
            .unwrap();
        assert_eq!(artifact.duration_ms, 600);
        assert!(artifact.audio_ref.starts_with("audio://generated/"));
        assert_eq!(artifact.viseme_ref, None);
        assert_eq!(
            server.stats().style_ignored,
            1,
            "style is ignored and counted"
        );
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn viseme_tracks_are_published_when_a_viseme_root_is_set() {
        let root = scratch("viseme-audio");
        let pack = scratch("viseme-pack");
        let mut config = config(&root);
        config.viseme_root = Some(pack.clone());
        let server = start(config, ToneSynthesizer::default());
        let reply = post(server.local_addr(), &ok_body("おはよう"));
        assert_eq!(reply.status, 200);
        let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let viseme_ref = value["viseme_ref"].as_str().unwrap();
        assert!(viseme_ref.starts_with("viseme/generated/") && viseme_ref.ends_with(".json"));
        let track = fs::read_to_string(pack.join(viseme_ref)).unwrap();
        let track: serde_json::Value = serde_json::from_str(&track).unwrap();
        assert_eq!(track["schema_version"], "0.1.0");
        assert_eq!(track["duration_ms"], value["duration_ms"]);
        server.stop();
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&pack);
    }

    struct Counting(Arc<AtomicUsize>);

    impl Synthesizer for Counting {
        fn synthesize(
            &self,
            text: &str,
            settings: &GenerationSettings,
            deadline: Instant,
        ) -> Result<crate::speech_tts::Pcm, SynthError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToneSynthesizer::default().synthesize(text, settings, deadline)
        }
        fn bundle_sha256(&self) -> &str {
            "counting"
        }
    }

    #[test]
    fn the_same_request_reuses_the_file_and_does_not_synthesize_again() {
        let root = scratch("cache");
        let calls = Arc::new(AtomicUsize::new(0));
        let server = start(config(&root), Counting(calls.clone()));
        let first = post(server.local_addr(), &ok_body("やあ"));
        let second = post(server.local_addr(), &ok_body("やあ"));
        let third = post(server.local_addr(), &ok_body("やあ!"));
        assert_eq!((first.status, second.status, third.status), (200, 200, 200));
        assert_eq!(first.body, second.body);
        assert_ne!(first.body, third.body);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(server.stats().cache_hits, 1);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_cache_file_is_synthesized_again() {
        let root = scratch("damaged");
        let calls = Arc::new(AtomicUsize::new(0));
        let server = start(config(&root), Counting(calls.clone()));
        let reply = post(server.local_addr(), &ok_body("やあ"));
        let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let name = &value["audio_ref"].as_str().unwrap()["audio://generated/".len()..];
        fs::write(root.join("generated").join(name), b"RIFF-not-a-wav").unwrap();
        assert_eq!(post(server.local_addr(), &ok_body("やあ")).status, 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn request_validation_returns_typed_statuses() {
        let root = scratch("validation");
        let mut config = config(&root);
        config.max_text_chars = 10;
        config.max_body_bytes = 256;
        let server = start(config, ToneSynthesizer::default());
        let addr = server.local_addr();

        assert_eq!(post(addr, "not json").status, 400);
        assert_eq!(post(addr, r#"{"text":"x","surprise":1}"#).status, 400);
        assert_eq!(post(addr, r#"{"text":"   "}"#).status, 400);
        assert_eq!(post(addr, r#"{"text":"\u0000\u0007"}"#).status, 400);
        assert_eq!(post(addr, &ok_body(&"あ".repeat(11))).status, 422);
        assert_eq!(post(addr, &ok_body(&"あ".repeat(10))).status, 200);
        let oversize = format!(r#"{{"text":"{}"}}"#, "a".repeat(300));
        assert_eq!(post(addr, &oversize).status, 413);
        assert_eq!(post_to(addr, "/other", "", &ok_body("a")).status, 404);
        assert_eq!(
            raw(
                addr,
                format!("GET {DEFAULT_PATH} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()
            )
            .status,
            405
        );
        let wrong_type = raw(
            addr,
            format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}")
                .as_bytes(),
        );
        assert_eq!(wrong_type.status, 415);
        let no_length = raw(
            addr,
            format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\r\n")
                .as_bytes(),
        );
        assert_eq!(no_length.status, 411);
        let chunked = raw(
            addr,
            format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")
                .as_bytes(),
        );
        assert_eq!(chunked.status, 400);
        // A NUL in the text is stripped, not passed to the engine.
        assert_eq!(post(addr, r#"{"text":"あ\u0000い"}"#).status, 200);
        assert!(server.stats().bad_requests >= 8);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_non_loopback_host_header_is_refused() {
        let root = scratch("host");
        let server = start(config(&root), ToneSynthesizer::default());
        let addr = server.local_addr();
        let body = ok_body("a");
        for host in [
            "attacker.example",
            "evil.localhost.example:80",
            "192.168.1.5",
        ] {
            let reply = raw(
                addr,
                format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
            );
            assert_eq!(reply.status, 400, "{host}");
        }
        for host in ["localhost:1234", "127.0.0.1:1", "[::1]:80"] {
            assert!(host_is_loopback(host), "{host}");
        }
        assert!(!host_is_loopback("localhost.attacker.example"));
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn it_refuses_to_bind_anything_but_loopback_or_a_missing_audio_root() {
        let root = scratch("bind");
        let mut remote = config(&root);
        remote.listen = "0.0.0.0:0".parse().unwrap();
        let error = TtsServer::bind(remote, Arc::new(ToneSynthesizer::default()))
            .err()
            .unwrap();
        assert!(error.contains("not loopback"), "{error}");
        let mut lan = config(&root);
        lan.listen = "192.168.1.10:0".parse().unwrap();
        assert!(TtsServer::bind(lan, Arc::new(ToneSynthesizer::default())).is_err());
        let missing = config(&root.join("does-not-exist"));
        assert!(
            TtsServer::bind(missing, Arc::new(ToneSynthesizer::default()))
                .err()
                .unwrap()
                .contains("audio root")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_synthesis_while_one_runs_is_refused_busy() {
        let root = scratch("busy");
        let server = start(
            config(&root),
            ToneSynthesizer {
                delay: Duration::from_millis(700),
                ..ToneSynthesizer::default()
            },
        );
        let addr = server.local_addr();
        let first = thread::spawn(move || post(addr, &ok_body("一つめ")));
        thread::sleep(Duration::from_millis(200));
        let second = post(addr, &ok_body("二つめ"));
        assert_eq!(second.status, 503);
        assert!(second.headers.contains("Retry-After"));
        assert_eq!(first.join().unwrap().status, 200);
        assert_eq!(server.stats().busy_rejected, 1);
        // Free again afterwards.
        assert_eq!(post(addr, &ok_body("三つめ")).status, 200);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_adapter_maps_the_busy_status_to_overloaded() {
        let root = scratch("busy-adapter");
        let server = start(
            config(&root),
            ToneSynthesizer {
                delay: Duration::from_millis(600),
                ..ToneSynthesizer::default()
            },
        );
        let addr = server.local_addr();
        let first = thread::spawn(move || post(addr, &ok_body("一つめ")));
        thread::sleep(Duration::from_millis(200));
        let adapter = NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: format!("http://{addr}{DEFAULT_PATH}"),
                backend_name: "b".to_owned(),
                model_alias: None,
                model_version: None,
                voice_model: None,
                viseme_mapping: None,
                timeout: Duration::from_secs(3),
            },
            None,
        )
        .unwrap();
        let error = adapter
            .synthesize_sync(&SpeechRequest {
                text: "別の文".to_owned(),
                style: None,
            })
            .unwrap_err();
        assert_eq!(error.kind, EngineErrorKind::Overloaded);
        first.join().unwrap();
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_synthesis_past_its_deadline_is_a_504_and_writes_nothing() {
        let root = scratch("deadline");
        let mut config = config(&root);
        config.synth_deadline = Duration::from_millis(150);
        let server = start(
            config,
            ToneSynthesizer {
                delay: Duration::from_millis(500),
                ..ToneSynthesizer::default()
            },
        );
        let reply = post(server.local_addr(), &ok_body("遅い"));
        assert_eq!(reply.status, 504);
        thread::sleep(Duration::from_millis(600));
        let wavs = fs::read_dir(root.join("generated"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(wavs, 0, "a late result is discarded, never published");
        assert_eq!(server.stats().deadline_exceeded, 1);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_engine_failure_is_a_500_and_the_server_recovers() {
        let root = scratch("engine");
        let failing = start(
            config(&root),
            ToneSynthesizer {
                fail: true,
                ..ToneSynthesizer::default()
            },
        );
        let reply = post(failing.local_addr(), &ok_body("だめ"));
        assert_eq!(reply.status, 500);
        assert!(
            !reply.body.contains("tone synthesizer"),
            "no engine detail on the wire"
        );
        assert_eq!(failing.stats().engine_errors, 1);
        failing.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn simultaneous_connections_are_bounded_and_a_stalled_client_times_out() {
        let root = scratch("connections");
        let mut config = config(&root);
        config.max_connections = 1;
        config.io_timeout = Duration::from_millis(1_500);
        let server = start(config, ToneSynthesizer::default());
        let addr = server.local_addr();
        // A client that connects and says nothing holds the only slot.
        let _idle = TcpStream::connect(addr).unwrap();
        thread::sleep(Duration::from_millis(150));
        let refused = post(addr, &ok_body("a"));
        assert_eq!(refused.status, 503);
        assert_eq!(server.stats().connections_rejected, 1);
        // The idle client is dropped by the read deadline, which frees the slot.
        thread::sleep(Duration::from_millis(1_700));
        assert_eq!(
            post(addr, &ok_body("a")).status,
            200,
            "the slot is free again"
        );
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_head_gets_408_instead_of_holding_a_thread() {
        let root = scratch("slowloris");
        let mut config = config(&root);
        config.io_timeout = Duration::from_millis(400);
        let server = start(config, ToneSynthesizer::default());
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"POST /v1/synth").unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn header_injection_and_obsolete_line_folding_are_refused() {
        let root = scratch("inject");
        let server = start(config(&root), ToneSynthesizer::default());
        let addr = server.local_addr();
        let body = ok_body("a");
        let base = format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: {addr}\r\n");
        let tail = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        for (name, middle) in [
            (
                "obsolete line folding",
                "Content-Type: application/json\r\n  folded\r\n",
            ),
            (
                "bare LF in a value",
                "Content-Type: application/json\nX-Injected: 1\r\n",
            ),
            (
                "NUL in a value",
                "X-A: a\0b\r\nContent-Type: application/json\r\n",
            ),
            (
                "space in a header name",
                "Content Type: application/json\r\n",
            ),
            (
                "header without a colon",
                "Content-Type application/json\r\n",
            ),
        ] {
            let reply = raw(addr, format!("{base}{middle}{tail}").as_bytes());
            assert_eq!(reply.status, 400, "{name}");
        }
        let line = raw(
            addr,
            format!("POST  {DEFAULT_PATH}  HTTP/1.1\r\n\r\n").as_bytes(),
        );
        assert_eq!(line.status, 400);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversize_head_is_refused() {
        let root = scratch("head");
        let server = start(config(&root), ToneSynthesizer::default());
        let flood = format!(
            "POST {DEFAULT_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES + 100)
        );
        assert_eq!(raw(server.local_addr(), flood.as_bytes()).status, 431);
        let many: String = (0..40).map(|i| format!("X-{i}: 1\r\n")).collect();
        let lines = format!("POST {DEFAULT_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\n{many}\r\n");
        assert_eq!(raw(server.local_addr(), lines.as_bytes()).status, 431);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_generated_directory_stays_under_its_byte_cap() {
        let root = scratch("prune");
        let mut config = config(&root);
        // 1 MiB cap; each 5-char reply is 44.1 kHz * 0.5 s * 2 bytes = 44 KB.
        config.max_cache_bytes = 1024 * 1024;
        let server = start(config, ToneSynthesizer::default());
        for i in 0..40 {
            let text = format!("あいうえ{}", char::from_u32(0x3041 + i).unwrap());
            assert_eq!(post(server.local_addr(), &ok_body(&text)).status, 200);
        }
        let total: u64 = fs::read_dir(root.join("generated"))
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 1024 * 1024, "{total} bytes");
        assert!(total > 0);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    /// A named voice: counts its calls and has its own identity (the part of the
    /// content address a model, style or tuning change must change).
    struct Voice {
        identity: &'static str,
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl Synthesizer for Voice {
        fn synthesize(
            &self,
            text: &str,
            settings: &GenerationSettings,
            deadline: Instant,
        ) -> Result<crate::speech_tts::Pcm, SynthError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(SynthError::Engine("voice: HTTP 401: scripted".to_owned()));
            }
            ToneSynthesizer::default().synthesize(text, settings, deadline)
        }
        fn bundle_sha256(&self) -> &str {
            self.identity
        }
    }

    fn voice(identity: &'static str, calls: &Arc<AtomicUsize>, fail: bool) -> Arc<dyn Synthesizer> {
        Arc::new(Voice {
            identity,
            calls: calls.clone(),
            fail,
        })
    }

    #[test]
    fn a_swapped_voice_never_replays_another_voices_audio() {
        let root = scratch("swap");
        let calls = Arc::new(AtomicUsize::new(0));
        let swap = SwapSynthesizer::new(voice("voice-a", &calls, false));
        let server = start(config(&root), swap.clone());
        let addr = server.local_addr();
        let a1 = post(addr, &ok_body("やあ"));
        let a2 = post(addr, &ok_body("やあ"));
        assert_eq!((a1.status, a1.body == a2.body), (200, true));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "same voice: cache hit");
        // The operator changes the voice (model, style or tuning): the same
        // text is synthesized again and lands on another address.
        swap.swap(voice("voice-b", &calls, false));
        let b = post(addr, &ok_body("やあ"));
        assert_eq!(b.status, 200);
        assert_ne!(a1.body, b.body, "another address");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Going back to the first voice finds its own audio again.
        swap.swap(voice("voice-a", &calls, false));
        let back = post(addr, &ok_body("やあ"));
        assert_eq!(back.body, a1.body);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_last_failure_is_remembered_until_a_success_or_a_swap() {
        let root = scratch("swap-error");
        let calls = Arc::new(AtomicUsize::new(0));
        let swap = SwapSynthesizer::new(voice("voice-a", &calls, true));
        let server = start(config(&root), swap.clone());
        let addr = server.local_addr();
        assert!(swap.last_error().is_none());
        assert_ne!(post(addr, &ok_body("やあ")).status, 200);
        assert_eq!(
            swap.last_error().as_deref(),
            Some("voice: HTTP 401: scripted")
        );
        swap.swap(voice("voice-b", &calls, false));
        assert!(swap.last_error().is_none(), "a swap forgets the failure");
        assert_eq!(post(addr, &ok_body("やあ")).status, 200);
        assert!(swap.last_error().is_none());
        server.stop();
        let _ = fs::remove_dir_all(&root);
    }
}
