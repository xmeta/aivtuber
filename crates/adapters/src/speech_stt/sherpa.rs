//! sherpa-onnx offline recognizer as a [`Decoder`] (issue #92, S3b).
//!
//! Wraps the official `sherpa-onnx` crate (2.0.1, Apache-2.0) behind the
//! off-by-default `sherpa-stt` feature. The crate is reused as is: model
//! pinning, segmentation, partial cadence, deadlines, revision semantics,
//! trust labelling and replay stay project-owned. Every model file is checked
//! against its compiled-in SHA-256 before the recognizer is built, so an
//! unpinned model is refused rather than run.

use crate::speech_stt::pins::{ModelPin, PARAKEET_JA_INT8, verify_model_dir};
use crate::speech_stt::worker::{Decoder, WORKER_SAMPLE_RATE_HZ};
use sherpa_onnx::{
    OfflineNemoEncDecCtcModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineTransducerModelConfig,
};
use std::path::Path;

pub struct SherpaDecoder {
    recognizer: OfflineRecognizer,
}

fn path_str(dir: &Path, file_name: &str) -> Result<String, String> {
    dir.join(file_name)
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "model path is not valid UTF-8".to_owned())
}

impl SherpaDecoder {
    /// Verify the pinned files in `dir` and load the recognizer on CPU with
    /// `threads` threads (4 measured best on short segments; 1 still keeps up
    /// in real time, EXP-0002 6.1).
    pub fn load(pin: &ModelPin, dir: &Path, threads: i32) -> Result<Self, String> {
        verify_model_dir(pin, dir)?;
        if !(1..=16).contains(&threads) {
            return Err("threads must be 1..=16".to_owned());
        }
        let file = |role: &str| -> Result<String, String> {
            let pinned = pin
                .files
                .iter()
                .find(|f| f.role == role)
                .ok_or_else(|| format!("model pin has no {role}"))?;
            path_str(dir, pinned.file_name)
        };
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.tokens = Some(file("tokens")?);
        config.model_config.num_threads = threads;
        config.model_config.provider = Some("cpu".to_owned());
        config.model_config.debug = false;
        if *pin == PARAKEET_JA_INT8 {
            config.model_config.nemo_ctc = OfflineNemoEncDecCtcModelConfig {
                model: Some(file("model")?),
            };
        } else {
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: Some(file("encoder")?),
                decoder: Some(file("decoder")?),
                joiner: Some(file("joiner")?),
            };
            config.model_config.model_type = Some("transducer".to_owned());
            config.decoding_method = Some("greedy_search".to_owned());
        }
        let recognizer = OfflineRecognizer::create(&config)
            .ok_or_else(|| "sherpa-onnx could not create the recognizer".to_owned())?;
        Ok(Self { recognizer })
    }
}

impl Decoder for SherpaDecoder {
    fn decode(&mut self, samples: &[f32]) -> Result<String, String> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(WORKER_SAMPLE_RATE_HZ as i32, samples);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|result| result.text)
            .ok_or_else(|| "sherpa-onnx returned no result".to_owned())
    }
}
