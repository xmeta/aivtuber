//! Stream-chat connections started from the operation panel (issue #361).
//!
//! The read-only sidecars (`twitch-chat-ingress` #257, `youtube-chat-ingress`
//! #293) used to work only when a shell piped their stdout into the app. This
//! module lets the operator connect a channel or a live video from the panel
//! (or `aivtuberctl`) instead: the app starts the sidecar itself, supervises
//! it and reads its stdout through the same raw-ingress path as piped input.
//!
//! * **Same path, same limits.** The child's stdout goes through
//!   [`pump_bounded_records`] (bounded line length, bounded queue, newest
//!   dropped and counted when full) and each record is then admitted by
//!   [`crate::ProductionApp::process_content_bytes`], so the app's own ingress
//!   ceiling (`AIVTUBER_INGRESS_MAX_TRUST`, `AIVTUBER_INGRESS_SOURCE_CLASSES`),
//!   envelope validation, plane check and rate limits apply exactly as they do
//!   to a stdin line. Nothing the sidecar says is trusted: before that, a line
//!   whose source, source class or trust is not what this platform's sidecar
//!   may produce (`untrusted` `public_chat`, plus `donation` for YouTube) is
//!   dropped and counted, whatever the environment would have allowed. A
//!   panel-started source can therefore never exceed what the panel could
//!   already do with chat.
//! * **No blocking.** Each pipe is read on its own thread and each platform has
//!   one bounded queue. The composition loop takes a few records per tick; a
//!   flood fills the queue and the newest lines are dropped with counters.
//! * **Supervision.** Program: the sidecar binary next to the app, by absolute
//!   path, arguments only (no shell), stdin null. A child that stops is
//!   restarted with capped exponential backoff and a bounded number of
//!   consecutive failures; exit code 2 (the sidecar's own "bad input, bad key
//!   or chat not available") and a clean exit are not retried. A child is
//!   killed on disconnect and when the app ends (Windows: the whole tree, by
//!   the absolute System32 `taskkill`, like `aivtuber-setup start`).
//! * **Authority.** The only entry points are `ProductionApp` methods that
//!   demand `ingress.control`, minted by `LocalControlIngress` and audited like
//!   stop and mute. The request names a platform and a validated target; it
//!   cannot name a program, an argument, a trust level or a source class.
//! * **The key.** The YouTube API key lives in its own file beside the settings
//!   (owner-only where the OS allows, atomic write) or in `YOUTUBE_API_KEY`,
//!   which wins. It is a `SecretString` from the moment it arrives, goes to the
//!   child through its environment only (never an argument), and is never in a
//!   response, a status, a log line or an audit record. The child's stderr is
//!   logged sanitized and bounded, with the key text replaced if it ever
//!   appeared there.

use crate::raw_ingress::{
    RawIngressConfig, RawIngressMetrics, RawIngressMetricsSnapshot, pump_bounded_records,
};
use crate::voice_control::{is_valid_key, read_key_file, write_atomic, write_key_file};
use aivtuber_adapters::{ReconnectPolicy, SecretString, parse_twitch_login, parse_youtube_video};
use aivtuber_domain::{SourceClass, TrustLevel};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Instant;

pub const CHAT_SETTINGS_VAR: &str = "AIVTUBER_CHAT_INGRESS_SETTINGS";
pub const YOUTUBE_KEY_FILE_VAR: &str = "AIVTUBER_YOUTUBE_KEY_FILE";
/// Operator-only override of the YouTube Data API origin (a fixture server or a
/// proxy). Not reachable from the panel or the operator endpoint.
pub const YOUTUBE_API_BASE_VAR: &str = "AIVTUBER_YOUTUBE_API_BASE";
/// The settings file is at most this large; a bigger one is ignored.
pub const MAX_CHAT_SETTINGS_BYTES: u64 = 1_024;
/// The one argument of `ingress.connect` / `ingress.disconnect`, as JSON text.
pub const MAX_INGRESS_ARGUMENT_BYTES: usize = 1_024;
/// Records one platform contributes per composition tick.
pub const CHAT_RECORDS_PER_TICK: usize = 8;

const SIDECAR_STDERR_LINE_BYTES: usize = 512;
const SIDECAR_STDERR_QUEUE: usize = 32;
const STDERR_LINES_PER_POLL: usize = 8;
const STDERR_LOGGED_PER_WINDOW: u32 = 20;
const STDERR_WINDOW_MS: u64 = 60_000;
/// After a child exits, how long its stderr reader may take to deliver the
/// last lines (normally a few ms) before the exit is reported without them.
const STDERR_DRAIN_MS: u64 = 500;
const ERROR_CHARS: usize = 160;
/// A child that has run this long counts as healthy: its failures are forgotten.
const HEALTHY_RUN_MS: u64 = 60_000;
const RATE_BUCKET_MS: u64 = 10_000;
const RATE_BUCKETS: usize = 6;

/// The exit code the sidecars use for "do not retry": bad arguments, a missing
/// or rejected key, a video with no live chat.
const SIDECAR_FATAL_EXIT: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatPlatform {
    Twitch,
    Youtube,
}

impl ChatPlatform {
    pub const ALL: [Self; 2] = [Self::Twitch, Self::Youtube];

    pub fn name(self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::Youtube => "youtube",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "twitch" => Some(Self::Twitch),
            "youtube" => Some(Self::Youtube),
            _ => None,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Twitch => 0,
            Self::Youtube => 1,
        }
    }

    /// The source id the platform's sidecar writes into its records.
    fn source_ok(self, source: &str) -> bool {
        match self {
            Self::Twitch => source
                .strip_prefix("twitch:")
                .is_some_and(|login| parse_twitch_login(login).is_ok_and(|l| l == login)),
            Self::Youtube => source == "youtube:live-chat",
        }
    }

    /// The most a panel-started source of this platform may declare.
    fn class_ok(self, class: SourceClass) -> bool {
        match self {
            Self::Twitch => class == SourceClass::PublicChat,
            Self::Youtube => matches!(class, SourceClass::PublicChat | SourceClass::Donation),
        }
    }

    fn parse_target(self, text: &str) -> Result<String, &'static str> {
        match self {
            Self::Twitch => parse_twitch_login(text),
            Self::Youtube => parse_youtube_video(text),
        }
    }
}

/// Why a line from a sidecar was dropped before the app's own admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRefusal {
    NotAnEnvelope,
    WrongSource,
    WrongClass,
    NotUntrusted,
}

/// The ceiling a panel-started source is held to, regardless of the
/// environment's own ingress policy (which then applies on top).
pub fn gate_line(platform: ChatPlatform, raw: &[u8]) -> Result<(), LineRefusal> {
    #[derive(Deserialize)]
    struct Header {
        source: String,
        source_class: SourceClass,
        trust_level: TrustLevel,
    }
    let header: Header = serde_json::from_slice(raw).map_err(|_| LineRefusal::NotAnEnvelope)?;
    if !platform.source_ok(&header.source) {
        return Err(LineRefusal::WrongSource);
    }
    if !platform.class_ok(header.source_class) {
        return Err(LineRefusal::WrongClass);
    }
    if header.trust_level != TrustLevel::Untrusted {
        return Err(LineRefusal::NotUntrusted);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatState {
    Disconnected,
    Connecting,
    Connected,
    Backoff,
    Failed,
}

/// One platform as the operator sees it. No key, no line content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatPlatformStatus {
    pub state: ChatState,
    /// The channel login or video id in use (or last used).
    pub target: Option<String>,
    /// The one saved by the last connect, offered as the next default. It is
    /// never connected automatically.
    pub saved_target: Option<String>,
    /// The sidecar program exists next to the app.
    pub sidecar_found: bool,
    pub messages_accepted: u64,
    /// Everything that did not become an accepted message: queue full, too
    /// long, refused by the ceiling or by the app's admission.
    pub messages_dropped: u64,
    pub dropped_queue_full: u64,
    pub dropped_refused: u64,
    pub messages_per_minute: u32,
    pub restarts: u32,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatKeyStatus {
    pub set: bool,
    /// `environment` (`YOUTUBE_API_KEY`, wins) or `file`.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatIngressStatus {
    pub twitch: ChatPlatformStatus,
    pub youtube: ChatPlatformStatus,
    pub youtube_key: ChatKeyStatus,
    /// Why the last choice could not be saved, if it could not.
    pub save_error: Option<String>,
}

// ---- settings file ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitch_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub youtube_video: Option<String>,
}

/// The saved settings, or the default when the file is missing, too large or
/// not the documented shape. A target that no longer validates is dropped.
pub fn read_chat_settings(path: &Path) -> ChatSettings {
    let read = || -> Option<ChatSettings> {
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_CHAT_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_CHAT_SETTINGS_BYTES {
            return None;
        }
        serde_json::from_slice(&bytes).ok()
    };
    let settings = read().unwrap_or_default();
    ChatSettings {
        twitch_channel: settings
            .twitch_channel
            .and_then(|v| parse_twitch_login(&v).ok()),
        youtube_video: settings
            .youtube_video
            .and_then(|v| parse_youtube_video(&v).ok()),
    }
}

fn write_chat_settings(path: &Path, settings: &ChatSettings) -> std::io::Result<()> {
    let text = serde_json::to_string(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    write_atomic(path, &format!("{text}\n"), false)
}

// ---- environment ----

#[derive(Debug, Clone)]
pub struct ChatIngressEnvironment {
    pub settings_path: PathBuf,
    pub key_file: PathBuf,
    /// `YOUTUBE_API_KEY`, when set: it wins over the key file.
    pub env_key: Option<SecretString>,
    pub twitch_program: PathBuf,
    pub youtube_program: PathBuf,
    /// Passed to the YouTube sidecar as `--api-base` (a fixture or a proxy).
    pub youtube_api_base: Option<String>,
    /// The same bounds as the piped stdin path.
    pub raw: RawIngressConfig,
    /// Delay before a stopped child is started again.
    pub restart: ReconnectPolicy,
    /// Consecutive stops (each shorter than a healthy run) before giving up.
    pub max_failures: u32,
    /// How long a running child is shown as `connecting` before `connected`
    /// (the sidecars have no handshake line; a first record also confirms it).
    pub connecting_ms: u64,
    /// A logical clock instead of the wall clock. `None` in production; a test
    /// moves it explicitly so the backoff and the connecting window never
    /// depend on how fast the machine runs.
    pub clock: Option<ManualChatClock>,
}

/// The supervisor's logical time, advanced only by the holder (tests).
#[derive(Debug, Clone, Default)]
pub struct ManualChatClock(Arc<AtomicU64>);

impl ManualChatClock {
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl ChatIngressEnvironment {
    /// The sidecars next to the app binary (`program_dir`).
    pub fn new(
        settings_path: PathBuf,
        key_file: PathBuf,
        env_key: Option<SecretString>,
        program_dir: &Path,
        youtube_api_base: Option<String>,
        raw: RawIngressConfig,
    ) -> Self {
        let program =
            |name: &str| program_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        Self {
            settings_path,
            key_file,
            env_key,
            twitch_program: program("twitch-chat-ingress"),
            youtube_program: program("youtube-chat-ingress"),
            youtube_api_base,
            raw,
            restart: ReconnectPolicy {
                initial_delay_ms: 2_000,
                max_delay_ms: 60_000,
            },
            max_failures: 6,
            connecting_ms: 3_000,
            clock: None,
        }
    }
}

/// An API origin the YouTube sidecar may be pointed at: `https`, or `http` on
/// loopback (a fixture server).
pub fn is_acceptable_api_base(base: &str) -> bool {
    let Ok(url) = url_host(base) else {
        return false;
    };
    match url.0.as_str() {
        "https" => true,
        "http" => matches!(url.1.as_str(), "127.0.0.1" | "localhost" | "[::1]"),
        _ => false,
    }
}

/// `(scheme, host)` of a URL, parsed by hand: this crate has no URL parser and
/// the check is deliberately narrow.
fn url_host(base: &str) -> Result<(String, String), ()> {
    let (scheme, rest) = base.split_once("://").ok_or(())?;
    let authority = rest.split(['/', '?', '#']).next().ok_or(())?;
    if authority.contains('@') || authority.is_empty() {
        return Err(());
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        let end = stripped.find(']').ok_or(())?;
        format!("[{}]", &stripped[..end])
    } else {
        authority.split(':').next().unwrap_or("").to_owned()
    };
    if host.is_empty() {
        return Err(());
    }
    Ok((scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

// ---- process handling ----

/// Ends a child on drop. Windows: the whole tree first, by the absolute
/// System32 `taskkill` (a bare `kill` would orphan grandchildren).
struct ChildGuard(Option<Child>);

fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot")
            .filter(|v| !v.is_empty())
            .map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
        let _ = Command::new(root.join("System32").join("taskkill.exe"))
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl ChildGuard {
    /// The exit status once the child has ended. The finished child is let go
    /// then, so dropping the guard later spawns no `taskkill` on a dead pid
    /// (the loop would wait for it).
    fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        let status = self.0.as_mut().and_then(|c| c.try_wait().ok().flatten())?;
        self.0 = None;
        Some(status)
    }

    /// Ends the child. `background` hands the (possibly slow) tree kill and
    /// wait to a short-lived thread so a panel disconnect never waits for it.
    fn end(&mut self, background: bool) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if background {
            let spawned = std::thread::Builder::new()
                .name("chat-ingress-reaper".to_owned())
                .spawn(move || kill_tree(&mut child));
            if spawned.is_err() {
                // The closure (and the child) went with the failed spawn; the
                // OS still ends it when the app exits (see `shutdown`).
                eprintln!("chat_ingress: could not start a reaper thread");
            }
        } else {
            kill_tree(&mut child);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.end(false);
    }
}

// ---- one platform ----

struct Running {
    guard: ChildGuard,
    lines: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
    pump: RawIngressMetrics,
    started_ms: u64,
    saw_record: bool,
    /// When the exit was first seen; the exit is reported once the stderr
    /// reader has reached end of stream (or [`STDERR_DRAIN_MS`] later), so the
    /// child's last diagnostic line is not lost to a thread race.
    exit_seen_ms: Option<u64>,
    /// The status the child ended with. The guard reports it once (and lets
    /// the child go), so it is kept here for the polls that wait for the
    /// stderr reader.
    exit_status: Option<ExitStatus>,
}

#[derive(Default)]
struct RateWindow {
    buckets: VecDeque<(u64, u32)>,
}

impl RateWindow {
    fn note(&mut self, now_ms: u64) {
        let bucket = now_ms / RATE_BUCKET_MS;
        match self.buckets.back_mut() {
            Some((id, count)) if *id == bucket => *count = count.saturating_add(1),
            _ => {
                self.buckets.push_back((bucket, 1));
                while self.buckets.len() > RATE_BUCKETS {
                    self.buckets.pop_front();
                }
            }
        }
    }

    /// Messages in the last minute (whole buckets, the current one included).
    fn per_minute(&self, now_ms: u64) -> u32 {
        let current = now_ms / RATE_BUCKET_MS;
        self.buckets
            .iter()
            .filter(|(id, _)| current.saturating_sub(*id) < RATE_BUCKETS as u64)
            .fold(0_u32, |sum, (_, count)| sum.saturating_add(*count))
    }
}

struct Slot {
    platform: ChatPlatform,
    state: ChatState,
    target: Option<String>,
    running: Option<Running>,
    /// Records a child wrote before it ended, still to be read.
    leftover: VecDeque<Vec<u8>>,
    accepted: u64,
    dropped_refused: u64,
    /// Pump drops of earlier runs of this connection (the current run's are
    /// read live from its metrics).
    pump_base: RawIngressMetricsSnapshot,
    restarts: u32,
    failures: u32,
    retry_at_ms: u64,
    last_error: Option<String>,
    last_diag: Option<String>,
    rate: RateWindow,
    stderr_window_start_ms: u64,
    stderr_logged: u32,
}

impl Slot {
    fn new(platform: ChatPlatform) -> Self {
        Self {
            platform,
            state: ChatState::Disconnected,
            target: None,
            running: None,
            leftover: VecDeque::new(),
            accepted: 0,
            dropped_refused: 0,
            pump_base: RawIngressMetricsSnapshot::default(),
            restarts: 0,
            failures: 0,
            retry_at_ms: 0,
            last_error: None,
            last_diag: None,
            rate: RateWindow::default(),
            stderr_window_start_ms: 0,
            stderr_logged: 0,
        }
    }

    fn pump_totals(&self) -> (u64, u64) {
        let live = self
            .running
            .as_ref()
            .map(|r| r.pump.snapshot())
            .unwrap_or_default();
        (
            self.pump_base.dropped_queue_full + live.dropped_queue_full,
            self.pump_base.dropped_oversize + live.dropped_oversize,
        )
    }

    /// Keep the finished run's drop counters, then forget the run.
    fn retire_run(&mut self) {
        if let Some(run) = self.running.take() {
            let snap = run.pump.snapshot();
            self.pump_base.dropped_queue_full += snap.dropped_queue_full;
            self.pump_base.dropped_oversize += snap.dropped_oversize;
        }
    }

    /// Moves what an ended child left in its queue aside (bounded by that
    /// queue's capacity) so the next turns still read it.
    fn keep_unread_records(&mut self) {
        if let Some(run) = self.running.as_ref() {
            self.leftover.extend(run.lines.try_iter());
        }
    }

    fn reset_counters(&mut self) {
        self.leftover.clear();
        self.accepted = 0;
        self.dropped_refused = 0;
        self.pump_base = RawIngressMetricsSnapshot::default();
        self.restarts = 0;
        self.failures = 0;
        self.rate = RateWindow::default();
        self.last_error = None;
        self.last_diag = None;
    }
}

/// A bounded, printable one-line version of `text`, with the key (if any)
/// replaced. Used for everything a sidecar wrote and everything shown.
fn sanitize(text: &str, key: Option<&str>) -> String {
    let mut cleaned: String = text.to_owned();
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        cleaned = cleaned.replace(key, "[redacted]");
    }
    cleaned
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(ERROR_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn is_stats_line(line: &str) -> bool {
    line.contains("_stats:")
}

#[derive(Clone, Copy)]
struct Timing {
    restart: ReconnectPolicy,
    max_failures: u32,
    connecting_ms: u64,
}

/// The key text to scrub from anything a child wrote (borrowed, never copied).
fn redaction_text<'a>(
    env_key: &'a Option<SecretString>,
    file_key: &'a Option<SecretString>,
) -> Option<&'a str> {
    env_key
        .as_ref()
        .or(file_key.as_ref())
        .map(SecretString::expose)
}

/// Diagnostics, exit handling and the connecting -> connected step for one
/// platform. A restart is [`ChatIngressController::respawn_if_due`].
fn poll_slot(slot: &mut Slot, timing: Timing, key: Option<&str>, now_ms: u64) {
    let platform = slot.platform;
    if let Some(run) = slot.running.as_mut() {
        for _ in 0..STDERR_LINES_PER_POLL {
            let Ok(raw) = run.stderr.try_recv() else {
                break;
            };
            let line = sanitize(&String::from_utf8_lossy(&raw), key);
            if line.is_empty() {
                continue;
            }
            if now_ms.saturating_sub(slot.stderr_window_start_ms) >= STDERR_WINDOW_MS {
                slot.stderr_window_start_ms = now_ms;
                slot.stderr_logged = 0;
            }
            if slot.stderr_logged < STDERR_LOGGED_PER_WINDOW {
                slot.stderr_logged += 1;
                eprintln!("chat_ingress: {} child: {line}", platform.name());
            }
            if !is_stats_line(&line) {
                slot.last_diag = Some(line);
            }
        }
    }
    if let Some(run) = slot.running.as_mut()
        && run.exit_status.is_none()
    {
        run.exit_status = run.guard.try_wait();
    }
    let exited = slot
        .running
        .as_ref()
        .and_then(|run| run.exit_status.map(|status| (status, run.started_ms)));
    if let Some((status, started_ms)) = exited {
        // What the child wrote before it ended may still be on its way from
        // the reader thread: wait (across polls, never blocking) until the
        // reader reports end of stream, with a bound.
        if let Some(run) = slot.running.as_mut() {
            let drained = loop {
                match run.stderr.try_recv() {
                    Ok(raw) => {
                        let line = sanitize(&String::from_utf8_lossy(&raw), key);
                        if !line.is_empty() && !is_stats_line(&line) {
                            slot.last_diag = Some(line);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break false,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
                }
            };
            let seen = *run.exit_seen_ms.get_or_insert(now_ms);
            if !drained && now_ms.saturating_sub(seen) < STDERR_DRAIN_MS {
                return;
            }
        }
        let ran_ms = now_ms.saturating_sub(started_ms);
        let code = status.code();
        let reason = slot.last_diag.clone().unwrap_or_else(|| match code {
            Some(code) => format!("the sidecar stopped (exit code {code})"),
            None => "the sidecar was stopped".to_owned(),
        });
        slot.keep_unread_records();
        slot.retire_run();
        eprintln!(
            "chat_ingress: {} sidecar exited code={code:?} after {ran_ms} ms",
            platform.name()
        );
        if status.success() {
            slot.state = ChatState::Disconnected;
            slot.last_error = Some(sanitize(&format!("the sidecar ended: {reason}"), key));
        } else if code == Some(SIDECAR_FATAL_EXIT) {
            slot.state = ChatState::Failed;
            slot.last_error = Some(sanitize(&reason, key));
        } else {
            if ran_ms >= HEALTHY_RUN_MS {
                slot.failures = 0;
            }
            slot.failures += 1;
            if slot.failures >= timing.max_failures {
                slot.state = ChatState::Failed;
                slot.last_error = Some(sanitize(
                    &format!("it kept stopping; gave up ({reason})"),
                    key,
                ));
            } else {
                slot.state = ChatState::Backoff;
                slot.retry_at_ms = now_ms.saturating_add(timing.restart.delay_ms(slot.failures));
                slot.last_error = Some(sanitize(&reason, key));
            }
        }
        return;
    }
    // Connecting -> connected: it has run a moment without ending, or it has
    // already produced a record.
    if slot.state == ChatState::Connecting
        && let Some(run) = slot.running.as_ref()
        && (run.saw_record || now_ms.saturating_sub(run.started_ms) >= timing.connecting_ms)
    {
        slot.state = ChatState::Connected;
    }
}

/// A record handed to the app, to admit like a stdin line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    pub platform: ChatPlatform,
    pub raw: Vec<u8>,
}

pub struct ChatIngressController {
    env: ChatIngressEnvironment,
    epoch: Instant,
    clock: Option<ManualChatClock>,
    slots: [Slot; 2],
    settings: ChatSettings,
    file_key: Option<SecretString>,
    save_error: Option<String>,
}

impl fmt::Debug for ChatIngressController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatIngressController")
            .field("twitch", &self.slots[0].state)
            .field("youtube", &self.slots[1].state)
            .finish_non_exhaustive()
    }
}

impl ChatIngressController {
    pub fn new(env: ChatIngressEnvironment) -> Self {
        let settings = read_chat_settings(&env.settings_path);
        let file_key = read_key_file(&env.key_file);
        Self {
            clock: env.clock.clone(),
            env,
            epoch: Instant::now(),
            slots: [
                Slot::new(ChatPlatform::Twitch),
                Slot::new(ChatPlatform::Youtube),
            ],
            settings,
            file_key,
            save_error: None,
        }
    }

    fn now_ms(&self) -> u64 {
        match &self.clock {
            Some(clock) => clock.0.load(Ordering::SeqCst),
            None => u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }

    fn slot(&self, platform: ChatPlatform) -> &Slot {
        &self.slots[platform.index()]
    }

    fn slot_mut(&mut self, platform: ChatPlatform) -> &mut Slot {
        &mut self.slots[platform.index()]
    }

    fn effective_key(&self) -> Option<(SecretString, &'static str)> {
        match (&self.env.env_key, &self.file_key) {
            (Some(key), _) => Some((key.clone(), "environment")),
            (None, Some(key)) => Some((key.clone(), "file")),
            (None, None) => None,
        }
    }

    fn program(&self, platform: ChatPlatform) -> &Path {
        match platform {
            ChatPlatform::Twitch => &self.env.twitch_program,
            ChatPlatform::Youtube => &self.env.youtube_program,
        }
    }

    pub fn status(&self) -> ChatIngressStatus {
        let now_ms = self.now_ms();
        let platform_status = |platform: ChatPlatform| {
            let slot = self.slot(platform);
            let (queue_full, oversize) = slot.pump_totals();
            let dropped_refused = slot.dropped_refused;
            ChatPlatformStatus {
                state: slot.state,
                target: slot.target.clone(),
                saved_target: match platform {
                    ChatPlatform::Twitch => self.settings.twitch_channel.clone(),
                    ChatPlatform::Youtube => self.settings.youtube_video.clone(),
                },
                sidecar_found: self.program(platform).is_file(),
                messages_accepted: slot.accepted,
                messages_dropped: queue_full
                    .saturating_add(oversize)
                    .saturating_add(dropped_refused),
                dropped_queue_full: queue_full,
                dropped_refused,
                messages_per_minute: slot.rate.per_minute(now_ms),
                restarts: slot.restarts,
                last_error: slot.last_error.clone(),
            }
        };
        ChatIngressStatus {
            twitch: platform_status(ChatPlatform::Twitch),
            youtube: platform_status(ChatPlatform::Youtube),
            youtube_key: ChatKeyStatus {
                set: self.effective_key().is_some(),
                source: self.effective_key().map(|(_, s)| s.to_owned()),
            },
            save_error: self.save_error.clone(),
        }
    }

    /// Process ids of the running sidecars (diagnostics, and the tests that
    /// check a disconnect leaves nothing behind). Not secret.
    pub fn child_ids(&self) -> Vec<u32> {
        self.slots
            .iter()
            .filter_map(|slot| slot.running.as_ref())
            .filter_map(|run| run.guard.0.as_ref().map(Child::id))
            .collect()
    }

    /// Test seam: a running connection fed by the returned sender instead of a
    /// child process, so the app-level admission can be tested without one.
    #[cfg(test)]
    pub(crate) fn attach_queue_for_test(
        &mut self,
        platform: ChatPlatform,
        target: &str,
    ) -> std::sync::mpsc::SyncSender<Vec<u8>> {
        let (line_tx, lines) = sync_channel(self.env.raw.queue_capacity);
        let (_, stderr) = sync_channel(1);
        let slot = self.slot_mut(platform);
        slot.target = Some(target.to_owned());
        slot.state = ChatState::Connected;
        slot.running = Some(Running {
            guard: ChildGuard(None),
            lines,
            stderr,
            pump: RawIngressMetrics::default(),
            started_ms: 0,
            saw_record: true,
            exit_seen_ms: None,
            exit_status: None,
        });
        line_tx
    }

    // ---- operator actions ----

    /// Start reading `platform`'s chat for `target` (a login, a video id or a
    /// link, parsed here). A running connection to the same target is left as
    /// it is; a different one is replaced.
    pub fn connect(&mut self, platform: ChatPlatform, target: &str) -> Result<String, String> {
        let target = platform
            .parse_target(target)
            .map_err(|reason| format!("refused: {reason}"))?;
        let now_ms = self.now_ms();
        {
            let slot = self.slot(platform);
            if slot.target.as_deref() == Some(target.as_str())
                && matches!(
                    slot.state,
                    ChatState::Connecting | ChatState::Connected | ChatState::Backoff
                )
            {
                return Ok(format!("already connected to {} {target}", platform.name()));
            }
        }
        let key = match platform {
            ChatPlatform::Twitch => None,
            ChatPlatform::Youtube => Some(
                self.effective_key()
                    .ok_or_else(|| "refused: set the YouTube API key first".to_owned())?
                    .0,
            ),
        };
        if !self.program(platform).is_file() {
            return Err(format!(
                "refused: the {} sidecar is not installed next to the app",
                platform.name()
            ));
        }
        // A replaced connection ends first.
        self.end_slot(platform, true);
        {
            let slot = self.slot_mut(platform);
            slot.reset_counters();
            slot.target = Some(target.clone());
        }
        match self.spawn(platform, &target, key.as_ref(), now_ms) {
            Ok(running) => {
                let slot = self.slot_mut(platform);
                slot.running = Some(running);
                slot.state = ChatState::Connecting;
            }
            Err(reason) => {
                let slot = self.slot_mut(platform);
                slot.state = ChatState::Failed;
                slot.last_error = Some(reason.clone());
                return Err(format!("refused: {reason}"));
            }
        }
        match platform {
            ChatPlatform::Twitch => self.settings.twitch_channel = Some(target.clone()),
            ChatPlatform::Youtube => self.settings.youtube_video = Some(target.clone()),
        }
        self.save();
        let detail = format!("connecting to {} {target}", platform.name());
        eprintln!("chat_ingress: {detail}");
        Ok(detail)
    }

    pub fn disconnect(&mut self, platform: ChatPlatform) -> Result<String, String> {
        if self.slot(platform).state == ChatState::Disconnected {
            return Ok(format!("{} is not connected", platform.name()));
        }
        self.end_slot(platform, true);
        let detail = format!("disconnected from {}", platform.name());
        eprintln!("chat_ingress: {detail}");
        Ok(detail)
    }

    /// Store the YouTube API key in its own file. `YOUTUBE_API_KEY`, when set,
    /// still wins; a running connection keeps the key it started with.
    pub fn set_key(&mut self, key: &str) -> Result<String, String> {
        let key = key.trim();
        if !is_valid_key(key) {
            return Err("refused: the key is empty, too long or has whitespace".to_owned());
        }
        write_key_file(&self.env.key_file, key)
            .map_err(|error| format!("refused: cannot store the key ({:?})", error.kind()))?;
        self.file_key = Some(SecretString::new(key));
        Ok(if self.env.env_key.is_some() {
            "key set (the YOUTUBE_API_KEY environment variable is in use and takes precedence)"
                .to_owned()
        } else {
            "key set; it is used the next time YouTube is connected".to_owned()
        })
    }

    /// Forget the stored key. Without one (and no `YOUTUBE_API_KEY`) a YouTube
    /// connection is ended.
    pub fn remove_key(&mut self) -> Result<String, String> {
        match std::fs::remove_file(&self.env.key_file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "refused: cannot remove the key ({:?})",
                    error.kind()
                ));
            }
        }
        self.file_key = None;
        if self.env.env_key.is_some() {
            return Ok(
                "key removed (the YOUTUBE_API_KEY environment variable is still set)".to_owned(),
            );
        }
        if self.slot(ChatPlatform::Youtube).state != ChatState::Disconnected {
            self.end_slot(ChatPlatform::Youtube, true);
            return Ok("key removed; YouTube chat disconnected".to_owned());
        }
        Ok("key removed".to_owned())
    }

    fn save(&mut self) {
        self.save_error = write_chat_settings(&self.env.settings_path, &self.settings)
            .err()
            .map(|error| format!("cannot save the choice ({:?})", error.kind()));
    }

    fn spawn(
        &self,
        platform: ChatPlatform,
        target: &str,
        key: Option<&SecretString>,
        now_ms: u64,
    ) -> Result<Running, String> {
        let mut command = Command::new(self.program(platform));
        match platform {
            ChatPlatform::Twitch => {
                command.args(["--channel", target]);
            }
            ChatPlatform::Youtube => {
                command.args(["--video-id", target]);
                if let Some(base) = &self.env.youtube_api_base {
                    command.args(["--api-base", base]);
                }
            }
        }
        // Secrets the child has no use for stay out of its environment; the
        // YouTube key is set for that one child only.
        command
            .env_remove("YOUTUBE_API_KEY")
            .env_remove("AIVIS_API")
            .env_remove("AIVTUBER_CONTROL_SECRET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(key) = key {
            command.env("YOUTUBE_API_KEY", key.expose());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start the sidecar ({:?})", error.kind()))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let mut guard = ChildGuard(Some(child));
            guard.end(false);
            return Err("cannot read the sidecar's output".to_owned());
        };
        let mut guard = ChildGuard(Some(child));
        let (line_tx, lines) = sync_channel(self.env.raw.queue_capacity);
        let (err_tx, stderr_rx) = sync_channel(SIDECAR_STDERR_QUEUE);
        let pump = RawIngressMetrics::default();
        let stdout_pump = pump.clone();
        let raw = self.env.raw;
        let name = platform.name();
        let spawned = std::thread::Builder::new()
            .name(format!("chat-ingress-{name}-out"))
            .spawn(move || {
                if let Err(error) =
                    pump_bounded_records(BufReader::new(stdout), &line_tx, raw, &stdout_pump)
                {
                    eprintln!("chat_ingress: {name} reader_error={error}");
                }
            })
            .and_then(|_| {
                std::thread::Builder::new()
                    .name(format!("chat-ingress-{name}-err"))
                    .spawn(move || {
                        let _ = pump_bounded_records(
                            BufReader::new(stderr),
                            &err_tx,
                            RawIngressConfig {
                                max_record_bytes: SIDECAR_STDERR_LINE_BYTES,
                                queue_capacity: SIDECAR_STDERR_QUEUE,
                            },
                            &RawIngressMetrics::default(),
                        );
                    })
            });
        if spawned.is_err() {
            guard.end(false);
            return Err("cannot start the sidecar's reader threads".to_owned());
        }
        Ok(Running {
            guard,
            lines,
            stderr: stderr_rx,
            pump,
            started_ms: now_ms,
            saw_record: false,
            exit_seen_ms: None,
            exit_status: None,
        })
    }

    fn end_slot(&mut self, platform: ChatPlatform, background: bool) {
        let slot = self.slot_mut(platform);
        if let Some(run) = slot.running.as_mut() {
            run.guard.end(background);
        }
        slot.retire_run();
        slot.state = ChatState::Disconnected;
    }

    // ---- composition loop ----

    /// One loop turn: notice exits, restart after the backoff, read the
    /// children's diagnostics, and hand back up to `max_per_platform` records
    /// per platform that passed the panel-source ceiling. Never blocks.
    pub fn take_records(&mut self, max_per_platform: usize) -> Vec<ChatRecord> {
        let now_ms = self.now_ms();
        let timing = Timing {
            restart: self.env.restart,
            max_failures: self.env.max_failures,
            connecting_ms: self.env.connecting_ms,
        };
        let mut out = Vec::new();
        for platform in ChatPlatform::ALL {
            let key = redaction_text(&self.env.env_key, &self.file_key);
            poll_slot(&mut self.slots[platform.index()], timing, key, now_ms);
            self.respawn_if_due(platform, now_ms);
            let slot = &mut self.slots[platform.index()];
            let mut taken = 0;
            while taken < max_per_platform {
                let next = slot.leftover.pop_front().or_else(|| {
                    slot.running
                        .as_ref()
                        .and_then(|run| run.lines.try_recv().ok())
                });
                let Some(raw) = next else {
                    break;
                };
                if raw.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                taken += 1;
                if let Some(run) = slot.running.as_mut() {
                    run.saw_record = true;
                }
                match gate_line(platform, &raw) {
                    Ok(()) => out.push(ChatRecord { platform, raw }),
                    Err(_) => slot.dropped_refused += 1,
                }
            }
        }
        out
    }

    /// The app's verdict on a record from [`Self::take_records`].
    pub fn note_outcome(&mut self, platform: ChatPlatform, accepted: bool) {
        let now_ms = self.now_ms();
        let slot = self.slot_mut(platform);
        if accepted {
            slot.accepted += 1;
            slot.rate.note(now_ms);
        } else {
            slot.dropped_refused += 1;
        }
    }

    fn respawn_if_due(&mut self, platform: ChatPlatform, now_ms: u64) {
        let slot = &self.slots[platform.index()];
        if slot.state != ChatState::Backoff || now_ms < slot.retry_at_ms {
            return;
        }
        let Some(target) = slot.target.clone() else {
            self.slot_mut(platform).state = ChatState::Disconnected;
            return;
        };
        let key = match platform {
            ChatPlatform::Twitch => None,
            ChatPlatform::Youtube => self.effective_key().map(|(key, _)| key),
        };
        if platform == ChatPlatform::Youtube && key.is_none() {
            let slot = self.slot_mut(platform);
            slot.state = ChatState::Failed;
            slot.last_error = Some("no YouTube API key is set".to_owned());
            return;
        }
        let (max_failures, restart) = (self.env.max_failures, self.env.restart);
        match self.spawn(platform, &target, key.as_ref(), now_ms) {
            Ok(running) => {
                let slot = self.slot_mut(platform);
                slot.restarts += 1;
                slot.running = Some(running);
                slot.state = ChatState::Connecting;
            }
            Err(reason) => {
                let slot = self.slot_mut(platform);
                slot.failures += 1;
                slot.last_error = Some(sanitize(&reason, None));
                if slot.failures >= max_failures {
                    slot.state = ChatState::Failed;
                } else {
                    slot.retry_at_ms = now_ms.saturating_add(restart.delay_ms(slot.failures));
                }
            }
        }
    }

    /// End both children now and wait for them (the app is ending).
    pub fn shutdown(&mut self) {
        for platform in ChatPlatform::ALL {
            if self.slot(platform).running.is_some() {
                self.end_slot(platform, false);
            }
        }
    }
}

impl Drop for ChatIngressController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The JSON argument of `ingress.connect`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectArgument {
    platform: String,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectArgument {
    platform: String,
}

fn bounded(argument: Option<&str>) -> Result<&str, String> {
    let text = argument.unwrap_or("");
    if text.len() > MAX_INGRESS_ARGUMENT_BYTES {
        return Err("refused: the request is too large".to_owned());
    }
    Ok(text)
}

fn platform_of(name: &str) -> Result<ChatPlatform, String> {
    ChatPlatform::parse(name)
        .ok_or_else(|| "refused: platform must be twitch or youtube".to_owned())
}

/// `ingress.connect` argument -> (platform, target text).
pub fn parse_connect_argument(argument: Option<&str>) -> Result<(ChatPlatform, String), String> {
    let parsed: ConnectArgument = serde_json::from_str(bounded(argument)?)
        .map_err(|_| "refused: expected {\"platform\", \"target\"}".to_owned())?;
    Ok((platform_of(&parsed.platform)?, parsed.target))
}

/// `ingress.disconnect` argument -> platform.
pub fn parse_disconnect_argument(argument: Option<&str>) -> Result<ChatPlatform, String> {
    let parsed: DisconnectArgument = serde_json::from_str(bounded(argument)?)
        .map_err(|_| "refused: expected {\"platform\"}".to_owned())?;
    platform_of(&parsed.platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(source: &str, class: &str, trust: &str) -> Vec<u8> {
        format!(
            "{{\"schema_version\":\"0.2.0\",\"source\":\"{source}\",\"source_class\":\"{class}\",\"trust_level\":\"{trust}\",\"plane\":\"content\"}}"
        )
        .into_bytes()
    }

    /// #373: the guard reports a child's exit once. An exit first seen while
    /// the stderr reader was still open used to be lost on the next poll, so
    /// the slot stayed `connected` with a dead child forever.
    #[test]
    fn an_exit_seen_before_the_stderr_reader_ends_is_still_handled() {
        use std::time::Duration;
        // The test binary itself, with a flag it rejects: ends at once, non-zero.
        let child = Command::new(std::env::current_exe().expect("exe"))
            .arg("--no-such-flag")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let (_line_tx, lines) = sync_channel(1);
        // Held open for the whole test: the reader never reaches end of stream.
        let (_err_tx, stderr) = sync_channel(1);
        let mut slot = Slot::new(ChatPlatform::Twitch);
        slot.state = ChatState::Connected;
        slot.target = Some("alpha_one".to_owned());
        slot.running = Some(Running {
            guard: ChildGuard(Some(child)),
            lines,
            stderr,
            pump: RawIngressMetrics::default(),
            started_ms: 0,
            saw_record: true,
            exit_seen_ms: None,
            exit_status: None,
        });
        let timing = Timing {
            restart: ReconnectPolicy {
                initial_delay_ms: 40,
                max_delay_ms: 200,
            },
            max_failures: 3,
            connecting_ms: 100,
        };
        // The logical clock stays at 0 while the OS ends the child; the only
        // wall-clock bound here is a hang guard.
        let guard = Instant::now() + Duration::from_secs(120);
        while slot
            .running
            .as_ref()
            .is_some_and(|run| run.exit_seen_ms.is_none())
        {
            poll_slot(&mut slot, timing, None, 0);
            assert!(Instant::now() < guard, "the child never ended");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(slot.state, ChatState::Connected, "still waiting for stderr");
        poll_slot(&mut slot, timing, None, STDERR_DRAIN_MS - 1);
        assert_eq!(slot.state, ChatState::Connected, "inside the drain bound");
        poll_slot(&mut slot, timing, None, STDERR_DRAIN_MS);
        assert_eq!(slot.state, ChatState::Backoff);
        assert!(slot.running.is_none());
        assert_eq!(slot.failures, 1);
    }

    #[test]
    fn the_panel_source_ceiling_is_untrusted_chat_from_that_platform_only() {
        let ok = |p, r: &[u8]| gate_line(p, r);
        assert_eq!(
            ok(
                ChatPlatform::Twitch,
                &record("twitch:somechan", "public_chat", "untrusted")
            ),
            Ok(())
        );
        assert_eq!(
            ok(
                ChatPlatform::Youtube,
                &record("youtube:live-chat", "donation", "untrusted")
            ),
            Ok(())
        );
        // Donation is YouTube's: Twitch has no such class.
        assert_eq!(
            ok(
                ChatPlatform::Twitch,
                &record("twitch:somechan", "donation", "untrusted")
            ),
            Err(LineRefusal::WrongClass)
        );
        for class in ["speech", "game", "stream", "timer", "operator", "system"] {
            assert_eq!(
                ok(
                    ChatPlatform::Youtube,
                    &record("youtube:live-chat", class, "untrusted")
                ),
                Err(LineRefusal::WrongClass),
                "{class}"
            );
        }
        for trust in ["semi_trusted", "trusted"] {
            assert_eq!(
                ok(
                    ChatPlatform::Twitch,
                    &record("twitch:somechan", "public_chat", trust)
                ),
                Err(LineRefusal::NotUntrusted),
                "{trust}"
            );
        }
        // A sidecar that claims another source is not what was started.
        for (platform, source) in [
            (ChatPlatform::Twitch, "youtube:live-chat"),
            (ChatPlatform::Twitch, "local-panel"),
            (ChatPlatform::Twitch, "twitch:Bad Name"),
            (ChatPlatform::Youtube, "twitch:somechan"),
            (ChatPlatform::Youtube, "operator"),
        ] {
            assert_eq!(
                ok(platform, &record(source, "public_chat", "untrusted")),
                Err(LineRefusal::WrongSource),
                "{source}"
            );
        }
        assert_eq!(
            ok(ChatPlatform::Twitch, b"not json"),
            Err(LineRefusal::NotAnEnvelope)
        );
        assert_eq!(
            ok(ChatPlatform::Twitch, b"{}"),
            Err(LineRefusal::NotAnEnvelope)
        );
    }

    #[test]
    fn the_api_base_override_is_https_or_loopback() {
        for ok in [
            "https://www.googleapis.com",
            "https://proxy.example/youtube",
            "http://127.0.0.1:8080",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            assert!(is_acceptable_api_base(ok), "{ok}");
        }
        for bad in [
            "",
            "http://example.com",
            "http://127.0.0.1.evil.example",
            "http://user@127.0.0.1",
            "ftp://127.0.0.1",
            "127.0.0.1:8080",
            "https://",
            "javascript:alert(1)",
        ] {
            assert!(!is_acceptable_api_base(bad), "{bad}");
        }
    }

    #[test]
    fn stderr_text_is_bounded_printable_and_never_holds_the_key() {
        let key = "AIza-SENTINEL-KEY-0123456789";
        let text = format!("error: key={key}\u{1b}[31m\r\n{}", "x".repeat(1_000));
        let cleaned = sanitize(&text, Some(key));
        assert!(!cleaned.contains(key));
        assert!(cleaned.contains("[redacted]"));
        assert!(!cleaned.chars().any(char::is_control));
        assert!(cleaned.chars().count() <= ERROR_CHARS);
    }

    #[test]
    fn the_rate_window_counts_the_last_minute() {
        let mut rate = RateWindow::default();
        for t in [0, 1_000, 11_000, 25_000, 59_000] {
            rate.note(t);
        }
        assert_eq!(rate.per_minute(59_000), 5);
        assert_eq!(
            rate.per_minute(60_000),
            3,
            "the 0-10 s bucket just aged out"
        );
        assert_eq!(
            rate.per_minute(70_000),
            2,
            "and the 10-20 s bucket after it"
        );
        assert_eq!(rate.per_minute(300_000), 0);
        for t in 0..1_000 {
            rate.note(t * 10_000);
        }
        assert!(rate.buckets.len() <= RATE_BUCKETS);
    }

    #[test]
    fn arguments_are_strict_and_bounded() {
        assert_eq!(
            parse_connect_argument(Some("{\"platform\":\"twitch\",\"target\":\"abc\"}")),
            Ok((ChatPlatform::Twitch, "abc".to_owned()))
        );
        for bad in [
            None,
            Some(""),
            Some("{}"),
            Some("{\"platform\":\"kick\",\"target\":\"abc\"}"),
            Some("{\"platform\":\"twitch\",\"target\":\"abc\",\"program\":\"cmd\"}"),
            Some("[\"twitch\"]"),
        ] {
            assert!(parse_connect_argument(bad).is_err(), "{bad:?}");
        }
        let huge = format!(
            "{{\"platform\":\"twitch\",\"target\":\"{}\"}}",
            "a".repeat(MAX_INGRESS_ARGUMENT_BYTES)
        );
        assert!(parse_connect_argument(Some(&huge)).is_err());
        assert_eq!(
            parse_disconnect_argument(Some("{\"platform\":\"youtube\"}")),
            Ok(ChatPlatform::Youtube)
        );
        assert!(parse_disconnect_argument(Some("{\"platform\":\"x\"}")).is_err());
    }

    #[test]
    fn settings_round_trip_and_drop_what_no_longer_validates() {
        let dir =
            std::env::temp_dir().join(format!("aivtuber-chat-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chat.json");
        assert_eq!(read_chat_settings(&path), ChatSettings::default());
        let saved = ChatSettings {
            twitch_channel: Some("somechan".to_owned()),
            youtube_video: Some("dQw4w9WgXcQ".to_owned()),
        };
        write_chat_settings(&path, &saved).unwrap();
        assert_eq!(read_chat_settings(&path), saved);
        std::fs::write(
            &path,
            "{\"twitch_channel\":\"x; rm -rf\",\"youtube_video\":\"nope\"}",
        )
        .unwrap();
        assert_eq!(read_chat_settings(&path), ChatSettings::default());
        std::fs::write(&path, "x".repeat(2_000)).unwrap();
        assert_eq!(read_chat_settings(&path), ChatSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
