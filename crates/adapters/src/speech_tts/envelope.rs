//! Amplitude-driven mouth movement (#220 S2b, decision Q7).
//!
//! Supertonic returns no visemes, so the sidecar can derive a coarse mouth
//! track from the audio envelope and publish it through the existing
//! viseme-track contract (`schemas/viseme-track.schema.json`, resolved by the
//! app's `LocalVisemeStore` from `viseme_ref`). It carries no phonetic
//! information: one open-mouth viseme (`aa`) whose weight follows the
//! short-term loudness, and `neutral` when quiet. Text-derived visemes stay
//! with ADR-0004 / #222.

/// Window the loudness is measured over, and so the cue spacing at most.
const WINDOW_MS: u64 = 80;
/// RMS that maps to full weight. Speech from this model peaks well below
/// full scale, so this keeps the mouth visibly open on ordinary syllables.
const FULL_WEIGHT_RMS: f32 = 0.18;
/// Below this weight the mouth is closed.
const CLOSED_BELOW: f32 = 0.06;
/// A new cue is emitted only when the weight moved by at least this much, so
/// a steady vowel does not become a command flood in the avatar queue.
const MIN_STEP: f32 = 0.12;

/// The track as the JSON text the viseme store reads. Cues are monotonic, end
/// with `neutral` at `duration_ms`, and are bounded by
/// `duration_ms / WINDOW_MS + 2`.
pub fn amplitude_track_json(samples: &[f32], sample_rate: u32, duration_ms: u64) -> String {
    let rate = u64::from(sample_rate.max(1));
    let window = ((rate * WINDOW_MS) / 1000).max(1) as usize;
    let mut cues: Vec<(u64, &'static str, f32)> = Vec::new();
    let mut last_weight = -1.0_f32;
    for (index, chunk) in samples.chunks(window).enumerate() {
        let at_ms = (index as u64 * WINDOW_MS).min(duration_ms);
        let mean_square = chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32;
        let raw = (mean_square.sqrt() / FULL_WEIGHT_RMS).clamp(0.0, 1.0);
        let weight = if raw < CLOSED_BELOW {
            0.0
        } else {
            (raw * 100.0).round() / 100.0
        };
        let moved =
            (weight - last_weight).abs() >= MIN_STEP || (weight == 0.0) != (last_weight == 0.0);
        if cues.is_empty() || moved {
            let viseme = if weight == 0.0 { "neutral" } else { "aa" };
            cues.push((at_ms, viseme, weight));
            last_weight = weight;
        }
    }
    if cues.last().is_none_or(|(at, ..)| *at < duration_ms) {
        cues.push((duration_ms, "neutral", 0.0));
    }
    let body = cues
        .iter()
        .map(|(at, viseme, weight)| {
            format!("{{\"at_ms\":{at},\"viseme\":\"{viseme}\",\"weight\":{weight:.2}}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":\"0.1.0\",\"duration_ms\":{duration_ms},\"cues\":[{body}]}}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn tone(rate: u32, ms: u64, amplitude: f32) -> Vec<f32> {
        (0..(u64::from(rate) * ms / 1000) as usize)
            .map(|i| amplitude * (i as f32 * 0.2).sin())
            .collect()
    }

    fn cues(json: &str) -> Vec<(u64, String, f64)> {
        let value: Value = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(value["schema_version"], "0.1.0");
        value["cues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["at_ms"].as_u64().unwrap(),
                    c["viseme"].as_str().unwrap().to_owned(),
                    c["weight"].as_f64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn loud_audio_opens_the_mouth_and_silence_closes_it() {
        let mut samples = vec![0.0_f32; 4_410]; // 100 ms of silence
        samples.extend(tone(44_100, 400, 0.4));
        samples.extend(vec![0.0_f32; 4_410]);
        let json = amplitude_track_json(&samples, 44_100, 600);
        let cues = cues(&json);
        assert_eq!(cues.first().unwrap().1, "neutral");
        assert!(cues.iter().any(|(_, v, w)| v == "aa" && *w > 0.5));
        let last = cues.last().unwrap();
        assert_eq!((last.0, last.1.as_str(), last.2), (600, "neutral", 0.0));
    }

    #[test]
    fn cues_are_monotonic_bounded_and_within_the_duration() {
        let samples = tone(44_100, 5_000, 0.3);
        let json = amplitude_track_json(&samples, 44_100, 5_000);
        let cues = cues(&json);
        assert!(cues.len() as u64 <= 5_000 / WINDOW_MS + 2);
        assert!(cues.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(
            cues.iter()
                .all(|c| c.0 <= 5_000 && (0.0..=1.0).contains(&c.2))
        );
    }

    #[test]
    fn a_steady_tone_does_not_flood_cues() {
        let samples = tone(44_100, 3_000, 0.3);
        let cues = cues(&amplitude_track_json(&samples, 44_100, 3_000));
        assert!(cues.len() <= 4, "{} cues for a steady tone", cues.len());
    }
}
