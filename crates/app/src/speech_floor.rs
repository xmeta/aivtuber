//! Floor gate for spoken finals (issue #81, S1; ADR-0005).
//!
//! Sits between the transcript tracker and [`ProductionApp::route_spoken_finals`]:
//! a final is routed only when [`FloorPolicy`] answers `TakeTurn` for its
//! speaker. Otherwise it waits in a small bounded queue. The gate
//! * never creates a reply: a final is the only thing that can be routed, and
//!   a partial transcript only reaches the policy as `TranscriptHold`;
//! * is bounded in count (`max_deferred`) and age (`max_defer_ms`): the oldest
//!   is dropped on overflow, a final that outlives the bound is dropped, never
//!   answered over the user, and both are counted;
//! * has no handle to the scheduler, operator control or any output, and is
//!   absent unless composed, so the default path is unchanged;
//! * reads logical time only (`at_ms`).

use aivtuber_domain::{
    FloorAction, FloorConfig, FloorDecision, FloorInput, TranscriptHold, UserSpeechEvidence,
};
use aivtuber_runtime::{
    FloorHold, SpeechObservationTracker, SpeechTranscriptTracker, SurfacedFinal,
};
use aivtuber_scheduler::{FloorMetrics, FloorPolicy, OwnSpeechTracker, evaluate};
use std::collections::VecDeque;

/// What the raw VAD feed says about one speaker, with the feed's liveness.
/// `feed_age_ms` is `None` when the sidecar reports no liveness (D6 then
/// stays dormant and the S1 behaviour applies).
pub(crate) fn floor_evidence(
    observations: Option<&SpeechObservationTracker>,
    speaker: &str,
    at_ms: u64,
    feed_age_ms: Option<u64>,
) -> Option<UserSpeechEvidence> {
    observations
        .and_then(|tracker| tracker.view(speaker, at_ms))
        .map(|view| UserSpeechEvidence {
            signal: view.fallback,
            last_edge_ms: view.last_edge_ms,
            feed_age_ms,
        })
}

pub(crate) fn transcript_hold(
    transcripts: &SpeechTranscriptTracker,
    speaker: &str,
    at_ms: u64,
) -> TranscriptHold {
    match transcripts.floor_hold(speaker, at_ms) {
        FloorHold::None => TranscriptHold::None,
        FloorHold::Holding { .. } => TranscriptHold::Holding,
        FloorHold::Stale { .. } => TranscriptHold::Stale,
    }
}

/// Counters for the gate (the policy has its own `FloorMetrics`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechFloorMetrics {
    /// Finals that were not routed on their first evaluation.
    pub deferred: u64,
    /// Deferred finals later released to routing.
    pub released_after_deferral: u64,
    /// Dropped because `max_defer_ms` passed while the floor stayed held.
    pub dropped_expired: u64,
    /// Dropped because the deferred queue was full (oldest first).
    pub dropped_overflow: u64,
    /// Dropped because the operator turned voice input off (#400): speech
    /// heard before the switch is neither answered nor sent anywhere.
    pub dropped_by_operator: u64,
    /// Highest queue depth seen.
    pub queue_high_water: usize,
    /// Current queue depth.
    pub queued: usize,
    /// Policy counters.
    pub policy: FloorMetrics,
}

#[derive(Debug)]
struct Waiting {
    spoken: SurfacedFinal,
    since_ms: u64,
    counted_deferred: bool,
}

#[derive(Debug)]
pub(crate) struct SpeechFloor {
    policy: FloorPolicy,
    queue: VecDeque<Waiting>,
    metrics: SpeechFloorMetrics,
    /// The performer's own speaking state, fed only by drained speech
    /// lifecycle events (#222 S3): no provider event and no scheduler status.
    own: OwnSpeechTracker,
}

impl SpeechFloor {
    pub(crate) fn new(config: FloorConfig) -> Result<Self, aivtuber_domain::DomainValidationError> {
        Ok(Self {
            policy: FloorPolicy::new(config)?,
            queue: VecDeque::new(),
            metrics: SpeechFloorMetrics::default(),
            own: OwnSpeechTracker::new(),
        })
    }

    pub(crate) fn own_tracker(&self) -> &OwnSpeechTracker {
        &self.own
    }

    pub(crate) fn own_tracker_mut(&mut self) -> &mut OwnSpeechTracker {
        &mut self.own
    }

    pub(crate) fn metrics(&self) -> SpeechFloorMetrics {
        SpeechFloorMetrics {
            queued: self.queue.len(),
            policy: self.policy.metrics(),
            ..self.metrics
        }
    }

    pub(crate) fn recent(&self) -> Vec<FloorDecision> {
        self.policy.recent().copied().collect()
    }

    /// Voice input was turned off (#400): discard every final still waiting
    /// for the floor, counted in `dropped_by_operator`. The policy keeps no
    /// holder or speaker state (its sequence, decision ring and counters are
    /// history and stay), and nothing here reads a clock, so this is
    /// deterministic. Returns how many were dropped.
    pub(crate) fn clear(&mut self) -> usize {
        let dropped = self.queue.len();
        self.metrics.dropped_by_operator += dropped as u64;
        self.queue.clear();
        dropped
    }

    /// A confirmed barge-in, if `input` is one (ADR-0005 S-3). Evaluates
    /// without recording; only a `Yield` that has not already been acted on
    /// for this run (`already_handled`) is sequenced into the decision ring,
    /// so tentative and held evaluations at tick rate neither flood the ring
    /// nor leave speculation in it.
    pub(crate) fn confirm_barge_in(
        &mut self,
        input: &FloorInput,
        already_handled: bool,
    ) -> Option<FloorDecision> {
        if already_handled || evaluate(self.policy.config(), input).action != FloorAction::Yield {
            return None;
        }
        Some(self.policy.decide(input))
    }

    /// Take the finals that may be routed now (oldest first, at most
    /// `max_out`). `input_for` builds the policy input for a speaker from the
    /// trackers; it is the only place evidence enters.
    pub(crate) fn admit(
        &mut self,
        now_ms: u64,
        incoming: Vec<SurfacedFinal>,
        max_out: usize,
        mut input_for: impl FnMut(&str) -> FloorInput,
    ) -> Vec<SurfacedFinal> {
        let config = *self.policy.config();
        let before = self.queue.len();
        self.queue
            .retain(|w| now_ms.saturating_sub(w.since_ms) <= config.max_defer_ms);
        self.metrics.dropped_expired += (before - self.queue.len()) as u64;

        for spoken in incoming {
            if self.queue.len() >= config.max_deferred {
                self.queue.pop_front();
                self.metrics.dropped_overflow += 1;
            }
            self.queue.push_back(Waiting {
                spoken,
                since_ms: now_ms,
                counted_deferred: false,
            });
        }
        self.metrics.queue_high_water = self.metrics.queue_high_water.max(self.queue.len());

        let mut out = Vec::new();
        let mut index = 0;
        while index < self.queue.len() {
            let mut input = input_for(&self.queue[index].spoken.speaker_id);
            input.now_ms = now_ms;
            input.reply_waiting = true;
            let decision = self.policy.decide(&input);
            if decision.action == FloorAction::TakeTurn && out.len() < max_out {
                let waiting = self.queue.remove(index).expect("index in range");
                if waiting.counted_deferred {
                    self.metrics.released_after_deferral += 1;
                }
                out.push(waiting.spoken);
            } else {
                let waiting = &mut self.queue[index];
                if decision.action != FloorAction::TakeTurn && !waiting.counted_deferred {
                    waiting.counted_deferred = true;
                    self.metrics.deferred += 1;
                }
                index += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{FallbackTurnSignal, OwnSpeech, TrustedInterruptibility};
    use aivtuber_runtime::{IngressPolicy, SpeechTranscriptConfig};

    fn finals(texts: &[&str]) -> Vec<SurfacedFinal> {
        let mut tracker = SpeechTranscriptTracker::new(
            SpeechTranscriptConfig::default(),
            IngressPolicy::default(),
        )
        .unwrap();
        for (n, text) in texts.iter().enumerate() {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema_version": "0.1.0", "provider_id": "stt", "provider_version": "1",
                "language": "ja",
                "engine": {"model_id": "m:1", "sha256": "0".repeat(64)},
                "transcripts": [{
                    "speaker_id": format!("s{n}"), "segment_id": 1, "revision": 0,
                    "kind": "final", "text": text, "covered_audio_ms": 500,
                    "observed_at_ms": 900, "complete": true
                }]
            }))
            .unwrap();
            tracker.ingest_bytes(&bytes, 1_000).unwrap();
        }
        tracker.take_finals(16)
    }

    fn speaking(now_ms: u64) -> FloorInput {
        FloorInput {
            now_ms,
            user: Some(UserSpeechEvidence {
                signal: FallbackTurnSignal::SpeakerActive,
                last_edge_ms: Some(900),
                feed_age_ms: None,
            }),
            hold: TranscriptHold::None,
            own: OwnSpeech::Idle,
            reply_waiting: true,
            scene: TrustedInterruptibility::unknown(),
            reply_inbox: false,
        }
    }

    fn silent(now_ms: u64) -> FloorInput {
        FloorInput {
            user: None,
            ..speaking(now_ms)
        }
    }

    #[test]
    fn clear_discards_held_finals_counts_them_and_leaves_a_fresh_floor() {
        let mut floor = SpeechFloor::new(FloorConfig::default()).unwrap();
        // Held behind a speaking user.
        let out = floor.admit(1_000, finals(&["あ", "い"]), 8, |_| speaking(1_000));
        assert!(out.is_empty());
        assert_eq!(floor.metrics().queued, 2);

        assert_eq!(floor.clear(), 2);
        let m = floor.metrics();
        assert_eq!((m.queued, m.dropped_by_operator), (0, 2));
        // Dropped, not released and not expired.
        assert_eq!((m.released_after_deferral, m.dropped_expired), (0, 0));
        // Nothing comes out later, whatever the evidence says.
        assert!(
            floor
                .admit(1_010, Vec::new(), 8, |_| silent(1_010))
                .is_empty()
        );
        assert!(
            floor
                .admit(30_000, Vec::new(), 8, |_| silent(30_000))
                .is_empty()
        );
        // An empty clear counts nothing.
        assert_eq!(floor.clear(), 0);
        assert_eq!(floor.metrics().dropped_by_operator, 2);

        // Afterwards decisions are those of a fresh floor.
        let mut fresh = SpeechFloor::new(FloorConfig::default()).unwrap();
        let a = floor.admit(31_000, finals(&["う"]), 8, |_| silent(31_000));
        let b = fresh.admit(31_000, finals(&["う"]), 8, |_| silent(31_000));
        assert_eq!(a.len(), 1);
        assert_eq!(a, b);
        let last = |f: &SpeechFloor| f.recent().last().map(|d| (d.action, d.reason));
        assert_eq!(last(&floor), last(&fresh));
    }
}
