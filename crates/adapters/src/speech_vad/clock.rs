use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, Read};

/// The parent never sends more than a short fixed-shape line; anything longer
/// is not a sync message.
pub const CLOCK_SYNC_MAX_LINE_BYTES: usize = 256;
/// Offsets kept for the max filter. Each sample is the true offset minus the
/// one-way pipe delay, so the largest recent sample is the least delayed.
const OFFSET_WINDOW: usize = 8;
/// Chunk-arrival estimates kept for the min filter (about 2.5 s of 10 ms
/// callbacks). Arrival is never earlier than capture, so the smallest origin
/// estimate is the least delayed, and the window lets drift between the audio
/// device clock and the monotonic clock be tracked.
const ORIGIN_WINDOW: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    NotSynced,
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no clock sync received from the runtime yet")
    }
}

impl std::error::Error for ClockError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncBody {
    runtime_now_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncLine {
    clock_sync: SyncBody,
}

/// Line the runtime writes to the sidecar's stdin: its own monotonic
/// `now_ms`. Written at child start and re-sent on a bounded schedule.
pub fn encode_clock_sync_line(runtime_now_ms: u64) -> String {
    let line = SyncLine {
        clock_sync: SyncBody { runtime_now_ms },
    };
    let mut text = serde_json::to_string(&line).expect("sync line serializes");
    text.push('\n');
    text
}

/// `None` for anything that is not exactly a clock-sync line.
pub fn parse_clock_sync_line(line: &[u8]) -> Option<u64> {
    if line.len() > CLOCK_SYNC_MAX_LINE_BYTES {
        return None;
    }
    serde_json::from_slice::<SyncLine>(line)
        .ok()
        .map(|parsed| parsed.clock_sync.runtime_now_ms)
}

/// Maps the sidecar's local monotonic milliseconds onto the runtime timeline.
///
/// `offset = runtime_now_at_send - local_now_at_receipt`. Pipe delay makes
/// every sample smaller than the true offset by that delay, so the maximum
/// over a short window is the best estimate and one scheduling hiccup cannot
/// shift it. Both clocks are monotonic on the same host, so relative drift is
/// negligible; periodic re-sync (every few seconds) bounds it anyway.
#[derive(Debug, Clone, Default)]
pub struct RuntimeClockMap {
    offsets: VecDeque<f64>,
    syncs: u64,
}

impl RuntimeClockMap {
    pub fn on_sync(&mut self, runtime_now_ms: u64, local_now_ms: f64) {
        if self.offsets.len() == OFFSET_WINDOW {
            self.offsets.pop_front();
        }
        self.offsets.push_back(runtime_now_ms as f64 - local_now_ms);
        self.syncs += 1;
    }

    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    pub fn is_synced(&self) -> bool {
        !self.offsets.is_empty()
    }

    pub fn offset_ms(&self) -> Result<f64, ClockError> {
        self.offsets
            .iter()
            .copied()
            .reduce(f64::max)
            .ok_or(ClockError::NotSynced)
    }

    pub fn to_runtime_ms(&self, local_ms: f64) -> Result<u64, ClockError> {
        Ok((local_ms + self.offset_ms()?).round().max(0.0) as u64)
    }
}

/// Maps audio sample positions to the sidecar's local monotonic time.
///
/// For a live device, each captured chunk tells us "input sample N had been
/// captured by local time T". `origin = T - N / rate` is therefore an upper
/// bound on the true stream origin (delivery is never early); the smallest
/// recent value is used. Device acquisition latency before the callback is not
/// observable here and is the remaining, documented, error.
#[derive(Debug, Clone, Default)]
pub struct StreamClock {
    origins: VecDeque<f64>,
}

impl StreamClock {
    pub fn observe_chunk(&mut self, arrival_local_ms: f64, input_samples_end: u64, rate_hz: u32) {
        if self.origins.len() == ORIGIN_WINDOW {
            self.origins.pop_front();
        }
        let origin = arrival_local_ms - input_samples_end as f64 * 1000.0 / f64::from(rate_hz);
        self.origins.push_back(origin);
    }

    /// Local time of 16 kHz output sample `sample16` (zero-phase resampler:
    /// output sample k sits at input time k / 16000 s).
    pub fn sample16_local_ms(&self, sample16: u64) -> Option<f64> {
        let origin = self.origins.iter().copied().reduce(f64::min)?;
        Some(origin + sample16 as f64 / 16.0)
    }

    /// Capture gap: earlier estimates no longer describe the stream.
    pub fn reset(&mut self) {
        self.origins.clear();
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClockPumpStats {
    pub syncs: u64,
    pub ignored_lines: u64,
    pub oversized_lines: u64,
}

/// Read the parent's stdin until EOF, calling `on_sync` for each valid
/// clock-sync line. Retains at most `CLOCK_SYNC_MAX_LINE_BYTES + 1` bytes per
/// line, so a flood of junk or one endless line costs time, not memory.
/// Returning means the parent closed the pipe (it exited): the sidecar should
/// stop too.
pub fn pump_clock_syncs<R: BufRead>(
    mut reader: R,
    mut on_sync: impl FnMut(u64),
) -> io::Result<ClockPumpStats> {
    let mut stats = ClockPumpStats::default();
    let mut line = Vec::with_capacity(CLOCK_SYNC_MAX_LINE_BYTES + 1);
    loop {
        line.clear();
        let read = (&mut reader)
            .take(CLOCK_SYNC_MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(stats);
        }
        if line.last() != Some(&b'\n') && line.len() > CLOCK_SYNC_MAX_LINE_BYTES {
            // Oversized: discard through the delimiter without keeping it.
            stats.oversized_lines += 1;
            loop {
                let buffer = reader.fill_buf()?;
                if buffer.is_empty() {
                    return Ok(stats);
                }
                let newline = buffer.iter().position(|b| *b == b'\n');
                let used = newline.map_or(buffer.len(), |i| i + 1);
                reader.consume(used);
                if newline.is_some() {
                    break;
                }
            }
            continue;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        match parse_clock_sync_line(&line) {
            Some(runtime_now_ms) => {
                stats.syncs += 1;
                on_sync(runtime_now_ms);
            }
            None => stats.ignored_lines += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_line_round_trips_and_rejects_everything_else() {
        let line = encode_clock_sync_line(123_456);
        assert!(line.ends_with('\n'));
        assert_eq!(
            parse_clock_sync_line(line.trim_end().as_bytes()),
            Some(123_456)
        );
        for bad in [
            &b"{}"[..],
            b"{\"clock_sync\":{\"runtime_now_ms\":1,\"x\":2}}",
            b"{\"clock_sync\":{\"runtime_now_ms\":-1}}",
            b"not json",
            b"",
        ] {
            assert_eq!(parse_clock_sync_line(bad), None);
        }
        assert_eq!(parse_clock_sync_line(&vec![b' '; 300]), None);
    }

    #[test]
    fn the_handshake_reader_applies_valid_lines_and_survives_junk_and_floods() {
        let mut input = Vec::new();
        input.extend_from_slice(encode_clock_sync_line(1_000).as_bytes());
        input.extend_from_slice(b"garbage\n");
        input.extend(std::iter::repeat_n(b'x', 5 * 1024 * 1024));
        input.push(b'\n');
        input.extend_from_slice(encode_clock_sync_line(2_000).as_bytes());
        input.extend_from_slice(b"{\"clock_sync\":{\"runtime_now_ms\":3000}}\r\n");
        let mut seen = Vec::new();
        let stats = pump_clock_syncs(io::BufReader::with_capacity(64, &input[..]), |t| {
            seen.push(t)
        })
        .unwrap();
        assert_eq!(seen, vec![1_000, 2_000, 3_000]);
        assert_eq!(stats.syncs, 3);
        assert_eq!(stats.ignored_lines, 1);
        assert_eq!(stats.oversized_lines, 1);
    }

    #[test]
    fn unsynced_map_refuses_to_stamp() {
        let map = RuntimeClockMap::default();
        assert_eq!(map.to_runtime_ms(10.0), Err(ClockError::NotSynced));
    }

    #[test]
    fn offset_estimate_ignores_delayed_samples() {
        let mut map = RuntimeClockMap::default();
        // True offset 5000 ms; pipe delays of 0.2, 3.0 and 0.5 ms.
        map.on_sync(1_000, 1_000.0 - 5_000.0 + 0.2);
        map.on_sync(2_000, 2_000.0 - 5_000.0 + 3.0);
        map.on_sync(3_000, 3_000.0 - 5_000.0 + 0.5);
        let offset = map.offset_ms().unwrap();
        assert!((offset - 4_999.8).abs() < 1e-9, "{offset}");
        assert_eq!(map.to_runtime_ms(100.0), Ok(5_100));
    }

    #[test]
    fn offset_window_is_bounded_so_stale_syncs_age_out() {
        let mut map = RuntimeClockMap::default();
        map.on_sync(10_000, 0.0);
        for i in 1..=OFFSET_WINDOW as u64 {
            map.on_sync(i * 1_000, i as f64 * 1_000.0 - 50.0);
        }
        assert!((map.offset_ms().unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn stream_clock_uses_the_least_delayed_arrival_and_tracks_reset() {
        let mut clock = StreamClock::default();
        assert_eq!(clock.sample16_local_ms(0), None);
        // 10 ms chunks of 48 kHz audio (480 samples); true origin 100 ms.
        for (i, jitter) in [(1, 0.0), (2, 4.0), (3, 1.0)] {
            clock.observe_chunk(100.0 + i as f64 * 10.0 + jitter, i * 480, 48_000);
        }
        assert!((clock.sample16_local_ms(16).unwrap() - 101.0).abs() < 1e-9);
        clock.reset();
        assert_eq!(clock.sample16_local_ms(0), None);
    }
}
