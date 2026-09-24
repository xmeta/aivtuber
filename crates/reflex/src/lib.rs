#![forbid(unsafe_code)]

//! Event-driven reflex routing, semantic retrieval, deterministic policy,
//! and the TypeSafe System One (Jev) adapter.

mod jev;
mod pipeline;
mod retrieval;

pub use jev::{
    HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, JevCallEvidence,
    JevCallFailure, JevCancellationToken, JevUsage, NormalizedAnswer, TransportError,
    UreqTransport,
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
