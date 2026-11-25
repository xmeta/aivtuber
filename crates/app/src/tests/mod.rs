use super::*;
use aivtuber_adaptation::{ActorPseudonymizer, PromotionPolicy, WorkingMemoryConfig};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility, load_asset_file};
use aivtuber_domain::{
    AuthorizationMethod, BackendIdentity, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
    EngineErrorKind, EngineFuture, GeneratedReply, LocalControlIngress, OperatorCommandInput,
    PrivacyClass, ReflexContext, RetrievalSnapshot, SecurityPlane, SourceClass, SpeechArtifact,
    SpeechProgress, SpeechProgressSink, SpeechRequest, ThinkingContextSource, ThinkingEngine,
    TrustLevel, TtsBackendIdentity, TtsEngine,
};
use aivtuber_generative::{PerformanceCompiler, PerformanceCompilerConfig};
use aivtuber_reflex::{
    HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig,
    ReflexPipeline, TransportError,
};
use aivtuber_runtime::{CachedPlaybackConfig, SecurityRuntimeConfig};

mod affect_judge_status_tests;
mod aivis_cloud_tts_tests;
mod audio_backend_tests;
mod avatar_control_tests;
mod chat_burst_tests;
mod chat_ingress_tests;
mod crowd_routing_tests;
mod face_settle_tests;
mod initiative_tests;
mod irodori_tts_tests;
mod local_thinking_fallback_tests;
mod memory_seed_tests;
mod memory_writer_tests;
mod panel_url_tests;
mod persona_tests;
mod phrase_commit_tests;
mod recent_turns_tests;
mod scene_context_tests;
mod security_fixture_evidence_tests;
mod session_log_tests;
mod shutdown_tests;
mod silent_chat_drop_tests;
mod speech_lifecycle_tests;
mod streaming_thinking_tests;
mod thinking_cost_cap_tests;
mod thinking_warmup_tests;
mod voice_control_tests;
mod voicevox_tts_tests;
use aivtuber_scheduler::SchedulerConfig;
use aivtuber_telemetry::SecretRedactor;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Clone, Copy)]
struct FixedSilentRoute;

impl RoutePlanner for FixedSilentRoute {
    fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        Ok(PlaybackRoute::Silent)
    }
}

#[derive(Clone)]
struct FixedAssetRoute(&'static str);

impl RoutePlanner for FixedAssetRoute {
    fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        Ok(PlaybackRoute::AssetId(self.0.to_owned()))
    }
}

#[derive(Clone)]
struct FixedShadowDecision(ShadowDecision);

impl ShadowPolicy for FixedShadowDecision {
    fn evaluate(&mut self, _input: &ShadowPolicyInput<'_>) -> Result<ShadowDecision, AppError> {
        Ok(self.0.clone())
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

#[derive(Clone)]
struct FixedGenerateRoute {
    reply_context: ReflexContext,
    fallback_variant_group: Option<String>,
}

impl RoutePlanner for FixedGenerateRoute {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        Ok(PlaybackRoute::Generate(Box::new(GenerationRoute {
            source_event: event.clone(),
            thinking: aivtuber_domain::ThinkingRequest::from_event(
                event,
                event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("event"),
                PrivacyClass::Pseudonymous,
                self.reply_context.clone(),
                RetrievalSnapshot::default(),
                Vec::new(),
            ),
            routing_reason: aivtuber_generative::GenerationRoutingReason::ExplicitLlmRoute,
            intent: "generated.reply".to_owned(),
            style: None,
            fallback_variant_group: self.fallback_variant_group.clone(),
        })))
    }
}

#[derive(Clone)]
struct GenerateChatSilentDonation {
    generate: FixedGenerateRoute,
}

impl RoutePlanner for GenerateChatSilentDonation {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        if event.kind == aivtuber_domain::EventKind::ChatDonation {
            Ok(PlaybackRoute::Silent)
        } else {
            self.generate.route(event)
        }
    }
}

#[derive(Clone)]
struct MockThinking {
    reply: String,
    failure: Option<EngineErrorKind>,
}

impl ThinkingEngine for MockThinking {
    fn generate<'a>(
        &'a self,
        _request: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        Box::pin(async move {
            if let Some(kind) = self.failure {
                Err(EngineError::new(kind, "mock thinking failure"))
            } else {
                Ok(GeneratedReply {
                    text: self.reply.clone(),
                    usage: None,
                })
            }
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "mock-thinking".to_owned(),
            model_alias: Some("mock-model".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

#[derive(Debug, Default)]
struct BlockingGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}

impl BlockingGate {
    fn wait_until_entered(&self) {
        for _ in 0..1_000 {
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("blocking provider was never entered");
    }

    fn release(&self) {
        *self.released.lock().expect("blocking gate") = true;
        self.wake.notify_all();
    }
}

#[derive(Clone)]
struct BlockingThinking {
    gate: Arc<BlockingGate>,
    reply: String,
}

impl ThinkingEngine for BlockingThinking {
    fn generate<'a>(
        &'a self,
        _request: &'a aivtuber_domain::ThinkingRequest,
    ) -> EngineFuture<'a, GeneratedReply> {
        Box::pin(async move {
            self.gate.entered.store(true, Ordering::Release);
            let released = self.gate.released.lock().expect("blocking gate");
            let (released, wait) = self
                .gate
                .wake
                .wait_timeout_while(released, Duration::from_secs(2), |released| !*released)
                .expect("blocking gate wait");
            if wait.timed_out() && !*released {
                return Err(EngineError::new(
                    EngineErrorKind::Timeout,
                    "blocking thinking watchdog timeout",
                ));
            }
            Ok(GeneratedReply {
                text: self.reply.clone(),
                usage: None,
            })
        })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "blocking-thinking".to_owned(),
            model_alias: Some("blocked-model".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

#[derive(Clone, Default)]
struct MockTts {
    texts: Arc<Mutex<Vec<String>>>,
    failure: Option<EngineErrorKind>,
}

impl TtsEngine for MockTts {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        Box::pin(async move {
            self.texts
                .lock()
                .expect("tts log")
                .push(request.text.clone());
            if let Some(kind) = self.failure {
                Err(EngineError::new(kind, "mock tts failure"))
            } else {
                Ok(SpeechArtifact {
                    audio_ref: "audio://generated/mock.opus".to_owned(),
                    duration_ms: 760,
                    viseme_ref: Some("viseme/reaction-agree-01.json".to_owned()),
                })
            }
        })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "mock-tts".to_owned(),
                model_alias: Some("mock-voice".to_owned()),
                model_version: Some("1".to_owned()),
            },
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        }
    }
}

#[derive(Clone, Default)]
struct PartialStreamingTts;

impl TtsEngine for PartialStreamingTts {
    fn synthesize<'a>(&'a self, _request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        Box::pin(async {
            Err(EngineError::new(
                EngineErrorKind::Backend,
                "buffered path must not be used",
            ))
        })
    }

    fn synthesize_streaming<'a>(
        &'a self,
        _request: &'a SpeechRequest,
        sink: &'a mut dyn SpeechProgressSink,
    ) -> Option<EngineFuture<'a, SpeechArtifact>> {
        sink.push(SpeechProgress {
            sequence: 1,
            audio_ref: "audio://generated/partial.opus".to_owned(),
            duration_ms: 100,
            viseme_ref: None,
            final_chunk: false,
        })
        .expect("partial streaming progress");
        Some(Box::pin(async {
            Err(EngineError::new(
                EngineErrorKind::Timeout,
                "stream ended before final chunk",
            ))
        }))
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "partial-streaming-tts".to_owned(),
                model_alias: None,
                model_version: None,
            },
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        }
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
    repeating: bool,
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
        let mut response = self.response.lock().expect("response");
        if self.repeating {
            return response
                .clone()
                .ok_or_else(|| TransportError::Unavailable("no response configured".to_owned()));
        }
        response
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

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aivtuber-app-{name}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
            ..aivtuber_scheduler::SchedulerConfig::default()
        }),
        CachedPlaybackConfig {
            recent_variant_window: 1,
            ..CachedPlaybackConfig::default()
        },
    )
}

fn reflex_router() -> ReflexRoutePlanner<FixedEmbedding> {
    reflex_router_with_route("cached")
}

fn reflex_router_with_route(response_route: &str) -> ReflexRoutePlanner<FixedEmbedding> {
    reflex_router_with_transport(response_route, false)
}

/// `repeating` answers every Jev call with the same response (replay of
/// several events); otherwise the response is consumed by the first call.
fn reflex_router_with_transport(
    response_route: &str,
    repeating: bool,
) -> ReflexRoutePlanner<FixedEmbedding> {
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
                "response_route": choice(response_route),
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
            repeating,
            response: Mutex::new(Some(response)),
        }),
    )
    .expect("Jev adapter");
    let pipeline =
        ReflexPipeline::new(index, adapter, PolicyConfig::default(), 2).expect("pipeline");
    ReflexRoutePlanner::new(pipeline, FixedEmbedding)
}

/// Issue #54: reflex planner wired to answer `template` with the curated
/// template pack installed.
fn template_planner() -> ReflexRoutePlanner<FixedEmbedding> {
    reflex_router_with_route("template").with_template_pack(test_template_pack())
}

fn test_template_pack() -> TemplatePack {
    TemplatePack::new(vec![ResponseTemplate {
        id: "thanks.donation".to_owned(),
        version: "curated-v1".to_owned(),
        text: "{name}さん、ありがとう！".to_owned(),
        slots: vec!["name".to_owned()],
    }])
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
            ..aivtuber_scheduler::SchedulerConfig::default()
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

#[derive(Clone, Default)]
struct CapturingThinking {
    requests: Arc<std::sync::Mutex<Vec<aivtuber_domain::ThinkingRequest>>>,
}

impl aivtuber_domain::ThinkingEngine for CapturingThinking {
    fn generate<'a>(
        &'a self,
        request: &'a aivtuber_domain::ThinkingRequest,
    ) -> aivtuber_domain::EngineFuture<'a, aivtuber_domain::GeneratedReply> {
        self.requests
            .lock()
            .expect("captured requests")
            .push(request.clone());
        Box::pin(async {
            Ok(aivtuber_domain::GeneratedReply {
                text: "ok".to_owned(),
                usage: None,
            })
        })
    }

    fn identity(&self) -> aivtuber_domain::BackendIdentity {
        aivtuber_domain::BackendIdentity {
            name: "capturing-thinking".to_owned(),
            model_alias: Some("capture".to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

fn adaptation_runtime(policy: PromotionPolicy) -> AdaptationRuntime {
    AdaptationRuntime::new(
        WorkingMemory::new(
            WorkingMemoryConfig {
                max_entries: 8,
                working_ttl_ms: 100,
                durable_ttl_ms: 1_000,
                max_claim_bytes: 128,
                max_topic_bytes: 64,
                ..WorkingMemoryConfig::default()
            },
            ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("test pseudonymizer"),
        )
        .expect("working memory"),
        AdaptationEngine::new(policy, "test-adaptation-v1", 42).expect("adaptation engine"),
    )
}

fn generative_runtime<Thinking, Tts>(thinking: Thinking, tts: Tts) -> GenerativeRuntime
where
    Thinking: ThinkingEngine + 'static,
    Tts: TtsEngine + 'static,
{
    generative_runtime_with_config(thinking, tts, GenerationExecutionConfig::default())
}

fn generative_runtime_with_config<Thinking, Tts>(
    thinking: Thinking,
    tts: Tts,
    execution: GenerationExecutionConfig,
) -> GenerativeRuntime
where
    Thinking: ThinkingEngine + 'static,
    Tts: TtsEngine + 'static,
{
    let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
        compiler_version: "0.1.0".to_owned(),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
        expression_preset: Some("speaking.neutral".to_owned()),
        expression_intensity: 0.4,
    })
    .expect("compiler");
    GenerativeRuntime::with_execution_config(
        GenerativePipeline::new(Arc::new(thinking), Arc::new(tts), compiler),
        execution,
    )
    .expect("generation execution config")
}

fn generation_app<Thinking, Tts>(
    router: FixedGenerateRoute,
    thinking: Thinking,
    tts: Tts,
    redactor: SecretRedactor,
    cached_reaction: Option<String>,
    audio: RecordingAudio,
    avatar: RecordingAvatar,
) -> ProductionApp<FixedGenerateRoute>
where
    Thinking: ThinkingEngine + 'static,
    Tts: TtsEngine + 'static,
{
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        redactor,
        cached_reaction,
    )
    .expect("security runtime");
    ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        router,
        Box::new(audio),
        Box::new(avatar),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(thinking, tts))
}

/// #105: the thinking request carries retained memory and its class, and
/// nothing from another viewer.
#[test]
fn generated_reply_carries_retained_memory_with_its_epistemic_class() {
    let thinking = CapturingThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        thinking,
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");

    // A trusted system observation of the game state, claimed in the
    // event's own words, is the only way to a verified fact.
    let fact_text = "the boss is weak to fire";
    let mut fact = chat_event(40);
    fact.event_id = "evt-fact".to_owned();
    fact.source = "runtime:game".to_owned();
    fact.source_class = SourceClass::System;
    fact.trust_level = TrustLevel::Trusted;
    fact.kind = aivtuber_domain::EventKind::SystemHealth;
    fact.plane = SecurityPlane::System;
    fact.actor_id = None;
    fact.payload = BTreeMap::from([
        ("text".to_owned(), json!(fact_text)),
        ("topic".to_owned(), json!("game-x")),
    ]);
    let authority = memory_admin_authority();
    app.remember_durable_memory(&fact, Some(&authority), fact_text, Some("game-x"), 0)
        .expect("verified fact admitted");

    // Another viewer's claim on the same topic must never reach this reply.
    let mut other = chat_event(41);
    other.event_id = "evt-other".to_owned();
    other.actor_id = Some("viewer:other".to_owned());
    other.payload = BTreeMap::from([("text".to_owned(), json!("the boss is immune to fire"))]);
    app.remember_working_memory(&other, "the boss is immune to fire", Some("game-x"), 0)
        .expect("other viewer claim stored");

    // The current viewer's own claim is in scope. Retention normalizes
    // whitespace and case, so the retained form is what reaches the
    // request.
    let own_text = "i think the boss is weak to fire";
    let mut own = chat_event(42);
    own.event_id = "evt-own".to_owned();
    own.payload = BTreeMap::from([("text".to_owned(), json!(own_text))]);
    app.remember_working_memory(&own, own_text, Some("game-x"), 0)
        .expect("own claim stored");

    // An operator's private record, stored as a valid control-plane
    // envelope: it carries no actor, but actorless is not shared, so it
    // must not reach the provider request (#105 review).
    let private_text = "the operator's private note";
    let mut operator = chat_event(43);
    operator.event_id = "evt-op".to_owned();
    operator.source = "local-control".to_owned();
    operator.source_class = SourceClass::Operator;
    operator.plane = SecurityPlane::Control;
    operator.trust_level = TrustLevel::Trusted;
    operator.kind = aivtuber_domain::EventKind::OperatorCommand;
    operator.actor_id = None;
    operator.authorization = Some(aivtuber_domain::AuthorizationContext {
        principal: "operator:test".to_owned(),
        method: AuthorizationMethod::OperatorHotkey,
        capabilities: BTreeSet::from([Capability::MemoryAdmin]),
    });
    operator.payload = BTreeMap::from([("text".to_owned(), json!(private_text))]);
    app.remember_working_memory(&operator, private_text, Some("game-x"), 0)
        .expect("operator memory stored");

    // A public claim with no actor at all: a valid envelope, but nobody is
    // attributable for it, so it is not this reply's context either.
    let anonymous_text = "somebody said the boss is immune to fire";
    let mut anonymous = chat_event(44);
    anonymous.event_id = "evt-anon".to_owned();
    anonymous.actor_id = None;
    anonymous.payload = BTreeMap::from([("text".to_owned(), json!(anonymous_text))]);
    app.remember_working_memory(&anonymous, anonymous_text, Some("game-x"), 0)
        .expect("anonymous claim stored");

    let mut event = chat_event(30);
    event.payload.insert("topic".to_owned(), json!("game-x"));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 0, 30)
        .expect("generation submission");
    wait_for_generation_completion(&mut app, 0);

    let captured = requests.lock().expect("captured requests");
    let request = captured.last().expect("thinking request");
    request
        .validate()
        .expect("the enriched request stays valid");
    let context = &request.curated_context;
    assert_eq!(
        context.len(),
        2,
        "the verified fact and the speaker's own claim, never the other viewer's: {context:?}"
    );
    assert!(
        context
            .iter()
            .all(|item| item.source == ThinkingContextSource::Memory),
        "retained memory is labelled as memory: {context:?}"
    );
    let fact = context
        .iter()
        .find(|item| item.text == fact_text)
        .expect("the verified fact is in context");
    assert_eq!(
        fact.epistemic_class,
        Some(aivtuber_domain::EpistemicClass::VerifiedFact),
        "a trusted bound observation keeps its class instead of becoming a bare sentence"
    );
    assert_eq!(fact.privacy, PrivacyClass::Public);
    assert_eq!(fact.trust_level, TrustLevel::Trusted);
    let own = context
        .iter()
        .find(|item| item.text == own_text)
        .expect("the speaker's own claim is in context");
    assert_eq!(
        own.epistemic_class,
        Some(aivtuber_domain::EpistemicClass::ViewerClaim),
        "a viewer claim is carried as a claim, not as fact"
    );
    assert_eq!(own.privacy, PrivacyClass::Pseudonymous);
    assert!(
        !context
            .iter()
            .any(|item| item.text == "the boss is immune to fire"),
        "another viewer's claim is out of the reply's scope: {context:?}"
    );

    // The exact provider request: the OpenAI-compatible adapter serializes
    // this request, so what the wire would carry is what this asserts on.
    let wire = serde_json::to_string(request).expect("serialize thinking request");
    assert!(
        !wire.contains(private_text),
        "private operator content never reaches the provider request"
    );
    assert!(
        !wire.contains(anonymous_text),
        "an unattributable public claim is not another party's context"
    );
    assert!(
        wire.contains("verified_fact"),
        "the retained class travels on the wire: {wire}"
    );

    // Release the capture lock before the worker runs again: the engine
    // takes it on the generation thread.
    drop(captured);

    // Replay: the same retained state produces the same request context.
    let mut event = chat_event(30);
    event.payload.insert("topic".to_owned(), json!("game-x"));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 0, 30)
        .expect("second generation submission");
    wait_for_generation_completion(&mut app, 0);
    let replay = requests.lock().expect("captured requests");
    assert_eq!(
        replay[replay.len() - 1].curated_context,
        replay[replay.len() - 2].curated_context,
        "bounded retrieval over an unchanged retained state is deterministic"
    );
}

/// #105: the context budget drops a claim whole; it never shortens one.
#[test]
fn oversized_memory_drops_whole_instead_of_truncating() {
    let thinking = CapturingThinking::default();
    let requests = Arc::clone(&thinking.requests);
    let memory = WorkingMemory::new(
        WorkingMemoryConfig {
            max_entries: 8,
            working_ttl_ms: 1_000,
            durable_ttl_ms: 60_000,
            max_claim_bytes: 4_096,
            max_topic_bytes: 64,
            ..WorkingMemoryConfig::default()
        },
        ActorPseudonymizer::new("test-v1", [0x42; 32]).expect("test pseudonymizer"),
    )
    .expect("working memory");
    let adaptation = AdaptationRuntime::new(
        memory,
        AdaptationEngine::new(PromotionPolicy::default(), "test-adaptation-v1", 42)
            .expect("adaptation engine"),
    );
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        thinking,
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_adaptation(adaptation);
    app.startup().expect("startup");

    // Three claims that individually fit the request's per-item text bound
    // but not its total context budget: the prefix rule keeps the newest
    // two whole and leaves the oldest out entirely.
    let claims = (0..3)
        .map(|index| format!("{index}{}", "x".repeat(2_999)))
        .collect::<Vec<_>>();
    let authority = memory_admin_authority();
    for (index, claim) in claims.iter().enumerate() {
        let mut fact = chat_event(50 + index as u64);
        fact.event_id = format!("evt-big-{index}");
        fact.source = "runtime:game".to_owned();
        fact.source_class = SourceClass::System;
        fact.trust_level = TrustLevel::Trusted;
        fact.kind = aivtuber_domain::EventKind::SystemHealth;
        fact.plane = SecurityPlane::System;
        fact.actor_id = None;
        fact.payload = BTreeMap::from([("text".to_owned(), json!(claim))]);
        app.remember_durable_memory(&fact, Some(&authority), claim, Some("game-x"), 0)
            .expect("bounded claim admitted");
    }

    let mut event = chat_event(30);
    event.payload.insert("topic".to_owned(), json!("game-x"));
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 0, 30)
        .expect("generation submission");
    wait_for_generation_completion(&mut app, 0);

    let requests = requests.lock().expect("captured requests");
    let context = &requests.last().expect("thinking request").curated_context;
    assert_eq!(
        context.len(),
        2,
        "the request budget holds two 3 KB claims, not three: {:?}",
        context
            .iter()
            .map(|item| item.text.len())
            .collect::<Vec<_>>()
    );
    for item in context {
        assert!(
            claims.iter().any(|claim| claim == &item.text),
            "an included claim is the retained claim, never a shortened prefix (len {})",
            item.text.len()
        );
    }
    assert!(
        !context.iter().any(|item| item.text == claims[0]),
        "the oldest claim is the one the prefix budget leaves out"
    );
}

fn wait_for_generation_completion<R>(app: &mut ProductionApp<R>, now_ms: u64)
where
    R: RoutePlanner,
{
    for _ in 0..1_000 {
        app.tick(now_ms);
        let active = app
            .generation_cancellation_registry()
            .and_then(|registry| registry.active_event_id());
        let snapshot = app
            .generation_execution_snapshot()
            .expect("generation execution snapshot");
        if active.is_none() && snapshot.is_quiescent() {
            app.tick(now_ms);
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "generation did not quiesce: {:?}",
        app.generation_execution_snapshot()
    );
}

fn last_scheduled_plan<R>(app: &ProductionApp<R>) -> aivtuber_scheduler::PlannedPerformance
where
    R: RoutePlanner,
{
    app.performer()
        .scheduler()
        .items()
        .last()
        .expect("scheduled generation/fallback")
        .plan
        .clone()
}

fn memory_admin_authority() -> AuthenticatedControl {
    let secret = [0x33_u8; 32];
    let ingress = LocalControlIngress::new(
        "local-test",
        "operator:memory",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::MemoryAdmin]),
        ControlSecret::new(secret),
    )
    .expect("memory control ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-memory-admin".to_owned(),
                correlation_id: "corr-memory-admin".to_owned(),
                sequence: 98,
                observed_at: "2026-09-24T00:00:00Z".to_owned(),
                action: "memory.admin".to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .expect("authenticated memory admin")
        .authority()
        .clone()
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
fn reflex_llm_route_builds_typed_generation_request() {
    let mut router = reflex_router_with_route("llm");
    let event = chat_event(24);
    let route = router.route(&event).expect("LLM route");
    let PlaybackRoute::Generate(route) = route else {
        panic!("expected generation route");
    };

    assert_eq!(route.source_event, event);
    assert_eq!(route.thinking.input.text, "hello");
    assert_eq!(route.thinking.context, ReflexContext::default());
    assert_eq!(route.thinking.retrieval.candidates.len(), 2);
    assert_eq!(
        route.routing_reason,
        aivtuber_generative::GenerationRoutingReason::ExplicitLlmRoute
    );
    route.thinking.validate().expect("typed thinking request");
}

#[test]
fn production_memory_api_requires_gate_for_durable_public_chat() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");
    let event = chat_event(25);

    let working = app
        .remember_working_memory(&event, "  Viewer likes Rust ", Some("coding"), 10)
        .expect("working memory");
    assert_eq!(
        working.retention,
        aivtuber_adaptation::RetentionClass::Working
    );
    assert_ne!(
        working.source.pseudonymous_actor_id.as_deref(),
        event.actor_id.as_deref()
    );

    let denied = app
        .remember_durable_memory(&event, None, "viewer likes Rust", Some("coding"), 10)
        .expect_err("public chat without authority must not become durable");
    assert!(denied.to_string().contains("security gate"));

    let authority = memory_admin_authority();
    let durable = app
        .remember_durable_memory(
            &event,
            Some(&authority),
            "viewer likes Rust",
            Some("coding"),
            20,
        )
        .expect("memory-admin durable write");
    assert_eq!(
        durable.retention,
        aivtuber_adaptation::RetentionClass::Durable
    );
    assert_eq!(
        durable.write_decision,
        aivtuber_adaptation::MemoryGateDecision::AllowedMemoryAdmin
    );
    assert_eq!(app.adaptation().expect("adaptation").memory().len(), 2);
}

#[test]
fn viewer_forget_command_erases_only_the_requesters_memory_before_routing() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");
    let own = chat_event(40);
    let mut other = chat_event(41);
    other.actor_id = Some("viewer:other".to_owned());
    app.remember_working_memory(&own, "viewer likes Rust", Some("coding"), 10)
        .expect("own memory");
    app.remember_working_memory(&other, "other likes Go", Some("coding"), 11)
        .expect("other memory");

    let mut command = chat_event(42);
    command
        .payload
        .insert("text".to_owned(), serde_json::json!("!forgetme"));
    command.payload.remove("intent");
    let raw = serde_json::to_vec(&command).expect("serialize");
    let outcome = app
        .process_content_bytes(&raw, 50, 1)
        .expect("command consumed");
    assert!(outcome.playback.is_none(), "the command is never routed");
    let report = outcome.memory_erasure.expect("erasure report");
    assert_eq!(report.entries_removed, 1);
    assert_eq!(
        report.requester,
        aivtuber_adaptation::ErasureRequester::Viewer
    );
    let memory = app.adaptation().expect("adaptation").memory();
    assert_eq!(memory.len(), 1, "the other viewer's memory is untouched");
    assert_eq!(memory.retention_metrics().erasures_completed, 1);
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|record| record.decision.starts_with("erase_erased")),
        "the operator sees an audit record"
    );

    // A second command inside the window is rate limited and changes nothing.
    app.remember_working_memory(&own, "viewer again", Some("coding"), 60)
        .expect("own memory");
    let mut again = command.clone();
    again.event_id = "evt-43".to_owned();
    again.sequence = 43;
    let raw = serde_json::to_vec(&again).expect("serialize");
    let limited = app
        .process_content_bytes(&raw, 70, 1)
        .expect("command consumed")
        .memory_erasure
        .expect("report");
    assert_eq!(
        limited.outcome,
        aivtuber_adaptation::ErasureOutcome::RateLimited
    );
    assert_eq!(app.adaptation().expect("adaptation").memory().len(), 2);
}

#[test]
fn operator_memory_erase_needs_memory_admin_and_reports_remnants() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");
    let event = chat_event(50);
    app.remember_working_memory(&event, "viewer likes Rust", Some("coding"), 10)
        .expect("memory");

    let admin = memory_admin_authority();
    let report = app
        .erase_memory_operator(
            "op-erase-1",
            &admin,
            Some(("test-chat", "viewer:test")),
            &[],
            100,
        )
        .expect("operator erase");
    assert_eq!(report.entries_removed, 1);
    assert_eq!(
        report.unprovable_remnants,
        aivtuber_adaptation::ErasureRemnant::ALL.to_vec()
    );
    assert!(app.adaptation().expect("adaptation").memory().is_empty());
}

#[test]
fn supersession_evidence_reaches_the_link_store_through_production_app() {
    // The #105 update path has to be reachable from the composition root,
    // not only from the evaluation harness: a production caller that owns
    // update evidence declares it here, through the same permit gate as
    // the claim itself.
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");
    let authority = memory_admin_authority();
    let old = chat_event(40);
    let new = chat_event(41);
    let old_entry = app
        .remember_durable_memory(
            &old,
            Some(&authority),
            "viewer disliked the game",
            Some("game-x"),
            10,
        )
        .expect("first durable write");

    // Without the gate no supersession evidence may reach the store: the
    // write is denied and nothing is linked.
    let denied = app
        .remember_durable_memory_superseding(
            &new,
            None,
            "viewer enjoyed the game",
            Some("game-y"),
            20,
            &["evt-40"],
        )
        .expect_err("public chat without authority must not declare supersession");
    assert!(denied.to_string().contains("security gate"));
    assert_eq!(
        app.adaptation().expect("adaptation").memory().links().len(),
        0,
        "a denied write mints no link"
    );

    let update = app
        .remember_durable_memory_superseding(
            &new,
            Some(&authority),
            "viewer enjoyed the game",
            Some("game-y"),
            20,
            &["evt-40"],
        )
        .expect("superseding durable write");
    assert!(
        app.adaptation()
            .expect("adaptation")
            .memory()
            .links()
            .iter()
            .any(|link| {
                link.kind == aivtuber_adaptation::MemoryLinkKind::Supersedes
                    && link.from == update.memory_id
                    && link.to == old_entry.memory_id
            }),
        "a production superseding write creates the Supersedes edge"
    );

    // An unknown target refuses the whole write: nothing stored, no link.
    let third = chat_event(42);
    let error = app
        .remember_durable_memory_superseding(
            &third,
            Some(&authority),
            "an unrelated claim",
            Some("game-z"),
            30,
            &["evt-missing"],
        )
        .expect_err("an unknown supersession target refuses the write");
    assert!(
        error.to_string().contains("retained durable memory"),
        "{error}"
    );
    let memory = app.adaptation().expect("adaptation").memory();
    assert_eq!(memory.len(), 2, "the refused write stored nothing");
    assert_eq!(memory.links().len(), 1, "the refused write minted nothing");
}

#[test]
fn causal_evidence_reaches_the_link_store_through_production_app() {
    // A cause is caller-owned evidence, so the composition root has to
    // expose the same path a supersession gets (#105): the permit decides
    // first, then the target must resolve to a live retained claim, and
    // only then is the edge minted.
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_adaptation(adaptation_runtime(PromotionPolicy::default()));
    app.startup().expect("startup");
    let authority = memory_admin_authority();
    let cause = chat_event(50);
    let effect = chat_event(51);
    let cause_entry = app
        .remember_durable_memory(
            &cause,
            Some(&authority),
            "the stream crashed mid-song",
            Some("game-x"),
            10,
        )
        .expect("first durable write");

    // Without the gate no causal evidence may reach the store: the write
    // is denied, so neither the claim nor its edge exists.
    let denied = app
        .remember_durable_memory_with_evidence(
            &effect,
            None,
            "the viewer asked to restart the song",
            Some("game-x"),
            20,
            &[],
            &["evt-50"],
        )
        .expect_err("public chat without authority must not declare a cause");
    assert!(denied.to_string().contains("security gate"));
    assert_eq!(
        app.adaptation().expect("adaptation").memory().links().len(),
        0,
        "a denied write mints no link"
    );

    let effect_entry = app
        .remember_durable_memory_with_evidence(
            &effect,
            Some(&authority),
            "the viewer asked to restart the song",
            Some("game-x"),
            20,
            &[],
            &["evt-50"],
        )
        .expect("causal durable write");
    assert!(
        app.adaptation()
            .expect("adaptation")
            .memory()
            .links()
            .iter()
            .any(|link| {
                link.kind == aivtuber_adaptation::MemoryLinkKind::CausedBy
                    && link.from == effect_entry.memory_id
                    && link.to == cause_entry.memory_id
            }),
        "a production causal write creates the CausedBy edge"
    );

    // An unknown cause refuses the whole write, so a caller cannot record
    // an account of its own reasoning against evidence that never existed.
    let third = chat_event(52);
    let error = app
        .remember_durable_memory_with_evidence(
            &third,
            Some(&authority),
            "an unrelated claim",
            Some("game-z"),
            30,
            &[],
            &["evt-missing"],
        )
        .expect_err("an unknown cause refuses the write");
    assert!(
        error.to_string().contains("retained durable memory"),
        "{error}"
    );
    assert_eq!(
        app.adaptation().expect("adaptation").memory().len(),
        2,
        "the refused write stored nothing"
    );
}

#[test]
fn generated_asset_promotion_and_rollback_run_through_production_app() {
    let dir = TestDir::new("generated-promotion");
    let performer = CachedPerformer::new(
        AssetStore::new(dir.path(), runtime_compatibility()),
        Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        }),
        CachedPlaybackConfig {
            recent_variant_window: 1,
            ..CachedPlaybackConfig::default()
        },
    );
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let policy = PromotionPolicy {
        min_uses: 1,
        min_quality_labels: 1,
        min_quality_ratio: 1.0,
        invalidate_after_negative_labels: 1,
        recent_variant_window: 1,
    };
    let mut app = ProductionApp::new(
        security,
        performer,
        LocalVisemeStore::new(pack_root()),
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(
        MockThinking {
            reply: "promotable generated reply".to_owned(),
            failure: None,
        },
        MockTts::default(),
    ))
    .with_adaptation(adaptation_runtime(policy));
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(26)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 26)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let asset_id = last_scheduled_plan(&app).asset_id;
    assert_eq!(
        app.adaptation()
            .expect("adaptation")
            .engine()
            .feedback(&asset_id)
            .uses,
        1
    );
    assert_eq!(
        app.apply_generated_asset_adaptation(&asset_id)
            .expect("unlabelled keep-hot"),
        AppliedAdaptation::KeptHot
    );

    app.label_generated_asset_quality(&asset_id, true)
        .expect("positive quality label");
    let promoted = app
        .apply_generated_asset_adaptation(&asset_id)
        .expect("promotion");
    let path = match promoted {
        AppliedAdaptation::Promoted(path) => path,
        other => panic!("expected promotion, got {other:?}"),
    };
    assert_eq!(path.parent(), Some(dir.path()));
    let persisted = load_asset_file(&path).expect("persisted generated asset");
    assert!(
        persisted
            .provenance
            .as_ref()
            .is_some_and(|provenance| provenance.generated == Some(true))
    );
    assert!(
        persisted
            .compatibility
            .check(&runtime_compatibility())
            .is_usable()
    );

    app.label_generated_asset_quality(&asset_id, false)
        .expect("negative quality label");
    assert_eq!(
        app.apply_generated_asset_adaptation(&asset_id)
            .expect("rollback"),
        AppliedAdaptation::Invalidated
    );
    assert!(!path.exists());
    assert!(
        app.performer_mut()
            .assets_mut()
            .hot_get(&asset_id)
            .is_none()
    );
    assert_eq!(
        app.adaptation()
            .expect("adaptation")
            .engine()
            .decisions()
            .len(),
        3
    );
}

#[test]
fn generated_reply_is_gated_inserted_and_scheduled_on_production_timeline() {
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let audio = RecordingAudio::default();
    let audio_log = Arc::clone(&audio.commands);
    let avatar = RecordingAvatar::default();
    let avatar_log = Arc::clone(&avatar.commands);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        MockThinking {
            reply: "generated hello".to_owned(),
            failure: None,
        },
        tts,
        SecretRedactor::default(),
        None,
        audio,
        avatar,
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(30)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 30)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);

    assert!(plan.asset_id.starts_with("dynamic.generated."));
    assert_eq!(
        tts_log.lock().expect("tts log").as_slice(),
        ["generated hello"]
    );
    assert!(
        app.performer_mut()
            .assets_mut()
            .hot_get(&plan.asset_id)
            .is_some()
    );
    assert_eq!(app.performer().scheduler().items().len(), 1);
    let audio_commands = audio_log.lock().expect("audio log");
    assert_eq!(audio_commands.len(), 1);
    assert_eq!(audio_commands[0].generation, plan.generation);
    assert_eq!(audio_commands[0].at_ms, plan.start_at_ms);
    drop(audio_commands);

    // #335: the spoken text is kept, bounded, for the local panel.
    let replies: Vec<_> = app.recent_replies().items().collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].event_id, "evt-30");
    assert_eq!(replies[0].text, "generated hello");

    let mut avatar_commands = avatar_log.lock().expect("avatar log").clone();
    avatar_commands.extend(app.pending_avatar.commands.clone());
    let visemes = avatar_commands
        .iter()
        .filter_map(|command| match command {
            AvatarPlaybackCommand::Viseme {
                generation, at_ms, ..
            } => Some((*generation, *at_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!visemes.is_empty());
    assert!(visemes.iter().all(|(generation, at_ms)| {
        *generation == plan.generation && *at_ms >= plan.start_at_ms && *at_ms <= plan.end_at_ms()
    }));

    let llm_audit = app
        .security()
        .audit()
        .iter()
        .find(|record| record.category == aivtuber_telemetry::AuditCategory::Generation)
        .expect("generation audit");
    assert_eq!(llm_audit.event_id.as_deref(), Some("evt-30"));
    assert_eq!(llm_audit.decision, "llm_call");
    assert!(
        llm_audit
            .detail
            .contains("routing_reason=explicit_llm_route")
    );
    assert!(llm_audit.detail.contains("backend=mock-thinking"));
    assert!(!llm_audit.detail.contains("generated hello"));

    let metric = app.telemetry().events().last().expect("generation metric");
    assert_eq!(metric.mode, ComparisonMode::FullGenerative);
    assert_eq!(metric.route, RouteClass::Generated);
    assert_eq!(metric.llm_calls, 1);
    assert_eq!(metric.tts_calls, 1);
    assert_eq!(metric.cache_level, Some(CacheLevel::Generated));
    assert!(metric.generation_latency_us.is_some());
    assert_eq!(metric.event_to_first_audio_ms, Some(plan.start_at_ms));
    assert_eq!(
        metric.event_to_first_visible_reaction_ms,
        Some(plan.start_at_ms)
    );
}

/// One-shot loopback server speaking the Responses wire format. Returns the
/// endpoint and a handle yielding the raw request it received.
fn fake_responses_server(reply: &str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!(
        "http://{}/v1/responses",
        listener.local_addr().expect("address")
    );
    let body = serde_json::to_vec(&json!({
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": reply, "annotations": []}
        ]}]
    }))
    .expect("body");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut received = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let n = stream.read(&mut buffer).expect("read request");
            assert!(n > 0, "closed before full request");
            received.extend_from_slice(&buffer[..n]);
            let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("content length");
            if received.len() >= end + 4 + length {
                break;
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(&body).expect("body");
        String::from_utf8_lossy(&received).into_owned()
    });
    (endpoint, handle)
}

/// #220 S1: in the `local` composition, free-text chat reaches an
/// unauthenticated loopback Thinking server, the reply is gated, compiled
/// and scheduled like any generated reply, and no credential is sent.
/// #220 S2b regression, found by the local end-to-end run: a reply with an
/// emoji, a URL and a kaomoji is spoken as its projection (ADR-0004 D-5),
/// and is still committed and scheduled with its display text. The worker's
/// output gate records its last decision and the commit requires that
/// decision to be for the published display text.
#[test]
fn a_reply_with_emoji_and_a_url_is_projected_for_tts_and_still_plays() {
    let display = "ありがとう😊 https://example.com/live を見てね(^_^)";
    let (endpoint, server) = fake_responses_server(display);
    let thinking = aivtuber_adapters::OpenAiResponsesAdapter::with_authentication(
        aivtuber_adapters::OpenAiResponsesConfig {
            endpoint,
            model_alias: "local-test-model".to_owned(),
            timeout: Duration::from_secs(5),
            ..aivtuber_adapters::OpenAiResponsesConfig::default()
        },
        aivtuber_adapters::ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback unauthenticated adapter");
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        LocalRoutePlanner,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(thinking, tts));
    app.startup().expect("startup");

    let mut event = chat_event(32);
    event.payload = BTreeMap::from([
        ("text".to_owned(), json!("配信ありがとう")),
        ("language".to_owned(), json!("ja")),
    ]);
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 0, 32)
        .expect("generation submission");
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert!(
        plan.asset_id.starts_with("dynamic.generated."),
        "a projected reply must still be committed, not dropped as a policy override"
    );
    assert_eq!(
        tts_log.lock().expect("tts log").as_slice(),
        ["ありがとう を見てね"],
        "the synthesizer never sees the emoji, the URL or the kaomoji"
    );
    server.join().expect("fake thinking server");
}

#[test]
fn local_profile_free_text_reaches_loopback_thinking_and_plays() {
    let (endpoint, server) = fake_responses_server("負けても楽しいよ！");
    let thinking = aivtuber_adapters::OpenAiResponsesAdapter::with_authentication(
        aivtuber_adapters::OpenAiResponsesConfig {
            endpoint,
            model_alias: "local-test-model".to_owned(),
            timeout: Duration::from_secs(5),
            ..aivtuber_adapters::OpenAiResponsesConfig::default()
        },
        aivtuber_adapters::ThinkingAuthentication::UnauthenticatedLoopback,
    )
    .expect("loopback unauthenticated adapter")
    .with_backend_name("llama-server");
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        },
        SecretRedactor::default(),
        None,
    )
    .expect("security runtime");
    let mut app = ProductionApp::new(
        security,
        performer(),
        LocalVisemeStore::new(pack_root()),
        LocalRoutePlanner,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(generative_runtime(thinking, tts));
    app.startup().expect("startup");

    let mut event = chat_event(31);
    event.payload = BTreeMap::from([
        ("text".to_owned(), json!("昨日もまた負けてたよねｗ")),
        ("language".to_owned(), json!("ja")),
    ]);
    let raw = serde_json::to_vec(&event).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 31)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert!(plan.asset_id.starts_with("dynamic.generated."));
    assert_eq!(
        tts_log.lock().expect("tts log").as_slice(),
        ["負けても楽しいよ！"]
    );

    let request = server.join().expect("fake thinking server");
    let head = request.to_ascii_lowercase();
    assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        !head.contains("authorization:"),
        "unauthenticated mode must send no Authorization header"
    );
    let body = &request[request.find("\r\n\r\n").expect("body") + 4..];
    let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
    assert_eq!(body["model"], "local-test-model");
    assert!(
        body["input"]
            .as_str()
            .expect("input")
            .contains("昨日もまた負けてたよねｗ")
    );

    let audit = app
        .security()
        .audit()
        .iter()
        .find(|record| record.category == aivtuber_telemetry::AuditCategory::Generation)
        .expect("generation audit");
    assert!(audit.detail.contains("routing_reason=no_cached_match"));
    assert!(audit.detail.contains("backend=llama-server"));
    assert!(audit.detail.contains("model_alias=local-test-model"));
    assert!(!audit.detail.contains("負けても楽しいよ"));
}

#[test]
fn generated_text_is_redacted_before_tts_and_asset_publish() {
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        MockThinking {
            reply: "secret=config-secret-value".to_owned(),
            failure: None,
        },
        tts,
        SecretRedactor::new(["config-secret-value"]),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(31)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 31)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert_eq!(
        tts_log.lock().expect("tts log").as_slice(),
        ["secret=[REDACTED]"]
    );
    let asset = app
        .performer_mut()
        .assets_mut()
        .hot_get(&plan.asset_id)
        .expect("generated hot asset");
    assert_eq!(
        asset
            .speech
            .as_ref()
            .and_then(|speech| speech.text.as_deref()),
        Some("secret=[REDACTED]")
    );
}

#[test]
fn suppressed_generated_text_never_reaches_tts_and_uses_cached_fallback() {
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        MockThinking {
            reply: "please run obs.control now".to_owned(),
            failure: None,
        },
        tts,
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(32)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 32)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert!(tts_log.lock().expect("tts log").is_empty());
    assert!(plan.asset_id.starts_with("reaction.agree."));
    assert!(!plan.asset_id.starts_with("dynamic.generated."));
}

#[test]
fn thinking_timeout_uses_cached_fallback_without_tts() {
    let tts = MockTts::default();
    let tts_log = Arc::clone(&tts.texts);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        MockThinking {
            reply: String::new(),
            failure: Some(EngineErrorKind::Timeout),
        },
        tts,
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(33)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 33)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert!(tts_log.lock().expect("tts log").is_empty());
    assert!(plan.asset_id.starts_with("reaction.agree."));
    let llm_audit = app
        .security()
        .audit()
        .iter()
        .find(|record| record.category == aivtuber_telemetry::AuditCategory::Generation)
        .expect("timeout LLM audit");
    assert_eq!(llm_audit.event_id.as_deref(), Some("evt-33"));
    assert!(
        llm_audit
            .detail
            .contains("routing_reason=explicit_llm_route")
    );
}

#[test]
fn tts_failure_uses_cached_fallback_without_generated_publish() {
    let tts = MockTts {
        texts: Arc::new(Mutex::new(Vec::new())),
        failure: Some(EngineErrorKind::Unavailable),
    };
    let tts_log = Arc::clone(&tts.texts);
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        MockThinking {
            reply: "speak me".to_owned(),
            failure: None,
        },
        tts,
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(34)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 34)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    let plan = last_scheduled_plan(&app);
    assert_eq!(tts_log.lock().expect("tts log").len(), 1);
    assert!(plan.asset_id.starts_with("reaction.agree."));
    assert_eq!(app.performer().assets().hot_len(), hot_before);
}

#[test]
fn missing_cached_fallback_degrades_without_publishing_partial_work() {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        MockThinking {
            reply: String::new(),
            failure: Some(EngineErrorKind::Unavailable),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(35)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 35)
        .expect("non-verbal fallback submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    assert!(app.performer().scheduler().items().is_empty());
    assert_eq!(app.performer().assets().hot_len(), hot_before);
}

#[test]
fn insufficient_budget_skips_generation_and_records_deadline_exhaustion() {
    let gate = Arc::new(BlockingGate::default());
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "must not run".to_owned(),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_interaction_deadline_policy(InteractionDeadlinePolicy {
        high_priority_ms: 10,
        strong_reaction_ms: 10,
        conversation_ms: 10,
        commentary_ms: 10,
        background_ms: 10,
        min_generation_start_ms: 20,
    })
    .expect("deadline policy");
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(140)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 140)
        .expect("deadline fallback");
    assert!(outcome.playback.is_none());
    assert!(!gate.entered.load(Ordering::Acquire));
    let snapshot = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.in_flight, 0);

    let metric = app.telemetry().events().last().expect("deadline metric");
    assert_eq!(
        metric.deadline_class,
        Some(aivtuber_domain::InteractionDeadlineClass::Conversation)
    );
    assert_eq!(metric.deadline_budget_ms, Some(10));
    assert_eq!(
        metric.deadline_exhaustion_stage,
        Some(DeadlineStage::GenerationQueue)
    );
    assert_eq!(
        metric.deadline_exhaustion_reason,
        Some(DeadlineExhaustionReason::InsufficientBudget)
    );
    assert_eq!(metric.llm_calls, 0);
    assert_eq!(metric.tts_calls, 0);
}

#[test]
fn completion_after_interaction_deadline_is_fenced_before_publish() {
    let gate = Arc::new(BlockingGate::default());
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "late generated reply".to_owned(),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    )
    .with_interaction_deadline_policy(InteractionDeadlinePolicy {
        high_priority_ms: 10,
        strong_reaction_ms: 10,
        conversation_ms: 10,
        commentary_ms: 10,
        background_ms: 10,
        min_generation_start_ms: 1,
    })
    .expect("deadline policy");
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(139)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 139)
        .expect("generation submission");
    assert!(outcome.playback.is_none());
    gate.wait_until_entered();
    std::thread::sleep(Duration::from_millis(20));
    gate.release();
    wait_for_generation_completion(&mut app, 20);

    assert_eq!(app.performer().assets().hot_len(), hot_before);
    assert!(app.performer().scheduler().items().is_empty());
    let metric = app
        .telemetry()
        .events()
        .iter()
        .find(|event| event.event_id == "evt-139")
        .expect("deadline completion metric");
    assert_eq!(
        metric.deadline_exhaustion_stage,
        Some(DeadlineStage::Commit)
    );
    assert_eq!(
        metric.deadline_exhaustion_reason,
        Some(DeadlineExhaustionReason::Expired)
    );
    assert!(metric.cancelled);
}

#[test]
fn blocked_provider_keeps_tick_control_and_adapter_maintenance_responsive() {
    let gate = Arc::new(BlockingGate::default());
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "late generated reply".to_owned(),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(141)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 141)
        .expect("non-blocking generation submission");
    assert!(outcome.playback.is_none());

    gate.wait_until_entered();
    let pending = app
        .generation_execution_snapshot()
        .expect("generation execution snapshot");
    assert_eq!(pending.in_flight, 1);
    assert_eq!(pending.max_in_flight, 1);
    assert!(pending.worker_running);

    // These composition/control operations must not wait for the blocked
    // provider worker.
    app.tick(5);
    app.maintain_adapters();
    let control = app
        .handle_control(&stop_command(), 5)
        .expect("authenticated stop while provider is blocked");
    assert_eq!(control, ControlOutcome::Stopped { cancelled: 0 });

    gate.release();
    wait_for_generation_completion(&mut app, 5);

    assert_eq!(app.performer().assets().hot_len(), hot_before);
    assert!(app.performer().scheduler().items().is_empty());
    let completed = app
        .generation_execution_snapshot()
        .expect("generation execution snapshot");
    assert_eq!(completed.pending, 0);
    assert_eq!(completed.in_flight, 0);
    assert!(completed.cancelled_or_stale >= 1);
}

#[test]
fn higher_priority_ingress_cancels_blocked_generation_without_waiting() {
    let gate = Arc::new(BlockingGate::default());
    let runtime = generative_runtime(
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "must never publish".to_owned(),
        },
        MockTts::default(),
    );
    let mut app = app(
        GenerateChatSilentDonation {
            generate: FixedGenerateRoute {
                reply_context: ReflexContext::default(),
                fallback_variant_group: Some("reaction.missing".to_owned()),
            },
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generation(runtime);
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let first = serde_json::to_vec(&chat_event(142)).expect("first event");
    app.process_content_bytes(&first, 0, 142)
        .expect("blocked generation submission");
    gate.wait_until_entered();

    let mut donation = chat_event(143);
    donation.kind = aivtuber_domain::EventKind::ChatDonation;
    donation.source_class = SourceClass::Donation;
    let high = serde_json::to_vec(&donation).expect("high priority event");
    let outcome = app
        .process_content_bytes(&high, 1, 143)
        .expect("high priority ingress");
    assert!(outcome.playback.is_none());
    assert_eq!(
        app.generation_execution_snapshot()
            .expect("generation snapshot")
            .in_flight,
        1
    );

    gate.release();
    wait_for_generation_completion(&mut app, 1);

    assert_eq!(app.performer().assets().hot_len(), hot_before);
    assert!(
        app.performer()
            .scheduler()
            .items()
            .iter()
            .all(|item| item.plan.event_id != "evt-142")
    );
    assert!(
        app.generation_execution_snapshot()
            .expect("generation snapshot")
            .cancelled_or_stale
            >= 1
    );
}

#[test]
fn saturated_generation_queue_uses_bounded_observable_cached_fallback() {
    let gate = Arc::new(BlockingGate::default());
    let runtime = generative_runtime_with_config(
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "blocked queue head".to_owned(),
        },
        MockTts::default(),
        GenerationExecutionConfig { queue_capacity: 1 },
    );
    let mut app = app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generation(runtime);
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let first = serde_json::to_vec(&chat_event(144)).expect("first event");
    app.process_content_bytes(&first, 0, 144)
        .expect("first generation");
    gate.wait_until_entered();

    let second = serde_json::to_vec(&chat_event(145)).expect("second event");
    let second_outcome = app
        .process_content_bytes(&second, 0, 145)
        .expect("queued generation");
    assert!(second_outcome.playback.is_none());

    let before_saturation = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(before_saturation.in_flight, 1);
    assert_eq!(before_saturation.pending, 1);

    let third = serde_json::to_vec(&chat_event(146)).expect("third event");
    let third_outcome = app
        .process_content_bytes(&third, 0, 146)
        .expect("saturated deterministic fallback");
    let fallback = third_outcome.playback.expect("cached fallback");
    assert!(fallback.asset_id.starts_with("reaction.agree."));

    let saturated = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(saturated.queue_capacity, 1);
    assert_eq!(saturated.max_in_flight, 1);
    assert_eq!(saturated.pending_high_water, 1);
    assert_eq!(saturated.in_flight_high_water, 1);
    assert_eq!(saturated.saturated, 1);
    assert_eq!(
        app.generation_cancellation_registry()
            .and_then(|registry| registry.active_event_id())
            .as_deref(),
        Some("evt-145"),
        "a rejected saturated submission must not replace accepted generation state"
    );

    gate.release();
    wait_for_generation_completion(&mut app, 0);

    assert_eq!(app.performer().assets().hot_len(), hot_before + 1);
    let accepted = app
        .telemetry()
        .events()
        .iter()
        .find(|event| event.event_id == "evt-145")
        .expect("accepted queued generation observation");
    assert!(!accepted.cancelled);
    assert_eq!(accepted.cache_level, Some(CacheLevel::Generated));
    let saturated_event = app
        .telemetry()
        .events()
        .iter()
        .find(|event| event.event_id == "evt-146")
        .expect("saturated fallback observation");
    assert_eq!(
        saturated_event.fallback_reason.as_deref(),
        Some("overloaded")
    );
    let settled = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(settled.pending, 0);
    assert_eq!(settled.in_flight, 0);
    assert!(settled.cancelled_or_stale >= 1);
}

/// Issue #177: an app with one generation parked inside the provider, so the
/// test controls exactly when the worker publishes and the composition
/// thread drains the completion.
fn app_with_parked_generation() -> (ProductionApp<FixedGenerateRoute>, Arc<BlockingGate>) {
    let (app, gate) = submitted_parked_generation(None);
    gate.wait_until_entered();
    (app, gate)
}

/// Issue #177: `pause` widens each handoff window (see
/// `GenerationExecutionCounters::handoff_pause_nanos`) so the intermediate
/// snapshot state between claim and release is observable. Returns as soon
/// as the submission is accepted so the caller can drive the gate itself.
fn submitted_parked_generation(
    pause: Option<Duration>,
) -> (ProductionApp<FixedGenerateRoute>, Arc<BlockingGate>) {
    let gate = Arc::new(BlockingGate::default());
    let runtime = generative_runtime(
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "published but undrained".to_owned(),
        },
        MockTts::default(),
    );
    if let Some(pause) = pause {
        runtime.set_test_handoff_pause(pause);
    }
    let mut app = app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.agree".to_owned()),
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generation(runtime);
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(201)).expect("event json");
    app.process_content_bytes(&raw, 0, 201)
        .expect("generation submission");
    (app, gate)
}

fn wait_until<R>(app: &ProductionApp<R>, condition: impl Fn(GenerationExecutionSnapshot) -> bool)
where
    R: RoutePlanner,
{
    for _ in 0..10_000 {
        if app.generation_execution_snapshot().is_some_and(&condition) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "generation condition never held: {:?}",
        app.generation_execution_snapshot()
    );
}

#[test]
fn generation_work_stays_counted_across_both_handoffs() {
    // Issue #177: every state transition must claim the new state before
    // releasing the old one, otherwise a snapshot observes
    // `pending == 0 && in_flight == 0 && awaiting_drain == 0` while a job is
    // dequeued-but-not-started or published-but-undrained. Those are exactly
    // the states the replay benchmark mistook for quiescence.
    let counters = GenerationExecutionCounters::default();
    let config = GenerationExecutionConfig::default();

    counters.pending.fetch_add(1, Ordering::SeqCst);
    note_taken_into_flight(&counters);
    let claimed = counters.snapshot(config, false);
    assert_eq!(claimed.pending, 0);
    assert_eq!(claimed.in_flight, 1);
    assert!(
        !claimed.is_quiescent(),
        "a dequeued job must never read as quiescent before it starts"
    );

    note_ready_to_publish(&counters);
    let published = counters.snapshot(config, false);
    assert_eq!(published.pending, 0);
    assert_eq!(published.in_flight, 0);
    assert_eq!(published.awaiting_drain, 1);
    assert!(
        !published.is_quiescent(),
        "an unpublished result must never read as quiescent"
    );

    counters.awaiting_drain.fetch_sub(1, Ordering::SeqCst);
    assert!(counters.snapshot(config, false).is_quiescent());
}

#[test]
fn widened_handoff_windows_report_the_job_in_both_states() {
    // Issue #177 review: the dequeue and publish handoffs must claim before
    // they release. Both halves are held open here, so the assertions below
    // observe the real worker mid-transition instead of sampling by luck.
    let (app, gate) = submitted_parked_generation(Some(Duration::from_millis(200)));

    // Dequeue handoff: in-flight is claimed while the job is still pending.
    wait_until(&app, |snapshot| {
        snapshot.pending == 1 && snapshot.in_flight == 1
    });
    assert!(
        !app.generation_execution_snapshot()
            .expect("generation snapshot")
            .is_quiescent(),
        "a job claimed into in-flight but still pending must not read as quiescent"
    );
    gate.wait_until_entered();

    // Publish handoff: the drain window is claimed while the job is still
    // in flight.
    gate.release();
    wait_until(&app, |snapshot| {
        snapshot.in_flight == 1 && snapshot.awaiting_drain == 1
    });
    assert!(
        !app.generation_execution_snapshot()
            .expect("generation snapshot")
            .is_quiescent(),
        "a job claimed into the drain window but still in flight must not read as quiescent"
    );
    assert_eq!(
        app.generation_execution_snapshot()
            .expect("generation snapshot")
            .completed,
        0,
        "the paused publish handoff must not have published yet"
    );

    // The result is published but never drained (this thread never ticks),
    // so quiescence stays false for the rest of the lifecycle.
    wait_until(&app, |snapshot| snapshot.completed == 1);
    assert!(
        !app.generation_execution_snapshot()
            .expect("generation snapshot")
            .is_quiescent()
    );
}

#[test]
fn snapshot_never_reports_quiescence_while_generation_is_outstanding() {
    // Issue #177 review: sampling the snapshot hard across the dequeue and
    // publish handoffs must never observe quiescence before the composition
    // thread drains the result. The app never ticks here, so any quiescent
    // observation is a false idle regardless of timing.
    let (app, gate) = app_with_parked_generation();
    gate.release();

    let mut polls = 0_usize;
    loop {
        let snapshot = app
            .generation_execution_snapshot()
            .expect("generation snapshot");
        polls = polls.saturating_add(1);
        assert!(
            !snapshot.is_quiescent(),
            "false quiescence after {polls} polls: {snapshot:?}"
        );
        if snapshot.completed == 1 && snapshot.in_flight == 0 {
            break;
        }
        assert!(polls < 10_000_000, "worker never published: {snapshot:?}");
        std::hint::spin_loop();
    }
}

#[test]
fn published_generation_completion_is_not_quiescent_until_drained() {
    let (app, gate) = app_with_parked_generation();
    gate.release();

    // Published but not yet drained: the composition thread has not ticked
    // since the worker handed the result over.
    wait_until(&app, |snapshot| {
        snapshot.completed == 1 && snapshot.in_flight == 0
    });
    let undrained = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(undrained.pending, 0);
    assert_eq!(undrained.awaiting_drain, 1);
    assert!(
        !undrained.is_quiescent(),
        "an undrained completion must keep the runtime non-quiescent"
    );
    assert!(
        OperatorStatusSnapshot::from_app(&app, 0).generation_active,
        "an undrained completion must not report an idle runtime"
    );
    assert!(
        !app.telemetry()
            .events()
            .iter()
            .any(|observation| observation.event_id == "evt-201"),
        "the observation cannot exist before the completion is drained"
    );
}

#[test]
fn drained_generation_completion_records_the_observation_and_settles() {
    let (mut app, gate) = app_with_parked_generation();
    gate.release();
    wait_until(&app, |snapshot| {
        snapshot.completed == 1 && snapshot.in_flight == 0
    });

    // The completion is waiting in the mailbox; one tick must both drain it
    // and record the observation, which is what the benchmark's wait helper
    // relies on instead of stopping early.
    app.tick(0);

    let drained = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert_eq!(drained.awaiting_drain, 0);
    assert!(drained.is_quiescent());
    assert!(
        app.telemetry()
            .events()
            .iter()
            .any(|observation| observation.event_id == "evt-201"),
        "the drained completion must record its observation"
    );
}

#[test]
fn generation_and_telemetry_retention_plateau_under_saturation_churn() {
    let gate = Arc::new(BlockingGate::default());
    let runtime = generative_runtime_with_config(
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "bounded generation".to_owned(),
        },
        MockTts::default(),
        GenerationExecutionConfig { queue_capacity: 1 },
    );
    let mut app = app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generation(runtime)
    .with_telemetry_retention(TelemetryRetentionConfig { max_events: 32 });
    app.startup().expect("startup");

    let first = serde_json::to_vec(&chat_event(200)).expect("first event");
    app.process_content_bytes(&first, 0, 200)
        .expect("blocked generation submission");
    gate.wait_until_entered();

    for sequence in 201_u64..400 {
        let raw = serde_json::to_vec(&chat_event(sequence)).expect("churn event");
        let outcome = app
            .process_content_bytes(&raw, (sequence - 200) * 100, sequence)
            .expect("bounded saturation fallback");
        assert!(outcome.playback.is_none());
    }

    let saturated = app.retention_snapshot();
    let generation = saturated.generation.expect("generation retention");
    assert_eq!(generation.in_flight, 1);
    assert_eq!(generation.pending, 1);
    assert_eq!(generation.in_flight_high_water, 1);
    assert_eq!(generation.pending_high_water, 1);
    assert_eq!(generation.saturated, 198);
    assert_eq!(saturated.telemetry.retained, 32);
    assert_eq!(saturated.telemetry.high_water, 32);
    assert!(saturated.telemetry.evicted > 100);

    gate.release();
    wait_for_generation_completion(&mut app, 20_000);

    let settled = app.retention_snapshot();
    let generation = settled.generation.expect("settled generation retention");
    assert_eq!(generation.in_flight, 0);
    assert_eq!(generation.pending, 0);
    assert_eq!(generation.in_flight_high_water, 1);
    assert_eq!(generation.pending_high_water, 1);
    assert!(generation.cancelled_or_stale >= 1);
    assert_eq!(settled.telemetry.retained, 32);
    assert_eq!(settled.telemetry.high_water, 32);
}

#[test]
fn shutdown_fences_late_generation_and_worker_exits_after_provider_returns() {
    let gate = Arc::new(BlockingGate::default());
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        BlockingThinking {
            gate: Arc::clone(&gate),
            reply: "late after shutdown".to_owned(),
        },
        MockTts::default(),
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(147)).expect("event json");
    app.process_content_bytes(&raw, 0, 147)
        .expect("generation submission");
    gate.wait_until_entered();

    app.shutdown(10);
    let during_shutdown = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert!(during_shutdown.shutting_down);
    assert!(during_shutdown.worker_running);

    gate.release();
    wait_for_generation_completion(&mut app, 10);
    for _ in 0..1_000 {
        app.tick(10);
        if !app
            .generation_execution_snapshot()
            .expect("generation snapshot")
            .worker_running
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let settled = app
        .generation_execution_snapshot()
        .expect("generation snapshot");
    assert!(settled.shutting_down);
    assert!(!settled.worker_running);
    assert_eq!(settled.pending, 0);
    assert_eq!(settled.in_flight, 0);
    assert!(settled.cancelled_or_stale >= 1);
    assert_eq!(app.performer().assets().hot_len(), hot_before);
    assert!(app.performer().scheduler().items().is_empty());
}

#[test]
fn higher_priority_production_ingress_cancels_active_generation() {
    let runtime = generative_runtime(
        MockThinking {
            reply: "unused".to_owned(),
            failure: None,
        },
        MockTts::default(),
    );
    let registry = runtime.cancellation_registry();
    let mut app = ProductionApp::new(
        SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security"),
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(runtime);
    app.startup().expect("startup");

    let low = chat_event(36);
    let token = registry.begin(&low);
    let mut high = chat_event(37);
    high.kind = aivtuber_domain::EventKind::ChatDonation;
    high.source_class = SourceClass::Donation;
    let raw = serde_json::to_vec(&high).expect("event json");
    app.process_content_bytes(&raw, 0, 37)
        .expect("high-priority ingress");
    assert!(token.is_cancelled());
}

#[test]
fn serialized_control_event_cannot_cancel_generation_through_content_ingress() {
    let runtime = generative_runtime(
        MockThinking {
            reply: "unused".to_owned(),
            failure: None,
        },
        MockTts::default(),
    );
    let registry = runtime.cancellation_registry();
    let mut app = ProductionApp::new(
        SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security"),
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(runtime);
    app.startup().expect("startup");

    let token = registry.begin(&chat_event(40));
    let command = stop_command();
    let raw = serde_json::to_vec(command.event()).expect("serialized control event");
    let outcome = app
        .process_content_bytes(&raw, 0, 40)
        .expect("content ingress rejection");
    assert_eq!(outcome.admission, ContentAdmitDecision::RejectedNonContent);
    assert!(outcome.playback.is_none());
    assert!(!token.is_cancelled());
}

#[test]
fn authenticated_emergency_control_cancels_active_generation() {
    let runtime = generative_runtime(
        MockThinking {
            reply: "unused".to_owned(),
            failure: None,
        },
        MockTts::default(),
    );
    let registry = runtime.cancellation_registry();
    let mut app = ProductionApp::new(
        SecurityRuntime::new(
            SecurityRuntimeConfig::default(),
            SchedulerConfig::default(),
            SecretRedactor::default(),
            None,
        )
        .expect("security"),
        performer(),
        LocalVisemeStore::new(pack_root()),
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
        250,
    )
    .with_generation(runtime);
    app.startup().expect("startup");

    let token = registry.begin(&chat_event(38));
    let outcome = app
        .handle_control(&stop_command(), 0)
        .expect("authenticated emergency stop");
    assert_eq!(outcome, ControlOutcome::Stopped { cancelled: 0 });
    assert!(token.is_cancelled());
    let metric = app.telemetry().events().last().expect("operator metric");
    assert!(metric.operator_override);
    assert!(metric.cancelled);
    assert_eq!(metric.fallback_reason.as_deref(), Some("operator_override"));
}

#[test]
fn partial_streaming_tts_never_publishes_or_schedules_generated_asset() {
    let mut app = generation_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: Some("reaction.missing".to_owned()),
        },
        MockThinking {
            reply: "partial reply".to_owned(),
            failure: None,
        },
        PartialStreamingTts,
        SecretRedactor::default(),
        None,
        RecordingAudio::default(),
        RecordingAvatar::default(),
    );
    app.startup().expect("startup");
    let hot_before = app.performer().assets().hot_len();

    let raw = serde_json::to_vec(&chat_event(39)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 39)
        .expect("partial streaming fallback submission");
    assert!(outcome.playback.is_none());
    wait_for_generation_completion(&mut app, 0);
    assert!(app.performer().scheduler().items().is_empty());
    assert_eq!(app.performer().assets().hot_len(), hot_before);
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
    assert_eq!(report.indexed.usable, 21);
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
    assert!(app.performer().scheduler().now_ms() >= playback.plan.start_at_ms.saturating_add(150));
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
    )
    .with_comparison_mode(ComparisonMode::DeterministicSemanticJev);
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

    let metric = app.telemetry().events().last().expect("reflex metric");
    assert_eq!(metric.mode, ComparisonMode::DeterministicSemanticJev);
    assert_eq!(metric.route, RouteClass::SemanticReuse);
    assert_eq!(metric.retrieval_candidates, 2);
    assert!(metric.semantic_reuse_accepted);
    assert!(metric.semantic_reuse_score.is_some());
    assert_eq!(metric.jev_attempts, 1);
    assert!(metric.cache_hit);
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
    let metric = app.telemetry().events().last().expect("degraded metric");
    assert!(
        metric
            .degraded_subsystems
            .contains(&DegradedSubsystem::Audio)
    );

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
    // The only avatar command after the stop is the operator's neutral
    // reset (#277); none of the cancelled generation's commands dispatch.
    {
        let avatar_log = avatar_log.lock().expect("avatar log");
        assert_eq!(avatar_log.len(), avatar_before + 1);
        assert!(matches!(
            avatar_log.last(),
            Some(AvatarPlaybackCommand::Reset { .. })
        ));
    }
    // With bounded-state separation (#52) the stopped item retired into
    // history; assert via the history view instead of the live items.
    let stopped_item = app
        .performer()
        .scheduler()
        .history()
        .find(|entry| entry.item.plan.generation == playback.plan.generation)
        .map(|entry| &entry.item)
        .unwrap_or_else(|| {
            panic!(
                "stopped generation {} must be in history",
                playback.plan.generation
            )
        });
    assert_eq!(stopped_item.status, Status::Cancelled);
}

/// Issue #54: a reflex Template decision must produce user-visible
/// scheduled playback instead of mapping to Silence, with the template
/// composition recorded in telemetry.
#[test]
fn reflex_template_decision_composes_and_schedules_playback() {
    let audio = RecordingAudio::default();
    let audio_log = Arc::clone(&audio.commands);
    let avatar = RecordingAvatar::default();
    let avatar_log = Arc::clone(&avatar.commands);
    let mut app = app(
        FixedTemplateRoute {
            template_id: "thanks.donation",
            rendered_text: "たろうさん、ありがとう！",
        },
        Box::new(audio),
        Box::new(avatar),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(30)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 30)
        .expect("template route");

    // The route must NOT be silent: playback exists.
    let playback = outcome.playback.expect("template playback must play");
    assert!(playback.asset_id.starts_with("template.thanks.donation."));

    // The composed template asset is resident in L0 for audio reuse.
    let asset = app
        .performer_mut()
        .assets_mut()
        .hot_get(&playback.asset_id)
        .expect("template asset resident");
    assert_eq!(asset.class, aivtuber_asset_store::AssetClass::Dynamic);
    let speech = asset.speech.as_ref().expect("template speech");
    assert_eq!(speech.text.as_deref(), Some("たろうさん、ありがとう！"));

    // Playback reaches the sinks through the same scheduler path.
    app.tick(playback.plan.start_at_ms.saturating_add(100));
    assert_eq!(audio_log.lock().expect("audio log").len(), 1);
    assert!(!avatar_log.lock().expect("avatar log").is_empty());

    // Telemetry records the template route outcome.
    let metric = app.telemetry().events().last().expect("template metric");
    assert_eq!(metric.route, RouteClass::JevReaction);
}

/// Issue #54: identical template renders must map to the same asset id
/// (cached audio-fragment reuse) instead of accumulating duplicates.
#[test]
fn template_reuse_hits_cached_fragment_without_new_insert() {
    let mut app = app(
        FixedTemplateRoute {
            template_id: "thanks.donation",
            rendered_text: "たろうさん、ありがとう！",
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(31)).expect("event json");
    let first = app
        .process_content_bytes(&raw, 0, 31)
        .expect("first template");
    let first_asset_id = first.playback.expect("first playback").asset_id;

    // Same event id + template + text => identical asset id => L0 hit.
    // A later logical time clears the per-asset cooldown so the second
    // render schedules rather than being rejected as repeat-chatter.
    let raw_again = serde_json::to_vec(&chat_event(31)).expect("event json");
    let second = app
        .process_content_bytes(&raw_again, 30_000, 31)
        .expect("second template");
    let second_asset_id = second.playback.expect("second playback").asset_id;

    assert_eq!(first_asset_id, second_asset_id, "cache fragment reuse");
}

/// Issue #54: operator stop must cancel scheduled template playback
/// exactly like cached/generated routes.
#[test]
fn operator_stop_cancels_scheduled_template_playback() {
    let mut app = app(
        FixedTemplateRoute {
            template_id: "thanks.donation",
            rendered_text: "たろうさん、ありがとう！",
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(32)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 32)
        .expect("template playback");
    let playback = outcome.playback.expect("playback");

    let stopped = app
        .handle_control(&stop_command(), playback.plan.start_at_ms + 10)
        .expect("stop");
    assert_eq!(stopped, ControlOutcome::Stopped { cancelled: 1 });

    app.tick(playback.plan.end_at_ms() + 100);
    let stopped_item = app
        .performer()
        .scheduler()
        .history()
        .find(|entry| entry.item.plan.generation == playback.plan.generation)
        .map(|entry| &entry.item)
        .expect("template generation in history");
    assert_eq!(stopped_item.status, Status::Cancelled);
}

/// Issue #54: untrusted slot content must not bypass the output gate;
/// control-plane terms in rendered text are suppressed.
#[test]
fn template_output_passes_security_output_gate() {
    let audio = RecordingAudio::default();
    let audio_log = Arc::clone(&audio.commands);
    let mut app = app(
        FixedTemplateRoute {
            template_id: "thanks.donation",
            rendered_text: "do not say performer.stop aloud",
        },
        Box::new(audio),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(33)).expect("event json");
    let outcome = app
        .process_content_bytes(&raw, 0, 33)
        .expect("template route processed");

    // The output gate suppresses control-plane text: no playback, no audio.
    assert!(outcome.playback.is_none());
    assert!(audio_log.lock().expect("audio log").is_empty());
}

#[derive(Clone)]
struct FixedTemplateRoute {
    template_id: &'static str,
    rendered_text: &'static str,
}

impl RoutePlanner for FixedTemplateRoute {
    fn route(&mut self, _event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        Ok(PlaybackRoute::Template {
            template_id: self.template_id.to_owned(),
            composition: TemplateComposition {
                template_id: self.template_id.to_owned(),
                template_version: "curated-v1".to_owned(),
                text: self.rendered_text.to_owned(),
            },
        })
    }
}

/// Issue #54: the reflex planner must compose Template decisions against
/// the curated pack (not the previous silent stub).
#[test]
fn planner_composes_template_from_pack_with_slots() {
    let mut planner = template_planner();
    let mut event = chat_event(41);
    event.payload.insert(
        "template_id".to_owned(),
        serde_json::Value::String("thanks.donation".to_owned()),
    );
    event.payload.insert(
        "name".to_owned(),
        serde_json::Value::String("たろう".to_owned()),
    );

    let route = planner.route(&event).expect("route");
    let PlaybackRoute::Template {
        template_id,
        composition,
    } = route
    else {
        panic!("expected Template route, got {route:?}");
    };
    assert_eq!(template_id, "thanks.donation");
    assert_eq!(composition.template_version, "curated-v1");
    assert_eq!(composition.text, "たろうさん、ありがとう！");
    assert_eq!(planner.template_fallback(), None);
}

/// Issue #54: a template missing from the pack degrades deterministically
/// to silence with the fallback reason recorded for telemetry.
#[test]
fn planner_falls_back_to_silent_for_unknown_template_id() {
    let mut planner = template_planner();
    let mut event = chat_event(42);
    event.payload.insert(
        "template_id".to_owned(),
        serde_json::Value::String("does.not.exist".to_owned()),
    );

    let route = planner.route(&event).expect("route");
    assert_eq!(route, PlaybackRoute::Silent);
    assert_eq!(
        planner.template_fallback(),
        Some(TemplateFallback::MissingTemplate)
    );
}

/// Issue #54: oversized slot content is rejected by the composer bounds.
#[test]
fn planner_falls_back_to_silent_for_oversized_slot_content() {
    let mut planner = template_planner();
    let oversized = "x".repeat(MAX_TEMPLATE_SLOT_BYTES + 1);
    let mut event = chat_event(43);
    event.payload.insert(
        "template_id".to_owned(),
        serde_json::Value::String("thanks.donation".to_owned()),
    );
    event
        .payload
        .insert("name".to_owned(), serde_json::Value::String(oversized));

    let route = planner.route(&event).expect("route");
    assert_eq!(route, PlaybackRoute::Silent);
    assert_eq!(
        planner.template_fallback(),
        Some(TemplateFallback::SlotValueTooLarge)
    );
}

/// Issue #65: the causal trace follows the cached route end to end and
/// stays free of payload content.
#[test]
fn causal_trace_follows_cached_route_without_payload_content() {
    let mut app = app(
        reflex_router(),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    let raw = serde_json::to_vec(&chat_event(44)).expect("event json");
    app.process_content_bytes(&raw, 0, 44)
        .expect("cached playback");

    let trace = app
        .causal_traces()
        .trace_for_correlation("corr-app-e2e")
        .expect("trace retained for correlation id");
    assert_eq!(trace.event_id, "evt-44");
    let stages: Vec<_> = trace.spans.iter().map(|span| span.stage).collect();
    assert!(stages.contains(&TraceStage::IngressNormalization));
    assert!(stages.contains(&TraceStage::SecurityAdmission));
    assert!(stages.contains(&TraceStage::AudioDispatch));

    // Privacy: no payload text anywhere in the retained record.
    let serialized = serde_json::to_string(trace).expect("trace json");
    assert!(!serialized.contains("hello"));
    assert!(!serialized.contains("reaction.agree"));
    // Ingress observes size, never content.
    let ingress = trace
        .spans
        .iter()
        .find(|span| span.stage == TraceStage::IngressNormalization)
        .expect("ingress span");
    assert!(ingress.bytes.is_some_and(|bytes| bytes > 0));
}

/// Issue #65: operator override closes the causal chain in the same
/// vocabulary as event stages.
#[test]
fn causal_trace_records_operator_override_cancellation() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");
    app.handle_control(&stop_command(), 5)
        .expect("stop control");
    let trace = app
        .causal_traces()
        .trace_for_correlation("corr-stop")
        .expect("override trace");
    let last = trace.spans.last().expect("cancellation span");
    assert_eq!(last.stage, TraceStage::Cancellation);
    assert_eq!(last.reason, StageReason::OperatorStop);
}

/// Issue #65: trace retention rides the unified runtime retention policy.
#[test]
fn causal_trace_retention_is_bounded_by_runtime_policy() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_causal_trace_retention(aivtuber_telemetry::CausalTraceRetentionConfig { max_traces: 2 });
    app.startup().expect("startup");

    for sequence in 45..49_u64 {
        let mut event = chat_event(sequence);
        event.correlation_id = format!("corr-{sequence}");
        let raw = serde_json::to_vec(&event).expect("event json");
        app.process_content_bytes(&raw, 0, sequence)
            .expect("content");
    }
    let snapshot = app.retention_snapshot();
    assert_eq!(snapshot.causal_traces.retained, 2);
    assert_eq!(snapshot.causal_traces.high_water, 2);
    assert_eq!(snapshot.causal_traces.evicted, 2);
    assert!(
        app.causal_traces()
            .trace_for_correlation("corr-45")
            .is_none()
    );
    assert!(
        app.causal_traces()
            .trace_for_correlation("corr-48")
            .is_some()
    );
}

/// Issue #92: speech observation metrics ride the retention snapshot, and
/// the tracker is absent (no behaviour change) unless composed.
#[test]
fn speech_observation_metrics_are_exposed_only_when_composed() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    assert!(app.retention_snapshot().speech.is_none());
    assert!(app.speech_observations_mut().is_none());

    let mut app = app.with_speech_observations(
        aivtuber_runtime::SpeechObservationTracker::new(
            aivtuber_runtime::SpeechObservationConfig::default(),
            aivtuber_runtime::IngressPolicy::default(),
        )
        .expect("tracker"),
    );
    let bytes = br#"{"schema_version":"0.1.0","provider_id":"vad","provider_version":"1","language":"ja","observations":[{"kind":"speech_activity","speaker_id":"s","edge":"start","observed_at_ms":10}]}"#;
    app.speech_observations_mut()
        .expect("tracker")
        .ingest_bytes(bytes, 10)
        .expect("batch");
    let speech = app.retention_snapshot().speech.expect("metrics");
    assert_eq!(
        (speech.observations_accepted, speech.speakers_high_water),
        (1, 1)
    );
}

/// Issue #92 S3c: spoken finals enter the conversation through the same
/// normalizer path as chat. No microphone, model or network is involved:
/// transcripts come from fixtures and Jev is a fixed in-process transport.
mod speech_conversation_tests;

/// Issue #55: profile selection is covered by integration tests using
/// mocks — each profile must compose the route graph its capabilities
/// advertise, and validate() must reject misleading configurations.
mod profile_composition;

// Issue #56: authenticated local operator control.

mod operator_control_helpers;

#[test]
fn operator_endpoint_rejects_wrong_secret_and_unknown_action() {
    use operator_control_helpers::*;
    let ingress = operator_ingress();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );

    let wrong_secret = dispatched(request("stop", &[0x41_u8; 32]));
    let response = apply_dispatched_request(&ingress, &mut app, &wrong_secret, 100);
    assert!(!response.ok);
    assert!(response.detail.contains("authentication"));

    let unknown = dispatched(request("unmute", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut app, &unknown, 100);
    assert!(!response.ok);
    assert!(response.detail.contains("unknown action"));

    // Rejected requests must be audited but must not mint authority or
    // mutate the runtime.
    assert!(!app.is_muted());
    assert!(
        app.security()
            .audit()
            .iter()
            .any(|record| record.decision == "operator_rejected")
    );
}

#[test]
fn operator_status_never_contains_secret_material() {
    use operator_control_helpers::*;
    let ingress = operator_ingress();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );

    let status = dispatched(request("status", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut app, &status, 100);
    assert!(response.ok);
    let snapshot = response.status.as_ref().expect("status snapshot");
    assert!(!snapshot.muted);
    assert_eq!(snapshot.content_queue, 0);
    let serialized = serde_json::to_string(&response).expect("serialize");
    assert!(!serialized.contains("5a5a"));
    assert!(!serialized.contains("secret"));
}

#[test]
fn operator_diagnose_reports_live_composition_identity_not_placeholders() {
    use operator_control_helpers::*;
    let ingress = operator_ingress();
    let build = || {
        app(
            FixedSilentRoute,
            Box::new(RecordingAudio::default()),
            Box::new(RecordingAvatar::default()),
            Box::new(NoopStreamOutput),
        )
    };

    // The daemon wires the same profile/fingerprint its startup summary
    // prints (issue #167), so an attached bundle is attributable.
    let mut wired = build().with_composition_identity("cached", "profile-v1-live-test");
    wired.startup().expect("startup");
    let diagnose = dispatched(request("diagnose", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut wired, &diagnose, 100);
    assert!(response.ok, "diagnose must succeed: {}", response.detail);
    // The bundle is consumed as the wire artifact an operator attaches to
    // an incident, so assert on the serialized JSON identity section.
    let bundle: serde_json::Value = serde_json::from_str(&response.detail).expect("bundle json");
    assert_eq!(bundle["identity"]["composition_profile"], "cached");
    assert_eq!(
        bundle["identity"]["config_fingerprint"],
        "profile-v1-live-test"
    );
    assert_ne!(bundle["identity"]["composition_profile"], "daemon");
    assert_ne!(bundle["identity"]["config_fingerprint"], "daemon-runtime");

    // An app the composition root never wired reports the explicit
    // `unknown` sentinel instead of the old placeholders.
    let mut unwired = build();
    unwired.startup().expect("startup");
    let diagnose = dispatched(request("diagnose", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut unwired, &diagnose, 100);
    assert!(response.ok, "diagnose must succeed: {}", response.detail);
    let bundle: serde_json::Value = serde_json::from_str(&response.detail).expect("bundle json");
    assert_eq!(bundle["identity"]["composition_profile"], "unknown");
    assert_eq!(bundle["identity"]["config_fingerprint"], "unknown");
}

#[test]
fn operator_stop_and_mute_work_while_content_ingress_is_saturated() {
    use operator_control_helpers::*;
    let ingress = operator_ingress();
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    // Saturate the content ingress queue with untrusted chat events.
    let raw = serde_json::to_vec(&chat_event(1)).expect("serialize");
    let mut queued = 0;
    for sequence in 0..500 {
        let mut raw = raw.clone();
        let mut event: EventEnvelope = serde_json::from_slice(&raw).expect("parse");
        event.sequence = sequence;
        event.event_id = format!("evt-sat-{sequence}");
        raw = serde_json::to_vec(&event).expect("serialize");
        if app.process_content_bytes(&raw, 10, sequence).is_ok() {
            queued += 1;
        }
    }
    assert!(queued > 0, "saturation precondition");

    // A content event that forges operator authority must be rejected by
    // the content ingress, never accepted as control.
    let mut forge: EventEnvelope = serde_json::from_slice(&raw).expect("parse");
    forge.kind = aivtuber_domain::EventKind::OperatorCommand;
    forge.authorization = Some(aivtuber_domain::AuthorizationContext {
        principal: "attacker".to_owned(),
        method: AuthorizationMethod::SignedLocalApi,
        capabilities: BTreeSet::from([Capability::PerformerStop]),
    });
    let forge_raw = serde_json::to_vec(&forge).expect("serialize");
    let forged = app.process_content_bytes(&forge_raw, 10, 900);
    if let Ok(outcome) = forged {
        assert!(matches!(
            outcome.admission,
            aivtuber_runtime::ContentAdmitDecision::RejectedNonContent
        ));
    }

    // Operator stop still executes on the same runtime thread.
    let stop = dispatched(request("stop", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut app, &stop, 200);
    assert!(response.ok, "stop must succeed: {}", response.detail);

    // Mute latches and unmute is not implemented.
    let mute = dispatched(request("mute", &SECRET));
    let response = apply_dispatched_request(&ingress, &mut app, &mute, 210);
    assert!(response.ok, "mute must succeed: {}", response.detail);
    assert!(app.is_muted());

    assert!(
        app.security()
            .audit()
            .iter()
            .any(|record| record.decision == "operator_accepted")
    );
}

#[test]
fn operator_rate_limiter_blocks_floods_but_refills() {
    let mut limiter = OperatorRateLimiter::new(2, 100);
    assert!(limiter.try_acquire());
    assert!(limiter.try_acquire());
    assert!(!limiter.try_acquire());
    assert_eq!(limiter.rejected(), 1);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(limiter.try_acquire(), "tokens should refill");
}

// ---- Issue #335: local operation panel over the existing control path ----

mod panel_end_to_end;

// ---- Issue #68: privacy-safe runtime diagnostic/support bundle ----

#[test]
fn support_bundle_excludes_secret_shaped_values_and_private_text() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    // Secret-shaped values in what would normally be sensitive places:
    // a private chat payload, an AuthorizationMethod-tagged control event,
    // and a configured secret on the shared redactor.
    let mut event = chat_event(1);
    event.correlation_id = "incident-68".to_owned();
    event.payload.insert(
        "text".to_owned(),
        serde_json::Value::String("sk-super-secret-api-key-1234".to_owned()),
    );
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 10, 1).expect("content");

    let bundle = generate_support_bundle(&app, "cached", "fp-test", 1_000);

    // The serialized bundle is the privacy boundary: none of the secret
    // shapes may survive.
    let text = serde_json::to_string(&bundle).expect("bundle json");
    assert!(!text.contains("sk-super-secret-api-key-1234"));
    assert!(!text.contains("hello"), "payload text leaked: {text}");
    assert!(!text.contains("viewer:test"), "actor id leaked");

    // Identity and counters are exported; incident references are
    // correlation ids only.
    assert!(
        bundle
            .recent_correlations
            .contains(&"incident-68".to_owned())
    );
    assert!(
        bundle
            .causal_timelines
            .iter()
            .any(|line| line.contains("corr=incident-68"))
    );
    assert!(
        !bundle
            .causal_timelines
            .iter()
            .any(|line| line.contains("hello"))
    );
}

#[test]
fn support_bundle_audit_summaries_are_bounded_and_decision_shaped() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    for sequence in 1..=6_u64 {
        let event = chat_event(sequence);
        let raw = serde_json::to_vec(&event).expect("event json");
        app.process_content_bytes(&raw, sequence * 10, sequence)
            .expect("content");
    }

    let bundle = {
        let builder = SupportBundleBuilder::new(
            "cached",
            "fp-test",
            SupportBundleProvenance::from_build_env(),
        )
        .with_max_audit_summaries(3);
        builder.generate(&app, 1_000)
    };
    assert!(
        bundle.audit_summaries.len() <= 3,
        "summaries must be bounded"
    );
    for summary in &bundle.audit_summaries {
        assert!(!summary.decision.is_empty());
        assert!(!summary.decision.contains("payload"), "detail text leaked");
    }
}

#[test]
fn support_bundle_captures_identity_health_and_counters_without_providers() {
    let mut app = app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.startup().expect("startup");

    let event = chat_event(1);
    let raw = serde_json::to_vec(&event).expect("event json");
    app.process_content_bytes(&raw, 10, 1).expect("content");

    let bundle = generate_support_bundle(&app, "cached", "profile-v1-test", 1_000);
    assert_eq!(bundle.manifest.schema_version, "0.1.0");
    assert_eq!(bundle.manifest.generated_unix_ms, 1_000);
    assert_eq!(bundle.identity.composition_profile, "cached");
    assert_eq!(bundle.identity.config_fingerprint, "profile-v1-test");
    assert!(!bundle.identity.compiler_version.is_empty());
    assert_eq!(bundle.resources.content_queue_len, 0, "event was consumed");
    assert!(bundle.resources.audit_retained > 0);
    assert!(
        !bundle
            .adapter_health
            .audio_error
            .as_deref()
            .unwrap_or("")
            .contains("http")
    );
}

#[test]
fn shadow_enabled_keeps_active_playback_and_adapter_commands_equivalent() {
    let baseline_audio = RecordingAudio::default();
    let baseline_avatar = RecordingAvatar::default();
    let shadow_audio = RecordingAudio::default();
    let shadow_avatar = RecordingAvatar::default();

    let mut baseline = app(
        FixedAssetRoute("reaction.agree.01"),
        Box::new(baseline_audio.clone()),
        Box::new(baseline_avatar.clone()),
        Box::new(NoopStreamOutput),
    );
    let identity = |policy_id: &str| ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: "v1".to_owned(),
        config_fingerprint: format!("{policy_id}-cfg"),
        runtime_profile: "cached".to_owned(),
        dataset_id: Some("reaction-quality-stage2".to_owned()),
    };
    let shadow_router = ShadowingRoutePlanner::new(
        FixedAssetRoute("reaction.agree.01"),
        FixedShadowDecision(ShadowDecision::silent()),
        identity("active"),
        identity("shadow"),
    )
    .expect("shadow router");
    let mut shadowed = app(
        shadow_router,
        Box::new(shadow_audio.clone()),
        Box::new(shadow_avatar.clone()),
        Box::new(NoopStreamOutput),
    );

    baseline.startup().expect("baseline startup");
    shadowed.startup().expect("shadow startup");
    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
    let baseline_outcome = baseline
        .process_content_bytes(&raw, 10, 7)
        .expect("baseline content");
    let shadow_outcome = shadowed
        .process_content_bytes(&raw, 10, 7)
        .expect("shadow content");

    assert_eq!(shadow_outcome, baseline_outcome);
    assert_eq!(
        *shadow_audio.commands.lock().expect("shadow audio"),
        *baseline_audio.commands.lock().expect("baseline audio")
    );
    assert_eq!(
        *shadow_avatar.commands.lock().expect("shadow avatar"),
        *baseline_avatar.commands.lock().expect("baseline avatar")
    );

    let comparison = shadowed
        .router
        .last_comparison()
        .expect("shadow comparison");
    assert_eq!(comparison.route_diverged, Some(true));
    assert_eq!(comparison.active.route, ShadowRouteClass::SemanticReuse);
    assert_eq!(
        comparison.shadow,
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision::silent()
        }
    );
}

/// #164: the bounded pool must not change active output either. The shadow
/// decision deliberately diverges, and the comparison only becomes visible
/// after a drain, so equality proves the orchestrator is observational.
#[test]
fn bounded_shadow_orchestration_keeps_active_playback_and_adapter_commands_equivalent() {
    let baseline_audio = RecordingAudio::default();
    let baseline_avatar = RecordingAvatar::default();
    let shadow_audio = RecordingAudio::default();
    let shadow_avatar = RecordingAvatar::default();

    let mut baseline = app(
        FixedAssetRoute("reaction.agree.01"),
        Box::new(baseline_audio.clone()),
        Box::new(baseline_avatar.clone()),
        Box::new(NoopStreamOutput),
    );
    let identity = |policy_id: &str| ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: "v1".to_owned(),
        config_fingerprint: format!("{policy_id}-cfg"),
        runtime_profile: "cached".to_owned(),
        dataset_id: Some("reaction-quality-stage2".to_owned()),
    };
    let shadow_router = ShadowingRoutePlanner::with_orchestrator(
        FixedAssetRoute("reaction.agree.01"),
        FixedShadowDecision(ShadowDecision::silent()),
        identity("active"),
        identity("shadow"),
        None,
        ShadowOrchestratorConfig {
            enabled: true,
            sample_rate_per_10k: 10_000,
            deadline_ms: 30_000,
            ..ShadowOrchestratorConfig::default()
        },
    )
    .expect("bounded shadow router");
    let mut shadowed = app(
        shadow_router,
        Box::new(shadow_audio.clone()),
        Box::new(shadow_avatar.clone()),
        Box::new(NoopStreamOutput),
    );

    baseline.startup().expect("baseline startup");
    shadowed.startup().expect("shadow startup");
    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
    let baseline_outcome = baseline
        .process_content_bytes(&raw, 10, 7)
        .expect("baseline content");
    let shadow_outcome = shadowed
        .process_content_bytes(&raw, 10, 7)
        .expect("shadow content");

    assert_eq!(shadow_outcome, baseline_outcome);
    assert_eq!(
        *shadow_audio.commands.lock().expect("shadow audio"),
        *baseline_audio.commands.lock().expect("baseline audio")
    );
    assert_eq!(
        *shadow_avatar.commands.lock().expect("shadow avatar"),
        *baseline_avatar.commands.lock().expect("baseline avatar")
    );

    // A diverging comparison arrives asynchronously; poll the bounded drain
    // rather than assuming it has already published.
    let mut comparison = None;
    for _ in 0..2_000 {
        shadowed.tick(11);
        if let Some(record) = shadowed.shadow_comparisons().last() {
            comparison = Some(record.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let comparison = comparison.expect("bounded shadow comparison");
    assert_eq!(comparison.route_diverged, Some(true));
    assert_eq!(comparison.active.route, ShadowRouteClass::SemanticReuse);
    assert_eq!(
        comparison.shadow,
        ShadowEvaluationOutcome::Evaluated {
            decision: ShadowDecision::silent()
        }
    );

    let snapshot = shadowed.shadow_execution_snapshot().expect("snapshot");
    assert!(snapshot.is_quiescent());
    shadowed.shutdown(20);
    let after = shadowed.shadow_execution_snapshot().expect("snapshot");
    assert_eq!(after.worker_running, 0);
    assert!(after.shutting_down);
}

/// #164: shadow work is free in the #69 ledger unless explicitly charged.
#[test]
fn shadow_budget_admission_follows_the_orchestrator_charge_flag() {
    let identity = |policy_id: &str| ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: "v1".to_owned(),
        config_fingerprint: format!("{policy_id}-cfg"),
        runtime_profile: "cached".to_owned(),
        dataset_id: None,
    };
    let mut free = app(
        ShadowingRoutePlanner::with_orchestrator(
            FixedAssetRoute("reaction.agree.01"),
            FixedShadowDecision(ShadowDecision::silent()),
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                enabled: true,
                sample_rate_per_10k: 10_000,
                deadline_ms: 30_000,
                ..ShadowOrchestratorConfig::default()
            },
        )
        .expect("free router"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    free = free.with_generative_budget_policy(GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(1),
        interval_ms: 60_000,
        ..GenerativeBudgetPolicy::default()
    });
    free.startup().expect("startup");
    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");

    // Default: shadow work is admitted without touching the ledger, so the
    // single-call budget is still available to active traffic afterwards.
    free.process_content_bytes(&raw, 10, 7).expect("content");
    assert!(!free.router.shadow_charges_budget());
    let governor = free.budget.as_mut().expect("budget governor");
    assert!(governor.try_admit(BudgetAdmission::ordinary(), 10).is_ok());

    let mut charged = app(
        ShadowingRoutePlanner::with_orchestrator(
            FixedAssetRoute("reaction.agree.01"),
            FixedShadowDecision(ShadowDecision::silent()),
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                enabled: true,
                sample_rate_per_10k: 10_000,
                deadline_ms: 30_000,
                charge_shadow_to_budget: true,
                ..ShadowOrchestratorConfig::default()
            },
        )
        .expect("charged router"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    charged = charged.with_generative_budget_policy(GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(1),
        interval_ms: 60_000,
        // The #69 governor policy is the authority on charging; the
        // orchestrator config only decides whether shadow work is offered
        // to it. Both must opt in.
        charge_shadow_to_budget: true,
        ..GenerativeBudgetPolicy::default()
    });
    charged.startup().expect("startup");
    charged.process_content_bytes(&raw, 10, 7).expect("content");
    assert!(charged.router.shadow_charges_budget());
    let governor = charged.budget.as_mut().expect("budget governor");
    assert!(
        governor.try_admit(BudgetAdmission::ordinary(), 10).is_err(),
        "explicitly charged shadow work must be visible in the same ledger"
    );
}

/// #164 review: the #69 governor must be the admission authority. Once the
/// ledger is exhausted, a shadow event must be refused *before* it is
/// queued: nothing runs, no completion is ever published for it, and the
/// recorded submission describes exactly that.
#[test]
fn budget_denied_shadow_work_is_never_queued_or_executed() {
    let identity = |policy_id: &str| ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: "v1".to_owned(),
        config_fingerprint: format!("{policy_id}-cfg"),
        runtime_profile: "cached".to_owned(),
        dataset_id: None,
    };
    let mut app = app(
        ShadowingRoutePlanner::with_orchestrator(
            FixedAssetRoute("reaction.agree.01"),
            FixedShadowDecision(ShadowDecision::silent()),
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                enabled: true,
                sample_rate_per_10k: 10_000,
                deadline_ms: 30_000,
                charge_shadow_to_budget: true,
                ..ShadowOrchestratorConfig::default()
            },
        )
        .expect("charged router"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app = app.with_generative_budget_policy(GenerativeBudgetPolicy {
        enabled: true,
        // One shadow admission only; the second event is over budget.
        max_llm_calls_per_interval: Some(1),
        interval_ms: 60_000,
        charge_shadow_to_budget: true,
        ..GenerativeBudgetPolicy::default()
    });
    app.startup().expect("startup");

    // First event: admitted, charged, and queued.
    app.process_content_bytes(&serde_json::to_vec(&chat_event(1)).expect("json"), 10, 7)
        .expect("content");
    assert!(matches!(
        app.router.last_shadow_submission(),
        Some(ShadowSubmission::Admitted { .. })
    ));

    // Second event: the governor refuses before routing, so the job is
    // never queued. The active route is unaffected either way. Spaced past
    // the scheduler's per-asset cooldown so the runtime itself admits it.
    app.process_content_bytes(&serde_json::to_vec(&chat_event(2)).expect("json"), 2_000, 8)
        .expect("content");
    assert_eq!(
        app.router.last_shadow_submission(),
        Some(&ShadowSubmission::Skipped {
            reason: ShadowSkipReason::BudgetDenied
        }),
        "a denied event must be reported as denied, not as admitted"
    );
    let snapshot = app.router.shadow_execution_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.submitted, 1,
        "only the admitted event may ever be queued"
    );
    assert_eq!(snapshot.dropped_budget_denied, 1);
    assert!(
        snapshot.pending + snapshot.in_flight <= 1,
        "the denied job must not occupy the queue or a worker"
    );
    app.tick(2_100);
    let denied_records = app
        .shadow_comparisons()
        .iter()
        .filter(|record| record.event_id == chat_event(2).event_id)
        .count();
    assert_eq!(
        denied_records, 0,
        "denied work must not produce shadow evidence"
    );
}

/// An active planner that fails for a designated event, so the staged
/// shadow job is stranded and must be discarded.
#[derive(Clone)]
struct FlakyActiveRoute {
    asset_id: &'static str,
    fail_sequence: u64,
}

impl RoutePlanner for FlakyActiveRoute {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        if event.sequence == self.fail_sequence {
            return Err(AppError::Routing("active route failed".to_owned()));
        }
        Ok(PlaybackRoute::AssetId(self.asset_id.to_owned()))
    }
}

fn charged_shadow_app(
    fail_sequence: u64,
) -> ProductionApp<ShadowingRoutePlanner<FlakyActiveRoute, FixedShadowDecision>> {
    let identity = |policy_id: &str| ShadowPolicyIdentity {
        policy_id: policy_id.to_owned(),
        policy_version: "v1".to_owned(),
        config_fingerprint: format!("{policy_id}-cfg"),
        runtime_profile: "cached".to_owned(),
        dataset_id: None,
    };
    let app = app(
        ShadowingRoutePlanner::with_orchestrator(
            FlakyActiveRoute {
                asset_id: "reaction.agree.01",
                fail_sequence,
            },
            FixedShadowDecision(ShadowDecision::silent()),
            identity("active"),
            identity("shadow"),
            None,
            ShadowOrchestratorConfig {
                enabled: true,
                sample_rate_per_10k: 10_000,
                deadline_ms: 30_000,
                charge_shadow_to_budget: true,
                ..ShadowOrchestratorConfig::default()
            },
        )
        .expect("charged router"),
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    );
    app.with_generative_budget_policy(GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(1),
        interval_ms: 60_000,
        charge_shadow_to_budget: true,
        ..GenerativeBudgetPolicy::default()
    })
}

/// #164 review (required regression): a denied event whose *active route*
/// fails must not leave a one-shot denial behind. The next, unrelated
/// successful event has to be admitted normally.
#[test]
fn denied_shadow_event_with_a_failed_active_route_does_not_leak_into_the_next_event() {
    let mut app = charged_shadow_app(1);
    app.startup().expect("startup");

    // Exhaust the single-call ledger so both later events *would* be
    // denied by the governor. The reservation is dropped immediately; the
    // call-ledger entry it recorded is what makes the budget tight.
    {
        let governor = app.budget.as_mut().expect("budget governor");
        governor
            .try_admit(BudgetAdmission::ordinary(), 10)
            .expect("seed the exhausted ledger");
    }

    // Event 1 would be denied, but its active route fails first.
    let error = app
        .process_content_bytes(&serde_json::to_vec(&chat_event(1)).expect("json"), 20, 7)
        .expect_err("the active route fails for sequence 1");
    assert!(matches!(error, AppError::Routing(_)));

    let after_failure = app.router.shadow_execution_snapshot().expect("snapshot");
    assert_eq!(
        after_failure.dropped_budget_denied, 0,
        "no denial may be recorded for an event that never committed"
    );
    assert!(
        !app.router.shadow_submission_staged(),
        "a failed active route must discard the staged job"
    );

    // Event 2 routes successfully and is denied by the governor, exactly
    // once.
    app.process_content_bytes(&serde_json::to_vec(&chat_event(2)).expect("json"), 2_000, 8)
        .expect("content");
    assert_eq!(
        app.router.last_shadow_submission(),
        Some(&ShadowSubmission::Skipped {
            reason: ShadowSkipReason::BudgetDenied
        })
    );
    assert_eq!(
        app.router
            .shadow_execution_snapshot()
            .expect("snapshot")
            .dropped_budget_denied,
        1,
        "only the committed denial may be counted"
    );

    // Event 3 must be admitted normally: a one-shot denial armed by the
    // failed event 1 would wrongly deny this one.
    let mut later = app;
    later.budget = None;
    later
        .process_content_bytes(&serde_json::to_vec(&chat_event(3)).expect("json"), 4_000, 9)
        .expect("content");
    assert!(
        matches!(
            later.router.last_shadow_submission(),
            Some(ShadowSubmission::Admitted { .. })
        ),
        "a leaked one-shot denial from a failed route would deny this event"
    );
}

/// #164 review (required regression): when the active route fails, the
/// #69 ledger must not be charged for shadow work that never ran.
#[test]
fn a_failed_active_route_charges_no_budget_for_staged_shadow_work() {
    let mut app = charged_shadow_app(1);
    app.startup().expect("startup");

    app.process_content_bytes(&serde_json::to_vec(&chat_event(1)).expect("json"), 10, 7)
        .expect_err("the active route fails for sequence 1");

    // The whole single-call budget must still be available to active work.
    let governor = app.budget.as_mut().expect("budget governor");
    assert!(
        governor.try_admit(BudgetAdmission::ordinary(), 20).is_ok(),
        "shadow work that never ran must not have consumed the call ledger"
    );

    let snapshot = app.router.shadow_execution_snapshot().expect("snapshot");
    assert_eq!(snapshot.submitted, 0, "nothing may have been queued");
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.in_flight, 0);
    assert!(snapshot.is_quiescent());
}

fn plain_app() -> ProductionApp<FixedSilentRoute> {
    app(
        FixedSilentRoute,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
}

fn budget_test_app<R>(router: R, policy: GenerativeBudgetPolicy) -> ProductionApp<R>
where
    R: RoutePlanner,
{
    app(
        router,
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generative_budget_policy(policy)
}

#[test]
fn budget_disabled_by_default_keeps_generation_unchanged() {
    let mut app = app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        Box::new(RecordingAudio::default()),
        Box::new(RecordingAvatar::default()),
        Box::new(NoopStreamOutput),
    )
    .with_generation(generative_runtime(
        MockThinking {
            reply: "no budget reply".to_owned(),
            failure: None,
        },
        MockTts::default(),
    ));
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
    let outcome = app.process_content_bytes(&raw, 10, 1).expect("content");
    assert!(outcome.playback.is_none(), "generation is deferred");
    wait_for_generation_completion(&mut app, 20);
    assert!(
        app.generative_budget_snapshot(20).is_empty(),
        "no governor means no budget records"
    );
    assert!(
        last_scheduled_plan(&app)
            .asset_id
            .starts_with("dynamic.generated."),
        "generation still completes without the budget governor"
    );
}

#[test]
fn budget_exhaustion_degrades_deterministically_with_typed_reason() {
    let policy = GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(1),
        interval_ms: 60_000,
        ..GenerativeBudgetPolicy::default()
    };
    let mut app = budget_test_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        policy,
    )
    .with_generation(generative_runtime(
        MockThinking {
            reply: "should never run".to_owned(),
            failure: None,
        },
        MockTts::default(),
    ));
    app.startup().expect("startup");

    // First request is admitted and completes normally.
    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
    app.process_content_bytes(&raw, 10, 1).expect("content");
    wait_for_generation_completion(&mut app, 20);

    // Second request inside the window is denied before provider work:
    // deterministic fallback (cached/non-verbal), never a provider call.
    let raw = serde_json::to_vec(&chat_event(2)).expect("event json");
    let outcome = app.process_content_bytes(&raw, 30, 2).expect("content");
    let _ = outcome;

    let observations: Vec<_> = app
        .telemetry()
        .events()
        .iter()
        .filter(|event| event.event_id == "evt-2")
        .cloned()
        .collect();
    let denied = observations
        .iter()
        .find(|event| event.budget_denial_reason.is_some())
        .expect("budget denial recorded in telemetry");
    assert_eq!(
        denied.budget_denial_reason.as_deref(),
        Some("llm_call_budget_exhausted")
    );
    assert_eq!(denied.fallback_reason.as_deref(), Some("budget_exhausted"));
    assert!(
        matches!(
            denied.route,
            RouteClass::CachedFallback | RouteClass::NonVerbalFallback
        ),
        "denial degrades deterministically, got {:?}",
        denied.route
    );
    assert_eq!(denied.llm_calls, 0, "no provider work on denial");

    // Accounting: one admission, one rejection visible in the snapshot.
    let snapshot = app.generative_budget_snapshot(30);
    let llm = snapshot
        .iter()
        .find(|record| record.budget_type == BudgetType::LlmCalls)
        .expect("llm budget record");
    assert_eq!(llm.limit, Some(1));
    assert_eq!(llm.rejected_or_degraded, 1);
}

#[test]
fn budget_window_reset_allows_generation_again() {
    let policy = GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(1),
        interval_ms: 100,
        ..GenerativeBudgetPolicy::default()
    };
    let mut app = budget_test_app(
        FixedGenerateRoute {
            reply_context: ReflexContext::default(),
            fallback_variant_group: None,
        },
        policy,
    )
    .with_generation(generative_runtime(
        MockThinking {
            reply: "window reply".to_owned(),
            failure: None,
        },
        MockTts::default(),
    ));
    app.startup().expect("startup");

    let raw = serde_json::to_vec(&chat_event(1)).expect("event json");
    app.process_content_bytes(&raw, 10, 1).expect("content");
    wait_for_generation_completion(&mut app, 20);

    // Denied inside the window.
    let raw = serde_json::to_vec(&chat_event(2)).expect("event json");
    app.process_content_bytes(&raw, 30, 2).expect("content");
    let denials_before = app
        .telemetry()
        .events()
        .iter()
        .filter(|event| event.budget_denial_reason.is_some())
        .count();
    assert_eq!(denials_before, 1);

    // Admitted again after the window expires; the first reservation was
    // settled, so the concurrency slot is free and the call ledger trimmed.
    let raw = serde_json::to_vec(&chat_event(3)).expect("event json");
    app.process_content_bytes(&raw, 500, 3).expect("content");
    wait_for_generation_completion(&mut app, 520);
    let denials_after = app
        .telemetry()
        .events()
        .iter()
        .filter(|event| event.budget_denial_reason.is_some())
        .count();
    assert_eq!(denials_after, 1, "window reset admits again");
}

#[test]
fn budget_high_priority_reserve_favors_trusted_deadline_class() {
    let policy = GenerativeBudgetPolicy {
        enabled: true,
        max_llm_calls_per_interval: Some(2),
        interval_ms: 60_000,
        high_priority_reserve_percent: 50,
        ..GenerativeBudgetPolicy::default()
    };
    let mut governor = GenerativeBudgetGovernor::new(policy);
    // Ordinary share = 1; high priority can spend the reserved slot.
    // (Priority derivation itself is exercised through the deadline class
    // mapping in the admission path; the governor math is unit-tested in
    // budget.rs.)
    assert!(governor.try_admit(BudgetAdmission::ordinary(), 0).is_ok());
    assert!(governor.try_admit(BudgetAdmission::ordinary(), 1).is_err());
    assert!(
        governor
            .try_admit(BudgetAdmission::high_priority(), 2)
            .is_ok()
    );
}

#[test]
fn support_bundle_includes_budget_snapshot_when_enabled() {
    let policy = GenerativeBudgetPolicy {
        enabled: true,
        max_concurrent_generations: Some(2),
        interval_ms: 60_000,
        ..GenerativeBudgetPolicy::default()
    };
    let mut app = budget_test_app(FixedSilentRoute, policy);
    app.startup().expect("startup");

    let bundle = generate_support_bundle(&app, "cached", "fp-budget", 5_000);
    let concurrent = bundle
        .resources
        .generative_budget
        .iter()
        .find(|record| record.budget_type == BudgetType::Concurrent)
        .expect("concurrent budget record in support bundle");
    assert_eq!(concurrent.limit, Some(2));
    assert_eq!(concurrent.consumed, 0);

    // Disabled budgets contribute no records.
    let plain = plain_app();
    let bundle = generate_support_bundle(&plain, "cached", "fp-plain", 5_000);
    assert!(bundle.resources.generative_budget.is_empty());
}

/// Issue #277: the operator emergency stop ends audio that already
/// started. No speech/floor/barge-in feature is enabled in any of these.
mod operator_stop_audio;
