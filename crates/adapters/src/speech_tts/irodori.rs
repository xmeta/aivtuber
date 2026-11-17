//! Optional Irodori-TTS backend for the local TTS bridge (issue #411).
//!
//! [`IrodoriSynthesizer`] is a [`Synthesizer`]: plugged into the existing
//! [`super::server::TtsServer`], it turns the *existing* normalized TTS
//! contract into one OpenAI-style request to the official Irodori-TTS-Server
//! (`POST /v1/audio/speech`, answered with a WAV). The server is a separate,
//! operator-run process on loopback; this project links, bundles and downloads
//! nothing of it. Every server detail (request fields, the 48 kHz WAV) stays in
//! this file.
//!
//! Invariants:
//! * Loopback only: a non-loopback, non-`http://` or path-carrying endpoint is
//!   refused at construction, with the same host check the `local` profile
//!   applies to Thinking.
//! * The voice selection is configuration and constant for the session: every
//!   request carries the operator's fixed seed and either `no_ref` (the model's
//!   own unconditioned voice) or the name of a voice the operator installed on
//!   the server. There is deliberately no field that carries reference audio:
//!   the request body is built from a fixed set of fields and nothing in the
//!   normalized request (`voice_model`, `style`, the text) can add one.
//!   *Known limit (issue #411, measured):* without a reference the seed makes
//!   the output reproducible for identical text, but it does not keep one
//!   speaker across different texts; see the determinism note below.
//! * Bounded: the text length, the WAV size and one shared deadline are capped;
//!   no redirect is followed and no proxy is used.
//! * Errors never carry the spoken text or the URL; they are fixed strings plus
//!   a status code or an I/O kind.
//! * The server's output is data: the WAV is parsed strictly (16-bit PCM mono)
//!   and re-encoded by the bridge server, never passed through.
//!
//! Determinism: with a fixed seed and no reference the same text gives the same
//! audio (EXP-0006: identical output in 50 of 50 replies on one host, one
//! checkpoint, one code revision; the issue #411 smoke: byte-identical on a
//! repeat). Different texts at the same seed measured as different speakers
//! (see `docs/production-runtime.adoc`), so a seed alone is not a stable
//! character voice. The cache identity contains the seed, the voice name and the
//! operator's model label, not the server revision.

use super::voicevox::decode_wav_pcm16_mono;
use super::{GenerationSettings, Pcm, SynthError, Synthesizer};
use crate::{SystemHostResolver, verify_loopback_endpoint};
use serde_json::{Value, json};
use std::io::Read;
use std::time::Instant;

/// `backend_name` recorded in TTS provenance for this backend.
pub const BACKEND_NAME: &str = "irodori-tts-server";

/// The server's route.
pub const SPEECH_ROUTE: &str = "v1/audio/speech";

/// Printed at startup (bilingual, like the other fixed guide lines): the
/// backend is experimental and its reading accuracy is the open question.
pub const EXPERIMENTAL_NOTICE: &str = "EXPERIMENTAL: Irodori-TTS reads Japanese less accurately than the default voice in the project's round-trip test (EXP-0006: about 13% character error against 4.0%, with first words sometimes lost); listen before streaming / 実験的: Irodori-TTS の読みはラウンドトリップ試験で標準の声より不正確でした(EXP-0006: 文字誤り率およそ13%、標準は4.0%、先頭の語が欠けることがあります)。配信前に必ず聞いて確認してください";

/// Default for [`IrodoriConfig::seed`]: the seed EXP-0006 measured with.
pub const DEFAULT_SEED: u32 = 1234;
/// Default server-side model name (`IRODORI_MODEL_NAME` of the server).
pub const DEFAULT_MODEL_NAME: &str = "irodori-tts";
/// Default for [`IrodoriConfig::max_response_bytes`]: 16 MiB is about 170 s of
/// 48 kHz 16-bit mono, above the 120 s the WAV encoder accepts.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Default for [`IrodoriConfig::max_text_chars`], matching the server's.
pub const DEFAULT_MAX_TEXT_CHARS: usize = 500;
/// Longest voice name, model name or label accepted.
pub const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrodoriConfig {
    /// Origin of the server, e.g. `http://127.0.0.1:8088`. Loopback only.
    pub endpoint: String,
    /// The `model` field the server requires (`IRODORI_MODEL_NAME`).
    pub model_name: String,
    /// Sent on every request so the sampled voice is constant.
    pub seed: u32,
    /// A voice the operator installed on the server, addressed by name. `None`
    /// sends `no_ref` (the model's unconditioned voice, selected by the seed).
    pub voice: Option<String>,
    /// Free operator label of the checkpoint and precision (for example
    /// `LUNA-bf16`), mixed into the cache identity so swapping the checkpoint
    /// does not replay cached audio.
    pub model_label: Option<String>,
    pub max_response_bytes: usize,
    pub max_text_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrodoriConfigError {
    /// The endpoint is not a loopback `http://` origin (or has credentials,
    /// a path, a query or a fragment).
    Endpoint(String),
    /// A name (voice, model, label) is empty, too long or has forbidden
    /// characters.
    Name(&'static str),
    /// A bound is zero or out of range.
    Bound(&'static str),
}

impl std::fmt::Display for IrodoriConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint(reason) => write!(f, "Irodori-TTS endpoint refused: {reason}"),
            Self::Name(name) => write!(
                f,
                "Irodori-TTS {name} must be 1 to {MAX_NAME_CHARS} ASCII letters, digits, '.', '_' or '-'"
            ),
            Self::Bound(name) => write!(f, "Irodori-TTS bound out of range: {name}"),
        }
    }
}

impl std::error::Error for IrodoriConfigError {}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl IrodoriConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model_name: DEFAULT_MODEL_NAME.to_owned(),
            seed: DEFAULT_SEED,
            voice: None,
            model_label: None,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_text_chars: DEFAULT_MAX_TEXT_CHARS,
        }
    }

    /// The validated server origin, so a later `join` cannot be redirected to
    /// another path by the operator's value.
    pub fn validated_origin(&self) -> Result<url::Url, IrodoriConfigError> {
        let refuse = |reason: &str| IrodoriConfigError::Endpoint(reason.to_owned());
        let url = url::Url::parse(&self.endpoint).map_err(|_| refuse("not a valid URL"))?;
        if url.scheme() != "http" {
            return Err(refuse("scheme must be http (the server is on loopback)"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refuse("must not embed credentials"));
        }
        if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
            return Err(refuse("give the origin only, e.g. http://127.0.0.1:8088"));
        }
        verify_loopback_endpoint(&self.endpoint, &SystemHostResolver)
            .map_err(|_| refuse("host is not loopback (use 127.0.0.1, [::1] or localhost)"))?;
        Ok(url)
    }

    pub fn validate(&self) -> Result<(), IrodoriConfigError> {
        self.validated_origin()?;
        if !valid_name(&self.model_name) {
            return Err(IrodoriConfigError::Name("model name"));
        }
        if self.voice.as_deref().is_some_and(|v| !valid_name(v)) {
            return Err(IrodoriConfigError::Name("voice name"));
        }
        if self.model_label.as_deref().is_some_and(|v| !valid_name(v)) {
            return Err(IrodoriConfigError::Name("model label"));
        }
        if self.max_response_bytes < 1024 || self.max_response_bytes > 64 * 1024 * 1024 {
            return Err(IrodoriConfigError::Bound("max_response_bytes"));
        }
        if !(1..=2_000).contains(&self.max_text_chars) {
            return Err(IrodoriConfigError::Bound("max_text_chars"));
        }
        Ok(())
    }
}

pub struct IrodoriSynthesizer {
    url: url::Url,
    model_name: String,
    seed: u32,
    voice: Option<String>,
    max_response_bytes: usize,
    max_text_chars: usize,
    agent: ureq::Agent,
    identity: String,
}

impl std::fmt::Debug for IrodoriSynthesizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrodoriSynthesizer")
            .field("url", &self.url.as_str())
            .field("seed", &self.seed)
            .field("voice", &self.voice)
            .finish()
    }
}

impl IrodoriSynthesizer {
    pub fn new(config: &IrodoriConfig) -> Result<Self, IrodoriConfigError> {
        config.validate()?;
        let url = config
            .validated_origin()?
            .join(SPEECH_ROUTE)
            .map_err(|_| IrodoriConfigError::Endpoint("cannot build the server URL".to_owned()))?;
        Ok(Self {
            url,
            model_name: config.model_name.clone(),
            seed: config.seed,
            voice: config.voice.clone(),
            max_response_bytes: config.max_response_bytes,
            max_text_chars: config.max_text_chars,
            agent: ureq::Agent::new_with_defaults(),
            identity: format!(
                "irodori-tts-server-http;model={};seed={};voice={};label={}",
                config.model_name,
                config.seed,
                config.voice.as_deref().unwrap_or("-"),
                config.model_label.as_deref().unwrap_or("-"),
            ),
        })
    }

    /// The request body: a fixed set of fields. The voice is pinned by the seed
    /// and either `no_ref` or the operator's named voice; nothing from the
    /// normalized request except the text is copied in.
    fn request_body(&self, text: &str) -> Value {
        let mut irodori = json!({ "seed": self.seed });
        let mut body = json!({
            "model": self.model_name,
            "input": text,
            "response_format": "wav",
        });
        match &self.voice {
            Some(voice) => body["voice"] = json!(voice),
            None => irodori["no_ref"] = json!(true),
        }
        body["irodori"] = irodori;
        body
    }
}

fn map_transport_error(error: &ureq::Error) -> SynthError {
    match error {
        ureq::Error::Timeout(_) => SynthError::DeadlineExceeded,
        // Never `error.to_string()`: it can contain the request URL.
        ureq::Error::Io(io) => {
            SynthError::Engine(format!("irodori: server unreachable ({:?})", io.kind()))
        }
        _ => SynthError::Engine("irodori: transport error".to_owned()),
    }
}

impl Synthesizer for IrodoriSynthesizer {
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
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SynthError::DeadlineExceeded);
        }
        let body = serde_json::to_vec(&self.request_body(text))
            .map_err(|_| SynthError::Engine("irodori: cannot encode the request".to_owned()))?;
        let request = self
            .agent
            .post(self.url.as_str())
            .config()
            .timeout_global(Some(remaining))
            .http_status_as_error(false)
            .max_redirects(0)
            // The server is on this machine: never through a proxy from the
            // environment.
            .proxy(None)
            .build()
            .header("content-type", "application/json");
        let mut response = request
            .send(&body[..])
            .map_err(|error| map_transport_error(&error))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(SynthError::Engine(format!(
                "irodori: server answered HTTP {status}"
            )));
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
                        "irodori: cannot read the answer ({:?})",
                        error.kind()
                    ))
                }
            })?;
        if bytes.len() > cap {
            return Err(SynthError::Engine(format!(
                "irodori: answer exceeds the {cap} byte limit"
            )));
        }
        decode_wav_pcm16_mono(&bytes)
            .map_err(|reason| SynthError::Engine(format!("irodori: {reason}")))
    }

    /// Not a digest: a stable identity string (server kind, model, seed, voice
    /// and the operator's label) that the server mixes into the content
    /// address and records as the model version.
    fn bundle_sha256(&self) -> &str {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::super::irodori_fake::{FakeBehaviour, FakeIrodoriServer};
    use super::super::{VoiceConfig, wav};
    use super::*;
    use std::time::Duration;

    const TEST_DEADLINE: Duration = Duration::from_secs(10);

    fn settings(text: &str) -> GenerationSettings {
        GenerationSettings::for_text(text, VoiceConfig::default())
    }

    fn synth(endpoint: &str) -> IrodoriSynthesizer {
        IrodoriSynthesizer::new(&IrodoriConfig::new(endpoint)).expect("synthesizer")
    }

    fn deadline() -> Instant {
        Instant::now() + TEST_DEADLINE
    }

    #[test]
    fn only_a_loopback_http_origin_is_accepted() {
        for ok in [
            "http://127.0.0.1:8088",
            "http://127.0.0.1:8088/",
            "http://[::1]:8088",
            "http://localhost:8088",
        ] {
            assert!(IrodoriConfig::new(ok).validate().is_ok(), "{ok}");
        }
        for bad in [
            "http://192.168.1.4:8088",
            "http://tts.example:8088",
            "http://127.0.0.1.attacker.invalid:8088",
            "http://127.0.0.1@attacker.invalid:8088",
            "http://user:pw@127.0.0.1:8088",
            "https://127.0.0.1:8088",
            "http://127.0.0.1:8088/v1/audio/speech",
            "http://127.0.0.1:8088/?x=1",
            "http://0.0.0.0:8088",
            "127.0.0.1:8088",
            "",
        ] {
            assert!(
                matches!(
                    IrodoriConfig::new(bad).validate(),
                    Err(IrodoriConfigError::Endpoint(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn names_and_bounds_are_validated() {
        let base = IrodoriConfig::new("http://127.0.0.1:8088");
        for bad in ["", "a b", "../x", "声", &"a".repeat(MAX_NAME_CHARS + 1)] {
            let mut config = base.clone();
            config.voice = Some(bad.to_owned());
            assert_eq!(
                config.validate(),
                Err(IrodoriConfigError::Name("voice name")),
                "{bad:?}"
            );
        }
        let mut config = base.clone();
        config.model_label = Some("LUNA bf16".to_owned());
        assert!(config.validate().is_err());
        config.model_label = Some("LUNA-bf16".to_owned());
        assert!(config.validate().is_ok());
        let mut config = base.clone();
        config.max_text_chars = 0;
        assert_eq!(
            config.validate(),
            Err(IrodoriConfigError::Bound("max_text_chars"))
        );
        let mut config = base;
        config.max_response_bytes = 10;
        assert!(config.validate().is_err());
    }

    #[test]
    fn every_request_pins_the_voice_with_the_seed_and_carries_no_reference() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut config = IrodoriConfig::new(server.endpoint());
        config.seed = 4242;
        let synth = IrodoriSynthesizer::new(&config).unwrap();
        for text in ["こんにちは、世界 & more=1", "二つ目の文"] {
            let pcm = synth.synthesize(text, &settings("x"), deadline()).unwrap();
            assert_eq!(pcm.sample_rate, 48_000);
            assert!(!pcm.samples.is_empty());
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for (request, text) in requests
            .iter()
            .zip(["こんにちは、世界 & more=1", "二つ目の文"])
        {
            assert_eq!(request.method, "POST");
            assert_eq!(request.route(), "/v1/audio/speech");
            assert_eq!(
                request.header("content-type").as_deref(),
                Some("application/json")
            );
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["model"], json!("irodori-tts"));
            assert_eq!(body["input"], json!(text));
            assert_eq!(body["response_format"], json!("wav"));
            assert_eq!(body["irodori"]["seed"], json!(4242));
            assert_eq!(body["irodori"]["no_ref"], json!(true));
            assert!(body.get("voice").is_none());
            // The complete set of fields: nothing that could carry audio.
            let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["input", "irodori", "model", "response_format"]);
            let mut inner: Vec<_> = body["irodori"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            inner.sort();
            assert_eq!(inner, ["no_ref", "seed"]);
        }
    }

    #[test]
    fn a_named_voice_is_sent_by_name_with_the_seed_and_without_no_ref() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut config = IrodoriConfig::new(server.endpoint());
        config.voice = Some("my-voice".to_owned());
        config.model_name = "irodori-tts-luna".to_owned();
        let synth = IrodoriSynthesizer::new(&config).unwrap();
        synth.synthesize("あ", &settings("x"), deadline()).unwrap();
        let body: Value = serde_json::from_slice(&server.requests()[0].body).unwrap();
        assert_eq!(body["model"], json!("irodori-tts-luna"));
        assert_eq!(body["voice"], json!("my-voice"));
        assert_eq!(body["irodori"]["seed"], json!(DEFAULT_SEED));
        assert!(body["irodori"].get("no_ref").is_none());
    }

    #[test]
    fn the_normalized_request_cannot_change_the_voice_fields() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let synth = synth(&server.endpoint());
        let hostile = GenerationSettings::for_text(
            "x",
            VoiceConfig {
                speaker_id: 7,
                num_steps: 99,
                speed: 3.0,
            },
        );
        synth.synthesize("あ", &hostile, deadline()).unwrap();
        let plain = {
            synth.synthesize("あ", &settings("x"), deadline()).unwrap();
            server.requests()
        };
        assert_eq!(plain[0].body, plain[1].body);
    }

    #[test]
    fn the_cache_identity_names_seed_voice_and_label_not_the_text() {
        let base = IrodoriConfig::new("http://127.0.0.1:8088");
        let id = |f: &dyn Fn(&mut IrodoriConfig)| {
            let mut config = base.clone();
            f(&mut config);
            IrodoriSynthesizer::new(&config)
                .unwrap()
                .bundle_sha256()
                .to_owned()
        };
        let plain = id(&|_| {});
        assert_ne!(plain, id(&|c| c.seed = 1));
        assert_ne!(plain, id(&|c| c.voice = Some("v".to_owned())));
        assert_ne!(plain, id(&|c| c.model_label = Some("LUNA-bf16".to_owned())));
        assert_eq!(
            plain,
            IrodoriSynthesizer::new(&IrodoriConfig::new("http://127.0.0.1:9"))
                .unwrap()
                .bundle_sha256()
        );
    }

    #[test]
    fn identical_text_and_seed_is_identical_audio_and_a_new_seed_is_a_new_voice() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let a = synth(&server.endpoint());
        let first = a
            .synthesize("同じ文章", &settings("a"), deadline())
            .unwrap();
        let second = a
            .synthesize("同じ文章", &settings("b"), deadline())
            .unwrap();
        assert_eq!(first, second);
        let mut config = IrodoriConfig::new(server.endpoint());
        config.seed = 7;
        let other = IrodoriSynthesizer::new(&config).unwrap();
        let third = other
            .synthesize("同じ文章", &settings("a"), deadline())
            .unwrap();
        assert_ne!(first, third);
    }

    #[test]
    fn server_failures_are_typed_and_never_carry_the_text_or_url() {
        let secret = "ひみつの本文SECRET";
        for behaviour in [
            FakeBehaviour {
                status: 500,
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                status: 422,
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                status: 503,
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                wav_body: Some(b"RIFFnope".to_vec()),
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                wav_body: Some(b"{\"error\":\"not audio\"}".to_vec()),
                ..FakeBehaviour::default()
            },
            FakeBehaviour {
                wav_body: Some(Vec::new()),
                ..FakeBehaviour::default()
            },
        ] {
            let server = FakeIrodoriServer::start(behaviour);
            let synth = synth(&server.endpoint());
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
    fn a_malformed_wav_is_refused_even_when_the_status_is_ok() {
        let good = wav::encode_pcm16(&[0.0, 0.5, -0.5], 48_000).unwrap();
        let mut stereo = good.clone();
        stereo[22] = 2;
        let truncated = good[..good.len() - 2].to_vec();
        for body in [stereo, truncated] {
            let server = FakeIrodoriServer::start(FakeBehaviour {
                wav_body: Some(body),
                ..FakeBehaviour::default()
            });
            assert!(matches!(
                synth(&server.endpoint()).synthesize("あ", &settings("x"), deadline()),
                Err(SynthError::Engine(_))
            ));
        }
    }

    #[test]
    fn an_unreachable_server_is_an_engine_error_without_the_url() {
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
    fn a_slow_server_hits_the_deadline_and_a_spent_deadline_makes_no_call() {
        let server = FakeIrodoriServer::start(FakeBehaviour {
            delay: Duration::from_millis(1_500),
            ..FakeBehaviour::default()
        });
        let synth = synth(&server.endpoint());
        let started = Instant::now();
        let slow = synth.synthesize(
            "遅い",
            &settings("x"),
            Instant::now() + Duration::from_millis(200),
        );
        assert_eq!(slow, Err(SynthError::DeadlineExceeded));
        assert!(started.elapsed() < Duration::from_millis(1_400));

        let before = server.requests().len();
        let spent = synth.synthesize("今", &settings("x"), Instant::now());
        assert_eq!(spent, Err(SynthError::DeadlineExceeded));
        assert_eq!(
            server.requests().len(),
            before,
            "no request after the deadline"
        );
    }

    #[test]
    fn oversize_text_and_oversize_answers_are_refused() {
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut config = IrodoriConfig::new(server.endpoint());
        config.max_text_chars = 5;
        let small_text = IrodoriSynthesizer::new(&config).unwrap();
        assert!(matches!(
            small_text.synthesize("あいうえおか", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert!(server.requests().is_empty(), "refused before any call");

        let mut config = IrodoriConfig::new(server.endpoint());
        config.max_response_bytes = 1024;
        let small_wav = IrodoriSynthesizer::new(&config).unwrap();
        match small_wav.synthesize("あいうえおかきくけこ", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => assert!(message.contains("limit"), "{message}"),
            other => panic!("expected the byte limit, got {other:?}"),
        }
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let server = FakeIrodoriServer::start(FakeBehaviour {
            status: 307,
            ..FakeBehaviour::default()
        });
        assert!(matches!(
            synth(&server.endpoint()).synthesize("あ", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert_eq!(server.requests().len(), 1);
    }
}
