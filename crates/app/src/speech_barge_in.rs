//! Barge-in executor (issue #81 S2; ADR-0005 S-3).
//!
//! When the floor policy confirms that the user is speaking over the
//! performer (`Yield` / `user_barge_in_confirmed`: raw VAD activity for at
//! least `barge_in_confirm_ms`, on a *fresh* feed), the app stops the
//! performer's spoken output instead of talking over the user.
//!
//! What this module is, and is not:
//! * It is state and records only. The effect runs in
//!   [`ProductionApp::service_barge_in`](crate::ProductionApp), through the
//!   scheduler's cancel path (`stop_speaking_for`, which applies the same
//!   priority/interruptibility arbitration as `schedule`) and the existing
//!   audio and avatar output interfaces. There is no new privileged route,
//!   and operator stop does not pass through here.
//! * The only trigger is a confirmed `Yield` built from raw VAD evidence. A
//!   partial transcript, a model or Jev output, a tentative (unconfirmed)
//!   run and a stale feed cannot stop anything.
//! * Stopping publishes no content, so it may happen mid-phrase (the commit
//!   barrier guards starting output, not stopping it).
//! * Everything here is bounded: one handled-run key, a 32-entry record ring
//!   and fixed counters. Times are logical (`at_ms`); the one wall-clock
//!   number, `decision_to_stop_micros`, is a measurement and never an input.

use aivtuber_adapters::{AudioStopOutcome, MAX_AUDIO_STOP_FADE_MS};
use std::collections::VecDeque;

/// Retained [`BargeInRecord`]s.
pub const BARGE_IN_RECORDS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BargeInConfig {
    /// Fade requested from the audio output, `0..=150` ms. `0` asks for a
    /// hard stop. Backends that cannot ramp report `HardStopped`.
    pub fade_ms: u32,
}

impl Default for BargeInConfig {
    fn default() -> Self {
        Self { fade_ms: 120 }
    }
}

impl BargeInConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.fade_ms > MAX_AUDIO_STOP_FADE_MS {
            return Err(format!(
                "barge-in fade must be 0 to {MAX_AUDIO_STOP_FADE_MS} ms"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeInEffect {
    /// At least one audio performance was cancelled.
    Stopped,
    /// Confirmed, but nothing the user's speech may stop: nothing audible,
    /// or only better-ranked or non-interruptible performances.
    NothingToStop,
}

/// What one confirmed barge-in decided and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BargeInRecord {
    /// Sequence of the floor decision that confirmed it.
    pub decision_seq: u64,
    /// Logical time of the confirmed decision; also the scheduler's
    /// cancellation time.
    pub decided_at_ms: u64,
    pub effect: BargeInEffect,
    pub cancelled_playing: usize,
    pub cancelled_queued: usize,
    pub fade_ms: u32,
    /// What the audio output did; `None` when there was nothing to stop.
    pub audio: Option<AudioStopOutcome>,
    pub audio_error: bool,
    /// Whether the avatar `Reset` was accepted; `None` when not attempted.
    pub avatar_reset: Option<bool>,
    /// Wall time from the confirmed decision to the stop commands having
    /// been handed to the outputs. Measurement only.
    pub decision_to_stop_micros: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BargeInMetrics {
    pub confirmed: u64,
    pub stopped: u64,
    pub nothing_to_stop: u64,
    pub audio_faded: u64,
    pub audio_hard_stopped: u64,
    /// The output has no stop: audio already started plays out.
    pub audio_stop_unsupported: u64,
    pub audio_stop_errors: u64,
    pub avatar_resets: u64,
    pub avatar_reset_errors: u64,
    pub max_decision_to_stop_micros: u64,
}

#[derive(Debug)]
pub(crate) struct BargeIn {
    config: BargeInConfig,
    /// The user speech run (speaker, raw start edge) already acted on, so one
    /// run is decided and executed once however long it lasts.
    handled: Option<(String, u64)>,
    records: VecDeque<BargeInRecord>,
    metrics: BargeInMetrics,
}

impl BargeIn {
    pub(crate) fn new(config: BargeInConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            handled: None,
            records: VecDeque::with_capacity(BARGE_IN_RECORDS),
            metrics: BargeInMetrics::default(),
        })
    }

    pub(crate) fn config(&self) -> BargeInConfig {
        self.config
    }

    pub(crate) fn metrics(&self) -> BargeInMetrics {
        self.metrics
    }

    pub(crate) fn recent(&self) -> Vec<BargeInRecord> {
        self.records.iter().cloned().collect()
    }

    pub(crate) fn is_handled(&self, speaker: &str, run_start_ms: Option<u64>) -> bool {
        run_start_ms.is_some_and(|edge| {
            self.handled
                .as_ref()
                .is_some_and(|(handled, at)| handled == speaker && *at == edge)
        })
    }

    pub(crate) fn mark_handled(&mut self, speaker: &str, run_start_ms: Option<u64>) {
        if let Some(edge) = run_start_ms {
            self.handled = Some((speaker.to_owned(), edge));
        }
    }

    pub(crate) fn record(&mut self, record: BargeInRecord) {
        let m = &mut self.metrics;
        m.confirmed += 1;
        match record.effect {
            BargeInEffect::Stopped => m.stopped += 1,
            BargeInEffect::NothingToStop => m.nothing_to_stop += 1,
        }
        match record.audio {
            Some(AudioStopOutcome::Faded { .. }) => m.audio_faded += 1,
            Some(AudioStopOutcome::HardStopped) => m.audio_hard_stopped += 1,
            Some(AudioStopOutcome::Unsupported) => m.audio_stop_unsupported += 1,
            Some(AudioStopOutcome::NothingPlaying) | None => {}
        }
        if record.audio_error {
            m.audio_stop_errors += 1;
        }
        match record.avatar_reset {
            Some(true) => m.avatar_resets += 1,
            Some(false) => m.avatar_reset_errors += 1,
            None => {}
        }
        m.max_decision_to_stop_micros = m
            .max_decision_to_stop_micros
            .max(record.decision_to_stop_micros);
        if self.records.len() == BARGE_IN_RECORDS {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }
}
