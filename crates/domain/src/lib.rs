#![forbid(unsafe_code)]

//! Provider-neutral domain types and interfaces for the AI VTuber runtime.

mod control;
mod error;
mod event;
mod interfaces;
mod reflex;

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
    ReflexRequest, SpeechArtifact, SpeechProgress, SpeechProgressSink, SpeechRequest, StreamAction,
    StreamAdapter, ThinkingEngine, ToolAction, ToolAdapter, TtsBackendIdentity, TtsEngine,
    authorize_avatar_action, authorize_stream_action, authorize_tool_action,
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
