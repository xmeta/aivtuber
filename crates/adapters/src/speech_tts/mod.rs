//! Local Japanese text-to-speech building blocks (issue #220, slice S2b).
//!
//! `local-tts-sidecar` is a loopback HTTP server that speaks the *existing*
//! normalized TTS contract (the request and response bodies
//! `NormalizedHttpTtsAdapter` already sends and parses), so the production
//! adapter stays the client and no second wire contract exists. It runs one
//! Supertonic 3 int8 model through the official `sherpa-onnx` crate (the one the
//! `sherpa-stt` feature already depends on), writes 16-bit PCM WAV files under
//! `AIVTUBER_AUDIO_ROOT/generated/` and answers
//! `{"audio_ref":"audio://generated/<digest>.wav","duration_ms":N}`.
//!
//! Invariants of this module tree:
//! * Loopback only: the server refuses to bind a non-loopback address and
//!   checks the `Host` header, and requires `Content-Type: application/json`.
//! * Language and seed are always explicit ([`GenerationSettings`]): the
//!   engine's own defaults (`en`, a fresh random seed) cannot reach it, because
//!   the only constructor sets both.
//! * Voice selection is configuration ([`VoiceConfig`]), never request or
//!   model output. The request's `voice_model` and `style` are accepted and
//!   ignored; the response carries `X-TTS-Style-Applied: false`.
//! * Bounded everywhere: request bytes, text length, connections, one synthesis
//!   at a time (a busy server answers 503, which the adapter maps to
//!   `Overloaded`), a synthesis deadline, and a byte cap on the generated-audio
//!   directory.
//! * The sidecar never sees a URL or an emoji: the runtime projects display
//!   text to speech text before the TTS request (ADR-0004 D-5).
//! * Nothing is downloaded at build or run time; the model files and the engine
//!   libraries must match SHA-256 pins ([`pins`], `speech_stt::pins`).

pub mod envelope;
pub mod pins;
pub mod runner;
pub mod server;
#[cfg(feature = "sherpa-tts")]
pub mod sherpa;
pub mod supervisor;
pub mod wav;

use sha2::{Digest, Sha256};
use std::time::Instant;

/// The only language this sidecar speaks. It is passed to the engine on every
/// call: sherpa-onnx's Supertonic defaults to `en` when it is absent
/// (EXP-0003 5.3, 15 % worse reading error on Japanese).
pub const LANGUAGE: &str = "ja";
/// `backend_name` the supervised app records in TTS provenance.
pub const BACKEND_NAME: &str = "local-tts-sidecar";

/// Operator configuration of the voice. Never taken from a request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceConfig {
    /// Supertonic voice index (the model ships 10).
    pub speaker_id: i32,
    /// Denoising steps; 5 is the measured default.
    pub num_steps: i32,
    pub speed: f32,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            speaker_id: 0,
            num_steps: 5,
            speed: 1.0,
        }
    }
}

impl VoiceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(0..=9).contains(&self.speaker_id) {
            return Err("speaker id must be 0..=9".to_owned());
        }
        if !(1..=16).contains(&self.num_steps) {
            return Err("num steps must be 1..=16".to_owned());
        }
        if !self.speed.is_finite() || !(0.5..=2.0).contains(&self.speed) {
            return Err("speed must be 0.5..=2.0".to_owned());
        }
        Ok(())
    }
}

/// Everything that decides the audio for a text, fully explicit. The only
/// constructor is [`GenerationSettings::for_text`], which fixes the language to
/// [`LANGUAGE`] and derives the seed from the text, so an engine default can
/// never be the effective value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenerationSettings {
    lang: &'static str,
    seed: i32,
    voice: VoiceConfig,
}

impl GenerationSettings {
    pub fn for_text(text: &str, voice: VoiceConfig) -> Self {
        Self {
            lang: LANGUAGE,
            seed: text_seed(text),
            voice,
        }
    }

    pub fn lang(&self) -> &'static str {
        self.lang
    }

    /// Non-negative: a negative seed means "random" to the engine.
    pub fn seed(&self) -> i32 {
        self.seed
    }

    pub fn voice(&self) -> VoiceConfig {
        self.voice
    }

    /// The engine's `extra` map: language and seed. Both as strings, the form
    /// the Python wheel was measured with and the Rust crate reproduces
    /// bit-identically (EXP-0003 section 7).
    pub fn extra(&self) -> [(&'static str, String); 2] {
        [
            ("lang", self.lang.to_owned()),
            ("seed", self.seed.to_string()),
        ]
    }
}

/// Deterministic seed from the text alone: the same reply is the same audio on
/// any host and in any process, and no random seed is ever used.
pub fn text_seed(text: &str) -> i32 {
    let digest = Sha256::digest(text.as_bytes());
    let value = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
    (value & 0x7fff_ffff) as i32
}

/// Content address of one synthesis: any change to the text, language, seed,
/// voice, steps, speed or model bundle is a different file, so a cache hit is
/// always the same audio and replay/cache keys are stable.
pub fn content_digest(text: &str, settings: &GenerationSettings, bundle_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"aivtuber-local-tts-v1\0");
    hasher.update(text.as_bytes());
    hasher.update([0]);
    hasher.update(settings.lang.as_bytes());
    hasher.update([0]);
    hasher.update(settings.seed.to_le_bytes());
    hasher.update(settings.voice.speaker_id.to_le_bytes());
    hasher.update(settings.voice.num_steps.to_le_bytes());
    hasher.update(settings.voice.speed.to_bits().to_le_bytes());
    hasher.update(bundle_sha256.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Audio from an engine: mono float samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynthError {
    /// The synthesis was stopped or finished after its deadline.
    DeadlineExceeded,
    Engine(String),
}

/// What the server needs from an engine. `deadline` is advisory for the engine
/// (the real one stops between chunks once it passes); the server enforces it
/// regardless.
pub trait Synthesizer: Send + Sync + 'static {
    fn synthesize(
        &self,
        text: &str,
        settings: &GenerationSettings,
        deadline: Instant,
    ) -> Result<Pcm, SynthError>;

    /// `SHA-256` bundle digest of the pinned model, part of every content
    /// address and recorded as the model version.
    fn bundle_sha256(&self) -> &str;
}

/// Model-free synthesizer: a quiet tone whose length follows the text (100 ms
/// per character), for tests and the supervised-child double. It speaks
/// nothing and must never be used as a voice.
#[derive(Debug, Clone)]
pub struct ToneSynthesizer {
    pub sample_rate: u32,
    pub delay: std::time::Duration,
    pub fail: bool,
}

impl Default for ToneSynthesizer {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            delay: std::time::Duration::ZERO,
            fail: false,
        }
    }
}

impl Synthesizer for ToneSynthesizer {
    fn synthesize(
        &self,
        text: &str,
        _settings: &GenerationSettings,
        _deadline: Instant,
    ) -> Result<Pcm, SynthError> {
        std::thread::sleep(self.delay);
        if self.fail {
            return Err(SynthError::Engine(
                "tone synthesizer set to fail".to_owned(),
            ));
        }
        let samples_per_char = (self.sample_rate / 10) as usize;
        let count = text.chars().count().max(1) * samples_per_char;
        let samples = (0..count).map(|i| 0.2 * (i as f32 * 0.05).sin()).collect();
        Ok(Pcm {
            samples,
            sample_rate: self.sample_rate,
        })
    }

    fn bundle_sha256(&self) -> &str {
        "tone-synthesizer-test-double"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_and_seed_are_always_explicit_never_the_engine_default() {
        // The engine defaults are `en` and a random (negative) seed. Neither
        // can be the effective value: the only constructor fixes `ja` and a
        // text-derived non-negative seed.
        for text in ["", "こんにちは", "ありがとう😊", "a", &"あ".repeat(500)] {
            let settings = GenerationSettings::for_text(text, VoiceConfig::default());
            assert_eq!(settings.lang(), "ja");
            assert!(settings.seed() >= 0, "negative seed means random");
            let extra = settings.extra();
            assert_eq!(extra[0], ("lang", "ja".to_owned()));
            assert_eq!(extra[1].0, "seed");
            assert_eq!(extra[1].1, settings.seed().to_string());
            assert!(extra[1].1.parse::<i32>().unwrap() >= 0);
        }
    }

    #[test]
    fn the_seed_is_text_derived_and_repeatable() {
        assert_eq!(text_seed("こんにちは"), text_seed("こんにちは"));
        assert_ne!(text_seed("こんにちは"), text_seed("こんばんは"));
    }

    #[test]
    fn the_content_digest_tracks_every_input() {
        let voice = VoiceConfig::default();
        let base = content_digest(
            "やあ",
            &GenerationSettings::for_text("やあ", voice),
            "bundle",
        );
        assert_eq!(base.len(), 64);
        let same = content_digest(
            "やあ",
            &GenerationSettings::for_text("やあ", voice),
            "bundle",
        );
        assert_eq!(base, same);
        let other_text = content_digest(
            "やあ!",
            &GenerationSettings::for_text("やあ!", voice),
            "bundle",
        );
        let other_bundle = content_digest(
            "やあ",
            &GenerationSettings::for_text("やあ", voice),
            "other",
        );
        let other_voice = content_digest(
            "やあ",
            &GenerationSettings::for_text(
                "やあ",
                VoiceConfig {
                    speaker_id: 3,
                    ..voice
                },
            ),
            "bundle",
        );
        let other_speed = content_digest(
            "やあ",
            &GenerationSettings::for_text(
                "やあ",
                VoiceConfig {
                    speed: 1.2,
                    ..voice
                },
            ),
            "bundle",
        );
        for other in [other_text, other_bundle, other_voice, other_speed] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn voice_config_bounds() {
        assert!(VoiceConfig::default().validate().is_ok());
        for bad in [
            VoiceConfig {
                speaker_id: -1,
                ..VoiceConfig::default()
            },
            VoiceConfig {
                speaker_id: 10,
                ..VoiceConfig::default()
            },
            VoiceConfig {
                num_steps: 0,
                ..VoiceConfig::default()
            },
            VoiceConfig {
                speed: f32::NAN,
                ..VoiceConfig::default()
            },
            VoiceConfig {
                speed: 3.0,
                ..VoiceConfig::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
