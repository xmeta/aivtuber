#![forbid(unsafe_code)]

//! Provider-neutral domain types and interfaces for the AI VTuber runtime.

mod error;
mod event;
mod interfaces;
mod reflex;

pub use error::DomainValidationError;
pub use event::{
    AuthorizationContext, AuthorizationMethod, Capability, EventEnvelope, EventKind, SecurityPlane,
    SourceClass, TrustLevel, EVENT_SCHEMA_VERSION,
};
pub use interfaces::{
    authorize_avatar_action, authorize_stream_action, Authorized, AuthorizedAvatarAction,
    AuthorizedStreamAction, AvatarAction, AvatarAdapter, DecisionEngine, EngineError,
    EngineErrorKind, EngineFuture, GeneratedReply, ReflexRequest, SpeechArtifact, SpeechRequest,
    StreamAction, StreamAdapter, ThinkingEngine, TtsEngine,
};
pub use reflex::{
    AttentionTarget, BackendIdentity, FallbackReason, ReflexDecision, ResponseRoute,
    RouteDecision, REFLEX_SCHEMA_VERSION,
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
