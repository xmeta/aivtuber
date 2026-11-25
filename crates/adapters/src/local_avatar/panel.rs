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
//!   own queue. The client never supplies a path or a file name;
//! * voice input (#400) adds `POST /api/speech-input` with `{"enabled": bool}`
//!   and nothing else (a strict body: no device, program or argument), on its
//!   own queue. It asks the composition thread to start or stop the microphone
//!   sidecar; whether voice input is composed at all is decided there. Two
//!   more strict bodies share that queue (#396): `POST /api/speech-input/device`
//!   with `{"device": "<name>"|null}` (a capture device name, or null for the
//!   system default; the composition thread accepts only a device the sidecar
//!   listed) and `POST /api/speech-input/duplex` with `{"duplex":
//!   "half"|"full"}`. No path, program or argument can ride along;
//! * the character sheet (#401) adds `POST /api/persona/set` with
//!   `{"text": string}` and `POST /api/persona/get` with `{}`, on their own
//!   queue. The set body has its own larger bound, checked before the body is
//!   read; the text is held as a [`PanelSecret`] so it never reaches a `Debug`
//!   line, and only the get answer carries it back (`text`);
//! * the operator topic list (#87 S2) adds `POST /api/topics/set` with
//!   `{"text": string}` (one topic per line) and `POST /api/topics/get` with
//!   `{}`, on the character sheet's queue and under the same capability, with
//!   a 16 KiB set bound checked before the body is read and the same
//!   redaction: the text reaches no `Debug` line and only the get answer
//!   carries it back;
//! * the operator memory seed (#402) adds `POST /api/memory-seed/set`, whose
//!   body is the seed document itself (`schemas/memory-seed.schema.json`, parsed
//!   and bounded by the composition thread), and `POST /api/memory-seed/get`
//!   with `{}`, on their own queue. The set body has its own larger bound,
//!   checked before the body is read; the document is held as a
//!   [`PanelSecret`] so no claim reaches a `Debug` line, and only the get
//!   answer carries it back (`text`).

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
const SPEECH_INPUT_QUEUE: usize = 4;
const INITIATIVE_QUEUE: usize = 4;
const SESSION_LOG_QUEUE: usize = 4;
const PERSONA_QUEUE: usize = 4;
const MEMORY_SEED_QUEUE: usize = 4;
/// Largest `memory-seed.set` body: the seed file bound (64 KiB) plus room for a
/// whitespace-heavy hand-edited document. The app refuses a document over the
/// file bound; this only keeps the read itself bounded.
pub const MAX_PANEL_MEMORY_SEED_BODY_BYTES: usize = 96 * 1024;
/// Largest `persona.set` body: a 3,000-byte sheet whose every byte is a
/// newline, tab, quote or backslash escapes to 6,000 bytes, plus the envelope.
/// A sheet over 3,000 bytes is refused by the app's validator, not here.
pub const MAX_PANEL_PERSONA_BODY_BYTES: usize = 8 * 1024;
/// Largest `topics.set` body (#87 S2): 32 topics of 80 characters stay under
/// 10 KiB of UTF-8 even at four bytes a character; this leaves room for JSON
/// escapes. A list over its bounds is refused by the app's validator, not here.
pub const MAX_PANEL_TOPICS_BODY_BYTES: usize = 16 * 1024;
const CHAT_QUEUE: usize = 8;
/// Longest accepted API key, in characters (Aivis keys are about 40).
pub const MAX_PANEL_KEY_CHARS: usize = 256;
/// One model search may take this long, end to end.
const SEARCH_DEADLINE: Duration = Duration::from_secs(6);
/// Every state-changing endpoint (all are `POST`).
const STATE_CHANGING: [&str; 30] = [
    "/api/chat",
    "/api/stop",
    "/api/mute",
    "/api/shutdown",
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
    "/api/speech-input",
    "/api/initiative",
    "/api/session-log",
    "/api/session-log/delete",
    "/api/persona/set",
    "/api/persona/get",
    "/api/topics/set",
    "/api/topics/get",
    "/api/memory-seed/set",
    "/api/memory-seed/get",
    "/api/speech-input/device",
    "/api/speech-input/duplex",
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
    /// End the whole app (#449): the emergency stop, the answer, then a
    /// graceful exit decided on the composition thread.
    Shutdown,
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
    /// Turn voice input (the microphone sidecar) on or off (#400). Only the
    /// wish: the composition thread decides whether it is composed and
    /// whether the sidecar can start.
    SpeechInput {
        enabled: bool,
    },
    /// Choose the capture device (#396); `None` is the system default. Only
    /// the wish: the composition thread accepts a name only when it is
    /// acceptable and, once the sidecar listed its devices, one of them.
    SpeechInputDevice(Option<String>),
    /// Half or full duplex (#396): whether the performer listens while it
    /// speaks.
    SpeechInputDuplex {
        full: bool,
    },
    /// Let the performer speak on its own when no chat arrives, or not (#87).
    /// Only the wish: the composition thread decides whether the initiative is
    /// composed.
    Initiative {
        enabled: bool,
    },
    /// Save the conversation to a local file, or not (#463). Only the wish:
    /// the composition thread decides whether the log is composed.
    SessionLog {
        enabled: bool,
    },
    /// Delete every saved session log (#463).
    SessionLogDelete,
    /// Replace the character sheet (#401) with this text (empty clears it).
    /// Redacted in `Debug`; the composition thread validates it.
    PersonaSet(PanelSecret),
    /// Read the character sheet back into the editor (#401).
    PersonaGet,
    /// Replace the operator topic list (#87 S2) with this text, one topic per
    /// line (empty clears it). Redacted in `Debug`; the composition thread
    /// validates it. Shares the character sheet's queue and capability.
    TopicsSet(PanelSecret),
    /// Read the topic list back into the editor (#87 S2).
    TopicsGet,
    /// Replace the operator memory seed (#402) with this document. Redacted in
    /// `Debug`; the composition thread parses and validates it.
    MemorySeedSet(PanelSecret),
    /// Read the operator memory seed back into the editor (#402).
    MemorySeedGet,
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
    /// The character sheet text answering `PersonaGet` (#401); nothing else
    /// sets it.
    pub text: Option<String>,
}

/// One queued request with the slot its answer goes to.
pub struct PanelCommand {
    pub request: PanelRequest,
    reply: SyncSender<PanelReply>,
}

impl PanelCommand {
    pub fn respond(self, ok: bool, detail: impl Into<String>) {
        self.send(ok, detail.into(), None, None);
    }

    pub fn respond_with_id(self, ok: bool, detail: impl Into<String>, id: impl Into<String>) {
        self.send(ok, detail.into(), Some(id.into()), None);
    }

    /// Answer `PersonaGet` with the sheet text (#401).
    pub fn respond_with_text(self, ok: bool, detail: impl Into<String>, text: Option<String>) {
        self.send(ok, detail.into(), None, text);
    }

    fn send(self, ok: bool, detail: String, id: Option<String>, text: Option<String>) {
        // The page may have given up; there is nobody to tell.
        let _ = self.reply.try_send(PanelReply {
            ok,
            detail,
            id,
            text,
        });
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
    speech_input_tx: SyncSender<PanelCommand>,
    initiative_tx: SyncSender<PanelCommand>,
    session_log_tx: SyncSender<PanelCommand>,
    persona_tx: SyncSender<PanelCommand>,
    memory_seed_tx: SyncSender<PanelCommand>,
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
    speech_input_rx: Mutex<Receiver<PanelCommand>>,
    initiative_rx: Mutex<Receiver<PanelCommand>>,
    session_log_rx: Mutex<Receiver<PanelCommand>>,
    persona_rx: Mutex<Receiver<PanelCommand>>,
    memory_seed_rx: Mutex<Receiver<PanelCommand>>,
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

    /// Voice input on/off requests (#400). Their own queue, so switching the
    /// microphone can never sit in front of a stop or a mute.
    pub fn drain_speech_input(&self, max: usize) -> Vec<PanelCommand> {
        self.speech_input_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Idle-initiative on/off requests (#87). Their own queue, so the switch
    /// can never sit in front of a stop or a mute.
    pub fn drain_initiative(&self, max: usize) -> Vec<PanelCommand> {
        self.initiative_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Session log on/off and delete requests (#463). Their own queue, so they
    /// can never sit in front of a stop or a mute.
    pub fn drain_session_log(&self, max: usize) -> Vec<PanelCommand> {
        self.session_log_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Character sheet requests (#401). Their own queue, like the others.
    pub fn drain_persona(&self, max: usize) -> Vec<PanelCommand> {
        self.persona_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_iter()
            .take(max)
            .collect()
    }

    /// Memory seed requests (#402). Their own queue, like the others.
    pub fn drain_memory_seed(&self, max: usize) -> Vec<PanelCommand> {
        self.memory_seed_rx
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
        let (speech_input_tx, speech_input_rx) = sync_channel(SPEECH_INPUT_QUEUE);
        let (initiative_tx, initiative_rx) = sync_channel(INITIATIVE_QUEUE);
        let (session_log_tx, session_log_rx) = sync_channel(SESSION_LOG_QUEUE);
        let (persona_tx, persona_rx) = sync_channel(PERSONA_QUEUE);
        let (memory_seed_tx, memory_seed_rx) = sync_channel(MEMORY_SEED_QUEUE);
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
            speech_input_tx,
            initiative_tx,
            session_log_tx,
            persona_tx,
            memory_seed_tx,
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
            speech_input_rx: Mutex::new(speech_input_rx),
            initiative_rx: Mutex::new(initiative_rx),
            session_log_rx: Mutex::new(session_log_rx),
            persona_rx: Mutex::new(persona_rx),
            memory_seed_rx: Mutex::new(memory_seed_rx),
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
    json_full(status, ok, detail, id, None)
}

fn json_full(
    status: &'static str,
    ok: bool,
    detail: &str,
    id: Option<&str>,
    text: Option<&str>,
) -> Response {
    let mut value = serde_json::json!({ "ok": ok, "detail": detail, "id": id });
    if let (Some(text), Some(object)) = (text, value.as_object_mut()) {
        object.insert("text".to_owned(), serde_json::Value::from(text));
    }
    let body = value.to_string();
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
    let body_limit = if path == "/api/persona/set" {
        MAX_PANEL_PERSONA_BODY_BYTES
    } else if path == "/api/topics/set" {
        MAX_PANEL_TOPICS_BODY_BYTES
    } else if path == "/api/memory-seed/set" {
        MAX_PANEL_MEMORY_SEED_BODY_BYTES
    } else {
        MAX_PANEL_BODY_BYTES
    };
    if length > body_limit {
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
        "/api/shutdown" => match empty_object(&body) {
            Ok(()) => (PanelRequest::Shutdown, &shared.control_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
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
        "/api/speech-input" => match speech_input_body(&body) {
            Ok(enabled) => (
                PanelRequest::SpeechInput { enabled },
                &shared.speech_input_tx,
            ),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/initiative" => match speech_input_body(&body) {
            // The same strict `{"enabled": boolean}` as the voice input switch.
            Ok(enabled) => (PanelRequest::Initiative { enabled }, &shared.initiative_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/session-log" => match speech_input_body(&body) {
            // The same strict `{"enabled": boolean}` as the other switches.
            Ok(enabled) => (PanelRequest::SessionLog { enabled }, &shared.session_log_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/session-log/delete" => match empty_object(&body) {
            Ok(()) => (PanelRequest::SessionLogDelete, &shared.session_log_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/persona/set" => match persona_set_body(&body) {
            Ok(text) => (PanelRequest::PersonaSet(text), &shared.persona_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/memory-seed/set" => match memory_seed_set_body(&body) {
            Ok(document) => (
                PanelRequest::MemorySeedSet(document),
                &shared.memory_seed_tx,
            ),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/memory-seed/get" => match empty_object(&body) {
            Ok(()) => (PanelRequest::MemorySeedGet, &shared.memory_seed_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/topics/set" => match persona_set_body(&body) {
            Ok(text) => (PanelRequest::TopicsSet(text), &shared.persona_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/topics/get" => match empty_object(&body) {
            Ok(()) => (PanelRequest::TopicsGet, &shared.persona_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/persona/get" => match empty_object(&body) {
            Ok(()) => (PanelRequest::PersonaGet, &shared.persona_tx),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/speech-input/device" => match speech_input_device_body(&body) {
            Ok(device) => (
                PanelRequest::SpeechInputDevice(device),
                &shared.speech_input_tx,
            ),
            Err((status, detail)) => {
                shared.count(&shared.counters.rejected_request);
                return json(status, false, detail);
            }
        },
        "/api/speech-input/duplex" => match speech_input_duplex_body(&body) {
            Ok(full) => (
                PanelRequest::SpeechInputDuplex { full },
                &shared.speech_input_tx,
            ),
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

/// Parses `{"enabled": true|false}`: a strict boolean and nothing else, so a
/// request cannot name a device, a program, an argument or a trust level.
fn speech_input_body(body: &[u8]) -> Result<bool, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        enabled: bool,
    }
    serde_json::from_slice::<Body>(body)
        .map(|parsed| parsed.enabled)
        .map_err(|_| ("400 Bad Request", "expected {\"enabled\": boolean}"))
}

/// The seed document of `memory-seed.set` (#402): the body as UTF-8 text that
/// is a JSON object. The body is already bounded; the composition thread parses
/// it against the seed contract (unknown fields, bounds) before anything moves.
fn memory_seed_set_body(body: &[u8]) -> Result<PanelSecret, (&'static str, &'static str)> {
    const BAD: (&str, &str) = ("400 Bad Request", "expected a memory seed document");
    let text = std::str::from_utf8(body).map_err(|_| BAD)?;
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(_)) => Ok(PanelSecret(text.to_owned())),
        _ => Err(BAD),
    }
}

/// Parses `{"text": string}` and nothing else (#401). The body is already
/// bounded; the text is only checked for being a string here (the invalid UTF-8
/// of a raw body fails the JSON parse), the composition thread validates it.
fn persona_set_body(body: &[u8]) -> Result<PanelSecret, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        text: String,
    }
    serde_json::from_slice::<Body>(body)
        .map(|parsed| PanelSecret(parsed.text))
        .map_err(|_| ("400 Bad Request", "expected {\"text\": string}"))
}

/// Parses `{"device": "<name>"|null}`: a bounded name without control
/// characters, or null for the system default, and nothing else.
fn speech_input_device_body(body: &[u8]) -> Result<Option<String>, (&'static str, &'static str)> {
    const BAD: (&str, &str) = (
        "400 Bad Request",
        "expected {\"device\": string or null}, at most 128 bytes",
    );
    let parsed: serde_json::Value = serde_json::from_slice(body).map_err(|_| BAD)?;
    let object = parsed.as_object().filter(|o| o.len() == 1).ok_or(BAD)?;
    match object.get("device").ok_or(BAD)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(name) if aivtuber_domain::is_valid_speech_device_name(name) => {
            Ok(Some(name.clone()))
        }
        _ => Err(BAD),
    }
}

/// Parses `{"duplex": "half"|"full"}` and nothing else.
fn speech_input_duplex_body(body: &[u8]) -> Result<bool, (&'static str, &'static str)> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        duplex: String,
    }
    serde_json::from_slice::<Body>(body)
        .ok()
        .and_then(|parsed| aivtuber_domain::SpeechDuplex::parse(&parsed.duplex))
        .map(|duplex| duplex == aivtuber_domain::SpeechDuplex::Full)
        .ok_or((
            "400 Bad Request",
            "expected {\"duplex\": \"half\" or \"full\"}",
        ))
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
/// (bounded, a-z 0-9 . _ -, optionally vendor/model[:variant]); the allowlist lives with the composition thread.
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
        Ok(reply) => json_full(
            if reply.ok { "200 OK" } else { "409 Conflict" },
            reply.ok,
            &reply.detail,
            reply.id.as_deref(),
            reply.text.as_deref(),
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
mod tests;
