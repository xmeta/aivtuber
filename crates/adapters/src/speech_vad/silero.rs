use crate::speech_vad::pipeline::{FRAME_SAMPLES, FrameScorer};
use sha2::{Digest, Sha256};
use std::path::Path;
use tract_onnx::prelude::*;

/// Pinned Silero VAD v6 model: snakers4/silero-vad tag v6.2.3 (MIT), the
/// `silero_vad_op18_ifless.onnx` export. That export has the 16 kHz path only
/// and no ONNX `If` node, which tract cannot analyse in the standard
/// `silero_vad.onnx` (measured; see the S2 section of docs/production-runtime.adoc). Frame scores
/// match onnxruntime running the standard v6 model to within 1e-6.
pub const SILERO_MODEL_URL: &str = "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad_op18_ifless.onnx";
pub const SILERO_MODEL_SHA256: &str =
    "7671cd04b004e9076da0d4a7b1a5aec36adf161c39230c1cb94a4fd5db6bbd28";
/// Recorded as `provider_version`: model release plus the inference runtime,
/// because both can change scores.
pub const SILERO_PROVIDER_VERSION: &str = "silero-vad-v6.2.3-ifless-tract-0.23.10";

const MAX_MODEL_BYTES: u64 = 16 * 1024 * 1024;
const CONTEXT_SAMPLES: usize = 64;
const STATE_LEN: usize = 2 * 128;

type Plan = std::sync::Arc<TypedRunnableModel>;

/// Silero VAD v6 through tract, one 512-sample frame per call (EXP-0001
/// streaming layout: 64 samples of context, recurrent state 2x1x128).
pub struct SileroVad {
    plan: Plan,
    state: Vec<f32>,
    context: [f32; CONTEXT_SAMPLES],
}

pub fn verify_model_bytes(bytes: &[u8]) -> Result<(), String> {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if hex == SILERO_MODEL_SHA256 {
        Ok(())
    } else {
        Err(format!(
            "model sha256 {hex} does not match the pinned Silero v6.2.3 ifless export {SILERO_MODEL_SHA256}"
        ))
    }
}

impl SileroVad {
    /// Loads the pinned model. The file's SHA-256 must match: two Silero
    /// versions behaved materially differently in EXP-0001, so an unpinned
    /// model is refused rather than run.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("model {}: {e}", path.display()))?;
        if meta.len() > MAX_MODEL_BYTES {
            return Err("model file is larger than 16 MiB".to_owned());
        }
        let bytes = std::fs::read(path).map_err(|e| format!("model {}: {e}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        verify_model_bytes(bytes)?;
        let err = |e: TractError| format!("tract: {e:#}");
        let mut model = tract_onnx::onnx()
            // The export carries symbolic `batch` dims in value_info that
            // contradict the concrete input facts set below.
            .with_ignore_value_info(true)
            .model_for_read(&mut std::io::Cursor::new(bytes))
            .map_err(err)?;
        let names: Vec<String> = model
            .input_outlets()
            .map_err(err)?
            .iter()
            .map(|o| model.node(o.node).name.clone())
            .collect();
        if names != ["input", "sr", "state"] {
            return Err(format!("unexpected model inputs {names:?}"));
        }
        model
            .set_input_fact(
                0,
                f32::fact([1usize, FRAME_SAMPLES + CONTEXT_SAMPLES]).into(),
            )
            .map_err(err)?;
        model
            .set_input_fact(1, InferenceFact::from(tensor0(16_000_i64)))
            .map_err(err)?;
        model
            .set_input_fact(2, f32::fact([2usize, 1, 128]).into())
            .map_err(err)?;
        let plan = model
            .into_optimized()
            .map_err(err)?
            .into_runnable()
            .map_err(err)?;
        Ok(Self {
            plan,
            state: vec![0.0; STATE_LEN],
            context: [0.0; CONTEXT_SAMPLES],
        })
    }
}

impl FrameScorer for SileroVad {
    fn score(&mut self, frame: &[f32]) -> Result<f32, String> {
        if frame.len() != FRAME_SAMPLES {
            return Err(format!(
                "expected {FRAME_SAMPLES} samples, got {}",
                frame.len()
            ));
        }
        let err = |e: TractError| format!("tract: {e:#}");
        let mut input = Vec::with_capacity(CONTEXT_SAMPLES + FRAME_SAMPLES);
        input.extend_from_slice(&self.context);
        input.extend_from_slice(frame);
        let x = Tensor::from_shape(&[1, input.len()], &input).map_err(err)?;
        let state = Tensor::from_shape(&[2, 1, 128], &self.state).map_err(err)?;
        let out = self
            .plan
            .run(tvec!(
                x.into_tvalue(),
                tensor0(16_000_i64).into_tvalue(),
                state.into_tvalue()
            ))
            .map_err(err)?;
        let probability = *out[0]
            .try_as_plain_ram()
            .map_err(err)?
            .as_slice::<f32>()
            .map_err(err)?
            .first()
            .ok_or("empty output")?;
        let next = out[1]
            .try_as_plain_ram()
            .map_err(err)?
            .as_slice::<f32>()
            .map_err(err)?
            .to_vec();
        if next.len() != STATE_LEN {
            return Err("unexpected state size".to_owned());
        }
        self.state = next;
        self.context
            .copy_from_slice(&input[input.len() - CONTEXT_SAMPLES..]);
        Ok(probability)
    }

    fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
    }
}
