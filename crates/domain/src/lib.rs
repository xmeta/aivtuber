#![forbid(unsafe_code)]

//! Provider-neutral domain types and interfaces for the AI VTuber runtime.

mod error;
mod event;
mod interfaces;
mod reflex;

pub use error::DomainValidationError;
pub use event::{
    AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, EventEnvelope,
    EventKind, SecurityPlane, SourceClass, TrustLevel,
};
pub use interfaces::{
    Authorized, AuthorizedAvatarAction, AuthorizedStreamAction, AvatarAction, AvatarAdapter,
    DecisionEngine, EngineError, EngineErrorKind, EngineFuture, GeneratedReply, ReflexRequest,
    SpeechArtifact, SpeechRequest, StreamAction, StreamAdapter, ThinkingEngine, TtsEngine,
    authorize_avatar_action, authorize_stream_action,
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
