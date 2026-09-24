#![forbid(unsafe_code)]

//! Telemetry, calibration, and replay-benchmark vocabulary.
//!
//! Runtime instrumentation records bounded structured observations. Benchmark
//! reports aggregate those observations without retaining prompt/generated text.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

/// Initial metric groups required by the architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Latency,
    Routing,
    Cache,
    Generation,
    Fallback,
    Security,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonMode {
    DeterministicOnly,
    DeterministicSemantic,
    DeterministicSemanticJev,
    FullGenerative,
}

impl ComparisonMode {
    pub const ALL: [Self; 4] = [
        Self::DeterministicOnly,
        Self::DeterministicSemantic,
        Self::DeterministicSemanticJev,
        Self::FullGenerative,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeterministicOnly => "deterministic_only",
            Self::DeterministicSemantic => "deterministic_semantic",
            Self::DeterministicSemanticJev => "deterministic_semantic_jev",
            Self::FullGenerative => "full_generative",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteClass {
    Silent,
    Deterministic,
    SemanticReuse,
    JevReaction,
    Generated,
    CachedFallback,
    NonVerbalFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheLevel {
    Memory,
    LocalStorage,
    Generated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedSubsystem {
    Audio,
    Avatar,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventObservation {
    pub event_id: String,
    pub mode: ComparisonMode,
    pub route: RouteClass,
    pub routing_latency_us: u64,
    pub jev_latency_us: Option<u64>,
    pub generation_latency_us: Option<u64>,
    pub event_to_first_audio_ms: Option<u64>,
    pub event_to_first_visible_reaction_ms: Option<u64>,
    pub cache_level: Option<CacheLevel>,
    pub cache_lookup: bool,
    pub cache_hit: bool,
    pub degraded_subsystems: BTreeSet<DegradedSubsystem>,
    pub retrieval_candidates: usize,
    pub semantic_reuse_score: Option<f64>,
    pub semantic_reuse_accepted: bool,
    pub wrong_reuse: Option<bool>,
    pub operator_override: bool,
    pub cancelled: bool,
    pub fallback_reason: Option<String>,
    pub jev_attempts: u32,
    pub llm_calls: u32,
    pub tts_calls: u32,
    pub estimated_cost_microunits: Option<u64>,
}

impl EventObservation {
    pub fn new(event_id: impl Into<String>, mode: ComparisonMode, route: RouteClass) -> Self {
        Self {
            event_id: event_id.into(),
            mode,
            route,
            routing_latency_us: 0,
            jev_latency_us: None,
            generation_latency_us: None,
            event_to_first_audio_ms: None,
            event_to_first_visible_reaction_ms: None,
            cache_level: None,
            cache_lookup: false,
            cache_hit: false,
            degraded_subsystems: BTreeSet::new(),
            retrieval_candidates: 0,
            semantic_reuse_score: None,
            semantic_reuse_accepted: false,
            wrong_reuse: None,
            operator_override: false,
            cancelled: false,
            fallback_reason: None,
            jev_attempts: 0,
            llm_calls: 0,
            tts_calls: 0,
            estimated_cost_microunits: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproducibilityMetadata {
    pub dataset_id: String,
    pub git_commit: String,
    pub rust_toolchain: String,
    pub bun_toolchain: Option<String>,
    pub config_version: String,
    pub asset_version: String,
    pub index_version: Option<String>,
    pub jev_model: Option<String>,
    pub thinking_model: Option<String>,
    pub tts_model: Option<String>,
    pub seed: u64,
    pub stream_duration_ms: Option<u64>,
}

impl ReproducibilityMetadata {
    pub fn validate(&self) -> Result<(), TelemetryError> {
        for (name, value) in [
            ("dataset_id", self.dataset_id.as_str()),
            ("git_commit", self.git_commit.as_str()),
            ("rust_toolchain", self.rust_toolchain.as_str()),
            ("config_version", self.config_version.as_str()),
            ("asset_version", self.asset_version.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(TelemetryError::new(format!("{name} must not be empty")));
            }
        }
        if self.stream_duration_ms == Some(0) {
            return Err(TelemetryError::new(
                "stream_duration_ms must be positive when provided",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyDistribution {
    pub count: usize,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    pub events: usize,
    pub routing_latency_us: Option<LatencyDistribution>,
    pub jev_latency_us: Option<LatencyDistribution>,
    pub generation_latency_us: Option<LatencyDistribution>,
    pub event_to_first_audio_ms: Option<LatencyDistribution>,
    pub event_to_first_visible_reaction_ms: Option<LatencyDistribution>,
    pub route_counts: BTreeMap<RouteClass, u64>,
    pub cache_hits_by_level: BTreeMap<CacheLevel, u64>,
    pub cache_misses: u64,
    pub degraded_counts: BTreeMap<DegradedSubsystem, u64>,
    pub semantic_reuse_accepted: u64,
    pub semantic_reuse_rejected: u64,
    pub wrong_reuse_labels: u64,
    pub wrong_reuse_count: u64,
    pub operator_overrides: u64,
    pub cancellations: u64,
    pub fallback_counts: BTreeMap<String, u64>,
    pub jev_attempts: u64,
    pub llm_calls: u64,
    pub tts_calls: u64,
    pub llm_calls_per_event: f64,
    pub tts_calls_per_event: f64,
    pub llm_avoidance_rate_vs_one_call_per_event: Option<f64>,
    pub tts_avoidance_rate_vs_one_call_per_event: Option<f64>,
    pub estimated_cost_observations: u64,
    pub estimated_cost_microunits: u64,
    pub estimated_cost_microunits_per_event: Option<f64>,
    pub estimated_cost_microunits_per_stream_hour: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub metadata: ReproducibilityMetadata,
    pub mode: ComparisonMode,
    pub events: Vec<EventObservation>,
    pub summary: BenchmarkSummary,
}

impl BenchmarkReport {
    pub fn from_events(
        metadata: ReproducibilityMetadata,
        mode: ComparisonMode,
        events: Vec<EventObservation>,
    ) -> Result<Self, TelemetryError> {
        metadata.validate()?;
        if events.iter().any(|event| event.mode != mode) {
            return Err(TelemetryError::new(
                "all observations in one report must use the report comparison mode",
            ));
        }

        let summary = summarize(&events, metadata.stream_duration_ms);
        Ok(Self {
            metadata,
            mode,
            events,
            summary,
        })
    }

    pub fn to_json_pretty(&self) -> Result<Vec<u8>, TelemetryError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| TelemetryError::new(format!("serialize benchmark report: {error}")))
    }

    pub fn calibration_csv(&self) -> String {
        let mut out = String::from(
            "dataset_id,mode,event_id,semantic_reuse_score,semantic_reuse_accepted,wrong_reuse,operator_override,cancelled,fallback_reason\n",
        );
        for event in &self.events {
            let score = event
                .semantic_reuse_score
                .map(|value| format!("{value:.6}"))
                .unwrap_or_default();
            let wrong = event
                .wrong_reuse
                .map(|value| value.to_string())
                .unwrap_or_default();
            let fallback = event.fallback_reason.as_deref().unwrap_or_default();
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{}\n",
                csv(&self.metadata.dataset_id),
                self.mode.as_str(),
                csv(&event.event_id),
                score,
                event.semantic_reuse_accepted,
                wrong,
                event.operator_override,
                event.cancelled,
                csv(fallback),
            ));
        }
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryCollector {
    events: Vec<EventObservation>,
}

impl TelemetryCollector {
    pub fn record(&mut self, observation: EventObservation) {
        self.events.push(observation);
    }

    pub fn events(&self) -> &[EventObservation] {
        &self.events
    }

    pub fn events_mut(&mut self) -> &mut [EventObservation] {
        &mut self.events
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn label_wrong_reuse(&mut self, event_id: &str, wrong: bool) -> Result<(), TelemetryError> {
        let event = self
            .events
            .iter_mut()
            .rev()
            .find(|event| event.event_id == event_id)
            .ok_or_else(|| TelemetryError::new(format!("unknown telemetry event {event_id:?}")))?;
        event.wrong_reuse = Some(wrong);
        Ok(())
    }

    pub fn report(
        &self,
        metadata: ReproducibilityMetadata,
        mode: ComparisonMode,
    ) -> Result<BenchmarkReport, TelemetryError> {
        BenchmarkReport::from_events(metadata, mode, self.events.clone())
    }

    /// Aggregate retained observations without cloning the event detail set.
    pub fn summary(&self, stream_duration_ms: Option<u64>) -> BenchmarkSummary {
        summarize(&self.events, stream_duration_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonSuite {
    pub reports: Vec<BenchmarkReport>,
}

impl ComparisonSuite {
    pub fn complete(reports: Vec<BenchmarkReport>) -> Result<Self, TelemetryError> {
        if reports.len() != ComparisonMode::ALL.len() {
            return Err(TelemetryError::new(
                "comparison suite requires exactly four reports",
            ));
        }

        let mut modes = BTreeSet::new();
        let mut baseline_dataset: Option<&str> = None;
        let mut baseline_events: Option<Vec<&str>> = None;

        for report in &reports {
            if !modes.insert(report.mode) {
                return Err(TelemetryError::new("comparison modes must be unique"));
            }
            match baseline_dataset {
                None => baseline_dataset = Some(report.metadata.dataset_id.as_str()),
                Some(dataset) if dataset != report.metadata.dataset_id => {
                    return Err(TelemetryError::new(
                        "all comparison modes must use the same dataset_id",
                    ));
                }
                _ => {}
            }
            let ids = report
                .events
                .iter()
                .map(|event| event.event_id.as_str())
                .collect::<Vec<_>>();
            match &baseline_events {
                None => baseline_events = Some(ids),
                Some(expected) if *expected != ids => {
                    return Err(TelemetryError::new(
                        "all comparison modes must use the same ordered event fixture",
                    ));
                }
                _ => {}
            }
        }

        if ComparisonMode::ALL.iter().any(|mode| !modes.contains(mode)) {
            return Err(TelemetryError::new(
                "comparison suite is missing a required mode",
            ));
        }

        Ok(Self { reports })
    }

    pub fn report(&self, mode: ComparisonMode) -> Option<&BenchmarkReport> {
        self.reports.iter().find(|report| report.mode == mode)
    }

    pub fn llm_call_rate_reduction(
        &self,
        baseline: ComparisonMode,
        candidate: ComparisonMode,
    ) -> Option<f64> {
        let baseline = self.report(baseline)?.summary.llm_calls_per_event;
        let candidate = self.report(candidate)?.summary.llm_calls_per_event;
        if baseline <= 0.0 {
            return None;
        }
        Some((baseline - candidate) / baseline)
    }
}

fn summarize(events: &[EventObservation], stream_duration_ms: Option<u64>) -> BenchmarkSummary {
    let mut route_counts = BTreeMap::new();
    let mut cache_hits_by_level = BTreeMap::new();
    let mut degraded_counts = BTreeMap::new();
    let mut fallback_counts = BTreeMap::new();
    let mut cache_misses = 0_u64;
    let mut semantic_reuse_accepted = 0_u64;
    let mut semantic_reuse_rejected = 0_u64;
    let mut wrong_reuse_labels = 0_u64;
    let mut wrong_reuse_count = 0_u64;
    let mut operator_overrides = 0_u64;
    let mut cancellations = 0_u64;
    let mut jev_attempts = 0_u64;
    let mut llm_calls = 0_u64;
    let mut tts_calls = 0_u64;
    let mut cost_observations = 0_u64;
    let mut cost = 0_u64;

    for event in events {
        *route_counts.entry(event.route).or_insert(0) += 1;
        if event.cache_lookup {
            if event.cache_hit {
                if let Some(level) = event.cache_level {
                    *cache_hits_by_level.entry(level).or_insert(0) += 1;
                }
            } else {
                cache_misses += 1;
            }
        }
        for subsystem in &event.degraded_subsystems {
            *degraded_counts.entry(*subsystem).or_insert(0) += 1;
        }
        if event.retrieval_candidates > 0 {
            if event.semantic_reuse_accepted {
                semantic_reuse_accepted += 1;
            } else {
                semantic_reuse_rejected += 1;
            }
        }
        if let Some(wrong) = event.wrong_reuse {
            wrong_reuse_labels += 1;
            if wrong {
                wrong_reuse_count += 1;
            }
        }
        operator_overrides += u64::from(event.operator_override);
        cancellations += u64::from(event.cancelled);
        if let Some(reason) = &event.fallback_reason {
            *fallback_counts.entry(reason.clone()).or_insert(0) += 1;
        }
        jev_attempts += u64::from(event.jev_attempts);
        llm_calls += u64::from(event.llm_calls);
        tts_calls += u64::from(event.tts_calls);
        if let Some(measured_cost) = event.estimated_cost_microunits {
            cost_observations += 1;
            cost = cost.saturating_add(measured_cost);
        }
    }

    let count = events.len() as f64;
    BenchmarkSummary {
        events: events.len(),
        routing_latency_us: distribution(events.iter().map(|event| Some(event.routing_latency_us))),
        jev_latency_us: distribution(events.iter().map(|event| event.jev_latency_us)),
        generation_latency_us: distribution(events.iter().map(|event| event.generation_latency_us)),
        event_to_first_audio_ms: distribution(
            events.iter().map(|event| event.event_to_first_audio_ms),
        ),
        event_to_first_visible_reaction_ms: distribution(
            events
                .iter()
                .map(|event| event.event_to_first_visible_reaction_ms),
        ),
        route_counts,
        cache_hits_by_level,
        cache_misses,
        degraded_counts,
        semantic_reuse_accepted,
        semantic_reuse_rejected,
        wrong_reuse_labels,
        wrong_reuse_count,
        operator_overrides,
        cancellations,
        fallback_counts,
        jev_attempts,
        llm_calls,
        tts_calls,
        llm_calls_per_event: if count == 0.0 {
            0.0
        } else {
            llm_calls as f64 / count
        },
        tts_calls_per_event: if count == 0.0 {
            0.0
        } else {
            tts_calls as f64 / count
        },
        llm_avoidance_rate_vs_one_call_per_event: (count > 0.0)
            .then(|| (1.0 - llm_calls as f64 / count).clamp(0.0, 1.0)),
        tts_avoidance_rate_vs_one_call_per_event: (count > 0.0)
            .then(|| (1.0 - tts_calls as f64 / count).clamp(0.0, 1.0)),
        estimated_cost_observations: cost_observations,
        estimated_cost_microunits: cost,
        estimated_cost_microunits_per_event: if cost_observations == 0 || count == 0.0 {
            None
        } else {
            Some(cost as f64 / count)
        },
        estimated_cost_microunits_per_stream_hour: if cost_observations == 0 {
            None
        } else {
            stream_duration_ms.map(|duration| cost as f64 * 3_600_000.0 / duration as f64)
        },
    }
}

fn distribution<I>(values: I) -> Option<LatencyDistribution>
where
    I: IntoIterator<Item = Option<u64>>,
{
    let mut values = values.into_iter().flatten().collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(LatencyDistribution {
        count: values.len(),
        p50: nearest_rank(&values, 50),
        p95: nearest_rank(&values, 95),
        p99: nearest_rank(&values, 99),
        max: *values.last().expect("non-empty latency values"),
    })
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn csv(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryError {
    message: String,
}

impl TelemetryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for TelemetryError {}

/// Runtime audit categories contain decisions, never credentials or generated content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCategory {
    Ingress,
    Authorization,
    Output,
    Generation,
    Memory,
}

/// Minimal structured audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub event_id: Option<String>,
    pub category: AuditCategory,
    pub decision: String,
    pub detail: String,
}

/// Redacts configured secret/config values before text reaches logs.
#[derive(Clone, Default)]
pub struct SecretRedactor {
    secrets: Vec<String>,
}

impl std::fmt::Debug for SecretRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRedactor")
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl SecretRedactor {
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let secrets = secrets
            .into_iter()
            .map(Into::into)
            .filter(|secret: &String| !secret.is_empty())
            .collect();
        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        self.secrets.iter().fold(text.to_owned(), |output, secret| {
            output.replace(secret, "[REDACTED]")
        })
    }

    pub fn redact_error(&self, error: &(dyn std::error::Error + 'static)) -> String {
        self.redact(&error.to_string())
    }

    pub fn record(
        &self,
        event_id: Option<&str>,
        category: AuditCategory,
        decision: impl Into<String>,
        detail: impl AsRef<str>,
    ) -> AuditRecord {
        let decision = decision.into();
        AuditRecord {
            event_id: event_id.map(str::to_owned),
            category,
            decision: self.redact(&decision),
            detail: self.redact(detail.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(dataset: &str) -> ReproducibilityMetadata {
        ReproducibilityMetadata {
            dataset_id: dataset.to_owned(),
            git_commit: "deadbeef".to_owned(),
            rust_toolchain: "rustc 1.98.0".to_owned(),
            bun_toolchain: Some("bun 1.3.14".to_owned()),
            config_version: "bench-v1".to_owned(),
            asset_version: "starter-v1".to_owned(),
            index_version: Some("index-v1".to_owned()),
            jev_model: Some("jev-v1".to_owned()),
            thinking_model: Some("gpt-test".to_owned()),
            tts_model: Some("tts-test".to_owned()),
            seed: 42,
            stream_duration_ms: Some(3_600_000),
        }
    }

    fn observations(mode: ComparisonMode) -> Vec<EventObservation> {
        (1_u64..=100)
            .map(|index| {
                let mut event =
                    EventObservation::new(format!("evt-{index}"), mode, RouteClass::SemanticReuse);
                event.routing_latency_us = index;
                event.event_to_first_audio_ms = Some(index);
                event.event_to_first_visible_reaction_ms = Some(index + 1);
                event.cache_lookup = true;
                event.cache_hit = true;
                event.cache_level = Some(CacheLevel::Memory);
                event.retrieval_candidates = 3;
                event.semantic_reuse_score = Some(0.9);
                event.semantic_reuse_accepted = true;
                event.jev_attempts = u32::from(matches!(
                    mode,
                    ComparisonMode::DeterministicSemanticJev | ComparisonMode::FullGenerative
                ));
                event.llm_calls = u32::from(mode == ComparisonMode::FullGenerative);
                event.tts_calls = event.llm_calls;
                event.estimated_cost_microunits = Some(u64::from(event.llm_calls) * 10);
                event
            })
            .collect()
    }

    #[test]
    fn latency_summary_reports_nearest_rank_percentiles() {
        let report = BenchmarkReport::from_events(
            metadata("fixture-a"),
            ComparisonMode::DeterministicSemantic,
            observations(ComparisonMode::DeterministicSemantic),
        )
        .expect("report");

        let routing = report.summary.routing_latency_us.expect("routing latency");
        assert_eq!(routing.p50, 50);
        assert_eq!(routing.p95, 95);
        assert_eq!(routing.p99, 99);
        assert_eq!(report.summary.cache_hits_by_level[&CacheLevel::Memory], 100);
        assert_eq!(report.summary.semantic_reuse_accepted, 100);
    }

    #[test]
    fn labels_and_calibration_export_preserve_review_signal() {
        let mode = ComparisonMode::DeterministicSemantic;
        let mut collector = TelemetryCollector::default();
        collector.record(observations(mode).remove(0));
        collector
            .label_wrong_reuse("evt-1", true)
            .expect("review label");

        let report = collector
            .report(metadata("fixture-a"), mode)
            .expect("report");
        assert_eq!(report.summary.wrong_reuse_labels, 1);
        assert_eq!(report.summary.wrong_reuse_count, 1);
        let csv = report.calibration_csv();
        assert!(csv.contains("semantic_reuse_score"));
        assert!(csv.contains("evt-1,0.900000,true,true"));
    }

    #[test]
    fn non_cache_routes_do_not_inflate_misses_and_degraded_state_is_counted() {
        let mode = ComparisonMode::FullGenerative;
        let mut silent = EventObservation::new("evt-silent", mode, RouteClass::Silent);
        silent.degraded_subsystems.insert(DegradedSubsystem::Audio);
        let mut cache_miss =
            EventObservation::new("evt-cache-miss", mode, RouteClass::SemanticReuse);
        cache_miss.cache_lookup = true;
        cache_miss
            .degraded_subsystems
            .insert(DegradedSubsystem::Audio);
        cache_miss
            .degraded_subsystems
            .insert(DegradedSubsystem::Avatar);
        let report =
            BenchmarkReport::from_events(metadata("fixture-a"), mode, vec![silent, cache_miss])
                .expect("report");

        assert_eq!(report.summary.cache_misses, 1);
        assert_eq!(report.summary.degraded_counts[&DegradedSubsystem::Audio], 2);
        assert_eq!(
            report.summary.degraded_counts[&DegradedSubsystem::Avatar],
            1
        );
        assert_eq!(
            report.summary.llm_avoidance_rate_vs_one_call_per_event,
            Some(1.0)
        );
        assert_eq!(
            report.summary.tts_avoidance_rate_vs_one_call_per_event,
            Some(1.0)
        );
    }

    #[test]
    fn unmeasured_cost_is_not_reported_as_zero_cost() {
        let mode = ComparisonMode::FullGenerative;
        let event = EventObservation::new("evt-no-cost", mode, RouteClass::Generated);
        let report =
            BenchmarkReport::from_events(metadata("fixture-a"), mode, vec![event]).expect("report");

        assert_eq!(report.summary.estimated_cost_observations, 0);
        assert_eq!(report.summary.estimated_cost_microunits, 0);
        assert_eq!(report.summary.estimated_cost_microunits_per_event, None);
        assert_eq!(
            report.summary.estimated_cost_microunits_per_stream_hour,
            None
        );
    }

    #[test]
    fn comparison_suite_requires_same_fixture_for_all_four_modes() {
        let reports = ComparisonMode::ALL
            .into_iter()
            .map(|mode| {
                BenchmarkReport::from_events(metadata("fixture-a"), mode, observations(mode))
                    .expect("report")
            })
            .collect();
        let suite = ComparisonSuite::complete(reports).expect("complete suite");
        assert_eq!(suite.reports.len(), 4);

        let mut mismatched = suite.reports.clone();
        mismatched[3].metadata.dataset_id = "fixture-b".to_owned();
        assert!(ComparisonSuite::complete(mismatched).is_err());
    }

    #[test]
    fn configured_secrets_are_redacted_from_audit_text() {
        let redactor = SecretRedactor::new(["api-secret-123", "session-secret-456"]);
        let record = redactor.record(
            Some("evt-1"),
            AuditCategory::Authorization,
            "rejected api-secret-123",
            "token=api-secret-123 session=session-secret-456",
        );

        assert_eq!(record.decision, "rejected [REDACTED]");
        assert_eq!(record.detail, "token=[REDACTED] session=[REDACTED]");
        assert!(!format!("{record:?}").contains("api-secret-123"));
        assert!(!format!("{record:?}").contains("session-secret-456"));
    }

    #[test]
    fn configured_secrets_are_redacted_from_error_log_text() {
        #[derive(Debug)]
        struct ExampleError;

        impl std::fmt::Display for ExampleError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("backend rejected token=api-secret-123")
            }
        }

        impl std::error::Error for ExampleError {}

        let redactor = SecretRedactor::new(["api-secret-123"]);
        assert_eq!(
            redactor.redact_error(&ExampleError),
            "backend rejected token=[REDACTED]"
        );
    }
}
