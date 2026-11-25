use super::*;

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

pub(super) fn classify_never(
    _event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    EventVerdict::Outside
}

pub(super) fn classify_first_visible_latency(
    event: &EventObservation,
    target: Option<&SloTarget>,
) -> EventVerdict {
    classify_latency(event.event_to_first_visible_reaction_ms, target)
}

pub(super) fn classify_first_audio_latency(
    event: &EventObservation,
    target: Option<&SloTarget>,
) -> EventVerdict {
    classify_latency(event.event_to_first_audio_ms, target)
}

/// An event with no measured latency is outside the denominator, not a miss.
/// With a sample but no calibrated threshold there is no defined miss either,
/// so the event is eligible and unscored: it counts as the evidence a
/// threshold is calibrated from, and never as a failure.
pub(super) fn classify_latency(latency: Option<u64>, target: Option<&SloTarget>) -> EventVerdict {
    let Some(latency) = latency else {
        return EventVerdict::Outside;
    };
    match target.and_then(|target| target.threshold_ms) {
        Some(threshold) if latency <= threshold => EventVerdict::Conforms,
        Some(_) => EventVerdict::Misses,
        None => EventVerdict::Unscored,
    }
}

pub(super) fn classify_speech_presence(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    if event.cancelled {
        return EventVerdict::Outside;
    }
    if unintended_silence(event) {
        EventVerdict::Misses
    } else {
        EventVerdict::Conforms
    }
}

pub(super) fn classify_fallback_delivery(
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

pub(super) fn classify_deadline_adherence(
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

pub(super) fn classify_generative_budget_admission(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    if event.budget_denial_reason.is_some() {
        EventVerdict::Misses
    } else {
        EventVerdict::Conforms
    }
}

pub(super) fn classify_semantic_reuse_correctness(
    event: &EventObservation,
    _target: Option<&SloTarget>,
) -> EventVerdict {
    match event.wrong_reuse {
        None => EventVerdict::Outside,
        Some(false) => EventVerdict::Conforms,
        Some(true) => EventVerdict::Misses,
    }
}

pub(super) fn unintended_silence(event: &EventObservation) -> bool {
    event.route == RouteClass::Silent
        && !event.operator_override
        && event.deadline_exhaustion_stage.is_none()
        && event.budget_denial_reason.is_none()
}
