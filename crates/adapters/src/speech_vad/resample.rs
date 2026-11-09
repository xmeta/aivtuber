use crate::speech_vad::pipeline::SAMPLE_RATE_HZ;
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Zero crossings of the sinc kernel on each side. Twelve gives well over
/// 60 dB stop-band attenuation with a Hann window, which is far more than a
/// VAD front end needs.
const ZERO_CROSSINGS: f64 = 12.0;

/// Streaming windowed-sinc resampler from any input rate to 16 kHz mono.
///
/// Chosen over a crate because the need is one fixed output rate and a few
/// dozen lines; a band-limited kernel (not linear interpolation) is required
/// so 44.1/48 kHz microphones do not alias speech energy above 8 kHz into the
/// VAD input. Output sample `k` sits at input time `k * in_rate / 16000`
/// exactly (zero phase), so timestamps need no latency correction.
#[derive(Debug)]
pub struct Resampler {
    ratio: f64,
    scale: f64,
    half_width: f64,
    buf: VecDeque<f32>,
    /// Absolute input index of `buf[0]`.
    base: u64,
    next_output: u64,
}

impl Resampler {
    pub fn new(input_rate_hz: u32) -> Result<Self, String> {
        if !(8_000..=192_000).contains(&input_rate_hz) {
            return Err(format!("unsupported input sample rate {input_rate_hz} Hz"));
        }
        let ratio = f64::from(input_rate_hz) / f64::from(SAMPLE_RATE_HZ);
        let scale = (1.0 / ratio).min(1.0);
        Ok(Self {
            ratio,
            scale,
            half_width: ZERO_CROSSINGS / scale,
            buf: VecDeque::new(),
            base: 0,
            next_output: 0,
        })
    }

    pub fn is_passthrough(&self) -> bool {
        (self.ratio - 1.0).abs() < f64::EPSILON
    }

    /// Append mono input samples and emit every output sample whose kernel
    /// support is fully available. Retained input is bounded by the kernel
    /// width plus one chunk.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend(input.iter().copied());
        let end = self.base + self.buf.len() as u64;
        loop {
            let t = self.next_output as f64 * self.ratio;
            if (t + self.half_width).floor() as u64 >= end {
                break;
            }
            let lo = (t - self.half_width).ceil().max(0.0) as u64;
            let hi = (t + self.half_width).floor() as u64;
            let mut acc = 0.0_f64;
            for j in lo.max(self.base)..=hi {
                let x = j as f64 - t;
                acc += f64::from(self.buf[(j - self.base) as usize]) * self.kernel(x);
            }
            out.push(acc as f32);
            self.next_output += 1;
        }
        let keep_from = ((self.next_output as f64 * self.ratio) - self.half_width)
            .floor()
            .max(0.0) as u64;
        let drop = keep_from
            .saturating_sub(self.base)
            .min(self.buf.len() as u64);
        self.buf.drain(..drop as usize);
        self.base += drop;
    }

    fn kernel(&self, x: f64) -> f64 {
        let y = self.scale * x;
        let sinc = if y.abs() < 1e-12 {
            1.0
        } else {
            (PI * y).sin() / (PI * y)
        };
        let window = 0.5 * (1.0 + (PI * x / self.half_width).cos());
        self.scale * sinc * window
    }

    /// Forget buffered input (capture gap): the stream restarts cleanly.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.base = 0;
        self.next_output = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, secs: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| (2.0 * PI * hz * i as f64 / f64::from(rate)).sin() as f32 * 0.5)
            .collect()
    }

    fn rms_interior(x: &[f32]) -> f64 {
        let body = &x[x.len() / 4..x.len() * 3 / 4];
        (body.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / body.len() as f64).sqrt()
    }

    #[test]
    fn speech_band_tone_keeps_its_level_from_48k_and_44k1() {
        for rate in [48_000, 44_100] {
            let mut r = Resampler::new(rate).unwrap();
            let mut out = Vec::new();
            r.push(&tone(rate, 1_000.0, 1.0), &mut out);
            assert!((out.len() as i64 - 16_000).abs() < 64, "len {}", out.len());
            let rms = rms_interior(&out);
            assert!(
                (rms - 0.5 / 2f64.sqrt()).abs() < 0.01,
                "rate {rate} rms {rms}"
            );
        }
    }

    #[test]
    fn energy_above_the_new_nyquist_is_rejected_not_aliased() {
        let mut r = Resampler::new(48_000).unwrap();
        let mut out = Vec::new();
        // 10 kHz would alias to 6 kHz without a low-pass.
        r.push(&tone(48_000, 10_000.0, 1.0), &mut out);
        assert!(rms_interior(&out) < 0.01, "rms {}", rms_interior(&out));
    }

    #[test]
    fn chunked_input_matches_one_shot_and_retention_is_bounded() {
        let input = tone(48_000, 700.0, 0.5);
        let mut whole = Resampler::new(48_000).unwrap();
        let mut a = Vec::new();
        whole.push(&input, &mut a);
        let mut chunked = Resampler::new(48_000).unwrap();
        let mut b = Vec::new();
        for chunk in input.chunks(333) {
            chunked.push(chunk, &mut b);
            assert!(chunked.buf.len() < 333 + 2 * (chunked.half_width as usize) + 8);
        }
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6));
    }

    #[test]
    fn sixteen_khz_is_a_passthrough_and_odd_rates_are_refused() {
        let mut r = Resampler::new(16_000).unwrap();
        let mut out = Vec::new();
        r.push(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
        assert!(Resampler::new(4_000).is_err());
    }
}
