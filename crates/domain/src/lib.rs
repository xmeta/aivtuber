#![forbid(unsafe_code)]

//! Provider-neutral domain types and interfaces for the AI VTuber runtime.

mod avatar;
mod context;
mod control;
mod deadline;
mod error;
mod event;
mod floor;
mod interfaces;
mod reflex;
pub mod scenario;
mod sidecar;
mod speech;
mod speech_lifecycle;
mod speech_transcript;
mod task;

pub use avatar::Viseme;
pub use context::{
    EpistemicClass, MAX_RECENT_ITEMS, MAX_RETRIEVAL_CANDIDATES, MAX_SNAPSHOT_STRING_BYTES,
    MAX_THINKING_CONTEXT_BYTES, MAX_THINKING_CONTEXT_ITEMS, MAX_THINKING_TEXT_BYTES,
    PerformerSnapshot, PrivacyClass, REFLEX_CONTEXT_SCHEMA_VERSION, REFLEX_REQUEST_SCHEMA_VERSION,
    RecentInteractionSnapshot, ReflexContext, ReflexRequest, RetrievalCandidateContext,
    RetrievalSnapshot, StreamSnapshot, THINKING_REQUEST_SCHEMA_VERSION, ThinkingContent,
    ThinkingContextSource, ThinkingRequest,
};
pub use control::{
    AuthenticatedControl, AuthenticatedControlCommand, CONTROL_SECRET_LEN, ControlIngressError,
    ControlSecret, LocalControlIngress, OperatorCommandInput,
};
pub use deadline::{
    DeadlineExhaustionReason, DeadlineStage, InteractionDeadline, InteractionDeadlineClass,
};
pub use error::DomainValidationError;
pub use event::{
    AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, EventEnvelope,
    EventKind, SecurityPlane, SourceClass, TrustLevel,
};
pub use floor::*;
pub use interfaces::{
    Authorized, AuthorizedAvatarAction, AuthorizedStreamAction, AuthorizedToolAction, AvatarAction,
    AvatarAdapter, DecisionEngine, DeltaFlow, EngineError, EngineErrorKind, EngineFuture,
    GeneratedReply, MAX_TOOL_DESCRIPTOR_BYTES, MAX_TOOL_DISCOVERY_ITEMS,
    MAX_TOOL_OBSERVATION_BYTES, ReplyDeltaSink, SpeechArtifact, SpeechProgress, SpeechProgressSink,
    SpeechRequest, StreamAction, StreamAdapter, ThinkingEngine, ToolAction, ToolAdapter,
    ToolCommitFence, ToolDescriptor, ToolExecutionResult, ToolResultClass, ToolVerificationOutcome,
    ToolVerificationRule, TtsBackendIdentity, TtsEngine, authorize_avatar_action,
    authorize_stream_action, authorize_tool_action, verify_tool_result,
};
pub use reflex::{
    AttentionTarget, BackendIdentity, FallbackReason, REFLEX_SCHEMA_VERSION, ReflexDecision,
    ResponseRoute, RouteDecision,
};
pub use scenario::*;
pub use sidecar::{
    MAX_SIDECAR_CLAIM_BYTES, MAX_SIDECAR_QUERY_BYTES, MAX_SIDECAR_RECALL_ITEMS, MemorySidecar,
    SidecarClaim, SidecarHandle, SidecarRecallItem, SidecarRecallQuery,
};
pub use speech::{
    FallbackTurnConfig, FallbackTurnSignal, MAX_LANGUAGE_LEN, MAX_MODEL_ID_LEN,
    MAX_OBSERVATION_TIME_MS, MAX_OBSERVATION_VALIDITY_MS, MAX_PREDICTION_HORIZON_MS,
    MAX_PROVIDER_TOKEN_LEN, MAX_SPEAKER_ID_LEN, MAX_SPEECH_OBSERVATION_BATCH_BYTES,
    MAX_SPEECH_OBSERVATIONS_PER_BATCH, SPEECH_OBSERVATION_SCHEMA_VERSION, SpeakerActivity,
    SpeakerActivityState, SpeechActivityObservation, SpeechEdge, SpeechObservation,
    SpeechObservationBatch, TurnEndFallbackReason, TurnEndScoreObservation, fallback_turn_signal,
};
pub use speech_lifecycle::*;
pub use speech_transcript::*;
pub use task::{
    AgentTask, MAX_TASK_ID_BYTES, MAX_TASK_LIFETIME_MS_HARD, MAX_TASK_OBSERVATIONS_HARD,
    MAX_TASK_PENDING_TTL_MS_HARD, MAX_TASK_REASON_BYTES, MAX_TASK_REPLANS_HARD,
    MAX_TASK_REQUEST_BYTES, MAX_TASK_RETRIES_HARD, MAX_TASK_STEP_BYTES, MAX_TASK_STEPS_HARD,
    PendingCommitment, PendingCommitmentKind, TaskError, TaskId, TaskLimits, TaskObservation,
    TaskOutcome, TaskState, TaskStateKind, TaskStep, TaskStepState, TaskTransition,
    TaskTransitionReason, TaskVerificationOutcome, VerificationResult,
};

/// Runtime time-scale layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionLayer {
    /// Deterministic frame/timeline execution.
    Motor,
    /// Event-driven structured decisions.
    Reflex,
    /// Expensive generative reasoning.
    Thinking,
}

#[cfg(test)]
mod tests {
    use super::ExecutionLayer;

    #[test]
    fn layers_are_distinct() {
        assert_ne!(ExecutionLayer::Motor, ExecutionLayer::Reflex);
        assert_ne!(ExecutionLayer::Reflex, ExecutionLayer::Thinking);
    }
}
