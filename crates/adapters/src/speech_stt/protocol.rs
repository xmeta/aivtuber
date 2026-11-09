//! Binary framing of the audio pipe from the speech sidecar to the STT worker.
//!
//! The worker's stdin carries 16 kHz mono PCM plus segment control, nothing
//! else. One frame is `tag: u8`, `len: u32` little endian, `payload`:
//!
//! | tag  | frame | payload |
//! |------|-------|---------|
//! | 0x01 | START | `segment_id: u64`, `start_ms: u64` (runtime time of the first sample that follows) |
//! | 0x02 | AUDIO | little-endian `i16` samples, at most [`MAX_AUDIO_FRAME_SAMPLES`] |
//! | 0x03 | END   | `segment_id: u64`, `flags: u8` ([`END_FLAG_SKIP_DECODE`]) |
//!
//! The reader bounds the payload length before allocating, so a corrupt or
//! hostile length costs an error, not memory. Audio is never persisted: this
//! framing exists only in a pipe.

use std::fmt;
use std::io::{self, Read, Write};

pub const TAG_START: u8 = 0x01;
pub const TAG_AUDIO: u8 = 0x02;
pub const TAG_END: u8 = 0x03;

/// One AUDIO frame holds at most one second of 16 kHz samples.
pub const MAX_AUDIO_FRAME_SAMPLES: usize = 16_000;
/// The END flag: the segment is not worth transcribing (too little speech, or
/// audio was lost). The worker closes it with an empty final.
pub const END_FLAG_SKIP_DECODE: u8 = 0x01;

const START_PAYLOAD: usize = 16;
const END_PAYLOAD: usize = 9;

#[derive(Clone, PartialEq)]
pub enum Frame {
    Start {
        segment_id: u64,
        start_ms: u64,
    },
    /// Mono 16 kHz samples, normalised to [-1, 1].
    Audio(Vec<f32>),
    End {
        segment_id: u64,
        skip_decode: bool,
    },
}

// Samples are audio of a person: `Debug` shows lengths only.
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start {
                segment_id,
                start_ms,
            } => f
                .debug_struct("Start")
                .field("segment_id", segment_id)
                .field("start_ms", start_ms)
                .finish(),
            Self::Audio(samples) => f
                .debug_struct("Audio")
                .field("samples", &samples.len())
                .finish(),
            Self::End {
                segment_id,
                skip_decode,
            } => f
                .debug_struct("End")
                .field("segment_id", segment_id)
                .field("skip_decode", skip_decode)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    UnknownTag(u8),
    BadLength {
        tag: u8,
        len: usize,
    },
    /// The pipe ended inside a frame.
    Truncated,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "audio pipe: {error}"),
            Self::UnknownTag(tag) => write!(f, "unknown audio frame tag {tag:#04x}"),
            Self::BadLength { tag, len } => {
                write!(f, "audio frame {tag:#04x} has an invalid length {len}")
            }
            Self::Truncated => write!(f, "audio pipe ended inside a frame"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Encode samples as i16, clamped. The i16 quantisation step is about -90 dB.
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

fn header(tag: u8, len: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + len);
    frame.push(tag);
    frame.extend_from_slice(&(len as u32).to_le_bytes());
    frame
}

pub fn encode_start(segment_id: u64, start_ms: u64) -> Vec<u8> {
    let mut frame = header(TAG_START, START_PAYLOAD);
    frame.extend_from_slice(&segment_id.to_le_bytes());
    frame.extend_from_slice(&start_ms.to_le_bytes());
    frame
}

/// One AUDIO frame. `samples` must not exceed [`MAX_AUDIO_FRAME_SAMPLES`];
/// use [`encode_audio_chunks`] for longer runs.
pub fn encode_audio(samples: &[f32]) -> Vec<u8> {
    debug_assert!(samples.len() <= MAX_AUDIO_FRAME_SAMPLES);
    let mut frame = header(TAG_AUDIO, samples.len() * 2);
    for sample in samples {
        frame.extend_from_slice(&to_i16(*sample).to_le_bytes());
    }
    frame
}

pub fn encode_audio_chunks(samples: &[f32]) -> Vec<Vec<u8>> {
    samples
        .chunks(MAX_AUDIO_FRAME_SAMPLES)
        .map(encode_audio)
        .collect()
}

pub fn encode_end(segment_id: u64, skip_decode: bool) -> Vec<u8> {
    let mut frame = header(TAG_END, END_PAYLOAD);
    frame.extend_from_slice(&segment_id.to_le_bytes());
    frame.push(if skip_decode { END_FLAG_SKIP_DECODE } else { 0 });
    frame
}

/// Write one already encoded frame and flush it.
pub fn write_frame(out: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    out.write_all(frame)?;
    out.flush()
}

pub struct FrameReader<R> {
    reader: R,
}

impl<R: Read> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// The next frame, or `None` on a clean end of input at a frame boundary.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        let mut head = [0_u8; 5];
        match read_exact_or_eof(&mut self.reader, &mut head)? {
            ReadOutcome::Eof => return Ok(None),
            ReadOutcome::Full => {}
        }
        let tag = head[0];
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let bad = || ProtocolError::BadLength { tag, len };
        // Validate the length against the tag before reading or allocating.
        match tag {
            TAG_START if len != START_PAYLOAD => return Err(bad()),
            TAG_END if len != END_PAYLOAD => return Err(bad()),
            TAG_AUDIO if !len.is_multiple_of(2) || len > MAX_AUDIO_FRAME_SAMPLES * 2 => {
                return Err(bad());
            }
            TAG_START | TAG_AUDIO | TAG_END => {}
            other => return Err(ProtocolError::UnknownTag(other)),
        }
        let mut payload = vec![0_u8; len];
        match read_exact_or_eof(&mut self.reader, &mut payload)? {
            ReadOutcome::Eof if len > 0 => return Err(ProtocolError::Truncated),
            _ => {}
        }
        let u64_at = |at: usize| {
            u64::from_le_bytes([
                payload[at],
                payload[at + 1],
                payload[at + 2],
                payload[at + 3],
                payload[at + 4],
                payload[at + 5],
                payload[at + 6],
                payload[at + 7],
            ])
        };
        Ok(Some(match tag {
            TAG_START => Frame::Start {
                segment_id: u64_at(0),
                start_ms: u64_at(8),
            },
            TAG_END => Frame::End {
                segment_id: u64_at(0),
                skip_decode: payload[8] & END_FLAG_SKIP_DECODE != 0,
            },
            _ => Frame::Audio(
                payload
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0)
                    .collect(),
            ),
        }))
    }
}

enum ReadOutcome {
    Full,
    Eof,
}

/// Fill `buf` completely. `Eof` only when the pipe ended before the first
/// byte; ending part-way through is a truncated frame.
fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> Result<ReadOutcome, ProtocolError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(ReadOutcome::Eof),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
    Ok(ReadOutcome::Full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(bytes: &[u8]) -> Result<Vec<Frame>, ProtocolError> {
        let mut reader = FrameReader::new(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    #[test]
    fn frames_round_trip() {
        let mut bytes = encode_start(7, 12_345);
        bytes.extend(encode_audio(&[0.0, 0.5, -0.5, 1.0]));
        bytes.extend(encode_end(7, true));
        let frames = read_all(&bytes).unwrap();
        assert_eq!(
            frames[0],
            Frame::Start {
                segment_id: 7,
                start_ms: 12_345
            }
        );
        let Frame::Audio(samples) = &frames[1] else {
            panic!("{:?}", frames[1]);
        };
        assert_eq!(samples.len(), 4);
        assert!((samples[1] - 0.5).abs() < 1e-3 && (samples[2] + 0.5).abs() < 1e-3);
        assert!(samples[3] > 0.999);
        assert_eq!(
            frames[2],
            Frame::End {
                segment_id: 7,
                skip_decode: true
            }
        );
    }

    #[test]
    fn out_of_range_samples_are_clamped_not_wrapped() {
        let frames = read_all(&encode_audio(&[5.0, -5.0])).unwrap();
        let Frame::Audio(samples) = &frames[0] else {
            panic!()
        };
        assert!(samples[0] > 0.999 && samples[1] < -0.999);
    }

    #[test]
    fn hostile_lengths_are_refused_before_allocating() {
        // 4 GiB audio frame.
        let mut bytes = vec![TAG_AUDIO];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            read_all(&bytes),
            Err(ProtocolError::BadLength { tag: TAG_AUDIO, .. })
        ));
        // Odd audio length and wrong control lengths.
        for (tag, len) in [(TAG_AUDIO, 3_u32), (TAG_START, 15), (TAG_END, 10)] {
            let mut bytes = vec![tag];
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend(vec![0; len as usize]);
            assert!(matches!(
                read_all(&bytes),
                Err(ProtocolError::BadLength { .. })
            ));
        }
        // Over the one-second cap.
        let mut bytes = vec![TAG_AUDIO];
        let len = (MAX_AUDIO_FRAME_SAMPLES * 2 + 2) as u32;
        bytes.extend_from_slice(&len.to_le_bytes());
        assert!(matches!(
            read_all(&bytes),
            Err(ProtocolError::BadLength { .. })
        ));
    }

    #[test]
    fn unknown_tags_and_truncation_are_errors_and_clean_eof_is_not() {
        assert!(matches!(
            read_all(&[0x7f, 0, 0, 0, 0]),
            Err(ProtocolError::UnknownTag(0x7f))
        ));
        let full = encode_start(1, 2);
        assert!(matches!(
            read_all(&full[..full.len() - 1]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            read_all(&full[..3]),
            Err(ProtocolError::Truncated)
        ));
        assert!(read_all(&[]).unwrap().is_empty());
    }

    #[test]
    fn long_runs_are_split_at_the_frame_cap() {
        let samples = vec![0.25_f32; MAX_AUDIO_FRAME_SAMPLES * 2 + 5];
        let chunks = encode_audio_chunks(&samples);
        assert_eq!(chunks.len(), 3);
        let bytes: Vec<u8> = chunks.concat();
        let frames = read_all(&bytes).unwrap();
        let total: usize = frames
            .iter()
            .map(|f| match f {
                Frame::Audio(s) => s.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(total, samples.len());
    }

    #[test]
    fn debug_shows_lengths_only() {
        let text = format!("{:?}", Frame::Audio(vec![0.123_456; 8]));
        assert!(text.contains("samples: 8") && !text.contains("0.123"));
    }
}
