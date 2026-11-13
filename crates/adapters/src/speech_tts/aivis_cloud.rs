//! Optional Aivis Cloud API backend for the local TTS bridge (issue #327).
//!
//! [`AivisCloudSynthesizer`] is a [`Synthesizer`]: plugged into the existing
//! [`super::server::TtsServer`], it turns the *existing* normalized TTS
//! contract into one `POST {origin}/v1/tts/synthesize` call (JSON in, raw WAV
//! out). The app still speaks one TTS wire contract; every Aivis detail (model
//! UUID, speaker/style, the request DTO) stays in this file.
//!
//! Invariants:
//! * This is the only TTS backend that leaves the machine, so it is an explicit
//!   operator opt-in. The origin must be `https://` (any host) or `http://` on
//!   loopback (for test fakes), checked by the same helper the Thinking/TTS
//!   adapters use. Redirects are never followed, so the bearer key cannot be
//!   forwarded to another host.
//! * The key is a [`SecretString`]: redacted in `Debug`, sent only as the
//!   `Authorization` header, never put in an error, a log line or the cache key.
//! * `use_ssml` is always `false`: reply text is data and must never be able to
//!   inject SSML. Output is always `wav` / mono so the strict WAV path is reused.
//! * The voice (model UUID, optional speaker UUID, optional style id) is
//!   configuration; the request's `voice_model`/`style` never reach the API.
//! * Bounded: text length (the API's own 3000-character limit, rejected before
//!   sending), response bytes, one deadline for the call. Errors are fixed
//!   strings plus a status code or an I/O kind: never the text, never the body.
//!
//! Determinism: the service gives no byte-identical guarantee. The cache key
//! contains the model, speaker and style so changing the voice never replays
//! stale audio, but a model update on the service side is not visible to it.

use super::voicevox::decode_wav_pcm16_mono;
use super::{GenerationSettings, Pcm, SynthError, Synthesizer};
use crate::SecretString;
use aivtuber_domain::{EngineErrorKind, VoiceTuning};
use serde_json::json;
use std::io::Read;
use std::time::Instant;

/// `backend_name` recorded in TTS provenance for this backend.
pub const BACKEND_NAME: &str = "aivis-cloud";
/// Default API origin.
pub const DEFAULT_ENDPOINT: &str = "https://api.aivis-project.com";
/// Route of the synthesis call under the origin.
pub const SYNTHESIZE_ROUTE: &str = "/v1/tts/synthesize";
/// The API's text limit; longer text is refused before any request.
pub const MAX_TEXT_CHARS: usize = 3000;
/// Default for [`AivisCloudConfig::max_response_bytes`].
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Highest style id the API accepts.
pub const MAX_STYLE_ID: u8 = 31;
/// Longest `ak_...` access key accepted as a model reference.
const MAX_MODEL_REF_CHARS: usize = 128;

/// Operator-facing notice printed at startup: where reply text goes and whose
/// terms apply.
pub const EGRESS_NOTICE: &str = "reply text is sent to Aivis Cloud (an internet service) for synthesis; the Aivis Cloud API terms apply and each AivisHub model has its own licence and credit requirements, which you own";

#[derive(Debug, Clone, PartialEq)]
pub struct AivisCloudConfig {
    /// Origin of the API: `https://host[:port]`, or loopback `http://` for a
    /// test fake.
    pub endpoint: String,
    pub api_key: SecretString,
    /// Model UUID or `ak_...` access key. Required: there is no default voice.
    pub model_uuid: String,
    pub speaker_uuid: Option<String>,
    pub style_id: Option<u8>,
    /// Speaking rate, pitch, volume and emotional intensity (#360); the
    /// defaults are the API's own.
    pub tuning: VoiceTuning,
    pub leading_silence_seconds: f32,
    pub trailing_silence_seconds: f32,
    pub max_response_bytes: usize,
    pub max_text_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AivisCloudConfigError {
    /// Not an `https://` (or loopback `http://`) origin, or it has credentials,
    /// a path, a query or a fragment.
    Endpoint(String),
    /// The model reference is neither a UUID nor an `ak_...` access key.
    ModelUuid,
    SpeakerUuid,
    StyleId,
    ApiKey,
    /// A bound is zero or out of range.
    Bound(&'static str),
}

impl std::fmt::Display for AivisCloudConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint(reason) => write!(f, "Aivis Cloud endpoint refused: {reason}"),
            Self::ModelUuid => {
                write!(f, "the model must be a UUID or an ak_... access key")
            }
            Self::SpeakerUuid => write!(f, "the speaker must be a UUID"),
            Self::StyleId => write!(f, "the style id must be 0..={MAX_STYLE_ID}"),
            Self::ApiKey => write!(f, "the API key is empty or contains control characters"),
            Self::Bound(name) => write!(f, "Aivis Cloud bound out of range: {name}"),
        }
    }
}

impl std::error::Error for AivisCloudConfigError {}

/// `8-4-4-4-12` hex, any case.
pub fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The API's `model_uuid` pattern: `^(ak_[A-Za-z0-9_-]+|<uuid>)$`.
pub fn is_model_reference(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix("ak_") {
        return !rest.is_empty()
            && value.len() <= MAX_MODEL_REF_CHARS
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    }
    is_uuid(value)
}

/// The shared origin rule for every call to the service (synthesis, key
/// check, model search): TLS for any remote host, plain http only on loopback,
/// no embedded credentials, an origin and nothing else.
pub fn validate_origin(endpoint: &str) -> Result<url::Url, AivisCloudConfigError> {
    let refuse = |reason: &str| AivisCloudConfigError::Endpoint(reason.to_owned());
    if !crate::http::is_secure_provider_endpoint(endpoint) {
        return Err(refuse(
            "must be https:// (http:// only for a loopback test server) without credentials",
        ));
    }
    let url = url::Url::parse(endpoint).map_err(|_| refuse("not a valid URL"))?;
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err(refuse(
            "give the origin only, e.g. https://api.aivis-project.com",
        ));
    }
    Ok(url)
}

impl AivisCloudConfig {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: SecretString,
        model_uuid: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key,
            model_uuid: model_uuid.into(),
            speaker_uuid: None,
            style_id: None,
            tuning: VoiceTuning::default(),
            leading_silence_seconds: 0.0,
            trailing_silence_seconds: 0.1,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_text_chars: MAX_TEXT_CHARS,
        }
    }

    /// The validated origin (`scheme://host:port/`), so a later `join` cannot
    /// be redirected to another path by the operator's value.
    pub fn validated_origin(&self) -> Result<url::Url, AivisCloudConfigError> {
        validate_origin(&self.endpoint)
    }

    pub fn validate(&self) -> Result<(), AivisCloudConfigError> {
        self.validated_origin()?;
        let key = self.api_key.expose();
        if key.trim().is_empty() || key.chars().any(char::is_control) {
            return Err(AivisCloudConfigError::ApiKey);
        }
        if !is_model_reference(&self.model_uuid) {
            return Err(AivisCloudConfigError::ModelUuid);
        }
        if self.speaker_uuid.as_deref().is_some_and(|s| !is_uuid(s)) {
            return Err(AivisCloudConfigError::SpeakerUuid);
        }
        if self.style_id.is_some_and(|id| id > MAX_STYLE_ID) {
            return Err(AivisCloudConfigError::StyleId);
        }
        if self.tuning.validate().is_err() {
            return Err(AivisCloudConfigError::Bound("tuning"));
        }
        for (name, seconds) in [
            ("leading_silence_seconds", self.leading_silence_seconds),
            ("trailing_silence_seconds", self.trailing_silence_seconds),
        ] {
            if !seconds.is_finite() || !(0.0..=5.0).contains(&seconds) {
                return Err(AivisCloudConfigError::Bound(name));
            }
        }
        if self.max_response_bytes < 1024 || self.max_response_bytes > 64 * 1024 * 1024 {
            return Err(AivisCloudConfigError::Bound("max_response_bytes"));
        }
        if !(1..=MAX_TEXT_CHARS).contains(&self.max_text_chars) {
            return Err(AivisCloudConfigError::Bound("max_text_chars"));
        }
        Ok(())
    }
}

/// How an Aivis HTTP status is classified.
///
/// `402` (credit exhausted / payment required) maps to `Authentication`, not
/// `Unavailable`: like a rejected key it is an account-level refusal that no
/// retry fixes and only the operator can resolve, whereas `Unavailable` means
/// "try again later". The bridge reports every engine failure to the app as one
/// 5xx, so this class drives the operator-visible log line.
pub fn classify_status(status: u16) -> EngineErrorKind {
    match status {
        401..=403 => EngineErrorKind::Authentication,
        400 | 404 | 422 => EngineErrorKind::InvalidRequest,
        429 => EngineErrorKind::RateLimited,
        503 | 529 => EngineErrorKind::Overloaded,
        _ if status >= 500 => EngineErrorKind::Unavailable,
        _ => EngineErrorKind::Backend,
    }
}

fn status_message(status: u16) -> String {
    let what = match status {
        401 | 403 => "authentication failed (check the API key)",
        402 => "credit exhausted or payment required (check the Aivis account)",
        400 | 404 | 422 => "request rejected (check the model, speaker and style)",
        429 => "rate limited",
        503 | 529 => "service overloaded",
        _ if status >= 500 => "service error",
        _ => "unexpected answer",
    };
    format!("aivis-cloud: HTTP {status}: {what}")
}

pub struct AivisCloudSynthesizer {
    url: url::Url,
    api_key: SecretString,
    model_uuid: String,
    speaker_uuid: Option<String>,
    style_id: Option<u8>,
    tuning: VoiceTuning,
    leading_silence_seconds: f32,
    trailing_silence_seconds: f32,
    max_response_bytes: usize,
    max_text_chars: usize,
    loopback: bool,
    agent: ureq::Agent,
    identity: String,
}

impl std::fmt::Debug for AivisCloudSynthesizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AivisCloudSynthesizer")
            .field("origin", &self.url.origin().ascii_serialization())
            .field("model_uuid", &self.model_uuid)
            .field("speaker_uuid", &self.speaker_uuid)
            .field("style_id", &self.style_id)
            .field("api_key", &self.api_key)
            .finish()
    }
}

impl AivisCloudSynthesizer {
    pub fn new(config: &AivisCloudConfig) -> Result<Self, AivisCloudConfigError> {
        config.validate()?;
        let origin = config.validated_origin()?;
        let url = origin
            .join(SYNTHESIZE_ROUTE)
            .map_err(|_| AivisCloudConfigError::Endpoint("cannot build the URL".to_owned()))?;
        let loopback = origin.scheme() == "http";
        Ok(Self {
            url,
            api_key: config.api_key.clone(),
            model_uuid: config.model_uuid.clone(),
            speaker_uuid: config.speaker_uuid.clone(),
            style_id: config.style_id,
            tuning: config.tuning,
            leading_silence_seconds: config.leading_silence_seconds,
            trailing_silence_seconds: config.trailing_silence_seconds,
            max_response_bytes: config.max_response_bytes,
            max_text_chars: config.max_text_chars,
            loopback,
            agent: ureq::Agent::new_with_defaults(),
            identity: identity(
                &config.model_uuid,
                config.speaker_uuid.as_deref(),
                config.style_id,
                &config.tuning,
            ),
        })
    }

    fn request_body(&self, text: &str) -> Vec<u8> {
        let mut body = json!({
            "model_uuid": self.model_uuid,
            "text": text,
            // Reply text is data: never interpreted as SSML.
            "use_ssml": false,
            "output_format": "wav",
            "output_audio_channels": "mono",
            "leading_silence_seconds": self.leading_silence_seconds,
            "trailing_silence_seconds": self.trailing_silence_seconds,
            "speaking_rate": self.tuning.speaking_rate,
            "pitch": self.tuning.pitch,
            "volume": self.tuning.volume,
            "emotional_intensity": self.tuning.emotional_intensity,
        });
        if let Some(speaker) = &self.speaker_uuid {
            body["speaker_uuid"] = json!(speaker);
        }
        if let Some(style) = self.style_id {
            body["style_id"] = json!(style);
        }
        serde_json::to_vec(&body).expect("request JSON")
    }
}

/// The cache identity: provider, model, speaker, style and tuning. Never the
/// key and never the endpoint, so the same voice is the same cache entry and a
/// different voice or tuning is never replayed from another's audio.
fn identity(
    model_uuid: &str,
    speaker_uuid: Option<&str>,
    style_id: Option<u8>,
    tuning: &VoiceTuning,
) -> String {
    format!(
        "aivis-cloud-api;model={model_uuid};speaker={};style={};{}",
        speaker_uuid.unwrap_or("-"),
        style_id.map_or_else(|| "-".to_owned(), |id| id.to_string()),
        tuning.identity(),
    )
}

/// Provenance `model_alias`: the model reference (an `ak_` access key is a
/// credential, so it is never recorded) and the style.
pub fn model_alias(model_uuid: &str, style_id: Option<u8>) -> String {
    let model = if model_uuid.starts_with("ak_") {
        "ak_...".to_owned()
    } else {
        model_uuid.to_owned()
    };
    match style_id {
        Some(style) => format!("{model}/style-{style}"),
        None => model,
    }
}

impl Synthesizer for AivisCloudSynthesizer {
    fn synthesize(
        &self,
        text: &str,
        _settings: &GenerationSettings,
        deadline: Instant,
    ) -> Result<Pcm, SynthError> {
        if text.chars().count() > self.max_text_chars {
            return Err(SynthError::Engine(
                "text exceeds the Aivis Cloud limit".to_owned(),
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SynthError::DeadlineExceeded);
        }
        let body = self.request_body(text);
        let configured = self
            .agent
            .post(self.url.as_str())
            .config()
            .timeout_global(Some(remaining))
            .http_status_as_error(false)
            // A redirect would carry the bearer key to another host.
            .max_redirects(0);
        // A loopback fake is never reached through a proxy from the environment.
        let configured = if self.loopback {
            configured.proxy(None)
        } else {
            configured
        };
        let mut response = configured
            .build()
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.api_key.expose()))
            .send(&body[..])
            .map_err(|error| map_transport_error(&error))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(SynthError::Engine(status_message(status)));
        }
        let cap = self.max_response_bytes;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .with_config()
            .limit(cap as u64 + 1)
            .reader()
            .take(cap as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut {
                    SynthError::DeadlineExceeded
                } else {
                    SynthError::Engine(format!(
                        "aivis-cloud: cannot read the answer ({:?})",
                        error.kind()
                    ))
                }
            })?;
        if bytes.len() > cap {
            return Err(SynthError::Engine(format!(
                "aivis-cloud: answer exceeds the {cap} byte limit"
            )));
        }
        resolve_streamed_sizes(&mut bytes);
        decode_wav_pcm16_mono(&bytes)
            .map_err(|reason| SynthError::Engine(format!("aivis-cloud: {reason}")))
    }

    /// Not a digest: a stable identity string (provider, model, speaker, style)
    /// that the server mixes into the content address and records as the model
    /// version.
    fn bundle_sha256(&self) -> &str {
        &self.identity
    }
}

/// The service streams its WAV from an encoder that cannot seek back, so the
/// RIFF and `data` sizes are the `0xFFFFFFFF` "unknown length" sentinel
/// (observed against the live API). Only that exact sentinel on the `data`
/// chunk is resolved, to the bytes actually received; every other length stays
/// subject to the strict parser, so a truncated or lying file is still refused.
fn resolve_streamed_sizes(bytes: &mut [u8]) {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return;
    }
    let mut at = 12_usize;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]);
        if &bytes[at..at + 4] == b"data" {
            if size == u32::MAX {
                // Whole samples only; a dangling odd byte is dropped.
                let available = (bytes.len() - (at + 8)) & !1;
                bytes[at + 4..at + 8].copy_from_slice(&(available as u32).to_le_bytes());
            }
            return;
        }
        let Some(next) = (at + 8).checked_add(size as usize + (size as usize & 1)) else {
            return;
        };
        at = next;
    }
}

fn map_transport_error(error: &ureq::Error) -> SynthError {
    match error {
        ureq::Error::Timeout(_) => SynthError::DeadlineExceeded,
        // Never `error.to_string()`: keep the request details out of the log.
        ureq::Error::Io(io) => {
            SynthError::Engine(format!("aivis-cloud: unreachable ({:?})", io.kind()))
        }
        _ => SynthError::Engine("aivis-cloud: transport error".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::VoiceConfig;
    use super::super::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    use super::*;
    use serde_json::Value;
    use std::time::Duration;

    const TEST_DEADLINE: Duration = Duration::from_secs(10);
    const MODEL: &str = "a59cb814-0083-4369-8542-f51a29e72af7";
    const KEY: &str = "sk-test-SECRETKEY-123";

    fn settings(text: &str) -> GenerationSettings {
        GenerationSettings::for_text(text, VoiceConfig::default())
    }

    fn deadline() -> Instant {
        Instant::now() + TEST_DEADLINE
    }

    fn config(endpoint: &str) -> AivisCloudConfig {
        AivisCloudConfig::new(endpoint, SecretString::new(KEY), MODEL)
    }

    fn synth(endpoint: &str) -> AivisCloudSynthesizer {
        AivisCloudSynthesizer::new(&config(endpoint)).expect("synthesizer")
    }

    #[test]
    fn only_https_or_loopback_http_origins_are_accepted() {
        for ok in [
            "https://api.aivis-project.com",
            "https://api.aivis-project.com/",
            "https://example.test:8443",
            "http://127.0.0.1:9",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            assert!(config(ok).validate().is_ok(), "{ok}");
        }
        for bad in [
            "http://api.aivis-project.com",
            "http://192.168.1.4:9",
            "ftp://api.aivis-project.com",
            "https://user:pw@api.aivis-project.com",
            "https://api.aivis-project.com/v1/tts/synthesize",
            "https://api.aivis-project.com/?a=1",
            "https://api.aivis-project.com/#x",
            "api.aivis-project.com",
            "",
        ] {
            assert!(
                matches!(
                    config(bad).validate(),
                    Err(AivisCloudConfigError::Endpoint(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn model_speaker_and_style_are_validated() {
        for ok in [MODEL, "A59CB814-0083-4369-8542-F51A29E72AF7", "ak_Abc-12_x"] {
            assert!(is_model_reference(ok), "{ok}");
        }
        for bad in [
            "",
            "ak_",
            "ak_a b",
            "ak_é",
            "a59cb814",
            "not-a-uuid",
            "../x",
        ] {
            assert!(!is_model_reference(bad), "{bad}");
        }
        assert!(!is_model_reference(&format!("ak_{}", "a".repeat(200))));
        let mut c = config("https://api.aivis-project.com");
        c.model_uuid = "bad".to_owned();
        assert_eq!(c.validate(), Err(AivisCloudConfigError::ModelUuid));
        let mut c = config("https://api.aivis-project.com");
        c.speaker_uuid = Some("bad".to_owned());
        assert_eq!(c.validate(), Err(AivisCloudConfigError::SpeakerUuid));
        c.speaker_uuid = Some(MODEL.to_owned());
        assert!(c.validate().is_ok());
        c.style_id = Some(32);
        assert_eq!(c.validate(), Err(AivisCloudConfigError::StyleId));
        c.style_id = Some(31);
        assert!(c.validate().is_ok());
        let mut c = AivisCloudConfig::new(
            "https://api.aivis-project.com",
            SecretString::new("  "),
            MODEL,
        );
        assert_eq!(c.validate(), Err(AivisCloudConfigError::ApiKey));
        c.api_key = SecretString::new(KEY);
        c.trailing_silence_seconds = f32::NAN;
        assert!(matches!(c.validate(), Err(AivisCloudConfigError::Bound(_))));
    }

    #[test]
    fn only_the_unknown_length_sentinel_is_resolved_and_a_lie_is_still_refused() {
        let samples = [0.1_f32, -0.2, 0.3, 0.4];
        let good = super::super::wav::encode_pcm16(&samples, 44_100).unwrap();
        // Streamed header with a LIST chunk before data, like the live API.
        let mut streamed = good[..36].to_vec();
        streamed.extend_from_slice(b"LIST");
        streamed.extend_from_slice(&3_u32.to_le_bytes());
        streamed.extend_from_slice(&[1, 2, 3, 0]);
        streamed.extend_from_slice(b"data");
        streamed.extend_from_slice(&u32::MAX.to_le_bytes());
        streamed.extend_from_slice(&good[44..]);
        streamed.push(7); // a dangling odd byte is dropped
        resolve_streamed_sizes(&mut streamed);
        let pcm = decode_wav_pcm16_mono(&streamed).expect("resolved");
        assert_eq!(pcm.samples.len(), 4);
        // A data length that is merely wrong (not the sentinel) is refused.
        let mut lying = good.clone();
        lying[40..44].copy_from_slice(&1_000_u32.to_le_bytes());
        resolve_streamed_sizes(&mut lying);
        assert!(decode_wav_pcm16_mono(&lying).is_err());
        // Not a WAV: left alone.
        let mut junk = b"{\"a\":1}".to_vec();
        resolve_streamed_sizes(&mut junk);
        assert_eq!(junk, b"{\"a\":1}");
    }

    #[test]
    fn debug_never_shows_the_key() {
        let c = config("https://api.aivis-project.com");
        let s = AivisCloudSynthesizer::new(&c).unwrap();
        for text in [format!("{c:?}"), format!("{s:?}")] {
            assert!(!text.contains("SECRETKEY"), "{text}");
            assert!(text.contains("REDACTED"), "{text}");
        }
    }

    #[test]
    fn the_request_is_a_bearer_json_post_with_ssml_off_and_wav_mono_output() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = config(&api.endpoint());
        c.speaker_uuid = Some("11111111-2222-3333-4444-555555555555".to_owned());
        c.style_id = Some(2);
        let synth = AivisCloudSynthesizer::new(&c).unwrap();
        let text = "こんにちは <break time=\"5s\"/> & \"quote\"";
        let pcm = synth
            .synthesize(text, &settings("x"), deadline())
            .expect("audio");
        assert_eq!(pcm.sample_rate, 44_100);
        assert!(!pcm.samples.is_empty());

        let requests = api.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.route(), "/v1/tts/synthesize");
        assert_eq!(
            request.header("authorization").as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert_eq!(
            request.header("content-type").as_deref(),
            Some("application/json")
        );
        let body: Value = serde_json::from_slice(&request.body).expect("JSON body");
        assert_eq!(body["model_uuid"], MODEL);
        assert_eq!(body["text"], text, "text is sent verbatim as data");
        assert_eq!(body["use_ssml"], json!(false));
        assert_eq!(body["output_format"], "wav");
        assert_eq!(body["output_audio_channels"], "mono");
        assert_eq!(body["speaker_uuid"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(body["style_id"], 2);
        assert_eq!(body["leading_silence_seconds"], 0.0);
        assert!(body["trailing_silence_seconds"].as_f64().unwrap() > 0.09);
    }

    #[test]
    fn optional_voice_fields_are_omitted_when_unset() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        synth(&api.endpoint())
            .synthesize("あ", &settings("x"), deadline())
            .unwrap();
        let body: Value = serde_json::from_slice(&api.requests()[0].body).unwrap();
        assert!(body.get("speaker_uuid").is_none());
        assert!(body.get("style_id").is_none());
        // The tuning always goes out, at the API's own defaults when untouched.
        assert_eq!(body["speaking_rate"], 1.0);
        assert_eq!(body["pitch"], 0.0);
        assert_eq!(body["volume"], 1.0);
        assert_eq!(body["emotional_intensity"], 1.0);
    }

    #[test]
    fn tuning_reaches_the_request_and_out_of_range_tuning_is_refused() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut tuned = config(&api.endpoint());
        tuned.tuning = VoiceTuning {
            speaking_rate: 1.5,
            pitch: -0.25,
            volume: 0.5,
            emotional_intensity: 2.0,
        };
        AivisCloudSynthesizer::new(&tuned)
            .unwrap()
            .synthesize("あ", &settings("x"), deadline())
            .unwrap();
        let body: Value = serde_json::from_slice(&api.requests()[0].body).unwrap();
        assert_eq!(body["speaking_rate"], 1.5);
        assert_eq!(body["pitch"], -0.25);
        assert_eq!(body["volume"], 0.5);
        assert_eq!(body["emotional_intensity"], 2.0);
        tuned.tuning.pitch = 1.5;
        assert!(AivisCloudSynthesizer::new(&tuned).is_err());
    }

    #[test]
    fn the_cache_identity_names_model_speaker_and_style_but_not_key_or_origin() {
        let base = synth("http://127.0.0.1:9");
        let mut other_model = config("http://127.0.0.1:9");
        other_model.model_uuid = "ak_other".to_owned();
        let mut other_speaker = config("http://127.0.0.1:9");
        other_speaker.speaker_uuid = Some("11111111-2222-3333-4444-555555555555".to_owned());
        let mut other_style = config("http://127.0.0.1:9");
        other_style.style_id = Some(1);
        // Every tuning value is part of the identity (#360): audio made with
        // one voice or tuning is never replayed for another.
        let tuned: Vec<AivisCloudConfig> = [
            VoiceTuning {
                speaking_rate: 1.25,
                ..VoiceTuning::default()
            },
            VoiceTuning {
                pitch: 0.5,
                ..VoiceTuning::default()
            },
            VoiceTuning {
                volume: 1.5,
                ..VoiceTuning::default()
            },
            VoiceTuning {
                emotional_intensity: 0.5,
                ..VoiceTuning::default()
            },
        ]
        .into_iter()
        .map(|tuning| AivisCloudConfig {
            tuning,
            ..config("http://127.0.0.1:9")
        })
        .collect();
        for other in [other_model, other_speaker, other_style]
            .into_iter()
            .chain(tuned)
        {
            let other = AivisCloudSynthesizer::new(&other).unwrap();
            assert_ne!(base.bundle_sha256(), other.bundle_sha256());
        }
        let mut same = config("https://api.aivis-project.com");
        same.api_key = SecretString::new("another-key");
        let same = AivisCloudSynthesizer::new(&same).unwrap();
        assert_eq!(base.bundle_sha256(), same.bundle_sha256());
        assert!(!base.bundle_sha256().contains("SECRETKEY"));
    }

    #[test]
    fn model_alias_keeps_an_access_key_secret() {
        assert_eq!(model_alias(MODEL, None), MODEL);
        assert_eq!(model_alias(MODEL, Some(3)), format!("{MODEL}/style-3"));
        assert_eq!(model_alias("ak_secretvalue", None), "ak_...");
    }

    #[test]
    fn statuses_are_classified() {
        use EngineErrorKind::*;
        for (status, kind) in [
            (401, Authentication),
            (403, Authentication),
            (402, Authentication),
            (404, InvalidRequest),
            (422, InvalidRequest),
            (429, RateLimited),
            (503, Overloaded),
            (529, Overloaded),
            (500, Unavailable),
            (502, Unavailable),
            (304, Backend),
        ] {
            assert_eq!(classify_status(status), kind, "{status}");
        }
    }

    #[test]
    fn every_failure_status_is_a_typed_engine_error_without_text_key_or_body() {
        let secret = "ひみつの本文SECRET";
        for status in [401_u16, 402, 403, 404, 422, 429, 500, 503, 529] {
            let api = FakeAivisCloud::start(FakeAivisBehaviour {
                status,
                body: Some(format!("{{\"detail\":\"echo {secret} {KEY}\"}}").into_bytes()),
                ..FakeAivisBehaviour::default()
            });
            match synth(&api.endpoint()).synthesize(secret, &settings(secret), deadline()) {
                Err(SynthError::Engine(message)) => {
                    assert!(message.contains(&status.to_string()), "{message}");
                    assert!(!message.contains("SECRET"), "{message}");
                    assert!(!message.contains("127.0.0.1"), "{message}");
                }
                other => panic!("{status}: expected an engine error, got {other:?}"),
            }
        }
        for (status, expected) in [(402_u16, "credit"), (429, "rate limited")] {
            let api = FakeAivisCloud::start(FakeAivisBehaviour {
                status,
                ..FakeAivisBehaviour::default()
            });
            let Err(SynthError::Engine(message)) =
                synth(&api.endpoint()).synthesize("あ", &settings("x"), deadline())
            else {
                panic!("{status}");
            };
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn invalid_and_oversize_audio_are_refused() {
        for body in [
            b"RIFFnope".to_vec(),
            b"{\"not\":\"audio\"}".to_vec(),
            Vec::new(),
        ] {
            let api = FakeAivisCloud::start(FakeAivisBehaviour {
                body: Some(body),
                ..FakeAivisBehaviour::default()
            });
            assert!(matches!(
                synth(&api.endpoint()).synthesize("あ", &settings("x"), deadline()),
                Err(SynthError::Engine(_))
            ));
        }
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = config(&api.endpoint());
        c.max_response_bytes = 1024;
        match AivisCloudSynthesizer::new(&c)
            .unwrap()
            .synthesize("あ", &settings("x"), deadline())
        {
            Err(SynthError::Engine(message)) => assert!(message.contains("limit"), "{message}"),
            other => panic!("expected the byte limit, got {other:?}"),
        }
    }

    #[test]
    fn the_text_bound_is_3000_characters_checked_before_any_request() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let synth = synth(&api.endpoint());
        // The fake's 40 ms per character would exceed the response cap at 3000
        // characters, so the boundary is probed with a count-only check.
        let over = "あ".repeat(MAX_TEXT_CHARS + 1);
        assert!(matches!(
            synth.synthesize(&over, &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert!(api.requests().is_empty(), "refused before any call");
        let at_limit = "あ".repeat(MAX_TEXT_CHARS);
        let _ = synth.synthesize(&at_limit, &settings("x"), deadline());
        assert_eq!(api.requests().len(), 1, "3000 characters are sent");
    }

    #[test]
    fn a_slow_service_hits_the_deadline_and_a_spent_deadline_makes_no_call() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            delay: Duration::from_millis(1_500),
            ..FakeAivisBehaviour::default()
        });
        let synth = synth(&api.endpoint());
        let started = Instant::now();
        let slow = synth.synthesize(
            "遅い",
            &settings("x"),
            Instant::now() + Duration::from_millis(200),
        );
        assert_eq!(slow, Err(SynthError::DeadlineExceeded));
        assert!(started.elapsed() < Duration::from_millis(1_400));
        let before = api.requests().len();
        assert_eq!(
            synth.synthesize("今", &settings("x"), Instant::now()),
            Err(SynthError::DeadlineExceeded)
        );
        assert_eq!(api.requests().len(), before);
    }

    #[test]
    fn an_unreachable_service_is_an_engine_error_without_url_or_text() {
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            endpoint
        };
        match synth(&dead).synthesize("SECRETtext", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => {
                assert!(message.contains("unreachable"), "{message}");
                assert!(!message.contains("SECRET") && !message.contains("127.0.0.1"));
            }
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_redirect_is_not_followed_so_the_key_cannot_leave() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            status: 307,
            ..FakeAivisBehaviour::default()
        });
        assert!(matches!(
            synth(&api.endpoint()).synthesize("あ", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert_eq!(api.requests().len(), 1);
    }

    /// Opt-in live smoke (issue #327): one short Japanese sentence through the
    /// real Aivis Cloud API. Costs credits, so it is ignored by default:
    /// `AIVIS_API=... AIVTUBER_AIVIS_MODEL_UUID=... cargo test --locked -p
    /// aivtuber-adapters --lib live_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore = "calls the real Aivis Cloud API with AIVIS_API and costs credits"]
    fn live_smoke_synthesizes_one_short_sentence() {
        let key = std::env::var("AIVIS_API").expect("AIVIS_API");
        let model = std::env::var("AIVTUBER_AIVIS_MODEL_UUID").expect("AIVTUBER_AIVIS_MODEL_UUID");
        let synth = AivisCloudSynthesizer::new(&AivisCloudConfig::new(
            DEFAULT_ENDPOINT,
            SecretString::new(key),
            model,
        ))
        .expect("synthesizer");
        let started = Instant::now();
        let pcm = synth
            .synthesize(
                "こんにちは、配信へようこそ。",
                &settings("x"),
                Instant::now() + Duration::from_secs(30),
            )
            .expect("synthesis");
        let latency = started.elapsed();
        assert!(!pcm.samples.is_empty());
        let duration_ms = pcm.samples.len() as u64 * 1000 / u64::from(pcm.sample_rate);
        assert!(duration_ms > 0);
        println!(
            "live smoke: latency_ms={} audio_ms={duration_ms} sample_rate={}",
            latency.as_millis(),
            pcm.sample_rate
        );
    }
}
