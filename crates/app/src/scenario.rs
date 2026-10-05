//! Issue #70: the *cached replay consumer's* ceiling on a shared scenario.
//!
//! The scenario document contract ([`aivtuber_domain::scenario`]) is deliberately
//! consumer-neutral: it validates only what a document needs to be well formed and
//! representable. What a workload is *playable* is a property of this consumer -
//! the starter pack and the scheduler behind `replay-benchmark` - so the measured
//! ceiling lives here, next to the measurement that justifies it, rather than in
//! the shared format. A #40 soak or stress consumer that queues rather than
//! preempts has a different ceiling and must not have this one imposed on it.

use crate::AppError;
use aivtuber_domain::StreamScenario;

/// Highest event rate the *current replay consumer* can play, in events per
/// minute.
///
/// Measured against the starter pack, not guessed. Sweeping the shipped runtime
/// through cached playback:
/// * chat-only completes at 15 events/minute over two minutes, and 20 trips the
///   per-asset cooldown (`Rejection::Cooldown`);
/// * mixing any higher-priority kind - `game.event` or `chat.donation` -
///   completes at 10 and fails at 15 with `Rejection::Priority`, because the
///   scheduler will not preempt a playing item for a higher-ranked one;
/// * `timer.tick` and `stream.event` do not reach the generative replay's event
///   accounting at all, at any rate.
///
/// 10 is therefore the ceiling for this consumer's mixed workload. Raising it is
/// a runtime change with a measured justification, and this constant is where
/// that gets recorded.
pub const MAX_PLAYABLE_EVENTS_PER_MINUTE: f64 = 10.0;

/// Whether *this* consumer can play a scenario.
///
/// An extension trait rather than an inherent method: the ceiling belongs to the
/// consumer, so it cannot be added to the provider-neutral type in
/// `aivtuber-domain`. Keeping the two apart is what lets a #40 soak or stress
/// consumer load a workload this one refuses - and it is why the scenario corpus
/// can describe a burst the benchmark cannot yet run.
pub trait CachedReplayPlayability {
    /// Refuse a workload the cached replay path cannot play, naming the limit.
    ///
    /// Reported as an ordinary error so the refusal names the limit and where it
    /// comes from, rather than surfacing as a benchmark job that aborts halfway
    /// with `Rejection::Cooldown`.
    fn validate_playability_for_cached_replay(&self) -> Result<(), AppError>;
}

impl CachedReplayPlayability for StreamScenario {
    fn validate_playability_for_cached_replay(&self) -> Result<(), AppError> {
        self.validate()?;
        for (index, phase) in self.phases.iter().enumerate() {
            if phase.events_per_minute > MAX_PLAYABLE_EVENTS_PER_MINUTE {
                return Err(AppError::Routing(format!(
                    "scenario {:?} phase {index} ({}) runs at {} events/minute, above the {MAX_PLAYABLE_EVENTS_PER_MINUTE} events/minute the cached replay path can play; cached playback rejects the overlap instead of queueing it, so a denser workload aborts the run rather than producing a measurement. The scenario itself is well-formed, so a consumer with a different ceiling may still run it. Raise the runtime limit with evidence, or model the burst as a ramp",
                    self.scenario_id, phase.name, phase.events_per_minute
                )));
            }
        }
        Ok(())
    }
}
