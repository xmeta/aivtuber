use crate::{AuthenticatedControl, BackendIdentity, Capability, EventEnvelope, ReflexDecision};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, error::Error, fmt, future::Future, pin::Pin};

pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, EngineError>> + Send + 'a>>;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechProgress {
    pub sequence: u64,
    pub audio_ref: String,
    pub duration_ms: u64,
    pub viseme_ref: Option<String>,
    pub final_chunk: bool,
}

pub trait SpeechProgressSink: Send {
    fn push(&mut self, progress: SpeechProgress) -> Result<(), EngineError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsBackendIdentity {
    pub backend: BackendIdentity,
    pub voice_model: Option<String>,
    pub viseme_mapping: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolAction {
    pub tool: String,
    pub operation: String,
    pub arguments: BTreeMap<String, serde_json::Value>,
}

/// Privileged action that has passed authenticated ingress and deterministic
/// capability authorization.
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
pub type AuthorizedToolAction = Authorized<ToolAction>;

pub fn authorize_avatar_action(
    authority: &AuthenticatedControl,
    action: AvatarAction,
) -> Result<AuthorizedAvatarAction, EngineError> {
    authorize(authority, Capability::AvatarControl, action)
}

pub fn authorize_stream_action(
    authority: &AuthenticatedControl,
    action: StreamAction,
) -> Result<AuthorizedStreamAction, EngineError> {
    authorize(authority, Capability::ObsControl, action)
}

pub fn authorize_tool_action(
    authority: &AuthenticatedControl,
    action: ToolAction,
) -> Result<AuthorizedToolAction, EngineError> {
    authorize(authority, Capability::ToolGrant, action)
}

fn authorize<T>(
    authority: &AuthenticatedControl,
    required: Capability,
    action: T,
) -> Result<Authorized<T>, EngineError> {
    if !authority.has_capability(required) {
        return Err(EngineError::new(
            EngineErrorKind::Unauthorized,
            format!("missing authenticated capability: {required:?}"),
        ));
    }

    Ok(Authorized {
        action,
        principal: authority.principal().to_owned(),
        capability: required,
    })
}

pub trait DecisionEngine: Send + Sync {
    fn decide<'a>(&'a self, request: &'a ReflexRequest) -> EngineFuture<'a, ReflexDecision>;
}

pub trait ThinkingEngine: Send + Sync {
    fn generate<'a>(&'a self, request: &'a ReflexRequest) -> EngineFuture<'a, GeneratedReply>;
    fn identity(&self) -> BackendIdentity;
}

pub trait TtsEngine: Send + Sync {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact>;

    /// Optional streaming synthesis. Backends that support incremental output
    /// push stable-reference progress records and still return one final artifact.
    /// Returning None explicitly selects the buffered synthesize path.
    fn synthesize_streaming<'a>(
        &'a self,
        _request: &'a SpeechRequest,
        _sink: &'a mut dyn SpeechProgressSink,
    ) -> Option<EngineFuture<'a, SpeechArtifact>> {
        None
    }

    fn identity(&self) -> TtsBackendIdentity;
}

pub trait AvatarAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a AuthorizedAvatarAction) -> EngineFuture<'a, ()>;
}

pub trait StreamAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a AuthorizedStreamAction) -> EngineFuture<'a, ()>;
}

pub trait ToolAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a AuthorizedToolAction) -> EngineFuture<'a, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthorizationMethod, ControlSecret, LocalControlIngress, OperatorCommandInput};
    use std::collections::BTreeSet;

    fn authenticated(capability: Capability) -> AuthenticatedControl {
        let action = match capability {
            Capability::PerformerMute => "performer.mute",
            Capability::PerformerStop => "performer.stop",
            Capability::ObsControl => "obs.control",
            Capability::AvatarControl => "avatar.control",
            Capability::MemoryAdmin => "memory.admin",
            Capability::ToolGrant => "tool.grant",
        };
        let secret = [7_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-hotkey",
            "operator:local",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([capability]),
            ControlSecret::new(secret),
        )
        .expect("trusted ingress");
        ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-control".to_owned(),
                    correlation_id: "corr-control".to_owned(),
                    sequence: 1,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: action.to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone()
    }

    #[test]
    fn stream_action_requires_authenticated_obs_capability() {
        let action = StreamAction {
            action: "scene.set".to_owned(),
            arguments: BTreeMap::new(),
        };
        let authority = authenticated(Capability::PerformerStop);

        let error = authorize_stream_action(&authority, action).expect_err("must reject");
        assert_eq!(error.kind, EngineErrorKind::Unauthorized);
    }

    #[test]
    fn authorized_stream_action_records_principal_and_capability() {
        let action = StreamAction {
            action: "scene.set".to_owned(),
            arguments: BTreeMap::new(),
        };
        let authority = authenticated(Capability::ObsControl);

        let authorized = authorize_stream_action(&authority, action).expect("authorized");
        assert_eq!(authorized.principal(), "operator:local");
        assert_eq!(authorized.capability(), Capability::ObsControl);
    }

    #[test]
    fn tool_action_requires_authenticated_tool_grant() {
        let action = ToolAction {
            tool: "example".to_owned(),
            operation: "run".to_owned(),
            arguments: BTreeMap::new(),
        };
        let authority = authenticated(Capability::AvatarControl);
        assert!(authorize_tool_action(&authority, action).is_err());
    }
}
