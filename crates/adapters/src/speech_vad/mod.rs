//! Local speech-activity sidecar building blocks (issue #92, slice S2).
//!
//! A microphone (or a WAV file) is resampled to 16 kHz mono, cut into 512
//! sample (32 ms) frames, scored by a [`FrameScorer`] (Silero VAD v6 in the
//! shipped binary) and turned into *raw* speech-activity edges. The sidecar
//! binary stamps those edges on the runtime's timeline and writes
//! `speech-observation` batches (schemas/speech-observation.schema.json) as
//! JSON lines on stdout.
//!
//! Privacy invariants of this module tree:
//! * Audio stays in process memory. Nothing here opens a file for writing or a
//!   socket, and no type in the emitted wire contract can carry samples.
//! * The only audio retained beyond the 512-sample frame being scored is the
//!   bounded [`PreRoll`] ring, the hook the S3 transcriber will read. It has a
//!   hard capacity, is never serialized and its `Debug` shows only lengths.
//! * Edges are raw (ADR-0005): there is no debounce or minimum-speech filter.
//!   The only state is the start/end hysteresis of the VAD probability.

mod clock;
mod edges;
mod emit;
#[cfg(feature = "mic-capture")]
pub mod mic;
mod pipeline;
mod preroll;
mod resample;
#[cfg(feature = "silero-vad")]
mod silero;

pub use clock::{
    CLOCK_SYNC_MAX_LINE_BYTES, ClockError, ClockPumpStats, RuntimeClockMap, StreamClock,
    encode_clock_sync_line, parse_clock_sync_line, pump_clock_syncs,
};
pub use edges::{EdgeDetector, HysteresisConfig};
pub use emit::{LineSink, ObservationStamper, SinkStats, StamperConfig};
pub use pipeline::{
    FRAME_MS, FRAME_SAMPLES, FrameScorer, PipelineStats, RawEdge, SAMPLE_RATE_HZ, VadPipeline,
};
pub use preroll::{MAX_PREROLL_MS, PreRoll};
pub use resample::Resampler;
#[cfg(feature = "silero-vad")]
pub use silero::{
    SILERO_MODEL_SHA256, SILERO_MODEL_URL, SILERO_PROVIDER_VERSION, SileroVad, verify_model_bytes,
};
