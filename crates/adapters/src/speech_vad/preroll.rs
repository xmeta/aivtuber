use crate::speech_vad::pipeline::SAMPLE_RATE_HZ;
use std::collections::VecDeque;
use std::fmt;

/// Hard ceiling on the pre-roll window.
pub const MAX_PREROLL_MS: u64 = 2_000;

/// Bounded in-memory ring of the most recent 16 kHz mono samples.
///
/// This exists only as the hook the S3 transcriber reads to recover speech
/// onset that the VAD reported late (EXP-0001: 12.7% of clean utterances had
/// an onset lag above 100 ms). It is never written to disk or sent anywhere,
/// holds at most `MAX_PREROLL_MS` of audio, and its `Debug` output carries
/// lengths only.
pub struct PreRoll {
    capacity: usize,
    samples: VecDeque<f32>,
    /// Samples pushed since creation or the last `clear`, retained or not.
    /// Stream positions (`RawEdge::sample16`) count in the same unit.
    pushed: u64,
}

impl PreRoll {
    /// `window_ms == 0` disables retention entirely.
    pub fn new(window_ms: u64) -> Result<Self, String> {
        if window_ms > MAX_PREROLL_MS {
            return Err(format!("preroll must be at most {MAX_PREROLL_MS} ms"));
        }
        let capacity = (window_ms * SAMPLE_RATE_HZ as u64 / 1000) as usize;
        Ok(Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
            pushed: 0,
        })
    }

    pub fn capacity_samples(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.pushed += samples.len() as u64;
        if self.capacity == 0 {
            return;
        }
        // Only the newest `capacity` samples can survive, so skip the rest
        // instead of pushing and popping them.
        let keep = samples.len().min(self.capacity);
        let overflow = (self.samples.len() + keep).saturating_sub(self.capacity);
        self.samples.drain(..overflow.min(self.samples.len()));
        self.samples.extend(&samples[samples.len() - keep..]);
    }

    /// Copy of the most recent `window_ms` of audio (at most what is held).
    pub fn snapshot(&self, window_ms: u64) -> Vec<f32> {
        let want = ((window_ms * SAMPLE_RATE_HZ as u64 / 1000) as usize).min(self.samples.len());
        self.samples
            .iter()
            .skip(self.samples.len() - want)
            .copied()
            .collect()
    }

    /// Stream position just after the newest held sample.
    pub fn end_position(&self) -> u64 {
        self.pushed
    }

    /// Samples from stream position `from` (clamped to the oldest held
    /// sample) up to `end_position`, and the position the copy starts at. The
    /// STT segmenter uses it to take the pre-roll before a start edge.
    pub fn since(&self, from: u64) -> (u64, Vec<f32>) {
        let oldest = self.pushed - self.samples.len() as u64;
        let start = from.clamp(oldest, self.pushed);
        let skip = (start - oldest) as usize;
        (start, self.samples.iter().skip(skip).copied().collect())
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.pushed = 0;
    }
}

impl fmt::Debug for PreRoll {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreRoll")
            .field("capacity_samples", &self.capacity)
            .field("len", &self.samples.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_its_capacity_and_keeps_the_newest_samples() {
        let mut ring = PreRoll::new(300).unwrap();
        assert_eq!(ring.capacity_samples(), 4_800);
        for chunk in 0..100 {
            ring.push(&vec![chunk as f32; 777]);
            assert!(ring.len() <= 4_800);
        }
        assert_eq!(ring.len(), 4_800);
        let tail = ring.snapshot(10);
        assert_eq!(tail.len(), 160);
        assert!(tail.iter().all(|v| *v == 99.0));
    }

    #[test]
    fn one_oversized_push_is_truncated_to_the_newest_samples() {
        let mut ring = PreRoll::new(100).unwrap();
        let big: Vec<f32> = (0..10_000).map(|i| i as f32).collect();
        ring.push(&big);
        assert_eq!(ring.len(), 1_600);
        assert_eq!(ring.snapshot(100)[0], 8_400.0);
    }

    #[test]
    fn zero_window_retains_nothing_and_the_ceiling_is_enforced() {
        let mut ring = PreRoll::new(0).unwrap();
        ring.push(&[1.0; 100]);
        assert!(ring.is_empty());
        assert!(PreRoll::new(MAX_PREROLL_MS + 1).is_err());
    }

    #[test]
    fn positions_count_every_pushed_sample_and_since_clamps_to_what_is_held() {
        let mut ring = PreRoll::new(100).unwrap();
        let all: Vec<f32> = (0..5_000).map(|i| i as f32).collect();
        ring.push(&all[..3_000]);
        ring.push(&all[3_000..]);
        assert_eq!(ring.end_position(), 5_000);
        // Capacity is 1_600: positions 3_400..5_000 are held.
        let (start, samples) = ring.since(4_000);
        assert_eq!((start, samples.len(), samples[0]), (4_000, 1_000, 4_000.0));
        let (start, samples) = ring.since(0);
        assert_eq!((start, samples.len(), samples[0]), (3_400, 1_600, 3_400.0));
        let (start, samples) = ring.since(9_999);
        assert_eq!((start, samples.len()), (5_000, 0));
        ring.clear();
        assert_eq!(ring.end_position(), 0);
        assert!(ring.since(0).1.is_empty());
    }

    #[test]
    fn a_zero_window_still_counts_positions() {
        let mut ring = PreRoll::new(0).unwrap();
        ring.push(&[0.0; 321]);
        assert_eq!(ring.end_position(), 321);
        assert!(ring.since(0).1.is_empty());
    }

    #[test]
    fn debug_never_shows_samples() {
        let mut ring = PreRoll::new(100).unwrap();
        ring.push(&[0.123_456; 10]);
        assert!(!format!("{ring:?}").contains("0.123"));
    }
}
