//! Cadence, yield gate and latency-probe governor for live screen perception
//! (issue #90 S1b, ADR-0007 D-6/D-7, EXP-0004/0004b/0004c).
//!
//! A pure, deterministic state machine on the caller's monotonic (or logical)
//! clock. It owns no model, OBS, scheduler or output handle and performs no
//! I/O: the composition root asks [`PerceptionGovernor::poll`] what to start
//! and reports each result with [`PerceptionGovernor::on_result`]. That keeps
//! the rules that protect the performer testable without a model.
//!
//! * **One in flight, never faster than the cadence.** A capture or probe is
//!   dispatched only when nothing is in flight, and the next capture is due
//!   one cadence after the previous one *finished*. The cadence has a floor
//!   ([`MIN_PERCEPTION_CADENCE_MS`]); a faster setting is refused, not clamped.
//! * **Yield gate.** Nothing is dispatched while Thinking is generating or
//!   while speech (TTS playback or user speech) is active, and not until the
//!   gate has stayed open for `gate_settle_ms`, so a caption does not land in
//!   the first moments of the next utterance. A deferred capture simply waits;
//!   no queue builds up. EXP-0004b: with two slots, captions every 10 s took
//!   Thinking first-token p95 from 1.42x to 0.98x of baseline with this gate.
//! * **Spill detection by latency probe, not by free VRAM.** A vision model
//!   that spilled out of GPU memory still loads and answers, only much slower
//!   (EXP-0004). A fixed small generation is timed at start and periodically;
//!   a probe above `probe_ceiling_ms`, or above `probe_growth_percent` of the
//!   start baseline, suspends perception. Consecutive late or failed captures
//!   suspend it too. A suspended path stays off until the operator re-enables
//!   it, and its status and reason stay visible.
//! * **Disable wins.** [`PerceptionGovernor::disable`] and
//!   [`PerceptionGovernor::suspend`] bump the epoch and clear the in-flight
//!   slot, so a result that arrives afterwards is discarded and a worker that
//!   checks the epoch before touching OBS never captures again.

use crate::RuntimeError;
use serde::Serialize;

/// Hard floor on the capture cadence (ADR-0007 D-Q9).
pub const MIN_PERCEPTION_CADENCE_MS: u64 = 5_000;
/// The measured default cadence (EXP-0004b: 10 s with two slots and the gate).
pub const DEFAULT_PERCEPTION_CADENCE_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerceptionGovernorConfig {
    /// Time from the end of one capture to the start of the next.
    pub cadence_ms: u64,
    /// The yield gate must have been open this long before a dispatch.
    pub gate_settle_ms: u64,
    /// A capture whose model call took longer than this is late: its
    /// observation is dropped and it counts towards suspension.
    pub caption_deadline_ms: u64,
    /// Consecutive late captures that suspend perception.
    pub late_strikes_to_suspend: u32,
    /// Consecutive failed captures or probes that suspend perception.
    pub failure_strikes_to_suspend: u32,
    /// Time from the end of one probe to the next.
    pub probe_interval_ms: u64,
    /// A probe slower than this is a spill. EXP-0004b/0004c on the reference
    /// 8 GB card: 59 to 86 ms healthy, 179 ms spilled.
    pub probe_ceiling_ms: u64,
    /// A periodic probe above this percentage of the start baseline is a
    /// spill even when under the ceiling.
    pub probe_growth_percent: u64,
}

impl Default for PerceptionGovernorConfig {
    fn default() -> Self {
        Self {
            cadence_ms: DEFAULT_PERCEPTION_CADENCE_MS,
            gate_settle_ms: 750,
            caption_deadline_ms: 3_000,
            late_strikes_to_suspend: 3,
            failure_strikes_to_suspend: 5,
            probe_interval_ms: 60_000,
            probe_ceiling_ms: 150,
            probe_growth_percent: 200,
        }
    }
}

impl PerceptionGovernorConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let bad = RuntimeError::InvalidConfiguration;
        if !(MIN_PERCEPTION_CADENCE_MS..=3_600_000).contains(&self.cadence_ms) {
            return Err(bad("perception cadence_ms must be in 5000..=3600000"));
        }
        if self.gate_settle_ms > 60_000 {
            return Err(bad("perception gate_settle_ms must be at most 60000"));
        }
        if !(100..=60_000).contains(&self.caption_deadline_ms) {
            return Err(bad("perception caption_deadline_ms must be in 100..=60000"));
        }
        if !(1..=100).contains(&self.late_strikes_to_suspend)
            || !(1..=100).contains(&self.failure_strikes_to_suspend)
        {
            return Err(bad("perception strike limits must be in 1..=100"));
        }
        if !(5_000..=3_600_000).contains(&self.probe_interval_ms) {
            return Err(bad(
                "perception probe_interval_ms must be in 5000..=3600000",
            ));
        }
        if !(1..=60_000).contains(&self.probe_ceiling_ms) {
            return Err(bad("perception probe_ceiling_ms must be in 1..=60000"));
        }
        if !(101..=10_000).contains(&self.probe_growth_percent) {
            return Err(bad(
                "perception probe_growth_percent must be in 101..=10000",
            ));
        }
        Ok(())
    }
}

/// What the runtime says about the performer right now. Read by the yield
/// gate only; it never carries text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerceptionGates {
    /// A generation (Thinking) is running or its result has not drained.
    pub thinking_generating: bool,
    /// TTS/performer speech is playing or queued, or a user is speaking, or a
    /// spoken final is waiting to be answered.
    pub speech_active: bool,
}

impl PerceptionGates {
    pub const fn is_open(self) -> bool {
        !self.thinking_generating && !self.speech_active
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerceptionStatus {
    /// Not enabled (the default) or disabled by the operator.
    Off,
    /// Enabled; waiting for the start latency probe.
    Probing,
    Active,
    /// Disabled by the runtime. Stays off until the operator re-enables it.
    Suspended,
}

impl PerceptionStatus {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Probing => "probing",
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
}

/// Why the runtime suspended perception.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendReason {
    /// The model server did not answer the start probe.
    StartProbeFailed,
    /// The model was already too slow at start (spilled or overloaded).
    StartProbeSpill,
    /// A periodic probe showed the model slowed down (spill).
    LatencySpill,
    /// Consecutive captures missed the caption deadline.
    SlowCaptions,
    /// Consecutive captures or probes failed.
    RepeatedFailures,
}

impl SuspendReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::StartProbeFailed => "start_probe_failed",
            Self::StartProbeSpill => "start_probe_spill",
            Self::LatencySpill => "latency_spill",
            Self::SlowCaptions => "slow_captions",
            Self::RepeatedFailures => "repeated_failures",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerceptionJob {
    /// Timed text-only generation: spill detection.
    Probe,
    /// Capture one frame and describe it.
    Capture,
}

/// A job the composition root should start now, tagged with the epoch it
/// belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerceptionDispatch {
    pub job: PerceptionJob,
    pub epoch: u64,
}

/// What a finished job reports. Durations are the model call's, in
/// milliseconds on the same clock family as the governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    ProbeDone {
        latency_ms: u64,
    },
    ProbeFailed,
    CaptureDone {
        model_latency_ms: u64,
    },
    CaptureFailed,
    /// The worker found the epoch stale and did nothing.
    Skipped,
}

/// What to do with a reported result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultDisposition {
    /// A capture result within its deadline: ingest the observation.
    Ingest,
    /// A capture result past its deadline: drop the observation.
    DropLate,
    /// Nothing to ingest (a probe, a failure or a skip).
    NoObservation,
    /// Belongs to a superseded epoch or nothing was in flight: drop it.
    Discarded,
}

/// Counters for the operator status. Aggregates only; no text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PerceptionGovernorMetrics {
    pub enabled: u64,
    pub probes_ok: u64,
    pub probes_failed: u64,
    pub captures_started: u64,
    pub captures_ok: u64,
    pub captures_failed: u64,
    pub captures_late: u64,
    /// Times a due job was deferred because Thinking was generating (once per
    /// closed-gate episode, not per poll).
    pub yielded_thinking: u64,
    /// Same, for speech.
    pub yielded_speech: u64,
    pub results_discarded: u64,
    pub suspensions: u64,
    pub last_model_latency_ms: u64,
    pub max_model_latency_ms: u64,
}

/// Operator-visible status snapshot. Serializable, text-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PerceptionGovernorStatus {
    pub status: PerceptionStatus,
    pub suspend_reason: Option<SuspendReason>,
    pub cadence_ms: u64,
    pub probe_baseline_ms: Option<u64>,
    pub last_probe_ms: Option<u64>,
    pub in_flight: bool,
    pub metrics: PerceptionGovernorMetrics,
}

#[derive(Debug)]
pub struct PerceptionGovernor {
    config: PerceptionGovernorConfig,
    status: PerceptionStatus,
    reason: Option<SuspendReason>,
    epoch: u64,
    in_flight: Option<PerceptionJob>,
    next_capture_at_ms: u64,
    next_probe_at_ms: u64,
    baseline_probe_ms: Option<u64>,
    last_probe_ms: Option<u64>,
    late_strikes: u32,
    failure_strikes: u32,
    gate_open_since_ms: Option<u64>,
    yield_counted: bool,
    metrics: PerceptionGovernorMetrics,
}

impl PerceptionGovernor {
    /// Starts [`PerceptionStatus::Off`].
    pub fn new(config: PerceptionGovernorConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        Ok(Self {
            config,
            status: PerceptionStatus::Off,
            reason: None,
            epoch: 0,
            in_flight: None,
            next_capture_at_ms: 0,
            next_probe_at_ms: 0,
            baseline_probe_ms: None,
            last_probe_ms: None,
            late_strikes: 0,
            failure_strikes: 0,
            gate_open_since_ms: None,
            yield_counted: false,
            metrics: PerceptionGovernorMetrics::default(),
        })
    }

    pub fn config(&self) -> &PerceptionGovernorConfig {
        &self.config
    }

    /// Bumped by every enable, disable and suspend. A worker compares the
    /// epoch of the job it was handed with this before it touches OBS.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn status(&self) -> PerceptionStatus {
        self.status
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            PerceptionStatus::Probing | PerceptionStatus::Active
        )
    }

    pub fn report(&self) -> PerceptionGovernorStatus {
        PerceptionGovernorStatus {
            status: self.status,
            suspend_reason: self.reason,
            cadence_ms: self.config.cadence_ms,
            probe_baseline_ms: self.baseline_probe_ms,
            last_probe_ms: self.last_probe_ms,
            in_flight: self.in_flight.is_some(),
            metrics: self.metrics,
        }
    }

    /// Begin (or restart after a suspension) with a start latency probe. The
    /// caller has already passed the capability gate.
    pub fn enable(&mut self, now_ms: u64) {
        self.epoch = self.epoch.saturating_add(1);
        self.status = PerceptionStatus::Probing;
        self.reason = None;
        self.in_flight = None;
        self.baseline_probe_ms = None;
        self.last_probe_ms = None;
        self.late_strikes = 0;
        self.failure_strikes = 0;
        self.gate_open_since_ms = None;
        self.yield_counted = false;
        self.next_capture_at_ms = now_ms;
        self.next_probe_at_ms = now_ms;
        self.metrics.enabled += 1;
    }

    /// Operator disable. Takes effect immediately: nothing more is dispatched
    /// and a result still in flight is discarded.
    pub fn disable(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.status = PerceptionStatus::Off;
        self.reason = None;
        self.in_flight = None;
    }

    /// Runtime-initiated stop (spill, repeated failure). The reason stays in
    /// the status until the operator enables perception again.
    pub fn suspend(&mut self, reason: SuspendReason) {
        self.epoch = self.epoch.saturating_add(1);
        self.status = PerceptionStatus::Suspended;
        self.reason = Some(reason);
        self.in_flight = None;
        self.metrics.suspensions += 1;
    }

    /// What to start now, if anything. Cheap enough to call every loop tick.
    pub fn poll(&mut self, now_ms: u64, gates: PerceptionGates) -> Option<PerceptionDispatch> {
        if !self.is_running() || self.in_flight.is_some() {
            return None;
        }
        if gates.is_open() {
            self.gate_open_since_ms.get_or_insert(now_ms);
        } else {
            self.gate_open_since_ms = None;
        }
        let job = match self.status {
            PerceptionStatus::Probing => PerceptionJob::Probe,
            PerceptionStatus::Active if now_ms >= self.next_probe_at_ms => PerceptionJob::Probe,
            PerceptionStatus::Active if now_ms >= self.next_capture_at_ms => PerceptionJob::Capture,
            _ => return None,
        };
        // Due. The yield gate decides whether it may start.
        if !gates.is_open() {
            if !self.yield_counted {
                self.yield_counted = true;
                if gates.thinking_generating {
                    self.metrics.yielded_thinking += 1;
                } else {
                    self.metrics.yielded_speech += 1;
                }
            }
            return None;
        }
        let open_for = now_ms.saturating_sub(self.gate_open_since_ms.unwrap_or(now_ms));
        if open_for < self.config.gate_settle_ms {
            return None;
        }
        self.yield_counted = false;
        self.in_flight = Some(job);
        if job == PerceptionJob::Capture {
            self.metrics.captures_started += 1;
        }
        Some(PerceptionDispatch {
            job,
            epoch: self.epoch,
        })
    }

    /// Report a finished job. `epoch` is the one the dispatch carried.
    pub fn on_result(&mut self, epoch: u64, outcome: JobOutcome, now_ms: u64) -> ResultDisposition {
        if epoch != self.epoch || self.in_flight.is_none() || !self.is_running() {
            self.metrics.results_discarded += 1;
            return ResultDisposition::Discarded;
        }
        self.in_flight = None;
        match (self.status, outcome) {
            (_, JobOutcome::Skipped) => ResultDisposition::NoObservation,
            (PerceptionStatus::Probing, JobOutcome::ProbeDone { latency_ms }) => {
                self.metrics.probes_ok += 1;
                self.last_probe_ms = Some(latency_ms);
                if latency_ms > self.config.probe_ceiling_ms {
                    self.suspend(SuspendReason::StartProbeSpill);
                } else {
                    self.baseline_probe_ms = Some(latency_ms);
                    self.status = PerceptionStatus::Active;
                    self.next_capture_at_ms = now_ms;
                    self.next_probe_at_ms = now_ms.saturating_add(self.config.probe_interval_ms);
                }
                ResultDisposition::NoObservation
            }
            (PerceptionStatus::Probing, JobOutcome::ProbeFailed) => {
                self.metrics.probes_failed += 1;
                self.suspend(SuspendReason::StartProbeFailed);
                ResultDisposition::NoObservation
            }
            (PerceptionStatus::Active, JobOutcome::ProbeDone { latency_ms }) => {
                self.metrics.probes_ok += 1;
                self.last_probe_ms = Some(latency_ms);
                let grown = self.baseline_probe_ms.is_some_and(|baseline| {
                    latency_ms.saturating_mul(100)
                        > baseline.saturating_mul(self.config.probe_growth_percent)
                });
                if latency_ms > self.config.probe_ceiling_ms || grown {
                    self.suspend(SuspendReason::LatencySpill);
                } else {
                    self.next_probe_at_ms = now_ms.saturating_add(self.config.probe_interval_ms);
                }
                ResultDisposition::NoObservation
            }
            (PerceptionStatus::Active, JobOutcome::ProbeFailed) => {
                self.metrics.probes_failed += 1;
                self.next_probe_at_ms = now_ms.saturating_add(self.config.probe_interval_ms);
                self.note_failure();
                ResultDisposition::NoObservation
            }
            (PerceptionStatus::Active, JobOutcome::CaptureDone { model_latency_ms }) => {
                self.failure_strikes = 0;
                self.metrics.last_model_latency_ms = model_latency_ms;
                self.metrics.max_model_latency_ms =
                    self.metrics.max_model_latency_ms.max(model_latency_ms);
                self.next_capture_at_ms = now_ms.saturating_add(self.config.cadence_ms);
                if model_latency_ms > self.config.caption_deadline_ms {
                    self.metrics.captures_late += 1;
                    self.late_strikes += 1;
                    if self.late_strikes >= self.config.late_strikes_to_suspend {
                        self.suspend(SuspendReason::SlowCaptions);
                    }
                    ResultDisposition::DropLate
                } else {
                    self.metrics.captures_ok += 1;
                    self.late_strikes = 0;
                    ResultDisposition::Ingest
                }
            }
            (PerceptionStatus::Active, JobOutcome::CaptureFailed) => {
                self.metrics.captures_failed += 1;
                self.next_capture_at_ms = now_ms.saturating_add(self.config.cadence_ms);
                self.note_failure();
                ResultDisposition::NoObservation
            }
            // A capture result while probing, or any other mismatch: not
            // something this state machine asked for.
            _ => {
                self.metrics.results_discarded += 1;
                ResultDisposition::Discarded
            }
        }
    }

    fn note_failure(&mut self) {
        self.failure_strikes += 1;
        if self.failure_strikes >= self.config.failure_strikes_to_suspend {
            self.suspend(SuspendReason::RepeatedFailures);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: PerceptionGates = PerceptionGates {
        thinking_generating: false,
        speech_active: false,
    };
    const THINKING: PerceptionGates = PerceptionGates {
        thinking_generating: true,
        speech_active: false,
    };
    const SPEECH: PerceptionGates = PerceptionGates {
        thinking_generating: false,
        speech_active: true,
    };

    fn governor() -> PerceptionGovernor {
        PerceptionGovernor::new(PerceptionGovernorConfig {
            gate_settle_ms: 0,
            ..PerceptionGovernorConfig::default()
        })
        .expect("governor")
    }

    /// Enable, run the start probe at `latency_ms`, return the epoch.
    fn started(governor: &mut PerceptionGovernor, latency_ms: u64) -> u64 {
        governor.enable(0);
        let dispatch = governor.poll(0, OPEN).expect("start probe");
        assert_eq!(dispatch.job, PerceptionJob::Probe);
        governor.on_result(dispatch.epoch, JobOutcome::ProbeDone { latency_ms }, 100);
        dispatch.epoch
    }

    #[test]
    fn off_by_default_and_the_floor_is_enforced() {
        let mut governor = governor();
        assert_eq!(governor.status(), PerceptionStatus::Off);
        assert_eq!(governor.poll(1_000_000, OPEN), None);
        for cadence_ms in [0, 1, 4_999] {
            assert!(
                PerceptionGovernor::new(PerceptionGovernorConfig {
                    cadence_ms,
                    ..PerceptionGovernorConfig::default()
                })
                .is_err(),
                "{cadence_ms}"
            );
        }
        assert!(
            PerceptionGovernor::new(PerceptionGovernorConfig {
                cadence_ms: MIN_PERCEPTION_CADENCE_MS,
                ..PerceptionGovernorConfig::default()
            })
            .is_ok()
        );
    }

    #[test]
    fn a_start_probe_precedes_any_capture_and_one_job_is_in_flight() {
        let mut governor = governor();
        governor.enable(0);
        assert_eq!(governor.status(), PerceptionStatus::Probing);
        let probe = governor.poll(0, OPEN).expect("probe");
        assert_eq!(probe.job, PerceptionJob::Probe);
        assert_eq!(governor.poll(1, OPEN), None, "one in flight");
        governor.on_result(probe.epoch, JobOutcome::ProbeDone { latency_ms: 70 }, 100);
        assert_eq!(governor.status(), PerceptionStatus::Active);
        let capture = governor.poll(100, OPEN).expect("capture");
        assert_eq!(capture.job, PerceptionJob::Capture);
        assert_eq!(governor.poll(100, OPEN), None, "one in flight");
    }

    #[test]
    fn captures_run_on_the_cadence_measured_from_completion() {
        let mut governor = governor();
        started(&mut governor, 70);
        let first = governor.poll(100, OPEN).expect("first");
        // The model took 2 s: the next capture is 10 s after it finished.
        assert_eq!(
            governor.on_result(
                first.epoch,
                JobOutcome::CaptureDone {
                    model_latency_ms: 2_000
                },
                2_100
            ),
            ResultDisposition::Ingest
        );
        assert_eq!(governor.poll(11_999, OPEN), None);
        assert!(governor.poll(12_100, OPEN).is_some());
    }

    #[test]
    fn nothing_is_dispatched_while_thinking_generates_or_speech_is_active() {
        let mut governor = governor();
        started(&mut governor, 70);
        assert_eq!(governor.poll(100, THINKING), None);
        assert_eq!(governor.poll(5_000, SPEECH), None);
        assert_eq!(governor.poll(9_000, THINKING), None);
        let report = governor.report();
        // Counted once per closed-gate episode, not per poll; the first cause
        // seen is the one counted until the gate opens.
        assert_eq!(report.metrics.yielded_thinking, 1);
        assert_eq!(report.metrics.yielded_speech, 0);
        assert_eq!(report.metrics.captures_started, 0);
        // The deferred capture starts as soon as the gate opens.
        assert!(governor.poll(9_001, OPEN).is_some());
    }

    #[test]
    fn captions_back_off_until_the_gate_has_settled() {
        let mut governor = PerceptionGovernor::new(PerceptionGovernorConfig {
            gate_settle_ms: 750,
            ..PerceptionGovernorConfig::default()
        })
        .expect("governor");
        governor.enable(0);
        assert_eq!(governor.poll(0, OPEN), None, "settle window not over");
        let probe = governor.poll(750, OPEN).expect("probe");
        governor.on_result(probe.epoch, JobOutcome::ProbeDone { latency_ms: 70 }, 800);
        assert_eq!(governor.poll(900, SPEECH), None);
        // Speech ended at 1000: the gate must stay open 750 ms.
        assert_eq!(governor.poll(1_000, OPEN), None);
        assert_eq!(governor.poll(1_749, OPEN), None);
        assert!(governor.poll(1_750, OPEN).is_some());
        assert_eq!(governor.report().metrics.yielded_speech, 1);
    }

    #[test]
    fn a_slow_start_probe_suspends_with_a_visible_reason() {
        let mut governor = governor();
        governor.enable(0);
        let probe = governor.poll(0, OPEN).expect("probe");
        governor.on_result(probe.epoch, JobOutcome::ProbeDone { latency_ms: 179 }, 200);
        let report = governor.report();
        assert_eq!(report.status, PerceptionStatus::Suspended);
        assert_eq!(report.suspend_reason, Some(SuspendReason::StartProbeSpill));
        assert_eq!(governor.poll(1_000_000, OPEN), None, "stays off");
    }

    #[test]
    fn a_failed_start_probe_suspends() {
        let mut governor = governor();
        governor.enable(0);
        let probe = governor.poll(0, OPEN).expect("probe");
        governor.on_result(probe.epoch, JobOutcome::ProbeFailed, 10);
        assert_eq!(
            governor.report().suspend_reason,
            Some(SuspendReason::StartProbeFailed)
        );
    }

    #[test]
    fn a_periodic_probe_that_slows_down_suspends_perception() {
        let mut governor = governor();
        let epoch = started(&mut governor, 60);
        // A capture, then the periodic probe comes due.
        let capture = governor.poll(100, OPEN).expect("capture");
        governor.on_result(
            capture.epoch,
            JobOutcome::CaptureDone {
                model_latency_ms: 1_000,
            },
            1_100,
        );
        let probe = governor.poll(60_100, OPEN).expect("periodic probe");
        assert_eq!((probe.job, probe.epoch), (PerceptionJob::Probe, epoch));
        // 130 ms is under the ceiling but more than 2x the 60 ms baseline.
        governor.on_result(
            probe.epoch,
            JobOutcome::ProbeDone { latency_ms: 130 },
            60_200,
        );
        let report = governor.report();
        assert_eq!(report.status, PerceptionStatus::Suspended);
        assert_eq!(report.suspend_reason, Some(SuspendReason::LatencySpill));
        assert_eq!(report.last_probe_ms, Some(130));
        assert_eq!(governor.poll(1_000_000, OPEN), None);
    }

    #[test]
    fn a_steady_periodic_probe_keeps_perception_active() {
        let mut governor = governor();
        started(&mut governor, 60);
        let capture = governor.poll(100, OPEN).expect("capture");
        governor.on_result(
            capture.epoch,
            JobOutcome::CaptureDone {
                model_latency_ms: 900,
            },
            1_000,
        );
        let probe = governor.poll(60_100, OPEN).expect("probe");
        governor.on_result(
            probe.epoch,
            JobOutcome::ProbeDone { latency_ms: 80 },
            60_200,
        );
        assert_eq!(governor.status(), PerceptionStatus::Active);
    }

    #[test]
    fn consecutive_late_captures_suspend_and_a_good_one_resets_the_count() {
        let mut governor = governor();
        started(&mut governor, 60);
        let mut now = 100;
        let mut run = |governor: &mut PerceptionGovernor, latency: u64| {
            let dispatch = governor.poll(now, OPEN).expect("capture");
            let done = now + 1_000;
            // Next capture is due one cadence (10 s) after this one finished.
            now = done + 10_000;
            governor.on_result(
                dispatch.epoch,
                JobOutcome::CaptureDone {
                    model_latency_ms: latency,
                },
                done,
            )
        };
        assert_eq!(run(&mut governor, 12_000), ResultDisposition::DropLate);
        assert_eq!(run(&mut governor, 12_000), ResultDisposition::DropLate);
        assert_eq!(run(&mut governor, 800), ResultDisposition::Ingest);
        assert_eq!(run(&mut governor, 12_000), ResultDisposition::DropLate);
        assert_eq!(run(&mut governor, 12_000), ResultDisposition::DropLate);
        assert_eq!(governor.status(), PerceptionStatus::Active);
        assert_eq!(run(&mut governor, 12_000), ResultDisposition::DropLate);
        assert_eq!(
            governor.report().suspend_reason,
            Some(SuspendReason::SlowCaptions)
        );
    }

    #[test]
    fn repeated_failures_suspend() {
        let mut governor = governor();
        started(&mut governor, 60);
        let mut now = 100;
        for _ in 0..5 {
            let dispatch = governor.poll(now, OPEN).expect("capture");
            let done = now + 1_000;
            now = done + 10_000;
            governor.on_result(dispatch.epoch, JobOutcome::CaptureFailed, done);
        }
        assert_eq!(
            governor.report().suspend_reason,
            Some(SuspendReason::RepeatedFailures)
        );
    }

    #[test]
    fn disable_discards_a_result_in_flight_and_stops_dispatch() {
        let mut governor = governor();
        started(&mut governor, 60);
        let capture = governor.poll(100, OPEN).expect("capture");
        let before = governor.epoch();
        governor.disable();
        assert_ne!(governor.epoch(), before, "a worker sees the new epoch");
        assert_eq!(governor.status(), PerceptionStatus::Off);
        assert_eq!(
            governor.on_result(
                capture.epoch,
                JobOutcome::CaptureDone {
                    model_latency_ms: 100
                },
                200
            ),
            ResultDisposition::Discarded
        );
        assert_eq!(governor.poll(1_000_000, OPEN), None);
        assert_eq!(governor.report().metrics.results_discarded, 1);
    }

    #[test]
    fn re_enabling_after_a_suspension_starts_over_with_a_new_probe() {
        let mut governor = governor();
        governor.enable(0);
        let probe = governor.poll(0, OPEN).expect("probe");
        governor.on_result(probe.epoch, JobOutcome::ProbeDone { latency_ms: 500 }, 10);
        assert_eq!(governor.status(), PerceptionStatus::Suspended);
        governor.enable(1_000);
        assert_eq!(governor.status(), PerceptionStatus::Probing);
        assert_eq!(governor.report().suspend_reason, None);
        assert_eq!(
            governor.poll(1_000, OPEN).expect("probe").job,
            PerceptionJob::Probe
        );
    }
}
