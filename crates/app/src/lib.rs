#![forbid(unsafe_code)]

mod outputs;
mod raw_ingress;
mod routing;
mod semantic;

pub use outputs::*;
pub use raw_ingress::*;
pub use routing::*;
pub use semantic::*;

use aivtuber_adaptation::{AdaptationEngine, AppliedAdaptation, MemoryEntry, WorkingMemory};
use aivtuber_asset_store::CacheTier;
use aivtuber_domain::{
    AuthenticatedControl, AuthenticatedControlCommand, AuthorizedStreamAction, EngineError,
    EventEnvelope, FallbackReason,
};
use aivtuber_generative::{
    FallbackDirective, GeneratedTextGateDecision, GenerationCancellationRegistry,
    GenerationDisposition, GenerationRequest, GenerationTrace, GenerativePipeline,
};
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction};
use aivtuber_runtime::{
    AudioPlaybackCommand, AudioPlaybackSink, AvatarPlaybackCommand, AvatarPlaybackSink,
    CachedAssetSelection, CachedPerformer, CachedPlaybackError, CachedPlaybackOutcome,
    CachedPlaybackTiming, ContentAdmitDecision, ControlOutcome, FastPathPreloadReport,
    LocalVisemeStore, OutputVerdict, RuntimeError, SecurityRuntime,
};
use aivtuber_scheduler::{Scheduler, Status};
use aivtuber_telemetry::{
    CacheLevel, ComparisonMode, DegradedSubsystem, EventObservation, RouteClass, TelemetryCollector,
};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

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
    Generation(String),
    Adaptation(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "{error}"),
            Self::Playback(error) => write!(f, "{error}"),
            Self::Routing(message) => write!(f, "routing failed: {message}"),
            Self::Generation(message) => write!(f, "generation failed: {message}"),
            Self::Adaptation(message) => write!(f, "adaptation failed: {message}"),
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

#[derive(Clone)]
pub struct GenerativeRuntime {
    pipeline: Arc<GenerativePipeline>,
    cancellation: Arc<GenerationCancellationRegistry>,
}

impl GenerativeRuntime {
    pub fn new(pipeline: GenerativePipeline) -> Self {
        Self {
            pipeline: Arc::new(pipeline),
            cancellation: Arc::new(GenerationCancellationRegistry::default()),
        }
    }

    pub fn cancellation_registry(&self) -> Arc<GenerationCancellationRegistry> {
        Arc::clone(&self.cancellation)
    }
}

#[derive(Debug)]
pub struct AdaptationRuntime {
    memory: WorkingMemory,
    engine: AdaptationEngine,
}

impl AdaptationRuntime {
    pub fn new(memory: WorkingMemory, engine: AdaptationEngine) -> Self {
        Self { memory, engine }
    }

    pub fn memory(&self) -> &WorkingMemory {
        &self.memory
    }

    pub fn engine(&self) -> &AdaptationEngine {
        &self.engine
    }
}

struct HandledEvent {
    playback: Option<CachedPlaybackOutcome>,
    route: RouteClass,
    routing_latency_us: u64,
    generation_latency_us: Option<u64>,
    generation_trace: Option<GenerationTrace>,
    decision: Option<DecisionReplayRecord>,
    cache_level: Option<CacheLevel>,
}

struct HandledGeneration {
    playback: Option<CachedPlaybackOutcome>,
    route: RouteClass,
    trace: GenerationTrace,
    cache_level: Option<CacheLevel>,
}

pub struct ProductionApp<R>
where
    R: RoutePlanner,
{
    security: SecurityRuntime,
    performer: CachedPerformer,
    visemes: LocalVisemeStore,
    router: R,
    generative: Option<GenerativeRuntime>,
    adaptation: Option<AdaptationRuntime>,
    audio: Box<dyn AudioOutput>,
    avatar: Box<dyn AvatarOutput>,
    stream: Box<dyn StreamOutput>,
    pending_audio: PendingAudio,
    pending_avatar: PendingAvatar,
    max_dispatch_lateness_ms: u64,
    health: AdapterHealth,
    telemetry: TelemetryCollector,
    comparison_mode: ComparisonMode,
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
            generative: None,
            adaptation: None,
            audio,
            avatar,
            stream,
            pending_audio: PendingAudio::default(),
            pending_avatar: PendingAvatar::default(),
            max_dispatch_lateness_ms,
            health: AdapterHealth::default(),
            telemetry: TelemetryCollector::default(),
            comparison_mode: ComparisonMode::DeterministicOnly,
        }
    }

    pub fn with_generation(mut self, generative: GenerativeRuntime) -> Self {
        self.generative = Some(generative);
        self.comparison_mode = ComparisonMode::FullGenerative;
        self
    }

    pub fn with_adaptation(mut self, adaptation: AdaptationRuntime) -> Self {
        self.adaptation = Some(adaptation);
        self
    }

    pub fn with_comparison_mode(mut self, mode: ComparisonMode) -> Self {
        self.comparison_mode = mode;
        self
    }

    pub fn adaptation(&self) -> Option<&AdaptationRuntime> {
        self.adaptation.as_ref()
    }

    pub fn telemetry(&self) -> &TelemetryCollector {
        &self.telemetry
    }

    pub fn telemetry_mut(&mut self) -> &mut TelemetryCollector {
        &mut self.telemetry
    }

    pub fn remember_working_memory(
        &mut self,
        event: &EventEnvelope,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
    ) -> Result<MemoryEntry, AppError> {
        event
            .validate()
            .map_err(|error| AppError::Adaptation(error.to_string()))?;
        let adaptation = self.adaptation.as_mut().ok_or_else(|| {
            AppError::Adaptation(
                "working memory requested without configured adaptation runtime".to_owned(),
            )
        })?;
        adaptation
            .memory
            .remember_working(event, claim, topic, now_ms)
            .cloned()
            .map_err(|error| AppError::Adaptation(error.to_string()))
    }

    pub fn remember_durable_memory(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
    ) -> Result<MemoryEntry, AppError> {
        event
            .validate()
            .map_err(|error| AppError::Adaptation(error.to_string()))?;
        let (_, permit) = self.security.authorize_memory_write(event, authority);
        let permit = permit.ok_or_else(|| {
            AppError::Adaptation("durable memory write denied by security gate".to_owned())
        })?;
        let adaptation = self.adaptation.as_mut().ok_or_else(|| {
            AppError::Adaptation(
                "durable memory requested without configured adaptation runtime".to_owned(),
            )
        })?;
        adaptation
            .memory
            .remember_durable(&permit, claim, topic, now_ms)
            .cloned()
            .map_err(|error| AppError::Adaptation(error.to_string()))
    }

    pub fn label_generated_asset_quality(
        &mut self,
        asset_id: &str,
        positive: bool,
    ) -> Result<(), AppError> {
        let adaptation = self.adaptation.as_mut().ok_or_else(|| {
            AppError::Adaptation(
                "quality label requested without configured adaptation runtime".to_owned(),
            )
        })?;
        adaptation.engine.record_quality(asset_id, positive);
        Ok(())
    }

    pub fn apply_generated_asset_adaptation(
        &mut self,
        asset_id: &str,
    ) -> Result<AppliedAdaptation, AppError> {
        let adaptation = self.adaptation.as_mut().ok_or_else(|| {
            AppError::Adaptation("asset adaptation requested without configured runtime".to_owned())
        })?;
        adaptation
            .engine
            .apply(self.performer.assets_mut(), asset_id)
            .map_err(|error| AppError::Adaptation(error.to_string()))
    }

    pub fn generation_cancellation_registry(&self) -> Option<Arc<GenerationCancellationRegistry>> {
        self.generative
            .as_ref()
            .map(GenerativeRuntime::cancellation_registry)
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

    /// Mutable access for tests that inspect the hot cache directly.
    pub fn performer_mut(&mut self) -> &mut CachedPerformer {
        &mut self.performer
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
        let event_id = event.event_id.clone();
        if let Some(generative) = &self.generative {
            generative.cancellation.cancel_for_event(&event);
        }
        let handled = self.handle_event(event, at_ms, seed)?;
        self.tick(at_ms);
        self.record_handled_event(&event_id, &handled, at_ms);
        let playback = handled.playback;
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
    ) -> Result<HandledEvent, AppError> {
        let route_started = Instant::now();
        let route = self.router.route(&event)?;
        let routing_latency_us = elapsed_us(route_started);
        let decision = self.router.decision_record().cloned();
        let mut generation_latency_us = None;
        let mut generation_trace = None;

        let (playback, route_class, cache_level) = match route {
            PlaybackRoute::Silent => (None, RouteClass::Silent, None),
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
                let cache_level = cache_level(Some(outcome.cache_tier));
                (Some(outcome), RouteClass::Deterministic, cache_level)
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
                let cache_level = cache_level(Some(outcome.cache_tier));
                (Some(outcome), RouteClass::SemanticReuse, cache_level)
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
                let cache_level = cache_level(Some(outcome.cache_tier));
                (Some(outcome), RouteClass::SemanticReuse, cache_level)
            }
            PlaybackRoute::Template {
                template_id,
                composition,
            } => {
                // Template composition: the composer renders the curated
                // template with bounded slots from the event, then the text
                // passes the same security output gate as generative speech
                // before playback (issue #54). Rendered text becomes a
                // template-backed performance asset inserted into L0 and
                // played through the same scheduler path (operator stop,
                // mute, cancellation all apply).
                let output = self.security.publish_text(&composition.text);
                if matches!(
                    output.verdict,
                    OutputVerdict::Suppress | OutputVerdict::ReplaceWithCached
                ) {
                    return Ok(HandledEvent {
                        playback: None,
                        route: RouteClass::Silent,
                        routing_latency_us,
                        generation_latency_us,
                        generation_trace,
                        decision,
                        cache_level: None,
                    });
                }
                let text = output.text.unwrap_or_else(|| composition.text.clone());
                let (playback, route_class, cache_level) = self.handle_template_playback(
                    &event,
                    &template_id,
                    &composition.template_version,
                    &text,
                    at_ms,
                    seed,
                )?;
                (playback, route_class, cache_level)
            }
            PlaybackRoute::Generate(route) => {
                let generation_started = Instant::now();
                let handled = self.handle_generation(event, *route, at_ms, seed)?;
                generation_latency_us = Some(elapsed_us(generation_started));
                let HandledGeneration {
                    playback,
                    route,
                    trace,
                    cache_level,
                } = handled;
                generation_trace = Some(trace);
                (playback, route, cache_level)
            }
        };

        Ok(HandledEvent {
            playback,
            route: route_class,
            routing_latency_us,
            generation_latency_us,
            generation_trace,
            decision,
            cache_level,
        })
    }

    /// Compose a validated template decision into scheduled playback.
    ///
    /// The rendered text passes the security output gate (caller handled), is
    /// compiled into a template-backed Performance Asset, and is inserted into
    /// L0 under a deterministic id so cached-audio reuse applies: the
    /// scheduler, operator stop/mute, and cancellation semantics are shared
    /// with cached/generated routes (issue #54).
    fn handle_template_playback(
        &mut self,
        event: &EventEnvelope,
        template_id: &str,
        template_version: &str,
        text: &str,
        at_ms: u64,
        seed: u64,
    ) -> Result<
        (
            Option<CachedPlaybackOutcome>,
            RouteClass,
            Option<CacheLevel>,
        ),
        AppError,
    > {
        // Deterministic template asset identity: same event + template +
        // rendered text -> same asset id, so repeated reuse stays cacheable.
        let asset_id = format!(
            "template.{template_id}.{}",
            short_hash((event.event_id.as_str(), template_id, text, seed))
        );

        // Cached-audio fragment reuse: if the exact text already has a hot
        // asset (previous generation or template render), reuse it instead of
        // re-inserting. Otherwise compile a new template performance.
        if self.performer.assets_mut().hot_get(&asset_id).is_none() {
            let duration_ms = estimated_speech_duration_ms(text);
            let asset = template_performance_asset(
                &asset_id,
                template_id,
                template_version,
                text,
                event,
                duration_ms,
            );
            self.performer
                .assets_mut()
                .insert_hot(asset)
                .map_err(|error| AppError::Generation(error.to_string()))?;
        }

        let outcome = self.performer.handle_asset_id(
            event,
            &asset_id,
            CachedPlaybackTiming { at_ms, seed },
            &self.visemes,
            &mut self.pending_audio,
            &mut self.pending_avatar,
        )?;
        let cache_level = cache_level(Some(outcome.cache_tier));
        Ok((Some(outcome), RouteClass::JevReaction, cache_level))
    }

    fn handle_generation(
        &mut self,
        event: EventEnvelope,
        route: GenerationRoute,
        at_ms: u64,
        seed: u64,
    ) -> Result<HandledGeneration, AppError> {
        if route.source_event != event {
            return Err(AppError::Generation(
                "generation route source event does not match dispatched event".to_owned(),
            ));
        }
        let generative = self.generative.clone().ok_or_else(|| {
            AppError::Generation("generative route requested without configured runtime".to_owned())
        })?;
        let request = GenerationRequest {
            source_event: event.clone(),
            thinking: route.thinking,
            routing_reason: route.routing_reason,
            intent: route.intent,
            style: route.style,
            fallback_variant_group: route.fallback_variant_group,
            seed,
        };
        let cancellation = generative.cancellation.begin(&event);
        let result = {
            let fallback_assets = self.performer.assets();
            let security = &mut self.security;
            generative.pipeline.run_with_output_gate(
                &request,
                &cancellation,
                Some(fallback_assets),
                |text| match security.publish_text(text) {
                    output
                        if matches!(
                            output.verdict,
                            OutputVerdict::Allow | OutputVerdict::Redact
                        ) =>
                    {
                        output
                            .text
                            .map(GeneratedTextGateDecision::Publish)
                            .unwrap_or(GeneratedTextGateDecision::Reject)
                    }
                    _ => GeneratedTextGateDecision::Reject,
                },
            )
        };
        generative.cancellation.finish(&event.event_id);
        let result = result.map_err(|error| AppError::Generation(error.to_string()))?;
        for call in &result.trace.llm_calls {
            self.security.record_generation_call(
                &call.event_id,
                call.routing_reason.as_str(),
                &call.backend.name,
                call.backend.model_alias.as_deref(),
                call.backend.model_version.as_deref(),
            );
        }
        let trace = result.trace.clone();

        match result.disposition {
            GenerationDisposition::Generated { asset } => {
                let asset_id = asset.id.clone();
                self.performer
                    .assets_mut()
                    .insert_hot(*asset)
                    .map_err(|error| AppError::Generation(error.to_string()))?;
                let outcome = self.performer.handle_asset_id(
                    &event,
                    &asset_id,
                    CachedPlaybackTiming { at_ms, seed },
                    &self.visemes,
                    &mut self.pending_audio,
                    &mut self.pending_avatar,
                )?;
                Ok(HandledGeneration {
                    playback: Some(outcome),
                    route: RouteClass::Generated,
                    trace,
                    cache_level: Some(CacheLevel::Generated),
                })
            }
            GenerationDisposition::Fallback { directive } => match directive {
                FallbackDirective::CachedReaction { asset_id, .. } => {
                    let outcome = self.performer.handle_asset_id(
                        &event,
                        &asset_id,
                        CachedPlaybackTiming { at_ms, seed },
                        &self.visemes,
                        &mut self.pending_audio,
                        &mut self.pending_avatar,
                    )?;
                    let cache_level = cache_level(Some(outcome.cache_tier));
                    Ok(HandledGeneration {
                        playback: Some(outcome),
                        route: RouteClass::CachedFallback,
                        trace,
                        cache_level,
                    })
                }
                FallbackDirective::NonVerbalReaction { .. } => Ok(HandledGeneration {
                    playback: None,
                    route: RouteClass::NonVerbalFallback,
                    trace,
                    cache_level: None,
                }),
            },
            GenerationDisposition::Cancelled { .. } => Ok(HandledGeneration {
                playback: None,
                route: RouteClass::Generated,
                trace,
                cache_level: None,
            }),
        }
    }

    fn record_handled_event(&mut self, event_id: &str, handled: &HandledEvent, at_ms: u64) {
        let mut route = handled.route;
        let mut observation = EventObservation::new(event_id, self.comparison_mode, route);
        observation.routing_latency_us = handled.routing_latency_us;
        observation.generation_latency_us = handled.generation_latency_us;
        observation.cache_level = handled.cache_level;
        observation.cache_lookup = matches!(
            handled.route,
            RouteClass::Deterministic
                | RouteClass::SemanticReuse
                | RouteClass::JevReaction
                | RouteClass::CachedFallback
        );
        observation.cache_hit = handled.playback.is_some()
            && matches!(
                handled.cache_level,
                Some(CacheLevel::Memory | CacheLevel::LocalStorage)
            );

        if let Some(playback) = &handled.playback {
            observation.event_to_first_audio_ms = playback.metrics.event_to_first_audio_ms;
            observation.event_to_first_visible_reaction_ms =
                playback.metrics.event_to_first_visible_reaction_ms;
            if let Some(asset) = self.performer.assets_mut().hot_get(&playback.asset_id)
                && asset
                    .provenance
                    .as_ref()
                    .is_some_and(|provenance| provenance.generated == Some(true))
            {
                // hot_get already touched LRU recency; record the logical-time
                // use so promotion metadata survives eviction (issue #53), and
                // feed the adaptation engine (#39).
                self.performer
                    .assets_mut()
                    .note_hot_use(&playback.asset_id, at_ms);
                if let Some(adaptation) = self.adaptation.as_mut() {
                    adaptation.engine.record_use(&asset);
                }
            }
        }

        if let Some(decision) = &handled.decision {
            observation.retrieval_candidates = decision.evidence.retrieval.candidates.len();
            observation.jev_attempts = decision.evidence.model.attempts;
            observation.jev_latency_us = millis_to_micros(decision.evidence.model.latency_ms);
            observation.semantic_reuse_accepted =
                decision.executed.action == ExecutedAction::Cached;
            observation.semantic_reuse_score = decision
                .evidence
                .selected_candidate_id
                .as_ref()
                .and_then(|selected| {
                    decision
                        .evidence
                        .retrieval
                        .candidates
                        .iter()
                        .find(|candidate| &candidate.asset_id == selected)
                        .map(|candidate| candidate.similarity)
                });
            if decision.executed.action == ExecutedAction::Reaction
                && route == RouteClass::Deterministic
            {
                route = RouteClass::JevReaction;
                observation.route = route;
            }
            observation.fallback_reason =
                fallback_reason_name(decision.evidence.normalized.fallback_reason)
                    .map(str::to_owned);
        }

        if self.health.audio_error.is_some() {
            observation
                .degraded_subsystems
                .insert(DegradedSubsystem::Audio);
        }
        if self.health.avatar_error.is_some() {
            observation
                .degraded_subsystems
                .insert(DegradedSubsystem::Avatar);
        }
        if self.health.stream_error.is_some() {
            observation
                .degraded_subsystems
                .insert(DegradedSubsystem::Stream);
        }

        if let Some(trace) = &handled.generation_trace {
            observation.llm_calls = trace.llm_calls.len().min(u32::MAX as usize) as u32;
            observation.tts_calls = u32::from(trace.tts_attempted);
            observation.cancelled = trace.cancelled_stage.is_some();
            if let Some(reason) = fallback_reason_name(trace.fallback_reason) {
                observation.fallback_reason = Some(reason.to_owned());
            }
        }

        self.telemetry.record(observation);
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
                Err(error) => {
                    self.health.audio_error = Some(error.to_string());
                    self.mark_degraded(&command.event_id, DegradedSubsystem::Audio);
                }
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

            let event_id = self
                .performer
                .scheduler()
                .items()
                .iter()
                .find(|item| item.plan.generation == avatar_generation(&command))
                .map(|item| item.plan.event_id.clone());
            match self.avatar.execute(&command) {
                Ok(()) => self.health.avatar_error = None,
                Err(error) => {
                    self.health.avatar_error = Some(error.to_string());
                    if let Some(event_id) = event_id {
                        self.mark_degraded(&event_id, DegradedSubsystem::Avatar);
                    }
                }
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
        let is_override = matches!(
            outcome,
            ControlOutcome::Stopped { .. } | ControlOutcome::Muted
        );
        let generation_cancelled = if is_override {
            self.generative
                .as_ref()
                .is_some_and(|generative| generative.cancellation.cancel_active())
        } else {
            false
        };
        if matches!(outcome, ControlOutcome::Stopped { .. }) {
            self.purge_cancelled_pending();
        }
        if is_override {
            let scheduler_cancelled =
                matches!(outcome, ControlOutcome::Stopped { cancelled } if cancelled > 0);
            let mut observation = EventObservation::new(
                command.event().event_id.clone(),
                self.comparison_mode,
                RouteClass::Silent,
            );
            observation.operator_override = true;
            observation.cancelled = generation_cancelled || scheduler_cancelled;
            observation.fallback_reason = Some("operator_override".to_owned());
            if self.health.audio_error.is_some() {
                observation
                    .degraded_subsystems
                    .insert(DegradedSubsystem::Audio);
            }
            if self.health.avatar_error.is_some() {
                observation
                    .degraded_subsystems
                    .insert(DegradedSubsystem::Avatar);
            }
            if self.health.stream_error.is_some() {
                observation
                    .degraded_subsystems
                    .insert(DegradedSubsystem::Stream);
            }
            self.telemetry.record(observation);
        }
        self.tick(at_ms);
        Ok(outcome)
    }

    pub fn shutdown(&mut self, at_ms: u64) {
        if let Some(generative) = &self.generative {
            generative.cancellation.cancel_active();
        }
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

    fn mark_degraded(&mut self, event_id: &str, subsystem: DegradedSubsystem) {
        if let Some(observation) = self
            .telemetry
            .events_mut()
            .iter_mut()
            .rev()
            .find(|observation| observation.event_id == event_id)
        {
            observation.degraded_subsystems.insert(subsystem);
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

fn cache_level(tier: Option<CacheTier>) -> Option<CacheLevel> {
    tier.map(|tier| match tier {
        CacheTier::Memory => CacheLevel::Memory,
        CacheTier::LocalStorage => CacheLevel::LocalStorage,
        CacheTier::Generated => CacheLevel::Generated,
    })
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

fn millis_to_micros(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * 1_000.0).round().min(u64::MAX as f64) as u64)
}

fn fallback_reason_name(reason: FallbackReason) -> Option<&'static str> {
    match reason {
        FallbackReason::None => None,
        FallbackReason::Timeout => Some("timeout"),
        FallbackReason::Unavailable => Some("unavailable"),
        FallbackReason::RateLimited => Some("rate_limited"),
        FallbackReason::Overloaded => Some("overloaded"),
        FallbackReason::Authentication => Some("authentication"),
        FallbackReason::InvalidRequest => Some("invalid_request"),
        FallbackReason::LowConfidence => Some("low_confidence"),
        FallbackReason::PolicyOverride => Some("policy_override"),
        FallbackReason::OperatorOverride => Some("operator_override"),
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

/// FNV-1a over mixed inputs, mirroring the generative asset-id hash style.
fn short_hash(parts: (&str, &str, &str, u64)) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for bytes in [
        parts.0.as_bytes(),
        parts.1.as_bytes(),
        parts.2.as_bytes(),
        &parts.3.to_le_bytes(),
    ] {
        hash_bytes(&mut hash, bytes);
    }
    hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

/// Deterministic speech duration estimate for template text without a
/// recorded audio fragment: ~6 characters per 100 ms, bounded to a sane
/// performance window. Real TTS integration replaces this estimate.
fn estimated_speech_duration_ms(text: &str) -> u64 {
    let characters = text.chars().count().max(1);
    (characters as u64 * 100 / 6).clamp(400, 8_000)
}

/// Compile a rendered template into a validated dynamic Performance Asset.
/// Provenance marks it as a template composition (not LLM generated) with the
/// template id/version recorded for replay/telemetry (issue #54).
fn template_performance_asset(
    asset_id: &str,
    template_id: &str,
    template_version: &str,
    text: &str,
    event: &EventEnvelope,
    duration_ms: u64,
) -> aivtuber_asset_store::PerformanceAsset {
    use aivtuber_asset_store::{
        AssetClass, AssetCompatibility, ExpressionTrack, PerformanceAsset, Provenance, SpeechTrack,
        TimelineEvent,
    };

    let timeline = vec![
        TimelineEvent {
            at_ms: 0,
            event: "expression.start".to_owned(),
            payload: Some(serde_json::json!({ "preset": "speaking.neutral" })),
        },
        TimelineEvent {
            at_ms: 0,
            event: "speech.start".to_owned(),
            payload: None,
        },
        TimelineEvent {
            at_ms: duration_ms,
            event: "speech.end".to_owned(),
            payload: None,
        },
    ];

    PerformanceAsset {
        schema_version: aivtuber_asset_store::PERFORMANCE_ASSET_SCHEMA_VERSION.to_owned(),
        id: asset_id.to_owned(),
        intent: format!("template.{template_id}"),
        class: AssetClass::Dynamic,
        variant_group: None,
        speech: Some(SpeechTrack {
            text: Some(text.to_owned()),
            audio_ref: Some(format!("audio://template/{template_id}.opus")),
            duration_ms: Some(duration_ms),
            viseme_ref: None,
        }),
        expression: Some(ExpressionTrack {
            preset: "speaking.neutral".to_owned(),
            intensity: 0.4,
        }),
        gesture: None,
        gaze: Some(aivtuber_asset_store::GazeTarget::Camera),
        timeline,
        interrupt_points_ms: vec![duration_ms],
        variation: None,
        compatibility: AssetCompatibility {
            compiler_version: aivtuber_asset_store::PERFORMANCE_ASSET_SCHEMA_VERSION.to_owned(),
            voice_model: None,
            avatar_profile: None,
            viseme_mapping: None,
            motion_library: None,
        },
        semantic_embedding: None,
        provenance: Some(Provenance {
            generated: Some(true),
            generator: Some("aivtuber-template".to_owned()),
            created_at: Some(event.observed_at.clone()),
            thinking_backend: Some("template-composer".to_owned()),
            thinking_model_alias: Some(template_id.to_owned()),
            thinking_model_version: Some(template_version.to_owned()),
            tts_backend: Some("reused-cached-fragment".to_owned()),
            tts_model_alias: None,
            tts_model_version: None,
            routing_reason: Some("template".to_owned()),
        }),
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
    use aivtuber_adaptation::{PromotionPolicy, WorkingMemoryConfig};
    use aivtuber_asset_store::{AssetStore, RuntimeCompatibility, load_asset_file};
    use aivtuber_domain::{
        AuthorizationMethod, BackendIdentity, Capability, ControlSecret, EVENT_SCHEMA_VERSION,
        EngineErrorKind, EngineFuture, GeneratedReply, LocalControlIngress, OperatorCommandInput,
        PrivacyClass, ReflexContext, RetrievalSnapshot, SecurityPlane, SourceClass, SpeechArtifact,
        SpeechProgress, SpeechProgressSink, SpeechRequest, ThinkingEngine, TrustLevel,
        TtsBackendIdentity, TtsEngine,
    };
    use aivtuber_generative::{PerformanceCompiler, PerformanceCompilerConfig};
    use aivtuber_reflex::{
        HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig,
        ReflexPipeline, TransportError,
    };
    use aivtuber_runtime::{CachedPlaybackConfig, SecurityRuntimeConfig};
    use aivtuber_scheduler::SchedulerConfig;
    use aivtuber_telemetry::SecretRedactor;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
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

    #[derive(Clone, Default)]
    struct MockTts {
        texts: Arc<Mutex<Vec<String>>>,
        failure: Option<EngineErrorKind>,
    }

    impl TtsEngine for MockTts {
        fn synthesize<'a>(
            &'a self,
            request: &'a SpeechRequest,
        ) -> EngineFuture<'a, SpeechArtifact> {
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
        fn synthesize<'a>(
            &'a self,
            _request: &'a SpeechRequest,
        ) -> EngineFuture<'a, SpeechArtifact> {
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
            },
        )
    }

    fn reflex_router() -> ReflexRoutePlanner<FixedEmbedding> {
        reflex_router_with_route("cached")
    }

    fn reflex_router_with_route(response_route: &str) -> ReflexRoutePlanner<FixedEmbedding> {
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

    fn adaptation_runtime(policy: PromotionPolicy) -> AdaptationRuntime {
        AdaptationRuntime::new(
            WorkingMemory::new(WorkingMemoryConfig {
                max_entries: 8,
                working_ttl_ms: 100,
                durable_ttl_ms: 1_000,
                max_claim_bytes: 128,
                max_topic_bytes: 64,
                pseudonym_salt: 7,
            })
            .expect("working memory"),
            AdaptationEngine::new(policy, "test-adaptation-v1", 42).expect("adaptation engine"),
        )
    }

    fn generative_runtime<T>(thinking: MockThinking, tts: T) -> GenerativeRuntime
    where
        T: TtsEngine + 'static,
    {
        let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
            compiler_version: "0.1.0".to_owned(),
            avatar_profile: Some("example-live2d-v1".to_owned()),
            motion_library: Some("starter-v1".to_owned()),
            expression_preset: Some("speaking.neutral".to_owned()),
            expression_intensity: 0.4,
        })
        .expect("compiler");
        GenerativeRuntime::new(GenerativePipeline::new(
            Arc::new(thinking),
            Arc::new(tts),
            compiler,
        ))
    }

    fn generation_app<T>(
        router: FixedGenerateRoute,
        thinking: MockThinking,
        tts: T,
        redactor: SecretRedactor,
        cached_reaction: Option<String>,
        audio: RecordingAudio,
        avatar: RecordingAvatar,
    ) -> ProductionApp<FixedGenerateRoute>
    where
        T: TtsEngine + 'static,
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
        let playback = app
            .process_content_bytes(&raw, 0, 26)
            .expect("generation")
            .playback
            .expect("generated playback");
        let asset_id = playback.asset_id;
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
        let outcome = app.process_content_bytes(&raw, 0, 30).expect("generation");
        let playback = outcome.playback.expect("generated playback");

        assert!(playback.asset_id.starts_with("dynamic.generated."));
        assert_eq!(
            tts_log.lock().expect("tts log").as_slice(),
            ["generated hello"]
        );
        assert!(
            app.performer_mut()
                .assets_mut()
                .hot_get(&playback.asset_id)
                .is_some()
        );
        assert_eq!(app.performer().scheduler().items().len(), 1);
        let audio_commands = audio_log.lock().expect("audio log");
        assert_eq!(audio_commands.len(), 1);
        assert_eq!(audio_commands[0].generation, playback.plan.generation);
        assert_eq!(audio_commands[0].at_ms, playback.plan.start_at_ms);
        drop(audio_commands);

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
            *generation == playback.plan.generation
                && *at_ms >= playback.plan.start_at_ms
                && *at_ms <= playback.plan.end_at_ms()
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
        assert_eq!(
            metric.event_to_first_audio_ms,
            playback.metrics.event_to_first_audio_ms
        );
        assert_eq!(
            metric.event_to_first_visible_reaction_ms,
            playback.metrics.event_to_first_visible_reaction_ms
        );
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
        let playback = app
            .process_content_bytes(&raw, 0, 31)
            .expect("generation")
            .playback
            .expect("generated playback");
        assert_eq!(
            tts_log.lock().expect("tts log").as_slice(),
            ["secret=[REDACTED]"]
        );
        let asset = app
            .performer_mut()
            .assets_mut()
            .hot_get(&playback.asset_id)
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
        let playback = app
            .process_content_bytes(&raw, 0, 32)
            .expect("fallback")
            .playback
            .expect("cached fallback");
        assert!(tts_log.lock().expect("tts log").is_empty());
        assert!(playback.asset_id.starts_with("reaction.agree."));
        assert!(!playback.asset_id.starts_with("dynamic.generated."));
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
        let playback = app
            .process_content_bytes(&raw, 0, 33)
            .expect("fallback")
            .playback
            .expect("cached fallback");
        assert!(tts_log.lock().expect("tts log").is_empty());
        assert!(playback.asset_id.starts_with("reaction.agree."));
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
        let playback = app
            .process_content_bytes(&raw, 0, 34)
            .expect("fallback")
            .playback
            .expect("cached fallback");
        assert_eq!(tts_log.lock().expect("tts log").len(), 1);
        assert!(playback.asset_id.starts_with("reaction.agree."));
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
            .expect("non-verbal fallback");
        assert!(outcome.playback.is_none());
        assert!(app.performer().scheduler().items().is_empty());
        assert_eq!(app.performer().assets().hot_len(), hot_before);
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
            .expect("partial streaming fallback");
        assert!(outcome.playback.is_none());
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
        assert_eq!(avatar_log.lock().expect("avatar log").len(), avatar_before);
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
}
