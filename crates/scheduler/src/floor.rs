//! Deterministic conversational-floor policy (issue #81, slice S1; ADR-0005).
//!
//! [`evaluate`] is a pure function of an explicit [`FloorInput`] and a
//! validated [`FloorConfig`]: no clock, no I/O, no randomness, no model.
//! [`FloorPolicy`] wraps it with a monotonic sequence, a bounded decision ring
//! and counters. Model, Jev or generated output is not an input at all, so it
//! cannot be authority here.
//!
//! Invariants (tested):
//! * Only a waiting reply can yield `TakeTurn`: a partial transcript, a
//!   silence or any other observation can delay a reply, never trigger one.
//! * Raw user speech, a fresh partial or a stale feed never yields
//!   `TakeTurn`.
//! * The policy cancels and dispatches nothing. `Yield` is a recommendation;
//!   the app executes a confirmed one (S2) through the scheduler's cancel
//!   path, which stays the only owner of cancellation, and operator
//!   emergency control does not pass through here.

use aivtuber_domain::{
    DomainValidationError, FallbackTurnSignal, FloorAction, FloorConfig, FloorDecision,
    FloorHolder, FloorInput, FloorReason, OwnSpeech, TranscriptHold, TurnState, UserSpeechEvidence,
};
use serde::Serialize;
use std::collections::VecDeque;

/// How the user's evidence reads after the freshness rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserView {
    /// No evidence (text-only path).
    Absent,
    /// Evidence unusable but the feed is only briefly late: wait.
    Stale,
    /// Feed dead beyond `vad_loss_ms`: degrade to text-only and say so.
    FeedLost,
    /// The last `active` edge is too old: a lost `end` edge.
    LostEdge,
    Speaking {
        run_ms: u64,
    },
    Silent {
        silence_ms: u64,
    },
}

fn user_view(config: &FloorConfig, user: Option<&UserSpeechEvidence>, now_ms: u64) -> UserView {
    let Some(user) = user else {
        return UserView::Absent;
    };
    if let Some(age) = user.feed_age_ms
        && age > config.observation_stale_ms
    {
        return if age > config.vad_loss_ms {
            UserView::FeedLost
        } else {
            UserView::Stale
        };
    }
    match user.signal {
        FallbackTurnSignal::NoSpeech => UserView::Absent,
        FallbackTurnSignal::ActivityStale => UserView::LostEdge,
        FallbackTurnSignal::SpeakerActive => UserView::Speaking {
            // The barge-in run: last raw edge to now.
            run_ms: user
                .last_edge_ms
                .map_or(0, |edge| now_ms.saturating_sub(edge)),
        },
        FallbackTurnSignal::Waiting { silence_ms }
        | FallbackTurnSignal::TurnEndEligible { silence_ms } => UserView::Silent { silence_ms },
    }
}

/// Result of the pure evaluation, before sequencing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorEvaluation {
    pub action: FloorAction,
    pub reason: FloorReason,
    pub holder: FloorHolder,
    pub turn: TurnState,
    pub silence_ms: Option<u64>,
}

/// Pure floor decision. See the crate-level table in ADR-0005 D2/D3/D5/D6/D7.
pub fn evaluate(config: &FloorConfig, input: &FloorInput) -> FloorEvaluation {
    let view = user_view(config, input.user.as_ref(), input.now_ms);
    let silence_ms = match view {
        UserView::Silent { silence_ms } => Some(silence_ms),
        _ => None,
    };
    let turn = match view {
        UserView::Absent | UserView::FeedLost | UserView::LostEdge => TurnState::Idle,
        UserView::Stale => TurnState::UserPaused,
        UserView::Speaking { .. } => TurnState::UserSpeaking,
        UserView::Silent { silence_ms } if silence_ms >= config.take_turn_silence_ms => {
            TurnState::UserEnded
        }
        UserView::Silent { .. } => TurnState::UserPaused,
    };
    let done = |action, reason, holder| FloorEvaluation {
        action,
        reason,
        holder,
        turn,
        silence_ms,
    };

    if input.own == OwnSpeech::Speaking {
        return match view {
            UserView::Speaking { run_ms } if run_ms >= config.barge_in_confirm_ms => done(
                FloorAction::Yield,
                FloorReason::UserBargeInConfirmed,
                FloorHolder::User,
            ),
            UserView::Speaking { .. } => done(
                FloorAction::Hold,
                FloorReason::UserSpeechUnconfirmed,
                FloorHolder::Performer,
            ),
            UserView::Stale => done(
                FloorAction::Hold,
                FloorReason::StaleObservation,
                FloorHolder::Performer,
            ),
            _ => done(
                FloorAction::Hold,
                FloorReason::PerformerSpeaking,
                FloorHolder::Performer,
            ),
        };
    }

    // The performer is idle: the floor is the user's until proven otherwise.
    if matches!(view, UserView::Speaking { .. }) {
        return done(
            FloorAction::Wait,
            FloorReason::UserSpeaking,
            FloorHolder::User,
        );
    }
    if input.hold == TranscriptHold::Holding {
        return done(
            FloorAction::Wait,
            FloorReason::PartialTranscriptHolding,
            FloorHolder::User,
        );
    }
    match view {
        UserView::Stale => {
            return done(
                FloorAction::Wait,
                FloorReason::StaleObservation,
                FloorHolder::Nobody,
            );
        }
        UserView::Silent { silence_ms } if silence_ms < config.take_turn_silence_ms => {
            return done(
                FloorAction::Wait,
                FloorReason::SilenceBelowThreshold,
                FloorHolder::User,
            );
        }
        _ => {}
    }
    if !input.reply_waiting {
        return done(
            FloorAction::Wait,
            FloorReason::NoReplyPending,
            FloorHolder::Nobody,
        );
    }
    let reason = match view {
        UserView::Silent { .. } => FloorReason::UserTurnEnded,
        UserView::LostEdge => FloorReason::ActivityStale,
        UserView::FeedLost => FloorReason::VadUnavailable,
        _ => FloorReason::NoSpeechEvidence,
    };
    done(FloorAction::TakeTurn, reason, FloorHolder::Performer)
}

/// Counters for the floor policy (ADR-0005 D9 subset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FloorMetrics {
    pub decisions: u64,
    pub wait: u64,
    pub hold: u64,
    pub yielded: u64,
    pub take_turn: u64,
    /// Decisions made while the observation feed was not fresh.
    pub stale_observation: u64,
    /// `TakeTurn` granted without usable speech evidence (text-only path,
    /// lost end edge, or a dead feed): the degradation made visible.
    pub fallback_take_turn: u64,
    /// Inputs whose `now_ms` ran backwards and were clamped.
    pub clock_regressions: u64,
}

/// Stateful wrapper: sequence, bounded ring, counters. Holds no model, no
/// clock and no handle to the scheduler.
#[derive(Debug)]
pub struct FloorPolicy {
    config: FloorConfig,
    seq: u64,
    last_at_ms: u64,
    log: VecDeque<FloorDecision>,
    metrics: FloorMetrics,
}

impl FloorPolicy {
    pub fn new(config: FloorConfig) -> Result<Self, DomainValidationError> {
        config.validate()?;
        Ok(Self {
            config,
            seq: 0,
            last_at_ms: 0,
            log: VecDeque::with_capacity(config.decision_log),
            metrics: FloorMetrics::default(),
        })
    }

    pub fn config(&self) -> &FloorConfig {
        &self.config
    }

    pub fn metrics(&self) -> FloorMetrics {
        self.metrics
    }

    /// Most recent decisions, oldest first; at most `decision_log` of them.
    pub fn recent(&self) -> impl Iterator<Item = &FloorDecision> {
        self.log.iter()
    }

    /// Decide and record. `at_ms` never decreases even if the input clock does
    /// (the silence in the evidence is the caller's, computed at its `now`).
    pub fn decide(&mut self, input: &FloorInput) -> FloorDecision {
        let mut input = *input;
        if input.now_ms < self.last_at_ms {
            self.metrics.clock_regressions += 1;
            input.now_ms = self.last_at_ms;
        }
        self.last_at_ms = input.now_ms;
        let evaluation = evaluate(&self.config, &input);
        self.seq += 1;
        let decision = FloorDecision {
            seq: self.seq,
            at_ms: input.now_ms,
            action: evaluation.action,
            reason: evaluation.reason,
            holder: evaluation.holder,
            turn: evaluation.turn,
            silence_ms: evaluation.silence_ms,
        };
        let m = &mut self.metrics;
        m.decisions += 1;
        match decision.action {
            FloorAction::Wait => m.wait += 1,
            FloorAction::Hold => m.hold += 1,
            FloorAction::Yield => m.yielded += 1,
            FloorAction::TakeTurn => m.take_turn += 1,
        }
        if decision.reason == FloorReason::StaleObservation {
            m.stale_observation += 1;
        }
        if decision.action == FloorAction::TakeTurn
            && matches!(
                decision.reason,
                FloorReason::NoSpeechEvidence
                    | FloorReason::ActivityStale
                    | FloorReason::VadUnavailable
            )
        {
            m.fallback_take_turn += 1;
        }
        if self.log.len() == self.config.decision_log {
            self.log.pop_front();
        }
        self.log.push_back(decision);
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        FallbackTurnConfig, SpeakerActivity, SpeakerActivityState, fallback_turn_signal,
    };

    const SILENCE: u64 = 800;

    fn config() -> FloorConfig {
        FloorConfig::default()
    }

    /// Evidence built exactly as the runtime tracker builds it: from a raw
    /// activity state and `now`, through the domain fallback function.
    fn evidence(activity: SpeakerActivity, last_edge_ms: u64, now_ms: u64) -> UserSpeechEvidence {
        let state = SpeakerActivityState {
            activity,
            last_edge_ms,
        };
        UserSpeechEvidence {
            signal: fallback_turn_signal(Some(&state), now_ms, &FallbackTurnConfig::default()),
            last_edge_ms: Some(last_edge_ms),
            feed_age_ms: None,
        }
    }

    fn input(now_ms: u64, user: Option<UserSpeechEvidence>) -> FloorInput {
        FloorInput {
            now_ms,
            user,
            hold: TranscriptHold::None,
            own: OwnSpeech::Idle,
            reply_waiting: true,
        }
    }

    fn eval(input: &FloorInput) -> (FloorAction, FloorReason) {
        let e = evaluate(&config(), input);
        (e.action, e.reason)
    }

    #[test]
    fn user_speaking_waits_and_a_short_pause_waits() {
        let user = Some(evidence(SpeakerActivity::Active, 1_000, 1_500));
        assert_eq!(
            eval(&input(1_500, user)),
            (FloorAction::Wait, FloorReason::UserSpeaking)
        );
        // A pause of about 450 ms followed by resumed speech never takes the turn.
        let paused = Some(evidence(SpeakerActivity::Inactive, 1_000, 1_450));
        assert_eq!(
            eval(&input(1_450, paused)),
            (FloorAction::Wait, FloorReason::SilenceBelowThreshold)
        );
        let resumed = Some(evidence(SpeakerActivity::Active, 1_460, 1_500));
        assert_eq!(
            eval(&input(1_500, resumed)),
            (FloorAction::Wait, FloorReason::UserSpeaking)
        );
    }

    #[test]
    fn the_turn_is_taken_at_exactly_the_silence_threshold() {
        let at = |now| Some(evidence(SpeakerActivity::Inactive, 1_000, now));
        assert_eq!(
            eval(&input(1_000 + SILENCE - 1, at(1_000 + SILENCE - 1))),
            (FloorAction::Wait, FloorReason::SilenceBelowThreshold)
        );
        let e = evaluate(&config(), &input(1_000 + SILENCE, at(1_000 + SILENCE)));
        assert_eq!(
            (e.action, e.reason, e.holder, e.turn, e.silence_ms),
            (
                FloorAction::TakeTurn,
                FloorReason::UserTurnEnded,
                FloorHolder::Performer,
                TurnState::UserEnded,
                Some(SILENCE)
            )
        );
    }

    #[test]
    fn a_partial_only_delays_and_never_triggers() {
        let ended = Some(evidence(SpeakerActivity::Inactive, 1_000, 2_000));
        let mut held = input(2_000, ended);
        held.hold = TranscriptHold::Holding;
        assert_eq!(
            eval(&held),
            (FloorAction::Wait, FloorReason::PartialTranscriptHolding)
        );
        // A stale partial does not hold the floor.
        held.hold = TranscriptHold::Stale;
        assert_eq!(
            eval(&held),
            (FloorAction::TakeTurn, FloorReason::UserTurnEnded)
        );
        // With nothing waiting, no observation can yield TakeTurn, whatever it says.
        let signals = [
            FallbackTurnSignal::NoSpeech,
            FallbackTurnSignal::SpeakerActive,
            FallbackTurnSignal::ActivityStale,
            FallbackTurnSignal::Waiting { silence_ms: 10 },
            FallbackTurnSignal::TurnEndEligible { silence_ms: 5_000 },
        ];
        for signal in signals {
            for hold in [
                TranscriptHold::None,
                TranscriptHold::Holding,
                TranscriptHold::Stale,
            ] {
                for own in [OwnSpeech::Idle, OwnSpeech::Speaking] {
                    for feed_age_ms in [None, Some(0), Some(200), Some(9_000)] {
                        for user in [
                            None,
                            Some(UserSpeechEvidence {
                                signal,
                                last_edge_ms: Some(0),
                                feed_age_ms,
                            }),
                        ] {
                            let i = FloorInput {
                                now_ms: 10_000,
                                user,
                                hold,
                                own,
                                reply_waiting: false,
                            };
                            assert_ne!(evaluate(&config(), &i).action, FloorAction::TakeTurn);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn speech_and_a_short_pause_beat_a_waiting_reply_at_any_fresh_feed_age() {
        for feed_age_ms in [None, Some(0), Some(160)] {
            for signal in [
                FallbackTurnSignal::SpeakerActive,
                FallbackTurnSignal::Waiting { silence_ms: 799 },
            ] {
                let i = FloorInput {
                    now_ms: 10_000,
                    user: Some(UserSpeechEvidence {
                        signal,
                        last_edge_ms: Some(9_900),
                        feed_age_ms,
                    }),
                    hold: TranscriptHold::None,
                    own: OwnSpeech::Idle,
                    reply_waiting: true,
                };
                assert_ne!(evaluate(&config(), &i).action, FloorAction::TakeTurn);
            }
        }
    }

    #[test]
    fn no_vad_is_the_text_only_path() {
        assert_eq!(
            eval(&input(5_000, None)),
            (FloorAction::TakeTurn, FloorReason::NoSpeechEvidence)
        );
        let no_speech = Some(UserSpeechEvidence {
            signal: FallbackTurnSignal::NoSpeech,
            last_edge_ms: None,
            feed_age_ms: None,
        });
        assert_eq!(
            eval(&input(5_000, no_speech)),
            (FloorAction::TakeTurn, FloorReason::NoSpeechEvidence)
        );
        // ...but a fresh partial still delays it.
        let mut held = input(5_000, None);
        held.hold = TranscriptHold::Holding;
        assert_eq!(eval(&held).0, FloorAction::Wait);
    }

    #[test]
    fn a_lost_end_edge_does_not_pin_the_floor() {
        let lost = Some(evidence(SpeakerActivity::Active, 1_000, 1_000 + 61_000));
        assert_eq!(
            eval(&input(62_000, lost)),
            (FloorAction::TakeTurn, FloorReason::ActivityStale)
        );
    }

    #[test]
    fn a_stale_feed_waits_then_degrades_to_text_only_with_a_reason() {
        let feed = |age| {
            Some(UserSpeechEvidence {
                signal: FallbackTurnSignal::TurnEndEligible { silence_ms: 5_000 },
                last_edge_ms: Some(0),
                feed_age_ms: Some(age),
            })
        };
        // At the bound the feed is still fresh.
        assert_eq!(
            eval(&input(9_000, feed(160))),
            (FloorAction::TakeTurn, FloorReason::UserTurnEnded)
        );
        // Frozen feed: never TakeTurn, even though the silence reads 5 s.
        assert_eq!(
            eval(&input(9_000, feed(161))),
            (FloorAction::Wait, FloorReason::StaleObservation)
        );
        assert_eq!(
            eval(&input(9_000, feed(5_000))),
            (FloorAction::Wait, FloorReason::StaleObservation)
        );
        // Dead feed: degrade to the text-only path and record why.
        assert_eq!(
            eval(&input(9_000, feed(5_001))),
            (FloorAction::TakeTurn, FloorReason::VadUnavailable)
        );
        // While speaking, a stale feed never produces Yield.
        let mut speaking = input(9_000, feed(300));
        speaking.own = OwnSpeech::Speaking;
        assert_eq!(
            eval(&speaking),
            (FloorAction::Hold, FloorReason::StaleObservation)
        );
    }

    #[test]
    fn barge_in_is_tentative_until_the_confirm_run() {
        let speaking = |now| {
            let mut i = input(now, Some(evidence(SpeakerActivity::Active, 1_000, now)));
            i.own = OwnSpeech::Speaking;
            i
        };
        assert_eq!(
            eval(&speaking(1_399)),
            (FloorAction::Hold, FloorReason::UserSpeechUnconfirmed)
        );
        let e = evaluate(&config(), &speaking(1_400));
        assert_eq!(
            (e.action, e.reason, e.holder),
            (
                FloorAction::Yield,
                FloorReason::UserBargeInConfirmed,
                FloorHolder::User
            )
        );
        // A user who is silent while the performer speaks changes nothing.
        let mut quiet = input(
            1_400,
            Some(evidence(SpeakerActivity::Inactive, 1_000, 1_400)),
        );
        quiet.own = OwnSpeech::Speaking;
        assert_eq!(
            eval(&quiet),
            (FloorAction::Hold, FloorReason::PerformerSpeaking)
        );
    }

    #[test]
    fn a_reply_waits_while_the_performer_speaks() {
        let mut i = input(5_000, None);
        i.own = OwnSpeech::Speaking;
        assert_eq!(
            eval(&i),
            (FloorAction::Hold, FloorReason::PerformerSpeaking)
        );
    }

    #[test]
    fn evaluation_is_pure_and_the_policy_is_deterministic() {
        let user = Some(evidence(SpeakerActivity::Inactive, 1_000, 1_900));
        let i = input(1_900, user);
        let first = evaluate(&config(), &i);
        for _ in 0..1_000 {
            assert_eq!(evaluate(&config(), &i), first);
        }
        let run = || {
            let mut policy = FloorPolicy::new(config()).expect("policy");
            (0..50_u64)
                .map(|n| {
                    let now = 1_000 + n * 40;
                    let activity = if n % 7 < 3 {
                        SpeakerActivity::Active
                    } else {
                        SpeakerActivity::Inactive
                    };
                    policy.decide(&input(
                        now,
                        Some(evidence(activity, 1_000 + n / 7 * 280, now)),
                    ))
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn state_is_bounded_and_the_clock_never_runs_backwards() {
        let mut policy = FloorPolicy::new(FloorConfig {
            decision_log: 4,
            ..config()
        })
        .expect("policy");
        for n in 0..100_u64 {
            policy.decide(&input(1_000 + n, None));
        }
        assert_eq!(policy.recent().count(), 4);
        assert_eq!(policy.recent().last().map(|d| d.seq), Some(100));
        assert_eq!(policy.metrics().decisions, 100);
        let back = policy.decide(&input(5, None));
        assert_eq!(back.at_ms, 1_099);
        assert_eq!(policy.metrics().clock_regressions, 1);
        assert!(
            FloorPolicy::new(FloorConfig {
                max_deferred: 0,
                ..config()
            })
            .is_err()
        );
    }

    #[test]
    fn counters_record_the_degradations() {
        let mut policy = FloorPolicy::new(config()).expect("policy");
        policy.decide(&input(1_000, None));
        let stale = Some(UserSpeechEvidence {
            signal: FallbackTurnSignal::NoSpeech,
            last_edge_ms: None,
            feed_age_ms: Some(300),
        });
        policy.decide(&input(1_100, stale));
        let m = policy.metrics();
        assert_eq!(
            (
                m.take_turn,
                m.fallback_take_turn,
                m.stale_observation,
                m.wait
            ),
            (1, 1, 1, 1)
        );
    }
}
