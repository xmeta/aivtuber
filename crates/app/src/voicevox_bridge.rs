//! Optional VOICEVOX TTS backend for the `local` profile (issue #295).
//!
//! `AIVTUBER_TTS_BACKEND=voicevox` starts the TTS bridge *inside this process*
//! on an ephemeral loopback port and points the existing normalized-HTTP TTS
//! adapter at it. The bridge is the `local-tts-sidecar` server with a
//! VOICEVOX ENGINE synthesizer (`aivtuber_adapters::speech_tts::voicevox`), so
//! the app has one TTS wire contract, one audio cache and one failure path
//! whichever voice is configured. The ENGINE itself is a separate process the
//! operator starts; it is neither linked nor bundled.
//!
//! The speaker (the engine's style id) and the credit line are required and
//! have no default: the operator chooses the character and owns its terms.
//!
//! | variable | meaning |
//! | --- | --- |
//! | `AIVTUBER_TTS_BACKEND` | `voicevox` selects this backend; unset or `http` keeps `AIVTUBER_TTS_ENDPOINT` |
//! | `AIVTUBER_VOICEVOX_ENDPOINT` | engine origin, loopback `http://` only (required) |
//! | `AIVTUBER_VOICEVOX_SPEAKER` | engine speaker/style id (required) |
//! | `AIVTUBER_VOICEVOX_CREDIT` | credit line, e.g. `VOICEVOX:<character>` (required) |
//! | `AIVTUBER_VOICEVOX_TIMEOUT_MS` | total budget for one synthesis (both engine calls), default 12000 |
//! | `AIVTUBER_VOICEVOX_AMPLITUDE_VISEMES` | derive a mouth track from the audio, default off |

use crate::CompositionProfile;
use aivtuber_adapters::speech_tts::server::{DEFAULT_PATH, ServerConfig, ServerHandle, TtsServer};
use aivtuber_adapters::speech_tts::voicevox::{
    self, VoicevoxConfig, VoicevoxConfigError, VoicevoxSynthesizer,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const TTS_BACKEND_VAR: &str = "AIVTUBER_TTS_BACKEND";
pub const ENDPOINT_VAR: &str = "AIVTUBER_VOICEVOX_ENDPOINT";
pub const SPEAKER_VAR: &str = "AIVTUBER_VOICEVOX_SPEAKER";
pub const CREDIT_VAR: &str = "AIVTUBER_VOICEVOX_CREDIT";
pub const TIMEOUT_VAR: &str = "AIVTUBER_VOICEVOX_TIMEOUT_MS";
pub const VISEMES_VAR: &str = "AIVTUBER_VOICEVOX_AMPLITUDE_VISEMES";

const DEFAULT_TIMEOUT_MS: u64 = 12_000;

/// A typed startup error: every misconfiguration names the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoicevoxSettingsError {
    UnknownBackend(String),
    NotLocalProfile,
    GenerativeDisabled,
    /// Another TTS selection is also set.
    Conflict(&'static str),
    MissingEndpoint,
    /// No default character: the operator must choose one.
    MissingSpeaker,
    MissingCredit,
    InvalidSpeaker(String),
    InvalidCredit(String),
    InvalidTimeout(String),
    InvalidFlag(&'static str),
    Endpoint(VoicevoxConfigError),
}

impl std::fmt::Display for VoicevoxSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownBackend(value) => write!(
                f,
                "{TTS_BACKEND_VAR} must be voicevox or http (got {value:?})"
            ),
            Self::NotLocalProfile => write!(
                f,
                "{TTS_BACKEND_VAR}=voicevox is supported with AIVTUBER_MODE=local only; start the bridge yourself and set AIVTUBER_TTS_ENDPOINT for other profiles"
            ),
            Self::GenerativeDisabled => write!(
                f,
                "{TTS_BACKEND_VAR}=voicevox requires AIVTUBER_GENERATIVE_ENABLED=true"
            ),
            Self::Conflict(name) => write!(
                f,
                "{name} must not be set with {TTS_BACKEND_VAR}=voicevox: the bridge supplies the TTS endpoint"
            ),
            Self::MissingEndpoint => write!(
                f,
                "{ENDPOINT_VAR} is required with {TTS_BACKEND_VAR}=voicevox (the engine origin, e.g. http://127.0.0.1:50021)"
            ),
            Self::MissingSpeaker => write!(
                f,
                "{SPEAKER_VAR} is required with {TTS_BACKEND_VAR}=voicevox: there is no default character; choose a speaker id from the engine's /speakers and read that character's terms"
            ),
            Self::MissingCredit => write!(
                f,
                "{CREDIT_VAR} is required with {TTS_BACKEND_VAR}=voicevox: every VOICEVOX character requires a credit line such as VOICEVOX:<character name>"
            ),
            Self::InvalidSpeaker(value) => {
                write!(
                    f,
                    "{SPEAKER_VAR} must be an unsigned integer (got {value:?})"
                )
            }
            Self::InvalidCredit(reason) => write!(f, "{CREDIT_VAR}: {reason}"),
            Self::InvalidTimeout(value) => write!(
                f,
                "{TIMEOUT_VAR} must be an integer from 1 to 120000 (got {value:?})"
            ),
            Self::InvalidFlag(name) => {
                write!(f, "{name} must be true/false, 1/0, yes/no, or on/off")
            }
            Self::Endpoint(error) => write!(f, "{ENDPOINT_VAR}: {error}"),
        }
    }
}

impl std::error::Error for VoicevoxSettingsError {}

/// The operator's VOICEVOX settings, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicevoxSettings {
    pub config: VoicevoxConfig,
    /// The credit line the operator must show; printed at startup.
    pub credit: String,
    /// Total budget for one synthesis (both engine calls).
    pub synth_timeout: Duration,
    pub amplitude_visemes: bool,
}

impl VoicevoxSettings {
    /// `Ok(None)` unless `AIVTUBER_TTS_BACKEND=voicevox`. `get` returns a
    /// variable's non-blank value.
    pub fn resolve(
        get: &dyn Fn(&str) -> Option<String>,
        profile: CompositionProfile,
        generative_enabled: bool,
    ) -> Result<Option<Self>, VoicevoxSettingsError> {
        match get(TTS_BACKEND_VAR)
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("http") | Some("normalized-http") => return Ok(None),
            Some("voicevox") => {}
            Some(other) => return Err(VoicevoxSettingsError::UnknownBackend(other.to_owned())),
        }
        if profile != CompositionProfile::Local {
            return Err(VoicevoxSettingsError::NotLocalProfile);
        }
        if !generative_enabled {
            return Err(VoicevoxSettingsError::GenerativeDisabled);
        }
        for name in ["AIVTUBER_TTS_SIDECAR", "AIVTUBER_TTS_ENDPOINT"] {
            if get(name).is_some() {
                return Err(VoicevoxSettingsError::Conflict(name));
            }
        }
        let endpoint = get(ENDPOINT_VAR).ok_or(VoicevoxSettingsError::MissingEndpoint)?;
        let speaker = get(SPEAKER_VAR).ok_or(VoicevoxSettingsError::MissingSpeaker)?;
        let speaker_id = speaker
            .trim()
            .parse::<u32>()
            .map_err(|_| VoicevoxSettingsError::InvalidSpeaker(speaker.clone()))?;
        let credit = voicevox::validate_credit_line(
            &get(CREDIT_VAR).ok_or(VoicevoxSettingsError::MissingCredit)?,
        )
        .map_err(VoicevoxSettingsError::InvalidCredit)?;
        let synth_timeout_ms = match get(TIMEOUT_VAR) {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| (1..=120_000).contains(ms))
                .ok_or(VoicevoxSettingsError::InvalidTimeout(value))?,
        };
        let amplitude_visemes = match get(VISEMES_VAR).map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("0" | "false" | "no" | "off") => false,
            Some("1" | "true" | "yes" | "on") => true,
            Some(_) => return Err(VoicevoxSettingsError::InvalidFlag(VISEMES_VAR)),
        };
        let config = VoicevoxConfig::new(endpoint.trim(), speaker_id);
        config.validate().map_err(VoicevoxSettingsError::Endpoint)?;
        Ok(Some(Self {
            config,
            credit,
            synth_timeout: Duration::from_millis(synth_timeout_ms),
            amplitude_visemes,
        }))
    }

    /// `model_alias` recorded in TTS provenance. The engine's version is not
    /// probed, so there is no `model_version`.
    pub fn model_alias(&self) -> String {
        format!("speaker-{}", self.config.speaker_id)
    }
}

/// The running in-process bridge. Dropping it stops the listener.
pub struct VoicevoxBridge {
    handle: ServerHandle,
    tts_endpoint: String,
    settings: VoicevoxSettings,
}

impl VoicevoxBridge {
    /// Bind the bridge on an ephemeral loopback port. A failure here is a
    /// startup error; an engine that is down is not (every synthesis then
    /// fails like any TTS failure and the reply degrades visibly).
    pub fn start(
        settings: VoicevoxSettings,
        audio_root: &Path,
        pack_root: &Path,
    ) -> Result<Self, String> {
        let synth = VoicevoxSynthesizer::new(&settings.config).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(audio_root).map_err(|error| {
            format!(
                "cannot create the audio root {}: {error}",
                audio_root.display()
            )
        })?;
        let mut config =
            ServerConfig::new("127.0.0.1:0".parse().expect("loopback literal"), audio_root);
        config.synth_deadline = settings.synth_timeout;
        if settings.amplitude_visemes {
            config.viseme_root = Some(pack_root.to_path_buf());
        }
        let server = TtsServer::bind(config, Arc::new(synth))?;
        let tts_endpoint = format!("http://{}{DEFAULT_PATH}", server.local_addr());
        Ok(Self {
            handle: server.spawn(),
            tts_endpoint,
            settings,
        })
    }

    /// Where the normalized TTS adapter posts. Loopback.
    pub fn tts_endpoint(&self) -> &str {
        &self.tts_endpoint
    }

    pub fn settings(&self) -> &VoicevoxSettings {
        &self.settings
    }

    pub fn stats(&self) -> aivtuber_adapters::speech_tts::server::ServerStats {
        self.handle.stats()
    }

    /// Non-secret startup lines: where the engine is, which speaker, and the
    /// credit and terms notice. The spoken text never appears.
    pub fn startup_lines(&self) -> Vec<String> {
        vec![
            format!(
                "voicevox: engine={} speaker={} bridge={} visemes={} (the engine is a separate process; synthesis fails like any TTS failure while it is down)",
                aivtuber_adapters::endpoint_origin(&self.settings.config.endpoint),
                self.settings.config.speaker_id,
                aivtuber_adapters::endpoint_origin(&self.tts_endpoint),
                if self.settings.amplitude_visemes {
                    "amplitude"
                } else {
                    "off"
                },
            ),
            format!("voicevox: credit line: {}", self.settings.credit),
            format!("voicevox: {}", voicevox::TERMS_NOTICE),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adapters::speech_tts::voicevox_fake::{FakeBehaviour, FakeVoicevoxEngine};
    use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
    use aivtuber_domain::{EngineErrorKind, SpeechRequest};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const BASE: [(&str, &str); 4] = [
        (TTS_BACKEND_VAR, "voicevox"),
        (ENDPOINT_VAR, "http://127.0.0.1:50021"),
        (SPEAKER_VAR, "3"),
        (CREDIT_VAR, "VOICEVOX:ずんだもん"),
    ];

    fn without(name: &str) -> Vec<(&'static str, &'static str)> {
        BASE.iter().copied().filter(|(k, _)| *k != name).collect()
    }

    fn resolve(pairs: &[(&str, &str)]) -> Result<Option<VoicevoxSettings>, VoicevoxSettingsError> {
        VoicevoxSettings::resolve(&lookup(pairs), CompositionProfile::Local, true)
    }

    #[test]
    fn another_backend_or_none_selects_nothing() {
        assert_eq!(resolve(&[]), Ok(None));
        assert_eq!(resolve(&[(TTS_BACKEND_VAR, "http")]), Ok(None));
        // Unrelated VOICEVOX variables without the selector do nothing.
        assert_eq!(resolve(&[(SPEAKER_VAR, "3")]), Ok(None));
        assert!(matches!(
            resolve(&[(TTS_BACKEND_VAR, "bark")]),
            Err(VoicevoxSettingsError::UnknownBackend(_))
        ));
    }

    #[test]
    fn a_complete_configuration_is_accepted_with_documented_defaults() {
        let settings = resolve(&BASE).unwrap().expect("voicevox");
        assert_eq!(settings.config.speaker_id, 3);
        assert_eq!(settings.credit, "VOICEVOX:ずんだもん");
        assert_eq!(settings.synth_timeout, Duration::from_millis(12_000));
        assert!(!settings.amplitude_visemes);
        assert_eq!(settings.model_alias(), "speaker-3");
    }

    #[test]
    fn a_missing_speaker_is_a_typed_startup_error_never_a_default_character() {
        let error = resolve(&without(SPEAKER_VAR)).unwrap_err();
        assert_eq!(error, VoicevoxSettingsError::MissingSpeaker);
        assert!(error.to_string().contains(SPEAKER_VAR));
        assert!(error.to_string().contains("no default character"));
        for bad in ["", "  ", "-1", "3.5", "zundamon", "99999999999"] {
            let mut pairs = without(SPEAKER_VAR);
            pairs.push((SPEAKER_VAR, bad));
            let error = resolve(&pairs).unwrap_err();
            assert!(
                matches!(
                    error,
                    VoicevoxSettingsError::MissingSpeaker
                        | VoicevoxSettingsError::InvalidSpeaker(_)
                ),
                "{bad:?}: {error:?}"
            );
        }
    }

    #[test]
    fn the_endpoint_and_the_credit_line_are_required() {
        assert_eq!(
            resolve(&without(ENDPOINT_VAR)).unwrap_err(),
            VoicevoxSettingsError::MissingEndpoint
        );
        assert_eq!(
            resolve(&without(CREDIT_VAR)).unwrap_err(),
            VoicevoxSettingsError::MissingCredit
        );
        let mut pairs = without(CREDIT_VAR);
        pairs.push((CREDIT_VAR, "line\nbreak"));
        assert!(matches!(
            resolve(&pairs).unwrap_err(),
            VoicevoxSettingsError::InvalidCredit(_)
        ));
    }

    #[test]
    fn a_non_loopback_engine_endpoint_is_refused_at_startup() {
        for bad in [
            "http://192.168.0.10:50021",
            "http://voicevox.example:50021",
            "https://127.0.0.1:50021",
            "http://127.0.0.1:50021/v1",
        ] {
            let mut pairs = without(ENDPOINT_VAR);
            pairs.push((ENDPOINT_VAR, bad));
            assert!(
                matches!(resolve(&pairs), Err(VoicevoxSettingsError::Endpoint(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn it_needs_the_local_profile_generation_and_no_competing_tts_setting() {
        assert_eq!(
            VoicevoxSettings::resolve(&lookup(&BASE), CompositionProfile::Cached, true),
            Err(VoicevoxSettingsError::NotLocalProfile)
        );
        assert_eq!(
            VoicevoxSettings::resolve(&lookup(&BASE), CompositionProfile::Local, false),
            Err(VoicevoxSettingsError::GenerativeDisabled)
        );
        for name in ["AIVTUBER_TTS_SIDECAR", "AIVTUBER_TTS_ENDPOINT"] {
            let mut pairs = BASE.to_vec();
            pairs.push((name, "x"));
            assert_eq!(
                resolve(&pairs).unwrap_err(),
                VoicevoxSettingsError::Conflict(name)
            );
        }
    }

    #[test]
    fn timeout_and_viseme_flag_are_parsed_strictly() {
        let mut pairs = BASE.to_vec();
        pairs.push((TIMEOUT_VAR, "5000"));
        pairs.push((VISEMES_VAR, "on"));
        let settings = resolve(&pairs).unwrap().unwrap();
        assert_eq!(settings.synth_timeout, Duration::from_secs(5));
        assert!(settings.amplitude_visemes);
        for bad in ["0", "120001", "fast"] {
            let mut pairs = BASE.to_vec();
            pairs.push((TIMEOUT_VAR, bad));
            assert!(matches!(
                resolve(&pairs),
                Err(VoicevoxSettingsError::InvalidTimeout(_))
            ));
        }
        let mut pairs = BASE.to_vec();
        pairs.push((VISEMES_VAR, "maybe"));
        assert!(matches!(
            resolve(&pairs),
            Err(VoicevoxSettingsError::InvalidFlag(_))
        ));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-voicevox-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn adapter(bridge: &VoicevoxBridge) -> NormalizedHttpTtsAdapter {
        NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: bridge.tts_endpoint().to_owned(),
                backend_name: voicevox::BACKEND_NAME.to_owned(),
                model_alias: Some(bridge.settings().model_alias()),
                model_version: None,
                voice_model: Some("example-voice-v1".to_owned()),
                viseme_mapping: Some("ja-5vowel-v1".to_owned()),
                timeout: Duration::from_secs(10),
            },
            None,
        )
        .expect("adapter")
    }

    fn settings_for(engine_endpoint: &str) -> VoicevoxSettings {
        let pairs = [
            (TTS_BACKEND_VAR, "voicevox"),
            (ENDPOINT_VAR, engine_endpoint),
            (SPEAKER_VAR, "8"),
            (CREDIT_VAR, "VOICEVOX:test"),
        ];
        resolve(&pairs).unwrap().unwrap()
    }

    #[test]
    fn the_unchanged_normalized_contract_reaches_the_engine_and_writes_a_wav() {
        let engine = FakeVoicevoxEngine::start(FakeBehaviour::default());
        let root = scratch("ok");
        let bridge =
            VoicevoxBridge::start(settings_for(&engine.endpoint()), &root.join("audio"), &root)
                .expect("bridge");
        assert!(bridge.tts_endpoint().starts_with("http://127.0.0.1:"));
        let artifact = adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "こんにちは".to_owned(),
                // The request's style never selects a voice or reaches the engine.
                style: Some("cheerful".to_owned()),
            })
            .expect("speech");
        assert!(artifact.audio_ref.starts_with("audio://generated/"));
        assert_eq!(artifact.duration_ms, 5 * 40);
        let file = root
            .join("audio")
            .join("generated")
            .join(artifact.audio_ref.trim_start_matches("audio://generated/"));
        assert!(file.is_file(), "the WAV is under the audio root");
        assert_eq!(engine.spoken_texts(), vec!["こんにちは".to_owned()]);
        assert!(
            !engine
                .requests()
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("cheerful")
                    || r.target.contains("cheerful"))
        );
        // The same text again is a cache hit: no further engine call.
        let calls = engine.requests().len();
        adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "こんにちは".to_owned(),
                style: None,
            })
            .expect("cached");
        assert_eq!(engine.requests().len(), calls);
        assert_eq!(bridge.stats().cache_hits, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn engine_unreachable_non_2xx_and_timeout_are_ordinary_tts_failures() {
        let root = scratch("fail");
        // Unreachable.
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            endpoint
        };
        let unreachable =
            VoicevoxBridge::start(settings_for(&dead), &root, &root).expect("bridge starts");
        let error = adapter(&unreachable)
            .synthesize_sync(&SpeechRequest {
                text: "あ".to_owned(),
                style: None,
            })
            .expect_err("engine is down");
        assert_eq!(error.kind, EngineErrorKind::Unavailable, "{error:?}");
        assert_eq!(unreachable.stats().engine_errors, 1);

        // Non-2xx from the engine.
        let broken = FakeVoicevoxEngine::start(FakeBehaviour {
            synthesis_status: 500,
            ..FakeBehaviour::default()
        });
        let bridge = VoicevoxBridge::start(settings_for(&broken.endpoint()), &root, &root).unwrap();
        let error = adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "い".to_owned(),
                style: None,
            })
            .expect_err("500");
        assert_eq!(error.kind, EngineErrorKind::Unavailable);

        // A hung engine: the synthesis budget ends it (504, not a hang).
        let slow = FakeVoicevoxEngine::start(FakeBehaviour {
            delay: Duration::from_millis(1_500),
            ..FakeBehaviour::default()
        });
        let mut settings = settings_for(&slow.endpoint());
        settings.synth_timeout = Duration::from_millis(250);
        let bridge = VoicevoxBridge::start(settings, &root, &root).unwrap();
        let started = std::time::Instant::now();
        let error = adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "う".to_owned(),
                style: None,
            })
            .expect_err("timeout");
        assert!(started.elapsed() < Duration::from_millis(1_400));
        assert_eq!(bridge.stats().deadline_exceeded, 1, "{error:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_lines_carry_the_credit_and_terms_but_no_secret_or_path() {
        let root = scratch("log");
        let bridge = VoicevoxBridge::start(settings_for("http://127.0.0.1:50021"), &root, &root)
            .expect("bridge");
        let lines = bridge.startup_lines().join("\n");
        assert!(lines.contains("credit line: VOICEVOX:test"));
        assert!(lines.contains("speaker=8"));
        assert!(lines.contains("http://127.0.0.1:50021"));
        assert!(lines.contains("character"), "terms notice present");
        assert!(!lines.contains(root.to_string_lossy().as_ref()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
