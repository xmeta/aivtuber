//! Optional VOICEVOX ENGINE backend for the local TTS bridge (issue #295).
//!
//! [`VoicevoxSynthesizer`] is a [`Synthesizer`]: plugged into the existing
//! [`super::server::TtsServer`], it turns the *existing* normalized TTS
//! contract into the VOICEVOX ENGINE's two-step HTTP API
//! (`POST /audio_query?text=..&speaker=..`, then `POST /synthesis?speaker=..`
//! with the returned query as the JSON body, answered with a WAV). The app
//! therefore still speaks exactly one TTS wire contract; every VOICEVOX
//! detail (query JSON, speaker ids, WAV from the engine) stays in this file.
//!
//! Invariants:
//! * The ENGINE is a separate process we neither link nor bundle (LGPL-3.0 or
//!   a separate licence). Only a loopback `http://` origin is accepted, and the
//!   host check is the same one the `local` profile applies to Thinking.
//! * The speaker is configuration, never a default: [`VoicevoxConfig::new`]
//!   needs it, and the request's `voice_model`/`style` never reach the engine.
//! * Bounded: both calls share the request's synthesis deadline, the engine's
//!   JSON query and the WAV are read under byte caps, the text length is
//!   capped, and no redirect is followed.
//! * Errors never carry the spoken text or the request URL (the text is in the
//!   query string); they are fixed strings plus a status code or an I/O kind.
//! * The engine's output is data: the WAV is parsed strictly (16-bit PCM mono)
//!   and re-encoded by the server, never passed through.
//!
//! Determinism: VOICEVOX has no seed. EXP-0003 measured identical output for
//! identical text in 50 of 50 replies, in-process and across processes, for one
//! engine build, one character and the default parameters. That is an observed
//! property of that build, not a guarantee of the engine: an engine, voice
//! library or parameter change can change the audio, and the cache key here
//! contains the speaker but not the engine version.

use super::{GenerationSettings, Pcm, SynthError, Synthesizer};
use crate::{SystemHostResolver, verify_loopback_endpoint};
use serde_json::{Value, json};
use std::io::Read;
use std::time::Instant;

/// `backend_name` recorded in TTS provenance for this backend.
pub const BACKEND_NAME: &str = "voicevox-engine";

/// Operator-facing notice printed at startup (S2a decision Q4): the character
/// is chosen by the operator, and each one has its own terms and credit line.
pub const TERMS_NOTICE: &str = "VOICEVOX voices are not freely licensed: the software terms and the terms of the character you chose apply, including a required credit line such as \"VOICEVOX:<character name>\" (https://voicevox.hiroshiba.jp/term/ ; each character's own terms page)";

/// Default for [`VoicevoxConfig::max_response_bytes`]: 8 MiB is about 170 s of
/// 24 kHz 16-bit mono, above the 120 s the WAV encoder accepts.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Default for [`VoicevoxConfig::max_query_bytes`].
pub const DEFAULT_MAX_QUERY_BYTES: usize = 2 * 1024 * 1024;
/// Default for [`VoicevoxConfig::max_text_chars`], matching the server's.
pub const DEFAULT_MAX_TEXT_CHARS: usize = 500;
/// Longest credit line accepted.
pub const MAX_CREDIT_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicevoxConfig {
    /// Origin of the engine, e.g. `http://127.0.0.1:50021`. Loopback only.
    pub endpoint: String,
    /// The engine's style id (`/speakers`), chosen by the operator.
    pub speaker_id: u32,
    pub max_response_bytes: usize,
    pub max_query_bytes: usize,
    pub max_text_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoicevoxConfigError {
    /// The endpoint is not a loopback `http://` origin (or has credentials,
    /// a path, a query or a fragment).
    Endpoint(String),
    /// A bound is zero or out of range.
    Bound(&'static str),
}

impl std::fmt::Display for VoicevoxConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint(reason) => write!(f, "VOICEVOX endpoint refused: {reason}"),
            Self::Bound(name) => write!(f, "VOICEVOX bound out of range: {name}"),
        }
    }
}

impl std::error::Error for VoicevoxConfigError {}

impl VoicevoxConfig {
    pub fn new(endpoint: impl Into<String>, speaker_id: u32) -> Self {
        Self {
            endpoint: endpoint.into(),
            speaker_id,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_query_bytes: DEFAULT_MAX_QUERY_BYTES,
            max_text_chars: DEFAULT_MAX_TEXT_CHARS,
        }
    }

    /// The validated engine origin (`scheme://host:port/`), so a later `join`
    /// cannot be redirected to another path by the operator's value.
    pub fn validated_origin(&self) -> Result<url::Url, VoicevoxConfigError> {
        let refuse = |reason: &str| VoicevoxConfigError::Endpoint(reason.to_owned());
        let url = url::Url::parse(&self.endpoint).map_err(|_| refuse("not a valid URL"))?;
        if url.scheme() != "http" {
            return Err(refuse("scheme must be http (the engine is on loopback)"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refuse("must not embed credentials"));
        }
        if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
            return Err(refuse("give the origin only, e.g. http://127.0.0.1:50021"));
        }
        // Same rule as the local profile's Thinking and TTS endpoints: an IP
        // loopback literal, or `localhost` resolving only to loopback.
        verify_loopback_endpoint(&self.endpoint, &SystemHostResolver)
            .map_err(|_| refuse("host is not loopback (use 127.0.0.1, [::1] or localhost)"))?;
        Ok(url)
    }

    pub fn validate(&self) -> Result<(), VoicevoxConfigError> {
        self.validated_origin()?;
        if self.max_response_bytes < 1024 || self.max_response_bytes > 64 * 1024 * 1024 {
            return Err(VoicevoxConfigError::Bound("max_response_bytes"));
        }
        if self.max_query_bytes < 1024 || self.max_query_bytes > 16 * 1024 * 1024 {
            return Err(VoicevoxConfigError::Bound("max_query_bytes"));
        }
        if !(1..=2_000).contains(&self.max_text_chars) {
            return Err(VoicevoxConfigError::Bound("max_text_chars"));
        }
        Ok(())
    }
}

/// A credit line the operator must show: non-blank, bounded, no control
/// characters (it is logged and may be copied to an overlay).
pub fn validate_credit_line(credit: &str) -> Result<String, String> {
    let credit = credit.trim();
    if credit.is_empty() {
        return Err("the credit line is empty".to_owned());
    }
    if credit.chars().count() > MAX_CREDIT_CHARS {
        return Err(format!(
            "the credit line exceeds {MAX_CREDIT_CHARS} characters"
        ));
    }
    if credit.chars().any(char::is_control) {
        return Err("the credit line contains control characters".to_owned());
    }
    Ok(credit.to_owned())
}

pub struct VoicevoxSynthesizer {
    origin: url::Url,
    speaker_id: u32,
    max_response_bytes: usize,
    max_query_bytes: usize,
    max_text_chars: usize,
    agent: ureq::Agent,
    identity: String,
}

impl std::fmt::Debug for VoicevoxSynthesizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicevoxSynthesizer")
            .field("origin", &self.origin.as_str())
            .field("speaker_id", &self.speaker_id)
            .finish()
    }
}

impl VoicevoxSynthesizer {
    pub fn new(config: &VoicevoxConfig) -> Result<Self, VoicevoxConfigError> {
        config.validate()?;
        let origin = config.validated_origin()?;
        Ok(Self {
            origin,
            speaker_id: config.speaker_id,
            max_response_bytes: config.max_response_bytes,
            max_query_bytes: config.max_query_bytes,
            max_text_chars: config.max_text_chars,
            agent: ureq::Agent::new_with_defaults(),
            identity: format!("voicevox-engine-http;speaker={}", config.speaker_id),
        })
    }

    pub fn speaker_id(&self) -> u32 {
        self.speaker_id
    }

    fn url(&self, route: &str, pairs: &[(&str, &str)]) -> Result<url::Url, SynthError> {
        let mut url = self
            .origin
            .join(route)
            .map_err(|_| SynthError::Engine("cannot build the engine URL".to_owned()))?;
        url.query_pairs_mut().extend_pairs(pairs.iter().copied());
        Ok(url)
    }

    /// One bounded POST. `Ok` only for a 2xx answer whose body fits `cap`.
    fn post(
        &self,
        what: &'static str,
        url: &url::Url,
        body: Option<&[u8]>,
        cap: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, SynthError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SynthError::DeadlineExceeded);
        }
        let config = self
            .agent
            .post(url.as_str())
            .config()
            .timeout_global(Some(remaining))
            .http_status_as_error(false)
            .max_redirects(0)
            // The engine is on this machine: never through a proxy from the
            // environment.
            .proxy(None);
        let mut request = config.build();
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let mut response = request
            .send(body.unwrap_or_default())
            .map_err(|error| map_transport_error(what, &error))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(SynthError::Engine(format!(
                "{what}: engine answered HTTP {status}"
            )));
        }
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
                        "{what}: cannot read the answer ({:?})",
                        error.kind()
                    ))
                }
            })?;
        if bytes.len() > cap {
            return Err(SynthError::Engine(format!(
                "{what}: answer exceeds the {cap} byte limit"
            )));
        }
        Ok(bytes)
    }
}

fn map_transport_error(what: &'static str, error: &ureq::Error) -> SynthError {
    match error {
        ureq::Error::Timeout(_) => SynthError::DeadlineExceeded,
        // Never `error.to_string()`: it can contain the request URL, and the
        // spoken text is in its query string.
        ureq::Error::Io(io) => {
            SynthError::Engine(format!("{what}: engine unreachable ({:?})", io.kind()))
        }
        _ => SynthError::Engine(format!("{what}: transport error")),
    }
}

impl Synthesizer for VoicevoxSynthesizer {
    fn synthesize(
        &self,
        text: &str,
        _settings: &GenerationSettings,
        deadline: Instant,
    ) -> Result<Pcm, SynthError> {
        if text.chars().count() > self.max_text_chars {
            return Err(SynthError::Engine(
                "text exceeds the bridge limit".to_owned(),
            ));
        }
        let speaker = self.speaker_id.to_string();

        let query_url = self.url("audio_query", &[("text", text), ("speaker", &speaker)])?;
        let query_bytes = self.post(
            "audio_query",
            &query_url,
            None,
            self.max_query_bytes,
            deadline,
        )?;
        let mut query: Value = serde_json::from_slice(&query_bytes)
            .map_err(|_| SynthError::Engine("audio_query: the answer is not JSON".to_owned()))?;
        let Some(object) = query.as_object_mut() else {
            return Err(SynthError::Engine(
                "audio_query: the answer is not a JSON object".to_owned(),
            ));
        };
        // Mono is what the WAV path accepts; the engine's default is mono, and
        // this makes it explicit rather than assumed.
        object.insert("outputStereo".to_owned(), json!(false));
        let body = serde_json::to_vec(&query)
            .map_err(|_| SynthError::Engine("audio_query: cannot re-encode".to_owned()))?;

        let synthesis_url = self.url("synthesis", &[("speaker", &speaker)])?;
        let wav = self.post(
            "synthesis",
            &synthesis_url,
            Some(&body),
            self.max_response_bytes,
            deadline,
        )?;
        decode_wav_pcm16_mono(&wav)
            .map_err(|reason| SynthError::Engine(format!("synthesis: {reason}")))
    }

    /// Not a digest: a stable identity string (engine kind and speaker) that
    /// the server mixes into the content address and records as the model
    /// version. The engine version is deliberately not probed; see the module
    /// docs on determinism.
    fn bundle_sha256(&self) -> &str {
        &self.identity
    }
}

/// Parse the engine's WAV: 16-bit PCM, one channel, a plausible rate. Chunks
/// are walked with every length checked against the buffer.
pub fn decode_wav_pcm16_mono(bytes: &[u8]) -> Result<Pcm, &'static str> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("the answer is not a WAV file");
    }
    let mut at = 12_usize;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body_start = at + 8;
        let body_end = body_start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or("a WAV chunk is truncated")?;
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err("the WAV fmt chunk is too short");
                }
                format = Some((
                    u16::from_le_bytes([body[0], body[1]]),
                    u16::from_le_bytes([body[2], body[3]]),
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u16::from_le_bytes([body[14], body[15]]),
                ));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word aligned.
        at = body_end + (size & 1);
    }
    let (tag, channels, rate, bits) = format.ok_or("the WAV has no fmt chunk")?;
    if tag != 1 || bits != 16 {
        return Err("the WAV is not 16-bit PCM");
    }
    if channels != 1 {
        return Err("the WAV is not mono");
    }
    if !(8_000..=192_000).contains(&rate) {
        return Err("the WAV sample rate is out of range");
    }
    let data = data.ok_or("the WAV has no data chunk")?;
    if data.is_empty() || data.len() % 2 != 0 {
        return Err("the WAV data is empty or not whole samples");
    }
    let samples = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32768.0)
        .collect();
    Ok(Pcm {
        samples,
        sample_rate: rate,
    })
}

#[cfg(test)]
mod tests {
    use super::super::voicevox_fake::{FakeBehaviour, FakeVoicevoxEngine};
    use super::super::{VoiceConfig, wav};
    use super::*;
    use std::time::Duration;

    /// A refused loopback connect can take a while on a slow host.
    const TEST_DEADLINE: Duration = Duration::from_secs(10);

    fn settings(text: &str) -> GenerationSettings {
        GenerationSettings::for_text(text, VoiceConfig::default())
    }

    fn synth(endpoint: &str, speaker: u32) -> VoicevoxSynthesizer {
        VoicevoxSynthesizer::new(&VoicevoxConfig::new(endpoint, speaker)).expect("synthesizer")
    }

    fn deadline() -> Instant {
        Instant::now() + TEST_DEADLINE
    }

    #[test]
    fn only_a_loopback_http_origin_is_accepted() {
        for ok in [
            "http://127.0.0.1:50021",
            "http://127.0.0.1:50021/",
            "http://[::1]:50021",
            "http://localhost:50021",
        ] {
            assert!(VoicevoxConfig::new(ok, 3).validate().is_ok(), "{ok}");
        }
        for bad in [
            "http://192.168.1.4:50021",
            "http://voicevox.example:50021",
            "http://127.0.0.1.attacker.invalid:50021",
            "http://localhost.attacker.invalid:50021",
            "http://127.0.0.1@attacker.invalid:50021",
            "http://user:pw@127.0.0.1:50021",
            "https://127.0.0.1:50021",
            "http://127.0.0.1:50021/audio_query",
            "http://127.0.0.1:50021/?speaker=1",
            "http://0.0.0.0:50021",
            "127.0.0.1:50021",
            "",
        ] {
            assert!(
                matches!(
                    VoicevoxConfig::new(bad, 3).validate(),
                    Err(VoicevoxConfigError::Endpoint(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn bounds_are_validated() {
        let mut config = VoicevoxConfig::new("http://127.0.0.1:50021", 1);
        config.max_text_chars = 0;
        assert!(matches!(
            config.validate(),
            Err(VoicevoxConfigError::Bound("max_text_chars"))
        ));
        let mut config = VoicevoxConfig::new("http://127.0.0.1:50021", 1);
        config.max_response_bytes = 10;
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_credit_line_is_required_and_bounded() {
        assert_eq!(
            validate_credit_line("  VOICEVOX:ずんだもん ").unwrap(),
            "VOICEVOX:ずんだもん"
        );
        assert!(validate_credit_line("   ").is_err());
        assert!(validate_credit_line("VOICEVOX:a\nb").is_err());
        assert!(validate_credit_line(&"あ".repeat(MAX_CREDIT_CHARS + 1)).is_err());
        assert!(validate_credit_line(&"あ".repeat(MAX_CREDIT_CHARS)).is_ok());
    }

    #[test]
    fn the_two_step_api_is_driven_with_the_text_speaker_and_query_body() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour::default());
        let synth = synth(&engine.endpoint(), 8);
        let pcm = synth
            .synthesize("こんにちは、世界 & more=1", &settings("x"), deadline())
            .expect("audio");
        assert_eq!(pcm.sample_rate, 24_000);
        assert!(!pcm.samples.is_empty());

        let requests = engine.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].route(), "/audio_query");
        // The text is percent-encoded in the query string and round-trips
        // exactly, including characters that would break a naive URL.
        assert_eq!(
            requests[0].query_param("text").as_deref(),
            Some("こんにちは、世界 & more=1")
        );
        assert_eq!(requests[0].query_param("speaker").as_deref(), Some("8"));
        assert_eq!(requests[1].route(), "/synthesis");
        assert_eq!(requests[1].query_param("speaker").as_deref(), Some("8"));
        assert_eq!(
            requests[1].query_param("text"),
            None,
            "text only in audio_query"
        );
        let body: Value = serde_json::from_slice(&requests[1].body).expect("query JSON");
        // The engine's own query came back unchanged, plus explicit mono.
        assert_eq!(body["accent_phrases"], json!([]));
        assert_eq!(body["speedScale"], json!(1.0));
        assert_eq!(body["outputStereo"], json!(false));
        assert_eq!(
            requests[1].header("content-type").as_deref(),
            Some("application/json")
        );
    }

    #[test]
    fn the_cache_identity_names_the_speaker_not_the_text() {
        let a = synth("http://127.0.0.1:50021", 3);
        let b = synth("http://127.0.0.1:50021", 8);
        assert_ne!(a.bundle_sha256(), b.bundle_sha256());
        assert_eq!(
            a.bundle_sha256(),
            synth("http://127.0.0.1:50022", 3).bundle_sha256()
        );
    }

    #[test]
    fn identical_text_is_identical_audio_from_a_deterministic_engine() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour::default());
        let synth = synth(&engine.endpoint(), 3);
        let a = synth
            .synthesize("同じ文章", &settings("a"), deadline())
            .unwrap();
        let b = synth
            .synthesize("同じ文章", &settings("b"), deadline())
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn engine_failures_are_typed_and_never_carry_the_text_or_url() {
        let secret = "ひみつの本文SECRET";
        for behaviour in [
            FakeBehaviour {
                query_status: 500,
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                synthesis_status: 422,
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                query_body: Some(b"not json".to_vec()),
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                query_body: Some(b"[1,2]".to_vec()),
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                wav_body: Some(b"RIFFnope".to_vec()),
                ..FakeBehaviour::default()
            },
        ] {
            let engine = FakeVoicevoxEngine::start(behaviour);
            let synth = synth(&engine.endpoint(), 3);
            match synth.synthesize(secret, &settings(secret), deadline()) {
                Err(SynthError::Engine(message)) => {
                    assert!(!message.contains("SECRET"), "{message}");
                    assert!(!message.contains("127.0.0.1"), "{message}");
                }
                other => panic!("expected an engine error, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unreachable_engine_is_an_engine_error_without_the_url() {
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            endpoint
        };
        let synth = synth(&dead, 3);
        match synth.synthesize("SECRETtext", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => {
                assert!(message.contains("unreachable"), "{message}");
                assert!(!message.contains("SECRET") && !message.contains("127.0.0.1"));
            }
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_slow_engine_hits_the_deadline_and_a_spent_deadline_makes_no_call() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour {
            delay: Duration::from_millis(1_500),
            ..FakeBehaviour::default()
        });
        let synth = synth(&engine.endpoint(), 3);
        let started = Instant::now();
        let slow = synth.synthesize(
            "遅い",
            &settings("x"),
            Instant::now() + Duration::from_millis(200),
        );
        assert_eq!(slow, Err(SynthError::DeadlineExceeded));
        assert!(started.elapsed() < Duration::from_millis(1_400));

        let before = engine.requests().len();
        let spent = synth.synthesize("今", &settings("x"), Instant::now());
        assert_eq!(spent, Err(SynthError::DeadlineExceeded));
        assert_eq!(
            engine.requests().len(),
            before,
            "no request after the deadline"
        );
    }

    #[test]
    fn oversize_text_and_oversize_answers_are_refused() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour::default());
        let mut config = VoicevoxConfig::new(engine.endpoint(), 3);
        config.max_text_chars = 5;
        let small_text = VoicevoxSynthesizer::new(&config).unwrap();
        assert!(matches!(
            small_text.synthesize("あいうえおか", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert!(engine.requests().is_empty(), "refused before any call");

        let mut config = VoicevoxConfig::new(engine.endpoint(), 3);
        config.max_response_bytes = 1024;
        let small_wav = VoicevoxSynthesizer::new(&config).unwrap();
        match small_wav.synthesize("あ", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => assert!(message.contains("limit"), "{message}"),
            other => panic!("expected the byte limit, got {other:?}"),
        }
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour {
            query_status: 307,
            ..FakeBehaviour::default()
        });
        let synth = synth(&engine.endpoint(), 3);
        assert!(matches!(
            synth.synthesize("あ", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert_eq!(engine.requests().len(), 1);
    }

    #[test]
    fn wav_decoding_is_strict() {
        let good = wav::encode_pcm16(&[0.0, 0.5, -0.5], 24_000).unwrap();
        let pcm = decode_wav_pcm16_mono(&good).unwrap();
        assert_eq!(pcm.sample_rate, 24_000);
        assert_eq!(pcm.samples.len(), 3);
        assert!((pcm.samples[1] - 0.5).abs() < 1e-3);

        // Stereo, 8-bit, float, truncated chunk, no data, odd data.
        let mut stereo = good.clone();
        stereo[22] = 2;
        assert_eq!(decode_wav_pcm16_mono(&stereo), Err("the WAV is not mono"));
        let mut eight = good.clone();
        eight[34] = 8;
        assert_eq!(
            decode_wav_pcm16_mono(&eight),
            Err("the WAV is not 16-bit PCM")
        );
        let mut float = good.clone();
        float[20] = 3;
        assert_eq!(
            decode_wav_pcm16_mono(&float),
            Err("the WAV is not 16-bit PCM")
        );
        assert_eq!(
            decode_wav_pcm16_mono(&good[..good.len() - 2]),
            Err("a WAV chunk is truncated")
        );
        assert!(decode_wav_pcm16_mono(&good[..36]).is_err());
        assert!(decode_wav_pcm16_mono(b"RIFF").is_err());
        let mut rate = good.clone();
        rate[24..28].copy_from_slice(&1_000_000_u32.to_le_bytes());
        assert_eq!(
            decode_wav_pcm16_mono(&rate),
            Err("the WAV sample rate is out of range")
        );
        // An unknown chunk before `data` (LIST) is skipped, with padding.
        let mut with_list = good[..36].to_vec();
        with_list.extend_from_slice(b"LIST");
        with_list.extend_from_slice(&3_u32.to_le_bytes());
        with_list.extend_from_slice(&[1, 2, 3, 0]);
        with_list.extend_from_slice(&good[36..]);
        assert_eq!(decode_wav_pcm16_mono(&with_list).unwrap().samples.len(), 3);
    }
}
