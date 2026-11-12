//! The performer's own speaking state, read from speech lifecycle events
//! (issue #222 S3, ADR-0004 D5; consumer of #81).
//!
//! [`OwnSpeechTracker`] is a pure consumer of
//! [`aivtuber_domain::SpeechLifecycleEvent`]: it holds no provider event, no
//! clock and no text. The performer "holds the floor" exactly while a unit is
//! between `Started` and its terminal event, which is the same interval the
//! scheduler's `Playing` status covers for audio items (a committed unit whose
//! start is still in the future is not speaking). Because the lifecycle is
//! scheduler-owned truth, a unit whose output failed, was muted or was dropped
//! as late is `Cancelled`, never `Finished`, and so releases the floor.
//!
//! State is bounded by [`MAX_LIVE_SPEECH`]. A `seq` gap (the log dropped events
//! from a full queue because nobody drained it) is detected and repaired from a
//! [`SpeechLifecycleSnapshot`] instead of trusting a stale view.

use crate::{MAX_LIVE_SPEECH, SpeechLifecycleSnapshot, SpeechStage};
use aivtuber_domain::{OwnSpeech, SpeechLifecycleEvent, SpeechPhase};
use std::collections::BTreeSet;

/// Counters for telemetry; cumulative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OwnSpeechMetrics {
    pub observed: u64,
    /// Events whose `seq` skipped ahead (the log dropped some).
    pub gaps: u64,
    /// Events whose `seq` was not newer than one already seen; ignored.
    pub ignored_stale: u64,
    /// Snapshot re-synchronizations performed.
    pub resyncs: u64,
}

#[derive(Debug, Default)]
pub struct OwnSpeechTracker {
    /// Event ids of units that have started and not ended.
    started: BTreeSet<String>,
    expected_seq: Option<u64>,
    needs_resync: bool,
    awaiting_next_phrase: bool,
    metrics: OwnSpeechMetrics,
}

impl OwnSpeechTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event. Events must be offered in `seq` order; an older or
    /// repeated `seq` is ignored and a skipped one marks the tracker for
    /// [`Self::resync`].
    pub fn observe(&mut self, event: &SpeechLifecycleEvent) {
        if let Some(expected) = self.expected_seq {
            if event.seq < expected {
                self.metrics.ignored_stale += 1;
                return;
            }
            if event.seq > expected {
                self.metrics.gaps += 1;
                self.needs_resync = true;
            }
        }
        self.expected_seq = Some(event.seq.saturating_add(1));
        self.metrics.observed += 1;

        let id = &event.speech.event_id;
        match event.phase {
            SpeechPhase::Started => {
                if self.started.len() >= MAX_LIVE_SPEECH && !self.started.contains(id) {
                    // The log never has this many live units; a view this large
                    // is wrong, so rebuild it rather than grow.
                    self.needs_resync = true;
                } else {
                    self.started.insert(id.clone());
                }
                self.awaiting_next_phrase = false;
            }
            SpeechPhase::Finished => {
                self.started.remove(id);
                // A non-final phrase ended: the reply is still being spoken,
                // even though nothing is audible in the gap.
                self.awaiting_next_phrase = !event.speech.final_phrase;
            }
            SpeechPhase::Cancelled { .. } => {
                self.started.remove(id);
                self.awaiting_next_phrase = false;
            }
            SpeechPhase::Prepared | SpeechPhase::Committed | SpeechPhase::Boundary { .. } => {}
        }
    }

    /// Floor-relevant state: `Speaking` from a unit's start to its end.
    pub fn own(&self) -> OwnSpeech {
        if self.started.is_empty() {
            OwnSpeech::Idle
        } else {
            OwnSpeech::Speaking
        }
    }

    /// A non-final phrase has finished and nothing has started or been
    /// cancelled since: more of the same reply is expected. Informational: the
    /// pipeline's own phrase state stays authoritative for a reply that was
    /// cut short, because a streamed reply cut short reports no final phrase.
    pub fn awaiting_next_phrase(&self) -> bool {
        self.awaiting_next_phrase
    }

    /// Whether a `seq` gap or overflow means the view must be rebuilt.
    pub fn needs_resync(&self) -> bool {
        self.needs_resync
    }

    /// Rebuild from a snapshot taken after the queue was drained. Events still
    /// queued in the snapshot are replayed idempotently afterwards.
    pub fn resync(&mut self, snapshot: &SpeechLifecycleSnapshot) {
        self.started = snapshot
            .live
            .iter()
            .filter(|view| view.stage == SpeechStage::Started)
            .map(|view| view.speech.event_id.clone())
            .collect();
        self.expected_seq = Some(
            snapshot
                .queued
                .first()
                .map_or(snapshot.next_seq, |event| event.seq),
        );
        self.needs_resync = false;
        self.awaiting_next_phrase = false;
        self.metrics.resyncs += 1;
    }

    pub fn metrics(&self) -> OwnSpeechMetrics {
        self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LiveSpeechView;
    use aivtuber_domain::{
        SPEECH_LIFECYCLE_SCHEMA_VERSION, SpeechCancelReason, SpeechReached, SpeechRef,
        SpeechTimeBasis,
    };

    fn event(seq: u64, id: &str, final_phrase: bool, phase: SpeechPhase) -> SpeechLifecycleEvent {
        SpeechLifecycleEvent {
            schema_version: SPEECH_LIFECYCLE_SCHEMA_VERSION.to_owned(),
            seq,
            at_ms: seq * 10,
            basis: SpeechTimeBasis::LogicalTimeline,
            speech: SpeechRef {
                event_id: id.to_owned(),
                generation: Some(1),
                phrase: 0,
                final_phrase,
                asset_id: "asset".to_owned(),
                speech_text_digest: None,
                projection: None,
            },
            phase,
        }
    }

    fn cancelled(reached: SpeechReached) -> SpeechPhase {
        SpeechPhase::Cancelled {
            reason: SpeechCancelReason::BargeIn,
            reached,
            spoken_ms: (reached == SpeechReached::Started).then_some(5),
        }
    }

    #[test]
    fn speaking_spans_started_to_terminal_and_committed_alone_is_idle() {
        let mut tracker = OwnSpeechTracker::new();
        tracker.observe(&event(1, "a", true, SpeechPhase::Prepared));
        tracker.observe(&event(2, "a", true, SpeechPhase::Committed));
        assert_eq!(tracker.own(), OwnSpeech::Idle);
        tracker.observe(&event(3, "a", true, SpeechPhase::Started));
        assert_eq!(tracker.own(), OwnSpeech::Speaking);
        tracker.observe(&event(4, "a", true, SpeechPhase::Finished));
        assert_eq!(tracker.own(), OwnSpeech::Idle);
        assert!(!tracker.awaiting_next_phrase());
    }

    #[test]
    fn a_non_final_finish_releases_audible_floor_but_expects_more() {
        let mut tracker = OwnSpeechTracker::new();
        tracker.observe(&event(1, "a", false, SpeechPhase::Started));
        tracker.observe(&event(2, "a", false, SpeechPhase::Finished));
        assert_eq!(tracker.own(), OwnSpeech::Idle);
        assert!(tracker.awaiting_next_phrase());
        tracker.observe(&event(3, "b", true, SpeechPhase::Started));
        assert_eq!(tracker.own(), OwnSpeech::Speaking);
        assert!(!tracker.awaiting_next_phrase());
        tracker.observe(&event(4, "b", true, SpeechPhase::Finished));
        assert!(!tracker.awaiting_next_phrase());
    }

    #[test]
    fn cancellation_at_every_stage_releases_the_floor() {
        for reached in [
            SpeechReached::BeforeCommit,
            SpeechReached::Committed,
            SpeechReached::Started,
        ] {
            let mut tracker = OwnSpeechTracker::new();
            tracker.observe(&event(1, "a", false, SpeechPhase::Started));
            tracker.observe(&event(2, "a", false, cancelled(reached)));
            assert_eq!(tracker.own(), OwnSpeech::Idle, "{reached:?}");
            assert!(
                !tracker.awaiting_next_phrase(),
                "a cut reply expects no more"
            );
        }
    }

    #[test]
    fn handoff_keeps_the_floor_held() {
        let mut tracker = OwnSpeechTracker::new();
        tracker.observe(&event(1, "a", false, SpeechPhase::Started));
        tracker.observe(&event(2, "a", false, SpeechPhase::Finished));
        tracker.observe(&event(3, "b", true, SpeechPhase::Started));
        assert_eq!(tracker.own(), OwnSpeech::Speaking);
    }

    #[test]
    fn repeated_or_older_events_are_ignored_and_a_gap_asks_for_resync() {
        let mut tracker = OwnSpeechTracker::new();
        tracker.observe(&event(5, "a", true, SpeechPhase::Started));
        tracker.observe(&event(5, "a", true, SpeechPhase::Finished));
        assert_eq!(tracker.own(), OwnSpeech::Speaking);
        assert_eq!(tracker.metrics().ignored_stale, 1);
        assert!(!tracker.needs_resync());
        tracker.observe(&event(9, "a", true, SpeechPhase::Finished));
        assert!(tracker.needs_resync());
        assert_eq!(tracker.metrics().gaps, 1);
    }

    #[test]
    fn resync_rebuilds_from_the_snapshot() {
        let mut tracker = OwnSpeechTracker::new();
        tracker.observe(&event(1, "a", true, SpeechPhase::Started));
        let snapshot = SpeechLifecycleSnapshot {
            next_seq: 40,
            dropped: 30,
            live: vec![LiveSpeechView {
                speech: event(1, "b", true, SpeechPhase::Started).speech,
                stage: SpeechStage::Committed,
                started_at_ms: None,
            }],
            queued: Vec::new(),
        };
        tracker.resync(&snapshot);
        assert_eq!(
            tracker.own(),
            OwnSpeech::Idle,
            "a is gone, b is not started"
        );
        assert!(!tracker.needs_resync());
        tracker.observe(&event(40, "b", true, SpeechPhase::Started));
        assert_eq!(tracker.own(), OwnSpeech::Speaking);
        assert_eq!(tracker.metrics().resyncs, 1);
        assert_eq!(tracker.metrics().gaps, 0);
    }
}
