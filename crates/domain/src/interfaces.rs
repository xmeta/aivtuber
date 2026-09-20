use crate::{Capability, EventEnvelope, ReflexDecision};
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
    pub capability: Capability,
    pub action: String,
    pub arguments: BTreeMap<String, serde_json::Value>,
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
    fn execute<'a>(&'a self, action: &'a AvatarAction) -> EngineFuture<'a, ()>;
}

pub trait StreamAdapter: Send + Sync {
    fn execute<'a>(&'a self, action: &'a StreamAction) -> EngineFuture<'a, ()>;
}
