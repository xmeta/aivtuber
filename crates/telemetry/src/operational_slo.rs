//! Operational service-level objectives and error-budget reporting (issue #71).
//!
//! This module answers a different question from [`crate::benchmark_gate`]:
//!
//! * the benchmark gate answers "did this change regress against the base
//!   revision?" — a *relative* judgement about one pull request;
//! * an SLO answers "is the runtime delivering an acceptable level of service
//!   over a session / stream hour?" — an *absolute* judgement about the
//!   running system.
//!
//! A change can improve relative performance while the product still misses its
//! operational objective, so the two contracts stay separate types with
//! separate configuration. Nothing here reads or writes `benchmarks/budgets.json`.
//!
//! Three properties are load-bearing and are enforced by construction rather
//! than by convention:
//!
//! 1. **Nothing is invented.** A target is a configured ratio plus the
//!    measured baseline it was chosen from. [`SloTargets::validate`] refuses a
//!    target without [`BaselineEvidence`] and refuses a target file whose
//!    catalog version does not match the code evaluating it, so a threshold
//!    cannot outlive the indicator definition it was calibrated against.
//! 2. **Absence of evidence is never a pass, and never a miss.** An indicator
//!    whose eligible denominator is empty reports [`SloStatus::NoData`], an
//!    indicator the benchmark artifact cannot reproduce at all reports
//!    [`SloStatus::NotYetMeasured`] together with the evidence that is missing,
//!    and a sample the objective has no calibrated boundary for is
//!    [`EventVerdict::Unscored`]. None of the three counts as
//!    [`SloStatus::Met`], and none of them enters a miss set, an attribution
//!    tally, or an error budget.
//! 3. **Zero-tolerance invariants are not SLOs.** Security, authorization and
//!    bounded-state invariants carry no ratio target and no error budget;
//!    [`SloTargets::validate`] rejects a target that would give one.
//!
//! Attribution is separate from attainment. The runtime cannot own an external
//! provider's uptime, but it does own the quality of its own degradation, so a
//! miss is classified as [`MissAttribution::ExternalProvider`] (the miss exists
//! only because a provider was unavailable) or
//! [`MissAttribution::RuntimeHandling`] (a provider was unavailable *and* the
//! runtime still produced no user-visible output). See [`attribution_of`].

use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::{BenchmarkReport, ComparisonMode, EventObservation, RouteClass, TelemetryError};

/// Report contract version this module emits.
pub const SLO_REPORT_SCHEMA_VERSION: &str = "1";

/// Catalog version. A target file pins it, so changing an indicator's
/// numerator or denominator invalidates every target calibrated against the
/// previous definition instead of silently reinterpreting them.
pub const SLO_CATALOG_VERSION: &str = "slo-catalog-v1";

/// One represented stream hour, in logical stream milliseconds.
pub const STREAM_HOUR_MS: u64 = 3_600_000;

/// Default span of a rolling error-budget window.
pub const DEFAULT_ROLLING_WINDOW_HOURS: u32 = 3;

/// Default number of consecutive missed windows before a miss becomes a
/// candidate for the after-action review process (#62).
pub const DEFAULT_PERSISTENT_MISS_WINDOWS: u64 = 3;

/// Minimum number of *distinct* active-plane runs a history must contain before
/// it may produce baseline evidence. One run calibrates nothing, and a run
/// identity seen twice (a retry, or the same artifact passed twice) is not a
/// second run.
pub const MIN_BASELINE_RUNS: u64 = 2;

/// Reserved prefix marking a probe placeholder in
/// [`SloTarget::threshold_probe`]. [`SloTargets::validate`] refuses it, so a
/// probed boundary can never be written into a targets file as if it were
/// calibration evidence.
pub const PROBE_SOURCE_MARKER: &str = "latency probe;";

/// Traffic plane the evaluation belongs to.
///
/// Experimental routes are evaluated under their own plane so a shadow or
/// experimental route cannot move the active user-facing numbers. An
/// experimental report is still a real measurement; it just is not the active
/// SLO, and the report says so in its own fields rather than leaving a consumer
/// to infer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficPlane {
    Active,
    Experimental,
}

/// Whether an objective tolerates misses at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveKind {
    /// Probabilistic objective with a target ratio and an error budget.
    Slo,
    /// Zero-tolerance invariant. Never carries a ratio target or a budget.
    Invariant,
}

/// Unit of the reported value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndicatorUnit {
    /// Conforming / eligible, in `[0, 1]`.
    Ratio,
    /// A count of violations that must stay at zero.
    Count,
}

/// Where an indicator's numerator and denominator come from.
///
/// [`EvidenceSource::Measured`] means both can be reproduced from a benchmark
/// artifact today. [`EvidenceSource::Declared`] means the objective is agreed
/// but the artifact does not carry the evidence yet; such an indicator is
/// reported as [`SloStatus::NotYetMeasured`] with the missing evidence named,
/// rather than being evaluated against a number the benchmark asserts rather
/// than measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Measured,
    Declared,
}

/// Reporting windows.
///
/// Windowing is over the *logical* stream timeline, never the wall clock: a
/// replay must not produce different windows on a faster machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// One run of the runtime: one replay/scenario run or one live session.
    Session,
    /// One represented stream hour, bucketed from the logical stream offset.
    StreamHour,
    /// A consecutive group of stream hours, for rolling error budgets.
    RollingStreamHours { window_hours: u32 },
}

impl WindowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::StreamHour => "stream_hour",
            Self::RollingStreamHours { .. } => "rolling_stream_hours",
        }
    }
}

/// Documented operator action path for an objective that breaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloAction {
    /// Create or raise an incident.
    RaiseIncident,
    /// Disable the experimental route or profile that produced the miss.
    DisableExperimentalRoute,
    /// Switch to the conservative cached/reflex profile.
    SwitchConservativeProfile,
    /// Open an optimization/reliability issue referencing this report.
    OpenOptimizationIssue,
    /// Block promotion from experimental to default.
    BlockPromotion,
}

/// Attribution of a miss to a cause the runtime does or does not own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissAttribution {
    /// The provider was unavailable and the runtime's own degradation is not
    /// implicated by the miss.
    ExternalProvider,
    /// A provider failure was in play and the runtime still produced no
    /// user-visible output. This is the runtime's own failure, and it is the
    /// one this project owns.
    RuntimeHandling,
    /// No declared cause; the miss is unexplained.
    Unattributed,
}

/// Which side of the runtime/provider boundary a declared cause sits on.
///
/// The slugs are the closed set emitted by `fallback_reason_name` in
/// `crates/app/src/lib.rs`. Matching on the string keeps this module free of a
/// dependency on the generative crate; anything outside the set is
/// [`FailureOrigin::Unknown`] rather than an optimistic guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureOrigin {
    /// Provider-side: `timeout`, `unavailable`, `rate_limited`, `overloaded`,
    /// `authentication`.
    ExternalProvider,
    /// Deliberate runtime decision: `invalid_request`, `low_confidence`,
    /// `policy_override`, `operator_override`, `budget_exhausted`.
    RuntimePolicy,
    /// Not a declared cause.
    Unknown,
}

pub fn failure_origin(declared_reason: &str) -> FailureOrigin {
    match declared_reason {
        "timeout" | "unavailable" | "rate_limited" | "overloaded" | "authentication" => {
            FailureOrigin::ExternalProvider
        }
        "invalid_request" | "low_confidence" | "policy_override" | "operator_override"
        | "budget_exhausted" => FailureOrigin::RuntimePolicy,
        _ => FailureOrigin::Unknown,
    }
}

/// The event reached the viewer in some form (audio and/or a visible reaction).
fn produced_visible_output(event: &EventObservation) -> bool {
    event.event_to_first_audio_ms.is_some() || event.event_to_first_visible_reaction_ms.is_some()
}

fn external_failure(event: &EventObservation) -> bool {
    event.fallback_reason.as_deref().map(failure_origin) == Some(FailureOrigin::ExternalProvider)
}

/// Classify why one event missed an objective.
///
/// The split is the one the issue asks for: an external provider failure and a
/// runtime degradation failure are different operational problems with
/// different owners. An event silenced while a provider was unavailable is
/// counted as a runtime-handling failure by construction — whether a visible
/// outcome was *expected* under the active profile is not carried by the
/// artifact, and the recorded route class is what says whether a fallback route
/// was taken.
pub fn attribution_of(event: &EventObservation) -> MissAttribution {
    if external_failure(event) {
        if produced_visible_output(event) {
            MissAttribution::ExternalProvider
        } else {
            MissAttribution::RuntimeHandling
        }
    } else {
        MissAttribution::Unattributed
    }
}

/// Tally of the runtime/provider boundary over one set of events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionSummary {
    pub external_provider_failures: u64,
    /// External failure with a user-visible outcome: degradation worked.
    pub runtime_handled: u64,
    /// External failure with no user-visible outcome: degradation failed.
    pub runtime_handling_failures: u64,
    /// A deliberate runtime decision (operator override, budget denial, ...).
    /// Recorded rather than dropped: a reason the runtime chose is not an
    /// unexplained failure, but the tally must still be complete.
    pub runtime_policy_decisions: u64,
    /// A declared reason outside the known slug set. Counted, not discarded —
    /// an unknown reason is the one case where the project genuinely cannot say
    /// whose failure this was. An event with *no* declared reason at all is
    /// not counted here: that is an ordinary unexplained miss, visible through
    /// the ratio and the miss count, not a reason to attribute.
    pub unattributed: u64,
}

impl AttributionSummary {
    pub fn of<'a>(events: impl IntoIterator<Item = &'a EventObservation>) -> Self {
        let mut summary = Self::default();
        for event in events {
            match event
                .fallback_reason
                .as_deref()
                .map(failure_origin)
                .unwrap_or(FailureOrigin::Unknown)
            {
                FailureOrigin::ExternalProvider => {
                    summary.external_provider_failures += 1;
                    match attribution_of(event) {
                        MissAttribution::ExternalProvider => summary.runtime_handled += 1,
                        MissAttribution::RuntimeHandling => summary.runtime_handling_failures += 1,
                        MissAttribution::Unattributed => summary.unattributed += 1,
                    }
                }
                FailureOrigin::RuntimePolicy => summary.runtime_policy_decisions += 1,
                FailureOrigin::Unknown => {
                    if event.fallback_reason.is_some() {
                        summary.unattributed += 1;
                    }
                }
            }
        }
        summary
    }
}

/// Status of one indicator in one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloStatus {
    /// Target met on enough samples.
    Met,
    /// Target missed on enough samples.
    Missed,
    /// Measured, but no target has been calibrated yet. Carries the observed
    /// value as calibration evidence.
    Uncalibrated,
    /// The eligible denominator was empty. Not a pass and not a failure.
    NoData,
    /// Samples exist but fewer than the indicator's sample floor.
    InsufficientSamples,
    /// The objective is declared but the artifact does not reproduce its
    /// denominator yet.
    NotYetMeasured,
}

impl SloStatus {
    /// Whether this status decides the objective.
    pub fn is_decided(self) -> bool {
        matches!(self, Self::Met | Self::Missed)
    }

    /// Whether this objective's evidence is still outstanding.
    ///
    /// Every state that is not a decision and not a legitimate absence of data
    /// belongs here, and each one is listed explicitly so a new status cannot
    /// be added without someone deciding whether it may let a report read as a
    /// pass. `InsufficientSamples` is included on purpose: an objective with
    /// fewer samples than its floor has not been decided, and treating "not
    /// enough evidence" as "nothing outstanding" is the same mistake as
    /// treating an unmeasured invariant as a satisfied one.
    pub fn is_unresolved(self) -> bool {
        matches!(
            self,
            Self::Uncalibrated | Self::NotYetMeasured | Self::InsufficientSamples
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Missed => "missed",
            Self::Uncalibrated => "uncalibrated",
            Self::NoData => "no_data",
            Self::InsufficientSamples => "insufficient_samples",
            Self::NotYetMeasured => "not_yet_measured",
        }
    }
}

/// Raw counts produced by an indicator's evaluator.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SloMeasurement {
    /// Events that conformed to the objective.
    pub conforming: u64,
    /// Events the objective applies to, including [`SloMeasurement::unscored`].
    pub eligible: u64,
    /// Eligible events the objective has no calibrated boundary to judge yet.
    pub unscored: u64,
    /// Reported value in the indicator's unit. `None` means "not measured",
    /// which is never 0 and never 1.
    pub value: Option<f64>,
}

impl SloMeasurement {
    /// Misses are always `eligible - conforming - unscored`, so the error
    /// budget and the indicator can never disagree about what was spent, and an
    /// uncalibrated sample is never charged as a spend.
    pub fn misses(&self) -> u64 {
        self.eligible
            .saturating_sub(self.conforming)
            .saturating_sub(self.unscored)
    }
}

/// How one event relates to one indicator.
///
/// Every indicator declares exactly one classifier, and the report uses that
/// single definition for three things: the measured ratio, the miss set used
/// for attribution, and the error-budget spend. An event that is outside an
/// indicator's denominator is neither conforming nor a miss — it is simply not
/// that indicator's business, and counting it as a miss would let an unrelated
/// failure show up in an objective that actually met its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventVerdict {
    /// Not this indicator's business.
    Outside,
    /// Eligible, measured, and inside the objective.
    Conforms,
    /// Eligible, measured, and outside the objective.
    Misses,
    /// Eligible and measured, but the objective has no calibrated boundary yet,
    /// so there is no defined pass or fail.
    ///
    /// This is the state a latency sample lands in while no `threshold_ms` has
    /// been calibrated. It is deliberately distinct from [`EventVerdict::Misses`]:
    /// without a threshold there is no miss to attribute, and a three-state enum
    /// would force an uncalibrated objective either to claim conformance or to
    /// invent a failure. The sample still counts toward the eligible
    /// denominator — that is the calibration evidence — but it never enters the
    /// miss set, the error budget, or the attribution.
    Unscored,
}

impl EventVerdict {
    /// Whether the event belongs to this indicator's denominator.
    pub fn is_eligible(self) -> bool {
        !matches!(self, Self::Outside)
    }

    /// Whether the event is a defined failure of this objective.
    pub fn is_miss(self) -> bool {
        matches!(self, Self::Misses)
    }
}

/// Raw counts produced by one indicator over one window.
fn measure(
    indicator: &SloIndicator,
    events: &[&EventObservation],
    target: Option<&SloTarget>,
) -> SloMeasurement {
    let mut eligible = 0_u64;
    let mut conforming = 0_u64;
    let mut unscored = 0_u64;
    for event in events {
        let verdict = (indicator.classify)(event, target);
        if !verdict.is_eligible() {
            continue;
        }
        eligible += 1;
        match verdict {
            EventVerdict::Conforms => conforming += 1,
            EventVerdict::Unscored => unscored += 1,
            EventVerdict::Outside | EventVerdict::Misses => {}
        }
    }
    // An indicator with unscored samples has no defined ratio: dividing
    // `conforming` by `eligible` would report every uncalibrated sample as a
    // failure, which is the inversion this module exists to prevent. The value
    // stays `None` and the samples are reported as calibration evidence
    // instead — deliberately different from 0%.
    let value = (unscored == 0 && eligible > 0).then(|| conforming as f64 / eligible as f64);
    SloMeasurement {
        conforming,
        eligible,
        unscored,
        value,
    }
}

/// One indicator definition. Denominator and numerator are written out rather
/// than implied, because an objective whose denominator cannot be reconstructed
/// from an artifact is not an objective.
#[derive(Debug, Clone, Copy)]
pub struct SloIndicator {
    pub id: &'static str,
    pub kind: ObjectiveKind,
    pub unit: IndicatorUnit,
    pub evidence: EvidenceSource,
    /// Exact description of what counts in the denominator.
    pub denominator: &'static str,
    /// Exact description of what counts in the numerator.
    pub numerator: &'static str,
    /// Declares that this objective is judged against a `threshold_ms`.
    ///
    /// This is a *declaration*, not the scoring rule: it drives target
    /// validation (a latency target without a threshold is refused) and it
    /// selects which indicators publish target-free percentiles. The scoring
    /// itself belongs to [`SloIndicator::classify`], so the two cannot disagree
    /// about what a sample means.
    pub latency_thresholded: bool,
    /// Minimum eligible events before a target may decide the objective.
    pub sample_floor: u64,
    /// Why a declared indicator cannot be evaluated from a benchmark artifact.
    pub missing_evidence: Option<&'static str>,
    /// Documented action path when the objective is missed.
    pub actions: &'static [SloAction],
    /// Single per-event definition of "does this indicator apply to this event,
    /// and did it conform". The report derives the ratio, the miss set, and the
    /// attribution from this one function so they cannot disagree.
    pub classify: fn(&EventObservation, Option<&SloTarget>) -> EventVerdict,
}

/// Repository-owned indicator catalog.
///
/// Indicator ids and definitions are project-owned on purpose. Generic SRE
/// error-budget machinery supplies the *reporting shape* (windows, budgets,
/// burn-down); it never supplies which events aivtuber counts, and its
/// thresholds are never imported.
pub fn catalog() -> Vec<SloIndicator> {
    vec![
        SloIndicator {
            id: "availability.event_to_first_visible_within_target",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events with a measured event-to-first-visible-reaction latency",
            numerator: "those whose latency was within the configured threshold_ms",
            latency_thresholded: true,
            sample_floor: 15,
            missing_evidence: None,
            actions: &[
                SloAction::RaiseIncident,
                SloAction::OpenOptimizationIssue,
                SloAction::SwitchConservativeProfile,
            ],
            classify: classify_first_visible_latency,
        },
        SloIndicator {
            id: "availability.event_to_first_audio_within_target",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events with a measured event-to-first-audio latency",
            numerator: "those whose latency was within the configured threshold_ms",
            latency_thresholded: true,
            sample_floor: 15,
            missing_evidence: None,
            actions: &[
                SloAction::RaiseIncident,
                SloAction::OpenOptimizationIssue,
                SloAction::SwitchConservativeProfile,
            ],
            classify: classify_first_audio_latency,
        },
        // Named and described as the conforming rate, not as a failure rate.
        // `SloTarget::target` is a *minimum* conforming ratio, so an indicator
        // whose id and numerator described failures while its value was a
        // success ratio would make any configured target ambiguous.
        SloIndicator {
            id: "availability.speech_presence_rate",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events that were not cancelled",
            numerator: "those that produced a user-visible output, or stayed silent for a declared reason \
                 (operator override, deadline exhaustion, generative-budget denial)",
            latency_thresholded: false,
            sample_floor: 15,
            missing_evidence: None,
            actions: &[
                SloAction::RaiseIncident,
                SloAction::OpenOptimizationIssue,
                SloAction::SwitchConservativeProfile,
            ],
            classify: classify_speech_presence,
        },
        SloIndicator {
            id: "reliability.fallback_delivery_rate",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events that hit a declared external-provider failure",
            numerator: "those that still delivered a user-visible outcome (cached/non-verbal fallback)",
            latency_thresholded: false,
            sample_floor: 1,
            missing_evidence: None,
            actions: &[
                SloAction::RaiseIncident,
                SloAction::OpenOptimizationIssue,
                SloAction::BlockPromotion,
            ],
            classify: classify_fallback_delivery,
        },
        SloIndicator {
            id: "reliability.deadline_adherence_rate",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events carrying a trusted interaction deadline class",
            numerator: "those whose trusted deadline was not exhausted",
            latency_thresholded: false,
            sample_floor: 15,
            missing_evidence: None,
            actions: &[SloAction::OpenOptimizationIssue, SloAction::RaiseIncident],
            classify: classify_deadline_adherence,
        },
        SloIndicator {
            id: "reliability.generative_budget_admission_rate",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "all admitted events",
            numerator: "those admitted into the generative budget rather than denied",
            latency_thresholded: false,
            sample_floor: 15,
            missing_evidence: None,
            actions: &[
                SloAction::OpenOptimizationIssue,
                SloAction::SwitchConservativeProfile,
            ],
            classify: classify_generative_budget_admission,
        },
        SloIndicator {
            id: "quality.semantic_reuse_correctness",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Measured,
            denominator: "admitted events carrying an offline wrong-reuse review label",
            numerator: "those labelled correct",
            latency_thresholded: false,
            sample_floor: 1,
            missing_evidence: None,
            actions: &[SloAction::OpenOptimizationIssue, SloAction::BlockPromotion],
            classify: classify_semantic_reuse_correctness,
        },
        SloIndicator {
            id: "operability.operator_stop_responsiveness_within_target",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Declared,
            denominator: "operator stop/mute commands issued in the window",
            numerator: "those whose cancellations took effect within the configured threshold_ms",
            latency_thresholded: true,
            sample_floor: 1,
            missing_evidence: Some(
                "the replay benchmark drives no operator stop/mute fixture, so the denominator \
                 (issued commands) is not in the artifact",
            ),
            actions: &[SloAction::RaiseIncident],
            classify: classify_never,
        },
        SloIndicator {
            id: "availability.event_admission_success_rate",
            kind: ObjectiveKind::Slo,
            unit: IndicatorUnit::Ratio,
            evidence: EvidenceSource::Declared,
            denominator: "events offered to the runtime",
            numerator: "those admitted",
            latency_thresholded: false,
            sample_floor: 15,
            missing_evidence: Some(
                "EventObservation records admitted events only; the offered-event count is not \
                 carried by the artifact, so the denominator cannot be reconstructed",
            ),
            actions: &[SloAction::OpenOptimizationIssue],
            classify: classify_never,
        },
        SloIndicator {
            id: "safety.stale_dispatch_count",
            kind: ObjectiveKind::Invariant,
            unit: IndicatorUnit::Count,
            evidence: EvidenceSource::Declared,
            denominator: "admitted events (context only; the invariant is the count itself)",
            numerator: "actions dispatched after their cancellation fence; must be 0",
            latency_thresholded: false,
            sample_floor: 0,
            missing_evidence: Some(
                "per-event post-fence dispatch is not recorded by EventObservation, and the \
                 benchmark-result invariant map asserts zero rather than measuring it",
            ),
            actions: &[SloAction::RaiseIncident],
            classify: classify_never,
        },
        SloIndicator {
            id: "safety.unauthorized_privileged_action_count",
            kind: ObjectiveKind::Invariant,
            unit: IndicatorUnit::Count,
            evidence: EvidenceSource::Declared,
            denominator: "privileged control attempts (context only)",
            numerator: "privileged actions executed without authenticated local capability; must be 0",
            latency_thresholded: false,
            sample_floor: 0,
            missing_evidence: Some(
                "privileged attempts are recorded in the runtime audit log, which is not part of \
                 the benchmark artifact",
            ),
            actions: &[SloAction::RaiseIncident],
            classify: classify_never,
        },
        SloIndicator {
            id: "resource.retention_bound_violation_count",
            kind: ObjectiveKind::Invariant,
            unit: IndicatorUnit::Count,
            evidence: EvidenceSource::Declared,
            denominator: "retained queues, histories, caches and observations (context only)",
            numerator: "observed bound violations; must be 0",
            latency_thresholded: false,
            sample_floor: 0,
            missing_evidence: Some(
                "retention metrics live on the runtime composition root; a replay report carries \
                 only the bounded observation set, not the retention snapshot",
            ),
            actions: &[SloAction::RaiseIncident],
            classify: classify_never,
        },
    ]
}

fn classify_never(_event: &EventObservation, _target: Option<&SloTarget>) -> EventVerdict {
    EventVerdict::Outside
}

fn classify_first_visible_latency(
    event: &EventObservation,
    target: Option<&SloTarget>,
) -> EventVerdict {
    classify_latency(event.event_to_first_visible_reaction_ms, target)
}

fn classify_first_audio_latency(
    event: &EventObservation,
    target: Option<&SloTarget>,
) -> EventVerdict {
    classify_latency(event.event_to_first_audio_ms, target)
}

/// An event with no measured latency is outside the denominator, not a miss.
/// With a sample but no calibrated threshold there is no defined miss either,
/// so the event is eligible and unscored: it counts as the evidence a
/// threshold is calibrated from, and never as a failure.
fn classify_latency(latency: Option<u64>, target: Option<&SloTarget>) -> EventVerdict {
    let Some(latency) = latency else {
        return EventVerdict::Outside;
    };
    match target.and_then(|target| target.threshold_ms) {
        Some(threshold) if latency <= threshold => EventVerdict::Conforms,
        Some(_) => EventVerdict::Misses,
        None => EventVerdict::Unscored,
    }
}

fn classify_speech_presence(event: &EventObservation, _target: Option<&SloTarget>) -> EventVerdict {
    if event.cancelled {
        return EventVerdict::Outside;
    }
    if unintended_silence(event) {
        EventVerdict::Misses
    } else {
        EventVerdict::Conforms
    }
}

fn classify_fallback_delivery(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    if !external_failure(event) {
        return EventVerdict::Outside;
    }
    if produced_visible_output(event) {
        EventVerdict::Conforms
    } else {
        EventVerdict::Misses
    }
}

fn classify_deadline_adherence(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    if event.deadline_class.is_none() {
        return EventVerdict::Outside;
    }
    if event.deadline_exhaustion_stage.is_some() {
        EventVerdict::Misses
    } else {
        EventVerdict::Conforms
    }
}

fn classify_generative_budget_admission(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    if event.budget_denial_reason.is_some() {
        EventVerdict::Misses
    } else {
        EventVerdict::Conforms
    }
}

fn classify_semantic_reuse_correctness(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    match event.wrong_reuse {
        None => EventVerdict::Outside,
        Some(false) => EventVerdict::Conforms,
        Some(true) => EventVerdict::Misses,
    }
}

fn unintended_silence(event: &EventObservation) -> bool {
    event.route == RouteClass::Silent
        && !event.operator_override
        && event.deadline_exhaustion_stage.is_none()
        && event.budget_denial_reason.is_none()
}

/// Target-free latency evidence for one window: the percentiles a threshold is
/// chosen *from*.
///
/// Reporting these next to an uncalibrated latency indicator is what keeps #71
/// from degenerating into "pick a round number and call it an SLO".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyCalibration {
    pub indicator: String,
    pub window: String,
    pub field: String,
    pub samples: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

/// Identity of one measured run a pooled baseline was computed from.
///
/// `run_id` carries #58's `recording.run_id` semantics: stable across retries
/// of one run, distinct across distinct runs. The revision travels per run
/// because one series may pool runs built from more than one commit; dataset,
/// config, mode and seed are pinned by the series the evidence names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineRun {
    pub run_id: String,
    pub git_commit: String,
}

/// Measured evidence a target was calibrated from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineEvidence {
    /// Measured value of the indicator at calibration time.
    pub value: f64,
    /// Artifact identity the value came from (file, dataset id, revision).
    pub source: String,
    /// Represented stream hours the calibration evidence covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_hours: Option<u64>,
    /// The #58 compatibility series (`mode|dataset_id|config_version|seed`) the
    /// evidence belongs to. It is carried in the evidence itself because this
    /// block is the part a reviewer copies into a durable target: the proposal's
    /// top-level series is gone once the block is copied out of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// The runs the value was pooled from. A target has to be able to prove
    /// *which* repeated runs formed its baseline, so the identities travel with
    /// the evidence, not merely alongside it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<BaselineRun>,
}

/// One calibrated objective.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SloTarget {
    /// Minimum conforming/eligible ratio in `[0, 1]`.
    pub target: f64,
    /// Latency threshold a latency objective is evaluated against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_ms: Option<u64>,
    /// The measurement the target was chosen from. Required.
    pub baseline: BaselineEvidence,
}

impl SloTarget {
    /// A threshold-only target used to *measure* a candidate latency boundary
    /// during calibration (the probe step). It is not a calibrated objective:
    /// it carries no measured baseline and must never be written into a targets
    /// file — [`SloTargets::validate`] would refuse it, which is the point.
    pub fn threshold_probe(threshold_ms: u64) -> Self {
        Self {
            target: 1.0,
            threshold_ms: Some(threshold_ms),
            baseline: BaselineEvidence {
                value: 0.0,
                source: format!("{PROBE_SOURCE_MARKER} not calibration evidence"),
                stream_hours: None,
                series: None,
                runs: Vec::new(),
            },
        }
    }
}

/// Repository-controlled operational targets, reviewed like code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SloTargets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Must equal [`SLO_CATALOG_VERSION`].
    pub catalog_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, SloTarget>,
}

impl Default for SloTargets {
    fn default() -> Self {
        Self {
            schema_version: Some(SLO_REPORT_SCHEMA_VERSION.to_owned()),
            catalog_version: SLO_CATALOG_VERSION.to_owned(),
            description: None,
            targets: BTreeMap::new(),
        }
    }
}

impl SloTargets {
    pub fn from_json(bytes: &[u8]) -> Result<Self, SloError> {
        serde_json::from_slice(bytes)
            .map_err(|error| SloError::new(format!("parse SLO targets: {error}")))
    }

    /// Reject a target file that cannot be trusted to describe this catalog.
    ///
    /// `allow_probe: true` is the measurement path (`slo-report --probe-latency`):
    /// a [`SloTarget::threshold_probe`] is the *tool* that measures a candidate
    /// boundary, so it may drive an evaluation, but it can never pass a targets
    /// file — the default `allow_probe: false` — because a probe is not
    /// calibration evidence.
    pub fn validate_for_evaluation(&self, catalog: &[SloIndicator]) -> Result<(), SloError> {
        self.validate_inner(catalog, true)
    }

    pub fn validate(&self, catalog: &[SloIndicator]) -> Result<(), SloError> {
        self.validate_inner(catalog, false)
    }

    fn validate_inner(&self, catalog: &[SloIndicator], allow_probe: bool) -> Result<(), SloError> {
        if self.catalog_version != SLO_CATALOG_VERSION {
            return Err(SloError::new(format!(
                "target file targets catalog {:?} but this build implements {:?}; a threshold \
                 calibrated against different indicator definitions is not comparable",
                self.catalog_version, SLO_CATALOG_VERSION
            )));
        }
        // An absent `schema_version` means "unversioned draft"; a *present* one
        // that this build does not implement is a file written for a different
        // report contract, which must not be silently reinterpreted.
        if let Some(schema_version) = &self.schema_version
            && schema_version != SLO_REPORT_SCHEMA_VERSION
        {
            return Err(SloError::new(format!(
                "target file declares report schema {schema_version:?} but this build \
                 emits {SLO_REPORT_SCHEMA_VERSION:?}"
            )));
        }
        for (id, target) in &self.targets {
            let Some(indicator) = catalog.iter().find(|entry| entry.id == id) else {
                return Err(SloError::new(format!(
                    "target {id:?} does not name an indicator in catalog {SLO_CATALOG_VERSION}"
                )));
            };
            if indicator.kind == ObjectiveKind::Invariant {
                return Err(SloError::new(format!(
                    "indicator {id:?} is a zero-tolerance invariant; it cannot carry a ratio \
                     target or an error budget"
                )));
            }
            if !target.target.is_finite() || !(0.0..=1.0).contains(&target.target) {
                return Err(SloError::new(format!(
                    "target {id:?} must be a finite ratio in [0, 1], got {}",
                    target.target
                )));
            }
            // The baseline is the measured evidence the target was chosen from.
            // An impossible or non-finite value there would not be caught by
            // any later evaluation — it would just be cited forever as the
            // justification for the number, so it is refused up front.
            if !target.baseline.value.is_finite() || !(0.0..=1.0).contains(&target.baseline.value) {
                return Err(SloError::new(format!(
                    "target {id:?} baseline value must be a finite ratio in [0, 1], got {}",
                    target.baseline.value
                )));
            }
            if target.baseline.stream_hours == Some(0) {
                return Err(SloError::new(format!(
                    "target {id:?} baseline cites 0 represented stream hours; calibration \
                     evidence that covers no time is not evidence"
                )));
            }
            if indicator.latency_thresholded && target.threshold_ms.is_none() {
                return Err(SloError::new(format!(
                    "latency indicator {id:?} requires threshold_ms; without it the objective \
                     has no defined miss"
                )));
            }
            if target.baseline.source.trim().is_empty() {
                return Err(SloError::new(format!(
                    "target {id:?} must cite the measured artifact it was calibrated from"
                )));
            }
            // The probe placeholder is a measurement *tool*, never evidence: a
            // targets file carrying one has skipped the measured-ratio step.
            let probe = target.baseline.source.starts_with(PROBE_SOURCE_MARKER);
            if !allow_probe && probe {
                return Err(SloError::new(format!(
                    "target {id:?} cites a latency probe, not calibration evidence; measure the \
                     candidate boundary over a run history first (slo-report with the probe target, \
                     then slo-baseline over the probed reports)"
                )));
            }
            // Everything a *calibrated* target must prove about the evidence it
            // cites. The proposal generates all of this; a probe is the tool
            // that produces it and is the one shape exempt (it only reaches
            // here on the measurement path, above). Without these checks a
            // target could carry a hand-written `source` string and no measured
            // history at all, which is a bypass around the whole evidence chain
            // this contract exists to build: `source` merely has to be
            // non-empty, and the runner manifest, series, and revision would
            // all be optional.
            if !probe {
                let series = target
                    .baseline
                    .series
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default();
                if series.is_empty() {
                    return Err(SloError::new(format!(
                        "target {id:?} baseline names no #58 compatibility series; a ratio \
                         measured outside a known mode+dataset_id+config_version+seed series is \
                         evidence about no particular workload"
                    )));
                }
                let mut named: BTreeSet<&str> = BTreeSet::new();
                for run in &target.baseline.runs {
                    let run_id = run.run_id.trim();
                    if run_id.is_empty() {
                        return Err(SloError::new(format!(
                            "target {id:?} baseline cites a contributing run with an empty run \
                             identity; a run that cannot be named is not evidence"
                        )));
                    }
                    if run.git_commit.trim().is_empty() {
                        return Err(SloError::new(format!(
                            "target {id:?} baseline cites run {run_id:?} without a revision; \
                             evidence whose revision is unknown cannot be re-measured or audited"
                        )));
                    }
                    if !named.insert(run_id) {
                        return Err(SloError::new(format!(
                            "target {id:?} baseline names run {run_id:?} more than once; repeated \
                             artifacts of one run are one measurement, so a run identity counts \
                             exactly one contribution"
                        )));
                    }
                }
                if (target.baseline.runs.len() as u64) < MIN_BASELINE_RUNS {
                    return Err(SloError::new(format!(
                        "target {id:?} baseline names {} contributing run(s); calibration \
                         evidence needs at least {MIN_BASELINE_RUNS} distinct measured runs, \
                         because one run is a data point and not a baseline",
                        target.baseline.runs.len()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&SloTarget> {
        self.targets.get(id)
    }
}

/// What produced the evaluated artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloProvenance {
    /// A #70 versioned scenario replay.
    ScenarioReplay,
    /// A checked-in replay fixture.
    ReplayFixture,
    /// A live operator session.
    LiveSession,
}

impl SloProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScenarioReplay => "scenario_replay",
            Self::ReplayFixture => "replay_fixture",
            Self::LiveSession => "live_session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SloEvaluationConfig {
    pub plane: TrafficPlane,
    pub provenance: SloProvenance,
    pub rolling_window_hours: u32,
    pub persistent_miss_windows: u64,
    /// Stable identity of the run this artifact measured, with the same
    /// semantics as #58's `recording.run_id`: stable across retries of one run,
    /// distinct across distinct runs ([`SloSource::run_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Default for SloEvaluationConfig {
    fn default() -> Self {
        Self {
            plane: TrafficPlane::Active,
            provenance: SloProvenance::ReplayFixture,
            rolling_window_hours: DEFAULT_ROLLING_WINDOW_HOURS,
            persistent_miss_windows: DEFAULT_PERSISTENT_MISS_WINDOWS,
            run_id: None,
        }
    }
}

/// Identity of the artifact the report was produced from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SloSource {
    pub dataset_id: String,
    pub git_commit: String,
    pub mode: ComparisonMode,
    pub config_version: String,
    pub seed: u64,
    pub stream_duration_ms: Option<u64>,
    pub provenance: SloProvenance,
    pub plane: TrafficPlane,
    /// Stable identity of the run this report measured, with the semantics #58
    /// already uses for `recording.run_id`: stable across retries of one run,
    /// distinct across distinct runs.
    ///
    /// It is the *only* run identity calibration pooling accepts. Report
    /// content cannot stand in for it: two independent deterministic runs of
    /// one revision produce byte-identical reports, so payload equality would
    /// collapse two measurements into one, while a retry can differ in content
    /// and would be counted twice. `None` means "a measurement, but not a
    /// calibratable run"; [`BaselineProposalSet::from_reports`] refuses such a
    /// report rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// One reporting window and the observations that fell inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SloWindow {
    pub kind: WindowKind,
    pub label: String,
    pub start_offset_ms: u64,
    /// Exclusive end offset.
    pub end_offset_ms: u64,
    pub admitted_events: u64,
}

/// Result of one indicator in one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SloIndicatorResult {
    pub id: String,
    pub kind: ObjectiveKind,
    pub unit: IndicatorUnit,
    pub window: String,
    pub window_kind: WindowKind,
    pub denominator: String,
    pub numerator: String,
    pub conforming: u64,
    /// Includes `unscored`: a sample the objective has no calibrated boundary
    /// for is part of the denominator, it is just not yet a pass or a failure.
    pub eligible: u64,
    /// Eligible events with no calibrated boundary to judge them against. These
    /// are the samples a threshold is calibrated from, never an SLO miss, so
    /// they never appear in `miss_attribution` or spend an error budget.
    #[serde(default)]
    pub unscored: u64,
    /// `None` means "not measured", never zero and never one.
    pub value: Option<f64>,
    pub target: Option<f64>,
    pub threshold_ms: Option<u64>,
    pub sample_floor: u64,
    pub status: SloStatus,
    pub miss_attribution: AttributionSummary,
    pub actions: Vec<SloAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_evidence: Option<String>,
}

/// Rolling allowance for one probabilistic objective.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBudget {
    pub indicator: String,
    pub window: String,
    pub window_kind: WindowKind,
    pub target: f64,
    pub allowed_misses: u64,
    pub observed_misses: u64,
    /// Negative once the budget is overspent.
    pub remaining_misses: i64,
    pub exhausted: bool,
}

/// A miss that should reach the after-action review process (#62).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AarCandidate {
    pub indicator: String,
    pub window_kind: WindowKind,
    pub windows_missed: u64,
    pub consecutive_miss_windows: u64,
    pub recommended_action: SloAction,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloVerdict {
    /// Every objective was decided, or had legitimately nothing to say, and
    /// every decided objective met its target.
    Ok,
    /// At least one decided objective missed.
    Breach,
    /// Nothing breached, but at least one objective's evidence is still
    /// unresolved — uncalibrated, or not measurable from this artifact. This is
    /// explicitly not a pass: an unmeasured invariant must never be readable as
    /// a satisfied one.
    Incomplete,
    /// Nothing was decided and nothing is outstanding either: no target is
    /// calibrated and no window carried data. This is not a pass.
    Uncalibrated,
}

/// The complete operational SLO report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SloReport {
    pub schema_version: String,
    pub catalog_version: String,
    /// False for an experimental report, so a consumer cannot silently promote
    /// it into the active user-facing SLO.
    pub counts_toward_active_slo: bool,
    pub source: SloSource,
    pub windows: Vec<SloWindow>,
    pub indicators: Vec<SloIndicatorResult>,
    pub latency_calibration: Vec<LatencyCalibration>,
    pub error_budgets: Vec<ErrorBudget>,
    pub failure_attribution: AttributionSummary,
    pub aar_candidates: Vec<AarCandidate>,
    /// Indicator ids that have no calibrated target yet.
    pub uncalibrated: Vec<String>,
    /// Indicator ids the artifact cannot measure yet, with the reason.
    pub not_yet_measured: Vec<String>,
    /// Measurement caveats a reader must not skip.
    pub limitations: Vec<String>,
    pub verdict: SloVerdict,
}

impl SloReport {
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, SloError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| SloError::new(format!("serialize SLO report: {error}")))
    }

    pub fn breaches(&self) -> Vec<&SloIndicatorResult> {
        self.indicators
            .iter()
            .filter(|result| result.status == SloStatus::Missed)
            .collect()
    }

    pub fn markdown_summary(&self) -> String {
        let plane = match self.source.plane {
            TrafficPlane::Active => "active",
            TrafficPlane::Experimental => "experimental",
        };
        let mut out = format!(
            "Operational SLO report — dataset `{}` @ `{}` ({} / {}, plane `{plane}`)\n\n",
            self.source.dataset_id,
            self.source.git_commit,
            self.source.mode.as_str(),
            self.source.provenance.as_str(),
        );
        out.push_str(
            "| Indicator | Window | Conforming | Eligible | Unscored | Value | Target | Status |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for result in &self.indicators {
            let value = result
                .value
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "-".to_owned());
            let target = result
                .target
                .map(|target| format!("{target:.4}"))
                .unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                result.id,
                result.window,
                result.conforming,
                result.eligible,
                result.unscored,
                value,
                target,
                result.status.as_str(),
            ));
        }
        for budget in &self.error_budgets {
            let suffix = if budget.exhausted {
                " — exhausted"
            } else {
                ""
            };
            out.push_str(&format!(
                "\n- error budget `{}` ({}): {}/{} allowed misses used{suffix}\n",
                budget.indicator, budget.window, budget.observed_misses, budget.allowed_misses,
            ));
        }
        if !self.not_yet_measured.is_empty() {
            out.push_str(&format!(
                "\nNot yet measurable from this artifact: {}\n",
                self.not_yet_measured.join("; ")
            ));
        }
        if !self.counts_toward_active_slo {
            out.push_str(
                "\n> This report covers an experimental plane and is excluded from the active \
                 user-facing SLO.\n",
            );
        }
        for limitation in &self.limitations {
            out.push_str(&format!("\n> {limitation}\n"));
        }
        for candidate in &self.aar_candidates {
            out.push_str(&format!(
                "\n- AAR candidate `{}` ({:?}): {}\n",
                candidate.indicator, candidate.recommended_action, candidate.rationale
            ));
        }
        let verdict = match self.verdict {
            SloVerdict::Ok => "OK",
            SloVerdict::Breach => "BREACH",
            SloVerdict::Incomplete => "INCOMPLETE",
            SloVerdict::Uncalibrated => "UNCALIBRATED",
        };
        out.push_str(&format!("\nVerdict: **{verdict}**\n"));
        out
    }
}

/// Evaluate one benchmark report into an operational SLO report.
pub fn evaluate(
    report: &BenchmarkReport,
    targets: &SloTargets,
    config: SloEvaluationConfig,
) -> Result<SloReport, SloError> {
    let catalog = catalog();
    // The evaluation path accepts a probe target (that is how a candidate
    // boundary gets measured); the targets-file path does not.
    targets.validate_for_evaluation(&catalog)?;

    let windows = build_windows(report, &config);
    let events = report.events.as_slice();

    let mut indicators = Vec::new();
    let mut error_budgets = Vec::new();
    let mut latency_calibration = Vec::new();
    let mut uncalibrated: Vec<String> = Vec::new();
    let mut not_yet_measured: Vec<String> = Vec::new();

    for indicator in &catalog {
        if indicator.evidence == EvidenceSource::Declared {
            not_yet_measured.push(format!(
                "{}: {}",
                indicator.id,
                indicator
                    .missing_evidence
                    .unwrap_or("evidence not recorded")
            ));
            for window in &windows {
                indicators.push(SloIndicatorResult {
                    id: indicator.id.to_owned(),
                    kind: indicator.kind,
                    unit: indicator.unit,
                    window: window.label.clone(),
                    window_kind: window.kind,
                    denominator: indicator.denominator.to_owned(),
                    numerator: indicator.numerator.to_owned(),
                    conforming: 0,
                    eligible: 0,
                    unscored: 0,
                    value: None,
                    target: None,
                    threshold_ms: None,
                    sample_floor: indicator.sample_floor,
                    status: SloStatus::NotYetMeasured,
                    miss_attribution: AttributionSummary::default(),
                    actions: indicator.actions.to_vec(),
                    missing_evidence: indicator.missing_evidence.map(ToOwned::to_owned),
                });
            }
            continue;
        }

        if indicator.latency_thresholded {
            latency_calibration.extend(latency_calibration_for(indicator.id, events, &windows));
        }

        let target = targets.get(indicator.id);
        for window in &windows {
            let scoped = window_events(events, window);
            let measurement = measure(indicator, &scoped, target);
            let status = status_for(indicator, &measurement, target);
            if status == SloStatus::Uncalibrated {
                uncalibrated.push(indicator.id.to_owned());
            }
            if let Some(budget) = error_budget_for(indicator, window, &measurement, target, status)
            {
                error_budgets.push(budget);
            }
            // Attribution is drawn from the *same* classifier that produced the
            // ratio, restricted to the events that are actually in this
            // indicator's denominator. An event outside the denominator can
            // neither conform nor miss, and an event the objective has no
            // calibrated boundary for is `Unscored` rather than a miss, so
            // neither can appear here.
            let misses = scoped
                .iter()
                .copied()
                .filter(|event| (indicator.classify)(event, target).is_miss());
            indicators.push(SloIndicatorResult {
                id: indicator.id.to_owned(),
                kind: indicator.kind,
                unit: indicator.unit,
                window: window.label.clone(),
                window_kind: window.kind,
                denominator: indicator.denominator.to_owned(),
                numerator: indicator.numerator.to_owned(),
                conforming: measurement.conforming,
                eligible: measurement.eligible,
                unscored: measurement.unscored,
                value: measurement.value,
                target: target.map(|target| target.target),
                threshold_ms: target.and_then(|target| target.threshold_ms),
                sample_floor: indicator.sample_floor,
                status,
                miss_attribution: AttributionSummary::of(misses),
                actions: indicator.actions.to_vec(),
                missing_evidence: None,
            });
        }
    }

    uncalibrated.sort();
    uncalibrated.dedup();
    not_yet_measured.sort();

    let verdict = overall_verdict(&indicators);

    let aar_candidates = aar_candidates(&indicators, config.persistent_miss_windows);
    let limitations = limitations(report, &windows);

    Ok(SloReport {
        schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
        catalog_version: SLO_CATALOG_VERSION.to_owned(),
        counts_toward_active_slo: config.plane == TrafficPlane::Active,
        source: SloSource {
            dataset_id: report.metadata.dataset_id.clone(),
            git_commit: report.metadata.git_commit.clone(),
            mode: report.mode,
            config_version: report.metadata.config_version.clone(),
            seed: report.metadata.seed,
            stream_duration_ms: report.metadata.stream_duration_ms,
            provenance: config.provenance,
            plane: config.plane,
            run_id: config.run_id.clone(),
        },
        windows,
        latency_calibration,
        indicators,
        error_budgets,
        failure_attribution: AttributionSummary::of(events.iter()),
        aar_candidates,
        uncalibrated,
        not_yet_measured,
        limitations,
        verdict,
    })
}

/// Aggregate verdict for a set of indicator results.
///
/// `Ok` is only reachable when every objective in the catalog has been decided
/// or has legitimately nothing to say. An objective whose evidence is still
/// unresolved — uncalibrated, under-sampled, or not measurable from the
/// artifact at all — makes the report `Incomplete`, never `Ok`, so a dashboard
/// cannot read "every calibrated objective passed" as "the runtime is
/// operating within its objectives" while the zero-tolerance invariants are
/// still unmeasured.
///
/// This is public because the rule is the part that has to be provable, and
/// [`evaluate`] cannot prove it on its own: while the catalog still contains
/// `not_yet_measured` objectives, every report is `Incomplete` for that reason
/// alone, so an under-sampled objective is invisible through the CLI. Taking
/// the results as an argument lets the aggregate contract be tested directly.
pub fn overall_verdict(indicators: &[SloIndicatorResult]) -> SloVerdict {
    if indicators
        .iter()
        .any(|result| result.status == SloStatus::Missed)
    {
        return SloVerdict::Breach;
    }
    if indicators
        .iter()
        .any(|result| result.status.is_unresolved())
    {
        return SloVerdict::Incomplete;
    }
    if indicators.iter().any(|result| result.status.is_decided()) {
        SloVerdict::Ok
    } else {
        SloVerdict::Uncalibrated
    }
}

// ---------------------------------------------------------------------------
// Baseline selection from a run history (#58 metrics -> #71 targets)
// ---------------------------------------------------------------------------

/// Target-free latency evidence pooled across a run history.
///
/// The artifact carries per-run percentiles, not raw samples, so a pooled
/// percentile cannot be reconstructed. A threshold chosen for a latency
/// objective has to hold for *every* run in the history, so the worst observed
/// percentile is the conservative evidence rather than the median run's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineLatency {
    pub field: String,
    pub runs: u64,
    pub samples: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

/// Pooled calibration evidence for one indicator over a history of runs.
///
/// This is deliberately *not* a target. It publishes the measured evidence a
/// target is chosen from, in the `BaselineEvidence` shape a target entry has to
/// cite, so choosing a target is a documented product decision above the
/// measurement rather than a number the fixture happened to produce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineProposal {
    pub indicator: String,
    /// Window series pooled. Always `session`: stream-hour and rolling windows
    /// partition the same run, so pooling them would count every event once per
    /// window it appears in.
    pub window: String,
    /// Number of contributing runs that carried eligible evidence.
    pub runs: u64,
    /// Pooled session denominator (ratio objectives only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conforming: Option<u64>,
    /// For a latency objective whose ratio was *measured* at a candidate
    /// boundary (the probe step), the threshold in ms the pooled ratio was
    /// computed at. A ratio is only comparable within one boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_ms: Option<u64>,
    /// Pooled measured ratio, ready to paste into a target entry as its
    /// `baseline`. The target ratio itself remains a product decision made
    /// *above* this value, not equal to it by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineEvidence>,
    /// Target-free latency percentiles; a `threshold_ms` is chosen from these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<BaselineLatency>,
}

/// Internal accumulator for pooled latency percentiles across contributing runs.
#[derive(Default)]
struct LatencyPool {
    runs: u64,
    samples: u64,
    p95_ms: u64,
    p99_ms: u64,
    max_ms: u64,
}

/// Calibration evidence pooled from a history of SLO reports (#58 history).
///
/// This is the missing link the issue names: repeated measured runs, not a
/// single one, are what turn a measurement into a baseline. It reads the
/// per-run SLO reports the project already produces and pools them, so a target
/// can be chosen from evidence instead of invented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineProposalSet {
    pub schema_version: String,
    pub catalog_version: String,
    pub reports: u64,
    pub contributing_reports: u64,
    /// Input reports that repeated a run identity already in the history with
    /// identical content: the same artifact passed twice, or a byte-identical
    /// retry. They carry no second measurement. A repeat of a run identity with
    /// *different* content is refused instead of pooled.
    #[serde(default)]
    pub duplicate_reports: u64,
    pub excluded_experimental_reports: u64,
    /// The #58 compatibility series every contributing run belongs to:
    /// `mode|dataset_id|config_version|seed`. Pooled evidence is only valid
    /// within one series; the field names it so the boundary is auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    pub revisions: Vec<String>,
    pub datasets: Vec<String>,
    /// Identity of every contributing run, sorted by run id. The same manifest
    /// is embedded in each proposal's [`BaselineEvidence`], so it survives the
    /// copy into a target.
    #[serde(default)]
    pub contributing_runs: Vec<BaselineRun>,
    pub proposals: BTreeMap<String, BaselineProposal>,
    /// Objectives with no eligible evidence in any contributing run.
    pub not_measured: Vec<String>,
    pub limitations: Vec<String>,
}

impl BaselineProposalSet {
    /// Pool a history of SLO reports into per-indicator calibration evidence.
    ///
    /// The rules a history must satisfy, each enforced rather than assumed:
    ///
    /// * **One compatibility series.** Reports from different
    ///   `mode + dataset_id + config_version + seed` series are the boundary
    ///   docs/performance-goals.adoc[#58] defines and are refused, not merged:
    ///   values written against a different workload/configuration are not
    ///   evidence about this one.
    /// * **One report contract.** A `schema_version` this build does not emit
    ///   fails closed, exactly like a target file for another contract.
    /// * **Distinct runs by identity, never by payload.** A run is identified
    ///   by [`SloSource::run_id`], the #58 `recording.run_id` semantics: two
    ///   independent deterministic runs of one revision produce byte-identical
    ///   reports and are two measurements, while a retry of one run is one
    ///   measurement however its numbers came out. A report with no run
    ///   identity is refused, because content cannot be used to infer one.
    ///   Repeated artifacts for one run identity collapse to that run when they
    ///   agree; when they *disagree* the history fails closed, because `run_id`
    ///   names the run but not which attempt is newer, and argument order is not
    ///   provenance. Fewer than [`MIN_BASELINE_RUNS`] distinct runs produce no
    ///   proposals at all, since one run calibrates nothing.
    /// * **Exact time.** Represented stream time is summed exactly; a partial
    ///   hour never rounds up to one. Sub-hour evidence omits `stream_hours`
    ///   instead of claiming an hour it did not stand for.
    ///
    /// Only `active`-plane reports contribute: an experimental route must not
    /// move the active baseline any more than it may move the active SLO.
    pub fn from_reports(reports: &[SloReport]) -> Result<Self, SloError> {
        for report in reports {
            if report.schema_version != SLO_REPORT_SCHEMA_VERSION {
                return Err(SloError::new(format!(
                    "report for dataset {:?} declares schema {:?} but this build emits \
                     {:?}; pooling a report written for another contract fails closed",
                    report.source.dataset_id, report.schema_version, SLO_REPORT_SCHEMA_VERSION
                )));
            }
            if report.catalog_version != SLO_CATALOG_VERSION {
                return Err(SloError::new(format!(
                    "report for dataset {:?} was evaluated against catalog {:?} but this build \
                     implements {:?}; pooling values written against different indicator \
                     definitions is not comparable",
                    report.source.dataset_id, report.catalog_version, SLO_CATALOG_VERSION
                )));
            }
            match report.source.run_id.as_deref().map(str::trim) {
                Some(run_id) if !run_id.is_empty() => {}
                _ => {
                    return Err(SloError::new(format!(
                        "report for dataset {:?} carries no run identity, so it is a measurement \
                         but not calibration evidence; regenerate it with `slo-report --run-id \
                         <stable-id>` (#58 `recording.run_id` semantics: stable across retries of \
                         one run, distinct across distinct runs), because two independent runs can \
                         produce identical reports and content can never stand in for identity",
                        report.source.dataset_id
                    )));
                }
            }
        }

        // A series is #58's compatibility boundary. Everything pooled must sit
        // inside one, or the pooled value describes no workload at all.
        let series_key = |report: &SloReport| {
            format!(
                "{}|{}|{}|{}",
                report.source.mode.as_str(),
                report.source.dataset_id,
                report.source.config_version,
                report.source.seed
            )
        };
        let mut series: Option<String> = None;
        for report in reports {
            if !report.counts_toward_active_slo {
                continue;
            }
            let key = series_key(report);
            match &series {
                None => series = Some(key),
                Some(first) if *first == key => {}
                Some(first) => {
                    return Err(SloError::new(format!(
                        "the history spans more than one compatibility series ({first:?} vs \
                         {key:?}); a baseline is only valid within one \
                         mode+dataset_id+config_version+seed series, so pool one series at a time"
                    )));
                }
            }
        }

        // Distinct-run detection is by run identity, never by payload. Two
        // independent deterministic runs of one revision produce identical
        // reports, and a retry of one run can produce a different one, so
        // content decides neither question. Content equality is used only to
        // recognise an identical repeat of one run; a *conflicting* repeat is
        // refused, never resolved (see below).
        let canonical_json = |report: &SloReport| -> Result<String, SloError> {
            serde_json::to_string(report)
                .map_err(|error| SloError::new(format!("canonicalize report: {error}")))
        };
        // run identity -> canonical payload of the artifact accepted for it
        let mut accepted: BTreeMap<String, String> = BTreeMap::new();
        let mut distinct: Vec<&SloReport> = Vec::new();
        let mut duplicate_reports = 0_u64;
        let mut excluded_experimental = 0_u64;
        for report in reports {
            if !report.counts_toward_active_slo {
                excluded_experimental += 1;
                continue;
            }
            let run_id = report
                .source
                .run_id
                .as_deref()
                .expect("every report's run identity was required above")
                .trim()
                .to_owned();
            let canonical = canonical_json(report)?;
            match accepted.entry(run_id) {
                Entry::Occupied(occupied) => {
                    // The same run identity with identical content is one run
                    // measured twice: a double-passed file or a byte-identical
                    // retry, which carries no second measurement.
                    if *occupied.get() == canonical {
                        duplicate_reports += 1;
                        continue;
                    }
                    // Two attempts of one run disagree. `run_id` says they are
                    // the same logical run; it does not say which attempt is
                    // newer, and the order in which paths reach this function is
                    // not provenance. Choosing the last one would make durable
                    // calibration evidence a function of filesystem/glob/argv
                    // ordering, so pooling fails closed instead: the operator
                    // names the authoritative artifact explicitly.
                    return Err(SloError::new(format!(
                        "run identity {:?} appears twice with different measurements; the two \
                         artifacts are attempts of one run and carry no authoritative ordering, so \
                         choosing one would make the baseline depend on the order the files were \
                         passed — pool exactly one artifact per run identity, or give each attempt \
                         its own --run-id",
                        occupied.key()
                    )));
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(canonical);
                    distinct.push(report);
                }
            }
        }

        // Identity of every contributing run, so the durable evidence can name
        // *which* runs formed a baseline rather than merely how many were seen.
        let mut run_manifest: Vec<BaselineRun> = distinct
            .iter()
            .map(|report| BaselineRun {
                run_id: report
                    .source
                    .run_id
                    .clone()
                    .expect("run identities were required above"),
                git_commit: report.source.git_commit.clone(),
            })
            .collect();
        run_manifest.sort_by(|left, right| left.run_id.cmp(&right.run_id));

        let mut limitations = vec![
            "baselines pool the `session` window only: stream-hour and rolling windows partition \
             the same run, so pooling them would count each event once per window"
                .to_owned(),
            "latency percentiles are the worst observed across contributing runs, not a pooled \
             percentile, because the artifact carries per-run percentiles rather than raw samples"
                .to_owned(),
        ];
        if duplicate_reports > 0 {
            limitations.push(format!(
                "{duplicate_reports} report(s) repeated a run identity already in the history with \
                 identical content and were not counted as further runs"
            ));
        }
        if (distinct.len() as u64) < MIN_BASELINE_RUNS {
            limitations.push(format!(
                "only {} distinct active run(s) in the history; at least {MIN_BASELINE_RUNS} are \
                 required before any pooled value is calibration evidence, so none is published",
                distinct.len()
            ));
            return Ok(Self {
                schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
                catalog_version: SLO_CATALOG_VERSION.to_owned(),
                reports: reports.len() as u64,
                contributing_reports: distinct.len() as u64,
                duplicate_reports,
                excluded_experimental_reports: excluded_experimental,
                revisions: distinct
                    .iter()
                    .map(|report| report.source.git_commit.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                datasets: distinct
                    .iter()
                    .map(|report| report.source.dataset_id.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                series,
                contributing_runs: run_manifest,
                proposals: BTreeMap::new(),
                not_measured: Vec::new(),
                limitations,
            });
        }

        let catalog = catalog();
        let latency_ids: BTreeSet<&str> = catalog
            .iter()
            .filter(|indicator| indicator.latency_thresholded)
            .map(|indicator| indicator.id)
            .collect();

        let mut datasets: BTreeSet<&str> = BTreeSet::new();
        let mut revisions: BTreeSet<&str> = BTreeSet::new();
        let mut config_versions: BTreeSet<&str> = BTreeSet::new();
        let mut modes: BTreeSet<&str> = BTreeSet::new();
        let mut seeds: BTreeSet<u64> = BTreeSet::new();
        // Represented stream time, summed exactly. No per-run ceil: ten
        // two-second runs are twenty seconds of evidence, not ten hours.
        let mut represented_ms = 0_u64;
        let mut reports_without_duration = 0_u64;

        // indicator -> (runs, eligible, conforming)
        let mut pools: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
        // (indicator, field) -> pooled percentile evidence
        let mut latency: BTreeMap<(&str, &str), LatencyPool> = BTreeMap::new();
        // Latency indicator -> the single candidate boundary every contributing
        // run was probed at. Ratios measured at different boundaries are not
        // poolable, so a mixed history is refused.
        let mut latency_thresholds: BTreeMap<&str, u64> = BTreeMap::new();

        for report in &distinct {
            datasets.insert(report.source.dataset_id.as_str());
            revisions.insert(report.source.git_commit.as_str());
            config_versions.insert(report.source.config_version.as_str());
            modes.insert(report.source.mode.as_str());
            seeds.insert(report.source.seed);
            match report.source.stream_duration_ms {
                Some(ms) => represented_ms = represented_ms.saturating_add(ms),
                None => reports_without_duration += 1,
            }

            for row in report
                .indicators
                .iter()
                .filter(|row| row.window == "session")
            {
                // Invariants are not probabilistic and never carry a ratio.
                if row.kind != ObjectiveKind::Slo || row.unit != IndicatorUnit::Ratio {
                    continue;
                }
                if latency_ids.contains(row.id.as_str()) {
                    // A latency objective's ratio depends on a candidate
                    // boundary. Without one (the target-free report) its
                    // evidence is the percentile; with one (the probe step)
                    // the measured ratio pools like any other. Ratios probed
                    // at two different boundaries are never pooled.
                    if let (Some(threshold_ms), true) = (row.threshold_ms, row.eligible > 0) {
                        match latency_thresholds.get(row.id.as_str()) {
                            Some(existing) if *existing != threshold_ms => {
                                return Err(SloError::new(format!(
                                    "latency indicator {:?} was probed at {} ms and {} ms in the \
                                     same history; ratios measured at different boundaries are not \
                                     poolable",
                                    row.id, existing, threshold_ms
                                )));
                            }
                            Some(_) => {}
                            None => {
                                latency_thresholds.insert(row.id.as_str(), threshold_ms);
                            }
                        }
                        let pool = pools.entry(row.id.as_str()).or_insert((0, 0, 0));
                        pool.0 += 1;
                        pool.1 += row.eligible;
                        pool.2 += row.conforming;
                    }
                    continue;
                }
                if row.eligible == 0 {
                    continue;
                }
                let pool = pools.entry(row.id.as_str()).or_insert((0, 0, 0));
                pool.0 += 1;
                pool.1 += row.eligible;
                pool.2 += row.conforming;
            }

            for point in report
                .latency_calibration
                .iter()
                .filter(|point| point.window == "session" && point.samples > 0)
            {
                let entry = latency
                    .entry((point.indicator.as_str(), point.field.as_str()))
                    .or_default();
                entry.runs += 1;
                entry.samples += point.samples;
                entry.p95_ms = entry.p95_ms.max(point.p95_ms);
                entry.p99_ms = entry.p99_ms.max(point.p99_ms);
                entry.max_ms = entry.max_ms.max(point.max_ms);
            }
        }

        // A boundary change restarts the series (docs/performance-goals.adoc),
        // so every identity field in the series key must be singular.
        debug_assert_eq!(modes.len(), 1);
        debug_assert_eq!(config_versions.len(), 1);
        debug_assert_eq!(seeds.len(), 1);
        let series = series.expect("distinct non-empty history has a series");
        // Provenance that survives the copy into a target entry: the evidence
        // block is the only durable part, so it must name what was measured —
        // which runs, over which dataset, revision, and series identity — not
        // merely how many of them there were. The same identities are also
        // carried structurally by [`BaselineEvidence::runs`] and
        // [`BaselineEvidence::series`]; the prose is for a reader.
        let provenance = |runs: u64, boundary: Option<u64>| {
            let mut source = format!(
                "slo-baseline: {runs} distinct session run(s) [{}]; series={series}; datasets={}; \
                 revisions={}; config={}; mode={}; seed={}",
                // `join_sorted` re-sorts, which is harmless: run ids are unique.
                join_sorted(run_manifest.iter().map(|run| run.run_id.as_str())),
                join_sorted(datasets.iter()),
                join_sorted(revisions.iter()),
                join_sorted(config_versions.iter()),
                join_sorted(modes.iter()),
                join_sorted(seeds.iter()),
            );
            if let Some(threshold_ms) = boundary {
                source.push_str(&format!("; probe_threshold_ms={threshold_ms}"));
            }
            source
        };
        // Stream coverage over the pooled history, exact: a partial hour is
        // evidence of a partial hour, not of one. Evidence covering less than a
        // full hour omits `stream_hours` rather than claiming one.
        let whole_hours = represented_ms / STREAM_HOUR_MS;
        let stream_hours = (whole_hours > 0).then_some(whole_hours);
        let mut proposals: BTreeMap<String, BaselineProposal> = BTreeMap::new();
        for (id, (runs, eligible, conforming)) in &pools {
            proposals.insert(
                (*id).to_owned(),
                BaselineProposal {
                    indicator: (*id).to_owned(),
                    window: "session".to_owned(),
                    runs: *runs,
                    threshold_ms: None,
                    eligible: Some(*eligible),
                    conforming: Some(*conforming),
                    baseline: Some(BaselineEvidence {
                        value: *conforming as f64 / *eligible as f64,
                        source: provenance(*runs, None),
                        stream_hours,
                        series: Some(series.clone()),
                        runs: run_manifest.clone(),
                    }),
                    latency: None,
                },
            );
        }
        for ((id, field), pool) in &latency {
            let proposal = proposals
                .entry((*id).to_owned())
                .or_insert_with(|| BaselineProposal {
                    indicator: (*id).to_owned(),
                    window: "session".to_owned(),
                    runs: 0,
                    threshold_ms: None,
                    eligible: None,
                    conforming: None,
                    baseline: None,
                    latency: None,
                });
            proposal.runs = proposal.runs.max(pool.runs);
            // The probe step closes the calibration cycle: with a single
            // candidate boundary across the history, the measured ratio is a
            // valid `BaselineEvidence` and the first latency target can be
            // calibrated without inventing anything.
            if let Some(threshold_ms) = latency_thresholds.get(id) {
                proposal.threshold_ms = Some(*threshold_ms);
                if let Some((runs, eligible, conforming)) = pools.get(id) {
                    proposal.eligible = Some(*eligible);
                    proposal.conforming = Some(*conforming);
                    proposal.baseline = Some(BaselineEvidence {
                        value: *conforming as f64 / *eligible as f64,
                        source: provenance(*runs, Some(*threshold_ms)),
                        stream_hours,
                        series: Some(series.clone()),
                        runs: run_manifest.clone(),
                    });
                }
            }
            proposal.latency = Some(BaselineLatency {
                field: (*field).to_owned(),
                runs: pool.runs,
                samples: pool.samples,
                p95_ms: pool.p95_ms,
                p99_ms: pool.p99_ms,
                max_ms: pool.max_ms,
            });
        }

        let not_measured: Vec<String> = catalog
            .iter()
            .filter(|indicator| {
                indicator.kind == ObjectiveKind::Slo
                    && indicator.evidence == EvidenceSource::Measured
            })
            .filter(|indicator| !proposals.contains_key(indicator.id))
            .map(|indicator| indicator.id.to_owned())
            .collect();

        if reports_without_duration > 0 {
            limitations.push(format!(
                "{reports_without_duration} contributing report(s) recorded no stream duration, so \
                 the represented time may be under-counted"
            ));
        }
        if excluded_experimental > 0 {
            limitations.push(format!(
                "{excluded_experimental} experimental-plane report(s) were excluded from the \
                 active baseline"
            ));
        }
        if stream_hours.is_none() {
            limitations.push(format!(
                "the history represents {represented_ms} ms of logical stream time, less than one \
                 full stream hour, so `stream_hours` is omitted rather than rounded up"
            ));
        }

        Ok(Self {
            schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
            catalog_version: SLO_CATALOG_VERSION.to_owned(),
            reports: reports.len() as u64,
            contributing_reports: distinct.len() as u64,
            duplicate_reports,
            excluded_experimental_reports: excluded_experimental,
            revisions: revisions.into_iter().map(ToOwned::to_owned).collect(),
            datasets: datasets.into_iter().map(ToOwned::to_owned).collect(),
            series: Some(series),
            contributing_runs: run_manifest,
            proposals,
            not_measured,
            limitations,
        })
    }

    pub fn to_json_pretty(&self) -> Result<Vec<u8>, SloError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| SloError::new(format!("serialize baseline proposal: {error}")))
    }

    pub fn markdown_summary(&self) -> String {
        let mut out = format!(
            "Operational SLO baseline proposal — {} report(s) pooled ({} contributing distinct, {} \
             duplicate, {} experimental excluded)\n\n",
            self.reports,
            self.contributing_reports,
            self.duplicate_reports,
            self.excluded_experimental_reports
        );
        if let Some(series) = &self.series {
            out.push_str(&format!("Series: `{series}`\n\n"));
        }
        if !self.contributing_runs.is_empty() {
            out.push_str(&format!(
                "Runs: {}\n\n",
                self.contributing_runs
                    .iter()
                    .map(|run| format!("`{}`@{}", run.run_id, run.git_commit))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push_str(
            "| Indicator | Runs | Eligible | Conforming | Value | Threshold ms | Stream hours | Latency p95/p99/max |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for proposal in self.proposals.values() {
            let value = proposal
                .baseline
                .as_ref()
                .map(|baseline| format!("{:.4}", baseline.value))
                .unwrap_or_else(|| "-".to_owned());
            let threshold = proposal
                .threshold_ms
                .map(|threshold| threshold.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let stream_hours = proposal
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.stream_hours)
                .map(|hours| hours.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let latency = proposal
                .latency
                .as_ref()
                .map(|latency| {
                    format!(
                        "{} / {} / {}",
                        latency.p95_ms, latency.p99_ms, latency.max_ms
                    )
                })
                .unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                proposal.indicator,
                proposal.runs,
                proposal
                    .eligible
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                proposal
                    .conforming
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                value,
                threshold,
                stream_hours,
                latency,
            ));
        }
        if !self.not_measured.is_empty() {
            out.push_str(&format!(
                "\nNot measured in any contributing run: {}\n",
                self.not_measured.join("; ")
            ));
        }
        for limitation in &self.limitations {
            out.push_str(&format!("\n> {limitation}\n"));
        }
        out.push_str(
            "\nThis is measured evidence, not a target. A target ratio is a product decision \
             made above the value shown, and the chosen `baseline` block is copied from here.\n",
        );
        out
    }
}

/// Join a collection into a comma-separated list for provenance text.
fn join_sorted<I>(values: I) -> String
where
    I: IntoIterator,
    I::Item: ToString,
{
    let mut items: Vec<String> = values.into_iter().map(|value| value.to_string()).collect();
    items.sort_unstable();
    items.dedup();
    items.join(",")
}

fn status_for(
    indicator: &SloIndicator,
    measurement: &SloMeasurement,
    target: Option<&SloTarget>,
) -> SloStatus {
    if indicator.kind == ObjectiveKind::Invariant {
        // A zero-tolerance invariant needs no configured target: the tolerated
        // value is zero by definition.
        return match measurement.value {
            Some(value) if value > 0.0 => SloStatus::Missed,
            Some(_) => SloStatus::Met,
            None => SloStatus::NoData,
        };
    }
    if measurement.eligible == 0 {
        return SloStatus::NoData;
    }
    let Some(target) = target else {
        return SloStatus::Uncalibrated;
    };
    // A target alone is not a calibrated objective: if the classifier could not
    // score any sample, the objective is still unresolved. This is defensive —
    // `validate` already refuses a thresholdless latency target — but the rule
    // that an unscoreable objective is never a breach is enforced here rather
    // than inferred from catalog metadata elsewhere.
    if measurement.unscored > 0 {
        return SloStatus::Uncalibrated;
    }
    if measurement.eligible < indicator.sample_floor {
        return SloStatus::InsufficientSamples;
    }
    match measurement.value {
        Some(value) if value >= target.target => SloStatus::Met,
        Some(_) => SloStatus::Missed,
        // `value` is `None` only when nothing was eligible, already handled
        // above; treating it as a miss would make "unmeasured" mean "failed".
        None => SloStatus::Uncalibrated,
    }
}

/// Build the error budget for one probabilistic objective.
///
/// Invariants are excluded by construction: a stale post-stop dispatch or an
/// unauthorized privileged action has no allowance to spend.
fn error_budget_for(
    indicator: &SloIndicator,
    window: &SloWindow,
    measurement: &SloMeasurement,
    target: Option<&SloTarget>,
    status: SloStatus,
) -> Option<ErrorBudget> {
    if indicator.kind == ObjectiveKind::Invariant || !status.is_decided() {
        return None;
    }
    let target = target?;
    let allowed_misses = ((1.0 - target.target) * measurement.eligible as f64).floor() as u64;
    let observed_misses = measurement.misses();
    Some(ErrorBudget {
        indicator: indicator.id.to_owned(),
        window: window.label.clone(),
        window_kind: window.kind,
        target: target.target,
        allowed_misses,
        observed_misses,
        remaining_misses: allowed_misses as i64 - observed_misses as i64,
        exhausted: observed_misses > allowed_misses,
    })
}

/// Misses that belong in an after-action review (#62).
///
/// A single noisy window is not an incident; a run of them is. Zero-tolerance
/// invariants bypass the persistence threshold because there is no acceptable
/// streak to wait for.
fn aar_candidates(indicators: &[SloIndicatorResult], threshold: u64) -> Vec<AarCandidate> {
    let mut candidates = Vec::new();
    let mut series_keys: Vec<(&str, WindowKind)> = indicators
        .iter()
        .map(|result| (result.id.as_str(), result.window_kind))
        .collect();
    series_keys.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
    series_keys.dedup();

    for (id, window_kind) in series_keys {
        // Persistence is a property of one window *series*, not of the whole
        // indicator row. A session aggregate, an hourly bucket and a rolling
        // bucket are different observations of the same runtime, and letting
        // their misses flow into one counter would let a two-hour run
        // manufacture a three-hour streak. Rolling windows are keyed by their
        // span, so a 3h and a 6h window never share a streak either.
        let series: Vec<&SloIndicatorResult> = indicators
            .iter()
            .filter(|result| result.id == id && result.window_kind == window_kind)
            .collect();
        let windows = series.len() as u64;
        let mut run = 0_u64;
        let mut total = 0_u64;
        for result in series {
            if result.status == SloStatus::Missed {
                run += 1;
                total += 1;
            } else {
                run = 0;
            }
            let zero_tolerance = result.kind == ObjectiveKind::Invariant;
            if run == 0 || (!zero_tolerance && run < threshold) {
                continue;
            }
            let rationale = if zero_tolerance {
                format!(
                    "zero-tolerance invariant missed in {} of {} {} window(s); no error budget applies",
                    total,
                    windows,
                    window_kind.as_str(),
                )
            } else {
                format!(
                    "missed {} consecutive {} window(s) ({} of {} missed in total)",
                    run,
                    window_kind.as_str(),
                    total,
                    windows
                )
            };
            candidates.push(AarCandidate {
                indicator: id.to_owned(),
                window_kind,
                windows_missed: total,
                consecutive_miss_windows: run,
                recommended_action: result
                    .actions
                    .first()
                    .copied()
                    .unwrap_or(SloAction::OpenOptimizationIssue),
                rationale,
            });
        }
    }
    candidates
}

/// Build the reporting windows for one report.
///
/// Bucketing is over the logical stream offset recorded at admission. An
/// artifact that carries no offsets degrades to a single session window and
/// says so, rather than inventing hour boundaries from the event index.
fn build_windows(report: &BenchmarkReport, config: &SloEvaluationConfig) -> Vec<SloWindow> {
    let events = &report.events;
    let observed_end = events
        .iter()
        .filter_map(|event| event.stream_offset_ms)
        .max()
        .map(|offset| offset.saturating_add(1))
        .unwrap_or(0);
    let mut windows = vec![SloWindow {
        kind: WindowKind::Session,
        label: "session".to_owned(),
        start_offset_ms: 0,
        end_offset_ms: report.metadata.stream_duration_ms.unwrap_or(observed_end),
        admitted_events: events.len() as u64,
    }];

    // Every observation needs an offset, or the buckets would silently drop
    // events and the per-hour denominators would disagree with the session.
    if events.is_empty() || events.iter().any(|event| event.stream_offset_ms.is_none()) {
        return windows;
    }
    // The represented stream is at least as long as the workload declares, even
    // if its last event arrived earlier. Bucketing on the last observation
    // alone would quietly truncate the tail of a long run and under-report how
    // many stream hours it actually stood for.
    let represented_ms = observed_end.max(report.metadata.stream_duration_ms.unwrap_or(0));
    if represented_ms <= STREAM_HOUR_MS {
        return windows;
    }
    let max_hour = represented_ms.saturating_sub(1) / STREAM_HOUR_MS;

    for hour in 0..=max_hour {
        let start = hour * STREAM_HOUR_MS;
        windows.push(SloWindow {
            kind: WindowKind::StreamHour,
            label: format!("stream-hour-{hour}"),
            start_offset_ms: start,
            end_offset_ms: start + STREAM_HOUR_MS,
            admitted_events: admitted_between(events, start, start + STREAM_HOUR_MS),
        });
    }

    let hours = max_hour + 1;
    let span = u64::from(config.rolling_window_hours.max(1));
    let mut start_hour = 0;
    while start_hour + span <= hours {
        let start = start_hour * STREAM_HOUR_MS;
        let end = (start_hour + span) * STREAM_HOUR_MS;
        windows.push(SloWindow {
            kind: WindowKind::RollingStreamHours {
                window_hours: config.rolling_window_hours,
            },
            label: format!("rolling-{span}h-from-hour-{start_hour}"),
            start_offset_ms: start,
            end_offset_ms: end,
            admitted_events: admitted_between(events, start, end),
        });
        start_hour += 1;
    }

    windows
}

fn admitted_between(events: &[EventObservation], start: u64, end: u64) -> u64 {
    events
        .iter()
        .filter(|event| {
            event
                .stream_offset_ms
                .is_some_and(|offset| offset >= start && offset < end)
        })
        .count() as u64
}

fn window_events<'a>(
    events: &'a [EventObservation],
    window: &SloWindow,
) -> Vec<&'a EventObservation> {
    if window.kind == WindowKind::Session {
        return events.iter().collect();
    }
    events
        .iter()
        .filter(|event| {
            event.stream_offset_ms.is_some_and(|offset| {
                offset >= window.start_offset_ms && offset < window.end_offset_ms
            })
        })
        .collect()
}

fn latency_calibration_for(
    indicator: &str,
    events: &[EventObservation],
    windows: &[SloWindow],
) -> Vec<LatencyCalibration> {
    let (field, audio): (&str, bool) = if indicator.contains("audio") {
        ("event_to_first_audio_ms", true)
    } else {
        ("event_to_first_visible_reaction_ms", false)
    };
    windows
        .iter()
        .map(|window| {
            let mut values: Vec<u64> = window_events(events, window)
                .into_iter()
                .filter_map(|event| {
                    if audio {
                        event.event_to_first_audio_ms
                    } else {
                        event.event_to_first_visible_reaction_ms
                    }
                })
                .collect();
            values.sort_unstable();
            LatencyCalibration {
                indicator: indicator.to_owned(),
                window: window.label.clone(),
                field: field.to_owned(),
                samples: values.len() as u64,
                p50_ms: nearest_rank(&values, 50),
                p95_ms: nearest_rank(&values, 95),
                p99_ms: nearest_rank(&values, 99),
                max_ms: values.last().copied().unwrap_or(0),
            }
        })
        .collect()
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn limitations(report: &BenchmarkReport, windows: &[SloWindow]) -> Vec<String> {
    let mut limitations = vec![
        "denominators are retained observations, not offered events: telemetry retention is \
         bounded, so a run that evicted observations reports the retained set"
            .to_owned(),
    ];
    if windows
        .iter()
        .all(|window| window.kind == WindowKind::Session)
    {
        let has_offsets = !report.events.is_empty()
            && report
                .events
                .iter()
                .all(|event| event.stream_offset_ms.is_some());
        limitations.push(if has_offsets {
            format!(
                "this run represents {} ms of logical stream time, less than one stream hour, so \
                 no stream-hour or rolling window exists to report",
                report
                    .events
                    .iter()
                    .filter_map(|event| event.stream_offset_ms)
                    .max()
                    .unwrap_or(0)
            )
        } else {
            "this artifact carries no per-event logical stream offsets, so only the session \
             window is available; stream-hour and rolling windows need the offset recorded at \
             admission"
                .to_owned()
        });
    }
    if report.summary.events != report.events.len() {
        limitations.push(format!(
            "the summary counted {} events while the report retains {}; the retained set was \
             used for every denominator",
            report.summary.events,
            report.events.len()
        ));
    }
    limitations
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SloError {
    message: String,
}

impl SloError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SloError {}

impl From<TelemetryError> for SloError {
    fn from(error: TelemetryError) -> Self {
        Self::new(error.to_string())
    }
}
