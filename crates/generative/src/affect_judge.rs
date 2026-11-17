//! Opt-in System One affect judgement, run in parallel with TTS (issue #426).
//!
//! The judge answers one closed six-class question about a phrase's gated
//! display text. It is advisory data only: the answer goes through the same
//! mapping as the rules classifier (pack presets, intensity cap,
//! anti-repetition), and it can never reach control, security or memory paths.
//!
//! Invariants:
//!
//! * Audio never waits for the judge. [`AffectJudgeRunner::start`] spawns one
//!   bounded worker and returns at once; [`AffectJudgeTicket::resolve`] only
//!   polls, so a slow, failed or rate-limited call costs the phrase nothing
//!   and the S1 rules label is used instead.
//! * At most [`MAX_JUDGE_IN_FLIGHT`] calls exist at any time; an extra phrase
//!   falls back to the rules label without any request.
//! * Every worker is bounded by the hard cap handed to the judge as its
//!   transport deadline, so no worker outlives a cancelled reply by more than
//!   that cap.
//! * The text sent is the phrase's display text after the output gate, cut to
//!   [`AFFECT_MAX_INPUT_CHARS`]. Text is never logged or stored here.

use crate::affect::{AFFECT_MAX_INPUT_CHARS, AffectClass};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Calls allowed in flight at once.
pub const MAX_JUDGE_IN_FLIGHT: usize = 2;
/// Hard cap on the time from request to answer; also the call's deadline.
pub const DEFAULT_JUDGE_HARD_CAP: Duration = Duration::from_millis(1_200);
/// Latency samples kept for the status percentiles.
const LATENCY_WINDOW: usize = 256;
/// Per-reply decisions kept in a trace (a reply has a handful of phrases).
pub const MAX_AFFECT_DECISIONS: usize = 64;

/// Why the judge could not answer. Typed, content-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffectJudgeFailure {
    Timeout,
    RateLimited,
    Unavailable,
    Malformed,
}

/// A System One style judge: one blocking, bounded call per phrase.
pub trait AffectJudge: Send + Sync + 'static {
    /// Classify `text` (already cut to the input bound). Must return within
    /// `deadline` or fail with [`AffectJudgeFailure::Timeout`].
    fn judge(&self, text: &str, deadline: Duration) -> Result<AffectClass, AffectJudgeFailure>;
}

/// Why a phrase used the rules label although the judge was enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AffectFallbackReason {
    /// No answer by the time the audio was ready (or after the hard cap).
    Late,
    Timeout,
    RateLimited,
    Unavailable,
    Malformed,
    /// Already [`MAX_JUDGE_IN_FLIGHT`] calls in flight; no request was made.
    Busy,
    /// The reply was cancelled, or the phrase ended, before audio was ready.
    Cancelled,
}

impl AffectFallbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Late => "late",
            Self::Timeout => "timeout",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::Malformed => "malformed",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
        }
    }
}

impl From<AffectJudgeFailure> for AffectFallbackReason {
    fn from(failure: AffectJudgeFailure) -> Self {
        match failure {
            AffectJudgeFailure::Timeout => Self::Timeout,
            AffectJudgeFailure::RateLimited => Self::RateLimited,
            AffectJudgeFailure::Unavailable => Self::Unavailable,
            AffectJudgeFailure::Malformed => Self::Malformed,
        }
    }
}

/// Where a phrase's affect class came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", content = "reason", rename_all = "snake_case")]
pub enum AffectSource {
    Judge,
    RulesFallback(AffectFallbackReason),
}

/// Replay record of one phrase's affect decision. Content-free: the class and
/// its source, never the text. Replay reproduces the expression from the
/// compiled asset; this record says how it was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectDecision {
    pub seq: u32,
    #[serde(flatten)]
    pub source: AffectSource,
    pub class: AffectClass,
}

/// Operator-visible counters. Never contains text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectJudgeStatus {
    /// Phrases whose expression came from the judge.
    pub judged: u64,
    pub late: u64,
    pub timeout: u64,
    pub rate_limited: u64,
    pub unavailable: u64,
    pub malformed: u64,
    pub busy: u64,
    pub cancelled: u64,
    /// Latency of calls that answered (late ones included), in ms.
    pub latency_samples: u64,
    pub latency_p50_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct StatsInner {
    counts: AffectJudgeStatus,
    latencies: VecDeque<u64>,
}

/// Shared, bounded counters of one judge.
#[derive(Debug, Clone, Default)]
pub struct AffectJudgeStats(Arc<Mutex<StatsInner>>);

impl AffectJudgeStats {
    fn with<T>(&self, f: impl FnOnce(&mut StatsInner) -> T) -> T {
        f(&mut self.0.lock().expect("affect judge stats lock"))
    }

    fn record_judged(&self) {
        self.with(|s| s.counts.judged += 1);
    }

    fn record_fallback(&self, reason: AffectFallbackReason) {
        self.with(|s| {
            let c = &mut s.counts;
            let slot = match reason {
                AffectFallbackReason::Late => &mut c.late,
                AffectFallbackReason::Timeout => &mut c.timeout,
                AffectFallbackReason::RateLimited => &mut c.rate_limited,
                AffectFallbackReason::Unavailable => &mut c.unavailable,
                AffectFallbackReason::Malformed => &mut c.malformed,
                AffectFallbackReason::Busy => &mut c.busy,
                AffectFallbackReason::Cancelled => &mut c.cancelled,
            };
            *slot += 1;
        });
    }

    fn record_latency(&self, latency: Duration) {
        self.with(|s| {
            if s.latencies.len() == LATENCY_WINDOW {
                s.latencies.pop_front();
            }
            s.latencies
                .push_back(latency.as_millis().min(u128::from(u64::MAX)) as u64);
        });
    }

    pub fn snapshot(&self) -> AffectJudgeStatus {
        self.with(|s| {
            let mut sorted: Vec<u64> = s.latencies.iter().copied().collect();
            sorted.sort_unstable();
            let pick = |q: f64| -> Option<u64> {
                if sorted.is_empty() {
                    return None;
                }
                let rank = ((sorted.len() as f64) * q).ceil() as usize;
                Some(sorted[rank.clamp(1, sorted.len()) - 1])
            };
            AffectJudgeStatus {
                latency_samples: sorted.len() as u64,
                latency_p50_ms: pick(0.50),
                latency_p95_ms: pick(0.95),
                ..s.counts
            }
        })
    }
}

struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Starts judge calls for phrases. Cheap to clone; clones share the bounds
/// and counters.
#[derive(Clone)]
pub struct AffectJudgeRunner {
    judge: Arc<dyn AffectJudge>,
    hard_cap: Duration,
    in_flight: Arc<AtomicUsize>,
    stats: AffectJudgeStats,
}

impl std::fmt::Debug for AffectJudgeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AffectJudgeRunner")
            .field("hard_cap", &self.hard_cap)
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .finish()
    }
}

impl AffectJudgeRunner {
    pub fn new(judge: Arc<dyn AffectJudge>, hard_cap: Duration) -> Result<Self, String> {
        if hard_cap.is_zero() || hard_cap > DEFAULT_JUDGE_HARD_CAP {
            return Err(format!(
                "affect judge hard cap must be in 1..={} ms",
                DEFAULT_JUDGE_HARD_CAP.as_millis()
            ));
        }
        Ok(Self {
            judge,
            hard_cap,
            in_flight: Arc::new(AtomicUsize::new(0)),
            stats: AffectJudgeStats::default(),
        })
    }

    pub fn stats(&self) -> AffectJudgeStats {
        self.stats.clone()
    }

    /// Calls currently running (workers that have not returned yet).
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    fn try_acquire(&self) -> Option<InFlightGuard> {
        let mut current = self.in_flight.load(Ordering::Acquire);
        loop {
            if current >= MAX_JUDGE_IN_FLIGHT {
                return None;
            }
            match self.in_flight.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(InFlightGuard(Arc::clone(&self.in_flight))),
                Err(seen) => current = seen,
            }
        }
    }

    /// Start the judge call for one phrase and return immediately.
    pub fn start(&self, display_text: &str) -> AffectJudgeTicket {
        let Some(guard) = self.try_acquire() else {
            return AffectJudgeTicket::settled(
                self.stats.clone(),
                Settled::Fallback(AffectFallbackReason::Busy),
            );
        };
        let text: String = display_text.chars().take(AFFECT_MAX_INPUT_CHARS).collect();
        let (sender, receiver) = mpsc::channel();
        let judge = Arc::clone(&self.judge);
        let stats = self.stats.clone();
        let hard_cap = self.hard_cap;
        let spawned = std::thread::Builder::new()
            .name("affect-judge".to_owned())
            .spawn(move || {
                let _guard = guard;
                let started = Instant::now();
                let result = judge.judge(&text, hard_cap);
                let elapsed = started.elapsed();
                if result.is_ok() {
                    stats.record_latency(elapsed);
                }
                let _ = sender.send((result, elapsed));
            });
        match spawned {
            Ok(_) => AffectJudgeTicket {
                stats: self.stats.clone(),
                hard_cap,
                state: TicketState::Pending(receiver),
            },
            // The closure (and its guard) was dropped by the failed spawn.
            Err(_) => AffectJudgeTicket::settled(
                self.stats.clone(),
                Settled::Fallback(AffectFallbackReason::Unavailable),
            ),
        }
    }
}

enum Settled {
    Fallback(AffectFallbackReason),
}

enum TicketState {
    Pending(Receiver<(Result<AffectClass, AffectJudgeFailure>, Duration)>),
    Settled(Settled),
    Done,
}

/// The pending answer of one phrase. Dropping it unresolved (reply cancelled,
/// barge-in, operator stop, synthesis failure) counts as a cancelled phrase
/// and discards whatever the worker later returns.
pub struct AffectJudgeTicket {
    stats: AffectJudgeStats,
    hard_cap: Duration,
    state: TicketState,
}

impl AffectJudgeTicket {
    fn settled(stats: AffectJudgeStats, settled: Settled) -> Self {
        Self {
            stats,
            hard_cap: DEFAULT_JUDGE_HARD_CAP,
            state: TicketState::Settled(settled),
        }
    }

    /// Fix the phrase's label now, without waiting: the judge's class when it
    /// has already answered within the hard cap, otherwise the reason the
    /// rules label is used.
    pub fn resolve(mut self) -> Result<AffectClass, AffectFallbackReason> {
        let outcome = match std::mem::replace(&mut self.state, TicketState::Done) {
            TicketState::Pending(receiver) => match receiver.try_recv() {
                Ok((Ok(class), elapsed)) if elapsed <= self.hard_cap => Ok(class),
                Ok((Ok(_), _)) => Err(AffectFallbackReason::Late),
                Ok((Err(failure), _)) => Err(failure.into()),
                Err(TryRecvError::Empty) => Err(AffectFallbackReason::Late),
                Err(TryRecvError::Disconnected) => Err(AffectFallbackReason::Unavailable),
            },
            TicketState::Settled(Settled::Fallback(reason)) => Err(reason),
            TicketState::Done => Err(AffectFallbackReason::Cancelled),
        };
        match outcome {
            Ok(_) => self.stats.record_judged(),
            Err(reason) => self.stats.record_fallback(reason),
        }
        outcome
    }
}

impl Drop for AffectJudgeTicket {
    fn drop(&mut self) {
        if matches!(
            self.state,
            TicketState::Pending(_) | TicketState::Settled(_)
        ) {
            self.stats.record_fallback(AffectFallbackReason::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        delay: Duration,
        answer: Result<AffectClass, AffectJudgeFailure>,
    }

    impl AffectJudge for Scripted {
        fn judge(
            &self,
            _text: &str,
            _deadline: Duration,
        ) -> Result<AffectClass, AffectJudgeFailure> {
            std::thread::sleep(self.delay);
            self.answer
        }
    }

    fn runner(delay_ms: u64, answer: Result<AffectClass, AffectJudgeFailure>) -> AffectJudgeRunner {
        AffectJudgeRunner::new(
            Arc::new(Scripted {
                delay: Duration::from_millis(delay_ms),
                answer,
            }),
            Duration::from_millis(400),
        )
        .expect("runner")
    }

    fn wait_idle(runner: &AffectJudgeRunner) {
        let until = Instant::now() + Duration::from_secs(3);
        while runner.in_flight() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(runner.in_flight(), 0, "a worker outlived its bound");
    }

    #[test]
    fn an_answer_in_time_is_used() {
        let runner = runner(5, Ok(AffectClass::Joy));
        let ticket = runner.start("hello");
        wait_idle(&runner);
        assert_eq!(ticket.resolve(), Ok(AffectClass::Joy));
        assert_eq!(runner.stats().snapshot().judged, 1);
        assert!(runner.stats().snapshot().latency_p50_ms.is_some());
        wait_idle(&runner);
    }

    #[test]
    fn a_late_answer_falls_back_without_waiting() {
        let runner = runner(250, Ok(AffectClass::Joy));
        let ticket = runner.start("hello");
        let started = Instant::now();
        assert_eq!(ticket.resolve(), Err(AffectFallbackReason::Late));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "resolve waited"
        );
        assert_eq!(runner.stats().snapshot().late, 1);
        wait_idle(&runner);
    }

    #[test]
    fn failures_map_to_their_reason() {
        for (failure, reason) in [
            (AffectJudgeFailure::Timeout, AffectFallbackReason::Timeout),
            (
                AffectJudgeFailure::RateLimited,
                AffectFallbackReason::RateLimited,
            ),
            (
                AffectJudgeFailure::Unavailable,
                AffectFallbackReason::Unavailable,
            ),
            (
                AffectJudgeFailure::Malformed,
                AffectFallbackReason::Malformed,
            ),
        ] {
            let runner = runner(0, Err(failure));
            let ticket = runner.start("hello");
            wait_idle(&runner);
            assert_eq!(ticket.resolve(), Err(reason));
        }
    }

    #[test]
    fn at_most_two_calls_are_in_flight() {
        let runner = runner(150, Ok(AffectClass::Calm));
        let a = runner.start("a");
        let b = runner.start("b");
        let c = runner.start("c");
        assert_eq!(runner.in_flight(), 2);
        assert_eq!(c.resolve(), Err(AffectFallbackReason::Busy));
        assert_eq!(runner.stats().snapshot().busy, 1);
        drop((a, b));
        wait_idle(&runner);
    }

    #[test]
    fn a_dropped_ticket_counts_as_cancelled_and_leaves_no_worker() {
        let runner = runner(100, Ok(AffectClass::Joy));
        drop(runner.start("hello"));
        assert_eq!(runner.stats().snapshot().cancelled, 1);
        wait_idle(&runner);
        assert_eq!(runner.stats().snapshot().judged, 0);
    }

    #[test]
    fn the_input_is_cut_to_the_bound() {
        struct Len(Arc<Mutex<usize>>);
        impl AffectJudge for Len {
            fn judge(&self, text: &str, _d: Duration) -> Result<AffectClass, AffectJudgeFailure> {
                *self.0.lock().unwrap() = text.chars().count();
                Ok(AffectClass::Neutral)
            }
        }
        let seen = Arc::new(Mutex::new(0));
        let runner =
            AffectJudgeRunner::new(Arc::new(Len(Arc::clone(&seen))), Duration::from_millis(400))
                .expect("runner");
        let ticket = runner.start(&"あ".repeat(5_000));
        wait_idle(&runner);
        let _ = ticket.resolve();
        assert_eq!(*seen.lock().unwrap(), AFFECT_MAX_INPUT_CHARS);
    }

    #[test]
    fn the_hard_cap_is_validated() {
        let judge: Arc<dyn AffectJudge> = Arc::new(Scripted {
            delay: Duration::ZERO,
            answer: Ok(AffectClass::Neutral),
        });
        assert!(AffectJudgeRunner::new(Arc::clone(&judge), Duration::ZERO).is_err());
        assert!(AffectJudgeRunner::new(Arc::clone(&judge), Duration::from_millis(1_201)).is_err());
        assert!(AffectJudgeRunner::new(judge, DEFAULT_JUDGE_HARD_CAP).is_ok());
    }

    /// What the opt-in adds to the audio path itself is `start` (spawn one
    /// bounded worker) plus a non-blocking `resolve`; the call is hidden under
    /// synthesis. Loose bound so a loaded CI host does not flake; the real
    /// figure is printed.
    #[test]
    fn start_plus_resolve_costs_next_to_nothing_on_the_audio_path() {
        let runner = runner(0, Ok(AffectClass::Joy));
        let mut costs: Vec<Duration> = (0..200)
            .map(|_| {
                let t = Instant::now();
                let ticket = runner.start("hello");
                let _ = ticket.resolve();
                let cost = t.elapsed();
                wait_idle(&runner);
                cost
            })
            .collect();
        costs.sort();
        eprintln!(
            "judge start+resolve: p50={:?} p95={:?} max={:?}",
            costs[100], costs[189], costs[199]
        );
        assert!(costs[100] < Duration::from_millis(5), "{:?}", costs[100]);
    }

    #[test]
    fn decisions_round_trip_through_json_for_every_source() {
        for source in [
            AffectSource::Judge,
            AffectSource::RulesFallback(AffectFallbackReason::Late),
            AffectSource::RulesFallback(AffectFallbackReason::RateLimited),
        ] {
            let decision = AffectDecision {
                seq: 3,
                source,
                class: AffectClass::Sadness,
            };
            let json = serde_json::to_string(&decision).expect("serialize");
            assert!(!json.contains("text"), "{json}");
            assert_eq!(
                serde_json::from_str::<AffectDecision>(&json).unwrap(),
                decision
            );
        }
        let json = serde_json::to_value(AffectDecision {
            seq: 0,
            source: AffectSource::RulesFallback(AffectFallbackReason::Busy),
            class: AffectClass::Joy,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"seq":0,"source":"rules_fallback","reason":"busy","class":"joy"})
        );
    }
}
