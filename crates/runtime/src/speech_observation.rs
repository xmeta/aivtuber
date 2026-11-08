//! Speech observation ingress and bounded per-speaker state (issue #92, S1).
//!
//! Microphone speech is the least trusted thing the runtime hears. Even the
//! operator's own microphone is content-plane `untrusted` input: it can never
//! mint authority, and its priority comes from configuration, not from the
//! observation (ADR-0005 S-7). So the *wire form carries no trust, plane,
//! source class or priority*; this module assigns them
//! (`SpeechObservationProvenance`) and applies the operator's
//! [`IngressPolicy`] ceiling before anything is retained.
//!
//! This is an observation layer, not a floor policy: it keeps raw activity
//! edges (no debounce), rejects stale or expired observations by reason, and
//! exposes per-speaker state plus the deterministic silence fallback that #81
//! consumes. It owns no scheduler, output or model access.

use crate::{IngressPolicy, RuntimeError};
use aivtuber_domain::{
    DomainValidationError, FallbackTurnConfig, FallbackTurnSignal, MAX_LANGUAGE_LEN,
    MAX_SPEECH_OBSERVATION_BATCH_BYTES, SecurityPlane, SourceClass, SpeakerActivity,
    SpeakerActivityState, SpeechEdge, SpeechObservation, SpeechObservationBatch, TrustLevel,
    TurnEndFallbackReason, TurnEndScoreObservation, fallback_turn_signal,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Hard ceiling on configured turn-end languages (bounded config).
pub const MAX_TURN_END_LANGUAGES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechObservationConfig {
    /// Activity edges older than this (`now - observed_at`) are rejected.
    /// Default 160 ms: the age limit ADR-0005 measured to bound false
    /// turn-takes under a stalled feed. A measured synthetic value; revisit on
    /// real dialogue.
    pub max_age_ms: u64,
    /// Same limit for turn-end scores, which lag the audio by the model's
    /// inference time (about 125 ms p99 plus a 100 ms polling cadence in
    /// EXP-0001 section 7). Unmeasured default; must allow that lag.
    pub turn_end_max_age_ms: u64,
    /// Tolerated `observed_at - now` before an observation counts as future
    /// dated (clock mapping error between adapter and runtime).
    pub max_future_skew_ms: u64,
    /// Hard cap on tracked speakers. A new speaker beyond it is rejected, never
    /// evicting a tracked one, so a flood of fresh ids cannot displace the
    /// real speaker.
    pub max_speakers: usize,
    /// A speaker with no accepted observation for this long is evicted.
    pub speaker_idle_ttl_ms: u64,
    pub fallback: FallbackTurnConfig,
    /// Languages (as declared on batches) for which a turn-end provider is
    /// configured. Empty means no turn-end provider: always the fallback.
    pub turn_end_languages: Vec<String>,
}

impl Default for SpeechObservationConfig {
    fn default() -> Self {
        Self {
            max_age_ms: 160,
            turn_end_max_age_ms: 400,
            max_future_skew_ms: 50,
            max_speakers: 64,
            speaker_idle_ttl_ms: 5 * 60 * 1_000,
            fallback: FallbackTurnConfig::default(),
            turn_end_languages: Vec::new(),
        }
    }
}

impl SpeechObservationConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let bad = RuntimeError::InvalidConfiguration;
        if !(1..=60_000).contains(&self.max_age_ms) {
            return Err(bad("speech max_age_ms must be in 1..=60000"));
        }
        if !(1..=60_000).contains(&self.turn_end_max_age_ms) {
            return Err(bad("speech turn_end_max_age_ms must be in 1..=60000"));
        }
        if self.max_future_skew_ms > 10_000 {
            return Err(bad("speech max_future_skew_ms must be at most 10000"));
        }
        if !(1..=1_024).contains(&self.max_speakers) {
            return Err(bad("speech max_speakers must be in 1..=1024"));
        }
        self.fallback
            .validate()
            .map_err(|_| bad("speech fallback configuration is out of range"))?;
        // An idle TTL shorter than the stale-activity bound would evict a
        // speaker before the heuristic could report the stale state.
        if self.speaker_idle_ttl_ms <= self.fallback.max_active_ms {
            return Err(bad(
                "speech speaker_idle_ttl_ms must exceed the fallback max_active_ms",
            ));
        }
        if self.turn_end_languages.len() > MAX_TURN_END_LANGUAGES
            || self
                .turn_end_languages
                .iter()
                .any(|l| l.is_empty() || l.len() > MAX_LANGUAGE_LEN)
        {
            return Err(bad("speech turn_end_languages is out of bounds"));
        }
        Ok(())
    }
}

/// Why one observation was not accepted. Each is counted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechRejectReason {
    /// Older than the configured max age.
    StaleAge,
    /// Past its own `expires_at_ms`.
    Expired,
    /// Dated beyond the tolerated skew into the future.
    FutureDated,
    /// Older than an already accepted observation of the same kind for the
    /// speaker (ordering is by observed time, never arrival).
    OutOfOrder,
    /// New speaker while the tracker is at `max_speakers`.
    SpeakerCapacity,
}

impl SpeechRejectReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::StaleAge => "stale_age",
            Self::Expired => "expired",
            Self::FutureDated => "future_dated",
            Self::OutOfOrder => "out_of_order",
            Self::SpeakerCapacity => "speaker_capacity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechObservationOutcome {
    Accepted { age_ms: u64 },
    Rejected(SpeechRejectReason),
}

impl SpeechObservationOutcome {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Accepted { .. } => "accepted",
            Self::Rejected(reason) => reason.name(),
        }
    }
}

/// Why a whole batch was refused before any observation was considered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechBatchRejection {
    Oversize,
    Invalid(DomainValidationError),
    /// The operator's ingress ceiling does not admit speech.
    IngressPolicy,
}

/// Classification the *runtime* assigned to a batch. It never comes from the
/// payload, and it is constant: microphone speech is untrusted content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechObservationProvenance {
    pub source_class: SourceClass,
    pub plane: SecurityPlane,
    pub trust_level: TrustLevel,
    /// Audit data copied from the batch; never read for any decision.
    pub provider_id: String,
    pub provider_version: String,
    pub language: String,
}

impl SpeechObservationProvenance {
    fn assign(batch: &SpeechObservationBatch) -> Self {
        Self {
            source_class: SourceClass::Speech,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            provider_id: batch.provider_id.clone(),
            provider_version: batch.provider_version.clone(),
            language: batch.language.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechBatchReport {
    pub provenance: SpeechObservationProvenance,
    /// One outcome per observation, in batch order.
    pub outcomes: Vec<SpeechObservationOutcome>,
}

/// Counters for the #92 metrics: observation age, rejects by reason, the
/// speaker-state high-water mark, fallback signals and provider availability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SpeechObservationMetrics {
    pub observations_accepted: u64,
    pub rejected_stale_age: u64,
    pub rejected_expired: u64,
    pub rejected_future_dated: u64,
    pub rejected_out_of_order: u64,
    pub rejected_speaker_capacity: u64,
    pub batches_rejected_oversize: u64,
    pub batches_rejected_invalid: u64,
    pub batches_rejected_ingress_policy: u64,
    /// Observation age (`now - observed_at`) over every observation that
    /// reached the age check, accepted or stale.
    pub age_samples: u64,
    pub age_sum_ms: u64,
    pub age_max_ms: u64,
    pub age_last_ms: u64,
    pub speakers: usize,
    pub speakers_high_water: usize,
    pub speakers_evicted: u64,
    /// Silence episodes that became turn-end eligible while the deterministic
    /// fallback applied. Counted once per episode, not per poll.
    pub fallback_signals: u64,
    pub fallback_no_turn_end_provider: u64,
    pub fallback_unsupported_language: u64,
    pub fallback_turn_end_stale: u64,
    /// Reported by adapters through `record_provider_unavailable`.
    pub provider_unavailable: u64,
}

impl SpeechObservationMetrics {
    pub fn rejected_total(&self) -> u64 {
        self.rejected_stale_age
            + self.rejected_expired
            + self.rejected_future_dated
            + self.rejected_out_of_order
            + self.rejected_speaker_capacity
    }
}

/// What #81 may read about one speaker. No floor decision is made here.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerView {
    pub activity: Option<SpeakerActivity>,
    pub last_edge_ms: Option<u64>,
    pub language: String,
    /// Fresh turn-end score from a configured provider, when one applies.
    /// An uncalibrated score is not a probability (see `calibrated`).
    pub turn_end: Option<TurnEndScoreObservation>,
    pub fallback: FallbackTurnSignal,
    /// `Some` when the deterministic fallback, not a provider score, is the
    /// applicable turn-end signal.
    pub fallback_reason: Option<TurnEndFallbackReason>,
}

#[derive(Debug)]
struct SpeakerEntry {
    state: Option<SpeakerActivityState>,
    /// Newest accepted edge observation time; guards edge ordering.
    latest_edge_observed_ms: Option<u64>,
    turn_end: Option<TurnEndScoreObservation>,
    language: String,
    last_seen_ms: u64,
    /// `last_edge_ms` of the silence episode already counted as a fallback
    /// signal, so polling faster does not inflate the metric.
    fallback_counted_for: Option<u64>,
}

#[derive(Debug)]
pub struct SpeechObservationTracker {
    config: SpeechObservationConfig,
    ingress_policy: IngressPolicy,
    speakers: BTreeMap<String, SpeakerEntry>,
    metrics: SpeechObservationMetrics,
}

impl SpeechObservationTracker {
    pub fn new(
        config: SpeechObservationConfig,
        ingress_policy: IngressPolicy,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        Ok(Self {
            config,
            ingress_policy,
            speakers: BTreeMap::new(),
            metrics: SpeechObservationMetrics::default(),
        })
    }

    pub fn config(&self) -> &SpeechObservationConfig {
        &self.config
    }

    pub fn metrics(&self) -> SpeechObservationMetrics {
        SpeechObservationMetrics {
            speakers: self.speakers.len(),
            ..self.metrics
        }
    }

    /// Adapter-reported: the VAD or turn-end provider is unavailable. Counted
    /// only; the fallback keeps working from whatever edges still arrive.
    pub fn record_provider_unavailable(&mut self) {
        self.metrics.provider_unavailable = self.metrics.provider_unavailable.saturating_add(1);
    }

    /// Bounded, validating entry for serialized batches. `now_ms` is the
    /// runtime's monotonic (or logical, in replay) time.
    pub fn ingest_bytes(
        &mut self,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<SpeechBatchReport, SpeechBatchRejection> {
        if bytes.len() > MAX_SPEECH_OBSERVATION_BATCH_BYTES {
            self.metrics.batches_rejected_oversize += 1;
            return Err(SpeechBatchRejection::Oversize);
        }
        // The class check does not depend on the payload, so run it first and
        // never parse bytes from an ingress the operator has not admitted.
        self.admit_ingress()?;
        match SpeechObservationBatch::from_json_bytes(bytes) {
            Ok(batch) => Ok(self.apply_batch(&batch, now_ms)),
            Err(error) => {
                self.metrics.batches_rejected_invalid += 1;
                Err(SpeechBatchRejection::Invalid(error))
            }
        }
    }

    /// Entry for an already deserialized batch (replay fixtures, adapters in
    /// the same process). Re-validates, so a hand-built batch cannot bypass the
    /// bounds.
    pub fn ingest_batch(
        &mut self,
        batch: &SpeechObservationBatch,
        now_ms: u64,
    ) -> Result<SpeechBatchReport, SpeechBatchRejection> {
        self.admit_ingress()?;
        if let Err(error) = batch.validate() {
            self.metrics.batches_rejected_invalid += 1;
            return Err(SpeechBatchRejection::Invalid(error));
        }
        Ok(self.apply_batch(batch, now_ms))
    }

    fn admit_ingress(&mut self) -> Result<(), SpeechBatchRejection> {
        if self
            .ingress_policy
            .admits_declared(SourceClass::Speech, TrustLevel::Untrusted)
        {
            Ok(())
        } else {
            self.metrics.batches_rejected_ingress_policy += 1;
            Err(SpeechBatchRejection::IngressPolicy)
        }
    }

    fn apply_batch(&mut self, batch: &SpeechObservationBatch, now_ms: u64) -> SpeechBatchReport {
        self.sweep(now_ms);
        let outcomes = batch
            .observations
            .iter()
            .map(|observation| self.apply_observation(observation, &batch.language, now_ms))
            .collect();
        SpeechBatchReport {
            provenance: SpeechObservationProvenance::assign(batch),
            outcomes,
        }
    }

    fn apply_observation(
        &mut self,
        observation: &SpeechObservation,
        language: &str,
        now_ms: u64,
    ) -> SpeechObservationOutcome {
        let observed = observation.observed_at_ms();
        let age_ms = if observed > now_ms {
            if observed - now_ms > self.config.max_future_skew_ms {
                return self.reject(SpeechRejectReason::FutureDated);
            }
            0
        } else {
            now_ms - observed
        };
        self.metrics.age_samples += 1;
        self.metrics.age_sum_ms = self.metrics.age_sum_ms.saturating_add(age_ms);
        self.metrics.age_max_ms = self.metrics.age_max_ms.max(age_ms);
        self.metrics.age_last_ms = age_ms;

        if observation
            .expires_at_ms()
            .is_some_and(|expires| now_ms > expires)
        {
            return self.reject(SpeechRejectReason::Expired);
        }
        let max_age = match observation {
            SpeechObservation::SpeechActivity(_) => self.config.max_age_ms,
            SpeechObservation::TurnEndScore(_) => self.config.turn_end_max_age_ms,
        };
        if age_ms > max_age {
            return self.reject(SpeechRejectReason::StaleAge);
        }

        let speaker_id = observation.speaker_id();
        if let Some(entry) = self.speakers.get(speaker_id) {
            let latest = match observation {
                SpeechObservation::SpeechActivity(_) => entry.latest_edge_observed_ms,
                SpeechObservation::TurnEndScore(_) => {
                    entry.turn_end.as_ref().map(|t| t.observed_at_ms)
                }
            };
            if latest.is_some_and(|latest| observed < latest) {
                return self.reject(SpeechRejectReason::OutOfOrder);
            }
        } else if self.speakers.len() >= self.config.max_speakers {
            return self.reject(SpeechRejectReason::SpeakerCapacity);
        }

        let entry = self
            .speakers
            .entry(speaker_id.to_owned())
            .or_insert_with(|| SpeakerEntry {
                state: None,
                latest_edge_observed_ms: None,
                turn_end: None,
                language: String::new(),
                last_seen_ms: now_ms,
                fallback_counted_for: None,
            });
        entry.last_seen_ms = now_ms;
        if entry.language != language {
            language.clone_into(&mut entry.language);
        }
        match observation {
            SpeechObservation::SpeechActivity(activity) => {
                entry.latest_edge_observed_ms = Some(activity.observed_at_ms);
                apply_edge(entry, activity.edge, activity.observed_at_ms);
            }
            SpeechObservation::TurnEndScore(score) => entry.turn_end = Some(score.clone()),
        }
        self.metrics.observations_accepted += 1;
        self.metrics.speakers_high_water =
            self.metrics.speakers_high_water.max(self.speakers.len());
        SpeechObservationOutcome::Accepted { age_ms }
    }

    fn reject(&mut self, reason: SpeechRejectReason) -> SpeechObservationOutcome {
        let counter = match reason {
            SpeechRejectReason::StaleAge => &mut self.metrics.rejected_stale_age,
            SpeechRejectReason::Expired => &mut self.metrics.rejected_expired,
            SpeechRejectReason::FutureDated => &mut self.metrics.rejected_future_dated,
            SpeechRejectReason::OutOfOrder => &mut self.metrics.rejected_out_of_order,
            SpeechRejectReason::SpeakerCapacity => &mut self.metrics.rejected_speaker_capacity,
        };
        *counter += 1;
        SpeechObservationOutcome::Rejected(reason)
    }

    /// Evict speakers with no accepted observation for `speaker_idle_ttl_ms`.
    pub fn sweep(&mut self, now_ms: u64) {
        let ttl = self.config.speaker_idle_ttl_ms;
        let before = self.speakers.len();
        self.speakers
            .retain(|_, entry| now_ms.saturating_sub(entry.last_seen_ms) < ttl);
        self.metrics.speakers_evicted += (before - self.speakers.len()) as u64;
    }

    /// Read-only view; counts nothing.
    pub fn view(&self, speaker_id: &str, now_ms: u64) -> Option<SpeakerView> {
        self.speakers
            .get(speaker_id)
            .map(|entry| self.view_of(entry, now_ms))
    }

    /// Like [`view`](Self::view), and counts a fallback signal once per
    /// silence episode when the fallback applies and the speaker is turn-end
    /// eligible.
    pub fn poll(&mut self, speaker_id: &str, now_ms: u64) -> Option<SpeakerView> {
        let view = self.view(speaker_id, now_ms)?;
        if let (FallbackTurnSignal::TurnEndEligible { .. }, Some(reason)) =
            (view.fallback, view.fallback_reason)
            && let Some(entry) = self.speakers.get_mut(speaker_id)
            && entry.fallback_counted_for != view.last_edge_ms
        {
            entry.fallback_counted_for = view.last_edge_ms;
            self.metrics.fallback_signals += 1;
            match reason {
                TurnEndFallbackReason::NoTurnEndProvider => {
                    self.metrics.fallback_no_turn_end_provider += 1;
                }
                TurnEndFallbackReason::UnsupportedLanguage => {
                    self.metrics.fallback_unsupported_language += 1;
                }
                TurnEndFallbackReason::TurnEndStale => self.metrics.fallback_turn_end_stale += 1,
            }
        }
        Some(view)
    }

    fn view_of(&self, entry: &SpeakerEntry, now_ms: u64) -> SpeakerView {
        let supported = self
            .config
            .turn_end_languages
            .iter()
            .any(|language| *language == entry.language);
        let fresh = entry.turn_end.as_ref().filter(|score| {
            now_ms.saturating_sub(score.observed_at_ms) <= self.config.turn_end_max_age_ms
                && entry
                    .state
                    .is_none_or(|state| score.observed_at_ms >= state.last_edge_ms)
        });
        let fallback_reason = if !supported {
            Some(TurnEndFallbackReason::UnsupportedLanguage)
        } else if entry.turn_end.is_none() {
            Some(TurnEndFallbackReason::NoTurnEndProvider)
        } else if fresh.is_none() {
            Some(TurnEndFallbackReason::TurnEndStale)
        } else {
            None
        };
        SpeakerView {
            activity: entry.state.map(|s| s.activity),
            last_edge_ms: entry.state.map(|s| s.last_edge_ms),
            language: entry.language.clone(),
            turn_end: if supported { fresh.cloned() } else { None },
            fallback: fallback_turn_signal(entry.state.as_ref(), now_ms, &self.config.fallback),
            fallback_reason,
        }
    }
}

/// Raw edges, no debounce. A repeated `start` refreshes the active edge time
/// (the feed is alive); a repeated `end` keeps the first, so duplicates cannot
/// restart the silence clock.
fn apply_edge(entry: &mut SpeakerEntry, edge: SpeechEdge, at_ms: u64) {
    match (edge, entry.state.map(|s| s.activity)) {
        (SpeechEdge::Start, _) => {
            entry.state = Some(SpeakerActivityState {
                activity: SpeakerActivity::Active,
                last_edge_ms: at_ms,
            });
        }
        (SpeechEdge::End, Some(SpeakerActivity::Inactive)) => {}
        (SpeechEdge::End, _) => {
            entry.state = Some(SpeakerActivityState {
                activity: SpeakerActivity::Inactive,
                last_edge_ms: at_ms,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{SpeechActivityObservation, SpeechEdge};

    fn edge(speaker: &str, edge: SpeechEdge, at: u64) -> SpeechObservation {
        SpeechObservation::SpeechActivity(SpeechActivityObservation {
            speaker_id: speaker.to_owned(),
            edge,
            observed_at_ms: at,
            expires_at_ms: None,
            confidence: None,
        })
    }

    fn score(speaker: &str, at: u64) -> SpeechObservation {
        SpeechObservation::TurnEndScore(TurnEndScoreObservation {
            speaker_id: speaker.to_owned(),
            observed_at_ms: at,
            expires_at_ms: None,
            provider: "smart-turn".to_owned(),
            model_id: "smart-turn-v3.2-cpu".to_owned(),
            calibrated: false,
            score: 0.9,
            prediction_horizon_ms: None,
        })
    }

    fn batch(language: &str, observations: Vec<SpeechObservation>) -> SpeechObservationBatch {
        SpeechObservationBatch {
            schema_version: "0.1.0".to_owned(),
            provider_id: "silero-vad".to_owned(),
            provider_version: "6.2.3".to_owned(),
            language: language.to_owned(),
            observations,
        }
    }

    fn tracker(config: SpeechObservationConfig) -> SpeechObservationTracker {
        SpeechObservationTracker::new(config, IngressPolicy::default()).expect("config")
    }

    fn outcome_names(report: &SpeechBatchReport) -> Vec<&'static str> {
        report.outcomes.iter().map(|o| o.name()).collect()
    }

    #[test]
    fn microphone_observations_are_untrusted_content_whatever_the_policy() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        let report = tracker
            .ingest_batch(&batch("ja", vec![edge("op", SpeechEdge::Start, 100)]), 100)
            .expect("admitted");
        assert_eq!(report.provenance.source_class, SourceClass::Speech);
        assert_eq!(report.provenance.plane, SecurityPlane::Content);
        assert_eq!(report.provenance.trust_level, TrustLevel::Untrusted);
    }

    #[test]
    fn ingress_ceiling_without_speech_rejects_before_parsing_or_state() {
        let policy = IngressPolicy::new(TrustLevel::Untrusted, &[SourceClass::PublicChat]);
        let mut tracker =
            SpeechObservationTracker::new(SpeechObservationConfig::default(), policy).unwrap();
        let bytes = br#"{"schema_version":"0.1.0","provider_id":"v","provider_version":"1","language":"ja","observations":[{"kind":"speech_activity","speaker_id":"s","edge":"start","observed_at_ms":1}]}"#;
        assert_eq!(
            tracker.ingest_bytes(bytes, 1),
            Err(SpeechBatchRejection::IngressPolicy)
        );
        // Garbage is also an ingress rejection, not a parse of untrusted bytes.
        assert_eq!(
            tracker.ingest_bytes(b"not json", 1),
            Err(SpeechBatchRejection::IngressPolicy)
        );
        let metrics = tracker.metrics();
        assert_eq!(metrics.batches_rejected_ingress_policy, 2);
        assert_eq!((metrics.speakers, metrics.observations_accepted), (0, 0));
    }

    #[test]
    fn oversize_and_invalid_batches_are_counted_and_retain_nothing() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        let huge = vec![b' '; MAX_SPEECH_OBSERVATION_BATCH_BYTES + 1];
        assert_eq!(
            tracker.ingest_bytes(&huge, 0),
            Err(SpeechBatchRejection::Oversize)
        );
        assert!(matches!(
            tracker.ingest_bytes(br#"{"trust_level":"trusted"}"#, 0),
            Err(SpeechBatchRejection::Invalid(_))
        ));
        let metrics = tracker.metrics();
        assert_eq!(metrics.batches_rejected_oversize, 1);
        assert_eq!(metrics.batches_rejected_invalid, 1);
        assert_eq!(metrics.speakers, 0);
    }

    #[test]
    fn stale_and_expired_observations_are_rejected_by_reason_without_blocking() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        let mut expiring = edge("a", SpeechEdge::Start, 1_000);
        if let SpeechObservation::SpeechActivity(a) = &mut expiring {
            a.expires_at_ms = Some(1_050);
        }
        let report = tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 1_000), // age 200 > 160: stale
                        edge("a", SpeechEdge::Start, 1_040), // age 160: boundary, accepted
                        expiring,                            // expired at 1_050 < now 1_200
                        edge("a", SpeechEdge::End, 1_100),   // 100: accepted
                        edge("a", SpeechEdge::Start, 1_400), // 200 ahead of now: future
                    ],
                ),
                1_200,
            )
            .expect("admitted");
        assert_eq!(
            outcome_names(&report),
            [
                "stale_age",
                "accepted",
                "expired",
                "accepted",
                "future_dated"
            ]
        );
        let metrics = tracker.metrics();
        assert_eq!(
            (
                metrics.rejected_stale_age,
                metrics.rejected_expired,
                metrics.rejected_future_dated
            ),
            (1, 1, 1)
        );
        // The rejects blocked nothing: the good edges were applied.
        let view = tracker.view("a", 1_200).unwrap();
        assert_eq!(view.activity, Some(SpeakerActivity::Inactive));
        assert_eq!(view.last_edge_ms, Some(1_100));
        // Age is recorded for stale observations too: max reflects the stall.
        assert_eq!(metrics.age_max_ms, 200);
        assert_eq!(metrics.age_samples, 4);
    }

    #[test]
    fn an_explicit_expiry_can_only_tighten_the_max_age() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        let mut late = edge("a", SpeechEdge::Start, 1_000);
        if let SpeechObservation::SpeechActivity(a) = &mut late {
            a.expires_at_ms = Some(60_000); // far future: does not extend 160 ms
        }
        let report = tracker
            .ingest_batch(&batch("ja", vec![late]), 1_500)
            .unwrap();
        assert_eq!(outcome_names(&report), ["stale_age"]);
    }

    #[test]
    fn ordering_uses_observed_time_not_arrival() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        let report = tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 1_100),
                        edge("a", SpeechEdge::End, 1_050), // arrives later, observed earlier
                        edge("a", SpeechEdge::End, 1_100), // equal time is not out of order
                    ],
                ),
                1_150,
            )
            .unwrap();
        assert_eq!(
            outcome_names(&report),
            ["accepted", "out_of_order", "accepted"]
        );
        assert_eq!(tracker.metrics().rejected_out_of_order, 1);
    }

    #[test]
    fn raw_edges_have_no_debounce_so_a_short_fragment_resets_the_silence_clock() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 100),
                        edge("a", SpeechEdge::End, 200),
                        edge("a", SpeechEdge::Start, 900), // 32 ms blip
                        edge("a", SpeechEdge::End, 932),
                    ],
                ),
                940,
            )
            .unwrap();
        // 800 ms after the *first* end would already be eligible; the blip
        // restarts the silence clock from 932.
        assert_eq!(
            tracker.view("a", 1_000).unwrap().fallback,
            FallbackTurnSignal::Waiting { silence_ms: 68 }
        );
    }

    #[test]
    fn duplicate_ends_do_not_restart_silence_and_repeated_starts_keep_it_alive() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 100),
                        edge("a", SpeechEdge::End, 150),
                        edge("a", SpeechEdge::End, 190),
                    ],
                ),
                200,
            )
            .unwrap();
        assert_eq!(tracker.view("a", 950).unwrap().last_edge_ms, Some(150));
    }

    #[test]
    fn speaker_state_is_capped_and_idle_speakers_are_evicted() {
        let config = SpeechObservationConfig {
            max_speakers: 2,
            speaker_idle_ttl_ms: 70_000,
            ..SpeechObservationConfig::default()
        };
        let mut tracker = tracker(config);
        let report = tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 100),
                        edge("b", SpeechEdge::Start, 100),
                        edge("c", SpeechEdge::Start, 100), // over the cap
                    ],
                ),
                100,
            )
            .unwrap();
        assert_eq!(
            outcome_names(&report),
            ["accepted", "accepted", "speaker_capacity"]
        );
        // Existing speakers were not displaced by the newcomer.
        assert!(tracker.view("a", 100).is_some() && tracker.view("b", 100).is_some());
        assert_eq!(tracker.metrics().speakers_high_water, 2);

        // `a` stays alive, `b` idles out; then `c` fits.
        tracker
            .ingest_batch(
                &batch("ja", vec![edge("a", SpeechEdge::End, 60_000)]),
                60_000,
            )
            .unwrap();
        let report = tracker
            .ingest_batch(
                &batch("ja", vec![edge("c", SpeechEdge::Start, 70_100)]),
                70_100,
            )
            .unwrap();
        assert_eq!(outcome_names(&report), ["accepted"]);
        let metrics = tracker.metrics();
        assert!(tracker.view("b", 70_100).is_none());
        assert_eq!((metrics.speakers_evicted, metrics.speakers), (1, 2));
        assert_eq!(metrics.speakers_high_water, 2);
    }

    #[test]
    fn fallback_reason_distinguishes_no_provider_unsupported_language_and_stale_score() {
        let config = SpeechObservationConfig {
            turn_end_languages: vec!["en".to_owned()],
            ..SpeechObservationConfig::default()
        };
        let mut tracker = tracker(config);
        // Japanese is not a configured turn-end language.
        tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("ja", SpeechEdge::Start, 0),
                        edge("ja", SpeechEdge::End, 100),
                    ],
                ),
                100,
            )
            .unwrap();
        // English is, but no score ever arrives.
        tracker
            .ingest_batch(
                &batch(
                    "en",
                    vec![
                        edge("en", SpeechEdge::Start, 0),
                        edge("en", SpeechEdge::End, 100),
                    ],
                ),
                100,
            )
            .unwrap();
        // English with a score: provider applies while fresh, stale later.
        tracker
            .ingest_batch(
                &batch(
                    "en",
                    vec![
                        edge("en2", SpeechEdge::Start, 0),
                        edge("en2", SpeechEdge::End, 100),
                        score("en2", 150),
                    ],
                ),
                150,
            )
            .unwrap();
        assert_eq!(
            tracker.view("ja", 1_000).unwrap().fallback_reason,
            Some(TurnEndFallbackReason::UnsupportedLanguage)
        );
        assert_eq!(
            tracker.view("en", 1_000).unwrap().fallback_reason,
            Some(TurnEndFallbackReason::NoTurnEndProvider)
        );
        let fresh = tracker.view("en2", 400).unwrap();
        assert_eq!(fresh.fallback_reason, None);
        assert!(fresh.turn_end.is_some() && !fresh.turn_end.unwrap().calibrated);
        assert_eq!(
            tracker.view("en2", 1_000).unwrap().fallback_reason,
            Some(TurnEndFallbackReason::TurnEndStale)
        );
        // A score from before the latest silence began belongs to earlier speech.
        tracker
            .ingest_batch(
                &batch(
                    "en",
                    vec![
                        edge("en2", SpeechEdge::Start, 1_000),
                        edge("en2", SpeechEdge::End, 1_050),
                    ],
                ),
                1_050,
            )
            .unwrap();
        assert_eq!(
            tracker.view("en2", 1_100).unwrap().fallback_reason,
            Some(TurnEndFallbackReason::TurnEndStale)
        );
    }

    #[test]
    fn fallback_signal_is_counted_once_per_silence_episode_not_per_poll() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 0),
                        edge("a", SpeechEdge::End, 100),
                    ],
                ),
                100,
            )
            .unwrap();
        for now in [200, 899] {
            tracker.poll("a", now);
        }
        assert_eq!(
            tracker.metrics().fallback_signals,
            0,
            "not eligible before 900"
        );
        for now in [900, 901, 2_000] {
            assert_eq!(
                tracker.poll("a", now).unwrap().fallback.name(),
                "turn_end_eligible"
            );
        }
        let m = tracker.metrics();
        assert_eq!(
            (m.fallback_signals, m.fallback_unsupported_language),
            (1, 1)
        );
        // A new utterance and a new silence is a new episode.
        tracker
            .ingest_batch(
                &batch(
                    "ja",
                    vec![
                        edge("a", SpeechEdge::Start, 3_000),
                        edge("a", SpeechEdge::End, 3_100),
                    ],
                ),
                3_100,
            )
            .unwrap();
        tracker.poll("a", 4_000);
        assert_eq!(tracker.metrics().fallback_signals, 2);
    }

    #[test]
    fn a_lost_end_edge_is_reported_stale_instead_of_pinning_the_speaker_active() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        tracker
            .ingest_batch(&batch("ja", vec![edge("a", SpeechEdge::Start, 0)]), 0)
            .unwrap();
        assert_eq!(
            tracker.view("a", 60_000).unwrap().fallback.name(),
            "speaker_active"
        );
        assert_eq!(
            tracker.view("a", 60_001).unwrap().fallback.name(),
            "activity_stale"
        );
    }

    #[test]
    fn provider_unavailable_is_counted() {
        let mut tracker = tracker(SpeechObservationConfig::default());
        tracker.record_provider_unavailable();
        tracker.record_provider_unavailable();
        assert_eq!(tracker.metrics().provider_unavailable, 2);
    }

    #[test]
    fn invalid_configuration_is_refused() {
        for config in [
            SpeechObservationConfig {
                max_age_ms: 0,
                ..Default::default()
            },
            SpeechObservationConfig {
                max_speakers: 0,
                ..Default::default()
            },
            SpeechObservationConfig {
                speaker_idle_ttl_ms: 1_000,
                ..Default::default()
            },
            SpeechObservationConfig {
                turn_end_languages: vec!["ja".into(); MAX_TURN_END_LANGUAGES + 1],
                ..Default::default()
            },
        ] {
            assert!(SpeechObservationTracker::new(config, IngressPolicy::default()).is_err());
        }
    }
}
