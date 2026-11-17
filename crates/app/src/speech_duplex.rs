//! Half-duplex gate for spoken finals (#396): the performer must not hear
//! itself.
//!
//! With speakers, the microphone can pick up the performer's own playback.
//! In half duplex a speech segment whose *start* falls while performer audio
//! is playing, or within [`PLAYBACK_TAIL_MS`] after it ended, is dropped (and
//! counted) before the floor sees it, so it is never held, routed or sent to a
//! model. The decision is made from a bounded history of playback intervals,
//! not from "is speaking now": a final arrives after recognition, usually
//! after the playback it echoes has ended.
//!
//! The intervals come from the scheduler's speech lifecycle events only
//! (`Started` to `Finished` or `Cancelled`), the same source as the floor's
//! own-speech tracker, on the scheduler's logical timeline, which is the
//! runtime timeline the transcripts are stamped on. Logical time is not proof
//! of audibility (a backend can still be draining after `Finished`); the tail
//! absorbs the usual difference. This module reads no clock and has no
//! handle to any output.

use aivtuber_domain::{SpeechLifecycleEvent, SpeechPhase};
use std::collections::VecDeque;

/// Speech that starts this long after playback ended is still dropped: room
/// reverberation and a backend that drains after its logical end.
pub const PLAYBACK_TAIL_MS: u64 = 400;
/// Most intervals kept; the oldest is forgotten.
const MAX_INTERVALS: usize = 64;
/// Most simultaneously started units tracked (the log never has this many).
const MAX_OPEN: usize = 16;

#[derive(Debug, Default)]
pub(crate) struct PlaybackGate {
    /// Units that started and have not ended: `(event_id, start_ms)`.
    open: Vec<(String, u64)>,
    /// Finished playback intervals `(start_ms, end_ms)`, oldest first.
    closed: VecDeque<(u64, u64)>,
    expected_seq: Option<u64>,
    dropped_during_playback: u64,
}

impl PlaybackGate {
    /// Apply one lifecycle event, offered in `seq` order. An older `seq` is
    /// ignored; a skipped one means events were lost, so the open units (whose
    /// end may be among them) are forgotten rather than trusted: dropping
    /// less is the safe error.
    pub(crate) fn observe(&mut self, event: &SpeechLifecycleEvent) {
        if let Some(expected) = self.expected_seq {
            if event.seq < expected {
                return;
            }
            if event.seq > expected {
                self.open.clear();
            }
        }
        self.expected_seq = Some(event.seq.saturating_add(1));
        let id = &event.speech.event_id;
        match event.phase {
            SpeechPhase::Started => {
                self.open.retain(|(open_id, _)| open_id != id);
                if self.open.len() >= MAX_OPEN {
                    self.open.remove(0);
                }
                self.open.push((id.clone(), event.at_ms));
            }
            SpeechPhase::Finished | SpeechPhase::Cancelled { .. } => {
                if let Some(index) = self.open.iter().position(|(open_id, _)| open_id == id) {
                    let (_, start) = self.open.remove(index);
                    if self.closed.len() >= MAX_INTERVALS {
                        self.closed.pop_front();
                    }
                    self.closed.push_back((start, event.at_ms.max(start)));
                }
            }
            SpeechPhase::Prepared | SpeechPhase::Committed | SpeechPhase::Boundary { .. } => {}
        }
    }

    /// Whether `start_ms` falls while performer audio plays or within the tail
    /// after it.
    pub(crate) fn covers(&self, start_ms: u64) -> bool {
        self.open.iter().any(|(_, start)| *start <= start_ms)
            || self
                .closed
                .iter()
                .any(|(start, end)| *start <= start_ms && start_ms <= end + PLAYBACK_TAIL_MS)
    }

    pub(crate) fn dropped_during_playback(&self) -> u64 {
        self.dropped_during_playback
    }

    pub(crate) fn count_dropped(&mut self, count: usize) {
        self.dropped_during_playback += count as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        SPEECH_LIFECYCLE_SCHEMA_VERSION, SpeechCancelReason, SpeechReached, SpeechRef,
        SpeechTimeBasis,
    };

    fn event(seq: u64, id: &str, at_ms: u64, phase: SpeechPhase) -> SpeechLifecycleEvent {
        SpeechLifecycleEvent {
            schema_version: SPEECH_LIFECYCLE_SCHEMA_VERSION.to_owned(),
            seq,
            at_ms,
            basis: SpeechTimeBasis::LogicalTimeline,
            speech: SpeechRef {
                event_id: id.to_owned(),
                generation: Some(1),
                phrase: 0,
                final_phrase: true,
                asset_id: "asset".to_owned(),
                speech_text_digest: None,
                projection: None,
            },
            phase,
        }
    }

    #[test]
    fn a_start_during_playback_or_in_the_tail_is_covered_and_later_ones_are_not() {
        let mut gate = PlaybackGate::default();
        assert!(!gate.covers(5_000), "nothing played yet");
        gate.observe(&event(1, "a", 1_000, SpeechPhase::Started));
        assert!(gate.covers(1_000) && gate.covers(9_999), "while it plays");
        assert!(!gate.covers(999), "before it started");
        gate.observe(&event(2, "a", 3_000, SpeechPhase::Finished));
        assert!(gate.covers(2_500));
        assert!(
            gate.covers(3_000 + PLAYBACK_TAIL_MS),
            "the tail is inclusive"
        );
        assert!(!gate.covers(3_000 + PLAYBACK_TAIL_MS + 1), "later is kept");
        assert!(!gate.covers(999), "and so is anything before");
    }

    #[test]
    fn a_cancelled_unit_closes_its_interval_and_a_never_started_one_adds_none() {
        let mut gate = PlaybackGate::default();
        gate.observe(&event(1, "a", 100, SpeechPhase::Started));
        gate.observe(&event(
            2,
            "a",
            200,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::OperatorStop,
                reached: SpeechReached::Started,
                spoken_ms: Some(100),
            },
        ));
        assert!(gate.covers(200 + PLAYBACK_TAIL_MS));
        assert!(!gate.covers(200 + PLAYBACK_TAIL_MS + 1));
        gate.observe(&event(
            3,
            "b",
            5_000,
            SpeechPhase::Cancelled {
                reason: SpeechCancelReason::Muted,
                reached: SpeechReached::Committed,
                spoken_ms: None,
            },
        ));
        assert!(!gate.covers(5_100));
    }

    #[test]
    fn phrases_of_one_reply_are_separate_units_with_their_own_tails() {
        let mut gate = PlaybackGate::default();
        gate.observe(&event(1, "r", 0, SpeechPhase::Started));
        gate.observe(&event(2, "r", 1_000, SpeechPhase::Finished));
        gate.observe(&event(3, "r#p1", 1_050, SpeechPhase::Started));
        gate.observe(&event(4, "r#p1", 2_000, SpeechPhase::Finished));
        assert!(gate.covers(1_020), "between the phrases");
        assert!(gate.covers(2_400));
        assert!(!gate.covers(2_401));
    }

    #[test]
    fn a_sequence_gap_forgets_open_units_but_keeps_closed_history() {
        let mut gate = PlaybackGate::default();
        gate.observe(&event(1, "a", 0, SpeechPhase::Started));
        gate.observe(&event(2, "a", 500, SpeechPhase::Finished));
        gate.observe(&event(3, "b", 1_000, SpeechPhase::Started));
        assert!(gate.covers(50_000), "b is open");
        // Events 4 and 5 were lost; b's end may have been among them.
        gate.observe(&event(6, "c", 9_000, SpeechPhase::Prepared));
        assert!(!gate.covers(50_000), "no longer assumed to be playing");
        assert!(gate.covers(600), "what was seen to end is remembered");
        // An old or repeated seq changes nothing.
        gate.observe(&event(2, "z", 0, SpeechPhase::Started));
        assert!(!gate.covers(50_000));
    }

    #[test]
    fn the_history_is_bounded() {
        let mut gate = PlaybackGate::default();
        let mut seq = 0;
        for i in 0..(MAX_INTERVALS as u64 + 10) {
            let id = format!("u{i}");
            seq += 1;
            gate.observe(&event(seq, &id, i * 1_000, SpeechPhase::Started));
            seq += 1;
            gate.observe(&event(seq, &id, i * 1_000 + 100, SpeechPhase::Finished));
        }
        assert_eq!(gate.closed.len(), MAX_INTERVALS);
        assert!(!gate.covers(50), "the oldest interval was forgotten");
        assert!(gate.covers(MAX_INTERVALS as u64 * 1_000 + 50));
    }
}
