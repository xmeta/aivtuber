//! Live input level of the VAD sidecar (#440): the meter behind the operation
//! panel's voice-input card.
//!
//! Like the feed heartbeat it is a tiny bounded status line, not part of the
//! normative `speech-observation` batch: `{"vad":{"level":{"rms":N,"p":N}}}`,
//! two integers in 0..=100, routed by the top-level key `vad`, strictly parsed
//! and ignored when malformed. It carries no audio, no text, no time claim and
//! no authority; the consumer ages it by its own receipt time and shows it,
//! nothing more.
//!
//! * `rms`: the loudest 32 ms frame of the interval, in dBFS mapped from
//!   [`LEVEL_FLOOR_DB`]..0 onto 0..100.
//! * `p`: the highest speech probability of the interval, as a percentage.
//!
//! At most one line per interval of at least [`LEVEL_MIN_MS`], written only
//! when frames were scored since the previous line, so a stalled device or
//! scorer stops it.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Longest level line the parser looks at.
pub const LEVEL_MAX_LINE_BYTES: usize = 128;
/// Accepted range for `--level-ms` (0 disables the line): at most 10 Hz.
pub const LEVEL_MIN_MS: u64 = 100;
pub const LEVEL_MAX_MS: u64 = 1_000;
/// Frame RMS at or below this many dBFS shows as 0.
pub const LEVEL_FLOOR_DB: f32 = -60.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LevelSample {
    pub rms: u8,
    pub p: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Level {
    level: LevelSample,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Line {
    vad: Level,
}

/// One level line, with its trailing newline. Values above 100 are clamped.
pub fn encode_level_line(sample: LevelSample) -> String {
    let line = Line {
        vad: Level {
            level: LevelSample {
                rms: sample.rms.min(100),
                p: sample.p.min(100),
            },
        },
    };
    let mut text = serde_json::to_string(&line).expect("integers always serialize");
    text.push('\n');
    text
}

/// The sample of a well-formed line, else `None`. Over-long lines, unknown
/// keys, non-integers and values outside 0..=100 are all rejected whole.
pub fn parse_level_line(line: &[u8]) -> Option<LevelSample> {
    if line.len() > LEVEL_MAX_LINE_BYTES {
        return None;
    }
    let sample = serde_json::from_slice::<Line>(line).ok()?.vad.level;
    (sample.rms <= 100 && sample.p <= 100).then_some(sample)
}

/// RMS of a frame as the 0..=100 meter value.
pub fn rms_to_meter(rms: f32) -> u8 {
    if !rms.is_finite() || rms <= 0.0 {
        return 0;
    }
    let db = 20.0 * rms.log10();
    ((db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8
}

/// Peak frame RMS and highest speech probability since the last
/// [`take`](Self::take). Two numbers and a flag: bounded by construction.
#[derive(Debug, Clone, Copy, Default)]
pub struct LevelAccum {
    peak_rms: f32,
    max_p: f32,
    seen: bool,
}

impl LevelAccum {
    pub fn observe(&mut self, frame: &[f32], p: f32) {
        if frame.is_empty() {
            return;
        }
        let rms = (frame.iter().map(|v| v * v).sum::<f32>() / frame.len() as f32).sqrt();
        if rms.is_finite() {
            self.peak_rms = self.peak_rms.max(rms);
        }
        self.max_p = self.max_p.max(p.clamp(0.0, 1.0));
        self.seen = true;
    }

    /// The aggregate since the last call; `None` when no frame was observed.
    pub fn take(&mut self) -> Option<LevelSample> {
        let seen = std::mem::take(&mut self.seen);
        let (rms, p) = (self.peak_rms, self.max_p);
        self.peak_rms = 0.0;
        self.max_p = 0.0;
        seen.then(|| LevelSample {
            rms: rms_to_meter(rms),
            p: (p * 100.0).round().clamp(0.0, 100.0) as u8,
        })
    }
}

/// Rate limiter for the sidecar: at most one line per interval.
#[derive(Debug)]
pub struct LevelBeat {
    interval: Duration,
    next: Option<Instant>,
}

impl LevelBeat {
    pub fn new(interval_ms: u64) -> Result<Self, String> {
        if !(LEVEL_MIN_MS..=LEVEL_MAX_MS).contains(&interval_ms) {
            return Err(format!(
                "level interval must be {LEVEL_MIN_MS} to {LEVEL_MAX_MS} ms"
            ));
        }
        Ok(Self {
            interval: Duration::from_millis(interval_ms),
            next: None,
        })
    }

    /// The line to write now, if one is due and `accum` saw audio. Taking
    /// from `accum` only when due keeps the peak over the whole interval.
    pub fn due(&mut self, now: Instant, accum: &mut LevelAccum) -> Option<String> {
        if self.next.is_some_and(|next| now < next) {
            return None;
        }
        let sample = accum.take()?;
        self.next = Some(now + self.interval);
        Some(encode_level_line(sample))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_round_trips_and_hostile_lines_are_ignored() {
        let sample = LevelSample { rms: 42, p: 97 };
        assert_eq!(
            encode_level_line(sample),
            "{\"vad\":{\"level\":{\"rms\":42,\"p\":97}}}\n"
        );
        assert_eq!(
            parse_level_line(encode_level_line(sample).as_bytes()),
            Some(sample)
        );
        for bad in [
            &b"{\"vad\":{\"level\":{\"rms\":101,\"p\":0}}}"[..],
            b"{\"vad\":{\"level\":{\"rms\":0,\"p\":256}}}",
            b"{\"vad\":{\"level\":{\"rms\":-1,\"p\":0}}}",
            b"{\"vad\":{\"level\":{\"rms\":1.5,\"p\":0}}}",
            b"{\"vad\":{\"level\":{\"rms\":\"1\",\"p\":0}}}",
            b"{\"vad\":{\"level\":{\"rms\":1}}}",
            b"{\"vad\":{\"level\":{\"rms\":1,\"p\":1,\"extra\":2}}}",
            b"{\"vad\":{\"other\":{\"rms\":1,\"p\":1}}}",
            b"{\"stt\":{\"level\":{\"rms\":1,\"p\":1}}}",
            b"not json",
            b"",
        ] {
            assert_eq!(parse_level_line(bad), None, "{bad:?}");
        }
        let long = format!(
            "{{\"vad\":{{\"level\":{{\"rms\":1,\"p\":1}}}}}}{}",
            " ".repeat(200)
        );
        assert_eq!(parse_level_line(long.as_bytes()), None);
    }

    #[test]
    fn the_meter_maps_dbfs_onto_zero_to_one_hundred() {
        assert_eq!(rms_to_meter(0.0), 0);
        assert_eq!(rms_to_meter(f32::NAN), 0);
        assert_eq!(rms_to_meter(0.0005), 0, "below -60 dBFS");
        assert_eq!(rms_to_meter(0.001), 0, "exactly the floor");
        assert_eq!(rms_to_meter(0.0316), 50, "-30 dBFS");
        assert_eq!(rms_to_meter(1.0), 100);
        assert_eq!(rms_to_meter(4.0), 100, "clamped");
    }

    #[test]
    fn the_accumulator_keeps_the_peak_and_the_highest_probability() {
        let mut accum = LevelAccum::default();
        assert_eq!(accum.take(), None, "nothing observed");
        accum.observe(&[0.01; 512], 0.10);
        accum.observe(&[0.5; 512], 0.20);
        accum.observe(&[0.01; 512], 0.97);
        let sample = accum.take().expect("frames were seen");
        assert_eq!(sample.rms, rms_to_meter(0.5));
        assert_eq!(sample.p, 97);
        assert_eq!(accum.take(), None, "taking resets");
        accum.observe(&[0.0; 512], 0.0);
        assert_eq!(accum.take(), Some(LevelSample { rms: 0, p: 0 }));
    }

    #[test]
    fn at_most_one_line_per_interval_and_none_without_audio() {
        assert!(LevelBeat::new(99).is_err(), "faster than 10 Hz is refused");
        assert!(LevelBeat::new(1_001).is_err());
        let mut beat = LevelBeat::new(100).unwrap();
        let mut accum = LevelAccum::default();
        let t0 = Instant::now();
        assert_eq!(beat.due(t0, &mut accum), None, "no audio, no line");
        accum.observe(&[0.1; 512], 0.5);
        assert!(beat.due(t0, &mut accum).is_some());
        accum.observe(&[0.9; 512], 0.9);
        assert_eq!(
            beat.due(t0 + Duration::from_millis(99), &mut accum),
            None,
            "too soon"
        );
        // The peak was kept for the next line, not lost.
        let line = beat
            .due(t0 + Duration::from_millis(100), &mut accum)
            .expect("due");
        assert_eq!(parse_level_line(line.as_bytes()).unwrap().p, 90);
        let lines = (0..1_000)
            .filter(|i| {
                accum.observe(&[0.1; 512], 0.1);
                beat.due(t0 + Duration::from_millis(200 + i), &mut accum)
                    .is_some()
            })
            .count();
        assert!(lines <= 10, "1 s of offers produced {lines} lines");
    }
}
