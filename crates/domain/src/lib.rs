#![forbid(unsafe_code)]

//! Provider-neutral domain types and interfaces for the AI VTuber runtime.

mod context;
mod control;
mod error;
mod event;
mod interfaces;
mod reflex;

pub use context::{
    MAX_RECENT_ITEMS, MAX_RETRIEVAL_CANDIDATES, MAX_SNAPSHOT_STRING_BYTES,
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
pub use error::DomainValidationError;
pub use event::{
    AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, EventEnvelope,
    EventKind, SecurityPlane, SourceClass, TrustLevel,
};
pub use interfaces::{
    Authorized, AuthorizedAvatarAction, AuthorizedStreamAction, AuthorizedToolAction, AvatarAction,
    AvatarAdapter, DecisionEngine, EngineError, EngineErrorKind, EngineFuture, GeneratedReply,
    SpeechArtifact, SpeechRequest, StreamAction, StreamAdapter, ThinkingEngine, ToolAction,
    ToolAdapter, TtsEngine, authorize_avatar_action, authorize_stream_action,
    authorize_tool_action,
};
pub use reflex::{
    AttentionTarget, BackendIdentity, FallbackReason, REFLEX_SCHEMA_VERSION, ReflexDecision,
    ResponseRoute, RouteDecision,
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
