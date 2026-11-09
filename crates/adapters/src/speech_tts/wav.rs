//! 16-bit PCM mono WAV encoding for the TTS sidecar (#220 S2b).
//!
//! The file is the one the default `ffplay` audio command plays and the one
//! `audio://generated/<digest>.wav` names. The header is the fixed 44-byte
//! canonical form, so a cache hit can recover rate and duration from the
//! header alone ([`parse_header`]).

/// Longest audio the sidecar will encode: a bound on memory and disk for one
/// request, far above a 500-character reply (about 30 s).
pub const MAX_AUDIO_SECONDS: u64 = 120;
pub const HEADER_LEN: usize = 44;

/// `None` when the samples are empty, the rate is zero, or the audio exceeds
/// [`MAX_AUDIO_SECONDS`].
pub fn encode_pcm16(samples: &[f32], sample_rate: u32) -> Option<Vec<u8>> {
    if samples.is_empty()
        || sample_rate == 0
        || samples.len() as u64 > u64::from(sample_rate) * MAX_AUDIO_SECONDS
    {
        return None;
    }
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(HEADER_LEN + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1_u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        // NaN converts to 0 (`as` saturates and maps NaN to zero).
        let value = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    Some(out)
}

/// Sample rate and duration of a file this module wrote. `None` for anything
/// else, including a truncated file (a cache hit is then re-synthesized).
pub fn parse_header(file_len: u64, header: &[u8]) -> Option<(u32, u64)> {
    if header.len() < HEADER_LEN
        || &header[0..4] != b"RIFF"
        || &header[8..16] != b"WAVEfmt "
        || &header[36..40] != b"data"
    {
        return None;
    }
    let rate = u32::from_le_bytes(header[24..28].try_into().ok()?);
    let data_len = u64::from(u32::from_le_bytes(header[40..44].try_into().ok()?));
    if rate == 0 || data_len == 0 || file_len != HEADER_LEN as u64 + data_len {
        return None;
    }
    Some((rate, duration_ms(data_len / 2, rate)))
}

/// Duration in whole milliseconds, rounded up, at least 1 for non-empty audio.
pub fn duration_ms(samples: u64, sample_rate: u32) -> u64 {
    (samples * 1000)
        .div_ceil(u64::from(sample_rate).max(1))
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_canonical_header_and_clamped_samples() {
        let wav = encode_pcm16(&[0.0, 1.0, -1.0, 2.0, f32::NAN], 44_100).unwrap();
        assert_eq!(wav.len(), HEADER_LEN + 10);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[44..46], 0_i16.to_le_bytes());
        assert_eq!(&wav[46..48], 32767_i16.to_le_bytes());
        assert_eq!(&wav[48..50], (-32767_i16).to_le_bytes());
        assert_eq!(&wav[50..52], 32767_i16.to_le_bytes());
        assert_eq!(&wav[52..54], 0_i16.to_le_bytes());
        let (rate, ms) = parse_header(wav.len() as u64, &wav).unwrap();
        assert_eq!(rate, 44_100);
        assert_eq!(ms, 1);
    }

    #[test]
    fn refuses_empty_zero_rate_and_oversize_audio() {
        assert!(encode_pcm16(&[], 44_100).is_none());
        assert!(encode_pcm16(&[0.0], 0).is_none());
        let too_long = vec![0.0_f32; (10 * MAX_AUDIO_SECONDS + 1) as usize];
        assert!(encode_pcm16(&too_long, 10).is_none());
    }

    #[test]
    fn a_truncated_or_foreign_file_is_not_a_cache_hit() {
        let wav = encode_pcm16(&vec![0.1; 4_410], 44_100).unwrap();
        assert_eq!(parse_header(wav.len() as u64, &wav).unwrap().1, 100);
        assert!(parse_header(wav.len() as u64 - 2, &wav).is_none());
        assert!(parse_header(10, &wav[..10]).is_none());
        let mut foreign = wav.clone();
        foreign[0] = b'X';
        assert!(parse_header(foreign.len() as u64, &foreign).is_none());
    }

    #[test]
    fn duration_rounds_up() {
        assert_eq!(duration_ms(44_100, 44_100), 1000);
        assert_eq!(duration_ms(44_101, 44_100), 1001);
        assert_eq!(duration_ms(1, 44_100), 1);
    }
}
