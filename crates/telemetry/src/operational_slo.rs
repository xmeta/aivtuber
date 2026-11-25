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
use sha2::{Digest, Sha256};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::{
    BenchmarkReport, BenchmarkResult, ComparisonMode, EventObservation, MetricValue, RouteClass,
    TelemetryError,
};

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

mod baseline;
mod catalog;
use baseline::*;
pub use baseline::{BaselineLatency, BaselineProposal, BaselineProposalSet};
pub use catalog::catalog;

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
    /// Worst observed sample, when the source artifact recorded one. `None`
    /// means "not recorded" and is never rendered as zero: a #58 aggregate
    /// history row carries percentiles but no per-run maximum, and inventing
    /// one would be a measurement the run never made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<u64>,
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
    /// Pooled eligible-sample count the value was measured over.
    ///
    /// Optional on the *wire* on purpose: an artifact written before this
    /// denominator existed — a version-1 targets or proposal file — must still
    /// parse, or its own `schema_version` could never be read to say what it
    /// is, and the incompatible change would demand a version bump the shared
    /// report contract (whose own shape did not change) cannot carry. Required
    /// by [`SloTargets::validate`] instead: a calibrated target whose evidence
    /// does not carry its denominator is refused there, in the contract, where
    /// the indicator's sample floor can actually be applied to it — evidence
    /// whose sample count is unknown can only be trusted by faith.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible: Option<u64>,
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
                eligible: Some(0),
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
                // Evidence below the indicator's own decision floor cannot
                // justify the decision: a baseline pooled from fewer eligible
                // samples than `sample_floor` would recreate exactly the
                // low-sample calibration this contract refuses to evaluate
                // with, so carrying the denominator is what lets that rule be
                // checked instead of trusted.
                //
                // A baseline that does not carry the denominator at all is the
                // older wire shape of this evidence. It parses — otherwise the
                // file's own `schema_version` could never be read to say what
                // contract it wrote — and it is refused here instead, named
                // for what is missing rather than as a parse failure.
                let Some(eligible) = target.baseline.eligible else {
                    return Err(SloError::new(format!(
                        "target {id:?} baseline does not carry `eligible`, the pooled sample \
                         denominator it was measured over; evidence whose sample count is unknown \
                         cannot be checked against indicator {id:?} sample floor of {} and cannot \
                         justify its target",
                        indicator.sample_floor
                    )));
                };
                if eligible < indicator.sample_floor {
                    return Err(SloError::new(format!(
                        "target {id:?} baseline carries {eligible} eligible sample(s), below \
                         indicator {id:?} sample floor of {}; evidence too thin to decide the \
                         objective cannot justify its target",
                        indicator.sample_floor
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
    /// SHA-256 over the canonical (recursively key-sorted) JSON of the replay
    /// report this one was evaluated from.
    ///
    /// The #58 series fields and the revision say *which workload* a run
    /// belongs to; they cannot say *which artifact* produced it, because two
    /// different observations of one revision, series, and duration are
    /// indistinguishable without it. This is what makes the pairing a stored
    /// report claims verifiable rather than assumed: the latency probe
    /// re-evaluates the source artifact, so a report paired with anything but
    /// the artifact it names could yield a ratio that run never measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
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

/// SHA-256 over the canonical (recursively key-sorted) JSON of the
/// replay report, recorded as [`SloSource::source_digest`].
///
/// The digest names the *artifact* the evaluation consumed. The #58
/// series fields say which workload a run belongs to, but two different
/// observations of one revision, series and duration are
/// indistinguishable without it: the latency probe verifies a stored
/// report's pairing by re-evaluating the artifact it claims to come
/// from, so a report paired with anything but the artifact it names
/// could yield a ratio that run never measured.
///
/// The canonical form is defined so a verifier holding only the stored
/// JSON document — `scripts/slo-record.mjs`, in JavaScript — can
/// recompute it byte-for-byte without this crate:
///
/// - object keys are sorted recursively and explicitly, so the form
///   depends on content, not on struct declaration order or on
///   serde_json's map backing (nothing here leans on the absence of
///   `preserve_order`);
/// - numbers are rendered exactly as ECMAScript renders them
///   (`JSON.stringify`): integer-backed values plain, floats by
///   shortest round-trip digits with no trailing `.0` and JS exponent
///   thresholds. serde_json would write `1.0` where ECMAScript writes
///   `1`, and a parsed document cannot tell an integral float from an
///   integer, so *this* side adopts ECMAScript's rendering — the type
///   distinction serde_json keeps is erased exactly where JSON
///   parsing erases it;
/// - strings, booleans and null use serde_json's standard escapes,
///   which coincide with `JSON.stringify` for every valid UTF-8
///   document.
fn source_digest_of(report: &BenchmarkReport) -> Result<String, SloError> {
    let value = serde_json::to_value(report)
        .map_err(|error| SloError::new(format!("canonicalize report: {error}")))?;
    Ok(canonical_json_digest(&value))
}

/// Digest of one JSON value in the canonical form [`source_digest_of`]
/// documents. Split out so the contract can be pinned against
/// ECMAScript's own rendering in tests without building a full report.
///
/// Public because the *rule* is shared, not just the function: an artifact
/// that names the document it describes — the SLO source report here, the
/// #105 long-range fixture in `aivtuber-app` — has to digest it the same way
/// on every machine, and a second canonicalization would be a second
/// contract that could disagree with this one.
pub fn canonical_json_digest(value: &serde_json::Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Render one JSON value in the canonical form: object keys sorted
/// (explicitly, never trusting serde_json's map implementation), array
/// order preserved because it is content, not layout.
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(*key).expect("a map key serializes"));
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        serde_json::Value::String(string) => {
            out.push_str(&serde_json::to_string(string).expect("a string serializes"));
        }
        serde_json::Value::Number(number) => write_canonical_number(number, out),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Null => out.push_str("null"),
    }
}

/// Integer-backed numbers keep serde_json's plain digits — the same
/// digits ECMAScript prints. Floats take the ECMAScript rendering.
fn write_canonical_number(number: &serde_json::Number, out: &mut String) {
    if let Some(unsigned) = number.as_u64() {
        out.push_str(&unsigned.to_string());
    } else if let Some(signed) = number.as_i64() {
        out.push_str(&signed.to_string());
    } else if let Some(float) = number.as_f64() {
        write_canonical_f64(float, out);
    }
}

/// ECMAScript `Number::toString` for one finite float: shortest
/// round-trip digits, decimal notation for `1e-6 ≤ |value| < 1e21`,
/// exponential `1e+21` / `1e-7` outside, never a trailing `.0`, and
/// `-0.0` collapsing to `0` exactly as `String(-0)` does.
fn write_canonical_f64(value: f64, out: &mut String) {
    if value == 0.0 {
        out.push('0');
        return;
    }
    if !value.is_finite() {
        // serde_json renders non-finite floats as `null`; a verifier
        // reading the stored document sees `null` too, so `null` is the
        // consistent canonical rendering on this side as well.
        out.push_str("null");
        return;
    }
    let (digits, exponent) = shortest_digits(value.abs());
    if value < 0.0 {
        out.push('-');
    }
    write_ecmascript_digits(&digits, exponent, out);
}

/// Shortest round-trip decimal digits of a positive finite float, with
/// the decimal exponent `n` such that the value equals `0.<digits> ×
/// 10^n`. The digits come from serde_json's own ryu rendering — never
/// recomputed here — so this side and the stored document can never
/// disagree about *which* digits are shortest; only their placement is
/// rewritten into ECMAScript's notation.
fn shortest_digits(value: f64) -> (Vec<u8>, i32) {
    let rendered = serde_json::to_string(&value).expect("a float serializes");
    let (mantissa, exponent) = match rendered.split_once('e') {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent
                .parse::<i32>()
                .expect("ryu writes a plain decimal exponent"),
        ),
        None => (rendered.as_str(), 0),
    };
    let (integer, fraction) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (mantissa, ""),
    };
    // value = <all digits as an integer> × 10^scale, then normalize to
    // shortest significant digits, adjusting the scale per digit lost.
    let mut digits: Vec<u8> = integer
        .bytes()
        .chain(fraction.bytes())
        .map(|byte| byte - b'0')
        .collect();
    let mut scale = exponent - fraction.len() as i32;
    while digits.len() > 1 && digits[0] == 0 {
        digits.remove(0);
    }
    while digits.len() > 1 && *digits.last().expect("digits is non-empty") == 0 {
        digits.pop();
        scale += 1;
    }
    let exponent = scale + digits.len() as i32;
    (digits, exponent)
}

/// ECMAScript `Number::toString` placement rules for shortest digits
/// `s` (length `k`) with value `0.s × 10^n`.
fn write_ecmascript_digits(digits: &[u8], exponent: i32, out: &mut String) {
    let significand: String = digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    let count = digits.len() as i32;
    if count <= exponent && exponent <= 21 {
        // s followed by n−k zeros
        out.push_str(&significand);
        for _ in 0..(exponent - count) {
            out.push('0');
        }
    } else if 0 < exponent && exponent <= count {
        // decimal point inside the digits
        let point = exponent as usize;
        out.push_str(&significand[..point]);
        out.push('.');
        out.push_str(&significand[point..]);
    } else if -6 < exponent && exponent <= 0 {
        // 0. + (−n zeros) + s
        out.push_str("0.");
        for _ in 0..(-exponent) {
            out.push('0');
        }
        out.push_str(&significand);
    } else {
        // d[.ddd]e±(n−1)
        out.push_str(&significand[..1]);
        if count > 1 {
            out.push('.');
            out.push_str(&significand[1..]);
        }
        out.push('e');
        let power = exponent - 1;
        out.push(if power < 0 { '-' } else { '+' });
        out.push_str(&power.abs().to_string());
    }
}

/// Evaluate one benchmark report into an operational SLO report.
/// One #58 compatibility series as a comparison key:
/// `mode|dataset_id|config_version|seed`, with `%` and `|` percent-escaped in
/// the free-text components. The escaping is what makes the key safe to
/// *compare*: without it, dataset `alpha|beta` + config `gamma` and dataset
/// `alpha` + config `beta|gamma` serialize to the same string, and a target
/// calibrated for one workload would silently apply to the other. Pooling
/// and evaluation must share this helper so the two sides can never diverge.
fn compatibility_series_key(
    mode: &str,
    dataset_id: &str,
    config_version: &str,
    seed: u64,
) -> String {
    fn escape(component: &str) -> String {
        component.replace('%', "%25").replace('|', "%7C")
    }
    format!(
        "{}|{}|{}|{}",
        mode,
        escape(dataset_id),
        escape(config_version),
        seed
    )
}

pub fn evaluate(
    report: &BenchmarkReport,
    targets: &SloTargets,
    config: SloEvaluationConfig,
) -> Result<SloReport, SloError> {
    let catalog = catalog();
    // The evaluation path accepts a probe target (that is how a candidate
    // boundary gets measured); the targets-file path does not.
    targets.validate_for_evaluation(&catalog)?;

    // The #58 compatibility series of the artifact under evaluation — the
    // same escaped key `slo-baseline` pools on (see
    // `compatibility_series_key`). A calibrated target names the series it
    // was measured in, and applying it to a different series would silently
    // reuse — or silently loosen — an objective calibrated for another
    // workload.
    let report_series = compatibility_series_key(
        report.mode.as_str(),
        &report.metadata.dataset_id,
        &report.metadata.config_version,
        report.metadata.seed,
    );
    let mut series_mismatches: Vec<(String, String)> = Vec::new();

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

        let target = targets.get(indicator.id).and_then(|target| {
            // A latency probe is the measurement tool for *this* artifact and
            // carries no series by construction; every calibrated target must
            // match the report's series or it is not applied at all. A
            // mismatched target leaves the objective `uncalibrated` — the
            // measured value still publishes — and the report says why in
            // `limitations`, so cross-series reuse fails closed instead of
            // deciding an unrelated workload.
            let probe = target.baseline.source.starts_with(PROBE_SOURCE_MARKER);
            if probe || target.baseline.series.as_deref() == Some(report_series.as_str()) {
                return Some(target);
            }
            series_mismatches.push((
                indicator.id.to_owned(),
                target.baseline.series.clone().unwrap_or_default(),
            ));
            None
        });
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
    let mut limitations = limitations(report, &windows);
    for (id, series) in &series_mismatches {
        limitations.push(format!(
            "target for {id} was calibrated on compatibility series {series}, which is not this \
             report's series ({report_series}); the objective is reported uncalibrated for this \
             run instead of being applied across series"
        ));
    }

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
            source_digest: Some(source_digest_of(report)?),
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

/// The one #58 benchmark suite the aggregate-history importer calibrates.
///
/// The shell maps this suite's metric names (`cached.first_audio.*`), and a
/// history file is only namespaced per suite on disk
/// (`data/<suite>.<series>.jsonl`). The importer receives bare rows, so suite
/// identity has to be re-checked at ingest: without it, another suite's row
/// that happens to share mode/dataset/config/seed would pool as if it
/// measured this suite's workload (review of #225).
pub const AGGREGATE_SUITE: &str = "replay-comparison";

/// The limitation that marks a report built by
/// [`SloReport::from_benchmark_result`]: the percentile-only shell of one #58
/// aggregate row. A full `slo-report` evaluation never carries this text, so
/// it doubles as the shell discriminator when one run arrives as both
/// artifacts and the two have to be told apart.
const AGGREGATE_SHELL_LIMITATION: &str = "aggregate #58 history row: a percentile-only shell";

/// The largest integer binary64 represents exactly. A raw JSON integer at or
/// above it has already been rounded during deserialization — `2^53 + 1`
/// arrives as `2^53` — so integer-valued latency in that range is refused
/// rather than recorded as a number the source never wrote. The bound also
/// sits far below the `f64 -> u64` cast boundary (2^64), which it subsumes
/// (re-review of PR #225).
const EXACT_INTEGER_BOUNDARY: f64 = 9_007_199_254_740_992.0;

impl SloReport {
    /// A percentile-only pooling shell built from one #58 aggregate history
    /// row (`schemas/benchmark-result.schema.json`, the `benchmark-data`
    /// branch's `data/<suite>.<series>.jsonl`).
    ///
    /// The aggregate row retains selected measured percentiles but *not* the
    /// per-event denominators and classifications the operational indicators
    /// are defined on (see "Where the history lives" in
    /// docs/operational-slo.adoc), so the shell deliberately carries no
    /// indicator rows, no windows, and no invariant tallies: it can feed the
    /// target-free latency evidence a probe boundary is chosen from — never a
    /// ratio, and never a citable `baseline`. The recorded invariant counters
    /// are assertions, not measurements, and are dropped for exactly that
    /// reason.
    ///
    /// Identity comes straight from the row (`recording.run_id`, revision,
    /// and the #58 series fields), so [`BaselineProposalSet::from_reports`]
    /// applies its usual rules — one series, distinct runs by identity, the
    /// repeated-run floor, and the full/shell collapse for a run supplied as
    /// both — unchanged to a mixed or aggregate-only history. Only rows of
    /// the [`AGGREGATE_SUITE`] suite convert at all.
    pub fn from_benchmark_result(result: &BenchmarkResult) -> Result<Self, SloError> {
        result
            .validate()
            .map_err(|error| SloError::new(format!("aggregate history row: {error}")))?;
        if result.benchmark_suite.trim() != AGGREGATE_SUITE {
            return Err(SloError::new(format!(
                "aggregate history row benchmark_suite {:?} is not {AGGREGATE_SUITE:?}; only \
                 that suite's rows carry the metric names this shell maps, so another suite's \
                 history (data/<suite>.<series>.jsonl) fails closed instead of pooling as this \
                 suite's calibration evidence",
                result.benchmark_suite
            )));
        }
        let dataset_id = result
            .dataset_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SloError::new(
                    "aggregate history row carries no dataset_id, so it belongs to no #58 \
                     compatibility series and is not calibration evidence"
                        .to_owned(),
                )
            })?
            .to_owned();
        let run_id = result
            .recording
            .as_ref()
            .map(|recording| recording.run_id.trim())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                SloError::new(
                    "aggregate history row carries no recording.run_id, so it is a measurement \
                     but not calibration evidence; append it through the #58 history recorder, \
                     which stamps the run identity (#58 recording.run_id semantics)"
                        .to_owned(),
                )
            })?;
        let mode = ComparisonMode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == result.mode)
            .ok_or_else(|| {
                SloError::new(format!(
                    "aggregate history row mode {:?} carries no operational latency series this \
                     catalog reports on (one of: {}), so it cannot feed SLO baseline selection",
                    result.mode,
                    ComparisonMode::ALL
                        .iter()
                        .map(|mode| mode.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        // Only the first-audio percentiles are complete in the aggregate row
        // (p50/p95/p99 together). The first-visible column records p95 alone,
        // and a percentile the row does not carry is left absent rather than
        // guessed; that ceiling is documented in docs/operational-slo.adoc.
        //
        // Calibration evidence preserves measured numbers; it never normalizes
        // them. The benchmark schema allows any numeric metric value, so a
        // fractional, negative, uncounted, or self-contradictory latency is
        // refused here rather than silently coerced into something that merely
        // looks measured (review of #225).
        const FIRST_AUDIO_PERCENTILES: [&str; 3] = [
            "cached.first_audio.p50_ms",
            "cached.first_audio.p95_ms",
            "cached.first_audio.p99_ms",
        ];
        let mut latency_calibration = Vec::new();
        let present: Vec<(&str, &MetricValue)> = FIRST_AUDIO_PERCENTILES
            .iter()
            .filter_map(|name| result.metrics.get(*name).map(|metric| (*name, metric)))
            .collect();
        match present.len() {
            // No first-audio column at all: no calibration, honestly absent.
            0 => {}
            // Some but not all of the triplet: neither usable evidence nor a
            // measurement this shell may silently drop half of.
            1 | 2 => {
                return Err(SloError::new(format!(
                    "aggregate history row carries only {} of the three cached.first_audio \
                     percentiles ({}); an incomplete triplet is neither evidence this shell can \
                     calibrate from nor a measurement it may silently discard",
                    present.len(),
                    FIRST_AUDIO_PERCENTILES.join(", ")
                )));
            }
            _ => {
                for (name, metric) in &present {
                    if !metric.value.is_finite()
                        || metric.value < 0.0
                        || metric.value.fract() != 0.0
                    {
                        return Err(SloError::new(format!(
                            "aggregate history row metric {name} value {} is not a finite, \
                             non-negative integral millisecond measurement; calibration \
                             preserves measured numbers as they are and refuses to coerce this \
                             one",
                            metric.value
                        )));
                    }
                    // binary64 represents every integer below 2^53 exactly
                    // and nothing at/above it reliably: a raw JSON integer
                    // in this range has already been rounded by
                    // deserialization before validation runs (2^53+1 arrives
                    // as 2^53), so accepting it would record a number the
                    // source never wrote. This also subsumes the f64->u64
                    // cast boundary, which sat at 2^64.
                    if metric.value >= EXACT_INTEGER_BOUNDARY {
                        return Err(SloError::new(format!(
                            "aggregate history row metric {name} value {} is at or above \
                             {EXACT_INTEGER_BOUNDARY} (2^53), where a raw JSON integer can be \
                             rounded during parsing (2^53+1 arrives as 2^53); calibration \
                             preserves the number the source wrote and refuses to record one \
                             it did not",
                            metric.value
                        )));
                    }
                    match metric.sample_count {
                        Some(count) if count > 0 => {}
                        other => {
                            return Err(SloError::new(format!(
                                "aggregate history row metric {name} carries sample_count \
                                 {other:?}, not a positive sample count; a percentile with no \
                                 samples behind it is not calibration evidence"
                            )));
                        }
                    }
                }
                let counts: BTreeSet<u64> = present
                    .iter()
                    .map(|(_, metric)| {
                        metric
                            .sample_count
                            .expect("every sample count was required to be present above")
                    })
                    .collect();
                if counts.len() != 1 {
                    return Err(SloError::new(format!(
                        "aggregate history row first-audio percentiles disagree on sample_count \
                         ({counts:?}); one run's percentiles must describe one sample set"
                    )));
                }
                let value = |name: &str| -> u64 {
                    present
                        .iter()
                        .find(|(candidate, _)| *candidate == name)
                        .expect("the triplet was required above")
                        .1
                        .value as u64
                };
                let (p50_ms, p95_ms, p99_ms) = (
                    value("cached.first_audio.p50_ms"),
                    value("cached.first_audio.p95_ms"),
                    value("cached.first_audio.p99_ms"),
                );
                if !(p50_ms <= p95_ms && p95_ms <= p99_ms) {
                    return Err(SloError::new(format!(
                        "aggregate history row first-audio percentiles are not ordered \
                         (p50={p50_ms} p95={p95_ms} p99={p99_ms}); an unordered triplet is not \
                         one sample set and is repaired by no one here"
                    )));
                }
                latency_calibration.push(LatencyCalibration {
                    indicator: "availability.event_to_first_audio_within_target".to_owned(),
                    window: "session".to_owned(),
                    field: "event_to_first_audio_ms".to_owned(),
                    samples: counts.into_iter().next().expect("one count above"),
                    p50_ms,
                    p95_ms,
                    p99_ms,
                    max_ms: None,
                });
            }
        }

        Ok(Self {
            schema_version: SLO_REPORT_SCHEMA_VERSION.to_owned(),
            catalog_version: SLO_CATALOG_VERSION.to_owned(),
            counts_toward_active_slo: true,
            source: SloSource {
                dataset_id,
                git_commit: result.git.commit.clone(),
                mode,
                config_version: result.configuration.config_version.clone(),
                seed: result.configuration.seed,
                stream_duration_ms: result.configuration.stream_duration_ms,
                source_digest: None,
                provenance: SloProvenance::ReplayFixture,
                plane: TrafficPlane::Active,
                run_id: Some(run_id),
            },
            windows: Vec::new(),
            indicators: Vec::new(),
            latency_calibration,
            error_budgets: Vec::new(),
            failure_attribution: AttributionSummary::default(),
            aar_candidates: Vec::new(),
            uncalibrated: Vec::new(),
            not_yet_measured: Vec::new(),
            limitations: vec![format!(
                "{AGGREGATE_SHELL_LIMITATION} — it carries no per-event denominators, so it \
                 contributes latency calibration evidence only and never a ratio or a citable \
                 baseline"
            )],
            verdict: SloVerdict::Uncalibrated,
        })
    }

    /// Whether this report is the percentile-only shell of a #58 aggregate
    /// row rather than a full `slo-report` evaluation (see
    /// [`AGGREGATE_SHELL_LIMITATION`]).
    fn is_aggregate_shell(&self) -> bool {
        self.limitations
            .iter()
            .any(|line| line.starts_with(AGGREGATE_SHELL_LIMITATION))
    }
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

#[cfg(test)]
mod source_digest_tests {
    use super::*;

    fn canonical(value: &serde_json::Value) -> String {
        let mut out = String::new();
        write_canonical(value, &mut out);
        out
    }

    /// Every expectation is ECMAScript's own rendering (`JSON.stringify`
    /// / `String`), captured from Bun: the recorder's JavaScript
    /// verifier recomputes the digest from the stored document with
    /// exactly these rules, so both sides must render each value
    /// identically or every recorded pair would be refused.
    #[test]
    fn numbers_render_exactly_as_ecmascript_renders_them() {
        let cases: &[(serde_json::Value, &str)] = &[
            (serde_json::Value::from(1_u64), "1"),
            (serde_json::Value::from(1.0_f64), "1"),
            (serde_json::Value::from(0.0_f64), "0"),
            (serde_json::Value::from(-0.0_f64), "0"),
            (serde_json::Value::from(0.5_f64), "0.5"),
            (serde_json::Value::from(0.9_f64), "0.9"),
            (serde_json::Value::from(7_u64), "7"),
            (serde_json::Value::from(100.0_f64), "100"),
            (serde_json::Value::from(123.456_f64), "123.456"),
            (serde_json::Value::from(0.0001_f64), "0.0001"),
            (serde_json::Value::from(1e-5_f64), "0.00001"),
            (serde_json::Value::from(1e-6_f64), "0.000001"),
            (serde_json::Value::from(1e-7_f64), "1e-7"),
            (serde_json::Value::from(2.5e-7_f64), "2.5e-7"),
            (serde_json::Value::from(1e16_f64), "10000000000000000"),
            (serde_json::Value::from(1e20_f64), "100000000000000000000"),
            (serde_json::Value::from(1e21_f64), "1e+21"),
            (serde_json::Value::from(1.5e21_f64), "1.5e+21"),
            (
                serde_json::Value::from(123456789012345680000.0_f64),
                "123456789012345680000",
            ),
            (serde_json::Value::from(-0.5_f64), "-0.5"),
        ];
        for (value, expected) in cases {
            assert_eq!(canonical(value), *expected, "for {value}");
        }
    }

    #[test]
    fn object_keys_sort_recursively_and_array_order_survives() {
        let value = serde_json::json!({
            "b": 1_u64,
            "a": { "d": [3_u64, 1_u64, 2_u64], "c": serde_json::Value::Null },
        });
        assert_eq!(canonical(&value), r#"{"a":{"c":null,"d":[3,1,2]},"b":1}"#);
    }

    /// Shared with `tests/slo-record.test.mjs`: both sides digest this
    /// exact document — the Rust side while evaluating, the JavaScript
    /// side from the stored file — so a digest mismatch can only mean
    /// the canonical forms diverged, never the pairing logic.
    #[test]
    fn the_digest_matches_the_javascript_verifier_on_one_document() {
        let value = serde_json::json!({
            "a": 1_u64,
            "b": serde_json::Value::from(1.0_f64),
            "c": serde_json::Value::from(1e-7_f64),
            "d": [serde_json::Value::from(0.5_f64), serde_json::Value::from(-0.0_f64)],
            "e": "x",
            "f": serde_json::Value::Null,
            "g": true,
            "h": serde_json::Value::from(1e21_f64),
        });
        assert_eq!(
            canonical_json_digest(&value),
            "67ab96b3cba323e2e72493aed8212bce68f7c432660d27492f338ab4cc477802"
        );
    }
}
