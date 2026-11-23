//! Immediate non-verbal acknowledgement of a deferred chat reply (issue #75).
//!
//! When a selected comment has to wait because the performer is speaking or
//! generating, the avatar gives one small nod so the viewer sees the comment
//! was noticed. The spoken reply still arrives later through the deferred
//! inbox, unchanged.
//!
//! # Why this is not a scheduler item
//!
//! Like [`crate::face_settle`], the ack is a host-direct `avatar.execute()`
//! (ADR-0006, precedent `reset_avatar_neutral`). A scheduled item would be
//! charged against `min_reaction_spacing_ms`, move `last_terminal_at` and the
//! asset cooldown, and so delay the very reply it acknowledges.
//!
//! # Bounds
//!
//! All timing is logical (`at_ms` from the caller), never a wall clock, so a
//! replay emits the same commands. State is a global last-ack time, a window of
//! at most [`ACK_WINDOW_MAX`] times and at most [`ACK_ACTOR_CAPACITY`] actor
//! keys (oldest evicted): bounded by construction. [`AckGate::clear`] drops it
//! all (avatar reset, operator stop, erasure).

use std::collections::{BTreeMap, VecDeque};

use aivtuber_scheduler::BusyCause;

/// Minimum logical gap between any two acks.
pub const ACK_GLOBAL_COOLDOWN_MS: u64 = 2_000;
/// Minimum logical gap between two acks for the same actor.
pub const ACK_ACTOR_COOLDOWN_MS: u64 = 15_000;
/// Length of the sliding window the ack budget applies to.
pub const ACK_WINDOW_MS: u64 = 10_000;
/// Most acks inside one [`ACK_WINDOW_MS`].
pub const ACK_WINDOW_MAX: usize = 3;
/// Most actor keys remembered; the oldest is evicted first.
pub const ACK_ACTOR_CAPACITY: usize = 64;

/// The gesture preset the ack plays. Code-owned: nothing a viewer or a model
/// writes can name another one.
pub const ACK_GESTURE_PRESET: &str = "nod_small";
/// Gentle on purpose: an ack, not a reply.
pub const ACK_GESTURE_AMPLITUDE: f64 = 0.6;
/// A crowd carrier's ack (#79 S4) is a slightly firmer nod: a wave noticed,
/// not one comment. The renderer draws only nod and shake, so every crowd
/// class nods (a shake would read as "no"); a class-specific gesture needs a
/// renderer gesture, which is not built here.
pub const ACK_CROWD_GESTURE_AMPLITUDE: f64 = 0.8;

/// Outcome counters. The ack is not a `DeferredOutcome`: it is not a terminal
/// outcome of any comment, only a side effect of deferring one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeferredAckMetrics {
    pub acked: u64,
    pub skipped_cause: u64,
    /// Acks that stood for a crowd carrier (included in `acked`).
    pub acked_crowd: u64,
    pub skipped_muted: u64,
    pub skipped_busy_channel: u64,
    pub skipped_global_cooldown: u64,
    pub skipped_actor_cooldown: u64,
    pub skipped_window: u64,
    /// The avatar output refused the gesture. Counted here only; an ack never
    /// sets the output health error.
    pub output_failed: u64,
}

/// Trusted facts about one deferral, read by the host from runtime state.
#[derive(Debug, Clone, Copy)]
pub struct AckContext<'a> {
    pub cause: BusyCause,
    /// The deferred item is a crowd carrier. It acks like any deferral, but its
    /// `actor` must be `None`: a crowd has no single actor, so a crowd ack
    /// neither obeys nor stamps a member's per-actor cooldown.
    pub crowd: bool,
    pub muted: bool,
    /// A Gesture-channel scheduler item is queued or playing.
    pub gesture_busy: bool,
    /// Source-scoped actor key, when the event has an actor.
    pub actor: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckGate {
    enabled: bool,
    last_ack_ms: Option<u64>,
    window: VecDeque<u64>,
    actors: BTreeMap<String, u64>,
    metrics: DeferredAckMetrics,
}

impl AckGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last_ack_ms: None,
            window: VecDeque::new(),
            actors: BTreeMap::new(),
            metrics: DeferredAckMetrics::default(),
        }
    }

    pub fn metrics(&self) -> DeferredAckMetrics {
        self.metrics
    }

    #[cfg(test)]
    pub fn tracked_actors(&self) -> usize {
        self.actors.len()
    }

    /// Forget every cooldown and the window (the avatar was reset or the
    /// lane was cancelled). Counters are kept.
    pub fn clear(&mut self) {
        self.last_ack_ms = None;
        self.window.clear();
        self.actors.clear();
    }

    pub fn note_output_failed(&mut self) {
        self.metrics.output_failed += 1;
    }

    /// Decide whether to ack now. `true` records the ack against the global
    /// cooldown, the window and the actor; the caller then executes it.
    /// Disabled gates answer `false` without counting anything.
    pub fn admit(&mut self, ctx: &AckContext<'_>, at_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if !matches!(ctx.cause, BusyCause::Speaking | BusyCause::Generating) {
            self.metrics.skipped_cause += 1;
            return false;
        }
        if ctx.muted {
            self.metrics.skipped_muted += 1;
            return false;
        }
        if ctx.gesture_busy {
            self.metrics.skipped_busy_channel += 1;
            return false;
        }
        // A crowd ack is not held back by a single comment's ack a moment
        // earlier (the wave is the bigger signal); the window still bounds it.
        if !ctx.crowd
            && self
                .last_ack_ms
                .is_some_and(|last| at_ms.saturating_sub(last) < ACK_GLOBAL_COOLDOWN_MS)
        {
            self.metrics.skipped_global_cooldown += 1;
            return false;
        }
        if let Some(actor) = ctx.actor
            && self
                .actors
                .get(actor)
                .is_some_and(|last| at_ms.saturating_sub(*last) < ACK_ACTOR_COOLDOWN_MS)
        {
            self.metrics.skipped_actor_cooldown += 1;
            return false;
        }
        while self
            .window
            .front()
            .is_some_and(|first| at_ms.saturating_sub(*first) >= ACK_WINDOW_MS)
        {
            self.window.pop_front();
        }
        if self.window.len() >= ACK_WINDOW_MAX {
            self.metrics.skipped_window += 1;
            return false;
        }
        self.window.push_back(at_ms);
        self.last_ack_ms = Some(at_ms);
        if let Some(actor) = ctx.actor {
            self.remember_actor(actor, at_ms);
        }
        self.metrics.acked += 1;
        if ctx.crowd {
            self.metrics.acked_crowd += 1;
        }
        true
    }

    fn remember_actor(&mut self, actor: &str, at_ms: u64) {
        if !self.actors.contains_key(actor)
            && self.actors.len() >= ACK_ACTOR_CAPACITY
            && let Some(oldest) = self
                .actors
                .iter()
                .min_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
                .map(|(key, _)| key.clone())
        {
            self.actors.remove(&oldest);
        }
        self.actors.insert(actor.to_owned(), at_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(actor: Option<&str>) -> AckContext<'_> {
        AckContext {
            cause: BusyCause::Speaking,
            crowd: false,
            muted: false,
            gesture_busy: false,
            actor,
        }
    }

    #[test]
    fn only_speaking_and_generating_ack() {
        let mut gate = AckGate::new(true);
        for cause in [
            BusyCause::Muted,
            BusyCause::SceneProtected,
            BusyCause::FloorHeld,
        ] {
            let c = AckContext { cause, ..ctx(None) };
            assert!(!gate.admit(&c, 0));
        }
        assert_eq!(gate.metrics().skipped_cause, 3);
        let c = AckContext {
            cause: BusyCause::Generating,
            ..ctx(None)
        };
        assert!(gate.admit(&c, 0));
    }

    #[test]
    fn crowd_muted_and_busy_channel_skip() {
        let mut gate = AckGate::new(true);
        assert!(!gate.admit(
            &AckContext {
                muted: true,
                ..ctx(None)
            },
            0
        ));
        assert!(!gate.admit(
            &AckContext {
                gesture_busy: true,
                ..ctx(None)
            },
            0
        ));
        let m = gate.metrics();
        assert_eq!(
            (m.skipped_muted, m.skipped_busy_channel, m.acked),
            (1, 1, 0)
        );
    }

    #[test]
    fn a_crowd_carrier_acks_without_touching_an_actor_cooldown() {
        let mut gate = AckGate::new(true);
        let crowd = AckContext {
            crowd: true,
            ..ctx(None)
        };
        assert!(gate.admit(&crowd, 0));
        assert_eq!(gate.tracked_actors(), 0, "a crowd stamps no actor");
        // An ordinary ack right after it still waits out the global cooldown...
        assert!(!gate.admit(&ctx(None), 1_000));
        assert!(gate.admit(&ctx(Some("a")), 2_000));
        // ...but a crowd ack is not held back by a single comment's ack, only
        // by the shared window.
        assert!(gate.admit(&crowd, 2_100));
        assert!(!gate.admit(&crowd, 2_200), "the window bounds crowd acks");
        let m = gate.metrics();
        assert_eq!((m.acked, m.acked_crowd, m.skipped_window), (3, 2, 1));
    }

    #[test]
    fn disabled_gate_never_acks_or_counts() {
        let mut gate = AckGate::new(false);
        assert!(!gate.admit(&ctx(Some("a")), 0));
        assert_eq!(gate.metrics(), DeferredAckMetrics::default());
    }

    #[test]
    fn global_cooldown_edge() {
        let mut gate = AckGate::new(true);
        assert!(gate.admit(&ctx(Some("a")), 1_000));
        assert!(!gate.admit(&ctx(Some("b")), 1_000 + ACK_GLOBAL_COOLDOWN_MS - 1));
        assert_eq!(gate.metrics().skipped_global_cooldown, 1);
        assert!(gate.admit(&ctx(Some("b")), 1_000 + ACK_GLOBAL_COOLDOWN_MS));
    }

    #[test]
    fn actor_cooldown_edge() {
        let mut gate = AckGate::new(true);
        assert!(gate.admit(&ctx(Some("a")), 0));
        assert!(!gate.admit(&ctx(Some("a")), ACK_ACTOR_COOLDOWN_MS - 1));
        assert_eq!(gate.metrics().skipped_actor_cooldown, 1);
        assert!(gate.admit(&ctx(Some("a")), ACK_ACTOR_COOLDOWN_MS));
    }

    #[test]
    fn window_allows_three_then_reopens() {
        let mut gate = AckGate::new(true);
        // Distinct actors, spaced just past the global cooldown.
        assert!(gate.admit(&ctx(Some("a")), 0));
        assert!(gate.admit(&ctx(Some("b")), 2_000));
        assert!(gate.admit(&ctx(Some("c")), 4_000));
        assert!(!gate.admit(&ctx(Some("d")), 6_000));
        assert_eq!(gate.metrics().skipped_window, 1);
        // The oldest ack (t=0) leaves the window exactly at ACK_WINDOW_MS.
        assert!(!gate.admit(&ctx(Some("d")), ACK_WINDOW_MS - 1));
        assert!(gate.admit(&ctx(Some("d")), ACK_WINDOW_MS));
    }

    #[test]
    fn actor_map_is_bounded_and_evicts_the_oldest() {
        let mut gate = AckGate::new(true);
        let mut t = 0;
        for index in 0..(ACK_ACTOR_CAPACITY + 10) {
            // Space acks out so only the capacity is under test.
            t += ACK_WINDOW_MS;
            assert!(gate.admit(&ctx(Some(&format!("actor-{index}"))), t));
            assert!(gate.tracked_actors() <= ACK_ACTOR_CAPACITY);
        }
        assert_eq!(gate.tracked_actors(), ACK_ACTOR_CAPACITY);
        // actor-0 was evicted, so it is no longer under its cooldown.
        assert!(!gate.actors.contains_key("actor-0"));
        assert!(gate.actors.contains_key("actor-73"));
    }

    #[test]
    fn actorless_events_use_only_global_limits() {
        let mut gate = AckGate::new(true);
        assert!(gate.admit(&ctx(None), 0));
        assert_eq!(gate.tracked_actors(), 0);
    }

    #[test]
    fn clear_forgets_cooldowns_but_keeps_counters() {
        let mut gate = AckGate::new(true);
        assert!(gate.admit(&ctx(Some("a")), 0));
        gate.clear();
        assert_eq!(gate.tracked_actors(), 0);
        assert!(gate.admit(&ctx(Some("a")), 1));
        assert_eq!(gate.metrics().acked, 2);
    }
}
