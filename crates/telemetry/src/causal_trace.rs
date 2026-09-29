//! Privacy-safe causal trace vocabulary and bounded collector (issue #65).
//!
//! A causal trace connects the stages one admitted event passes through —
//! ingress/normalization, security admission, retrieval, reflex/Jev, template
//! or generative routing, output gating, generation, scheduler admission, and
//! audio/avatar dispatch — into one bounded timeline keyed by the existing
//! `event_id`/`correlation_id` identity.
//!
//! Privacy contract: trace records carry IDs, stage names, typed
//! outcomes/reasons, counts, byte sizes, and monotonic timings only. Prompt
//! text, generated text, secrets, and private memory contents have no field
//! to reach a trace record; the type system makes storing them impossible.
//!
//! Determinism contract: the deterministic trace identity (stage sequence,
//! outcomes, reasons, relationships) is separated from observational
//! wall-clock timing. `CausalTrace::deterministic_bytes` normalizes timing
//! away so replay compares causal structure without requiring identical
//! observed latency.
//!
//! Retention: traces are stored in a ring bounded by the same
//! policy-style configuration used by every other retained runtime state
//! (issue #51 unified bounded retention).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Stable stage names for the event-to-performance execution path.
///
/// These names are contract: benchmark attribution (#58), diagnostic bundles
/// (#68), and AAR exports (#62) key on them, so new stages append rather than
/// rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceStage {
    /// Raw content admitted/bounded and normalized at the ingress boundary.
    IngressNormalization,
    /// Security runtime admitted the event into the content plane.
    SecurityAdmission,
    /// Asset-store / embedding snapshot lookup.
    CacheLookup,
    /// Semantic retrieval candidate selection.
    SemanticRetrieval,
    /// Reflex/Jev pipeline execution (routing decision included).
    ReflexJev,
    /// Template decision resolution (issue #54 planner).
    TemplateResolution,
    /// ThinkingEngine / generative pipeline submission or execution.
    Generation,
    /// Security output gate (`publish_text`) verdict.
    OutputGate,
    /// Generated asset compilation/insertion.
    AssetCompilation,
    /// Scheduler admitted the timeline (or rejected/dropped it).
    SchedulerAdmission,
    /// Audio adapter dispatch.
    AudioDispatch,
    /// Avatar adapter dispatch.
    AvatarDispatch,
    /// OBS/stream dispatch.
    StreamDispatch,
    /// Operator override (stop/mute) or generation cancellation.
    Cancellation,
}

impl TraceStage {
    pub const ALL: [Self; 14] = [
        Self::IngressNormalization,
        Self::SecurityAdmission,
        Self::CacheLookup,
        Self::SemanticRetrieval,
        Self::ReflexJev,
        Self::TemplateResolution,
        Self::Generation,
        Self::OutputGate,
        Self::AssetCompilation,
        Self::SchedulerAdmission,
        Self::AudioDispatch,
        Self::AvatarDispatch,
        Self::StreamDispatch,
        Self::Cancellation,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IngressNormalization => "ingress_normalization",
            Self::SecurityAdmission => "security_admission",
            Self::CacheLookup => "cache_lookup",
            Self::SemanticRetrieval => "semantic_retrieval",
            Self::ReflexJev => "reflex_jev",
            Self::TemplateResolution => "template_resolution",
            Self::Generation => "generation",
            Self::OutputGate => "output_gate",
            Self::AssetCompilation => "asset_compilation",
            Self::SchedulerAdmission => "scheduler_admission",
            Self::AudioDispatch => "audio_dispatch",
            Self::AvatarDispatch => "avatar_dispatch",
            Self::StreamDispatch => "stream_dispatch",
            Self::Cancellation => "cancellation",
        }
    }
}

/// Typed stage outcome. Enumerated, never prose, so downstream evaluators
/// (#108) can perform deterministic failure attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    /// Stage completed successfully.
    Completed,
    /// Stage skipped (not applicable for this route).
    Skipped,
    /// Stage degraded to a fallback.
    Degraded,
    /// Stage failed with a typed reason.
    Failed,
    /// Stage was cancelled before completing.
    Cancelled,
    /// Stage superseded by an operator override.
    Overridden,
}

/// Typed failure/degradation reasons. Stable vocabulary; never carry payload
/// content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageReason {
    #[default]
    None,
    CacheMiss,
    NoCandidates,
    ReuseThresholdRejected,
    RouteConfidenceRejected,
    DeadlineExhausted,
    DeadlineInsufficientBudget,
    AdapterError,
    SecuritySuppressed,
    SecurityReplaced,
    Muted,
    SchedulerLate,
    OperatorStop,
    GenerationCancelled,
    BackendUnavailable,
    Other,
}

/// One stage observation inside a causal trace.
///
/// Fields are restricted to identity, stage vocabulary, typed outcome/reason,
/// counts/sizes, monotonic timing, and provider/version identity. There is no
/// string free-text field: privacy is structural, not enforced by convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSpan {
    /// Causal predecessor span index within the same trace (`None` for the
    /// first stage of an event chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    pub stage: TraceStage,
    pub outcome: StageOutcome,
    #[serde(default, skip_serializing_if = "StageReason::is_none")]
    pub reason: StageReason,
    /// Monotonic start offset from event admission, milliseconds.
    pub start_offset_ms: u64,
    /// Stage duration, milliseconds (observational, never replay identity).
    pub duration_ms: u64,
    /// Bounded byte size observed at the stage (e.g. ingress size), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Provider/model/version identity relevant to the stage, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Bounded count observation (attempts, candidates, chunks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl StageReason {
    pub fn is_none(reason: &Self) -> bool {
        *reason == Self::None
    }
}

/// Bounded, privacy-safe causal timeline for one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalTrace {
    pub event_id: String,
    pub correlation_id: String,
    pub spans: Vec<TraceSpan>,
    /// Monotonic sequence for cross-trace ordering within a stream.
    pub sequence: u64,
}

impl CausalTrace {
    pub fn new(
        event_id: impl Into<String>,
        correlation_id: impl Into<String>,
        sequence: u64,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            correlation_id: correlation_id.into(),
            spans: Vec::new(),
            sequence,
        }
    }

    /// Append a stage observation; returns its span index for parenting.
    pub fn push_span(
        &mut self,
        stage: TraceStage,
        outcome: StageOutcome,
        reason: StageReason,
        start_offset_ms: u64,
        duration_ms: u64,
    ) -> usize {
        let parent = self.spans.last().map(|last| last_index(self, last));
        self.spans.push(TraceSpan {
            parent,
            stage,
            outcome,
            reason,
            start_offset_ms,
            duration_ms,
            bytes: None,
            provider: None,
            count: None,
        });
        self.spans.len() - 1
    }

    /// Attach provider/version identity to a span.
    pub fn with_provider(&mut self, index: usize, provider: impl Into<String>) {
        if let Some(span) = self.spans.get_mut(index) {
            span.provider = Some(provider.into());
        }
    }

    /// Attach a bounded count to a span.
    pub fn with_count(&mut self, index: usize, count: u32) {
        if let Some(span) = self.spans.get_mut(index) {
            span.count = Some(count);
        }
    }

    /// Attach a bounded byte size to a span.
    pub fn with_bytes(&mut self, index: usize, bytes: u64) {
        if let Some(span) = self.spans.get_mut(index) {
            span.bytes = Some(bytes);
        }
    }

    /// Record a cancellation/override as the closing stage of the chain.
    pub fn record_cancellation(&mut self, reason: StageReason, at_offset_ms: u64) {
        let last_end = self
            .spans
            .last()
            .map(|span| span.start_offset_ms + span.duration_ms)
            .unwrap_or(0);
        let index = self.push_span(
            TraceStage::Cancellation,
            StageOutcome::Cancelled,
            reason,
            at_offset_ms.max(last_end),
            0,
        );
        let _ = index;
    }

    /// Deterministic trace identity: stage sequence, outcomes, reasons,
    /// counts, and causal relationships. Observational timing and byte sizes
    /// are normalized away so replay compares causal structure, not latency.
    pub fn deterministic_bytes(&self) -> Vec<u8> {
        let normalized: Vec<TraceSpan> = self
            .spans
            .iter()
            .map(|span| TraceSpan {
                start_offset_ms: 0,
                duration_ms: 0,
                bytes: None,
                ..span.clone()
            })
            .collect();
        #[derive(Serialize)]
        struct DeterministicIdentity<'a> {
            correlation_id: &'a str,
            sequence: u64,
            spans: &'a Vec<TraceSpan>,
        }
        serde_json::to_vec(&DeterministicIdentity {
            correlation_id: &self.correlation_id,
            sequence: self.sequence,
            spans: &normalized,
        })
        .expect("causal trace identity is JSON-safe")
    }

    /// Stage-level latency attribution: total observed duration per stage
    /// name, for #58 regression decomposition (diagnostic, not causal proof).
    pub fn stage_latency_totals(&self) -> BTreeMap<TraceStage, u64> {
        let mut totals = BTreeMap::new();
        for span in &self.spans {
            *totals.entry(span.stage).or_insert(0) += span.duration_ms;
        }
        totals
    }

    /// Bounded redacted summary for diagnostic bundles (#68): one line per
    /// stage, no payload, suitable for incident references by correlation id.
    pub fn redacted_timeline(&self) -> String {
        let mut out = format!("corr={} seq={}\n", self.correlation_id, self.sequence);
        for span in &self.spans {
            let reason = if StageReason::is_none(&span.reason) {
                String::new()
            } else {
                format!(
                    " reason={}",
                    serde_json::to_string(&span.reason).unwrap_or_default()
                )
            };
            out.push_str(&format!(
                "  {} {} ({}ms){}\n",
                span.stage.as_str(),
                serde_json::to_string(&span.outcome).unwrap_or_default(),
                span.duration_ms,
                reason,
            ));
        }
        out
    }
}

fn last_index(trace: &CausalTrace, span: &TraceSpan) -> usize {
    trace
        .spans
        .iter()
        .position(|candidate| std::ptr::eq(candidate, span))
        .unwrap_or_else(|| trace.spans.len().saturating_sub(1))
}

/// Bounded retention for causal traces, mirroring the unified runtime
/// retention policy shape (issue #51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CausalTraceRetentionConfig {
    pub max_traces: usize,
}

impl Default for CausalTraceRetentionConfig {
    fn default() -> Self {
        Self { max_traces: 1_024 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalTraceRetentionMetrics {
    pub retained: usize,
    pub high_water: usize,
    pub evicted: u64,
}

/// Bounded collector: one trace per event, oldest evicted first.
#[derive(Debug, Clone)]
pub struct CausalTraceCollector {
    traces: Vec<CausalTrace>,
    /// Index from correlation_id to trace positions for bounded lookup.
    retention: CausalTraceRetentionConfig,
    high_water: usize,
    evicted: u64,
    next_sequence: u64,
}

impl Default for CausalTraceCollector {
    fn default() -> Self {
        Self::with_retention(CausalTraceRetentionConfig::default())
    }
}

impl CausalTraceCollector {
    pub fn with_retention(retention: CausalTraceRetentionConfig) -> Self {
        assert!(
            retention.max_traces > 0,
            "causal trace max_traces must be positive"
        );
        Self {
            traces: Vec::new(),
            retention,
            high_water: 0,
            evicted: 0,
            next_sequence: 0,
        }
    }

    pub fn set_retention(&mut self, retention: CausalTraceRetentionConfig) {
        assert!(
            retention.max_traces > 0,
            "causal trace max_traces must be positive"
        );
        self.retention = retention;
        self.trim_to_retention();
    }

    pub fn retention_metrics(&self) -> CausalTraceRetentionMetrics {
        CausalTraceRetentionMetrics {
            retained: self.traces.len(),
            high_water: self.high_water,
            evicted: self.evicted,
        }
    }

    /// Begin a new trace for an event; the sequence is assigned monotonically.
    pub fn begin_trace(
        &mut self,
        event_id: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> CausalTrace {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        CausalTrace::new(event_id, correlation_id, sequence)
    }

    /// Commit a completed trace; bounded ring applies.
    pub fn commit(&mut self, trace: CausalTrace) {
        self.traces.push(trace);
        self.trim_to_retention();
        self.high_water = self.high_water.max(self.traces.len());
    }

    fn trim_to_retention(&mut self) {
        if self.traces.len() <= self.retention.max_traces {
            return;
        }
        let remove = self.traces.len() - self.retention.max_traces;
        self.traces.drain(..remove);
        self.evicted = self.evicted.saturating_add(remove as u64);
    }

    pub fn traces(&self) -> &[CausalTrace] {
        &self.traces
    }

    /// Bounded extraction for diagnostic bundles: the trace matching a
    /// correlation id (incident references carry exactly one correlation id).
    pub fn trace_for_correlation(&self, correlation_id: &str) -> Option<&CausalTrace> {
        self.traces
            .iter()
            .rev()
            .find(|trace| trace.correlation_id == correlation_id)
    }

    /// Bounded allowlist feed for diagnostic/support bundles (#68): the most
    /// recent distinct correlation ids, newest first. Correlation ids are
    /// operator-generated references, not viewer identities, so listing them
    /// exposes no private content — only which incidents can be expanded via
    /// `trace_for_correlation` / `redacted_timeline`.
    pub fn recent_correlation_ids(&self, limit: usize) -> Vec<&str> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for trace in self.traces.iter().rev() {
            if out.len() >= limit {
                break;
            }
            if seen.insert(trace.correlation_id.as_str()) {
                out.push(trace.correlation_id.as_str());
            }
        }
        out
    }

    /// Aggregate stage-level latency attribution across retained traces (#58
    /// integration point). Returns total observed duration per stage.
    pub fn stage_latency_totals(&self) -> BTreeMap<TraceStage, u64> {
        let mut totals = BTreeMap::new();
        for trace in &self.traces {
            for (stage, duration) in trace.stage_latency_totals() {
                *totals.entry(stage).or_insert(0) += duration;
            }
        }
        totals
    }

    pub fn clear(&mut self) {
        self.traces.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CausalTraceError {
    message: String,
}

impl CausalTraceError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CausalTraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CausalTraceError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trace(sequence: u64) -> CausalTrace {
        let mut trace = CausalTrace::new("evt-1", "corr-1", sequence);
        let ingress = trace.push_span(
            TraceStage::IngressNormalization,
            StageOutcome::Completed,
            StageReason::None,
            0,
            1,
        );
        trace.with_bytes(ingress, 512);
        let admission = trace.push_span(
            TraceStage::SecurityAdmission,
            StageOutcome::Completed,
            StageReason::None,
            1,
            1,
        );
        let _ = admission;
        let retrieval = trace.push_span(
            TraceStage::SemanticRetrieval,
            StageOutcome::Completed,
            StageReason::None,
            2,
            3,
        );
        trace.with_count(retrieval, 5);
        let jev = trace.push_span(
            TraceStage::ReflexJev,
            StageOutcome::Completed,
            StageReason::None,
            5,
            19,
        );
        trace.with_provider(jev, "jev-v1");
        trace.push_span(
            TraceStage::SchedulerAdmission,
            StageOutcome::Completed,
            StageReason::None,
            24,
            1,
        );
        trace.push_span(
            TraceStage::AudioDispatch,
            StageOutcome::Completed,
            StageReason::None,
            25,
            2,
        );
        trace
    }

    #[test]
    fn trace_records_stage_chain_with_parent_links() {
        let trace = sample_trace(0);
        assert_eq!(trace.spans.len(), 6);
        // First span has no parent; every later span chains to its predecessor.
        assert!(trace.spans[0].parent.is_none());
        for (index, span) in trace.spans.iter().enumerate().skip(1) {
            assert_eq!(span.parent, Some(index - 1));
        }
    }

    #[test]
    fn deterministic_identity_ignores_observed_timing() {
        let mut timed = sample_trace(7);
        timed.spans[2].duration_ms = 99;
        timed.spans[2].start_offset_ms = 500;
        timed.spans[3].bytes = Some(4096);
        let baseline = sample_trace(7);

        assert_eq!(
            timed.deterministic_bytes(),
            baseline.deterministic_bytes(),
            "observed timing/bytes must not change deterministic identity"
        );
    }

    #[test]
    fn deterministic_identity_changes_with_causal_structure() {
        let baseline = sample_trace(7);
        let mut different = sample_trace(7);
        different.spans[2].outcome = StageOutcome::Degraded;
        different.spans[2].reason = StageReason::NoCandidates;
        assert_ne!(
            baseline.deterministic_bytes(),
            different.deterministic_bytes()
        );
    }

    #[test]
    fn stage_latency_totals_aggregate_per_stage() {
        let trace = sample_trace(0);
        let totals = trace.stage_latency_totals();
        assert_eq!(totals[&TraceStage::SemanticRetrieval], 3);
        assert_eq!(totals[&TraceStage::ReflexJev], 19);
    }

    #[test]
    fn redacted_timeline_contains_no_payload_text() {
        let mut trace = sample_trace(3);
        trace.record_cancellation(StageReason::OperatorStop, 40);
        let timeline = trace.redacted_timeline();
        assert!(timeline.starts_with("corr=corr-1 seq=3"));
        assert!(timeline.contains("cancellation"));
        assert!(timeline.contains("operator_stop"));
    }

    #[test]
    fn retention_ring_evicts_oldest_and_tracks_metrics() {
        let mut collector =
            CausalTraceCollector::with_retention(CausalTraceRetentionConfig { max_traces: 3 });
        for index in 0..10_u64 {
            let trace = collector.begin_trace(format!("evt-{index}"), format!("corr-{index}"));
            collector.commit(trace);
        }
        let metrics = collector.retention_metrics();
        assert_eq!(metrics.retained, 3);
        assert_eq!(metrics.high_water, 3);
        assert_eq!(metrics.evicted, 7);
        assert_eq!(collector.traces()[0].event_id, "evt-7");
    }

    #[test]
    fn correlation_lookup_returns_latest_matching_trace() {
        let mut collector = CausalTraceCollector::default();
        let first = collector.begin_trace("evt-a", "corr-x");
        collector.commit(first);
        let second = collector.begin_trace("evt-b", "corr-y");
        collector.commit(second);

        assert_eq!(
            collector
                .trace_for_correlation("corr-x")
                .expect("found")
                .event_id,
            "evt-a"
        );
        assert!(collector.trace_for_correlation("corr-missing").is_none());
    }

    #[test]
    fn cross_trace_stage_attribution_sums_all_traces() {
        let mut collector = CausalTraceCollector::default();
        for _ in 0..2 {
            let trace = sample_trace(collector.next_sequence);
            collector.commit(trace);
        }
        let totals = collector.stage_latency_totals();
        assert_eq!(totals[&TraceStage::ReflexJev], 38);
    }

    #[test]
    fn stage_vocabulary_is_stable_and_serializable() {
        assert_eq!(TraceStage::ALL.len(), 14);
        assert_eq!(TraceStage::ReflexJev.as_str(), "reflex_jev");
        let json = serde_json::to_string(&TraceStage::OutputGate).expect("stage json");
        assert_eq!(json, "\"output_gate\"");
    }
}
