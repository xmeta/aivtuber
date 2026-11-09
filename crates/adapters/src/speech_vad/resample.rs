use crate::speech_vad::pipeline::SAMPLE_RATE_HZ;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler as _, SincInterpolationParameters, WindowFunction};

/// Sinc kernel length. 128 taps (BlackmanHarris2 window, automatic cutoff)
/// keeps aliasing far below what a VAD front end needs at about 1 ms of
/// delay.
const SINC_LEN: usize = 128;

/// Streaming resampler from any input rate to 16 kHz mono, a thin wrapper
/// over rubato's asynchronous sinc resampler (fixed 10 ms input chunks, no FFT
/// feature). The wrapper buffers arbitrary push sizes into those chunks and
/// drops rubato's initial delay so output sample `k` sits at input time
/// `k * in_rate / 16000` (zero phase): timestamps need no latency correction.
pub struct Resampler {
    inner: Option<Async<f32>>,
    chunk: usize,
    pending: Vec<f32>,
    out: Vec<f32>,
    skip: usize,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("chunk", &self.chunk)
            .field("pending", &self.pending.len())
            .finish()
    }
}

impl Resampler {
    pub fn new(input_rate_hz: u32) -> Result<Self, String> {
        if !(8_000..=192_000).contains(&input_rate_hz) {
            return Err(format!("unsupported input sample rate {input_rate_hz} Hz"));
        }
        if input_rate_hz == SAMPLE_RATE_HZ {
            return Ok(Self {
                inner: None,
                chunk: 0,
                pending: Vec::new(),
                out: Vec::new(),
                skip: 0,
            });
        }
        let chunk = (input_rate_hz / 100) as usize;
        let params = SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2);
        let inner = Async::<f32>::new_sinc(
            f64::from(SAMPLE_RATE_HZ) / f64::from(input_rate_hz),
            1.0,
            &params,
            chunk,
            1,
            FixedAsync::Input,
        )
        .map_err(|e| format!("resampler: {e}"))?;
        let out = vec![0.0; inner.output_frames_max()];
        let skip = inner.output_delay();
        Ok(Self {
            inner: Some(inner),
            chunk,
            pending: Vec::with_capacity(chunk * 2),
            out,
            skip,
        })
    }

    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// Append mono input samples and emit output for every complete 10 ms
    /// chunk. At most one partial chunk is retained.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(inner) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return;
        };
        self.pending.extend_from_slice(input);
        let mut consumed = 0;
        while self.pending.len() - consumed >= self.chunk {
            let slice = &self.pending[consumed..consumed + self.chunk];
            consumed += self.chunk;
            let (Ok(src), Ok(mut dst)) = (
                InterleavedSlice::new(slice, 1, self.chunk),
                InterleavedSlice::new_mut(&mut self.out, 1, inner.output_frames_max()),
            ) else {
                continue;
            };
            let Ok((_, written)) = inner.process_into_buffer(&src, &mut dst, None) else {
                continue;
            };
            let drop = self.skip.min(written);
            self.skip -= drop;
            out.extend_from_slice(&self.out[drop..written]);
        }
        self.pending.drain(..consumed);
    }

    /// Forget buffered input (capture gap): the stream restarts cleanly.
    pub fn reset(&mut self) {
        self.pending.clear();
        if let Some(inner) = self.inner.as_mut() {
            inner.reset();
            self.skip = inner.output_delay();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

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
            // At most one 10 ms chunk is still pending at the end.
            assert!(
                (out.len() as i64 - 16_000).abs() <= 200,
                "len {}",
                out.len()
            );
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
            assert!(chunked.pending.len() < chunked.chunk);
        }
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6));
    }

    #[test]
    fn output_is_zero_phase_so_timestamps_need_no_latency_correction() {
        for rate in [48_000_u32, 44_100] {
            let mut r = Resampler::new(rate).unwrap();
            let at = rate as usize / 2; // 0.5 s
            let mut input = vec![0.0_f32; rate as usize];
            input[at] = 1.0;
            let mut out = Vec::new();
            r.push(&input, &mut out);
            let peak = out
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap()
                .0;
            assert!((peak as i64 - 8_000).abs() <= 1, "rate {rate} peak {peak}");
        }
    }

    #[test]
    fn sixteen_khz_is_a_passthrough_and_odd_rates_are_refused() {
        let mut r = Resampler::new(16_000).unwrap();
        assert!(r.is_passthrough());
        let mut out = Vec::new();
        r.push(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
        assert!(Resampler::new(4_000).is_err());
    }
}
