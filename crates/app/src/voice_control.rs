//! Live voice settings for the `local` profile (issue #360).
//!
//! The operator picks the voice from the operation panel (or `aivtuberctl`):
//! the local voice, or Aivis Cloud with a model/speaker/style, tuning and a
//! credit line. This module owns the one piece of runtime state that follows,
//! next to [`crate::model_switch`]:
//!
//! * **One TTS wire contract, one failure path.** The app's normalized-HTTP TTS
//!   adapter keeps posting the same request; only its *target* changes
//!   ([`NormalizedHttpTtsAdapter::select_target`], shared by every clone). For
//!   the local voice the target is the supervised sidecar's endpoint, exactly as
//!   configured. For Aivis it is an in-process loopback bridge (the
//!   `local-tts-sidecar` server, the same one the `AIVTUBER_TTS_BACKEND=aivis-cloud`
//!   path uses) whose synthesizer is a [`SwapSynthesizer`], so a new model,
//!   style or tuning is a swap of the engine behind a running bridge. Failures
//!   (a bad key, no credit, no network) are the bridge's typed errors, which the
//!   adapter already maps to the generation fallback: the performer never
//!   freezes. The audio cache is keyed by the engine's identity, which holds the
//!   model, speaker, style and tuning, so audio made with one voice is never
//!   replayed for another.
//! * **Authority.** The only entry points are the `ProductionApp` methods that
//!   demand `voice.configure`, minted by `LocalControlIngress` (the same path as
//!   stop and mute, audited the same way). The bridge is started only on
//!   demand and dropped when the operator goes back to the local voice, which
//!   stops all egress.
//! * **The key.** It lives in its own file beside the settings (owner-only
//!   where the OS allows, atomic write), or in `AIVIS_API`, which wins. It is a
//!   `SecretString` from the moment it arrives: never in a response, a status,
//!   a log line or an audit record (a test drives every output with a sentinel).
//! * **No blocking.** Everything here is small file I/O and in-process
//!   threads. The key check runs on its own thread and the composition loop
//!   only polls for its result; the model search never reaches this thread at
//!   all (the panel's HTTP layer proxies it).
//!
//! The preview is a one-shot override of the Thinking reply: the operator's
//! preview request arms [`PreviewSlot`] with the id of a panel chat line, and
//! [`PreviewThinking`] answers exactly that request with the fixed sentence
//! instead of calling the model. The line then takes the ordinary generation,
//! gate, TTS, scheduling and audio path, so it is audited, and mute and stop
//! apply to it like to any utterance.

use aivtuber_adapters::speech_tts::aivis_cloud::{
    self, AivisCloudConfig, AivisCloudSynthesizer, DEFAULT_ENDPOINT,
};
use aivtuber_adapters::speech_tts::aivis_models::{self, KeyCheck};
use aivtuber_adapters::speech_tts::server::ServerHandle;
use aivtuber_adapters::speech_tts::{SwapSynthesizer, Synthesizer};
use aivtuber_adapters::{NormalizedHttpTtsAdapter, SecretString, TtsTarget};
use aivtuber_domain::{
    BackendIdentity, EngineError, EngineFuture, GeneratedReply, ReplyDeltaSink, ThinkingEngine,
    ThinkingRequest, VOICE_PREVIEW_TEXT, VoiceBackend, VoicePatch, VoiceSettings, VoiceTuning,
    parse_voice_settings, render_voice_settings,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VOICE_SETTINGS_VAR: &str = "AIVTUBER_VOICE_SETTINGS";
pub const AIVIS_KEY_FILE_VAR: &str = "AIVTUBER_AIVIS_KEY_FILE";
/// A key file larger than this is ignored, not read.
pub const MAX_KEY_FILE_BYTES: u64 = 512;
/// Longest accepted API key, in characters.
pub const MAX_KEY_CHARS: usize = 256;
const KEY_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

/// Where the voice choice and the key are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePaths {
    pub settings: PathBuf,
    pub key_file: PathBuf,
}

/// Everything the controller needs that is neither the choice nor the key.
#[derive(Debug, Clone)]
pub struct VoiceEnvironment {
    pub paths: VoicePaths,
    /// `AIVIS_API`, when set: it wins over the key file.
    pub env_key: Option<SecretString>,
    /// Aivis Cloud origin (`AIVTUBER_AIVIS_ENDPOINT`, `https://` unless loopback).
    pub endpoint: String,
    /// Budget for one synthesis.
    pub synth_timeout: Duration,
    pub amplitude_visemes: bool,
    pub audio_root: PathBuf,
    pub pack_root: PathBuf,
}

impl VoiceEnvironment {
    pub fn default_endpoint() -> String {
        DEFAULT_ENDPOINT.to_owned()
    }
}

// ---- files ----

/// Replaces `path` with `text` (temp file in the same directory, then rename,
/// so a crash leaves the old or the new file, never half of one).
pub(crate) fn write_atomic(path: &Path, text: &str, owner_only: bool) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let result = (|| {
        create_private(&temp, owner_only)?;
        std::fs::write(&temp, text)?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Creates (truncates) `path`; with `owner_only` it is restricted to the owner
/// *before* any content is written, so the secret never sits in a file others
/// can read.
fn create_private(path: &Path, owner_only: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        if owner_only {
            options.mode(0o600);
        }
        options.open(path).map(drop)
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(path)?;
        if owner_only {
            restrict_to_owner(path);
        }
        Ok(())
    }
}

/// Windows: drop the inherited permissions and grant the owner alone (the
/// well-known `OWNER RIGHTS` identity), through the system `icacls`, by absolute
/// path (never a PATH lookup). Best effort: a failure is reported, not fatal.
#[cfg(not(unix))]
fn restrict_to_owner(path: &Path) {
    if !cfg!(windows) {
        return;
    }
    let root = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
    let status = std::process::Command::new(root.join("System32").join("icacls.exe"))
        .arg(path)
        .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !status.is_ok_and(|s| s.success()) {
        eprintln!("voice: could not restrict the API key file to its owner; check its permissions");
    }
}

pub(crate) fn write_voice_settings(path: &Path, settings: &VoiceSettings) -> std::io::Result<()> {
    let text = render_voice_settings(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    write_atomic(path, &text, false)
}

/// The saved settings, or `None` when the file is missing, too large or not
/// exactly the documented shape.
pub fn read_voice_settings(path: &Path) -> Option<VoiceSettings> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(aivtuber_domain::MAX_VOICE_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    parse_voice_settings(&bytes)
}

/// A key: 1..=[`MAX_KEY_CHARS`] characters, no whitespace or control characters.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().count() <= MAX_KEY_CHARS
        && !key.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// The key stored in `path`, if the file holds a valid one.
pub fn read_key_file(path: &Path) -> Option<SecretString> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let key = text.trim();
    is_valid_key(key).then(|| SecretString::new(key))
}

pub(crate) fn write_key_file(path: &Path, key: &str) -> std::io::Result<()> {
    write_atomic(path, &format!("{key}\n"), true)
}

// ---- preview ----

/// Which panel chat line, if any, the preview has armed.
#[derive(Debug, Clone, Default)]
pub struct PreviewSlot(Arc<Mutex<Option<String>>>);

impl PreviewSlot {
    fn arm(&self, event_id: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(event_id.to_owned());
    }

    fn disarm(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// True exactly once, for the armed event id.
    fn take_if(&self, event_id: Option<&str>) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match (slot.as_deref(), event_id) {
            (Some(armed), Some(id)) if armed == id => {
                *slot = None;
                true
            }
            _ => false,
        }
    }
}

/// Answers the armed preview request with the fixed sentence and everything
/// else with the wrapped engine. The decision is data the operator armed: the
/// request's text, a viewer's chat or a model never selects it.
pub struct PreviewThinking {
    inner: Arc<dyn ThinkingEngine>,
    slot: PreviewSlot,
}

impl PreviewThinking {
    pub fn new(inner: Arc<dyn ThinkingEngine>, slot: PreviewSlot) -> Self {
        Self { inner, slot }
    }

    fn reply() -> GeneratedReply {
        GeneratedReply {
            text: VOICE_PREVIEW_TEXT.to_owned(),
        }
    }
}

impl ThinkingEngine for PreviewThinking {
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        if self.slot.take_if(request.input.event_id.as_deref()) {
            return Box::pin(async { Ok(Self::reply()) });
        }
        self.inner.generate(request)
    }

    fn streams(&self) -> bool {
        self.inner.streams()
    }

    fn generate_streaming(
        &self,
        request: &ThinkingRequest,
        timeout: Duration,
        sink: &mut dyn ReplyDeltaSink,
    ) -> Option<Result<GeneratedReply, EngineError>> {
        if self.slot.take_if(request.input.event_id.as_deref()) {
            let _ = sink.push(VOICE_PREVIEW_TEXT);
            return Some(Ok(Self::reply()));
        }
        self.inner.generate_streaming(request, timeout, sink)
    }

    fn generate_with_timeout<'a>(
        &'a self,
        request: &'a ThinkingRequest,
        timeout: Duration,
    ) -> EngineFuture<'a, GeneratedReply> {
        if self.slot.take_if(request.input.event_id.as_deref()) {
            return Box::pin(async { Ok(Self::reply()) });
        }
        self.inner.generate_with_timeout(request, timeout)
    }

    fn identity(&self) -> BackendIdentity {
        self.inner.identity()
    }
}

// ---- status ----

/// What the operator sees (panel and `aivtuberctl status`). No key, ever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceStatus {
    /// The voice that is speaking now: `local` or `aivis`.
    pub backend: String,
    pub model_uuid: Option<String>,
    pub model_name: Option<String>,
    pub speaker_uuid: Option<String>,
    pub style_id: Option<u8>,
    pub tuning: VoiceTuning,
    pub credit: Option<String>,
    pub key: VoiceKeyStatus,
    /// The most recent Aivis failure (a status code or an I/O kind), until the
    /// next success or change.
    pub last_error: Option<String>,
    /// Why the last change could not be saved, if it could not.
    pub save_error: Option<String>,
    pub preview_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceKeyStatus {
    pub set: bool,
    /// `environment` (AIVIS_API, wins) or `file`.
    pub source: Option<String>,
    /// `unchecked`, `checking`, `valid`, `rejected`, `network_error` or `unexpected`.
    pub check: String,
}

enum CheckState {
    Unchecked,
    Checking(Receiver<KeyCheck>),
    Done(KeyCheck),
}

/// The running in-process bridge to Aivis Cloud. Dropping it stops the listener.
struct Bridge {
    _handle: ServerHandle,
    endpoint: String,
    synth: SwapSynthesizer,
}

pub struct VoiceController {
    env: VoiceEnvironment,
    adapter: NormalizedHttpTtsAdapter,
    local_target: TtsTarget,
    /// Opens the supervised-sidecar liveness gate while Aivis is speaking.
    bypass: Arc<AtomicBool>,
    preview: PreviewSlot,
    settings: VoiceSettings,
    file_key: Option<SecretString>,
    check: CheckState,
    bridge: Option<Bridge>,
    save_error: Option<String>,
    preview_seq: u64,
    pending_preview: Option<String>,
}

impl fmt::Debug for VoiceController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceController")
            .field("backend", &self.settings.backend)
            .finish_non_exhaustive()
    }
}

fn aivis_target(bridge_endpoint: &str, settings: &VoiceSettings) -> TtsTarget {
    TtsTarget {
        endpoint: bridge_endpoint.to_owned(),
        backend_name: aivis_cloud::BACKEND_NAME.to_owned(),
        model_alias: settings
            .model_uuid
            .as_deref()
            .map(|model| aivis_cloud::model_alias(model, settings.style_id)),
        model_version: Some(settings.tuning.identity()),
    }
}

impl VoiceController {
    /// `adapter` is a clone of the one the pipeline owns; its current target is
    /// the local voice. `initial` is what the settings file said (or the
    /// default); an Aivis choice that cannot start (no key) begins on the local
    /// voice and says so.
    pub fn new(
        env: VoiceEnvironment,
        adapter: NormalizedHttpTtsAdapter,
        bypass: Arc<AtomicBool>,
        preview: PreviewSlot,
        initial: VoiceSettings,
    ) -> Self {
        let file_key = read_key_file(&env.paths.key_file);
        let local_target = adapter.target();
        let mut controller = Self {
            env,
            adapter,
            local_target,
            bypass,
            preview,
            settings: VoiceSettings {
                backend: VoiceBackend::Local,
                ..initial.clone()
            },
            file_key,
            check: CheckState::Unchecked,
            bridge: None,
            save_error: None,
            preview_seq: 0,
            pending_preview: None,
        };
        if initial.backend == VoiceBackend::Aivis {
            match controller.activate(&initial) {
                Ok(()) => {
                    controller.settings = initial;
                    eprintln!(
                        "voice: aivis-cloud active ({}); reply text is sent to Aivis Cloud",
                        controller.settings.audit_summary()
                    );
                }
                Err(reason) => eprintln!(
                    "voice: the saved Aivis Cloud voice is not started ({reason}); using the local voice"
                ),
            }
        }
        controller
    }

    pub fn endpoint(&self) -> &str {
        &self.env.endpoint
    }

    fn effective_key(&self) -> Option<(SecretString, &'static str)> {
        match (&self.env.env_key, &self.file_key) {
            (Some(key), _) => Some((key.clone(), "environment")),
            (None, Some(key)) => Some((key.clone(), "file")),
            (None, None) => None,
        }
    }

    /// Point the TTS path at Aivis Cloud with `settings` (a model and the
    /// credit line were checked by the caller). Starts the bridge on first use
    /// and swaps the engine behind it afterwards. Nothing changes on failure.
    fn activate(&mut self, settings: &VoiceSettings) -> Result<(), String> {
        let (key, _) = self
            .effective_key()
            .ok_or_else(|| "set the Aivis API key first".to_owned())?;
        let model = settings
            .model_uuid
            .clone()
            .ok_or_else(|| "choose a model first".to_owned())?;
        let mut config = AivisCloudConfig::new(self.env.endpoint.clone(), key, model);
        config.speaker_uuid = settings.speaker_uuid.clone();
        config.style_id = settings.style_id;
        config.tuning = settings.tuning;
        let synth: Arc<dyn Synthesizer> =
            Arc::new(AivisCloudSynthesizer::new(&config).map_err(|error| error.to_string())?);
        let endpoint = match &self.bridge {
            Some(bridge) => {
                bridge.synth.swap(synth);
                bridge.endpoint.clone()
            }
            None => {
                let swap = SwapSynthesizer::new(synth);
                let (handle, endpoint) = crate::voicevox_bridge::start_loopback_bridge(
                    swap.clone(),
                    self.env.synth_timeout,
                    self.env.amplitude_visemes,
                    &self.env.audio_root,
                    &self.env.pack_root,
                )?;
                self.bridge = Some(Bridge {
                    _handle: handle,
                    endpoint: endpoint.clone(),
                    synth: swap,
                });
                endpoint
            }
        };
        self.adapter
            .select_target(aivis_target(&endpoint, settings))
            .map_err(|error| error.to_string())?;
        self.bypass.store(true, Ordering::Release);
        Ok(())
    }

    /// Back to the local voice: the next request goes to the sidecar again and
    /// the bridge (and with it all egress) is gone.
    fn deactivate(&mut self) {
        let _ = self.adapter.select_target(self.local_target.clone());
        self.bypass.store(false, Ordering::Release);
        self.bridge = None;
    }

    fn save(&mut self) {
        self.save_error = write_voice_settings(&self.env.paths.settings, &self.settings)
            .err()
            .map(|error| format!("could not save the voice settings ({:?})", error.kind()));
        if let Some(message) = &self.save_error {
            eprintln!("voice: {message}");
        }
    }

    /// Apply a patch (the JSON text of a [`VoicePatch`]). Returns a short
    /// outcome for the operator and the audit record: no key and no free text.
    pub fn apply(&mut self, patch_json: &str) -> Result<String, String> {
        let patch = VoicePatch::parse(patch_json).map_err(|e| format!("refused: {e}"))?;
        let next = self
            .settings
            .apply(&patch)
            .map_err(|e| format!("refused: {e}"))?;
        match next.backend {
            VoiceBackend::Aivis => self
                .activate(&next)
                .map_err(|reason| format!("refused: {reason}"))?,
            VoiceBackend::Local => {
                if self.settings.backend == VoiceBackend::Aivis {
                    self.deactivate();
                }
            }
        }
        let detail = format!("applied: {}", next.audit_summary());
        eprintln!("voice: {detail}");
        self.settings = next;
        self.save();
        Ok(detail)
    }

    /// Store the key in its own file. `AIVIS_API`, when set, still wins; the
    /// stored key is used once that variable is gone.
    pub fn set_key(&mut self, key: &str) -> Result<String, String> {
        let key = key.trim();
        if !is_valid_key(key) {
            return Err("refused: the key is empty, too long or has whitespace".to_owned());
        }
        write_key_file(&self.env.paths.key_file, key)
            .map_err(|error| format!("refused: cannot store the key ({:?})", error.kind()))?;
        self.file_key = Some(SecretString::new(key));
        // The key in use may have changed (the environment still wins): a new
        // check is what tells the operator whether it works.
        if self.env.env_key.is_none() {
            self.rebuild_active_engine();
            let _ = self.check_key();
            Ok("key set; checking it".to_owned())
        } else {
            self.check = CheckState::Unchecked;
            Ok(
                "key set (the AIVIS_API environment variable is in use and takes precedence)"
                    .to_owned(),
            )
        }
    }

    /// The active Aivis engine holds the old key; build it again with the new.
    fn rebuild_active_engine(&mut self) {
        if self.settings.backend == VoiceBackend::Aivis {
            let current = self.settings.clone();
            if let Err(reason) = self.activate(&current) {
                eprintln!("voice: could not apply the new key to the running voice ({reason})");
            }
        }
    }

    /// Forget the stored key. Without a key (and no `AIVIS_API`) Aivis cannot
    /// speak, so it goes back to the local voice.
    pub fn remove_key(&mut self) -> Result<String, String> {
        match std::fs::remove_file(&self.env.paths.key_file) {
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
        self.check = CheckState::Unchecked;
        if self.env.env_key.is_some() {
            return Ok("key removed (the AIVIS_API environment variable is still set)".to_owned());
        }
        if self.settings.backend == VoiceBackend::Aivis {
            self.deactivate();
            self.settings.backend = VoiceBackend::Local;
            self.save();
            return Ok("key removed; back on the local voice".to_owned());
        }
        Ok("key removed".to_owned())
    }

    /// Ask the service whether the key works, on a background thread.
    pub fn check_key(&mut self) -> Result<String, String> {
        self.poll();
        if matches!(self.check, CheckState::Checking(_)) {
            return Err("refused: a key check is already running".to_owned());
        }
        let (key, _) = self
            .effective_key()
            .ok_or_else(|| "refused: no key is set".to_owned())?;
        let endpoint = self.env.endpoint.clone();
        let (sender, receiver) = channel();
        let spawned = std::thread::Builder::new()
            .name("aivis-key-check".to_owned())
            .spawn(move || {
                let outcome = aivis_models::check_api_key(&endpoint, &key, KEY_CHECK_TIMEOUT)
                    .unwrap_or(KeyCheck::NetworkError);
                let _ = sender.send(outcome);
            });
        if spawned.is_err() {
            return Err("refused: cannot start the key check".to_owned());
        }
        self.check = CheckState::Checking(receiver);
        Ok("checking the key".to_owned())
    }

    /// Collect a finished key check. Never blocks.
    pub fn poll(&mut self) {
        let CheckState::Checking(receiver) = &self.check else {
            return;
        };
        let outcome = match receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => KeyCheck::NetworkError,
        };
        eprintln!("voice: key check {}", check_name(Some(outcome)));
        self.check = CheckState::Done(outcome);
    }

    /// Arm the preview for the next panel line and return its event id and
    /// sequence. `None` if one is already waiting to be submitted.
    pub fn request_preview(&mut self) -> Result<String, String> {
        if self.pending_preview.is_some() {
            return Err("refused: a preview is already waiting".to_owned());
        }
        self.preview_seq += 1;
        let event_id = format!("voice-preview-{}", self.preview_seq);
        self.pending_preview = Some(event_id);
        Ok("preview queued".to_owned())
    }

    /// The preview waiting to be submitted as a panel chat line: its event id
    /// and sequence. Armed here, so the Thinking override and the line it
    /// answers are decided in one place.
    pub fn take_pending_preview(&mut self) -> Option<(String, u64)> {
        let event_id = self.pending_preview.take()?;
        self.preview.arm(&event_id);
        Some((event_id, self.preview_seq))
    }

    /// The line was not admitted: nothing will consume the override.
    pub fn cancel_preview(&mut self) {
        self.preview.disarm();
    }

    pub fn status(&self) -> VoiceStatus {
        let key = self.effective_key();
        let check = match &self.check {
            CheckState::Unchecked => "unchecked",
            CheckState::Checking(_) => "checking",
            CheckState::Done(outcome) => check_name(Some(*outcome)),
        };
        VoiceStatus {
            backend: self.settings.backend.as_str().to_owned(),
            model_uuid: self.settings.model_uuid.clone(),
            model_name: self.settings.model_name.clone(),
            speaker_uuid: self.settings.speaker_uuid.clone(),
            style_id: self.settings.style_id,
            tuning: self.settings.tuning,
            credit: self.settings.credit.clone(),
            key: VoiceKeyStatus {
                set: key.is_some(),
                source: key.map(|(_, source)| source.to_owned()),
                check: check.to_owned(),
            },
            last_error: self
                .bridge
                .as_ref()
                .and_then(|bridge| bridge.synth.last_error()),
            save_error: self.save_error.clone(),
            preview_text: VOICE_PREVIEW_TEXT.to_owned(),
        }
    }

    /// Stop the bridge (all egress) at shutdown.
    pub fn shutdown(&mut self) {
        self.deactivate();
    }
}

fn check_name(check: Option<KeyCheck>) -> &'static str {
    match check {
        None => "unchecked",
        Some(KeyCheck::Valid) => "valid",
        Some(KeyCheck::Rejected) => "rejected",
        Some(KeyCheck::NetworkError) => "network_error",
        Some(KeyCheck::Unexpected) => "unexpected",
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use aivtuber_adapters::NormalizedHttpTtsConfig;
    use aivtuber_adapters::speech_tts::ToneSynthesizer;
    use aivtuber_adapters::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    use aivtuber_adapters::speech_tts::server::{DEFAULT_PATH, ServerConfig, TtsServer};
    use aivtuber_domain::SpeechRequest;

    const MODEL: &str = "a59cb814-0083-4369-8542-f51a29e72af7";
    const SPEAKER: &str = "e756b8e4-b606-4e15-99b1-3f9c6a1b2317";
    const KEY: &str = "sk-voice-SENTINEL-KEY-0123456789";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-360-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// A local sidecar double (the `local-tts-sidecar` server with a tone
    /// engine) and the adapter that posts to it: the local voice.
    struct LocalVoice {
        _server: ServerHandle,
        adapter: NormalizedHttpTtsAdapter,
    }

    fn local_voice(root: &Path) -> LocalVoice {
        std::fs::create_dir_all(root.join("local-audio")).unwrap();
        let server = TtsServer::bind(
            ServerConfig::new("127.0.0.1:0".parse().unwrap(), root.join("local-audio")),
            Arc::new(ToneSynthesizer::default()),
        )
        .expect("local sidecar double")
        .spawn();
        let adapter = NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: format!("http://{}{DEFAULT_PATH}", server.local_addr()),
                backend_name: "local-tts-sidecar".to_owned(),
                model_alias: Some("supertonic".to_owned()),
                model_version: None,
                voice_model: Some("example-voice-v1".to_owned()),
                viseme_mapping: Some("ja-5vowel-v1".to_owned()),
                timeout: Duration::from_secs(10),
            },
            None,
        )
        .expect("adapter");
        LocalVoice {
            _server: server,
            adapter,
        }
    }

    fn environment(root: &Path, endpoint: &str, env_key: Option<&str>) -> VoiceEnvironment {
        VoiceEnvironment {
            paths: VoicePaths {
                settings: root.join("local-voice.json"),
                key_file: root.join("local-aivis-key.txt"),
            },
            env_key: env_key.map(SecretString::new),
            endpoint: endpoint.to_owned(),
            synth_timeout: Duration::from_secs(5),
            amplitude_visemes: false,
            audio_root: root.join("audio"),
            pack_root: root.to_path_buf(),
        }
    }

    fn controller(
        root: &Path,
        local: &LocalVoice,
        endpoint: &str,
        key: Option<&str>,
    ) -> VoiceController {
        VoiceController::new(
            environment(root, endpoint, key),
            local.adapter.clone(),
            Arc::new(AtomicBool::new(false)),
            PreviewSlot::default(),
            VoiceSettings::default(),
        )
    }

    fn aivis_patch() -> String {
        format!(
            r#"{{"backend":"aivis","model_uuid":"{MODEL}","model_name":"まお","speaker_uuid":"{SPEAKER}","style_id":2,"credit":"AivisSpeech: まお","confirm_egress":true}}"#
        )
    }

    fn speak(adapter: &NormalizedHttpTtsAdapter, text: &str) -> Result<String, EngineError> {
        adapter
            .synthesize_sync(&SpeechRequest {
                text: text.to_owned(),
                style: None,
            })
            .map(|artifact| artifact.audio_ref)
    }

    fn wait_for_check(c: &mut VoiceController, want: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while c.status().key.check != want {
            assert!(std::time::Instant::now() < deadline, "{:?}", c.status());
            c.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_key_file_is_bounded_validated_and_atomic() {
        let root = scratch("keyfile");
        let path = root.join("k.txt");
        assert!(read_key_file(&path).is_none(), "missing");
        write_key_file(&path, KEY).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{KEY}\n"));
        assert!(!root.join("k.txt.tmp").exists());
        assert_eq!(read_key_file(&path), Some(SecretString::new(KEY)));
        // Replaced, not appended.
        write_key_file(&path, "second-key").unwrap();
        assert_eq!(read_key_file(&path), Some(SecretString::new("second-key")));
        for bad in [
            "".to_owned(),
            "two words".to_owned(),
            "ctrl\u{0007}char".to_owned(),
            "k".repeat(MAX_KEY_CHARS + 1),
            "k".repeat(MAX_KEY_FILE_BYTES as usize + 10),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(read_key_file(&path).is_none(), "{} bytes", bad.len());
        }
        assert!(is_valid_key("sk-abc") && !is_valid_key("a b") && !is_valid_key(""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            write_key_file(&path, KEY).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "owner only");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_settings_file_round_trips_and_a_bad_one_is_ignored() {
        let root = scratch("settings");
        let path = root.join("voice.json");
        assert!(read_voice_settings(&path).is_none(), "missing");
        let on = VoiceSettings::default()
            .apply(&VoicePatch::parse(&aivis_patch()).unwrap())
            .unwrap();
        write_voice_settings(&path, &on).unwrap();
        assert_eq!(read_voice_settings(&path), Some(on));
        assert!(!root.join("voice.json.tmp").exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("key"), "{text}");
        for bad in [
            "not json".to_owned(),
            r#"{"schema":"local-voice-settings/1","settings":{"backend":"aivis"}}"#.to_owned(),
            "x".repeat(10_000),
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(read_voice_settings(&path).is_none());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_voice_switches_live_between_local_and_aivis_and_stops_egress() {
        let root = scratch("switch");
        let local = local_voice(&root);
        let local_target = local.adapter.target();
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = controller(&root, &local, &api.endpoint(), Some(KEY));
        assert_eq!(c.status().backend, "local");
        assert!(speak(&local.adapter, "ローカル").unwrap().ends_with(".wav"));
        assert!(api.requests().is_empty(), "nothing left the machine");

        let detail = c.apply(&aivis_patch()).unwrap();
        assert!(detail.contains(MODEL) && !detail.contains(KEY), "{detail}");
        assert_eq!(c.status().backend, "aivis");
        assert_eq!(local.adapter.target().backend_name, "aivis-cloud");
        assert!(
            local
                .adapter
                .target()
                .endpoint
                .starts_with("http://127.0.0.1:")
        );
        speak(&local.adapter, "クラウド").unwrap();
        let sent = &api.requests()[0];
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["model_uuid"], MODEL);
        assert_eq!(body["speaker_uuid"], SPEAKER);
        assert_eq!(body["style_id"], 2);
        assert_eq!(body["speaking_rate"], 1.0);
        assert_eq!(
            sent.header("authorization").as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert_eq!(api.spoken_texts(), ["クラウド"]);
        // The provenance names the Aivis voice and its tuning.
        let target = local.adapter.target();
        assert_eq!(
            target.model_alias.as_deref(),
            Some(format!("{MODEL}/style-2").as_str())
        );
        assert!(target.model_version.unwrap().contains("rate=1"));
        let bridge_endpoint = local.adapter.target().endpoint;

        // A style and a tuning change apply to the next utterance; the same
        // text is not replayed from the other voice's cache.
        c.apply(r#"{"style_id":5,"speaking_rate":1.4,"emotional_intensity":0.5}"#)
            .unwrap();
        speak(&local.adapter, "クラウド").unwrap();
        assert_eq!(
            api.spoken_texts().len(),
            2,
            "synthesized again, not replayed"
        );
        let body: serde_json::Value = serde_json::from_slice(&api.requests()[1].body).unwrap();
        assert_eq!(body["style_id"], 5);
        // An f32 on the wire: compare with the tolerance of one.
        assert!((body["speaking_rate"].as_f64().unwrap() - 1.4).abs() < 1e-6);
        assert_eq!(body["emotional_intensity"], 0.5);
        assert_eq!(
            local.adapter.target().endpoint,
            bridge_endpoint,
            "the bridge stays up while Aivis is on"
        );
        // The same voice again is a cache hit: no second request.
        speak(&local.adapter, "クラウド").unwrap();
        assert_eq!(api.spoken_texts().len(), 2);

        // Back to local: the sidecar answers, nothing more is sent, and the
        // bridge (the only egress path) is gone.
        c.apply(r#"{"backend":"local"}"#).unwrap();
        assert_eq!(c.status().backend, "local");
        assert_eq!(local.adapter.target(), local_target);
        let before = api.requests().len();
        assert!(speak(&local.adapter, "また").is_ok());
        assert_eq!(api.requests().len(), before);
        assert!(
            std::net::TcpStream::connect(
                bridge_endpoint
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap()
            )
            .is_err(),
            "the bridge listener is gone"
        );
        // It was remembered, and the choice survives for the next enable.
        let saved = read_voice_settings(&root.join("local-voice.json")).unwrap();
        assert_eq!(saved.backend, VoiceBackend::Local);
        assert_eq!(saved.model_uuid.as_deref(), Some(MODEL));
        assert_eq!(saved.style_id, Some(5));
        c.apply(r#"{"backend":"aivis","confirm_egress":true}"#)
            .unwrap();
        assert_eq!(c.status().backend, "aivis");
        c.shutdown();
        assert_eq!(local.adapter.target(), local_target);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enabling_needs_the_key_the_credit_the_model_and_the_confirmation() {
        let root = scratch("refusals");
        let local = local_voice(&root);
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = controller(&root, &local, &api.endpoint(), None);
        let local_target = local.adapter.target();
        // No key: refused, and nothing changed.
        let error = c.apply(&aivis_patch()).unwrap_err();
        assert!(error.contains("API key"), "{error}");
        assert_eq!(c.status().backend, "local");
        assert_eq!(local.adapter.target(), local_target);
        assert!(
            read_voice_settings(&root.join("local-voice.json")).is_none(),
            "not saved"
        );
        c.set_key(KEY).unwrap();
        for (patch, needle) in [
            (
                r#"{"backend":"aivis","confirm_egress":true}"#.to_owned(),
                "choose a model",
            ),
            (
                format!(r#"{{"backend":"aivis","model_uuid":"{MODEL}","confirm_egress":true}}"#),
                "credit line",
            ),
            (
                aivis_patch().replace(r#","confirm_egress":true"#, ""),
                "confirm",
            ),
            (r#"{"style_id":99}"#.to_owned(), "style"),
            (r#"{"pitch":3}"#.to_owned(), "out of range"),
            ("nonsense".to_owned(), "not a voice settings"),
        ] {
            let error = c.apply(&patch).unwrap_err();
            assert!(error.contains(needle), "{patch}: {error}");
            assert_eq!(c.status().backend, "local");
            assert_eq!(local.adapter.target(), local_target);
        }
        assert!(
            api.requests()
                .iter()
                .all(|r| r.route() != "/v1/tts/synthesize")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_key_is_stored_checked_and_removed_and_the_environment_wins() {
        let root = scratch("key");
        let local = local_voice(&root);
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            accept_key: Some(KEY.to_owned()),
            ..FakeAivisBehaviour::default()
        });
        let mut c = controller(&root, &local, &api.endpoint(), None);
        let status = c.status();
        assert!(!status.key.set && status.key.source.is_none() && status.key.check == "unchecked");
        assert!(c.check_key().is_err(), "no key to check");
        assert!(c.set_key("has space").is_err());
        assert!(c.set_key("").is_err());
        c.set_key(KEY).unwrap();
        assert!(root.join("local-aivis-key.txt").is_file());
        wait_for_check(&mut c, "valid");
        let status = c.status();
        assert_eq!(status.key.source.as_deref(), Some("file"));
        // A wrong key is reported as rejected, not as a crash.
        c.set_key("a-wrong-key").unwrap();
        wait_for_check(&mut c, "rejected");
        // The stored key survives a restart.
        let again = controller(&root, &local, &api.endpoint(), None);
        assert!(again.status().key.set);
        // Removing it deletes the file.
        c.remove_key().unwrap();
        assert!(!root.join("local-aivis-key.txt").exists());
        assert!(!c.status().key.set);
        c.remove_key().unwrap();
        // The environment key wins and is reported as such.
        let mut e = controller(&root, &local, &api.endpoint(), Some(KEY));
        e.set_key("file-key").unwrap();
        let status = e.status();
        assert_eq!(status.key.source.as_deref(), Some("environment"));
        let (key, source) = e.effective_key().unwrap();
        assert_eq!((key, source), (SecretString::new(KEY), "environment"));
        e.remove_key().unwrap();
        assert!(e.status().key.set, "the environment key stays");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_the_key_while_aivis_speaks_returns_to_the_local_voice() {
        let root = scratch("key-removed");
        let local = local_voice(&root);
        let local_target = local.adapter.target();
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = controller(&root, &local, &api.endpoint(), None);
        c.set_key(KEY).unwrap();
        c.apply(&aivis_patch()).unwrap();
        assert_eq!(c.status().backend, "aivis");
        let detail = c.remove_key().unwrap();
        assert!(detail.contains("local voice"), "{detail}");
        assert_eq!(c.status().backend, "local");
        assert_eq!(local.adapter.target(), local_target);
        assert_eq!(
            read_voice_settings(&root.join("local-voice.json"))
                .unwrap()
                .backend,
            VoiceBackend::Local
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_key_reaches_the_running_voice() {
        let root = scratch("rekey");
        let local = local_voice(&root);
        let api = FakeAivisCloud::start(FakeAivisBehaviour {
            accept_key: Some(KEY.to_owned()),
            ..FakeAivisBehaviour::default()
        });
        let mut c = controller(&root, &local, &api.endpoint(), None);
        c.set_key("old-wrong-key").unwrap();
        c.apply(&aivis_patch()).unwrap();
        assert!(
            speak(&local.adapter, "あ").is_err(),
            "the service rejects it"
        );
        let reported = c.status().last_error.expect("reported");
        assert!(
            reported.contains("HTTP 401") && !reported.contains("wrong"),
            "{reported}"
        );
        c.set_key(KEY).unwrap();
        assert!(c.status().last_error.is_none(), "forgotten with the swap");
        assert!(speak(&local.adapter, "あ").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_saved_aivis_choice_starts_only_when_it_can() {
        let root = scratch("restart");
        let local = local_voice(&root);
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let on = VoiceSettings::default()
            .apply(&VoicePatch::parse(&aivis_patch()).unwrap())
            .unwrap();
        let start = |key: Option<&str>| {
            VoiceController::new(
                environment(&root, &api.endpoint(), key),
                local.adapter.clone(),
                Arc::new(AtomicBool::new(false)),
                PreviewSlot::default(),
                on.clone(),
            )
        };
        let local_target = local.adapter.target();
        // No key: the local voice, the choice kept in memory for later.
        let c = start(None);
        assert_eq!(c.status().backend, "local");
        assert_eq!(c.status().model_uuid.as_deref(), Some(MODEL));
        assert_eq!(local.adapter.target(), local_target);
        // A key: Aivis comes up as it was left.
        let c = start(Some(KEY));
        assert_eq!(c.status().backend, "aivis");
        assert_eq!(c.status().style_id, Some(2));
        assert_eq!(c.status().credit.as_deref(), Some("AivisSpeech: まお"));
        assert_eq!(local.adapter.target().backend_name, "aivis-cloud");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_sentinel_key_appears_in_no_status_debug_or_saved_settings() {
        let root = scratch("sentinel");
        let local = local_voice(&root);
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let mut c = controller(&root, &local, &api.endpoint(), None);
        let mut outputs = vec![c.set_key(KEY).unwrap()];
        outputs.push(c.apply(&aivis_patch()).unwrap());
        outputs.push(c.apply(r#"{"style_id":4}"#).unwrap());
        outputs.push(format!("{c:?}"));
        outputs.push(format!("{:?}", c.status()));
        outputs.push(serde_json::to_string(&c.status()).unwrap());
        outputs.push(format!("{:?}", c.env));
        outputs.push(format!("{:?}", local.adapter));
        // A failing synthesis (the bridge's typed error) reports no key either.
        let failing = FakeAivisCloud::start(FakeAivisBehaviour {
            status: 401,
            body: Some(format!("{{\"detail\":\"echo {KEY}\"}}").into_bytes()),
            ..FakeAivisBehaviour::default()
        });
        let mut d = controller(&root, &local, &failing.endpoint(), Some(KEY));
        d.apply(&aivis_patch()).unwrap();
        let error = speak(&local.adapter, "あ").unwrap_err();
        outputs.push(error.to_string());
        outputs.push(format!("{:?}", d.status()));
        outputs.push(d.status().last_error.unwrap_or_default());
        outputs.push(std::fs::read_to_string(root.join("local-voice.json")).unwrap());
        for (index, output) in outputs.iter().enumerate() {
            assert!(!output.contains("SENTINEL"), "output {index}: {output}");
        }
        // The one place it lives is the key file.
        assert!(
            std::fs::read_to_string(root.join("local-aivis-key.txt"))
                .unwrap()
                .contains("SENTINEL")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_preview_is_armed_once_for_one_event_and_a_viewer_cannot_use_it() {
        let slot = PreviewSlot::default();
        assert!(!slot.take_if(Some("x")));
        slot.arm("voice-preview-1");
        assert!(!slot.take_if(None));
        assert!(!slot.take_if(Some("viewer-chat-1")), "another event");
        assert!(slot.take_if(Some("voice-preview-1")));
        assert!(!slot.take_if(Some("voice-preview-1")), "one shot");
        slot.arm("voice-preview-2");
        slot.disarm();
        assert!(!slot.take_if(Some("voice-preview-2")));
    }
}
