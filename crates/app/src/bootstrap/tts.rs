use super::env::*;
use aivtuber_adapters::speech_tts::pins::VOICE_LICENCE_NOTICE;
use aivtuber_adapters::speech_tts::supervisor::TtsSidecar;
use aivtuber_app::{
    AivisBridge, AivisSettings, CompositionProfile, IrodoriBridge, IrodoriSettings,
    TtsSidecarSettings, VoicevoxBridge, VoicevoxSettings,
};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The supervised local TTS sidecar and what the app knows of its health.
pub(crate) struct TtsSidecarHandle {
    pub(crate) child: Option<TtsSidecar>,
    /// False once the sidecar failed to start or exited; the TTS engine is
    /// gated on it so replies degrade at once instead of timing out.
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) reported_exit: bool,
}

impl TtsSidecarHandle {
    /// Report a mid-run exit once. Requests degrade on their own.
    pub(crate) fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if !self.reported_exit && (!child.is_alive() || !child.output_open()) {
            self.reported_exit = true;
            self.alive.store(false, Ordering::Release);
            eprintln!(
                "tts_sidecar: exited; speech degrades to non-verbal reactions (no restart; restart the app to retry)"
            );
        }
    }
}

/// `AIVTUBER_TTS_SIDECAR` (off by default): spawn and own `local-tts-sidecar`.
/// Misconfiguration is a startup error. A sidecar that cannot start or become
/// ready is not: the app carries on and every reply degrades to the typed
/// non-verbal reaction, the same as any TTS failure.
pub(crate) fn start_tts_sidecar(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<TtsSidecarHandle>, Box<dyn Error>> {
    let Some(program) = env_optional("AIVTUBER_TTS_SIDECAR") else {
        return Ok(None);
    };
    if !profile.local_stack() {
        return Err(
            "AIVTUBER_TTS_SIDECAR is supported with AIVTUBER_MODE=local or hybrid only; start the sidecar yourself and point AIVTUBER_TTS_ENDPOINT at it"
                .into(),
        );
    }
    if !env_bool("AIVTUBER_GENERATIVE_ENABLED", false)? {
        return Err("AIVTUBER_TTS_SIDECAR requires AIVTUBER_GENERATIVE_ENABLED=true".into());
    }
    let model_dir = env_path("AIVTUBER_TTS_SIDECAR_MODEL_DIR")
        .ok_or("AIVTUBER_TTS_SIDECAR_MODEL_DIR is required with AIVTUBER_TTS_SIDECAR")?;
    let extra_args: Vec<String> = env_optional("AIVTUBER_TTS_SIDECAR_ARGS")
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let settings = TtsSidecarSettings {
        program: PathBuf::from(&program),
        model_dir,
        audio_root: audio_root.to_path_buf(),
        viseme_root: env_bool("AIVTUBER_TTS_SIDECAR_AMPLITUDE_VISEMES", false)?
            .then(|| pack_root.to_path_buf()),
        endpoint: required_env("AIVTUBER_TTS_ENDPOINT")?,
        speaker_id: u32::try_from(env_u64("AIVTUBER_TTS_SIDECAR_SPEAKER_ID", 0)?)
            .map_err(|_| "AIVTUBER_TTS_SIDECAR_SPEAKER_ID is too large")?,
        threads: u32::try_from(env_u64("AIVTUBER_TTS_SIDECAR_THREADS", 4)?)
            .map_err(|_| "AIVTUBER_TTS_SIDECAR_THREADS is too large")?,
        ready_timeout: Duration::from_millis(env_u64(
            "AIVTUBER_TTS_SIDECAR_READY_TIMEOUT_MS",
            60_000,
        )?),
        extra_args,
    };
    let config = settings.sidecar_config()?;
    // Operator-facing notice of the voice licence (S2a decision Q1).
    eprintln!("tts_sidecar: {VOICE_LICENCE_NOTICE}");
    let started = Instant::now();
    let alive = Arc::new(AtomicBool::new(false));
    match TtsSidecar::spawn(&config) {
        Ok(child) => {
            alive.store(true, Ordering::Release);
            if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                eprintln!(
                    "tts_sidecar: ready listen={} model={} load_ms={} visemes={}",
                    child.ready().listen,
                    child.ready().model_id,
                    started.elapsed().as_millis(),
                    if settings.viseme_root.is_some() {
                        "amplitude"
                    } else {
                        "off"
                    }
                );
            }
            Ok(Some(TtsSidecarHandle {
                child: Some(child),
                alive,
                reported_exit: false,
            }))
        }
        Err(message) => {
            eprintln!(
                "tts_sidecar: unavailable: {message}; speech degrades to non-verbal reactions"
            );
            Ok(Some(TtsSidecarHandle {
                child: None,
                alive,
                reported_exit: true,
            }))
        }
    }
}

/// `AIVTUBER_TTS_BACKEND=voicevox` (off by default): validate the settings and
/// start the in-process bridge to a separately started VOICEVOX ENGINE.
/// Misconfiguration (no speaker, non-loopback engine, ...) is a startup error;
/// an engine that is not running yet is not.
pub(crate) fn start_voicevox_bridge(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<VoicevoxBridge>, Box<dyn Error>> {
    let generative = env_bool("AIVTUBER_GENERATIVE_ENABLED", false)?;
    let Some(settings) =
        VoicevoxSettings::resolve(&|name| env_optional(name), profile, generative)?
    else {
        return Ok(None);
    };
    let bridge = VoicevoxBridge::start(settings, audio_root, pack_root)?;
    for line in bridge.startup_lines() {
        eprintln!("{line}");
    }
    Ok(Some(bridge))
}

/// `AIVTUBER_TTS_BACKEND=aivis-cloud` (off by default): validate the settings
/// and start the in-process bridge to the Aivis Cloud API. Misconfiguration (no
/// key, no model, no credit line, non-https endpoint, ...) is a startup error;
/// an unreachable service or a rejected key is not (synthesis then degrades
/// like any TTS failure). The startup log states that reply text leaves the
/// machine; it never carries the key or the spoken text.
pub(crate) fn start_aivis_bridge(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<AivisBridge>, Box<dyn Error>> {
    let generative = env_bool("AIVTUBER_GENERATIVE_ENABLED", false)?;
    let Some(settings) = AivisSettings::resolve(&|name| env_optional(name), profile, generative)?
    else {
        return Ok(None);
    };
    let bridge = AivisBridge::start(settings, audio_root, pack_root)?;
    for line in bridge.startup_lines() {
        eprintln!("{line}");
    }
    Ok(Some(bridge))
}

/// `AIVTUBER_TTS_BACKEND=irodori` (off by default, experimental): validate the
/// settings and start the in-process bridge to an operator-run
/// Irodori-TTS-Server. Misconfiguration (no endpoint, non-loopback server, bad
/// seed, ...) is a startup error; a server that is not running is not.
pub(crate) fn start_irodori_bridge(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<IrodoriBridge>, Box<dyn Error>> {
    let generative = env_bool("AIVTUBER_GENERATIVE_ENABLED", false)?;
    let Some(settings) = IrodoriSettings::resolve(&|name| env_optional(name), profile, generative)?
    else {
        return Ok(None);
    };
    let bridge = IrodoriBridge::start(settings, audio_root, pack_root)?;
    for line in bridge.startup_lines() {
        eprintln!("{line}");
    }
    Ok(Some(bridge))
}
