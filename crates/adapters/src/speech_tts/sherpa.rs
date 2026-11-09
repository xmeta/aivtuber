//! sherpa-onnx offline TTS (Supertonic 3 int8) as a [`Synthesizer`] (#220 S2b).
//!
//! Wraps the official `sherpa-onnx` crate (2.0.1, Apache-2.0) behind the
//! off-by-default `sherpa-tts` feature, the same crate the `sherpa-stt` feature
//! already depends on. The crate is reused as is: model pinning, voice choice,
//! the language and seed, deadlines, bounds and the file contract stay
//! project-owned. Every model file is checked against its compiled-in SHA-256
//! before the engine is built, so an unpinned model is refused rather than run.

use super::{GenerationSettings, Pcm, SynthError, Synthesizer};
use crate::speech_stt::pins::{ModelPin, bundle_sha256, verify_model_dir};
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsModelConfig,
    OfflineTtsSupertonicModelConfig,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

pub struct SherpaSynthesizer {
    tts: OfflineTts,
    bundle: String,
    sample_rate: u32,
}

fn path_str(dir: &Path, file_name: &str) -> Result<String, String> {
    dir.join(file_name)
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "model path is not valid UTF-8".to_owned())
}

impl SherpaSynthesizer {
    /// Verify the pinned files in `dir` and load the engine on CPU with
    /// `threads` threads (4 measured, EXP-0003).
    pub fn load(pin: &ModelPin, dir: &Path, threads: i32) -> Result<Self, String> {
        verify_model_dir(pin, dir)?;
        if !(1..=16).contains(&threads) {
            return Err("threads must be 1..=16".to_owned());
        }
        let file = |role: &str| -> Result<Option<String>, String> {
            let pinned = pin
                .files
                .iter()
                .find(|f| f.role == role)
                .ok_or_else(|| format!("model pin has no {role}"))?;
            path_str(dir, pinned.file_name).map(Some)
        };
        let config = OfflineTtsConfig {
            model: OfflineTtsModelConfig {
                supertonic: OfflineTtsSupertonicModelConfig {
                    duration_predictor: file("duration_predictor")?,
                    text_encoder: file("text_encoder")?,
                    vector_estimator: file("vector_estimator")?,
                    vocoder: file("vocoder")?,
                    tts_json: file("tts_json")?,
                    unicode_indexer: file("unicode_indexer")?,
                    voice_style: file("voice_style")?,
                },
                num_threads: threads,
                debug: false,
                provider: Some("cpu".to_owned()),
                ..OfflineTtsModelConfig::default()
            },
            max_num_sentences: 1,
            ..OfflineTtsConfig::default()
        };
        let tts = OfflineTts::create(&config)
            .ok_or_else(|| "sherpa-onnx could not create the TTS engine".to_owned())?;
        let sample_rate = u32::try_from(tts.sample_rate())
            .ok()
            .filter(|rate| *rate > 0)
            .ok_or_else(|| "the engine reported no sample rate".to_owned())?;
        Ok(Self {
            tts,
            bundle: bundle_sha256(pin),
            sample_rate,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn num_speakers(&self) -> i32 {
        self.tts.num_speakers()
    }
}

/// Map explicit settings to the engine's per-request configuration. The `extra`
/// map always carries `lang` and `seed`: without them the engine speaks
/// English with a fresh random seed (EXP-0003 5.3).
pub fn generation_config(settings: &GenerationSettings) -> GenerationConfig {
    let extra: HashMap<String, serde_json::Value> = settings
        .extra()
        .into_iter()
        .map(|(key, value)| (key.to_owned(), serde_json::Value::String(value)))
        .collect();
    GenerationConfig {
        sid: settings.voice().speaker_id,
        speed: settings.voice().speed,
        num_steps: settings.voice().num_steps,
        extra: Some(extra),
        ..GenerationConfig::default()
    }
}

impl Synthesizer for SherpaSynthesizer {
    fn synthesize(
        &self,
        text: &str,
        settings: &GenerationSettings,
        deadline: Instant,
    ) -> Result<Pcm, SynthError> {
        // A NUL would panic the crate's C string conversion.
        if text.contains('\0') {
            return Err(SynthError::Engine("text contains NUL".to_owned()));
        }
        let config = generation_config(settings);
        // The engine calls back between chunks; stopping there bounds a long
        // text, while the server's own deadline answers the request on time.
        let callback = move |_samples: &[f32], _progress: f32| Instant::now() < deadline;
        let audio = self
            .tts
            .generate_with_config(text, &config, Some(callback))
            .ok_or_else(|| {
                if Instant::now() >= deadline {
                    SynthError::DeadlineExceeded
                } else {
                    SynthError::Engine("the engine returned no audio".to_owned())
                }
            })?;
        if Instant::now() > deadline {
            return Err(SynthError::DeadlineExceeded);
        }
        let rate = u32::try_from(audio.sample_rate())
            .ok()
            .filter(|rate| *rate > 0)
            .unwrap_or(self.sample_rate);
        Ok(Pcm {
            samples: audio.samples().to_vec(),
            sample_rate: rate,
        })
    }

    fn bundle_sha256(&self) -> &str {
        &self.bundle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech_tts::VoiceConfig;

    /// The engine defaults are language `en` and a random seed; the mapping to
    /// the engine's configuration must never leave either unset.
    #[test]
    fn the_engine_configuration_always_carries_language_and_seed() {
        let settings = GenerationSettings::for_text(
            "こんにちは",
            VoiceConfig {
                speaker_id: 2,
                num_steps: 7,
                speed: 1.1,
            },
        );
        let config = generation_config(&settings);
        let extra = config.extra.expect("extra is always set");
        assert_eq!(extra["lang"], serde_json::json!("ja"));
        let seed: i32 = extra["seed"].as_str().unwrap().parse().unwrap();
        assert!(seed >= 0, "a negative seed means a fresh random one");
        assert_eq!(seed, settings.seed());
        assert_eq!(config.sid, 2);
        assert_eq!(config.num_steps, 7);
        assert!((config.speed - 1.1).abs() < f32::EPSILON);
        assert_eq!(extra.len(), 2);
    }
}
