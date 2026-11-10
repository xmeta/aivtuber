//! Read-only YouTube Live chat ingress for the `youtube-chat-ingress` sidecar (#293).
//!
//! Source: the official YouTube Data API v3 `liveChatMessages.list` (polling,
//! API key, public data only). Provider DTOs stop at this module. What leaves
//! it is the existing raw-ingress wire format, a serialized domain
//! [`EventEnvelope`], so `aivtuber-app` reads it through the unchanged bounded
//! stdin path and the operator ingress ceiling (`IngressPolicy`).
//!
//! Trust boundary: chat is always declared `chat.message` / `public_chat` /
//! `content` / `untrusted` with no `authorization`; paid events are
//! `chat.donation` / `donation` / `content` / `untrusted`. Nothing the viewer
//! writes (text, owner/moderator flags) influences those declarations. The
//! ceiling is enforced by the app, not here; this module only guarantees it
//! never asks for more.
//!
//! Secrets: the API key is wrapped in [`YoutubeApiKey`] (redacted `Debug`, no
//! `Display`), sent in the `x-goog-api-key` header so it is never part of a
//! URL, and scrubbed from any error text before it is returned.

use crate::{EnqueueOutcome, HttpResponse, HttpTransportError, ReconnectPolicy};
use aivtuber_domain::{
    EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, SecurityPlane, SourceClass, TrustLevel,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::io::Read;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

pub const DEFAULT_YOUTUBE_API_BASE: &str = "https://www.googleapis.com";
/// Mirrors `SecurityRuntimeConfig::default()`; the app crate pins them together.
pub const YOUTUBE_DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024;
pub const YOUTUBE_DEFAULT_MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// YouTube caps a chat message at 200 characters; anything over this is not a
/// real message and is rejected whole, never truncated.
pub const YOUTUBE_DEFAULT_MAX_TEXT_CHARS: usize = 500;
/// Bound on remembered message ids (oldest evicted first).
pub const YOUTUBE_DEDUPE_CAPACITY: usize = 4096;
/// Operator floor under the API's `pollingIntervalMillis`; see the quota math
/// in docs/production-runtime.adoc.
pub const YOUTUBE_DEFAULT_MIN_POLL_MS: u64 = 5_000;
const MAX_POLL_WAIT_MS: u64 = 600_000;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ITEMS_PER_PAGE: usize = 2_000;
const MAX_MESSAGE_ID_BYTES: usize = 256;
const MAX_SHORT_FIELD: usize = 64;

/// Transient failures (transport, 5xx): 2 s doubling to 2 min.
pub const YOUTUBE_ERROR_BACKOFF: ReconnectPolicy = ReconnectPolicy {
    initial_delay_ms: 2_000,
    max_delay_ms: 120_000,
};
/// Quota / rate-limit 403 and 429: 1 min doubling to 15 min. Daily quota
/// resets at midnight Pacific Time, so hammering earlier is pointless.
pub const YOUTUBE_QUOTA_BACKOFF: ReconnectPolicy = ReconnectPolicy {
    initial_delay_ms: 60_000,
    max_delay_ms: 900_000,
};

/// Operator-supplied API key. Redacted in `Debug`; there is deliberately no
/// `Display`, `Serialize` or `Clone`-to-string path.
pub struct YoutubeApiKey(String);

impl YoutubeApiKey {
    pub fn new(key: &str) -> Result<Self, String> {
        let key = key.trim();
        let valid =
            !key.is_empty() && key.len() <= 256 && key.bytes().all(|b| b.is_ascii_graphic());
        if valid {
            Ok(Self(key.to_owned()))
        } else {
            Err("YouTube API key must be 1-256 printable ASCII characters".to_owned())
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Remove every occurrence of the key from `text`.
    pub fn redact(&self, text: &str) -> String {
        text.replace(self.0.as_str(), "[REDACTED]")
    }
}

impl fmt::Debug for YoutubeApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("YoutubeApiKey(<redacted>)")
    }
}

/// Video ids are 11 characters of `[A-Za-z0-9_-]`.
pub fn validate_video_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    let ok = id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(id.to_owned())
    } else {
        Err(format!("invalid YouTube video id: {id:?}"))
    }
}

pub fn validate_live_chat_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    let ok = !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'=' | b'.'));
    if ok {
        Ok(id.to_owned())
    } else {
        Err(format!("invalid YouTube live chat id: {id:?}"))
    }
}

// ---------------------------------------------------------------------------
// HTTP boundary
// ---------------------------------------------------------------------------

/// One authenticated GET. The key travels as a header, never in `url`.
pub trait YoutubeHttp {
    fn get(
        &self,
        url: &str,
        api_key: &YoutubeApiKey,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError>;
}

#[derive(Clone)]
pub struct UreqYoutubeHttp {
    agent: ureq::Agent,
}

impl Default for UreqYoutubeHttp {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
        }
    }
}

impl fmt::Debug for UreqYoutubeHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqYoutubeHttp")
    }
}

impl YoutubeHttp for UreqYoutubeHttp {
    fn get(
        &self,
        url: &str,
        api_key: &YoutubeApiKey,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError> {
        // Same endpoint policy as every other provider call: TLS for remote
        // hosts, plain HTTP only for loopback (the fake server in tests).
        if !crate::http::is_secure_provider_endpoint(url) {
            return Err(HttpTransportError::Unavailable(
                "YouTube endpoint must use HTTPS or loopback HTTP without embedded credentials"
                    .to_owned(),
            ));
        }
        let mut config = self
            .agent
            .get(url)
            .config()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0);
        if url::Url::parse(url)
            .is_ok_and(|u| matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            config = config.proxy(None);
        }
        let request = config.build().header("x-goog-api-key", api_key.expose());
        let mut response = request.call().map_err(|error| match error {
            ureq::Error::Timeout(_) => HttpTransportError::Timeout,
            other => HttpTransportError::Unavailable(api_key.redact(&other.to_string())),
        })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES + 1)
            .reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|e| HttpTransportError::Unavailable(api_key.redact(&e.to_string())))?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(HttpTransportError::Unavailable(
                "YouTube response exceeded the byte limit".to_owned(),
            ));
        }
        Ok(HttpResponse {
            status,
            body,
            content_type,
        })
    }
}

/// Reasons from `error.errors[].reason`; the body is never printed.
fn error_reasons(body: &[u8]) -> Vec<String> {
    #[derive(Deserialize)]
    struct Detail {
        reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct Inner {
        #[serde(default)]
        errors: Vec<Detail>,
    }
    #[derive(Deserialize)]
    struct Outer {
        error: Option<Inner>,
    }
    serde_json::from_slice::<Outer>(body)
        .ok()
        .and_then(|o| o.error)
        .map(|e| e.errors.into_iter().filter_map(|d| d.reason).collect())
        .unwrap_or_default()
}

fn short_reason(reasons: &[String]) -> String {
    reasons
        .first()
        .filter(|r| r.len() <= MAX_SHORT_FIELD && r.bytes().all(|b| b.is_ascii_alphanumeric()))
        .cloned()
        .unwrap_or_else(|| "unspecified".to_owned())
}

/// Resolve `videos.list?part=liveStreamingDetails` to the active live chat id
/// (1 quota unit).
pub fn resolve_live_chat_id<H: YoutubeHttp>(
    http: &H,
    api_base: &str,
    api_key: &YoutubeApiKey,
    video_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    let video_id = validate_video_id(video_id)?;
    let mut url = url::Url::parse(api_base).map_err(|_| "invalid API base URL".to_owned())?;
    url.set_path("/youtube/v3/videos");
    url.query_pairs_mut()
        .append_pair("part", "liveStreamingDetails")
        .append_pair("id", &video_id);
    let response = http
        .get(url.as_str(), api_key, timeout)
        .map_err(|e| api_key.redact(&e.to_string()))?;
    if response.status != 200 {
        return Err(format!(
            "videos.list failed: HTTP {} ({})",
            response.status,
            short_reason(&error_reasons(&response.body))
        ));
    }
    #[derive(Deserialize)]
    struct Details {
        #[serde(rename = "activeLiveChatId")]
        active_live_chat_id: Option<String>,
    }
    #[derive(Deserialize)]
    struct Video {
        #[serde(rename = "liveStreamingDetails")]
        details: Option<Details>,
    }
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Video>,
    }
    let list: List = serde_json::from_slice(&response.body)
        .map_err(|_| "videos.list returned an unparseable body".to_owned())?;
    let id = list
        .items
        .into_iter()
        .next()
        .ok_or_else(|| "video not found (or not visible to this API key)".to_owned())?
        .details
        .and_then(|d| d.active_live_chat_id)
        .ok_or_else(|| "video has no active live chat (not live, or chat disabled)".to_owned())?;
    validate_live_chat_id(&id)
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YoutubeIngressLimits {
    pub max_record_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_text_chars: usize,
}

impl Default for YoutubeIngressLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: YOUTUBE_DEFAULT_MAX_RECORD_BYTES,
            max_payload_bytes: YOUTUBE_DEFAULT_MAX_PAYLOAD_BYTES,
            max_text_chars: YOUTUBE_DEFAULT_MAX_TEXT_CHARS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YoutubeSkip {
    Oversize,
    /// Missing or ill-typed fields; counted and dropped.
    Malformed,
    /// New member / milestone / gifting. The wire format has no membership
    /// kind, so it is counted, not forwarded (future work).
    Membership,
    /// Tombstone, ban, mode change, poll and other non-chat items.
    NotChat,
    /// `chatEndedEvent`: the poller stops after this page.
    ChatEnded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YoutubeMapOutcome {
    Record {
        line: String,
        /// Super Chat / Super Sticker (`chat.donation`).
        paid: bool,
    },
    Skipped(YoutubeSkip),
}

#[derive(Deserialize, Default)]
struct Item {
    id: Option<String>,
    snippet: Option<Snippet>,
    #[serde(rename = "authorDetails")]
    author: Option<Author>,
}

#[derive(Deserialize, Default)]
struct Snippet {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "publishedAt")]
    published_at: Option<String>,
    #[serde(rename = "textMessageDetails")]
    text: Option<TextDetails>,
    #[serde(rename = "superChatDetails")]
    super_chat: Option<PaidDetails>,
    #[serde(rename = "superStickerDetails")]
    super_sticker: Option<PaidDetails>,
}

#[derive(Deserialize, Default)]
struct TextDetails {
    #[serde(rename = "messageText")]
    message_text: Option<String>,
}

#[derive(Deserialize, Default)]
struct PaidDetails {
    #[serde(rename = "amountMicros")]
    amount_micros: Option<Value>,
    currency: Option<String>,
    #[serde(rename = "amountDisplayString")]
    amount_display: Option<String>,
    #[serde(rename = "userComment")]
    user_comment: Option<String>,
    tier: Option<u32>,
    #[serde(rename = "superStickerMetadata")]
    sticker: Option<StickerMeta>,
}

#[derive(Deserialize, Default)]
struct StickerMeta {
    #[serde(rename = "altText")]
    alt_text: Option<String>,
}

#[derive(Deserialize, Default)]
struct Author {
    #[serde(rename = "channelId")]
    channel_id: Option<String>,
    #[serde(rename = "isChatOwner")]
    is_chat_owner: Option<bool>,
    #[serde(rename = "isChatModerator")]
    is_chat_moderator: Option<bool>,
    #[serde(rename = "isChatSponsor")]
    is_chat_sponsor: Option<bool>,
    #[serde(rename = "isVerified")]
    is_verified: Option<bool>,
}

fn message_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_MESSAGE_ID_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
}

fn micros(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Maps `liveChatMessages.list` items to raw-ingress records.
#[derive(Debug)]
pub struct YoutubeRecordMapper {
    limits: YoutubeIngressLimits,
    /// Logical ordering counter; never a wall-clock value.
    sequence: u64,
}

impl YoutubeRecordMapper {
    pub fn new(limits: YoutubeIngressLimits) -> Self {
        Self {
            limits,
            sequence: 0,
        }
    }

    pub fn map_item(&mut self, raw: &Value) -> YoutubeMapOutcome {
        use YoutubeMapOutcome::Skipped;
        let Ok(item) = serde_json::from_value::<Item>(raw.clone()) else {
            return Skipped(YoutubeSkip::Malformed);
        };
        let (Some(id), Some(snippet)) = (item.id, item.snippet) else {
            return Skipped(YoutubeSkip::Malformed);
        };
        if !message_id_ok(&id) {
            return Skipped(YoutubeSkip::Malformed);
        }
        let kind = snippet.kind.clone().unwrap_or_default();
        let author = item.author.unwrap_or_default();

        let mut payload: BTreeMap<String, Value> = BTreeMap::new();
        payload.insert("message_id".to_owned(), json!(id));
        let event_kind;
        match kind.as_str() {
            "textMessageEvent" => {
                let Some(text) = snippet.text.and_then(|t| t.message_text) else {
                    return Skipped(YoutubeSkip::Malformed);
                };
                if text.trim().is_empty() {
                    return Skipped(YoutubeSkip::Malformed);
                }
                if text.chars().count() > self.limits.max_text_chars {
                    return Skipped(YoutubeSkip::Oversize);
                }
                payload.insert("text".to_owned(), Value::String(text));
                // Audit data only: nothing reads these flags to decide anything.
                payload.insert(
                    "author_flags".to_owned(),
                    json!({
                        "is_chat_owner": author.is_chat_owner.unwrap_or(false),
                        "is_chat_moderator": author.is_chat_moderator.unwrap_or(false),
                        "is_chat_sponsor": author.is_chat_sponsor.unwrap_or(false),
                        "is_verified": author.is_verified.unwrap_or(false),
                    }),
                );
                event_kind = EventKind::ChatMessage;
            }
            "superChatEvent" | "superStickerEvent" => {
                let sticker = kind == "superStickerEvent";
                let paid = if sticker {
                    snippet.super_sticker
                } else {
                    snippet.super_chat
                };
                let Some(paid) = paid else {
                    return Skipped(YoutubeSkip::Malformed);
                };
                let Some(amount) = paid.amount_micros.as_ref().and_then(micros) else {
                    return Skipped(YoutubeSkip::Malformed);
                };
                let Some(currency) = paid.currency.filter(|c| {
                    (1..=8).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_alphanumeric())
                }) else {
                    return Skipped(YoutubeSkip::Malformed);
                };
                let comment = if sticker {
                    paid.sticker.and_then(|s| s.alt_text)
                } else {
                    paid.user_comment
                }
                .unwrap_or_default();
                if comment.chars().count() > self.limits.max_text_chars {
                    return Skipped(YoutubeSkip::Oversize);
                }
                payload.insert("amount".to_owned(), json!(amount as f64 / 1_000_000.0));
                payload.insert("currency".to_owned(), json!(currency));
                payload.insert("message".to_owned(), json!(comment));
                payload.insert(
                    "paid_kind".to_owned(),
                    json!(if sticker {
                        "super_sticker"
                    } else {
                        "super_chat"
                    }),
                );
                if let Some(tier) = paid.tier {
                    payload.insert("tier".to_owned(), json!(tier));
                }
                if let Some(display) = paid
                    .amount_display
                    .filter(|d| d.chars().count() <= MAX_SHORT_FIELD)
                {
                    payload.insert("amount_display".to_owned(), json!(display));
                }
                event_kind = EventKind::ChatDonation;
            }
            "newSponsorEvent"
            | "memberMilestoneChatEvent"
            | "membershipGiftingEvent"
            | "giftMembershipReceivedEvent" => return Skipped(YoutubeSkip::Membership),
            "chatEndedEvent" => return Skipped(YoutubeSkip::ChatEnded),
            _ => return Skipped(YoutubeSkip::NotChat),
        }

        // Author channel id is the only viewer identifier; display name and
        // avatar are never carried. The app pseudonymizes it (namespaced by
        // `source`) before retention.
        let Some(actor) = author
            .channel_id
            .filter(|c| !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_graphic()))
        else {
            return Skipped(YoutubeSkip::Malformed);
        };
        let Some(observed_at) = snippet.published_at else {
            return Skipped(YoutubeSkip::Malformed);
        };

        let payload_bytes = serde_json::to_vec(&payload).map_or(usize::MAX, |b| b.len());
        if payload_bytes > self.limits.max_payload_bytes {
            return Skipped(YoutubeSkip::Oversize);
        }
        let donation = event_kind == EventKind::ChatDonation;
        let event = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("youtube-{id}"),
            correlation_id: format!("corr-youtube-{id}"),
            sequence: self.sequence,
            observed_at,
            source: "youtube:live-chat".to_owned(),
            source_class: if donation {
                SourceClass::Donation
            } else {
                SourceClass::PublicChat
            },
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: event_kind,
            actor_id: Some(actor),
            priority_hint: None,
            authorization: None,
            payload,
        };
        if event.validate().is_err() {
            return Skipped(YoutubeSkip::Malformed);
        }
        let Ok(line) = serde_json::to_string(&event) else {
            return Skipped(YoutubeSkip::Malformed);
        };
        if line.len() > self.limits.max_record_bytes {
            return Skipped(YoutubeSkip::Oversize);
        }
        self.sequence = self.sequence.saturating_add(1);
        YoutubeMapOutcome::Record {
            line,
            paid: donation,
        }
    }
}

// ---------------------------------------------------------------------------
// Stats and queue
// ---------------------------------------------------------------------------

/// Everything the sidecar counts. Drops are reported, never hidden.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct YoutubeIngressStats {
    pub emitted: u64,
    pub emitted_paid: u64,
    pub dropped_queue_full: u64,
    pub dropped_oversize: u64,
    pub dropped_malformed: u64,
    pub duplicates: u64,
    pub skipped_backlog: u64,
    pub membership_unmapped: u64,
    pub not_chat: u64,
    pub polls: u64,
    pub transient_errors: u64,
    pub quota_backoffs: u64,
    pub malformed_responses: u64,
}

impl fmt::Display for YoutubeIngressStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "emitted={} emitted_paid={} dropped_queue_full={} dropped_oversize={} \
             dropped_malformed={} duplicates={} skipped_backlog={} membership_unmapped={} \
             not_chat={} polls={} transient_errors={} quota_backoffs={} malformed_responses={}",
            self.emitted,
            self.emitted_paid,
            self.dropped_queue_full,
            self.dropped_oversize,
            self.dropped_malformed,
            self.duplicates,
            self.skipped_backlog,
            self.membership_unmapped,
            self.not_chat,
            self.polls,
            self.transient_errors,
            self.quota_backoffs,
            self.malformed_responses
        )
    }
}

/// Hand a record to the bounded writer queue without blocking the poller; a
/// full queue drops the newest record and counts it.
pub fn enqueue_youtube_record(
    sender: &SyncSender<String>,
    line: String,
    stats: &mut YoutubeIngressStats,
) -> EnqueueOutcome {
    match sender.try_send(line) {
        Ok(()) => {
            stats.emitted = stats.emitted.saturating_add(1);
            EnqueueOutcome::Queued
        }
        Err(TrySendError::Full(_)) => {
            stats.dropped_queue_full = stats.dropped_queue_full.saturating_add(1);
            EnqueueOutcome::Queued
        }
        Err(TrySendError::Disconnected(_)) => EnqueueOutcome::WriterGone,
    }
}

// ---------------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------------

/// Fixed-capacity set of seen message ids; the oldest id is evicted first.
#[derive(Debug)]
struct SeenIds {
    capacity: usize,
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIds {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Returns false when the id was already present.
    fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        if self.order.len() >= self.capacity
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        self.set.insert(id.to_owned());
        self.order.push_back(id.to_owned());
        true
    }
}

#[derive(Debug, Clone)]
pub struct YoutubePollerConfig {
    pub api_base: String,
    pub live_chat_id: String,
    /// Operator floor; the effective wait is `max(pollingIntervalMillis, this)`.
    pub min_poll_ms: u64,
    /// Emit the messages already in chat on the first poll. Off by default so a
    /// mid-stream start does not answer old chat.
    pub emit_backlog: bool,
    pub request_timeout: Duration,
    pub limits: YoutubeIngressLimits,
    pub dedupe_capacity: usize,
}

impl YoutubePollerConfig {
    pub fn new(api_base: &str, live_chat_id: &str) -> Self {
        Self {
            api_base: api_base.to_owned(),
            live_chat_id: live_chat_id.to_owned(),
            min_poll_ms: YOUTUBE_DEFAULT_MIN_POLL_MS,
            emit_backlog: false,
            request_timeout: Duration::from_secs(15),
            limits: YoutubeIngressLimits::default(),
            dedupe_capacity: YOUTUBE_DEDUPE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollEnd {
    /// `liveChatEnded`, a `chatEndedEvent`, or `offlineAt`: stop cleanly.
    ChatEnded,
    /// Misconfiguration (bad key, chat not found, other 4xx): do not retry.
    Fatal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollStep {
    pub records: Vec<String>,
    /// How long the caller must wait before the next `poll_once`.
    pub wait: Duration,
    pub end: Option<PollEnd>,
}

pub struct YoutubeChatPoller<H: YoutubeHttp> {
    http: H,
    api_key: YoutubeApiKey,
    config: YoutubePollerConfig,
    mapper: YoutubeRecordMapper,
    seen: SeenIds,
    page_token: Option<String>,
    first_page_done: bool,
    failures: u32,
    pub stats: YoutubeIngressStats,
}

impl<H: YoutubeHttp> fmt::Debug for YoutubeChatPoller<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YoutubeChatPoller")
            .field("api_key", &self.api_key)
            .field("live_chat_id", &self.config.live_chat_id)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct ListResponse {
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "pollingIntervalMillis")]
    polling_interval_millis: Option<u64>,
    #[serde(rename = "offlineAt")]
    offline_at: Option<Value>,
    #[serde(default)]
    items: Vec<Value>,
}

impl<H: YoutubeHttp> YoutubeChatPoller<H> {
    pub fn new(http: H, api_key: YoutubeApiKey, config: YoutubePollerConfig) -> Self {
        Self {
            http,
            api_key,
            mapper: YoutubeRecordMapper::new(config.limits),
            seen: SeenIds::new(config.dedupe_capacity),
            config,
            page_token: None,
            first_page_done: false,
            failures: 0,
            stats: YoutubeIngressStats::default(),
        }
    }

    fn floor(&self) -> Duration {
        Duration::from_millis(self.config.min_poll_ms.min(MAX_POLL_WAIT_MS))
    }

    fn backoff(&mut self, policy: ReconnectPolicy) -> Duration {
        self.failures = self.failures.saturating_add(1);
        Duration::from_millis(policy.delay_ms(self.failures)).max(self.floor())
    }

    fn request_url(&self) -> Result<String, String> {
        let mut url = url::Url::parse(&self.config.api_base)
            .map_err(|_| "invalid API base URL".to_owned())?;
        url.set_path("/youtube/v3/liveChat/messages");
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("liveChatId", &self.config.live_chat_id)
                .append_pair("part", "snippet,authorDetails")
                // 200 is the API minimum.
                .append_pair("maxResults", "200");
            if let Some(token) = &self.page_token {
                query.append_pair("pageToken", token);
            }
        }
        Ok(url.into())
    }

    /// One `liveChatMessages.list` call (5 quota units) and its mapping.
    pub fn poll_once(&mut self) -> PollStep {
        let fail = |wait, end| PollStep {
            records: Vec::new(),
            wait,
            end,
        };
        let url = match self.request_url() {
            Ok(url) => url,
            Err(message) => return fail(self.floor(), Some(PollEnd::Fatal(message))),
        };
        self.stats.polls = self.stats.polls.saturating_add(1);
        let response = match self
            .http
            .get(&url, &self.api_key, self.config.request_timeout)
        {
            Ok(response) => response,
            Err(_) => {
                self.stats.transient_errors = self.stats.transient_errors.saturating_add(1);
                return fail(self.backoff(YOUTUBE_ERROR_BACKOFF), None);
            }
        };

        if response.status != 200 {
            let reasons = error_reasons(&response.body);
            let reason = short_reason(&reasons);
            let has = |name: &str| reasons.iter().any(|r| r == name);
            return match response.status {
                403 if has("liveChatEnded") => fail(self.floor(), Some(PollEnd::ChatEnded)),
                403 if has("quotaExceeded")
                    || has("dailyLimitExceeded")
                    || has("rateLimitExceeded")
                    || has("userRateLimitExceeded") =>
                {
                    self.stats.quota_backoffs = self.stats.quota_backoffs.saturating_add(1);
                    fail(self.backoff(YOUTUBE_QUOTA_BACKOFF), None)
                }
                429 => {
                    self.stats.quota_backoffs = self.stats.quota_backoffs.saturating_add(1);
                    fail(self.backoff(YOUTUBE_QUOTA_BACKOFF), None)
                }
                500..=599 => {
                    self.stats.transient_errors = self.stats.transient_errors.saturating_add(1);
                    fail(self.backoff(YOUTUBE_ERROR_BACKOFF), None)
                }
                status => fail(
                    self.floor(),
                    Some(PollEnd::Fatal(format!(
                        "liveChatMessages.list failed: HTTP {status} ({reason})"
                    ))),
                ),
            };
        }

        let Ok(page) = serde_json::from_slice::<ListResponse>(&response.body) else {
            self.stats.malformed_responses = self.stats.malformed_responses.saturating_add(1);
            return fail(self.backoff(YOUTUBE_ERROR_BACKOFF), None);
        };
        self.failures = 0;
        if let Some(token) = page
            .next_page_token
            .filter(|t| !t.is_empty() && t.len() <= 1024)
        {
            self.page_token = Some(token);
        }

        let skip_page = !self.first_page_done && !self.config.emit_backlog;
        self.first_page_done = true;
        let mut records = Vec::new();
        let mut ended = page.offline_at.is_some();
        for (index, raw) in page.items.iter().enumerate() {
            if index >= MAX_ITEMS_PER_PAGE {
                self.stats.dropped_oversize = self.stats.dropped_oversize.saturating_add(1);
                continue;
            }
            let id = raw.get("id").and_then(Value::as_str).unwrap_or_default();
            if !message_id_ok(id) {
                self.stats.dropped_malformed = self.stats.dropped_malformed.saturating_add(1);
                continue;
            }
            if !self.seen.insert(id) {
                self.stats.duplicates = self.stats.duplicates.saturating_add(1);
                continue;
            }
            let outcome = self.mapper.map_item(raw);
            if skip_page {
                // Still honour an end-of-chat marker inside the backlog.
                if outcome == YoutubeMapOutcome::Skipped(YoutubeSkip::ChatEnded) {
                    ended = true;
                } else {
                    self.stats.skipped_backlog = self.stats.skipped_backlog.saturating_add(1);
                }
                continue;
            }
            let counter = match outcome {
                YoutubeMapOutcome::Record { line, paid } => {
                    if paid {
                        self.stats.emitted_paid = self.stats.emitted_paid.saturating_add(1);
                    }
                    records.push(line);
                    continue;
                }
                YoutubeMapOutcome::Skipped(YoutubeSkip::Oversize) => {
                    &mut self.stats.dropped_oversize
                }
                YoutubeMapOutcome::Skipped(YoutubeSkip::Malformed) => {
                    &mut self.stats.dropped_malformed
                }
                YoutubeMapOutcome::Skipped(YoutubeSkip::Membership) => {
                    &mut self.stats.membership_unmapped
                }
                YoutubeMapOutcome::Skipped(YoutubeSkip::NotChat) => &mut self.stats.not_chat,
                YoutubeMapOutcome::Skipped(YoutubeSkip::ChatEnded) => {
                    ended = true;
                    continue;
                }
            };
            *counter = counter.saturating_add(1);
        }

        let api_ms = page
            .polling_interval_millis
            .unwrap_or(0)
            .min(MAX_POLL_WAIT_MS);
        let wait = Duration::from_millis(api_ms).max(self.floor());
        PollStep {
            records,
            wait,
            end: ended.then_some(PollEnd::ChatEnded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_ids_are_bounded_and_evict_oldest_first() {
        let mut seen = SeenIds::new(3);
        for id in ["a", "b", "c"] {
            assert!(seen.insert(id));
        }
        assert!(!seen.insert("a"));
        assert!(seen.insert("d")); // evicts "a"
        assert_eq!(seen.set.len(), 3);
        assert_eq!(seen.order.len(), 3);
        assert!(seen.insert("a")); // forgotten, so accepted again
    }

    #[test]
    fn chat_is_least_privileged_and_carries_no_display_name() {
        let item = json!({
            "id": "m1",
            "snippet": { "type": "textMessageEvent", "publishedAt": "2026-11-10T12:00:00+00:00",
                "textMessageDetails": { "messageText": "こんにちは" } },
            "authorDetails": { "channelId": "UCabc", "displayName": "Alice",
                "isChatOwner": true, "isChatModerator": true }
        });
        let mut mapper = YoutubeRecordMapper::new(YoutubeIngressLimits::default());
        let YoutubeMapOutcome::Record { line, paid } = mapper.map_item(&item) else {
            panic!("record");
        };
        assert!(!paid);
        assert!(!line.contains("Alice"));
        let event: EventEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(event.kind, EventKind::ChatMessage);
        assert_eq!(event.source_class, SourceClass::PublicChat);
        assert_eq!(event.plane, SecurityPlane::Content);
        assert_eq!(event.trust_level, TrustLevel::Untrusted);
        assert!(event.authorization.is_none());
        assert_eq!(event.actor_id.as_deref(), Some("UCabc"));
        assert_eq!(event.payload["text"], "こんにちは");
        assert_eq!(event.payload["author_flags"]["is_chat_moderator"], true);
    }

    #[test]
    fn rejected_items_do_not_consume_sequence_numbers() {
        let mut mapper = YoutubeRecordMapper::new(YoutubeIngressLimits::default());
        assert_eq!(
            mapper.map_item(&json!({ "id": "x" })),
            YoutubeMapOutcome::Skipped(YoutubeSkip::Malformed)
        );
        let ok = json!({
            "id": "m", "snippet": { "type": "textMessageEvent",
                "publishedAt": "2026-11-10T12:00:00+00:00",
                "textMessageDetails": { "messageText": "hi" } },
            "authorDetails": { "channelId": "UC1" }
        });
        let YoutubeMapOutcome::Record { line, .. } = mapper.map_item(&ok) else {
            panic!("record");
        };
        let event: EventEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(event.sequence, 0);
    }

    #[test]
    fn ids_are_validated() {
        assert!(validate_video_id("dQw4w9WgXcQ").is_ok());
        assert!(validate_video_id("short").is_err());
        assert!(validate_video_id("dQw4w9WgXc&").is_err());
        assert!(validate_live_chat_id("Cg0KC2FiYw==").is_ok());
        assert!(validate_live_chat_id("a b").is_err());
        assert!(validate_live_chat_id("x&key=1").is_err());
        assert!(YoutubeApiKey::new("bad key").is_err());
        assert!(YoutubeApiKey::new("").is_err());
    }
}
