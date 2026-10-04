use crate::{
    AuthenticatedControl, BackendIdentity, Capability, ReflexDecision, ReflexRequest,
    ThinkingRequest,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, error::Error, fmt, future::Future, pin::Pin, time::Duration};

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

pub const MAX_TOOL_OBSERVATION_BYTES: usize = 32 * 1024;
pub const MAX_TOOL_DESCRIPTOR_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_IDENTITY_BYTES: usize = 256;
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;

fn validate_tool_identity(value: &str, label: &str) -> Result<(), EngineError> {
    if value.is_empty() || value.len() > MAX_TOOL_IDENTITY_BYTES {
        return Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            format!("{label} must be non-empty and at most {MAX_TOOL_IDENTITY_BYTES} bytes"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    name: String,
    description: Option<String>,
    input_schema: serde_json::Value,
    schema_fingerprint: String,
}

impl ToolDescriptor {
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        input_schema: serde_json::Value,
        schema_fingerprint: impl Into<String>,
    ) -> Result<Self, EngineError> {
        let name = name.into();
        let schema_fingerprint = schema_fingerprint.into();
        validate_tool_identity(&name, "tool name")?;
        validate_tool_identity(&schema_fingerprint, "tool schema fingerprint")?;
        if description
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TOOL_DESCRIPTION_BYTES)
        {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "tool description exceeds the bounded metadata limit",
            ));
        }
        let bytes = serde_json::to_vec(&input_schema).map_err(|error| {
            EngineError::new(
                EngineErrorKind::InvalidRequest,
                format!("tool input schema is not serializable: {error}"),
            )
        })?;
        if bytes.len() > MAX_TOOL_DESCRIPTOR_BYTES {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "tool input schema exceeds the bounded descriptor limit",
            ));
        }
        Ok(Self {
            name,
            description,
            input_schema,
            schema_fingerprint,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn input_schema(&self) -> &serde_json::Value {
        &self.input_schema
    }

    pub fn schema_fingerprint(&self) -> &str {
        &self.schema_fingerprint
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultClass {
    Success,
    ToolError,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolExecutionResult {
    server: String,
    tool: String,
    operation: String,
    class: ToolResultClass,
    content: Option<serde_json::Value>,
    schema_fingerprint: Option<String>,
    latency_ms: u64,
}

impl ToolExecutionResult {
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        operation: impl Into<String>,
        class: ToolResultClass,
        content: Option<serde_json::Value>,
        schema_fingerprint: Option<String>,
        latency_ms: u64,
    ) -> Result<Self, EngineError> {
        let server = server.into();
        let tool = tool.into();
        let operation = operation.into();
        validate_tool_identity(&server, "tool server identity")?;
        validate_tool_identity(&tool, "tool identity")?;
        validate_tool_identity(&operation, "tool operation")?;
        if let Some(fingerprint) = &schema_fingerprint {
            validate_tool_identity(fingerprint, "tool schema fingerprint")?;
        }
        if let Some(value) = &content {
            let bytes = serde_json::to_vec(value).map_err(|error| {
                EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    format!("tool observation is not serializable: {error}"),
                )
            })?;
            if bytes.len() > MAX_TOOL_OBSERVATION_BYTES {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "tool observation exceeds the bounded result limit",
                ));
            }
        }
        Ok(Self {
            server,
            tool,
            operation,
            class,
            content,
            schema_fingerprint,
            latency_ms,
        })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn class(&self) -> ToolResultClass {
        self.class
    }

    pub fn content(&self) -> Option<&serde_json::Value> {
        self.content.as_ref()
    }

    pub fn schema_fingerprint(&self) -> Option<&str> {
        self.schema_fingerprint.as_deref()
    }

    pub fn latency_ms(&self) -> u64 {
        self.latency_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolVerificationOutcome {
    Verified,
    Failed,
    NotProven,
    Cancelled,
    Stale,
}

impl ToolVerificationOutcome {
    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCommitFence {
    pub expected_generation: u64,
    pub observed_generation: u64,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolVerificationRule {
    JsonEquals {
        pointer: String,
        expected: serde_json::Value,
    },
}

pub fn verify_tool_result(
    result: &ToolExecutionResult,
    expected_tool: &str,
    expected_operation: &str,
    fence: ToolCommitFence,
    rule: &ToolVerificationRule,
) -> ToolVerificationOutcome {
    if fence.cancelled {
        return ToolVerificationOutcome::Cancelled;
    }
    if fence.expected_generation != fence.observed_generation {
        return ToolVerificationOutcome::Stale;
    }
    if result.tool() != expected_tool || result.operation() != expected_operation {
        return ToolVerificationOutcome::Failed;
    }
    if result.class() != ToolResultClass::Success {
        return ToolVerificationOutcome::Failed;
    }
    let Some(content) = result.content() else {
        return ToolVerificationOutcome::NotProven;
    };
    match rule {
        ToolVerificationRule::JsonEquals { pointer, expected } => {
            if content.pointer(pointer) == Some(expected) {
                ToolVerificationOutcome::Verified
            } else {
                ToolVerificationOutcome::NotProven
            }
        }
    }
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
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply>;

    fn generate_with_timeout<'a>(
        &'a self,
        request: &'a ThinkingRequest,
        _timeout: Duration,
    ) -> EngineFuture<'a, GeneratedReply> {
        self.generate(request)
    }

    fn identity(&self) -> BackendIdentity;
}

pub trait TtsEngine: Send + Sync {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact>;

    fn synthesize_with_timeout<'a>(
        &'a self,
        request: &'a SpeechRequest,
        _timeout: Duration,
    ) -> EngineFuture<'a, SpeechArtifact> {
        self.synthesize(request)
    }

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

    fn synthesize_streaming_with_timeout<'a>(
        &'a self,
        request: &'a SpeechRequest,
        sink: &'a mut dyn SpeechProgressSink,
        _timeout: Duration,
    ) -> Option<EngineFuture<'a, SpeechArtifact>> {
        self.synthesize_streaming(request, sink)
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
    fn discover<'a>(&'a self) -> EngineFuture<'a, Vec<ToolDescriptor>>;
    fn execute<'a>(
        &'a self,
        action: &'a AuthorizedToolAction,
    ) -> EngineFuture<'a, ToolExecutionResult>;
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
    fn untrusted_tool_metadata_is_bounded_before_core_state() {
        let descriptor_error = ToolDescriptor::new(
            "fixture",
            Some("x".repeat(MAX_TOOL_DESCRIPTION_BYTES + 1)),
            serde_json::json!({"type": "object"}),
            "a".repeat(64),
        )
        .expect_err("oversized description must fail closed");
        assert_eq!(descriptor_error.kind, EngineErrorKind::InvalidRequest);

        let result_error = ToolExecutionResult::new(
            "server",
            "x".repeat(MAX_TOOL_IDENTITY_BYTES + 1),
            "read",
            ToolResultClass::Success,
            Some(serde_json::json!({"ok": true})),
            None,
            1,
        )
        .expect_err("oversized tool identity must fail closed");
        assert_eq!(result_error.kind, EngineErrorKind::InvalidRequest);
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
