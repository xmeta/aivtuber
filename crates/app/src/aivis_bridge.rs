//! Optional Aivis Cloud API TTS backend for the `local` profile (issue #327).
//!
//! `AIVTUBER_TTS_BACKEND=aivis-cloud` (or `aivis`) starts the TTS bridge
//! *inside this process* on an ephemeral loopback port, exactly like the
//! VOICEVOX backend, and points the existing normalized-HTTP TTS adapter at it.
//! The bridge is the `local-tts-sidecar` server with an Aivis Cloud
//! synthesizer (`aivtuber_adapters::speech_tts::aivis_cloud`): one TTS wire
//! contract, one audio cache and one failure path whichever voice is
//! configured.
//!
//! This is the only backend that sends data off the machine: reply text (the
//! projected speech text, ADR-0004 D-5) goes to Aivis Cloud over HTTPS. It is an
//! explicit opt-in, announced at startup. The key and the spoken text are never
//! logged. There is no default voice: the operator chooses the AivisHub model
//! and owns its licence and credit line.
//!
//! | variable | meaning |
//! | --- | --- |
//! | `AIVTUBER_TTS_BACKEND` | `aivis-cloud` (or `aivis`) selects this backend |
//! | `AIVIS_API` | API key (required); held as a secret, never printed |
//! | `AIVTUBER_AIVIS_MODEL_UUID` | AivisHub model UUID or `ak_...` access key (required) |
//! | `AIVTUBER_AIVIS_SPEAKER_UUID` | speaker UUID of a multi-speaker model (optional) |
//! | `AIVTUBER_AIVIS_STYLE_ID` | style id 0..=31 (optional) |
//! | `AIVTUBER_AIVIS_CREDIT` | credit line the model requires (required) |
//! | `AIVTUBER_AIVIS_ENDPOINT` | API origin, default `https://api.aivis-project.com`; `http://` only on loopback |
//! | `AIVTUBER_AIVIS_TIMEOUT_MS` | budget for one synthesis, default 12000 |
//! | `AIVTUBER_AIVIS_AMPLITUDE_VISEMES` | derive a mouth track from the audio, default off |

use crate::CompositionProfile;
use crate::voicevox_bridge::{TTS_BACKEND_VAR, start_loopback_bridge};
use aivtuber_adapters::SecretString;
use aivtuber_adapters::speech_tts::aivis_cloud::{
    self, AivisCloudConfig, AivisCloudConfigError, AivisCloudSynthesizer,
};
use aivtuber_adapters::speech_tts::server::ServerHandle;
use std::path::Path;
use std::time::Duration;

pub const AIVIS_KEY_VAR: &str = "AIVIS_API";
pub const AIVIS_MODEL_VAR: &str = "AIVTUBER_AIVIS_MODEL_UUID";
pub const AIVIS_SPEAKER_VAR: &str = "AIVTUBER_AIVIS_SPEAKER_UUID";
pub const AIVIS_STYLE_VAR: &str = "AIVTUBER_AIVIS_STYLE_ID";
pub const AIVIS_CREDIT_VAR: &str = "AIVTUBER_AIVIS_CREDIT";
pub const AIVIS_ENDPOINT_VAR: &str = "AIVTUBER_AIVIS_ENDPOINT";
pub const AIVIS_TIMEOUT_VAR: &str = "AIVTUBER_AIVIS_TIMEOUT_MS";
pub const AIVIS_VISEMES_VAR: &str = "AIVTUBER_AIVIS_AMPLITUDE_VISEMES";

const DEFAULT_TIMEOUT_MS: u64 = 12_000;

/// Whether `AIVTUBER_TTS_BACKEND` (already trimmed and lower-cased) names this
/// backend. The VOICEVOX settings use it to stay out of the way.
pub(crate) fn is_aivis_backend_name(name: &str) -> bool {
    matches!(name, "aivis-cloud" | "aivis")
}

/// A typed startup error: every misconfiguration names the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AivisSettingsError {
    NotLocalProfile,
    GenerativeDisabled,
    /// Another TTS selection is also set.
    Conflict(&'static str),
    MissingKey,
    /// No default voice: the operator must choose one.
    MissingModel,
    MissingCredit,
    InvalidModel,
    InvalidSpeaker,
    InvalidStyle(String),
    InvalidCredit(String),
    InvalidTimeout(String),
    InvalidFlag(&'static str),
    Endpoint(AivisCloudConfigError),
    /// The key is present but unusable (blank after trimming is "missing").
    InvalidKey,
}

impl std::fmt::Display for AivisSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocalProfile => write!(
                f,
                "{TTS_BACKEND_VAR}=aivis-cloud is supported with AIVTUBER_MODE=local only; start a bridge yourself and set AIVTUBER_TTS_ENDPOINT for other profiles"
            ),
            Self::GenerativeDisabled => write!(
                f,
                "{TTS_BACKEND_VAR}=aivis-cloud requires AIVTUBER_GENERATIVE_ENABLED=true"
            ),
            Self::Conflict(name) => write!(
                f,
                "{name} must not be set with {TTS_BACKEND_VAR}=aivis-cloud: the bridge supplies the TTS endpoint"
            ),
            Self::MissingKey => write!(
                f,
                "{AIVIS_KEY_VAR} is required with {TTS_BACKEND_VAR}=aivis-cloud (your Aivis Cloud API key)"
            ),
            Self::InvalidKey => write!(
                f,
                "{AIVIS_KEY_VAR} contains control characters (check how the key was set)"
            ),
            Self::MissingModel => write!(
                f,
                "{AIVIS_MODEL_VAR} is required with {TTS_BACKEND_VAR}=aivis-cloud: there is no default voice; choose an AivisHub model UUID and read that model's licence"
            ),
            Self::MissingCredit => write!(
                f,
                "{AIVIS_CREDIT_VAR} is required with {TTS_BACKEND_VAR}=aivis-cloud: AivisHub models carry per-model licences; state the credit line the model requires"
            ),
            Self::InvalidModel => write!(
                f,
                "{AIVIS_MODEL_VAR} must be a model UUID or an ak_... access key"
            ),
            Self::InvalidSpeaker => write!(f, "{AIVIS_SPEAKER_VAR} must be a UUID"),
            Self::InvalidStyle(value) => write!(
                f,
                "{AIVIS_STYLE_VAR} must be an integer from 0 to {} (got {value:?})",
                aivis_cloud::MAX_STYLE_ID
            ),
            Self::InvalidCredit(reason) => write!(f, "{AIVIS_CREDIT_VAR}: {reason}"),
            Self::InvalidTimeout(value) => write!(
                f,
                "{AIVIS_TIMEOUT_VAR} must be an integer from 1 to 120000 (got {value:?})"
            ),
            Self::InvalidFlag(name) => {
                write!(f, "{name} must be true/false, 1/0, yes/no, or on/off")
            }
            Self::Endpoint(error) => write!(f, "{AIVIS_ENDPOINT_VAR}: {error}"),
        }
    }
}

impl std::error::Error for AivisSettingsError {}

/// The operator's Aivis Cloud settings, already validated. `Debug` redacts the
/// key (it is a `SecretString`).
#[derive(Debug, Clone, PartialEq)]
pub struct AivisSettings {
    pub config: AivisCloudConfig,
    /// The credit line the operator must show; printed at startup.
    pub credit: String,
    /// Budget for one synthesis.
    pub synth_timeout: Duration,
    pub amplitude_visemes: bool,
}

impl AivisSettings {
    /// `Ok(None)` unless `AIVTUBER_TTS_BACKEND` is `aivis-cloud` or `aivis`.
    /// `get` returns a variable's non-blank value.
    pub fn resolve(
        get: &dyn Fn(&str) -> Option<String>,
        profile: CompositionProfile,
        generative_enabled: bool,
    ) -> Result<Option<Self>, AivisSettingsError> {
        let selected = get(TTS_BACKEND_VAR)
            .map(|value| value.trim().to_ascii_lowercase())
            .is_some_and(|name| is_aivis_backend_name(&name));
        if !selected {
            // Unknown names are rejected by the VOICEVOX resolver, which runs
            // first at startup and owns the selector's vocabulary.
            return Ok(None);
        }
        if profile != CompositionProfile::Local {
            return Err(AivisSettingsError::NotLocalProfile);
        }
        if !generative_enabled {
            return Err(AivisSettingsError::GenerativeDisabled);
        }
        for name in ["AIVTUBER_TTS_SIDECAR", "AIVTUBER_TTS_ENDPOINT"] {
            if get(name).is_some() {
                return Err(AivisSettingsError::Conflict(name));
            }
        }
        let key = get(AIVIS_KEY_VAR).ok_or(AivisSettingsError::MissingKey)?;
        let model = get(AIVIS_MODEL_VAR).ok_or(AivisSettingsError::MissingModel)?;
        let credit = aivtuber_adapters::speech_tts::voicevox::validate_credit_line(
            &get(AIVIS_CREDIT_VAR).ok_or(AivisSettingsError::MissingCredit)?,
        )
        .map_err(AivisSettingsError::InvalidCredit)?;
        let endpoint =
            get(AIVIS_ENDPOINT_VAR).unwrap_or_else(|| aivis_cloud::DEFAULT_ENDPOINT.to_owned());
        let mut config =
            AivisCloudConfig::new(endpoint.trim(), SecretString::new(key.trim()), model.trim());
        if let Some(speaker) = get(AIVIS_SPEAKER_VAR) {
            config.speaker_uuid = Some(speaker.trim().to_owned());
        }
        if let Some(style) = get(AIVIS_STYLE_VAR) {
            config.style_id = Some(
                style
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|id| *id <= aivis_cloud::MAX_STYLE_ID)
                    .ok_or(AivisSettingsError::InvalidStyle(style))?,
            );
        }
        let synth_timeout_ms = match get(AIVIS_TIMEOUT_VAR) {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| (1..=120_000).contains(ms))
                .ok_or(AivisSettingsError::InvalidTimeout(value))?,
        };
        let amplitude_visemes = match get(AIVIS_VISEMES_VAR)
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("0" | "false" | "no" | "off") => false,
            Some("1" | "true" | "yes" | "on") => true,
            Some(_) => return Err(AivisSettingsError::InvalidFlag(AIVIS_VISEMES_VAR)),
        };
        config.validate().map_err(|error| match error {
            AivisCloudConfigError::ModelUuid => AivisSettingsError::InvalidModel,
            AivisCloudConfigError::SpeakerUuid => AivisSettingsError::InvalidSpeaker,
            AivisCloudConfigError::ApiKey => AivisSettingsError::InvalidKey,
            other => AivisSettingsError::Endpoint(other),
        })?;
        Ok(Some(Self {
            config,
            credit,
            synth_timeout: Duration::from_millis(synth_timeout_ms),
            amplitude_visemes,
        }))
    }

    /// `model_alias` recorded in TTS provenance (never an `ak_` access key).
    pub fn model_alias(&self) -> String {
        aivis_cloud::model_alias(&self.config.model_uuid, self.config.style_id)
    }
}

/// The running in-process bridge. Dropping it stops the listener.
pub struct AivisBridge {
    handle: ServerHandle,
    tts_endpoint: String,
    settings: AivisSettings,
}

impl AivisBridge {
    /// Bind the bridge on an ephemeral loopback port. A failure here is a
    /// startup error; an unreachable service or a rejected key is not (every
    /// synthesis then fails like any TTS failure and the reply degrades
    /// visibly).
    pub fn start(
        settings: AivisSettings,
        audio_root: &Path,
        pack_root: &Path,
    ) -> Result<Self, String> {
        let synth = AivisCloudSynthesizer::new(&settings.config).map_err(|e| e.to_string())?;
        let (handle, tts_endpoint) = start_loopback_bridge(
            synth,
            settings.synth_timeout,
            settings.amplitude_visemes,
            audio_root,
            pack_root,
        )?;
        Ok(Self {
            handle,
            tts_endpoint,
            settings,
        })
    }

    /// Where the normalized TTS adapter posts. Loopback.
    pub fn tts_endpoint(&self) -> &str {
        &self.tts_endpoint
    }

    pub fn settings(&self) -> &AivisSettings {
        &self.settings
    }

    pub fn stats(&self) -> aivtuber_adapters::speech_tts::server::ServerStats {
        self.handle.stats()
    }

    /// Non-secret startup lines: where the service is, which model, the credit
    /// line, and the egress notice. Neither the key nor the spoken text appears.
    pub fn startup_lines(&self) -> Vec<String> {
        vec![
            format!(
                "aivis-cloud: service={} model={} bridge={} visemes={}",
                aivtuber_adapters::endpoint_origin(&self.settings.config.endpoint),
                self.settings.model_alias(),
                aivtuber_adapters::endpoint_origin(&self.tts_endpoint),
                if self.settings.amplitude_visemes {
                    "amplitude"
                } else {
                    "off"
                },
            ),
            format!("aivis-cloud: credit line: {}", self.settings.credit),
            format!("aivis-cloud: NOTICE {}", aivis_cloud::EGRESS_NOTICE),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adapters::speech_tts::aivis_cloud_fake::{FakeAivisBehaviour, FakeAivisCloud};
    use aivtuber_adapters::{NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig};
    use aivtuber_domain::{EngineErrorKind, SpeechRequest};
    use std::collections::HashMap;
    use std::path::PathBuf;

    const MODEL: &str = "a59cb814-0083-4369-8542-f51a29e72af7";
    const KEY: &str = "sk-app-SECRETKEY-456";

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const BASE: [(&str, &str); 5] = [
        (TTS_BACKEND_VAR, "aivis-cloud"),
        (AIVIS_KEY_VAR, KEY),
        (AIVIS_MODEL_VAR, MODEL),
        (AIVIS_CREDIT_VAR, "Aivis:test model"),
        // Loopback fake in tests; the default is the https production origin.
        (AIVIS_ENDPOINT_VAR, "http://127.0.0.1:9"),
    ];

    fn without(name: &str) -> Vec<(&'static str, &'static str)> {
        BASE.iter().copied().filter(|(k, _)| *k != name).collect()
    }

    fn resolve(pairs: &[(&str, &str)]) -> Result<Option<AivisSettings>, AivisSettingsError> {
        AivisSettings::resolve(&lookup(pairs), CompositionProfile::Local, true)
    }

    #[test]
    fn another_backend_or_none_selects_nothing_and_aivis_names_are_accepted() {
        assert_eq!(resolve(&[]), Ok(None));
        assert_eq!(resolve(&[(TTS_BACKEND_VAR, "http")]), Ok(None));
        assert_eq!(resolve(&[(TTS_BACKEND_VAR, "voicevox")]), Ok(None));
        assert_eq!(resolve(&[(AIVIS_MODEL_VAR, MODEL)]), Ok(None));
        for name in ["aivis-cloud", "aivis", " AIVIS-Cloud "] {
            let mut pairs = without(TTS_BACKEND_VAR);
            pairs.push((TTS_BACKEND_VAR, name));
            assert!(resolve(&pairs).unwrap().is_some(), "{name}");
            // The VOICEVOX resolver stays out of the way.
            assert_eq!(
                crate::VoicevoxSettings::resolve(&lookup(&pairs), CompositionProfile::Local, true),
                Ok(None)
            );
        }
    }

    #[test]
    fn a_complete_configuration_is_accepted_with_documented_defaults() {
        let mut pairs = BASE.to_vec();
        pairs.retain(|(k, _)| *k != AIVIS_ENDPOINT_VAR);
        let settings = resolve(&pairs).unwrap().expect("aivis");
        assert_eq!(settings.config.endpoint, "https://api.aivis-project.com");
        assert_eq!(settings.config.model_uuid, MODEL);
        assert_eq!(settings.config.speaker_uuid, None);
        assert_eq!(settings.config.style_id, None);
        assert_eq!(settings.credit, "Aivis:test model");
        assert_eq!(settings.synth_timeout, Duration::from_millis(12_000));
        assert!(!settings.amplitude_visemes);
        assert_eq!(settings.model_alias(), MODEL);
        let debug = format!("{settings:?}");
        assert!(!debug.contains("SECRETKEY"), "{debug}");
    }

    #[test]
    fn the_key_model_and_credit_are_required_typed_errors_naming_the_variable() {
        for (name, expected) in [
            (AIVIS_KEY_VAR, AivisSettingsError::MissingKey),
            (AIVIS_MODEL_VAR, AivisSettingsError::MissingModel),
            (AIVIS_CREDIT_VAR, AivisSettingsError::MissingCredit),
        ] {
            let error = resolve(&without(name)).unwrap_err();
            assert_eq!(error, expected);
            assert!(error.to_string().contains(name), "{error}");
        }
        assert!(
            AivisSettingsError::MissingModel
                .to_string()
                .contains("no default voice")
        );
        let mut pairs = without(AIVIS_CREDIT_VAR);
        pairs.push((AIVIS_CREDIT_VAR, "line\nbreak"));
        assert!(matches!(
            resolve(&pairs).unwrap_err(),
            AivisSettingsError::InvalidCredit(_)
        ));
    }

    #[test]
    fn model_speaker_style_and_endpoint_are_validated() {
        for bad in ["", "not-a-uuid", "ak_", "../x"] {
            let mut pairs = without(AIVIS_MODEL_VAR);
            pairs.push((AIVIS_MODEL_VAR, bad));
            let error = resolve(&pairs).unwrap_err();
            assert!(
                matches!(
                    error,
                    AivisSettingsError::MissingModel | AivisSettingsError::InvalidModel
                ),
                "{bad:?}: {error:?}"
            );
        }
        let mut pairs = BASE.to_vec();
        pairs.push((AIVIS_MODEL_VAR, "ak_abc-123"));
        // Last one wins in the map: an access key is accepted.
        assert!(resolve(&pairs).unwrap().is_some());

        let mut pairs = BASE.to_vec();
        pairs.push((AIVIS_SPEAKER_VAR, "nope"));
        assert_eq!(resolve(&pairs), Err(AivisSettingsError::InvalidSpeaker));
        let mut pairs = BASE.to_vec();
        pairs.push((AIVIS_SPEAKER_VAR, "11111111-2222-3333-4444-555555555555"));
        pairs.push((AIVIS_STYLE_VAR, "31"));
        let settings = resolve(&pairs).unwrap().unwrap();
        assert_eq!(settings.config.style_id, Some(31));
        assert_eq!(settings.model_alias(), format!("{MODEL}/style-31"));
        for bad in ["32", "-1", "x", "1.5"] {
            let mut pairs = BASE.to_vec();
            pairs.push((AIVIS_STYLE_VAR, bad));
            assert!(
                matches!(resolve(&pairs), Err(AivisSettingsError::InvalidStyle(_))),
                "{bad}"
            );
        }
        for bad in [
            "http://api.aivis-project.com",
            "http://192.168.0.10:9",
            "https://api.aivis-project.com/v1",
            "https://user:pw@api.aivis-project.com",
        ] {
            let mut pairs = without(AIVIS_ENDPOINT_VAR);
            pairs.push((AIVIS_ENDPOINT_VAR, bad));
            assert!(
                matches!(resolve(&pairs), Err(AivisSettingsError::Endpoint(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn it_needs_the_local_profile_generation_and_no_competing_tts_setting() {
        assert_eq!(
            AivisSettings::resolve(&lookup(&BASE), CompositionProfile::Cached, true),
            Err(AivisSettingsError::NotLocalProfile)
        );
        assert_eq!(
            AivisSettings::resolve(&lookup(&BASE), CompositionProfile::Local, false),
            Err(AivisSettingsError::GenerativeDisabled)
        );
        for name in ["AIVTUBER_TTS_SIDECAR", "AIVTUBER_TTS_ENDPOINT"] {
            let mut pairs = BASE.to_vec();
            pairs.push((name, "x"));
            assert_eq!(
                resolve(&pairs).unwrap_err(),
                AivisSettingsError::Conflict(name)
            );
        }
    }

    #[test]
    fn timeout_and_viseme_flag_are_parsed_strictly() {
        let mut pairs = BASE.to_vec();
        pairs.push((AIVIS_TIMEOUT_VAR, "5000"));
        pairs.push((AIVIS_VISEMES_VAR, "on"));
        let settings = resolve(&pairs).unwrap().unwrap();
        assert_eq!(settings.synth_timeout, Duration::from_secs(5));
        assert!(settings.amplitude_visemes);
        for bad in ["0", "120001", "fast"] {
            let mut pairs = BASE.to_vec();
            pairs.push((AIVIS_TIMEOUT_VAR, bad));
            assert!(matches!(
                resolve(&pairs),
                Err(AivisSettingsError::InvalidTimeout(_))
            ));
        }
        let mut pairs = BASE.to_vec();
        pairs.push((AIVIS_VISEMES_VAR, "maybe"));
        assert!(matches!(
            resolve(&pairs),
            Err(AivisSettingsError::InvalidFlag(_))
        ));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-aivis-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings_for(endpoint: &str, extra: &[(&str, &str)]) -> AivisSettings {
        let mut pairs = without(AIVIS_ENDPOINT_VAR);
        pairs.push((AIVIS_ENDPOINT_VAR, endpoint));
        pairs.extend_from_slice(extra);
        resolve(&pairs).unwrap().unwrap()
    }

    fn adapter(bridge: &AivisBridge) -> NormalizedHttpTtsAdapter {
        NormalizedHttpTtsAdapter::new(
            NormalizedHttpTtsConfig {
                endpoint: bridge.tts_endpoint().to_owned(),
                backend_name: aivis_cloud::BACKEND_NAME.to_owned(),
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

    #[test]
    fn the_unchanged_normalized_contract_reaches_the_service_and_writes_a_wav() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let root = scratch("ok");
        let bridge = AivisBridge::start(
            settings_for(&api.endpoint(), &[]),
            &root.join("audio"),
            &root,
        )
        .expect("bridge");
        assert!(bridge.tts_endpoint().starts_with("http://127.0.0.1:"));
        let artifact = adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "こんにちは".to_owned(),
                // The request's style never selects a voice or reaches the API.
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
        assert_eq!(api.spoken_texts(), vec!["こんにちは".to_owned()]);
        assert!(
            !api.requests()
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("cheerful"))
        );
        // The same text again is a cache hit: no further call (no extra cost).
        let calls = api.requests().len();
        adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "こんにちは".to_owned(),
                style: None,
            })
            .expect("cached");
        assert_eq!(api.requests().len(), calls);
        assert_eq!(bridge.stats().cache_hits, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn changing_the_voice_never_replays_stale_audio() {
        let api = FakeAivisCloud::start(FakeAivisBehaviour::default());
        let root = scratch("voice");
        let request = SpeechRequest {
            text: "同じ文".to_owned(),
            style: None,
        };
        let mut refs = Vec::new();
        for extra in [
            &[][..],
            &[(AIVIS_STYLE_VAR, "1")][..],
            &[(AIVIS_SPEAKER_VAR, "11111111-2222-3333-4444-555555555555")][..],
            &[(AIVIS_MODEL_VAR, "ak_other-model")][..],
        ] {
            // Same audio root every time: only the voice differs.
            let bridge = AivisBridge::start(
                settings_for(&api.endpoint(), extra),
                &root.join("audio"),
                &root,
            )
            .expect("bridge");
            refs.push(
                adapter(&bridge)
                    .synthesize_sync(&request)
                    .unwrap()
                    .audio_ref,
            );
        }
        let unique: std::collections::BTreeSet<_> = refs.iter().collect();
        assert_eq!(
            unique.len(),
            refs.len(),
            "one cache entry per voice: {refs:?}"
        );
        assert_eq!(api.requests().len(), 4, "no voice reused another's audio");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unauthorized_rate_limited_and_timeout_are_ordinary_tts_failures() {
        let root = scratch("fail");
        for status in [401_u16, 402, 429, 503] {
            let api = FakeAivisCloud::start(FakeAivisBehaviour {
                status,
                ..FakeAivisBehaviour::default()
            });
            let bridge =
                AivisBridge::start(settings_for(&api.endpoint(), &[]), &root, &root).unwrap();
            let error = adapter(&bridge)
                .synthesize_sync(&SpeechRequest {
                    text: "あ".to_owned(),
                    style: None,
                })
                .expect_err("failure");
            assert_eq!(error.kind, EngineErrorKind::Unavailable, "{status}");
            assert_eq!(bridge.stats().engine_errors, 1);
        }
        let slow = FakeAivisCloud::start(FakeAivisBehaviour {
            delay: Duration::from_millis(1_500),
            ..FakeAivisBehaviour::default()
        });
        let mut settings = settings_for(&slow.endpoint(), &[]);
        settings.synth_timeout = Duration::from_millis(250);
        let bridge = AivisBridge::start(settings, &root, &root).unwrap();
        let started = std::time::Instant::now();
        let _ = adapter(&bridge)
            .synthesize_sync(&SpeechRequest {
                text: "う".to_owned(),
                style: None,
            })
            .expect_err("timeout");
        assert!(started.elapsed() < Duration::from_millis(1_400));
        assert_eq!(bridge.stats().deadline_exceeded, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_lines_state_the_egress_and_carry_no_key_text_or_path() {
        let root = scratch("log");
        let bridge = AivisBridge::start(settings_for("http://127.0.0.1:9", &[]), &root, &root)
            .expect("bridge");
        let lines = bridge.startup_lines().join("\n");
        assert!(lines.contains("credit line: Aivis:test model"));
        assert!(lines.contains(MODEL));
        assert!(lines.contains("http://127.0.0.1:9"));
        assert!(lines.contains("sent to Aivis Cloud"), "{lines}");
        assert!(lines.contains("internet service"), "{lines}");
        assert!(!lines.contains("SECRETKEY"), "{lines}");
        assert!(!lines.contains(root.to_string_lossy().as_ref()));
        // An access key used as the model is never printed.
        let bridge = AivisBridge::start(
            settings_for(
                "http://127.0.0.1:9",
                &[(AIVIS_MODEL_VAR, "ak_secretaccess")],
            ),
            &root,
            &root,
        )
        .unwrap();
        assert!(!bridge.startup_lines().join("\n").contains("secretaccess"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
