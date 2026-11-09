use crate::speech_vad::edges::{EdgeDetector, HysteresisConfig};
use crate::speech_vad::preroll::PreRoll;
use crate::speech_vad::resample::Resampler;
use aivtuber_domain::SpeechEdge;

/// Model input rate. Fixed: Silero VAD v6 is run in its 16 kHz mode.
pub const SAMPLE_RATE_HZ: u32 = 16_000;
/// 512 samples per model step, as in EXP-0001.
pub const FRAME_SAMPLES: usize = 512;
pub const FRAME_MS: u64 = 32;

/// Scores one 512-sample 16 kHz frame with a speech probability in [0, 1].
pub trait FrameScorer {
    fn score(&mut self, frame: &[f32]) -> Result<f32, String>;
    /// Forget recurrent state (capture gap).
    fn reset(&mut self);
}

/// A raw activity edge on the 16 kHz stream timeline. `sample16` is the first
/// sample of the frame that produced it (or the current position for a forced
/// end). The caller maps it to the runtime timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawEdge {
    pub edge: SpeechEdge,
    pub sample16: u64,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineStats {
    pub frames_scored: u64,
    pub scorer_errors: u64,
    pub start_edges: u64,
    pub end_edges: u64,
    pub gaps: u64,
}

/// Mono input samples in, raw speech edges out. Holds at most one partial
/// frame, the resampler's kernel window and the bounded pre-roll.
pub struct VadPipeline<S: FrameScorer> {
    scorer: S,
    detector: EdgeDetector,
    resampler: Resampler,
    preroll: PreRoll,
    resampled: Vec<f32>,
    frame: Vec<f32>,
    /// 16 kHz samples already consumed into scored frames.
    scored_samples: u64,
    stats: PipelineStats,
}

impl<S: FrameScorer> VadPipeline<S> {
    pub fn new(
        scorer: S,
        input_rate_hz: u32,
        hysteresis: HysteresisConfig,
        preroll_ms: u64,
    ) -> Result<Self, String> {
        Ok(Self {
            scorer,
            detector: EdgeDetector::new(hysteresis)?,
            resampler: Resampler::new(input_rate_hz)?,
            preroll: PreRoll::new(preroll_ms)?,
            resampled: Vec::new(),
            frame: Vec::with_capacity(FRAME_SAMPLES * 2),
            scored_samples: 0,
            stats: PipelineStats::default(),
        })
    }

    pub fn stats(&self) -> PipelineStats {
        self.stats
    }

    /// Read hook for the S3 transcriber. Nothing in S2 consumes it.
    pub fn preroll(&self) -> &PreRoll {
        &self.preroll
    }

    /// Total 16 kHz samples consumed so far (the stream position).
    pub fn position16(&self) -> u64 {
        self.scored_samples + self.frame.len() as u64
    }

    pub fn push_input(&mut self, input: &[f32], edges: &mut Vec<RawEdge>) {
        self.resampled.clear();
        self.resampler.push(input, &mut self.resampled);
        self.preroll.push(&self.resampled);
        self.frame.extend_from_slice(&self.resampled);
        let mut consumed = 0;
        while self.frame.len() - consumed >= FRAME_SAMPLES {
            let frame = &self.frame[consumed..consumed + FRAME_SAMPLES];
            let start = self.scored_samples;
            match self.scorer.score(frame) {
                Ok(p) if p.is_finite() => {
                    self.stats.frames_scored += 1;
                    if let Some(edge) = self.detector.push(p) {
                        match edge {
                            SpeechEdge::Start => self.stats.start_edges += 1,
                            SpeechEdge::End => self.stats.end_edges += 1,
                        }
                        edges.push(RawEdge {
                            edge,
                            sample16: start,
                            confidence: Some(p.clamp(0.0, 1.0)),
                        });
                    }
                }
                // A failed or NaN score carries no information: no edge.
                _ => self.stats.scorer_errors += 1,
            }
            self.scored_samples += FRAME_SAMPLES as u64;
            consumed += FRAME_SAMPLES;
        }
        self.frame.drain(..consumed);
    }

    /// Capture dropped audio: close any open segment so the consumer is not
    /// left believing the speaker is still talking, and restart the stream
    /// (model state, resampler and partial frame) rather than splice across the
    /// gap. The pre-roll is cleared: it must stay contiguous.
    pub fn gap(&mut self, edges: &mut Vec<RawEdge>) {
        self.stats.gaps += 1;
        self.close_open_segment(edges);
        self.scorer.reset();
        self.resampler.reset();
        self.preroll.clear();
        self.frame.clear();
        // The stream restarts: positions are relative to the new origin.
        self.scored_samples = 0;
    }

    /// End of input: close an open segment at the current position.
    pub fn finish(&mut self, edges: &mut Vec<RawEdge>) {
        self.close_open_segment(edges);
    }

    fn close_open_segment(&mut self, edges: &mut Vec<RawEdge>) {
        if self.detector.force_end() {
            self.stats.end_edges += 1;
            edges.push(RawEdge {
                edge: SpeechEdge::End,
                sample16: self.position16(),
                confidence: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic stand-in for a model: loud frames are speech. Used only
    /// to test pipeline mechanics on synthetic signals; the real model is
    /// exercised by the `silero-vad` feature tests.
    #[derive(Default)]
    struct EnergyScorer {
        resets: u32,
    }

    impl FrameScorer for EnergyScorer {
        fn score(&mut self, frame: &[f32]) -> Result<f32, String> {
            assert_eq!(frame.len(), FRAME_SAMPLES);
            let rms = (frame.iter().map(|v| v * v).sum::<f32>() / frame.len() as f32).sqrt();
            Ok(if rms > 0.05 { 0.95 } else { 0.02 })
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    fn pipeline(rate: u32) -> VadPipeline<EnergyScorer> {
        VadPipeline::new(
            EnergyScorer::default(),
            rate,
            HysteresisConfig::default(),
            300,
        )
        .unwrap()
    }

    fn tone(rate: u32, ms: u64, amp: f32) -> Vec<f32> {
        (0..(u64::from(rate) * ms / 1000) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn silence_produces_no_edges_and_bounded_state() {
        let mut p = pipeline(16_000);
        let mut edges = Vec::new();
        for _ in 0..200 {
            p.push_input(&vec![0.0; 800], &mut edges);
        }
        assert!(edges.is_empty());
        assert!(p.preroll().len() <= p.preroll().capacity_samples());
        assert_eq!(p.stats().frames_scored, 200 * 800 / 512);
    }

    #[test]
    fn a_burst_yields_a_start_then_an_end_at_frame_boundaries() {
        let mut p = pipeline(16_000);
        let mut edges = Vec::new();
        let mut signal = vec![0.0; 16 * 320];
        signal.extend(tone(16_000, 640, 0.5));
        signal.extend(vec![0.0; 16 * 320]);
        p.push_input(&signal, &mut edges);
        assert_eq!(edges.len(), 2, "{edges:?}");
        assert_eq!(edges[0].edge, SpeechEdge::Start);
        assert_eq!(edges[1].edge, SpeechEdge::End);
        assert!(edges[0].sample16 % FRAME_SAMPLES as u64 == 0);
        // Burst begins at sample 5120 = frame 10 (the frame is fully loud).
        assert_eq!(edges[0].sample16, 10 * 512);
        assert!(edges[1].sample16 > edges[0].sample16);
        assert!(edges[0].confidence.unwrap() > 0.9);
    }

    #[test]
    fn no_debounce_a_one_frame_blip_is_still_both_edges() {
        let mut p = pipeline(16_000);
        let mut edges = Vec::new();
        let mut signal = vec![0.0; 512 * 3];
        signal.extend(tone(16_000, 32, 0.5));
        signal.extend(vec![0.0; 512 * 3]);
        p.push_input(&signal, &mut edges);
        assert_eq!(
            edges.iter().map(|e| e.edge).collect::<Vec<_>>(),
            vec![SpeechEdge::Start, SpeechEdge::End]
        );
        assert_eq!(edges[1].sample16 - edges[0].sample16, FRAME_SAMPLES as u64);
    }

    #[test]
    fn chunking_does_not_change_the_edges_including_resampled_input() {
        let mut signal = vec![0.0; 48 * 400];
        signal.extend(tone(48_000, 500, 0.5));
        signal.extend(vec![0.0; 48 * 400]);
        let run = |chunk: usize| {
            let mut p = pipeline(48_000);
            let mut edges = Vec::new();
            for c in signal.chunks(chunk) {
                p.push_input(c, &mut edges);
            }
            edges
                .iter()
                .map(|e| (e.edge, e.sample16))
                .collect::<Vec<_>>()
        };
        let a = run(signal.len());
        assert_eq!(a.len(), 2);
        assert_eq!(a, run(480));
        assert_eq!(a, run(97));
    }

    #[test]
    fn a_gap_closes_the_open_segment_resets_and_clears_the_preroll() {
        let mut p = pipeline(16_000);
        let mut edges = Vec::new();
        p.push_input(&tone(16_000, 200, 0.5), &mut edges);
        assert_eq!(edges.len(), 1);
        p.gap(&mut edges);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[1].edge, SpeechEdge::End);
        assert!(p.preroll().is_empty());
        assert_eq!(p.scorer.resets, 1);
        p.gap(&mut edges);
        assert_eq!(edges.len(), 2, "no segment open, so no extra end");
    }

    #[test]
    fn finish_closes_a_segment_that_is_still_open() {
        let mut p = pipeline(16_000);
        let mut edges = Vec::new();
        p.push_input(&tone(16_000, 200, 0.5), &mut edges);
        p.finish(&mut edges);
        assert_eq!(edges.last().unwrap().edge, SpeechEdge::End);
    }

    #[test]
    fn a_failing_scorer_is_counted_and_produces_no_edges() {
        struct Broken;
        impl FrameScorer for Broken {
            fn score(&mut self, _: &[f32]) -> Result<f32, String> {
                Ok(f32::NAN)
            }
            fn reset(&mut self) {}
        }
        let mut p = VadPipeline::new(Broken, 16_000, HysteresisConfig::default(), 0).unwrap();
        let mut edges = Vec::new();
        p.push_input(&vec![0.5; 4096], &mut edges);
        assert!(edges.is_empty());
        assert_eq!(p.stats().scorer_errors, 8);
    }
}
