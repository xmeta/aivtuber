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
//! 2. **Absence of evidence is never a pass.** An indicator whose eligible
//!    denominator is empty reports [`SloStatus::NoData`], and an indicator the
//!    benchmark artifact cannot reproduce at all reports
//!    [`SloStatus::NotYetMeasured`] together with the evidence that is missing.
//!    Neither counts as [`SloStatus::Met`].
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
use std::collections::BTreeMap;
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
    /// Events the objective applies to.
    pub eligible: u64,
    /// Reported value in the indicator's unit. `None` means "not measured",
    /// which is never 0 and never 1.
    pub value: Option<f64>,
}

impl SloMeasurement {
    /// Misses are always `eligible - conforming`, so the error budget and the
    /// indicator can never disagree about what was spent.
    pub fn misses(&self) -> u64 {
        self.eligible.saturating_sub(self.conforming)
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
    Outside,
    Conforms,
    Misses,
}

/// Raw counts produced by one indicator over one window.
fn measure(
    indicator: &SloIndicator,
    events: &[&EventObservation],
    target: Option<&SloTarget>,
) -> SloMeasurement {
    let eligible = events
        .iter()
        .filter(|event| (indicator.classify)(event, target) != EventVerdict::Outside)
        .count() as u64;
    let conforming = events
        .iter()
        .filter(|event| (indicator.classify)(event, target) == EventVerdict::Conforms)
        .count() as u64;
    // A latency objective with no calibrated threshold has an eligible set but
    // no defined miss, so it reports the sample count as calibration evidence
    // and no ratio at all. That is deliberately different from 0%.
    let unscorable =
        indicator.latency_thresholded && target.and_then(|target| target.threshold_ms).is_none();
    let value = (!unscorable && eligible > 0).then(|| conforming as f64 / eligible as f64);
    SloMeasurement {
        conforming,
        eligible,
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
    /// Latency objectives need a configured `threshold_ms` to have a miss at all.
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
/// so the event is eligible and counted as not-conforming — which is what
/// makes the ratio `None` rather than a misleading zero.
fn classify_latency(latency: Option<u64>, target: Option<&SloTarget>) -> EventVerdict {
    let Some(latency) = latency else {
        return EventVerdict::Outside;
    };
    match target.and_then(|target| target.threshold_ms) {
        Some(threshold) if latency <= threshold => EventVerdict::Conforms,
        Some(_) => EventVerdict::Misses,
        None => EventVerdict::Misses,
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
    pub fn validate(&self, catalog: &[SloIndicator]) -> Result<(), SloError> {
        if self.catalog_version != SLO_CATALOG_VERSION {
            return Err(SloError::new(format!(
                "target file targets catalog {:?} but this build implements {:?}; a threshold \
                 calibrated against different indicator definitions is not comparable",
                self.catalog_version, SLO_CATALOG_VERSION
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
            if !(0.0..=1.0).contains(&target.target) {
                return Err(SloError::new(format!(
                    "target {id:?} must be a ratio in [0, 1], got {}",
                    target.target
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SloEvaluationConfig {
    pub plane: TrafficPlane,
    pub provenance: SloProvenance,
    pub rolling_window_hours: u32,
    pub persistent_miss_windows: u64,
}

impl Default for SloEvaluationConfig {
    fn default() -> Self {
        Self {
            plane: TrafficPlane::Active,
            provenance: SloProvenance::ReplayFixture,
            rolling_window_hours: DEFAULT_ROLLING_WINDOW_HOURS,
            persistent_miss_windows: DEFAULT_PERSISTENT_MISS_WINDOWS,
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
    pub eligible: u64,
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
            "| Indicator | Window | Conforming | Eligible | Value | Target | Status |\n\
             |---|---|---|---|---|---|---|\n",
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
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                result.id,
                result.window,
                result.conforming,
                result.eligible,
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
    targets.validate(&catalog)?;

    let windows = build_windows(report, config);
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
            // neither conform nor miss, so it cannot appear here.
            let misses = scoped
                .iter()
                .copied()
                .filter(|event| (indicator.classify)(event, target) == EventVerdict::Misses);
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

/// Aggregate verdict for the whole report.
///
/// `Ok` is only reachable when every objective in the catalog has been decided
/// or has legitimately nothing to say. An objective whose evidence is still
/// unresolved — uncalibrated, or not measurable from the artifact at all —
/// makes the report `Incomplete`, never `Ok`, so a dashboard cannot read
/// "every calibrated objective passed" as "the runtime is operating within its
/// objectives" while the zero-tolerance invariants are still unmeasured.
fn overall_verdict(indicators: &[SloIndicatorResult]) -> SloVerdict {
    if indicators
        .iter()
        .any(|result| result.status == SloStatus::Missed)
    {
        return SloVerdict::Breach;
    }
    let unresolved = indicators.iter().any(|result| {
        matches!(
            result.status,
            SloStatus::Uncalibrated | SloStatus::NotYetMeasured
        )
    });
    if unresolved {
        return SloVerdict::Incomplete;
    }
    if indicators.iter().any(|result| result.status.is_decided()) {
        SloVerdict::Ok
    } else {
        SloVerdict::Uncalibrated
    }
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
    if measurement.eligible < indicator.sample_floor {
        return SloStatus::InsufficientSamples;
    }
    match measurement.value {
        Some(value) if value >= target.target => SloStatus::Met,
        _ => SloStatus::Missed,
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
fn build_windows(report: &BenchmarkReport, config: SloEvaluationConfig) -> Vec<SloWindow> {
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
