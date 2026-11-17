//! Optional Irodori-TTS backend for the `local` profile (issue #411).
//!
//! `AIVTUBER_TTS_BACKEND=irodori` starts the TTS bridge *inside this process*
//! on an ephemeral loopback port, like the VOICEVOX backend, with an
//! [`IrodoriSynthesizer`] behind it: the normalized TTS contract is translated
//! to one request to the official Irodori-TTS-Server the operator runs on
//! loopback (GPU, LUNA checkpoint). The server is neither linked, bundled nor
//! downloaded by this project, and the backend is experimental: the start-up
//! line says so (EXP-0006 measured a higher reading error than the default
//! voice).
//!
//! The seed is fixed for the session (and optionally a server-side voice the
//! operator installed is addressed by name). There is no way to pass reference
//! audio from this project. A fixed seed makes identical text reproducible; it
//! does not keep one speaker across different texts (measured, issue #411).
//!
//! | variable | meaning |
//! | --- | --- |
//! | `AIVTUBER_TTS_BACKEND` | `irodori` (or `irodori-tts`) selects this backend |
//! | `AIVTUBER_IRODORI_ENDPOINT` | server origin, loopback `http://` only (required) |
//! | `AIVTUBER_IRODORI_SEED` | fixed seed sent with every request, default 1234 |
//! | `AIVTUBER_IRODORI_VOICE` | optional server-side voice name (otherwise the unconditioned voice) |
//! | `AIVTUBER_IRODORI_MODEL` | the server's model name, default `irodori-tts` |
//! | `AIVTUBER_IRODORI_MODEL_LABEL` | optional label of the checkpoint, e.g. `LUNA-bf16`, in the cache identity |
//! | `AIVTUBER_IRODORI_TIMEOUT_MS` | total budget for one synthesis, default 12000 |
//! | `AIVTUBER_IRODORI_AMPLITUDE_VISEMES` | derive a mouth track from the audio, default off |

use crate::CompositionProfile;
use crate::voicevox_bridge::{TTS_BACKEND_VAR, start_loopback_bridge};
use aivtuber_adapters::speech_tts::irodori::{
    self, IrodoriConfig, IrodoriConfigError, IrodoriSynthesizer,
};
use aivtuber_adapters::speech_tts::server::ServerHandle;
use std::path::Path;
use std::time::Duration;

pub const IRODORI_ENDPOINT_VAR: &str = "AIVTUBER_IRODORI_ENDPOINT";
pub const IRODORI_SEED_VAR: &str = "AIVTUBER_IRODORI_SEED";
pub const IRODORI_VOICE_VAR: &str = "AIVTUBER_IRODORI_VOICE";
pub const IRODORI_MODEL_VAR: &str = "AIVTUBER_IRODORI_MODEL";
pub const IRODORI_LABEL_VAR: &str = "AIVTUBER_IRODORI_MODEL_LABEL";
pub const IRODORI_TIMEOUT_VAR: &str = "AIVTUBER_IRODORI_TIMEOUT_MS";
pub const IRODORI_VISEMES_VAR: &str = "AIVTUBER_IRODORI_AMPLITUDE_VISEMES";

/// A cold server loads the model on its first request unless the operator set
/// `IRODORI_PRELOAD=true`; LUNA on a GPU answers a reply in about a second.
const DEFAULT_TIMEOUT_MS: u64 = 12_000;

/// Whether `AIVTUBER_TTS_BACKEND` (already trimmed and lower-cased) names this
/// backend. The VOICEVOX settings use it to stay out of the way.
pub(crate) fn is_irodori_backend_name(name: &str) -> bool {
    matches!(name, "irodori" | "irodori-tts")
}

/// A typed startup error: every misconfiguration names the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrodoriSettingsError {
    NotLocalProfile,
    GenerativeDisabled,
    /// Another TTS selection is also set.
    Conflict(&'static str),
    MissingEndpoint,
    InvalidSeed(String),
    InvalidTimeout(String),
    InvalidFlag(&'static str),
    Config(&'static str, IrodoriConfigError),
}

impl std::fmt::Display for IrodoriSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocalProfile => write!(
                f,
                "{TTS_BACKEND_VAR}=irodori is supported with AIVTUBER_MODE=local or hybrid only; start a bridge yourself and set AIVTUBER_TTS_ENDPOINT for other profiles"
            ),
            Self::GenerativeDisabled => write!(
                f,
                "{TTS_BACKEND_VAR}=irodori requires AIVTUBER_GENERATIVE_ENABLED=true"
            ),
            Self::Conflict(name) => write!(
                f,
                "{name} must not be set with {TTS_BACKEND_VAR}=irodori: the bridge supplies the TTS endpoint"
            ),
            Self::MissingEndpoint => write!(
                f,
                "{IRODORI_ENDPOINT_VAR} is required with {TTS_BACKEND_VAR}=irodori (the Irodori-TTS-Server origin, e.g. http://127.0.0.1:8088)"
            ),
            Self::InvalidSeed(value) => write!(
                f,
                "{IRODORI_SEED_VAR} must be an integer from 0 to 4294967295 (got {value:?})"
            ),
            Self::InvalidTimeout(value) => write!(
                f,
                "{IRODORI_TIMEOUT_VAR} must be an integer from 1 to 120000 (got {value:?})"
            ),
            Self::InvalidFlag(name) => {
                write!(f, "{name} must be true/false, 1/0, yes/no, or on/off")
            }
            Self::Config(name, error) => write!(f, "{name}: {error}"),
        }
    }
}

impl std::error::Error for IrodoriSettingsError {}

/// The operator's Irodori-TTS settings, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrodoriSettings {
    pub config: IrodoriConfig,
    /// Total budget for one synthesis.
    pub synth_timeout: Duration,
    pub amplitude_visemes: bool,
}

impl IrodoriSettings {
    /// `Ok(None)` unless `AIVTUBER_TTS_BACKEND` is `irodori` or `irodori-tts`.
    /// `get` returns a variable's non-blank value.
    pub fn resolve(
        get: &dyn Fn(&str) -> Option<String>,
        profile: CompositionProfile,
        generative_enabled: bool,
    ) -> Result<Option<Self>, IrodoriSettingsError> {
        let selected = get(TTS_BACKEND_VAR)
            .map(|value| value.trim().to_ascii_lowercase())
            .is_some_and(|name| is_irodori_backend_name(&name));
        if !selected {
            // Unknown names are rejected by the VOICEVOX resolver, which owns
            // the selector's vocabulary.
            return Ok(None);
        }
        if !profile.local_stack() {
            return Err(IrodoriSettingsError::NotLocalProfile);
        }
        if !generative_enabled {
            return Err(IrodoriSettingsError::GenerativeDisabled);
        }
        for name in ["AIVTUBER_TTS_SIDECAR", "AIVTUBER_TTS_ENDPOINT"] {
            if get(name).is_some() {
                return Err(IrodoriSettingsError::Conflict(name));
            }
        }
        let endpoint = get(IRODORI_ENDPOINT_VAR).ok_or(IrodoriSettingsError::MissingEndpoint)?;
        let mut config = IrodoriConfig::new(endpoint.trim());
        if let Some(seed) = get(IRODORI_SEED_VAR) {
            config.seed = seed
                .trim()
                .parse::<u32>()
                .map_err(|_| IrodoriSettingsError::InvalidSeed(seed))?;
        }
        config.voice = get(IRODORI_VOICE_VAR).map(|v| v.trim().to_owned());
        if let Some(model) = get(IRODORI_MODEL_VAR) {
            config.model_name = model.trim().to_owned();
        }
        config.model_label = get(IRODORI_LABEL_VAR).map(|v| v.trim().to_owned());
        let synth_timeout_ms = match get(IRODORI_TIMEOUT_VAR) {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| (1..=120_000).contains(ms))
                .ok_or(IrodoriSettingsError::InvalidTimeout(value))?,
        };
        let amplitude_visemes = match get(IRODORI_VISEMES_VAR)
            .map(|v| v.to_ascii_lowercase())
            .as_deref()
        {
            None | Some("0" | "false" | "no" | "off") => false,
            Some("1" | "true" | "yes" | "on") => true,
            Some(_) => return Err(IrodoriSettingsError::InvalidFlag(IRODORI_VISEMES_VAR)),
        };
        config.validate().map_err(|error| {
            let name = match error {
                IrodoriConfigError::Endpoint(_) | IrodoriConfigError::Bound(_) => {
                    IRODORI_ENDPOINT_VAR
                }
                IrodoriConfigError::Name("voice name") => IRODORI_VOICE_VAR,
                IrodoriConfigError::Name("model label") => IRODORI_LABEL_VAR,
                IrodoriConfigError::Name(_) => IRODORI_MODEL_VAR,
            };
            IrodoriSettingsError::Config(name, error)
        })?;
        Ok(Some(Self {
            config,
            synth_timeout: Duration::from_millis(synth_timeout_ms),
            amplitude_visemes,
        }))
    }

    /// `model_alias` recorded in TTS provenance. The server's revision and the
    /// checkpoint are not probed, so there is no `model_version`.
    pub fn model_alias(&self) -> String {
        match &self.config.model_label {
            Some(label) => format!("{label}-seed-{}", self.config.seed),
            None => format!("seed-{}", self.config.seed),
        }
    }
}

/// The running in-process bridge. Dropping it stops the listener.
pub struct IrodoriBridge {
    handle: ServerHandle,
    tts_endpoint: String,
    settings: IrodoriSettings,
}

impl IrodoriBridge {
    /// Bind the bridge on an ephemeral loopback port. A failure here is a
    /// startup error; a server that is down is not (every synthesis then fails
    /// like any TTS failure and the reply degrades visibly).
    pub fn start(
        settings: IrodoriSettings,
        audio_root: &Path,
        pack_root: &Path,
    ) -> Result<Self, String> {
        let synth = IrodoriSynthesizer::new(&settings.config).map_err(|e| e.to_string())?;
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

    pub fn settings(&self) -> &IrodoriSettings {
        &self.settings
    }

    pub fn stats(&self) -> aivtuber_adapters::speech_tts::server::ServerStats {
        self.handle.stats()
    }

    /// Non-secret startup lines: where the server is, the pinned voice, and the
    /// experimental notice. The spoken text never appears.
    pub fn startup_lines(&self) -> Vec<String> {
        vec![
            format!(
                "irodori: server={} seed={} voice={} bridge={} visemes={} (the server is a separate process; synthesis fails like any TTS failure while it is down)",
                aivtuber_adapters::endpoint_origin(&self.settings.config.endpoint),
                self.settings.config.seed,
                self.settings
                    .config
                    .voice
                    .as_deref()
                    .unwrap_or("none (unconditioned)"),
                aivtuber_adapters::endpoint_origin(&self.tts_endpoint),
                if self.settings.amplitude_visemes {
                    "amplitude"
                } else {
                    "off"
                },
            ),
            format!("irodori: {}", irodori::EXPERIMENTAL_NOTICE),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const BASE: [(&str, &str); 2] = [
        (TTS_BACKEND_VAR, "irodori"),
        (IRODORI_ENDPOINT_VAR, "http://127.0.0.1:8088"),
    ];

    fn resolve(pairs: &[(&str, &str)]) -> Result<Option<IrodoriSettings>, IrodoriSettingsError> {
        IrodoriSettings::resolve(&lookup(pairs), CompositionProfile::Local, true)
    }

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        BASE.iter().chain(extra).copied().collect()
    }

    #[test]
    fn another_backend_or_none_selects_nothing() {
        assert_eq!(resolve(&[]), Ok(None));
        assert_eq!(resolve(&[(TTS_BACKEND_VAR, "http")]), Ok(None));
        assert_eq!(resolve(&[(TTS_BACKEND_VAR, "voicevox")]), Ok(None));
        // Unrelated variables without the selector do nothing.
        assert_eq!(resolve(&[(IRODORI_SEED_VAR, "5")]), Ok(None));
        // The VOICEVOX resolver, which owns the vocabulary, stays out of the way.
        for name in ["irodori", "irodori-tts", " Irodori "] {
            assert_eq!(
                crate::VoicevoxSettings::resolve(
                    &lookup(&[(TTS_BACKEND_VAR, name)]),
                    CompositionProfile::Local,
                    true
                ),
                Ok(None),
                "{name}"
            );
        }
    }

    #[test]
    fn defaults_are_the_measured_seed_and_the_unconditioned_voice() {
        let settings = resolve(&BASE).unwrap().unwrap();
        assert_eq!(settings.config.seed, irodori::DEFAULT_SEED);
        assert_eq!(settings.config.voice, None);
        assert_eq!(settings.config.model_name, "irodori-tts");
        assert_eq!(settings.synth_timeout, Duration::from_millis(12_000));
        assert!(!settings.amplitude_visemes);
        assert_eq!(settings.model_alias(), "seed-1234");
        assert!(
            resolve(&with(&[(TTS_BACKEND_VAR, "irodori-tts")]))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn every_variable_is_applied() {
        let settings = resolve(&with(&[
            (IRODORI_SEED_VAR, "42"),
            (IRODORI_VOICE_VAR, "my-voice"),
            (IRODORI_MODEL_VAR, "irodori-tts-luna"),
            (IRODORI_LABEL_VAR, "LUNA-bf16"),
            (IRODORI_TIMEOUT_VAR, "9000"),
            (IRODORI_VISEMES_VAR, "on"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(settings.config.seed, 42);
        assert_eq!(settings.config.voice.as_deref(), Some("my-voice"));
        assert_eq!(settings.config.model_name, "irodori-tts-luna");
        assert_eq!(settings.synth_timeout, Duration::from_millis(9_000));
        assert!(settings.amplitude_visemes);
        assert_eq!(settings.model_alias(), "LUNA-bf16-seed-42");
    }

    #[test]
    fn misconfiguration_is_a_typed_error_naming_the_variable() {
        assert_eq!(
            IrodoriSettings::resolve(&lookup(&BASE), CompositionProfile::Cached, true),
            Err(IrodoriSettingsError::NotLocalProfile)
        );
        assert_eq!(
            IrodoriSettings::resolve(&lookup(&BASE), CompositionProfile::Local, false),
            Err(IrodoriSettingsError::GenerativeDisabled)
        );
        assert_eq!(
            resolve(&[(TTS_BACKEND_VAR, "irodori")]),
            Err(IrodoriSettingsError::MissingEndpoint)
        );
        assert_eq!(
            resolve(&with(&[("AIVTUBER_TTS_ENDPOINT", "http://127.0.0.1:1")])),
            Err(IrodoriSettingsError::Conflict("AIVTUBER_TTS_ENDPOINT"))
        );
        for bad in ["-1", "abc", "4294967296", "1.5"] {
            assert!(
                matches!(
                    resolve(&with(&[(IRODORI_SEED_VAR, bad)])),
                    Err(IrodoriSettingsError::InvalidSeed(_))
                ),
                "{bad}"
            );
        }
        for bad in ["0", "120001", "x"] {
            assert!(
                matches!(
                    resolve(&with(&[(IRODORI_TIMEOUT_VAR, bad)])),
                    Err(IrodoriSettingsError::InvalidTimeout(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            resolve(&with(&[(IRODORI_VISEMES_VAR, "maybe")])),
            Err(IrodoriSettingsError::InvalidFlag(IRODORI_VISEMES_VAR))
        );
        assert!(matches!(
            resolve(&with(&[(IRODORI_VOICE_VAR, "a/b")])),
            Err(IrodoriSettingsError::Config(IRODORI_VOICE_VAR, _))
        ));
    }

    #[test]
    fn a_non_loopback_endpoint_is_refused_at_start() {
        for bad in [
            "http://192.168.1.4:8088",
            "http://tts.example:8088",
            "https://127.0.0.1:8088",
            "http://0.0.0.0:8088",
        ] {
            let error = resolve(&[(TTS_BACKEND_VAR, "irodori"), (IRODORI_ENDPOINT_VAR, bad)])
                .expect_err(bad);
            assert!(
                matches!(error, IrodoriSettingsError::Config(IRODORI_ENDPOINT_VAR, _)),
                "{bad}: {error:?}"
            );
            assert!(error.to_string().contains(IRODORI_ENDPOINT_VAR));
        }
    }

    #[test]
    fn the_bridge_binds_loopback_and_prints_the_experimental_line() {
        let root = std::env::temp_dir().join(format!("aivtuber-411-bridge-{}", std::process::id()));
        let settings = resolve(&BASE).unwrap().unwrap();
        let bridge = IrodoriBridge::start(settings, &root.join("audio"), &root).unwrap();
        assert!(bridge.tts_endpoint().starts_with("http://127.0.0.1:"));
        let lines = bridge.startup_lines().join("\n");
        assert!(
            lines.contains("EXPERIMENTAL") && lines.contains("実験的"),
            "{lines}"
        );
        assert!(lines.contains("seed=1234"), "{lines}");
        assert!(
            lines.contains("13%") && lines.contains("EXP-0006"),
            "{lines}"
        );
        drop(bridge);
        let _ = std::fs::remove_dir_all(&root);
    }
}
