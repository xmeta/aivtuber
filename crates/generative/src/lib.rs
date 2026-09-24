#![forbid(unsafe_code)]

//! Generative fallback orchestration.
//!
//! Generation is intentionally staged outside the scheduler. No scheduler
//! mutation occurs until the caller receives a fully validated Performance
//! Asset, so cancellation or partial backend output cannot corrupt playback.

use aivtuber_asset_store::{
    AssetClass, AssetCompatibility, AssetStore, ExpressionTrack, GazeTarget,
    PERFORMANCE_ASSET_SCHEMA_VERSION, PerformanceAsset, Provenance, SpeechTrack, TimelineEvent,
};
use aivtuber_domain::{
    BackendIdentity, EngineError, EngineErrorKind, EventEnvelope, EventKind, FallbackReason,
    SpeechArtifact, SpeechProgress, SpeechProgressSink, SpeechRequest, ThinkingEngine,
    ThinkingRequest, TtsBackendIdentity, TtsEngine,
};
use aivtuber_scheduler::Priority;
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::error::Error;
use std::fmt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationRoutingReason {
    ExplicitLlmRoute,
    NoCachedMatch,
    LowSemanticConfidence,
    NovelResponseRequested,
    CachedRouteFailed,
}

impl GenerationRoutingReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExplicitLlmRoute => "explicit_llm_route",
            Self::NoCachedMatch => "no_cached_match",
            Self::LowSemanticConfidence => "low_semantic_confidence",
            Self::NovelResponseRequested => "novel_response_requested",
            Self::CachedRouteFailed => "cached_route_failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmCallRecord {
    pub event_id: String,
    pub routing_reason: GenerationRoutingReason,
    pub backend: BackendIdentity,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationStage {
    BeforeThinking,
    AfterThinking,
    AfterTts,
    BeforePublish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FallbackDirective {
    CachedReaction {
        variant_group: String,
        asset_id: String,
    },
    NonVerbalReaction {
        reaction_family: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationTrace {
    pub llm_calls: Vec<LlmCallRecord>,
    pub tts_attempted: bool,
    pub tts_streaming: bool,
    pub tts_progress_chunks: usize,
    pub tts_backend: Option<TtsBackendIdentity>,
    pub fallback_reason: FallbackReason,
    pub cancelled_stage: Option<CancellationStage>,
}

impl Default for GenerationTrace {
    fn default() -> Self {
        Self {
            llm_calls: Vec::new(),
            tts_attempted: false,
            tts_streaming: false,
            tts_progress_chunks: 0,
            tts_backend: None,
            fallback_reason: FallbackReason::None,
            cancelled_stage: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GenerationDisposition {
    Generated { asset: Box<PerformanceAsset> },
    Fallback { directive: FallbackDirective },
    Cancelled { stage: CancellationStage },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationResult {
    pub trace: GenerationTrace,
    pub disposition: GenerationDisposition,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub source_event: EventEnvelope,
    pub thinking: ThinkingRequest,
    pub routing_reason: GenerationRoutingReason,
    pub intent: String,
    pub style: Option<String>,
    pub fallback_variant_group: Option<String>,
    pub seed: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PerformanceCompilerConfig {
    pub compiler_version: String,
    pub avatar_profile: Option<String>,
    pub motion_library: Option<String>,
    pub expression_preset: Option<String>,
    pub expression_intensity: f64,
}

impl Default for PerformanceCompilerConfig {
    fn default() -> Self {
        Self {
            compiler_version: PERFORMANCE_ASSET_SCHEMA_VERSION.to_owned(),
            avatar_profile: None,
            motion_library: None,
            expression_preset: Some("speaking.neutral".to_owned()),
            expression_intensity: 0.4,
        }
    }
}

#[derive(Debug)]
pub struct GenerationError {
    message: String,
}

impl GenerationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for GenerationError {}

#[derive(Debug, Clone, Default)]
pub struct GenerationCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl GenerationCancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
struct ActiveGeneration {
    event_id: String,
    priority: Priority,
    token: GenerationCancellationToken,
}

#[derive(Debug, Default)]
pub struct GenerationCancellationRegistry {
    active: Mutex<Option<ActiveGeneration>>,
}

impl GenerationCancellationRegistry {
    pub fn begin(&self, event: &EventEnvelope) -> GenerationCancellationToken {
        let token = GenerationCancellationToken::default();
        let active = ActiveGeneration {
            event_id: event.event_id.clone(),
            priority: generation_priority(event.kind),
            token: token.clone(),
        };
        let mut guard = self.active.lock().expect("generation registry lock");
        if let Some(previous) = guard.replace(active) {
            previous.token.cancel();
        }
        token
    }

    pub fn cancel_for_event(&self, event: &EventEnvelope) -> bool {
        let guard = self.active.lock().expect("generation registry lock");
        let Some(active) = guard.as_ref() else {
            return false;
        };
        if generation_priority(event.kind).rank() < active.priority.rank() {
            active.token.cancel();
            true
        } else {
            false
        }
    }

    pub fn finish(&self, event_id: &str) {
        let mut guard = self.active.lock().expect("generation registry lock");
        if guard
            .as_ref()
            .is_some_and(|active| active.event_id == event_id)
        {
            *guard = None;
        }
    }

    pub fn active_event_id(&self) -> Option<String> {
        self.active
            .lock()
            .expect("generation registry lock")
            .as_ref()
            .map(|active| active.event_id.clone())
    }
}
#[derive(Debug, Clone)]
pub struct PerformanceCompiler {
    config: PerformanceCompilerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingSpeechUpdate {
    pub audio_ref: String,
    pub duration_ms: u64,
    pub viseme_ref: Option<String>,
    pub final_chunk: bool,
}

/// Incremental compiler state for providers that can stream text and/or TTS
/// into a stable media reference. Partial state is never a Performance Asset.
#[derive(Debug, Clone)]
pub struct StreamingCompilationSession {
    request: GenerationRequest,
    thinking: BackendIdentity,
    tts: TtsBackendIdentity,
    reply_text: String,
    speech: Option<SpeechArtifact>,
    speech_final: bool,
    cancelled: bool,
}

impl StreamingCompilationSession {
    pub fn new(
        request: GenerationRequest,
        thinking: BackendIdentity,
        tts: TtsBackendIdentity,
    ) -> Self {
        Self {
            request,
            thinking,
            tts,
            reply_text: String::new(),
            speech: None,
            speech_final: false,
            cancelled: false,
        }
    }

    pub fn push_text_chunk(&mut self, chunk: &str) -> Result<(), GenerationError> {
        self.ensure_active()?;
        self.reply_text.push_str(chunk);
        Ok(())
    }

    pub fn push_speech_update(
        &mut self,
        update: StreamingSpeechUpdate,
    ) -> Result<(), GenerationError> {
        self.ensure_active()?;
        if update.audio_ref.trim().is_empty() || update.duration_ms == 0 {
            return Err(GenerationError::new(
                "streaming speech update requires audio_ref and positive duration_ms",
            ));
        }

        if let Some(existing) = &self.speech {
            if existing.audio_ref != update.audio_ref {
                return Err(GenerationError::new(
                    "streaming speech updates must use one stable audio_ref",
                ));
            }
            if update.duration_ms < existing.duration_ms {
                return Err(GenerationError::new(
                    "streaming speech duration must be monotonic",
                ));
            }
        }

        self.speech = Some(SpeechArtifact {
            audio_ref: update.audio_ref,
            duration_ms: update.duration_ms,
            viseme_ref: update.viseme_ref,
        });
        self.speech_final |= update.final_chunk;
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn finalize(
        self,
        compiler: &PerformanceCompiler,
    ) -> Result<PerformanceAsset, GenerationError> {
        if self.cancelled {
            return Err(GenerationError::new(
                "streaming compilation was cancelled before publish",
            ));
        }
        if !self.speech_final {
            return Err(GenerationError::new(
                "streaming speech is incomplete; final chunk was not received",
            ));
        }
        let speech = self
            .speech
            .as_ref()
            .ok_or_else(|| GenerationError::new("streaming speech produced no artifact"))?;
        compiler.compile(
            &self.request,
            &self.reply_text,
            speech,
            &self.thinking,
            &self.tts,
        )
    }

    fn ensure_active(&self) -> Result<(), GenerationError> {
        if self.cancelled {
            Err(GenerationError::new(
                "streaming compilation is already cancelled",
            ))
        } else if self.speech_final {
            Err(GenerationError::new(
                "streaming compilation already received its final speech chunk",
            ))
        } else {
            Ok(())
        }
    }
}

impl PerformanceCompiler {
    pub fn new(config: PerformanceCompilerConfig) -> Result<Self, GenerationError> {
        if config.compiler_version.trim().is_empty() {
            return Err(GenerationError::new("compiler_version must not be empty"));
        }
        if !config.expression_intensity.is_finite()
            || !(0.0..=1.0).contains(&config.expression_intensity)
        {
            return Err(GenerationError::new(
                "expression_intensity must be a finite value in 0..=1",
            ));
        }
        Ok(Self { config })
    }

    pub fn compile(
        &self,
        request: &GenerationRequest,
        reply_text: &str,
        speech: &SpeechArtifact,
        thinking: &BackendIdentity,
        tts: &TtsBackendIdentity,
    ) -> Result<PerformanceAsset, GenerationError> {
        if request.intent.trim().is_empty() {
            return Err(GenerationError::new("generation intent must not be empty"));
        }
        if reply_text.trim().is_empty() {
            return Err(GenerationError::new("generated reply must not be empty"));
        }
        if speech.audio_ref.trim().is_empty() || speech.duration_ms == 0 {
            return Err(GenerationError::new(
                "speech artifact requires audio_ref and positive duration_ms",
            ));
        }

        let asset_id = generated_asset_id(&request.source_event, reply_text, thinking, tts);
        let expression = self
            .config
            .expression_preset
            .as_ref()
            .map(|preset| ExpressionTrack {
                preset: preset.clone(),
                intensity: self.config.expression_intensity,
            });

        let mut timeline = Vec::new();
        if let Some(expression) = &expression {
            timeline.push(TimelineEvent {
                at_ms: 0,
                event: "expression.start".to_owned(),
                payload: Some(json!({ "preset": expression.preset })),
            });
        }
        timeline.push(TimelineEvent {
            at_ms: 0,
            event: "speech.start".to_owned(),
            payload: None,
        });
        timeline.push(TimelineEvent {
            at_ms: speech.duration_ms,
            event: "speech.end".to_owned(),
            payload: None,
        });
        let asset = PerformanceAsset {
            schema_version: PERFORMANCE_ASSET_SCHEMA_VERSION.to_owned(),
            id: asset_id,
            intent: request.intent.clone(),
            class: AssetClass::Dynamic,
            variant_group: None,
            speech: Some(SpeechTrack {
                text: Some(reply_text.to_owned()),
                audio_ref: Some(speech.audio_ref.clone()),
                duration_ms: Some(speech.duration_ms),
                viseme_ref: speech.viseme_ref.clone(),
            }),
            expression,
            gesture: None,
            gaze: Some(GazeTarget::Camera),
            timeline,
            interrupt_points_ms: vec![speech.duration_ms],
            variation: None,
            compatibility: AssetCompatibility {
                compiler_version: self.config.compiler_version.clone(),
                voice_model: tts.voice_model.clone(),
                avatar_profile: self.config.avatar_profile.clone(),
                viseme_mapping: tts.viseme_mapping.clone(),
                motion_library: self.config.motion_library.clone(),
            },
            semantic_embedding: None,
            provenance: Some(Provenance {
                generated: Some(true),
                generator: Some("aivtuber-generative".to_owned()),
                created_at: Some(request.source_event.observed_at.clone()),
                thinking_backend: Some(thinking.name.clone()),
                thinking_model_alias: thinking.model_alias.clone(),
                thinking_model_version: thinking.model_version.clone(),
                tts_backend: Some(tts.backend.name.clone()),
                tts_model_alias: tts.backend.model_alias.clone(),
                tts_model_version: tts.backend.model_version.clone(),
                routing_reason: Some(request.routing_reason.as_str().to_owned()),
            }),
        };

        asset
            .validate()
            .map_err(|error| GenerationError::new(format!("compiled asset is invalid: {error}")))?;
        Ok(asset)
    }
}

#[derive(Debug, Default)]
struct StreamingSpeechCollector {
    progress: Vec<SpeechProgress>,
}

impl SpeechProgressSink for StreamingSpeechCollector {
    fn push(&mut self, progress: SpeechProgress) -> Result<(), EngineError> {
        if progress.audio_ref.trim().is_empty() || progress.duration_ms == 0 {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "streaming TTS progress requires audio_ref and positive duration_ms",
            ));
        }
        if let Some(previous) = self.progress.last() {
            if progress.sequence <= previous.sequence {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "streaming TTS sequence must be strictly increasing",
                ));
            }
            if progress.audio_ref != previous.audio_ref {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "streaming TTS must keep one stable audio_ref",
                ));
            }
            if progress.duration_ms < previous.duration_ms {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "streaming TTS duration must be monotonic",
                ));
            }
            if previous.final_chunk {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    "streaming TTS emitted data after final_chunk",
                ));
            }
        }
        self.progress.push(progress);
        Ok(())
    }
}

impl StreamingSpeechCollector {
    fn validate_final(&self, artifact: &SpeechArtifact) -> Result<(), EngineError> {
        let final_progress = self.progress.last().ok_or_else(|| {
            EngineError::new(
                EngineErrorKind::Backend,
                "streaming TTS returned no progress chunks",
            )
        })?;
        if !final_progress.final_chunk {
            return Err(EngineError::new(
                EngineErrorKind::Backend,
                "streaming TTS completed without final_chunk",
            ));
        }
        if final_progress.audio_ref != artifact.audio_ref
            || final_progress.duration_ms != artifact.duration_ms
        {
            return Err(EngineError::new(
                EngineErrorKind::Backend,
                "streaming TTS final progress does not match final artifact",
            ));
        }
        Ok(())
    }
}

fn run_streaming_tts(
    tts: &dyn TtsEngine,
    request: &SpeechRequest,
) -> Option<(
    Result<SpeechArtifact, EngineError>,
    StreamingSpeechCollector,
)> {
    let mut progress = StreamingSpeechCollector::default();
    let streaming = tts.synthesize_streaming(request, &mut progress)?;
    let result = block_on(streaming);
    Some((result, progress))
}

pub struct GenerativePipeline {
    thinking: Arc<dyn ThinkingEngine>,
    tts: Arc<dyn TtsEngine>,
    compiler: PerformanceCompiler,
}

impl fmt::Debug for GenerativePipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerativePipeline")
            .field("thinking", &self.thinking.identity())
            .field("tts", &self.tts.identity())
            .field("compiler", &self.compiler)
            .finish()
    }
}

impl GenerativePipeline {
    pub fn new(
        thinking: Arc<dyn ThinkingEngine>,
        tts: Arc<dyn TtsEngine>,
        compiler: PerformanceCompiler,
    ) -> Self {
        Self {
            thinking,
            tts,
            compiler,
        }
    }

    pub fn run(
        &self,
        request: &GenerationRequest,
        cancellation: &GenerationCancellationToken,
        fallback_assets: Option<&AssetStore>,
    ) -> Result<GenerationResult, GenerationError> {
        request
            .source_event
            .validate()
            .map_err(|error| GenerationError::new(format!("invalid generation event: {error}")))?;
        request
            .thinking
            .validate()
            .map_err(|error| GenerationError::new(format!("invalid thinking request: {error}")))?;
        if request.thinking.input.event_id.as_deref()
            != Some(request.source_event.event_id.as_str())
            || request.thinking.input.event_kind != Some(request.source_event.kind)
            || request.thinking.input.source_class != Some(request.source_event.source_class)
            || request.thinking.input.trust_level != request.source_event.trust_level
        {
            return Err(GenerationError::new(
                "thinking request input provenance must match source_event",
            ));
        }
        if request.intent.trim().is_empty() {
            return Err(GenerationError::new("generation intent must not be empty"));
        }

        let mut trace = GenerationTrace::default();
        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::BeforeThinking));
        }

        let thinking_identity = self.thinking.identity();
        trace.llm_calls.push(LlmCallRecord {
            event_id: request.source_event.event_id.clone(),
            routing_reason: request.routing_reason.clone(),
            backend: thinking_identity.clone(),
        });

        let reply = match block_on(self.thinking.generate(&request.thinking)) {
            Ok(reply) => reply,
            Err(error) => {
                trace.fallback_reason = fallback_reason_from_error(&error);
                return Ok(fallback_result(request, fallback_assets, trace));
            }
        };

        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::AfterThinking));
        }

        let tts_identity = self.tts.identity();
        trace.tts_attempted = true;
        trace.tts_backend = Some(tts_identity.clone());
        let speech_request = SpeechRequest {
            text: reply.text.clone(),
            style: request.style.clone(),
        };

        let speech_result = if let Some((result, progress)) =
            run_streaming_tts(self.tts.as_ref(), &speech_request)
        {
            trace.tts_streaming = true;
            trace.tts_progress_chunks = progress.progress.len();
            match result {
                Ok(speech) => progress.validate_final(&speech).map(|()| speech),
                Err(error) => Err(error),
            }
        } else {
            block_on(self.tts.synthesize(&speech_request))
        };

        let speech = match speech_result {
            Ok(speech) => speech,
            Err(error) => {
                trace.fallback_reason = fallback_reason_from_error(&error);
                return Ok(fallback_result(request, fallback_assets, trace));
            }
        };

        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::AfterTts));
        }

        let asset = match self.compiler.compile(
            request,
            &reply.text,
            &speech,
            &thinking_identity,
            &tts_identity,
        ) {
            Ok(asset) => asset,
            Err(_) => {
                trace.fallback_reason = FallbackReason::InvalidRequest;
                return Ok(fallback_result(request, fallback_assets, trace));
            }
        };

        if cancellation.is_cancelled() {
            return Ok(cancelled_result(trace, CancellationStage::BeforePublish));
        }

        Ok(GenerationResult {
            trace,
            disposition: GenerationDisposition::Generated {
                asset: Box::new(asset),
            },
        })
    }
}
fn cancelled_result(mut trace: GenerationTrace, stage: CancellationStage) -> GenerationResult {
    trace.cancelled_stage = Some(stage);
    GenerationResult {
        trace,
        disposition: GenerationDisposition::Cancelled { stage },
    }
}

fn fallback_result(
    request: &GenerationRequest,
    fallback_assets: Option<&AssetStore>,
    trace: GenerationTrace,
) -> GenerationResult {
    let reaction_family = request
        .fallback_variant_group
        .clone()
        .unwrap_or_else(|| "reaction.neutral".to_owned());

    let directive = match fallback_assets
        .and_then(|assets| assets.select_variant_id(&reaction_family, request.seed, &[]))
    {
        Some(asset_id) => FallbackDirective::CachedReaction {
            variant_group: reaction_family.clone(),
            asset_id,
        },
        None => FallbackDirective::NonVerbalReaction { reaction_family },
    };

    GenerationResult {
        trace,
        disposition: GenerationDisposition::Fallback { directive },
    }
}

fn fallback_reason_from_error(error: &EngineError) -> FallbackReason {
    match error.kind {
        EngineErrorKind::Timeout => FallbackReason::Timeout,
        EngineErrorKind::Unavailable => FallbackReason::Unavailable,
        EngineErrorKind::RateLimited => FallbackReason::RateLimited,
        EngineErrorKind::Overloaded => FallbackReason::Overloaded,
        EngineErrorKind::Authentication => FallbackReason::Authentication,
        EngineErrorKind::InvalidRequest => FallbackReason::InvalidRequest,
        EngineErrorKind::Unauthorized | EngineErrorKind::Backend => FallbackReason::Unavailable,
    }
}
fn generation_priority(kind: EventKind) -> Priority {
    match kind {
        EventKind::OperatorCommand => Priority::Operator,
        EventKind::ChatDonation => Priority::HighPriorityInteraction,
        EventKind::GameEvent => Priority::StrongReaction,
        EventKind::ChatMessage | EventKind::SpeechInput => Priority::Conversation,
        EventKind::StreamEvent => Priority::Commentary,
        EventKind::TimerTick | EventKind::SystemHealth => Priority::Background,
    }
}

fn generated_asset_id(
    event: &EventEnvelope,
    reply_text: &str,
    thinking: &BackendIdentity,
    tts: &TtsBackendIdentity,
) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    hash_bytes(&mut hash, event.event_id.as_bytes());
    hash_bytes(&mut hash, &event.sequence.to_le_bytes());
    hash_bytes(&mut hash, reply_text.as_bytes());
    hash_bytes(&mut hash, thinking.name.as_bytes());
    if let Some(alias) = thinking.model_alias.as_deref() {
        hash_bytes(&mut hash, alias.as_bytes());
    }
    if let Some(version) = thinking.model_version.as_deref() {
        hash_bytes(&mut hash, version.as_bytes());
    }
    hash_bytes(&mut hash, tts.backend.name.as_bytes());
    if let Some(alias) = tts.backend.model_alias.as_deref() {
        hash_bytes(&mut hash, alias.as_bytes());
    }
    if let Some(version) = tts.backend.model_version.as_deref() {
        hash_bytes(&mut hash, version.as_bytes());
    }
    format!("dynamic.generated.{hash:016x}")
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_asset_store::{RuntimeCompatibility, load_asset_file};
    use aivtuber_domain::{
        EngineFuture, GeneratedReply, PrivacyClass, ReflexContext, RetrievalSnapshot,
        SecurityPlane, SourceClass, TrustLevel,
    };
    use aivtuber_scheduler::{BlendChannel, PlannedPerformance, Scheduler, SchedulerConfig};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[derive(Clone)]
    struct MockThinking {
        result: Result<GeneratedReply, EngineError>,
        calls: Arc<AtomicUsize>,
        cancel_on_call: Option<GenerationCancellationToken>,
    }

    impl MockThinking {
        fn success(text: &str) -> Self {
            Self {
                result: Ok(GeneratedReply {
                    text: text.to_owned(),
                }),
                calls: Arc::new(AtomicUsize::new(0)),
                cancel_on_call: None,
            }
        }

        fn failure(kind: EngineErrorKind) -> Self {
            Self {
                result: Err(EngineError::new(kind, "thinking failed")),
                calls: Arc::new(AtomicUsize::new(0)),
                cancel_on_call: None,
            }
        }
    }

    impl ThinkingEngine for MockThinking {
        fn generate<'a>(
            &'a self,
            _request: &'a ThinkingRequest,
        ) -> EngineFuture<'a, GeneratedReply> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(token) = &self.cancel_on_call {
                token.cancel();
            }
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn identity(&self) -> BackendIdentity {
            BackendIdentity {
                name: "mock-thinking".to_owned(),
                model_alias: Some("mock-reasoner".to_owned()),
                model_version: Some("2026-09-24".to_owned()),
            }
        }
    }
    #[derive(Clone)]
    struct MockTts {
        result: Result<SpeechArtifact, EngineError>,
        calls: Arc<AtomicUsize>,
        cancel_on_call: Option<GenerationCancellationToken>,
        streaming: bool,
    }

    impl MockTts {
        fn success() -> Self {
            Self {
                result: Ok(SpeechArtifact {
                    audio_ref: "audio://generated/mock.opus".to_owned(),
                    duration_ms: 1_200,
                    viseme_ref: Some("viseme/generated/mock.json".to_owned()),
                }),
                calls: Arc::new(AtomicUsize::new(0)),
                cancel_on_call: None,
                streaming: false,
            }
        }

        fn streaming_success() -> Self {
            let mut mock = Self::success();
            mock.streaming = true;
            mock
        }

        fn failure(kind: EngineErrorKind) -> Self {
            Self {
                result: Err(EngineError::new(kind, "tts failed")),
                calls: Arc::new(AtomicUsize::new(0)),
                cancel_on_call: None,
                streaming: false,
            }
        }
    }

    impl TtsEngine for MockTts {
        fn synthesize<'a>(
            &'a self,
            _request: &'a SpeechRequest,
        ) -> EngineFuture<'a, SpeechArtifact> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(token) = &self.cancel_on_call {
                token.cancel();
            }
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn synthesize_streaming<'a>(
            &'a self,
            _request: &'a SpeechRequest,
            sink: &'a mut dyn SpeechProgressSink,
        ) -> Option<EngineFuture<'a, SpeechArtifact>> {
            if !self.streaming {
                return None;
            }
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if let Some(token) = &self.cancel_on_call {
                token.cancel();
            }
            let result = self.result.clone();
            Some(Box::pin(async move {
                let artifact = result?;
                sink.push(SpeechProgress {
                    sequence: 0,
                    audio_ref: artifact.audio_ref.clone(),
                    duration_ms: artifact.duration_ms / 2,
                    viseme_ref: None,
                    final_chunk: false,
                })?;
                sink.push(SpeechProgress {
                    sequence: 1,
                    audio_ref: artifact.audio_ref.clone(),
                    duration_ms: artifact.duration_ms,
                    viseme_ref: artifact.viseme_ref.clone(),
                    final_chunk: true,
                })?;
                Ok(artifact)
            }))
        }

        fn identity(&self) -> TtsBackendIdentity {
            TtsBackendIdentity {
                backend: BackendIdentity {
                    name: "mock-tts".to_owned(),
                    model_alias: Some("tts-fast".to_owned()),
                    model_version: Some("2026-09".to_owned()),
                },
                voice_model: Some("voice-ja-v2".to_owned()),
                viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            }
        }
    }
    fn chat_event(sequence: u64) -> EventEnvelope {
        let mut event: EventEnvelope =
            serde_json::from_str(include_str!("../../../examples/events/chat-message.json"))
                .expect("chat fixture");
        event.event_id = format!("evt-generation-{sequence}");
        event.correlation_id = "corr-generation".to_owned();
        event.sequence = sequence;
        event
    }

    fn donation_event(sequence: u64) -> EventEnvelope {
        let mut event = chat_event(sequence);
        event.event_id = format!("evt-donation-{sequence}");
        event.source = "donation".to_owned();
        event.source_class = SourceClass::Donation;
        event.trust_level = TrustLevel::SemiTrusted;
        event.kind = EventKind::ChatDonation;
        event
    }

    fn timer_event(sequence: u64) -> EventEnvelope {
        let mut event = chat_event(sequence);
        event.event_id = format!("evt-timer-{sequence}");
        event.source = "timer".to_owned();
        event.source_class = SourceClass::Timer;
        event.plane = SecurityPlane::System;
        event.trust_level = TrustLevel::Trusted;
        event.kind = EventKind::TimerTick;
        event.actor_id = None;
        event
    }

    fn generation_request(sequence: u64) -> GenerationRequest {
        let source_event = chat_event(sequence);
        let thinking = ThinkingRequest::from_event(
            &source_event,
            "hello from the current event",
            PrivacyClass::Pseudonymous,
            ReflexContext::default(),
            RetrievalSnapshot::default(),
            Vec::new(),
        );
        GenerationRequest {
            source_event,
            thinking,
            routing_reason: GenerationRoutingReason::NoCachedMatch,
            intent: "dynamic.reply".to_owned(),
            style: Some("cheerful".to_owned()),
            fallback_variant_group: Some("reaction.surprise".to_owned()),
            seed: 42,
        }
    }

    fn compiler() -> PerformanceCompiler {
        PerformanceCompiler::new(PerformanceCompilerConfig {
            compiler_version: "0.1.0".to_owned(),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
            expression_preset: Some("speaking.neutral".to_owned()),
            expression_intensity: 0.4,
        })
        .expect("compiler")
    }

    fn runtime_compatibility() -> RuntimeCompatibility {
        RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("voice-ja-v2".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v2".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        }
    }
    #[test]
    fn successful_generation_records_routing_reason_and_full_provenance() {
        let thinking = Arc::new(MockThinking::success("生成された返答です"));
        let tts = Arc::new(MockTts::success());
        let pipeline = GenerativePipeline::new(thinking.clone(), tts.clone(), compiler());
        let request = generation_request(1);
        let token = GenerationCancellationToken::default();

        let result = pipeline
            .run(&request, &token, None)
            .expect("generation must succeed");

        assert_eq!(result.trace.llm_calls.len(), 1);
        assert_eq!(
            result.trace.llm_calls[0].routing_reason,
            GenerationRoutingReason::NoCachedMatch
        );
        assert_eq!(thinking.calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(tts.calls.load(AtomicOrdering::SeqCst), 1);

        let GenerationDisposition::Generated { asset } = result.disposition else {
            panic!("expected generated asset");
        };
        asset.validate().expect("compiled asset remains valid");

        let provenance = asset.provenance.as_ref().expect("provenance");
        assert_eq!(provenance.generated, Some(true));
        assert_eq!(
            provenance.thinking_backend.as_deref(),
            Some("mock-thinking")
        );
        assert_eq!(
            provenance.thinking_model_version.as_deref(),
            Some("2026-09-24")
        );
        assert_eq!(provenance.tts_backend.as_deref(), Some("mock-tts"));
        assert_eq!(provenance.tts_model_version.as_deref(), Some("2026-09"));
        assert_eq!(
            provenance.routing_reason.as_deref(),
            Some("no_cached_match")
        );

        let mut store = AssetStore::new(".", runtime_compatibility());
        let inserted = store
            .insert_hot((*asset).clone())
            .expect("generated asset is promotable");
        assert_eq!(inserted.id, asset.id);
    }
    #[test]
    fn deterministic_inputs_compile_to_the_same_promotable_asset_id() {
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::success("同じ返答")),
            Arc::new(MockTts::success()),
            compiler(),
        );
        let request = generation_request(2);

        let first = pipeline
            .run(&request, &GenerationCancellationToken::default(), None)
            .expect("first");
        let second = pipeline
            .run(&request, &GenerationCancellationToken::default(), None)
            .expect("second");

        let GenerationDisposition::Generated { asset: first } = first.disposition else {
            panic!("generated");
        };
        let GenerationDisposition::Generated { asset: second } = second.disposition else {
            panic!("generated");
        };
        assert_eq!(first.id, second.id);
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize"),
            serde_json::to_vec(&second).expect("serialize")
        );
    }

    #[test]
    fn llm_failure_still_records_the_routing_reason_before_fallback() {
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::failure(EngineErrorKind::Timeout)),
            Arc::new(MockTts::success()),
            compiler(),
        );
        let request = generation_request(3);

        let result = pipeline
            .run(&request, &GenerationCancellationToken::default(), None)
            .expect("fallback");

        assert_eq!(result.trace.llm_calls.len(), 1);
        assert_eq!(
            result.trace.llm_calls[0].routing_reason,
            GenerationRoutingReason::NoCachedMatch
        );
        assert_eq!(result.trace.fallback_reason, FallbackReason::Timeout);
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Fallback {
                directive: FallbackDirective::NonVerbalReaction { .. }
            }
        ));
    }

    fn fallback_store() -> AssetStore {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/performance-assets/valid/reaction-surprise.json");
        let asset = load_asset_file(fixture).expect("fallback fixture");
        let runtime = RuntimeCompatibility {
            compiler_version: "0.1.0".to_owned(),
            voice_model: Some("example-voice-v1".to_owned()),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
        };
        let mut store = AssetStore::new(".", runtime);
        store.insert_hot(asset).expect("insert fallback");
        store
    }

    #[test]
    fn pipeline_uses_streaming_tts_when_backend_supports_it() {
        let tts = MockTts::streaming_success();
        let calls = Arc::clone(&tts.calls);
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::success("streamed speech")),
            Arc::new(tts),
            compiler(),
        );

        let result = pipeline
            .run(
                &generation_request(14),
                &GenerationCancellationToken::default(),
                None,
            )
            .expect("streaming generation");

        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert!(result.trace.tts_streaming);
        assert_eq!(result.trace.tts_progress_chunks, 2);
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Generated { .. }
        ));
    }

    #[test]
    fn tts_failure_degrades_to_cached_reaction_when_available() {
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::success("音声化する返答")),
            Arc::new(MockTts::failure(EngineErrorKind::Unavailable)),
            compiler(),
        );
        let request = generation_request(4);
        let store = fallback_store();

        let result = pipeline
            .run(
                &request,
                &GenerationCancellationToken::default(),
                Some(&store),
            )
            .expect("fallback");

        assert_eq!(result.trace.fallback_reason, FallbackReason::Unavailable);
        assert!(result.trace.tts_attempted);
        match result.disposition {
            GenerationDisposition::Fallback {
                directive:
                    FallbackDirective::CachedReaction {
                        variant_group,
                        asset_id,
                    },
            } => {
                assert_eq!(variant_group, "reaction.surprise");
                assert_eq!(asset_id, "reaction.surprise.01");
            }
            other => panic!("unexpected fallback: {other:?}"),
        }
    }

    #[test]
    fn tts_failure_degrades_to_nonverbal_when_cache_has_no_match() {
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::success("音声化する返答")),
            Arc::new(MockTts::failure(EngineErrorKind::RateLimited)),
            compiler(),
        );
        let request = generation_request(5);

        let result = pipeline
            .run(&request, &GenerationCancellationToken::default(), None)
            .expect("fallback");

        assert_eq!(result.trace.fallback_reason, FallbackReason::RateLimited);
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Fallback {
                directive: FallbackDirective::NonVerbalReaction { .. }
            }
        ));
    }
    #[test]
    fn cancellation_after_thinking_skips_tts_and_returns_no_partial_asset() {
        let token = GenerationCancellationToken::default();
        let mut thinking = MockThinking::success("partial reply");
        thinking.cancel_on_call = Some(token.clone());
        let tts = MockTts::success();
        let tts_calls = Arc::clone(&tts.calls);
        let pipeline = GenerativePipeline::new(Arc::new(thinking), Arc::new(tts), compiler());

        let result = pipeline
            .run(&generation_request(6), &token, None)
            .expect("cancelled result");

        assert_eq!(tts_calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            result.trace.cancelled_stage,
            Some(CancellationStage::AfterThinking)
        );
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Cancelled {
                stage: CancellationStage::AfterThinking
            }
        ));
    }

    #[test]
    fn cancellation_after_tts_discards_partial_output_without_scheduler_mutation() {
        let token = GenerationCancellationToken::default();
        let mut tts = MockTts::success();
        tts.cancel_on_call = Some(token.clone());
        let pipeline = GenerativePipeline::new(
            Arc::new(MockThinking::success("partial reply")),
            Arc::new(tts),
            compiler(),
        );

        let mut scheduler = Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
        });
        scheduler
            .schedule(PlannedPerformance {
                event_id: "existing".to_owned(),
                asset_id: "reaction.existing".to_owned(),
                priority: Priority::Conversation,
                interruptible: true,
                interrupt_points_ms: vec![500, 1000],
                start_at_ms: 0,
                duration_ms: 1000,
                generation: 0,
                exclusive: false,
                channels: BTreeSet::from([BlendChannel::Audio]),
            })
            .expect("existing plan");
        let before = scheduler.items().to_vec();

        let result = pipeline
            .run(&generation_request(7), &token, None)
            .expect("cancelled result");

        assert_eq!(scheduler.items(), before.as_slice());
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Cancelled {
                stage: CancellationStage::AfterTts
            }
        ));
    }
    #[test]
    fn higher_priority_event_cancels_active_generation_but_lower_priority_does_not() {
        let registry = GenerationCancellationRegistry::default();
        let chat = chat_event(8);
        let token = registry.begin(&chat);

        assert!(!registry.cancel_for_event(&timer_event(9)));
        assert!(!token.is_cancelled());
        assert!(registry.cancel_for_event(&donation_event(10)));
        assert!(token.is_cancelled());
        assert_eq!(
            registry.active_event_id().as_deref(),
            Some(chat.event_id.as_str())
        );

        registry.finish(&chat.event_id);
        assert_eq!(registry.active_event_id(), None);
    }

    #[test]
    fn already_cancelled_generation_never_calls_llm() {
        let thinking = MockThinking::success("unused");
        let calls = Arc::clone(&thinking.calls);
        let pipeline =
            GenerativePipeline::new(Arc::new(thinking), Arc::new(MockTts::success()), compiler());
        let token = GenerationCancellationToken::default();
        token.cancel();

        let result = pipeline
            .run(&generation_request(11), &token, None)
            .expect("cancelled");

        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert!(result.trace.llm_calls.is_empty());
        assert!(matches!(
            result.disposition,
            GenerationDisposition::Cancelled {
                stage: CancellationStage::BeforeThinking
            }
        ));
    }

    #[test]
    fn streaming_compilation_publishes_only_after_final_speech_chunk() {
        let request = generation_request(12);
        let thinking = MockThinking::success("unused").identity();
        let tts = MockTts::success().identity();
        let mut session = StreamingCompilationSession::new(request, thinking, tts);

        session.push_text_chunk("ストリーム").expect("text chunk");
        session.push_text_chunk("生成").expect("text chunk");
        session
            .push_speech_update(StreamingSpeechUpdate {
                audio_ref: "audio://generated/stream-12.opus".to_owned(),
                duration_ms: 400,
                viseme_ref: None,
                final_chunk: false,
            })
            .expect("partial speech");

        let partial = session.clone().finalize(&compiler());
        assert!(partial.is_err(), "partial stream must not publish an asset");

        session
            .push_speech_update(StreamingSpeechUpdate {
                audio_ref: "audio://generated/stream-12.opus".to_owned(),
                duration_ms: 900,
                viseme_ref: Some("viseme/generated/stream-12.json".to_owned()),
                final_chunk: true,
            })
            .expect("final speech");

        let asset = session.finalize(&compiler()).expect("finalize stream");
        assert_eq!(
            asset.speech.as_ref().and_then(|speech| speech.duration_ms),
            Some(900)
        );
        assert_eq!(
            asset
                .speech
                .as_ref()
                .and_then(|speech| speech.text.as_deref()),
            Some("ストリーム生成")
        );
        asset.validate().expect("streamed asset is valid");
    }

    #[test]
    fn cancelled_streaming_compilation_never_publishes_partial_asset() {
        let request = generation_request(13);
        let thinking = MockThinking::success("unused").identity();
        let tts = MockTts::success().identity();
        let mut session = StreamingCompilationSession::new(request, thinking, tts);

        session.push_text_chunk("partial").expect("text");
        session
            .push_speech_update(StreamingSpeechUpdate {
                audio_ref: "audio://generated/stream-13.opus".to_owned(),
                duration_ms: 300,
                viseme_ref: None,
                final_chunk: false,
            })
            .expect("partial speech");
        session.cancel();

        assert!(session.finalize(&compiler()).is_err());
    }
}
