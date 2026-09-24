#![forbid(unsafe_code)]

mod outputs;
mod routing;
mod semantic;

pub use outputs::*;
pub use routing::*;
pub use semantic::*;

use aivtuber_domain::{
    AuthenticatedControlCommand, AuthorizedStreamAction, EngineError, EventEnvelope,
};
use aivtuber_runtime::{
    AudioPlaybackCommand, AudioPlaybackSink, AvatarPlaybackCommand, AvatarPlaybackSink,
    CachedAssetSelection, CachedPerformer, CachedPlaybackError, CachedPlaybackOutcome,
    CachedPlaybackTiming, ContentAdmitDecision, ControlOutcome, FastPathPreloadReport,
    LocalVisemeStore, RuntimeError, SecurityRuntime,
};
use aivtuber_scheduler::{Scheduler, Status};
use std::error::Error;
use std::fmt;

pub trait AudioOutput: Send {
    fn execute(&mut self, command: &AudioPlaybackCommand) -> Result<(), EngineError>;
}

pub trait AvatarOutput: Send {
    fn connect(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
    fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError>;
}

pub trait StreamOutput: Send {
    fn connect(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn execute(&mut self, action: &AuthorizedStreamAction) -> Result<(), EngineError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterHealth {
    pub audio_error: Option<String>,
    pub avatar_error: Option<String>,
    pub stream_error: Option<String>,
}

#[derive(Debug)]
pub enum AppError {
    Runtime(RuntimeError),
    Playback(CachedPlaybackError),
    Routing(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "{error}"),
            Self::Playback(error) => write!(f, "{error}"),
            Self::Routing(message) => write!(f, "routing failed: {message}"),
        }
    }
}

impl Error for AppError {}

impl From<RuntimeError> for AppError {
    fn from(value: RuntimeError) -> Self {
        Self::Runtime(value)
    }
}

impl From<CachedPlaybackError> for AppError {
    fn from(value: CachedPlaybackError) -> Self {
        Self::Playback(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentProcessOutcome {
    pub admission: ContentAdmitDecision,
    pub playback: Option<CachedPlaybackOutcome>,
}

#[derive(Debug, Default)]
struct PendingAudio {
    commands: Vec<AudioPlaybackCommand>,
}

impl AudioPlaybackSink for PendingAudio {
    fn schedule_audio(&mut self, command: AudioPlaybackCommand) {
        self.commands.push(command);
    }
}

#[derive(Debug, Default)]
struct PendingAvatar {
    commands: Vec<AvatarPlaybackCommand>,
}

impl AvatarPlaybackSink for PendingAvatar {
    fn schedule_avatar(&mut self, command: AvatarPlaybackCommand) {
        self.commands.push(command);
    }
}

pub struct ProductionApp<R>
where
    R: RoutePlanner,
{
    security: SecurityRuntime,
    performer: CachedPerformer,
    visemes: LocalVisemeStore,
    router: R,
    audio: Box<dyn AudioOutput>,
    avatar: Box<dyn AvatarOutput>,
    stream: Box<dyn StreamOutput>,
    pending_audio: PendingAudio,
    pending_avatar: PendingAvatar,
    max_dispatch_lateness_ms: u64,
    health: AdapterHealth,
}

impl<R> ProductionApp<R>
where
    R: RoutePlanner,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        security: SecurityRuntime,
        performer: CachedPerformer,
        visemes: LocalVisemeStore,
        router: R,
        audio: Box<dyn AudioOutput>,
        avatar: Box<dyn AvatarOutput>,
        stream: Box<dyn StreamOutput>,
        max_dispatch_lateness_ms: u64,
    ) -> Self {
        Self {
            security,
            performer,
            visemes,
            router,
            audio,
            avatar,
            stream,
            pending_audio: PendingAudio::default(),
            pending_avatar: PendingAvatar::default(),
            max_dispatch_lateness_ms,
            health: AdapterHealth::default(),
        }
    }

    pub fn startup(&mut self) -> Result<FastPathPreloadReport, AppError> {
        let report = self.performer.index_and_preload_fast_path()?;
        self.try_connect_avatar();
        self.try_connect_stream();
        Ok(report)
    }

    pub fn maintain_adapters(&mut self) {
        self.try_connect_avatar();
        self.try_connect_stream();
    }

    pub fn health(&self) -> &AdapterHealth {
        &self.health
    }

    pub fn security(&self) -> &SecurityRuntime {
        &self.security
    }

    pub fn performer(&self) -> &CachedPerformer {
        &self.performer
    }

    pub fn router(&self) -> &R {
        &self.router
    }

    pub fn router_mut(&mut self) -> &mut R {
        &mut self.router
    }

    pub fn process_content_bytes(
        &mut self,
        raw: &[u8],
        at_ms: u64,
        seed: u64,
    ) -> Result<ContentProcessOutcome, AppError> {
        let admission = self.security.admit_content_bytes(raw, at_ms)?;
        if admission != ContentAdmitDecision::Queued {
            return Ok(ContentProcessOutcome {
                admission,
                playback: None,
            });
        }

        let event = self.security.pop_content().ok_or_else(|| {
            AppError::Routing("queued content was unavailable for dispatch".to_owned())
        })?;
        let playback = self.handle_event(event, at_ms, seed)?;
        self.tick(at_ms);
        Ok(ContentProcessOutcome {
            admission,
            playback,
        })
    }

    fn handle_event(
        &mut self,
        mut event: EventEnvelope,
        at_ms: u64,
        seed: u64,
    ) -> Result<Option<CachedPlaybackOutcome>, AppError> {
        match self.router.route(&event)? {
            PlaybackRoute::Silent => Ok(None),
            PlaybackRoute::Intent(intent) => {
                event
                    .payload
                    .insert("intent".to_owned(), serde_json::Value::String(intent));
                let outcome = self.performer.handle_event(
                    &event,
                    at_ms,
                    seed,
                    &self.visemes,
                    &mut self.pending_audio,
                    &mut self.pending_avatar,
                )?;
                Ok(Some(outcome))
            }
            PlaybackRoute::AssetId(asset_id) => {
                let outcome = self.performer.handle_asset_id(
                    &event,
                    &asset_id,
                    CachedPlaybackTiming { at_ms, seed },
                    &self.visemes,
                    &mut self.pending_audio,
                    &mut self.pending_avatar,
                )?;
                Ok(Some(outcome))
            }
            PlaybackRoute::AssetIdentity {
                asset_id,
                asset_identity,
            } => {
                let outcome = self.performer.handle_asset_identity(
                    &event,
                    CachedAssetSelection {
                        asset_id: &asset_id,
                        expected_identity: &asset_identity,
                    },
                    CachedPlaybackTiming { at_ms, seed },
                    &self.visemes,
                    &mut self.pending_audio,
                    &mut self.pending_avatar,
                )?;
                Ok(Some(outcome))
            }
        }
    }

    pub fn tick(&mut self, now_ms: u64) {
        self.performer.scheduler_mut().advance_to(now_ms);
        self.dispatch_audio(now_ms);
        self.dispatch_avatar(now_ms);
    }

    fn dispatch_audio(&mut self, now_ms: u64) {
        let mut keep = Vec::new();
        let mut commands = std::mem::take(&mut self.pending_audio.commands);
        commands.sort_by_key(|command| command.at_ms);

        for command in commands {
            if command.at_ms > now_ms {
                keep.push(command);
                continue;
            }
            if !command_is_live(
                self.performer.scheduler(),
                command.generation,
                command.at_ms,
                now_ms,
                self.max_dispatch_lateness_ms,
            ) {
                continue;
            }
            if self.security.is_muted() {
                continue;
            }

            match self.audio.execute(&command) {
                Ok(()) => self.health.audio_error = None,
                Err(error) => self.health.audio_error = Some(error.to_string()),
            }
        }
        self.pending_audio.commands = keep;
    }

    fn dispatch_avatar(&mut self, now_ms: u64) {
        let mut keep = Vec::new();
        let mut commands = std::mem::take(&mut self.pending_avatar.commands);
        commands.sort_by_key(AvatarPlaybackCommand::at_ms);

        for command in commands {
            let at_ms = command.at_ms();
            if at_ms > now_ms {
                keep.push(command);
                continue;
            }
            if !command_is_live(
                self.performer.scheduler(),
                avatar_generation(&command),
                at_ms,
                now_ms,
                self.max_dispatch_lateness_ms,
            ) {
                continue;
            }

            match self.avatar.execute(&command) {
                Ok(()) => self.health.avatar_error = None,
                Err(error) => self.health.avatar_error = Some(error.to_string()),
            }
        }
        self.pending_avatar.commands = keep;
    }

    pub fn handle_control(
        &mut self,
        command: &AuthenticatedControlCommand,
        at_ms: u64,
    ) -> Result<ControlOutcome, AppError> {
        let outcome = self.security.handle_control_with_scheduler(
            command,
            at_ms,
            self.performer.scheduler_mut(),
        )?;
        if matches!(outcome, ControlOutcome::Stopped { .. }) {
            self.purge_cancelled_pending();
        }
        self.tick(at_ms);
        Ok(outcome)
    }

    pub fn shutdown(&mut self, at_ms: u64) {
        self.performer.scheduler_mut().stop_all(at_ms);
        self.purge_cancelled_pending();
        self.tick(at_ms);
    }

    pub fn execute_stream_action(
        &mut self,
        action: &AuthorizedStreamAction,
    ) -> Result<(), EngineError> {
        match self.stream.execute(action) {
            Ok(()) => {
                self.health.stream_error = None;
                Ok(())
            }
            Err(error) => {
                self.health.stream_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn try_connect_avatar(&mut self) {
        match self.avatar.connect() {
            Ok(()) => self.health.avatar_error = None,
            Err(error) => self.health.avatar_error = Some(error.to_string()),
        }
    }

    fn try_connect_stream(&mut self) {
        match self.stream.connect() {
            Ok(()) => self.health.stream_error = None,
            Err(error) => self.health.stream_error = Some(error.to_string()),
        }
    }

    fn purge_cancelled_pending(&mut self) {
        let scheduler = self.performer.scheduler();
        self.pending_audio
            .commands
            .retain(|command| command_not_cancelled(scheduler, command.generation, command.at_ms));
        self.pending_avatar.commands.retain(|command| {
            command_not_cancelled(scheduler, avatar_generation(command), command.at_ms())
        });
    }
}

fn avatar_generation(command: &AvatarPlaybackCommand) -> u64 {
    match command {
        AvatarPlaybackCommand::Expression { generation, .. }
        | AvatarPlaybackCommand::Gesture { generation, .. }
        | AvatarPlaybackCommand::Gaze { generation, .. }
        | AvatarPlaybackCommand::Viseme { generation, .. } => *generation,
    }
}

fn command_not_cancelled(scheduler: &Scheduler, generation: u64, command_at_ms: u64) -> bool {
    scheduler.items().iter().any(|item| {
        item.plan.generation == generation
            && matches!(item.status, Status::Queued | Status::Playing)
            && item
                .cancel_at_ms
                .is_none_or(|cancel_at_ms| command_at_ms < cancel_at_ms)
    })
}

fn command_is_live(
    scheduler: &Scheduler,
    generation: u64,
    command_at_ms: u64,
    now_ms: u64,
    max_lateness_ms: u64,
) -> bool {
    if command_at_ms > now_ms || now_ms.saturating_sub(command_at_ms) > max_lateness_ms {
        return false;
    }
    command_not_cancelled(scheduler, generation, command_at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
    use aivtuber_domain::{
        AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION, EngineErrorKind,
        LocalControlIngress, OperatorCommandInput, SecurityPlane, SourceClass, TrustLevel,
    };
    use aivtuber_reflex::{
        HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig,
        ReflexPipeline, TransportError,
    };
    use aivtuber_runtime::{CachedPlaybackConfig, SecurityRuntimeConfig};
    use aivtuber_scheduler::SchedulerConfig;
    use aivtuber_telemetry::SecretRedactor;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    #[derive(Clone)]
    struct FixedAssetRoute(&'static str);

    impl RoutePlanner for FixedAssetRoute {
        fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
            Ok(PlaybackRoute::AssetId(self.0.to_owned()))
        }
    }

    #[derive(Clone)]
    struct FixedIdentityRoute {
        asset_id: &'static str,
        asset_identity: &'static str,
    }

    impl RoutePlanner for FixedIdentityRoute {
        fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
            Ok(PlaybackRoute::AssetIdentity {
                asset_id: self.asset_id.to_owned(),
                asset_identity: self.asset_identity.to_owned(),
            })
        }
    }

    #[derive(Debug, Default)]
    struct FixedEmbedding;

    impl QueryEmbeddingProvider for FixedEmbedding {
        fn embedding(&mut self, _event: &EventEnvelope) -> Result<Vec<f32>, AppError> {
            Ok(vec![1.0, 0.0, 0.0])
        }
    }

    #[derive(Debug)]
    struct OneShotTransport {
        response: Mutex<Option<HttpResponse>>,
    }

    impl HttpTransport for OneShotTransport {
        fn post_json(
            &self,
            _endpoint: &str,
            _bearer_token: &str,
            _body: &[u8],
            _timeout: Duration,
        ) -> Result<HttpResponse, TransportError> {
            self.response
                .lock()
                .expect("response")
                .take()
                .ok_or_else(|| TransportError::Unavailable("response already consumed".to_owned()))
        }
    }

    #[derive(Clone, Default)]
    struct RecordingAudio {
        commands: Arc<Mutex<Vec<AudioPlaybackCommand>>>,
    }

    impl AudioOutput for RecordingAudio {
        fn execute(&mut self, command: &AudioPlaybackCommand) -> Result<(), EngineError> {
            self.commands
                .lock()
                .expect("audio log")
                .push(command.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingAvatar {
        commands: Arc<Mutex<Vec<AvatarPlaybackCommand>>>,
    }

    impl AvatarOutput for RecordingAvatar {
        fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
            self.commands
                .lock()
                .expect("avatar log")
                .push(command.clone());
            Ok(())
        }
    }

    struct FailingAudio {
        attempts: Arc<AtomicUsize>,
    }

    impl AudioOutput for FailingAudio {
        fn execute(&mut self, _command: &AudioPlaybackCommand) -> Result<(), EngineError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "audio device unavailable",
            ))
        }
    }

    struct RecoveringAvatar {
        connect_attempts: Arc<AtomicUsize>,
        execute_attempts: Arc<AtomicUsize>,
        connected: bool,
    }

    impl AvatarOutput for RecoveringAvatar {
        fn connect(&mut self) -> Result<(), EngineError> {
            let attempt = self.connect_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(EngineError::new(
                    EngineErrorKind::Unavailable,
                    "avatar adapter unavailable",
                ));
            }
            self.connected = true;
            Ok(())
        }

        fn execute(&mut self, _command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
            self.execute_attempts.fetch_add(1, Ordering::SeqCst);
            if self.connected {
                Ok(())
            } else {
                Err(EngineError::new(
                    EngineErrorKind::Unavailable,
                    "avatar adapter disconnected",
                ))
            }
        }
    }

    #[derive(Default)]
    struct FailingStream;

    impl StreamOutput for FailingStream {
        fn connect(&mut self) -> Result<(), EngineError> {
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "OBS unavailable",
            ))
        }

        fn execute(&mut self, _action: &AuthorizedStreamAction) -> Result<(), EngineError> {
            Err(EngineError::new(
                EngineErrorKind::Unavailable,
                "OBS unavailable",
            ))
        }
    }

    fn pack_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/starter-reaction-pack")
    }

    fn runtime_compatibility() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("example-voice-v1".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }

    fn chat_event(sequence: u64) -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("evt-{sequence}"),
            correlation_id: "corr-app-e2e".to_owned(),
            sequence,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "test-chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: aivtuber_domain::EventKind::ChatMessage,
            actor_id: Some("viewer:test".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([
                (
                    "text".to_owned(),
                    serde_json::Value::String("hello".to_owned()),
                ),
                (
                    "intent".to_owned(),
                    serde_json::Value::String("reaction.agree".to_owned()),
                ),
            ]),
        }
    }

    fn performer() -> CachedPerformer {
        CachedPerformer::new(
            AssetStore::new(pack_root().join("descriptors"), runtime_compatibility()),
            Scheduler::new(SchedulerConfig {
                min_reaction_spacing_ms: 0,
            }),
            CachedPlaybackConfig {
                recent_variant_window: 1,
            },
        )
    }

    fn reflex_router() -> ReflexRoutePlanner<FixedEmbedding> {
        let mut assets = AssetStore::new(pack_root().join("descriptors"), runtime_compatibility());
        assets.index_local().expect("index Performance Assets");
        let index = build_semantic_index_from_asset_store(
            &assets,
            &AssetSemanticIndexConfig {
                retriever_version: "semantic-test-v1".to_owned(),
                embedding_model: "starter-semantic".to_owned(),
                embedding_model_version: "1".to_owned(),
            },
        )
        .expect("semantic index");

        let choice = |value: &str| {
            json!({
                "type": "choice",
                "choice": value,
                "confidence": 0.95,
                "probabilities": {"primary": 0.95, "other": 0.05}
            })
        };
        let response = HttpResponse {
            status: 200,
            body: serde_json::to_vec(&json!({
                "model": "jev-test-v1",
                "answers": {
                    "response_route": choice("cached"),
                    "reaction_family": choice("agree"),
                    "gesture_family": choice("nod_small"),
                    "attention_target": choice("camera"),
                    "interrupt": {"type": "noul", "noul": 0.05},
                    "cache_reuse": {"type": "noul", "noul": 0.95},
                    "importance": {
                        "type": "score",
                        "score": 0.5,
                        "confidence": 0.9,
                        "legend": {"0": "low", "1": "high"},
                        "probabilities": {"0": 0.5, "1": 0.5}
                    },
                    "emotion_intensity": {
                        "type": "score",
                        "score": 0.4,
                        "confidence": 0.9,
                        "legend": {"0": "low", "1": "high"},
                        "probabilities": {"0": 0.6, "1": 0.4}
                    },
                    "selected_candidate": choice("reaction.agree.01")
                },
                "usage": {"input_tokens": 64, "output_tokens": 8}
            }))
            .expect("Jev response"),
        };
        let adapter = JevAdapter::with_transport(
            JevAdapterConfig {
                deadline: Duration::from_millis(100),
                max_attempts: 1,
                initial_backoff: Duration::ZERO,
                ..JevAdapterConfig::default()
            },
            JevApiKey::new("test-key").expect("key"),
            Arc::new(OneShotTransport {
                response: Mutex::new(Some(response)),
            }),
        )
        .expect("Jev adapter");
        let pipeline =
            ReflexPipeline::new(index, adapter, PolicyConfig::default(), 2).expect("pipeline");
        ReflexRoutePlanner::new(pipeline, FixedEmbedding)
    }

    fn app<R>(
        router: R,
        audio: Box<dyn AudioOutput>,
        avatar: Box<dyn AvatarOutput>,
        stream: Box<dyn StreamOutput>,
    ) -> ProductionApp<R>
    where
        R: RoutePlanner,
    {
        let security = SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig {
                min_reaction_spacing_ms: 0,
            },
            SecretRedactor::default(),
            Some("safe cached reaction".to_owned()),
        )
        .expect("security runtime");
        ProductionApp::new(
            security,
            performer(),
            LocalVisemeStore::new(pack_root()),
            router,
            audio,
            avatar,
            stream,
            250,
        )
    }

    fn stop_command() -> AuthenticatedControlCommand {
        let secret = [0x45_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::PerformerStop]),
            ControlSecret::new(secret),
        )
        .expect("control ingress");
        ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-stop".to_owned(),
                    correlation_id: "corr-stop".to_owned(),
                    sequence: 99,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: "performer.stop".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated stop")
    }

    #[test]
    fn selected_cached_asset_runs_through_production_composition() {
        let audio = RecordingAudio::default();
        let audio_log = Arc::clone(&audio.commands);
        let avatar = RecordingAvatar::default();
        let avatar_log = Arc::clone(&avatar.commands);
        let mut app = app(
            FixedAssetRoute("reaction.agree.01"),
            Box::new(audio),
            Box::new(avatar),
            Box::new(FailingStream),
        );

        let report = app.startup().expect("startup");
        assert_eq!(report.indexed.usable, 6);
        assert!(app.health().stream_error.is_some());

        let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
        let outcome = app
            .process_content_bytes(&raw, 0, 7)
            .expect("production path");
        assert_eq!(outcome.admission, ContentAdmitDecision::Queued);
        let playback = outcome.playback.expect("cached playback");
        assert_eq!(playback.asset_id, "reaction.agree.01");

        app.tick(playback.plan.start_at_ms.saturating_add(150));

        assert_eq!(audio_log.lock().expect("audio log").len(), 1);
        assert!(!avatar_log.lock().expect("avatar log").is_empty());
        assert!(
            app.performer().scheduler().now_ms() >= playback.plan.start_at_ms.saturating_add(150)
        );
    }

    #[test]
    fn reflex_reuse_selection_resolves_through_asset_store_and_scheduler() {
        let audio = RecordingAudio::default();
        let audio_log = Arc::clone(&audio.commands);
        let avatar = RecordingAvatar::default();
        let avatar_log = Arc::clone(&avatar.commands);
        let mut app = app(
            reflex_router(),
            Box::new(audio),
            Box::new(avatar),
            Box::new(NoopStreamOutput),
        );
        app.startup().expect("startup");

        let raw = serde_json::to_vec(&chat_event(20)).expect("event json");
        let outcome = app
            .process_content_bytes(&raw, 0, 29)
            .expect("reflex production path");
        let playback = outcome.playback.expect("cached reuse playback");

        assert_eq!(playback.asset_id, "reaction.agree.01");
        let (selected_id, selected_identity, metadata) = {
            let record = app.router().last_record().expect("decision record");
            (
                record.executed.asset_id.clone(),
                record.executed.asset_identity.clone(),
                record.evidence.retrieval.metadata.clone(),
            )
        };
        assert_eq!(selected_id.as_deref(), Some("reaction.agree.01"));
        assert_eq!(metadata.embedding_model, "starter-semantic@1");
        assert_eq!(metadata.asset_compiler_version, "0.1.0");
        assert!(
            metadata
                .index_version
                .starts_with("asset-semantic-fnv1a64-")
        );

        let actual_identity = app
            .performer()
            .assets()
            .local_entry("reaction.agree.01")
            .expect("playback asset")
            .identity
            .stable_key();
        assert_eq!(selected_identity.as_deref(), Some(actual_identity.as_str()));

        app.tick(playback.plan.start_at_ms.saturating_add(150));
        assert_eq!(audio_log.lock().expect("audio log").len(), 1);
        assert!(!avatar_log.lock().expect("avatar log").is_empty());
    }

    #[test]
    fn stale_semantic_identity_is_rejected_before_playback() {
        let mut app = app(
            FixedIdentityRoute {
                asset_id: "reaction.agree.01",
                asset_identity: "stale-identity",
            },
            Box::new(RecordingAudio::default()),
            Box::new(RecordingAvatar::default()),
            Box::new(NoopStreamOutput),
        );
        app.startup().expect("startup");

        let raw = serde_json::to_vec(&chat_event(22)).expect("event json");
        let error = app
            .process_content_bytes(&raw, 0, 31)
            .expect_err("stale selection must not play");

        match error {
            AppError::Playback(CachedPlaybackError::StaleAssetIdentity { asset_id, .. }) => {
                assert_eq!(asset_id, "reaction.agree.01");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn failed_audio_command_is_dropped_instead_of_replayed() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut app = app(
            FixedAssetRoute("reaction.agree.01"),
            Box::new(FailingAudio {
                attempts: Arc::clone(&attempts),
            }),
            Box::new(RecordingAvatar::default()),
            Box::new(NoopStreamOutput),
        );
        app.startup().expect("startup");

        let raw = serde_json::to_vec(&chat_event(2)).expect("event json");
        let outcome = app
            .process_content_bytes(&raw, 0, 11)
            .expect("production path");

        let playback = outcome.playback.expect("cached playback");
        let dispatch_at = playback.plan.start_at_ms.saturating_add(150);
        app.tick(dispatch_at);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(app.health().audio_error.is_some());

        app.tick(dispatch_at.saturating_add(10));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn avatar_reconnect_does_not_replay_failed_due_commands() {
        let connect_attempts = Arc::new(AtomicUsize::new(0));
        let execute_attempts = Arc::new(AtomicUsize::new(0));
        let avatar = RecoveringAvatar {
            connect_attempts: Arc::clone(&connect_attempts),
            execute_attempts: Arc::clone(&execute_attempts),
            connected: false,
        };
        let mut app = app(
            FixedAssetRoute("reaction.agree.01"),
            Box::new(RecordingAudio::default()),
            Box::new(avatar),
            Box::new(NoopStreamOutput),
        );

        app.startup().expect("startup continues in degraded mode");
        assert_eq!(connect_attempts.load(Ordering::SeqCst), 1);
        assert!(app.health().avatar_error.is_some());

        let raw = serde_json::to_vec(&chat_event(21)).expect("event json");
        let outcome = app
            .process_content_bytes(&raw, 0, 23)
            .expect("production path");
        let playback = outcome.playback.expect("cached playback");
        let dispatch_at = playback.plan.start_at_ms.saturating_add(150);

        app.tick(dispatch_at);
        let failed_attempts = execute_attempts.load(Ordering::SeqCst);
        assert!(failed_attempts > 0);
        assert!(app.health().avatar_error.is_some());

        app.maintain_adapters();
        assert_eq!(connect_attempts.load(Ordering::SeqCst), 2);
        assert!(app.health().avatar_error.is_none());

        app.tick(dispatch_at.saturating_add(10));
        assert_eq!(execute_attempts.load(Ordering::SeqCst), failed_attempts);
    }

    #[test]
    fn authenticated_stop_purges_future_commands_on_shared_scheduler() {
        let audio = RecordingAudio::default();
        let audio_log = Arc::clone(&audio.commands);
        let avatar = RecordingAvatar::default();
        let avatar_log = Arc::clone(&avatar.commands);
        let mut app = app(
            FixedAssetRoute("reaction.agree.01"),
            Box::new(audio),
            Box::new(avatar),
            Box::new(NoopStreamOutput),
        );
        app.startup().expect("startup");

        let raw = serde_json::to_vec(&chat_event(3)).expect("event json");
        let outcome = app.process_content_bytes(&raw, 0, 13).expect("event");

        let playback = outcome.playback.expect("cached playback");
        let audio_before = audio_log.lock().expect("audio log").len();
        let avatar_before = avatar_log.lock().expect("avatar log").len();
        let stop_at = playback.plan.start_at_ms.saturating_add(10);

        let stopped = app
            .handle_control(&stop_command(), stop_at)
            .expect("authenticated stop");
        assert_eq!(stopped, ControlOutcome::Stopped { cancelled: 1 });

        app.tick(playback.plan.end_at_ms().saturating_add(100));
        assert_eq!(audio_log.lock().expect("audio log").len(), audio_before);
        assert_eq!(avatar_log.lock().expect("avatar log").len(), avatar_before);
        assert_eq!(
            app.performer().scheduler().items()[0].status,
            Status::Cancelled
        );
    }
}
