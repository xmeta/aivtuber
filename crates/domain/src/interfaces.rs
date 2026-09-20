use crate::{
    AuthorizationContext, Capability, EventEnvelope, ReflexDecision,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
};

pub type EngineFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, EngineError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineErrorKind {
    Timeout,
    Unavailable,
    RateLimited,
    Overloaded,
    Authentication,
    Unauthorized,
    InvalidRequest,
    Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    pub kind: EngineErrorKind,
    pub message: String,
}

impl EngineError {
    pub fn new(kind: EngineErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl Error for EngineError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflexRequest {
    pub event: EventEnvelope,
    pub state: BTreeMap<String, serde_json::Value>,
    pub candidate_asset_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedReply {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechRequest {
    pub text: String,
    pub style: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechArtifact {
    pub audio_ref: String,
    pub duration_ms: u64,
    pub viseme_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvatarAction {
    pub action: String,
    pub parameters: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamAction {
    pub action: String,
    pub arguments: BTreeMap<String, serde_json::Value>,
}

/// Privileged action that has passed deterministic capability authorization.
///
/// Fields are intentionally private. Callers must use the authorization
/// helpers in this module rather than deserializing or constructing this type.
#[derive(Debug, Clone, PartialEq)]
pub struct Authorized<T> {
    action: T,
    principal: String,
    capability: Capability,
}

impl<T> Authorized<T> {
    pub fn action(&self) -> &T {
        &self.action
    }

    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }
}

pub type AuthorizedAvatarAction = Authorized<AvatarAction>;
pub type AuthorizedStreamAction = Authorized<StreamAction>;

pub fn authorize_avatar_action(
    authorization: &AuthorizationContext,
    action: AvatarAction,
) -> Result<AuthorizedAvatarAction, EngineError> {
    authorize(authorization, Capability::AvatarControl, action)
}

pub fn authorize_stream_action(
    authorization: &AuthorizationContext,
    action: StreamAction,
) -> Result<AuthorizedStreamAction, EngineError> {
    authorize(authorization, Capability::ObsControl, action)
}

fn authorize<T>(
    authorization: &AuthorizationContext,
    required: Capability,
    action: T,
) -> Result<Authorized<T>, EngineError> {
    if authorization.principal.trim().is_empty() {
        return Err(EngineError::new(
            EngineErrorKind::Unauthorized,
            "authorization principal is empty",
        ));
    }
    if !authorization.capabilities.contains(&required) {
        return Err(EngineError::new(
            EngineErrorKind::Unauthorized,
            format!("missing required capability: {required:?}"),
        ));
    }

    Ok(Authorized {
        action,
        principal: authorization.principal.clone(),
        capability: required,
    })
}

pub trait DecisionEngine: Send + Sync {
    fn decide<'a>(&'a self, request: &'a ReflexRequest) -> EngineFuture<'a, ReflexDecision>;
}

pub trait ThinkingEngine: Send + Sync {
    fn generate<'a>(&'a self, request: &'a ReflexRequest) -> EngineFuture<'a, GeneratedReply>;
}

pub trait TtsEngine: Send + Sync {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact>;
}

pub trait AvatarAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a AuthorizedAvatarAction) -> EngineFuture<'a, ()>;
}

pub trait StreamAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a AuthorizedStreamAction) -> EngineFuture<'a, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthorizationMethod;
    use std::collections::BTreeSet;

    fn authorization(capability: Capability) -> AuthorizationContext {
        AuthorizationContext {
            principal: "operator:local".to_owned(),
            method: AuthorizationMethod::OperatorHotkey,
            capabilities: BTreeSet::from([capability]),
        }
    }

    #[test]
    fn stream_action_requires_obs_capability() {
        let action = StreamAction {
            action: "scene.set".to_owned(),
            arguments: BTreeMap::new(),
        };
        let auth = authorization(Capability::PerformerStop);

        let error = authorize_stream_action(&auth, action).expect_err("must reject");
        assert_eq!(error.kind, EngineErrorKind::Unauthorized);
    }

    #[test]
    fn authorized_stream_action_records_principal_and_capability() {
        let action = StreamAction {
            action: "scene.set".to_owned(),
            arguments: BTreeMap::new(),
        };
        let auth = authorization(Capability::ObsControl);

        let authorized = authorize_stream_action(&auth, action).expect("authorized");
        assert_eq!(authorized.principal(), "operator:local");
        assert_eq!(authorized.capability(), Capability::ObsControl);
    }
}
