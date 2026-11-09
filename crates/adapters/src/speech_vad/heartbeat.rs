//! Feed-liveness heartbeat of the VAD sidecar (issue #81 S2, ADR-0005 D6).
//!
//! The speech-observation stream is edge-only: a silent room produces no
//! lines, so "no edge" cannot be told apart from "the feed died". The sidecar
//! therefore writes a tiny bounded line on its stdout, only when it has just
//! *processed audio*, so a stalled microphone, a hung scorer or a dead process
//! stops the beat. The line is deliberately not part of the normative
//! `speech-observation` batch (it carries no observation, no time claim and no
//! authority); it follows the `{"stt":{...}}` status-line precedent: routed by
//! its top-level key (`vad`), strictly parsed, ignored when malformed.
//!
//! The consumer ages the beat by its own receipt time, so a wrong or drifting
//! cross-process clock mapping can neither hide a stall nor fake one.

use serde::Deserialize;
use std::time::{Duration, Instant};

/// Longest heartbeat line the parser looks at.
pub const FEED_HEARTBEAT_MAX_LINE_BYTES: usize = 128;
/// Accepted range for `--feed-heartbeat-ms` (0 disables the beat).
pub const FEED_HEARTBEAT_MIN_MS: u64 = 16;
pub const FEED_HEARTBEAT_MAX_MS: u64 = 1_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Beat {
    seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VadStatus {
    heartbeat: Beat,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    vad: VadStatus,
}

/// One heartbeat line, with its trailing newline.
pub fn encode_feed_heartbeat_line(seq: u64) -> String {
    format!("{{\"vad\":{{\"heartbeat\":{{\"seq\":{seq}}}}}}}\n")
}

/// The sequence number of a well-formed heartbeat line, else `None`.
pub fn parse_feed_heartbeat_line(line: &[u8]) -> Option<u64> {
    if line.len() > FEED_HEARTBEAT_MAX_LINE_BYTES {
        return None;
    }
    serde_json::from_slice::<Line>(line)
        .ok()
        .map(|parsed| parsed.vad.heartbeat.seq)
}

/// Rate limiter for the sidecar: at most one beat per interval, called after
/// each processed chunk of audio.
#[derive(Debug)]
pub struct FeedHeartbeat {
    interval: Duration,
    next: Option<Instant>,
    seq: u64,
}

impl FeedHeartbeat {
    pub fn new(interval_ms: u64) -> Result<Self, String> {
        if !(FEED_HEARTBEAT_MIN_MS..=FEED_HEARTBEAT_MAX_MS).contains(&interval_ms) {
            return Err(format!(
                "feed heartbeat interval must be {FEED_HEARTBEAT_MIN_MS} to {FEED_HEARTBEAT_MAX_MS} ms"
            ));
        }
        Ok(Self {
            interval: Duration::from_millis(interval_ms),
            next: None,
            seq: 0,
        })
    }

    /// The line to write now, if one is due. The first call beats at once.
    pub fn due(&mut self, now: Instant) -> Option<String> {
        if self.next.is_some_and(|next| now < next) {
            return None;
        }
        self.next = Some(now + self.interval);
        let line = encode_feed_heartbeat_line(self.seq);
        self.seq = self.seq.wrapping_add(1);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq_of(line: Option<String>) -> Option<u64> {
        line.as_deref()
            .and_then(|line| parse_feed_heartbeat_line(line.as_bytes()))
    }

    #[test]
    fn a_beat_round_trips_and_hostile_lines_are_ignored() {
        let line = encode_feed_heartbeat_line(7);
        assert_eq!(parse_feed_heartbeat_line(line.as_bytes()), Some(7));
        for bad in [
            &b"{\"vad\":{\"heartbeat\":{\"seq\":-1}}}"[..],
            b"{\"vad\":{\"heartbeat\":{\"seq\":1,\"extra\":2}}}",
            b"{\"vad\":{\"heartbeat\":{}}}",
            b"{\"vad\":{\"other\":{\"seq\":1}}}",
            b"{\"vad\":1}",
            b"{\"stt\":{\"heartbeat\":{\"seq\":1}}}",
            b"not json",
            b"",
        ] {
            assert_eq!(parse_feed_heartbeat_line(bad), None, "{bad:?}");
        }
        let long = format!(
            "{{\"vad\":{{\"heartbeat\":{{\"seq\":1}}}}}}{}",
            " ".repeat(200)
        );
        assert_eq!(parse_feed_heartbeat_line(long.as_bytes()), None);
    }

    #[test]
    fn the_beat_is_rate_limited_and_numbered() {
        let mut beat = FeedHeartbeat::new(64).expect("beat");
        let t0 = Instant::now();
        assert_eq!(seq_of(beat.due(t0)), Some(0));
        assert!(beat.due(t0 + Duration::from_millis(63)).is_none());
        assert_eq!(seq_of(beat.due(t0 + Duration::from_millis(64))), Some(1));
        assert!(FeedHeartbeat::new(15).is_err());
        assert!(FeedHeartbeat::new(1_001).is_err());
    }
}
