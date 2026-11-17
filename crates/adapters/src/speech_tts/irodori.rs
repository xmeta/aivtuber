//! Optional Irodori-TTS backend for the local TTS bridge (issue #411).
//!
//! [`IrodoriSynthesizer`] is a [`Synthesizer`]: plugged into the existing
//! [`super::server::TtsServer`], it turns the *existing* normalized TTS
//! contract into requests to the official Irodori-TTS-Server
//! (`POST /v1/audio/speech`, answered with a WAV). The server is a separate,
//! operator-run process on loopback; this project links, bundles and downloads
//! nothing of it. Every server detail (request fields, the voice upload, the
//! 48 kHz WAV) stays in this file.
//!
//! The speaker is fixed by a *self-synthesized anchor*: the first time it is
//! needed the bridge asks the server for one clip of a fixed anchor sentence
//! (no reference, the configured seed), keeps it on disk under a name that
//! hashes (model, label, seed, anchor text), uploads it to the server as a
//! named voice, and sends that voice name as the reference on every later
//! request. A fixed seed alone does not keep one speaker across different
//! texts (measured, issue #411); a reference does.
//!
//! Invariants:
//! * Loopback only: a non-loopback, non-`http://` or path-carrying endpoint is
//!   refused at construction, with the same host check the `local` profile
//!   applies to Thinking.
//! * No operator-supplied reference audio: there is no setting, path, URL or
//!   voice name for it. The only reference ever sent is the anchor this module
//!   synthesized, from a fixed sentence, so there is no path to clone a real
//!   person's voice. The request body is a fixed field set; nothing in the
//!   normalized request (`voice_model`, `style`, the text) can add one.
//! * Bounded: the text length, every answer's size and one shared deadline
//!   (anchor creation included) are capped; no redirect, no proxy.
//! * Errors never carry the spoken text or the URL; fixed strings plus a
//!   status code or an I/O kind. An anchor that cannot be made or uploaded
//!   fails the synthesis like any engine failure and is retried next time.
//! * The server's output is data: WAVs are parsed strictly (16-bit PCM mono)
//!   and re-encoded by the bridge server, never passed through.
//!
//! Determinism: identical text, seed and anchor give identical audio (EXP-0006,
//! the issue #411 smoke). The cache identity contains the model, label, seed
//! and anchor hash, not the server revision.

use super::voicevox::decode_wav_pcm16_mono;
use super::{GenerationSettings, Pcm, SynthError, Synthesizer};
use crate::{SystemHostResolver, verify_loopback_endpoint};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// `backend_name` recorded in TTS provenance for this backend.
pub const BACKEND_NAME: &str = "irodori-tts-server";

/// The server's routes.
pub const SPEECH_ROUTE: &str = "v1/audio/speech";
pub const VOICES_ROUTE: &str = "v1/audio/voices";

/// The fixed sentence the anchor clip says. It is part of the anchor's cache
/// key, so changing it makes a new anchor (and a new voice).
pub const ANCHOR_TEXT: &str = "こんにちは、今日も配信に来てくれてありがとう。これからいっしょに、ゆっくりお話ししていこうね。";

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
/// Longest model name or label accepted.
pub const MAX_NAME_CHARS: usize = 64;
/// Time allowed to make and upload the anchor, whatever the reply's own
/// deadline: a first anchor on a cold GPU measured 12 s, and an anchor that is
/// abandoned every time would never exist. The reply that triggered it still
/// obeys its own deadline; the next one finds the anchor ready.
const ANCHOR_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
/// Answers to the voice upload are small JSON.
const MAX_UPLOAD_ANSWER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrodoriConfig {
    /// Origin of the server, e.g. `http://127.0.0.1:8088`. Loopback only.
    pub endpoint: String,
    /// The `model` field the server requires (`IRODORI_MODEL_NAME`).
    pub model_name: String,
    /// Sent on every request; with the anchor it also makes the anchor
    /// reproducible.
    pub seed: u32,
    /// Free operator label of the checkpoint and precision (for example
    /// `LUNA-bf16`), part of the anchor key and the cache identity so swapping
    /// the checkpoint makes a new anchor.
    pub model_label: Option<String>,
    /// Where the anchor clips are kept; the bridge's own directory, never an
    /// operator-provided audio location.
    pub anchor_dir: PathBuf,
    pub max_response_bytes: usize,
    pub max_text_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrodoriConfigError {
    /// The endpoint is not a loopback `http://` origin (or has credentials,
    /// a path, a query or a fragment).
    Endpoint(String),
    /// A name (model, label) is empty, too long or has forbidden characters.
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
    /// `anchor_dir` is left empty here; the bridge sets it before the
    /// synthesizer is built, which refuses an empty one.
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model_name: DEFAULT_MODEL_NAME.to_owned(),
            seed: DEFAULT_SEED,
            model_label: None,
            anchor_dir: PathBuf::new(),
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

/// The anchor's name: it hashes everything that decides which speaker it is,
/// so a different seed, model name, label or anchor sentence is a different
/// anchor (and a different server-side voice).
fn anchor_id(config: &IrodoriConfig) -> String {
    let mut hasher = Sha256::new();
    for part in [
        config.model_name.as_str(),
        config.model_label.as_deref().unwrap_or("-"),
        &config.seed.to_string(),
        ANCHOR_TEXT,
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("aivtuber-anchor-{hex}")
}

pub struct IrodoriSynthesizer {
    speech_url: url::Url,
    voices_url: url::Url,
    model_name: String,
    seed: u32,
    max_response_bytes: usize,
    max_text_chars: usize,
    agent: ureq::Agent,
    identity: String,
    anchor_id: String,
    anchor_path: PathBuf,
    /// `true` once the server has the anchor voice. Held across the network
    /// calls that make it, so concurrent requests make one anchor.
    anchor_ready: Mutex<bool>,
}

impl std::fmt::Debug for IrodoriSynthesizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrodoriSynthesizer")
            .field("url", &self.speech_url.as_str())
            .field("seed", &self.seed)
            .field("anchor", &self.anchor_id)
            .finish()
    }
}

impl IrodoriSynthesizer {
    pub fn new(config: &IrodoriConfig) -> Result<Self, IrodoriConfigError> {
        config.validate()?;
        if config.anchor_dir.as_os_str().is_empty() {
            return Err(IrodoriConfigError::Bound("anchor_dir"));
        }
        let origin = config.validated_origin()?;
        let join = |route: &str| {
            origin
                .join(route)
                .map_err(|_| IrodoriConfigError::Endpoint("cannot build the server URL".to_owned()))
        };
        let id = anchor_id(config);
        Ok(Self {
            speech_url: join(SPEECH_ROUTE)?,
            voices_url: join(VOICES_ROUTE)?,
            model_name: config.model_name.clone(),
            seed: config.seed,
            max_response_bytes: config.max_response_bytes,
            max_text_chars: config.max_text_chars,
            agent: ureq::Agent::new_with_defaults(),
            identity: format!(
                "irodori-tts-server-http;model={};seed={};label={};anchor={id}",
                config.model_name,
                config.seed,
                config.model_label.as_deref().unwrap_or("-"),
            ),
            anchor_path: config.anchor_dir.join(format!("{id}.wav")),
            anchor_id: id,
            anchor_ready: Mutex::new(false),
        })
    }

    /// The anchor's name on the server and in the cache directory.
    pub fn anchor_id(&self) -> &str {
        &self.anchor_id
    }

    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }

    /// The request body: a fixed set of fields. `anchor` names the reference;
    /// without it the request is the unconditioned one that makes the anchor.
    fn request_body(&self, text: &str, anchor: bool) -> Value {
        let mut irodori = json!({ "seed": self.seed });
        let mut body = json!({
            "model": self.model_name,
            "input": text,
            "response_format": "wav",
        });
        if anchor {
            body["voice"] = json!(self.anchor_id);
        } else {
            irodori["no_ref"] = json!(true);
        }
        body["irodori"] = irodori;
        body
    }

    /// One bounded POST: `(status, body)` whatever the status, the body read
    /// under `cap`.
    fn post(
        &self,
        url: &url::Url,
        content_type: &str,
        body: &[u8],
        cap: usize,
        deadline: Instant,
    ) -> Result<(u16, Vec<u8>), SynthError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SynthError::DeadlineExceeded);
        }
        let request = self
            .agent
            .post(url.as_str())
            .config()
            .timeout_global(Some(remaining))
            .http_status_as_error(false)
            .max_redirects(0)
            // The server is on this machine: never through a proxy from the
            // environment.
            .proxy(None)
            .build()
            .header("content-type", content_type);
        let mut response = request
            .send(body)
            .map_err(|error| map_transport_error(&error))?;
        let status = response.status().as_u16();
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
        Ok((status, bytes))
    }

    /// A speech request answered with a validated WAV.
    fn speech(&self, text: &str, anchor: bool, deadline: Instant) -> Result<Vec<u8>, SynthError> {
        let body = serde_json::to_vec(&self.request_body(text, anchor))
            .map_err(|_| SynthError::Engine("irodori: cannot encode the request".to_owned()))?;
        let (status, bytes) = self.post(
            &self.speech_url,
            "application/json",
            &body,
            self.max_response_bytes,
            deadline,
        )?;
        if !(200..300).contains(&status) {
            if anchor && matches!(status, 400 | 404) {
                // The server does not know the anchor voice (restarted with a
                // new voices directory): upload it again next time.
                *self.anchor_ready.lock().unwrap_or_else(|e| e.into_inner()) = false;
            }
            return Err(SynthError::Engine(format!(
                "irodori: server answered HTTP {status}"
            )));
        }
        decode_wav_pcm16_mono(&bytes)
            .map_err(|reason| SynthError::Engine(format!("irodori: {reason}")))?;
        Ok(bytes)
    }

    /// The anchor clip: from disk when a valid one is cached, otherwise
    /// synthesized now (unconditioned, the configured seed) and kept.
    fn anchor_wav(&self, deadline: Instant) -> Result<Vec<u8>, SynthError> {
        if let Ok(bytes) = std::fs::read(&self.anchor_path)
            && bytes.len() <= self.max_response_bytes
            && decode_wav_pcm16_mono(&bytes).is_ok()
        {
            return Ok(bytes);
        }
        let bytes = self.speech(ANCHOR_TEXT, false, deadline)?;
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.anchor_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.anchor_path.with_extension("wav.tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &self.anchor_path)
        };
        // A cache that cannot be written costs a re-synthesis later, not this
        // reply.
        let _ = write();
        Ok(bytes)
    }

    /// Make sure the server has the anchor voice.
    fn ensure_anchor(&self, deadline: Instant) -> Result<(), SynthError> {
        let mut ready = self.anchor_ready.lock().unwrap_or_else(|e| e.into_inner());
        if *ready {
            return Ok(());
        }
        let deadline = deadline.max(Instant::now() + ANCHOR_BUDGET);
        let wav = self.anchor_wav(deadline)?;
        let boundary = format!("aivtuber-{}", &self.anchor_id["aivtuber-anchor-".len()..]);
        let mut body = Vec::with_capacity(wav.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"voice_id\"\r\n\r\n{}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
                self.anchor_id, self.anchor_id
            )
            .as_bytes(),
        );
        body.extend_from_slice(&wav);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (status, _) = self.post(
            &self.voices_url,
            &format!("multipart/form-data; boundary={boundary}"),
            &body,
            MAX_UPLOAD_ANSWER_BYTES,
            deadline,
        )?;
        // 409: an earlier run uploaded the same anchor (its name is a hash of
        // everything that defines it), which is what we want.
        if !matches!(status, 200..=299 | 409) {
            return Err(SynthError::Engine(format!(
                "irodori: anchor upload answered HTTP {status}"
            )));
        }
        *ready = true;
        Ok(())
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
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(SynthError::DeadlineExceeded);
        }
        self.ensure_anchor(deadline)?;
        let bytes = self.speech(text, true, deadline)?;
        decode_wav_pcm16_mono(&bytes)
            .map_err(|reason| SynthError::Engine(format!("irodori: {reason}")))
    }

    /// Not a digest: a stable identity string (server kind, model, seed, label
    /// and anchor hash) that the server mixes into the content address and
    /// records as the model version.
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-411-anchor-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn config(endpoint: &str, dir: &Path) -> IrodoriConfig {
        let mut config = IrodoriConfig::new(endpoint);
        config.anchor_dir = dir.to_path_buf();
        config
    }

    fn synth(endpoint: &str, dir: &Path) -> IrodoriSynthesizer {
        IrodoriSynthesizer::new(&config(endpoint, dir)).expect("synthesizer")
    }

    fn deadline() -> Instant {
        Instant::now() + TEST_DEADLINE
    }

    fn body(request: &super::super::voicevox_fake::RecordedRequest) -> Value {
        serde_json::from_slice(&request.body).expect("JSON body")
    }

    #[test]
    fn only_a_loopback_http_origin_is_accepted() {
        let dir = scratch("loopback");
        for ok in [
            "http://127.0.0.1:8088",
            "http://127.0.0.1:8088/",
            "http://[::1]:8088",
            "http://localhost:8088",
        ] {
            assert!(config(ok, &dir).validate().is_ok(), "{ok}");
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
                    config(bad, &dir).validate(),
                    Err(IrodoriConfigError::Endpoint(_))
                ),
                "{bad}"
            );
            assert!(IrodoriSynthesizer::new(&config(bad, &dir)).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_and_bounds_are_validated() {
        let dir = scratch("bounds");
        let base = config("http://127.0.0.1:8088", &dir);
        let mut c = base.clone();
        c.model_label = Some("LUNA bf16".to_owned());
        assert_eq!(c.validate(), Err(IrodoriConfigError::Name("model label")));
        c.model_label = Some("LUNA-bf16".to_owned());
        assert!(c.validate().is_ok());
        let mut c = base.clone();
        c.model_name = "../x".to_owned();
        assert_eq!(c.validate(), Err(IrodoriConfigError::Name("model name")));
        let mut c = base.clone();
        c.max_text_chars = 0;
        assert_eq!(
            c.validate(),
            Err(IrodoriConfigError::Bound("max_text_chars"))
        );
        let mut c = base.clone();
        c.max_response_bytes = 10;
        assert!(c.validate().is_err());
        let mut c = base;
        c.anchor_dir = PathBuf::new();
        assert_eq!(
            IrodoriSynthesizer::new(&c).unwrap_err(),
            IrodoriConfigError::Bound("anchor_dir")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_anchor_is_made_once_uploaded_and_sent_as_the_reference_on_every_request() {
        let dir = scratch("flow");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut c = config(&server.endpoint(), &dir);
        c.seed = 4242;
        let synth = IrodoriSynthesizer::new(&c).unwrap();
        for text in ["こんにちは、世界 & more=1", "二つ目の文", "三つ目"] {
            let pcm = synth.synthesize(text, &settings("x"), deadline()).unwrap();
            assert_eq!(pcm.sample_rate, 48_000);
            assert!(!pcm.samples.is_empty());
        }
        let requests = server.requests();
        let routes: Vec<&str> = requests.iter().map(|r| r.route()).collect();
        assert_eq!(
            routes,
            [
                "/v1/audio/speech",
                "/v1/audio/voices",
                "/v1/audio/speech",
                "/v1/audio/speech",
                "/v1/audio/speech"
            ],
            "anchor synthesis and one upload, then only speech"
        );
        // Request 0: the anchor itself, unconditioned, fixed sentence and seed.
        let anchor = body(&requests[0]);
        assert_eq!(anchor["input"], json!(ANCHOR_TEXT));
        assert_eq!(anchor["irodori"]["no_ref"], json!(true));
        assert_eq!(anchor["irodori"]["seed"], json!(4242));
        assert!(anchor.get("voice").is_none());
        // Request 1: the upload names the anchor and carries a WAV.
        let upload = String::from_utf8_lossy(&requests[1].body).into_owned();
        assert!(upload.contains(synth.anchor_id()), "{upload}");
        assert!(upload.contains("RIFF"));
        assert!(
            requests[1]
                .header("content-type")
                .is_some_and(|t| t.starts_with("multipart/form-data; boundary="))
        );
        assert_eq!(server.voices(), [synth.anchor_id().to_owned()]);
        // Every reply request: the anchor as the reference, the same seed, no
        // `no_ref`, and exactly this key set.
        for (request, text) in
            requests[2..]
                .iter()
                .zip(["こんにちは、世界 & more=1", "二つ目の文", "三つ目"])
        {
            let b = body(request);
            assert_eq!(b["model"], json!("irodori-tts"));
            assert_eq!(b["input"], json!(text));
            assert_eq!(b["response_format"], json!("wav"));
            assert_eq!(b["voice"], json!(synth.anchor_id()));
            assert_eq!(b["irodori"]["seed"], json!(4242));
            let mut keys: Vec<_> = b.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                ["input", "irodori", "model", "response_format", "voice"]
            );
            let inner: Vec<_> = b["irodori"].as_object().unwrap().keys().cloned().collect();
            assert_eq!(inner, ["seed"]);
        }
        // The clip is cached on disk and is a valid WAV.
        let cached = std::fs::read(synth.anchor_path()).unwrap();
        assert!(decode_wav_pcm16_mono(&cached).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cached_anchor_is_reused_by_a_new_synthesizer_without_resynthesizing() {
        let dir = scratch("cached");
        let first_server = FakeIrodoriServer::start(FakeBehaviour::default());
        synth(&first_server.endpoint(), &dir)
            .synthesize("あ", &settings("x"), deadline())
            .unwrap();
        // A restarted app and a restarted server with no voices.
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let again = synth(&server.endpoint(), &dir);
        again.synthesize("い", &settings("x"), deadline()).unwrap();
        let routes: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.route().to_owned())
            .collect();
        assert_eq!(
            routes,
            ["/v1/audio/voices", "/v1/audio/speech"],
            "upload the cached clip, no anchor synthesis"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cached_anchor_is_replaced() {
        let dir = scratch("corrupt");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let synth = synth(&server.endpoint(), &dir);
        std::fs::write(synth.anchor_path(), b"not a wav").unwrap();
        synth.synthesize("あ", &settings("x"), deadline()).unwrap();
        assert!(decode_wav_pcm16_mono(&std::fs::read(synth.anchor_path()).unwrap()).is_ok());
        assert_eq!(server.requests().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_seed_model_or_label_is_a_new_anchor() {
        let dir = scratch("regen");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut ids = Vec::new();
        for edit in [
            (|_: &mut IrodoriConfig| {}) as fn(&mut IrodoriConfig),
            |c| c.seed = 7,
            |c| c.model_name = "irodori-tts-luna".to_owned(),
            |c| c.model_label = Some("LUNA-bf16".to_owned()),
        ] {
            let mut c = config(&server.endpoint(), &dir);
            edit(&mut c);
            let synth = IrodoriSynthesizer::new(&c).unwrap();
            synth.synthesize("あ", &settings("x"), deadline()).unwrap();
            assert!(synth.anchor_path().is_file());
            ids.push(synth.anchor_id().to_owned());
        }
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 4, "{ids:?}");
        // Four anchors synthesized, four uploaded, each reply on its own anchor.
        let anchors = server
            .requests()
            .iter()
            .filter(|r| body_has(r, "no_ref"))
            .count();
        assert_eq!(anchors, 4);
        assert_eq!(server.voices().len(), 4);
        // The endpoint is not part of the anchor.
        let other = config("http://127.0.0.1:9", &dir);
        assert_eq!(anchor_id(&other), ids[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn body_has(request: &super::super::voicevox_fake::RecordedRequest, key: &str) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .is_some_and(|b| b["irodori"].get(key).is_some())
    }

    #[test]
    fn nothing_in_the_normalized_request_changes_the_fields() {
        let dir = scratch("hostile");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let synth = synth(&server.endpoint(), &dir);
        let hostile = GenerationSettings::for_text(
            "x",
            VoiceConfig {
                speaker_id: 7,
                num_steps: 99,
                speed: 3.0,
            },
        );
        synth.synthesize("あ", &hostile, deadline()).unwrap();
        synth.synthesize("あ", &settings("x"), deadline()).unwrap();
        let requests = server.requests();
        assert_eq!(requests[2].body, requests[3].body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_identity_names_model_seed_label_and_anchor_not_the_text() {
        let dir = scratch("identity");
        let base = config("http://127.0.0.1:8088", &dir);
        let id = |f: &dyn Fn(&mut IrodoriConfig)| {
            let mut c = base.clone();
            f(&mut c);
            IrodoriSynthesizer::new(&c)
                .unwrap()
                .bundle_sha256()
                .to_owned()
        };
        let plain = id(&|_| {});
        assert!(plain.contains("anchor=aivtuber-anchor-"));
        assert_ne!(plain, id(&|c| c.seed = 1));
        assert_ne!(plain, id(&|c| c.model_label = Some("LUNA-bf16".to_owned())));
        assert_ne!(plain, id(&|c| c.model_name = "other".to_owned()));
        assert_eq!(plain, id(&|c| c.endpoint = "http://127.0.0.1:9".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identical_text_and_anchor_is_identical_audio_and_a_new_seed_is_a_new_voice() {
        let dir = scratch("determinism");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let a = synth(&server.endpoint(), &dir);
        let first = a
            .synthesize("同じ文章", &settings("a"), deadline())
            .unwrap();
        let second = a
            .synthesize("同じ文章", &settings("b"), deadline())
            .unwrap();
        assert_eq!(first, second);
        let mut c = config(&server.endpoint(), &dir);
        c.seed = 7;
        let third = IrodoriSynthesizer::new(&c)
            .unwrap()
            .synthesize("同じ文章", &settings("a"), deadline())
            .unwrap();
        assert_ne!(first, third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_anchor_that_cannot_be_made_or_uploaded_fails_like_any_engine_error_and_is_retried() {
        let secret = "ひみつの本文SECRET";
        // The server fails the anchor synthesis.
        let dir = scratch("anchor-fail");
        let server = FakeIrodoriServer::start(FakeBehaviour {
            status: 500,
            ..FakeBehaviour::default()
        });
        let s = synth(&server.endpoint(), &dir);
        for _ in 0..2 {
            match s.synthesize(secret, &settings(secret), deadline()) {
                Err(SynthError::Engine(message)) => {
                    assert!(message.contains("500"), "{message}");
                    assert!(!message.contains("SECRET") && !message.contains("127.0.0.1"));
                }
                other => panic!("expected an engine error, got {other:?}"),
            }
        }
        assert!(!s.anchor_path().exists(), "a failed anchor is not cached");
        assert_eq!(server.requests().len(), 2, "tried again, never a reply");
        // The upload fails: the clip is kept, the reply is not attempted.
        let dir2 = scratch("upload-fail");
        let server = FakeIrodoriServer::start(FakeBehaviour {
            upload_status: 500,
            ..FakeBehaviour::default()
        });
        let s = synth(&server.endpoint(), &dir2);
        for _ in 0..2 {
            match s.synthesize(secret, &settings(secret), deadline()) {
                Err(SynthError::Engine(message)) => {
                    assert!(message.contains("upload"), "{message}");
                    assert!(!message.contains("SECRET"), "{message}");
                }
                other => panic!("expected an engine error, got {other:?}"),
            }
        }
        let routes: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.route().to_owned())
            .collect();
        assert_eq!(
            routes,
            ["/v1/audio/speech", "/v1/audio/voices", "/v1/audio/voices"],
            "the cached clip is reused on the retry"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn a_server_that_forgot_the_anchor_is_given_it_again() {
        let dir = scratch("forgot");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let s = synth(&server.endpoint(), &dir);
        s.synthesize("あ", &settings("x"), deadline()).unwrap();
        server.forget_voices();
        assert!(matches!(
            s.synthesize("い", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        s.synthesize("う", &settings("x"), deadline()).unwrap();
        assert_eq!(server.voices(), [s.anchor_id().to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_failures_are_typed_and_never_carry_the_text_or_url() {
        let secret = "ひみつの本文SECRET";
        for behaviour in [
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
            let dir = scratch("typed");
            let server = FakeIrodoriServer::start(behaviour);
            match synth(&server.endpoint(), &dir).synthesize(secret, &settings(secret), deadline())
            {
                Err(SynthError::Engine(message)) => {
                    assert!(!message.contains("SECRET"), "{message}");
                    assert!(!message.contains("127.0.0.1"), "{message}");
                }
                other => panic!("expected an engine error, got {other:?}"),
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_malformed_wav_is_refused_even_when_the_status_is_ok() {
        let good = wav::encode_pcm16(&[0.0, 0.5, -0.5], 48_000).unwrap();
        let mut stereo = good.clone();
        stereo[22] = 2;
        let truncated = good[..good.len() - 2].to_vec();
        for wav_body in [stereo, truncated] {
            let dir = scratch("malformed");
            let server = FakeIrodoriServer::start(FakeBehaviour {
                wav_body: Some(wav_body),
                ..FakeBehaviour::default()
            });
            assert!(matches!(
                synth(&server.endpoint(), &dir).synthesize("あ", &settings("x"), deadline()),
                Err(SynthError::Engine(_))
            ));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn an_unreachable_server_is_an_engine_error_without_the_url() {
        let dir = scratch("dead");
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            endpoint
        };
        match synth(&dead, &dir).synthesize("SECRETtext", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => {
                assert!(message.contains("unreachable"), "{message}");
                assert!(!message.contains("SECRET") && !message.contains("127.0.0.1"));
            }
            other => panic!("expected unreachable, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_server_hits_the_deadline_and_a_spent_deadline_makes_no_call() {
        let dir = scratch("slow");
        let server = FakeIrodoriServer::start(FakeBehaviour {
            delay: Duration::from_millis(700),
            ..FakeBehaviour::default()
        });
        let synth = synth(&server.endpoint(), &dir);
        let started = Instant::now();
        let slow = synth.synthesize(
            "遅い",
            &settings("x"),
            Instant::now() + Duration::from_millis(200),
        );
        assert_eq!(slow, Err(SynthError::DeadlineExceeded));
        // The anchor (made and uploaded under its own budget) survives the
        // reply that timed out, so the next reply finds it ready.
        assert!(synth.anchor_path().is_file());
        assert_eq!(server.voices().len(), 1);
        assert!(started.elapsed() < Duration::from_millis(4_000));

        let before = server.requests().len();
        let spent = synth.synthesize("今", &settings("x"), Instant::now());
        assert_eq!(spent, Err(SynthError::DeadlineExceeded));
        assert_eq!(
            server.requests().len(),
            before,
            "no request after the deadline"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversize_text_and_oversize_answers_are_refused() {
        let dir = scratch("oversize");
        let server = FakeIrodoriServer::start(FakeBehaviour::default());
        let mut c = config(&server.endpoint(), &dir);
        c.max_text_chars = 5;
        let small_text = IrodoriSynthesizer::new(&c).unwrap();
        assert!(matches!(
            small_text.synthesize("あいうえおか", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert!(server.requests().is_empty(), "refused before any call");

        let mut c = config(&server.endpoint(), &dir);
        c.max_response_bytes = 1024;
        let small_wav = IrodoriSynthesizer::new(&c).unwrap();
        match small_wav.synthesize("あいうえおかきくけこ", &settings("x"), deadline()) {
            Err(SynthError::Engine(message)) => assert!(message.contains("limit"), "{message}"),
            other => panic!("expected the byte limit, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let dir = scratch("redirect");
        let server = FakeIrodoriServer::start(FakeBehaviour {
            status: 307,
            ..FakeBehaviour::default()
        });
        assert!(matches!(
            synth(&server.endpoint(), &dir).synthesize("あ", &settings("x"), deadline()),
            Err(SynthError::Engine(_))
        ));
        assert_eq!(server.requests().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
