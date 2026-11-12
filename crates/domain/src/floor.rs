//! Conversational-floor contract (issue #81, slice S1; ADR-0005 S-1).
//!
//! Provider-neutral, bounded, closed vocabulary for the deterministic floor
//! policy that lives in `aivtuber-scheduler`. These types are *internal*:
//! they are never read from or written to an untrusted wire, so there is no
//! JSON Schema for them and no `Deserialize` (a decision a payload could
//! supply would be self-escalation). `Serialize` exists only so replay
//! fixtures and metrics can render decisions.
//!
//! Nothing here carries text, audio, a transcript or a source/trust/priority
//! claim: the inputs are counts, ages and enums derived by the runtime from
//! normalizer-classified observations (ADR-0005 S-7). A decision is a
//! recommendation to the scheduler/app, not an authority and not a
//! capability; it can only delay or allow a reply, never trigger one.

use crate::{DomainValidationError, FallbackTurnSignal, Interruptibility, TrustedInterruptibility};
use serde::Serialize;

/// Upper bound on the retained decision ring (`FloorConfig::decision_log`).
pub const MAX_FLOOR_DECISION_LOG: usize = 256;
/// Upper bound on finals deferred behind the floor (`FloorConfig::max_deferred`).
pub const MAX_FLOOR_DEFERRED: usize = 32;

/// Validated range of `FloorConfig::take_turn_silence_ms`.
pub const TAKE_TURN_SILENCE_RANGE_MS: (u64, u64) = (700, 1_000);
/// Validated range of `FloorConfig::barge_in_confirm_ms`.
pub const BARGE_IN_CONFIRM_RANGE_MS: (u64, u64) = (256, 800);

/// Who has the floor after a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorHolder {
    Nobody,
    User,
    Performer,
}

/// The user's turn, as far as the observations can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// No evidence of a user turn (no speech, no VAD, or evidence unusable).
    Idle,
    UserSpeaking,
    /// Silent, but not for `take_turn_silence_ms` yet.
    UserPaused,
    /// Silent for at least `take_turn_silence_ms`.
    UserEnded,
}

/// S1 subset of the ADR-0005 `FloorAction` set. `Backchannel` and
/// `Interrupt` belong to later slices and are intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorAction {
    /// Idle performer: do not start a reply yet.
    Wait,
    /// Speaking performer keeps the floor (a pending reply stays deferred).
    Hold,
    /// The user confirmed a barge-in; the performer should give way. The
    /// policy only recommends: the app executes it (a bounded-fade stop,
    /// #81 S2) through the scheduler and its outputs, never the policy.
    Yield,
    /// A pending reply may be routed now.
    TakeTurn,
}

impl FloorAction {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Hold => "hold",
            Self::Yield => "yield",
            Self::TakeTurn => "take_turn",
        }
    }
}

/// Closed reason set for replay and metrics; never free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorReason {
    /// Raw VAD says the user is speaking.
    UserSpeaking,
    /// A fresh partial transcript says the user is still talking. Delays only.
    PartialTranscriptHolding,
    /// The user paused for less than `take_turn_silence_ms`.
    SilenceBelowThreshold,
    /// The user was silent for at least `take_turn_silence_ms`.
    UserTurnEnded,
    /// No speech evidence for the speaker (text-only path, ADR-0005 D7).
    NoSpeechEvidence,
    /// The last `active` edge is older than the bound: a lost `end` edge must
    /// not pin the floor; degrade as if there were no evidence.
    ActivityStale,
    /// The observation feed is not fresh; costly decisions wait.
    StaleObservation,
    /// The feed has been stale for longer than `vad_loss_ms`: degrade to the
    /// text-only path and say so.
    VadUnavailable,
    /// The floor is open but no reply is waiting.
    NoReplyPending,
    /// The performer is speaking and a reply (if any) must wait.
    PerformerSpeaking,
    /// The user speaks over the performer for less than the confirm run.
    UserSpeechUnconfirmed,
    /// The user's raw run reached `barge_in_confirm_ms` over the performer.
    UserBargeInConfirmed,
    /// A trusted scene says the performer must not be interrupted (#83 S3b):
    /// a reply that the default policy would start waits, or is handed on to
    /// the deferred-reply inbox, which holds it until the scene ends.
    SceneProtected,
}

impl FloorReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::UserSpeaking => "user_speaking",
            Self::PartialTranscriptHolding => "partial_transcript_holding",
            Self::SilenceBelowThreshold => "silence_below_threshold",
            Self::UserTurnEnded => "user_turn_ended",
            Self::NoSpeechEvidence => "no_speech_evidence",
            Self::ActivityStale => "activity_stale",
            Self::StaleObservation => "stale_observation",
            Self::VadUnavailable => "vad_unavailable",
            Self::NoReplyPending => "no_reply_pending",
            Self::PerformerSpeaking => "performer_speaking",
            Self::UserSpeechUnconfirmed => "user_speech_unconfirmed",
            Self::UserBargeInConfirmed => "user_barge_in_confirmed",
            Self::SceneProtected => "scene_protected",
        }
    }
}

/// One floor decision. `seq` is a per-policy monotonic counter and `at_ms` is
/// logical (runtime-timeline) time, never the wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FloorDecision {
    pub seq: u64,
    pub at_ms: u64,
    pub action: FloorAction,
    pub reason: FloorReason,
    pub holder: FloorHolder,
    pub turn: TurnState,
    /// Silence since the user's last `end` edge, when known.
    pub silence_ms: Option<u64>,
    /// The trusted interruptibility this decision used (`unknown` when no
    /// trusted, fresh scene source spoke), so replay names its input.
    pub scene: Interruptibility,
}

/// What the raw VAD feed says about the one speaker whose reply is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSpeechEvidence {
    /// The deterministic silence heuristic (`fallback_turn_signal`). A
    /// turn-end *score* is deliberately not an input in S1: it may only start
    /// reversible preparation (ADR-0005 D2), which this slice has none of.
    pub signal: FallbackTurnSignal,
    /// Time of the last accepted raw edge; the barge-in run is `now - this`
    /// while the signal is `SpeakerActive`.
    pub last_edge_ms: Option<u64>,
    /// Age of the newest observation *delivery* from the VAD feed, when the
    /// feed reports liveness. `None` means "not reported" and disables the
    /// freshness rule (an edge-only feed is silent by design during silence).
    pub feed_age_ms: Option<u64>,
}

/// What the partial transcripts say. Sizes and ages only, never text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptHold {
    None,
    /// A fresh partial: the recognizer thinks the user is still talking.
    Holding,
    /// An open segment gone stale; it must not hold the floor.
    Stale,
}

/// The performer's own speaking state as the scheduler sees it. (ADR-0004
/// lifecycle events are not in the code yet; the scheduler's `Playing`
/// status is the available truth.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnSpeech {
    /// The floor is free.
    Idle,
    /// Reserved or audible: the performer holds the floor.
    Speaking,
}

/// Explicit, bounded input of one decision. Carries no response or chat text:
/// deciding *whether* to speak is separate from deciding *what* to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorInput {
    /// Runtime monotonic (or replay-logical) milliseconds.
    pub now_ms: u64,
    /// `None`: no VAD tracker or no view of this speaker.
    pub user: Option<UserSpeechEvidence>,
    pub hold: TranscriptHold,
    pub own: OwnSpeech,
    /// A final transcript is waiting to be answered.
    pub reply_waiting: bool,
    /// Trusted scene interruptibility (#83 S3b). The type cannot carry a
    /// perceived (rank 4) value; `unknown` is the default behaviour.
    pub scene: TrustedInterruptibility,
    /// A reply the floor lets through in an uninterruptible scene is held by
    /// the app's deferred-reply inbox (#75), so the floor need not keep it
    /// waiting itself (where it would expire after `max_defer_ms`).
    pub reply_inbox: bool,
}

/// Validated floor configuration. Every value has a range; out-of-range is
/// rejected at construction, never clamped (ADR-0005 D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorConfig {
    /// Silence before the irreversible TakeTurn. 800 ms measured on synthetic
    /// pauses only (A-FLOOR-001, not validated on real conversation).
    pub take_turn_silence_ms: u64,
    /// Continuous raw user activity over the performer that confirms a
    /// barge-in. 400 ms (ADR-0005 D5, S-3).
    pub barge_in_confirm_ms: u64,
    /// Newest-delivery age above which costly decisions wait (D6).
    pub observation_stale_ms: u64,
    /// A feed stale for this long degrades to the text-only path (D7).
    pub vad_loss_ms: u64,
    /// Finals that may wait behind the floor (app deferral bound).
    pub max_deferred: usize,
    /// A final deferred this long is dropped, not answered over the user.
    pub max_defer_ms: u64,
    /// Retained decisions in the policy's ring.
    pub decision_log: usize,
}

impl Default for FloorConfig {
    fn default() -> Self {
        Self {
            take_turn_silence_ms: 800,
            barge_in_confirm_ms: 400,
            observation_stale_ms: 160,
            vad_loss_ms: 5_000,
            max_deferred: 8,
            max_defer_ms: 10_000,
            decision_log: 64,
        }
    }
}

impl FloorConfig {
    /// Silence before taking the turn, modulated by the trusted scene (#83
    /// S3b). No new knob: an uninterruptible scene uses the top of the
    /// validated `take_turn_silence_ms` range (never below the configured
    /// value), so the performer is slower to start; every other reading,
    /// including `unknown`, is the configured value.
    pub fn effective_take_turn_silence_ms(&self, scene: TrustedInterruptibility) -> u64 {
        match scene.get() {
            Interruptibility::Uninterruptible => {
                self.take_turn_silence_ms.max(TAKE_TURN_SILENCE_RANGE_MS.1)
            }
            _ => self.take_turn_silence_ms,
        }
    }

    /// Continuous user speech that confirms a barge-in, modulated by the
    /// trusted scene. An uninterruptible scene halves the configured value,
    /// never below the bottom of the validated `barge_in_confirm_ms` range:
    /// the human wins the floor sooner. Everything else is the configured
    /// value.
    pub fn effective_barge_in_confirm_ms(&self, scene: TrustedInterruptibility) -> u64 {
        match scene.get() {
            Interruptibility::Uninterruptible => {
                (self.barge_in_confirm_ms / 2).max(BARGE_IN_CONFIRM_RANGE_MS.0)
            }
            _ => self.barge_in_confirm_ms,
        }
    }

    pub fn validate(&self) -> Result<(), DomainValidationError> {
        fn range(
            field: &'static str,
            value: u64,
            lo: u64,
            hi: u64,
        ) -> Result<(), DomainValidationError> {
            if (lo..=hi).contains(&value) {
                Ok(())
            } else {
                Err(DomainValidationError::new(
                    field,
                    format!("must be in {lo}..={hi}"),
                ))
            }
        }
        range(
            "take_turn_silence_ms",
            self.take_turn_silence_ms,
            TAKE_TURN_SILENCE_RANGE_MS.0,
            TAKE_TURN_SILENCE_RANGE_MS.1,
        )?;
        range(
            "barge_in_confirm_ms",
            self.barge_in_confirm_ms,
            BARGE_IN_CONFIRM_RANGE_MS.0,
            BARGE_IN_CONFIRM_RANGE_MS.1,
        )?;
        range("observation_stale_ms", self.observation_stale_ms, 96, 320)?;
        range("vad_loss_ms", self.vad_loss_ms, 1_000, 60_000)?;
        range("max_defer_ms", self.max_defer_ms, 1_000, 60_000)?;
        range(
            "max_deferred",
            self.max_deferred as u64,
            1,
            MAX_FLOOR_DEFERRED as u64,
        )?;
        range(
            "decision_log",
            self.decision_log as u64,
            1,
            MAX_FLOOR_DECISION_LOG as u64,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_and_every_bound_is_enforced() {
        let ok = FloorConfig::default();
        assert!(ok.validate().is_ok());
        let bad = [
            FloorConfig {
                take_turn_silence_ms: 699,
                ..ok
            },
            FloorConfig {
                take_turn_silence_ms: 1_001,
                ..ok
            },
            FloorConfig {
                barge_in_confirm_ms: 255,
                ..ok
            },
            FloorConfig {
                barge_in_confirm_ms: 801,
                ..ok
            },
            FloorConfig {
                observation_stale_ms: 95,
                ..ok
            },
            FloorConfig {
                observation_stale_ms: 321,
                ..ok
            },
            FloorConfig {
                vad_loss_ms: 999,
                ..ok
            },
            FloorConfig {
                vad_loss_ms: 60_001,
                ..ok
            },
            FloorConfig {
                max_defer_ms: 999,
                ..ok
            },
            FloorConfig {
                max_defer_ms: 60_001,
                ..ok
            },
            FloorConfig {
                max_deferred: 0,
                ..ok
            },
            FloorConfig {
                max_deferred: MAX_FLOOR_DEFERRED + 1,
                ..ok
            },
            FloorConfig {
                decision_log: 0,
                ..ok
            },
            FloorConfig {
                decision_log: MAX_FLOOR_DECISION_LOG + 1,
                ..ok
            },
        ];
        for config in &bad {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }

    #[test]
    fn decisions_serialize_with_closed_snake_case_names() {
        let decision = FloorDecision {
            seq: 1,
            at_ms: 2,
            action: FloorAction::TakeTurn,
            reason: FloorReason::UserTurnEnded,
            holder: FloorHolder::Performer,
            turn: TurnState::UserEnded,
            silence_ms: Some(800),
            scene: Interruptibility::Unknown,
        };
        let value = serde_json::to_value(decision).expect("json");
        assert_eq!(value["action"], "take_turn");
        assert_eq!(value["reason"], "user_turn_ended");
        assert_eq!(value["reason"], decision.reason.name());
        assert_eq!(value["action"], decision.action.name());
    }
}
