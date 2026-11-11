#![forbid(unsafe_code)]

//! Event-driven reflex routing, semantic retrieval, deterministic policy,
//! and the TypeSafe System One (Jev) adapter.

mod audience;
mod burst;
mod jev;
mod pipeline;
mod retrieval;

pub use audience::{
    AUDIENCE_METHOD_VERSION, AUDIENCE_SIGNAL_SCHEMA_VERSION, AggregationMethod, AudienceCluster,
    AudienceConfig, AudienceConfigError, AudienceInput, AudiencePlan, AudienceSignal,
    AudienceSignalClass, AudienceTrend, AudienceWindow, GuardReason, MAX_AUDIENCE_MIN_CLUSTER,
    MAX_DUPLICATE_TEXT_CHARS, MAX_SIGNAL_REPRESENTATIVES, ReactionFamily, guard_text,
    plan_audience, reaction_family,
};
pub use burst::{
    BATCH_ENGAGEMENT_SCHEMA_VERSION, BatchCandidate, BatchEngagementRequest, BatchRankFailure,
    BatchRanker, BatchReplayRecord, BatchRequestError, BatchResolution, BurstConfig,
    BurstConfigError, BurstEngine, BurstMetrics, CandidateOutcome, CapacityEnvelope, Disposition,
    FlushReason, ImmediateReason, MAX_BATCH_BYTES, MAX_BATCH_CANDIDATES,
    MAX_BATCH_REQUEST_CANDIDATES, MAX_BATCH_WINDOW_MS, MAX_BURST_ENTER_COUNT,
    MAX_CANDIDATE_TEXT_BYTES, MAX_READY_BATCHES, MAX_TRACKED_ACTORS, RankingSource, RefusalReason,
    RequestCandidate, TerminalOutcome,
};
pub use jev::{
    HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, JevCallEvidence,
    JevCallFailure, JevCancellationToken, JevUsage, NormalizedAnswer, NormalizedResponse,
    SystemOneResponse, TransportError, UreqTransport, normalize_answers, parse_jev_response_body,
};
pub use pipeline::{
    DecisionEvidence, DecisionReplayRecord, ExecutedAction, ExecutedDecision, ModelEvidence,
    PolicyConfig, PolicyDecision, ReflexPipeline, ReflexPipelineInput, apply_policy,
    execute_policy,
};
pub use retrieval::{
    IndexedAsset, RetrievalCandidate, RetrievalError, RetrievalMetadata, RetrievalResult,
    SemanticIndex, SimilarityMetric,
};

/// Availability state used by deterministic fallback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionAvailability {
    Available,
    TimedOut,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::DecisionAvailability;

    #[test]
    fn timeout_is_not_available() {
        assert_ne!(
            DecisionAvailability::TimedOut,
            DecisionAvailability::Available
        );
    }
}
