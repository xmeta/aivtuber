#![forbid(unsafe_code)]

mod affect_judge;
mod aivis_bridge;
mod audio_stop;
mod avatar_control;
mod budget;
mod chat_burst;
mod chat_ingress;
mod deadline;
mod deferred_ack;
mod face_settle;
pub mod initiative;
mod initiative_host;
mod irodori_bridge;
mod memory_compare;
mod memory_context;
mod memory_eval;
mod memory_seed;
mod memory_sidecar_eval;
mod memory_writer;
mod model_switch;
mod moderation_evidence;
mod operator_control;
mod outputs;
mod pack_readiness;
mod panel;
mod perception_service;
pub mod persona;
mod phrase_benchmark;
mod phrase_commit;
mod profile;
mod raw_ingress;
pub mod ready_guide;
mod recent_replies;
pub mod recent_turns;
pub mod repeat_guard;
mod retention;
mod routing;
mod scenario;
mod scene_context;
mod semantic;
pub mod session_log;
mod session_log_host;
mod shadow;
mod shadow_evidence;
mod shadow_orchestrator;
mod speech_barge_in;
mod speech_conversation;
mod speech_duplex;
mod speech_floor;
pub mod speech_input;
mod speech_resynthesis;
mod speech_sidecar;
mod support_bundle;
mod task_plane;
mod thinking_meter;
mod thinking_provider;
mod thinking_warmup;
mod tool_execution;
pub mod topics;
mod tts_sidecar;
mod voice_control;
mod voicevox_bridge;

pub use affect_judge::{
    AFFECT_JUDGE_DEADLINE_VAR, AFFECT_JUDGE_VAR, AffectJudgeMode, SystemOneAffectJudge,
    startup_notice as affect_judge_startup_notice,
};
pub use aivis_bridge::*;
pub use aivtuber_domain::scenario::*;
pub use audio_stop::{AUDIO_STOP_BOUND, AudioStopReport, OperatorStopReport};
pub use avatar_control::{
    AVATAR_DIR_VAR, AVATAR_SETTINGS_VAR, AvatarController, AvatarEnvironment, AvatarModelInfo,
    AvatarStatus, MAX_STORED_MODELS, SAMPLE_ID,
};
pub use budget::*;
pub use chat_burst::{
    ChatBurstConfig, ChatBurstConfigError, ChatBurstMetrics, ChatBurstRecord, ChatOutcome,
    DeferredReplyReport, MAX_AUDIENCE_SIGNALS, MAX_CHAT_BURST_RECORDS, MAX_DEFERRED_REPORTS,
    MAX_REPLY_TTL_MS,
};
pub use chat_ingress::{
    CHAT_RECORDS_PER_TICK, CHAT_SETTINGS_VAR, ChatIngressController, ChatIngressEnvironment,
    ChatIngressStatus, ChatKeyStatus, ChatPlatform, ChatPlatformStatus, ChatRecord, ChatSettings,
    ChatState, LineRefusal, MAX_INGRESS_ARGUMENT_BYTES, ManualChatClock, YOUTUBE_API_BASE_VAR,
    YOUTUBE_KEY_FILE_VAR, gate_line, is_acceptable_api_base, parse_connect_argument,
    parse_disconnect_argument, read_chat_settings,
};
pub use deadline::*;
pub use initiative_host::InitiativeStatus;
pub use irodori_bridge::*;
pub use memory_compare::*;
pub use memory_context::*;
pub use memory_eval::*;
pub use memory_seed::{
    EXPORT_SEED_ID, MAX_SEED_CLAIM_BYTES, MAX_SEED_ENTRIES, MAX_SEED_FILE_BYTES,
    MEMORY_SEED_EVENT_PREFIX, MEMORY_SEED_FILE_VAR, MEMORY_SEED_SCHEMA_VERSION,
    MemoryFalsifierMetrics, MemorySeed, MemorySeedEditor, MemorySeedEditorStatus, MemorySeedError,
    MemorySeedExport, MemorySeedIdentity, MemorySeedOutcome, SeedEntry, SeedKind,
};
pub use memory_sidecar_eval::*;
pub use memory_writer::{
    MEMORY_WRITER_EVENT_PREFIX, MEMORY_WRITER_MAX_LIVE_ENTRIES_CEILING, MemoryWriterConfig,
    MemoryWriterMetrics,
};
pub use model_switch::{
    ModelSwitchError, ModelSwitcher, SwitchWarmup, ThinkingModelStatus, WarmFn, remembered_model,
    switch_warmup_for,
};
pub use moderation_evidence::*;
pub use operator_control::*;
pub use outputs::*;
pub use pack_readiness::{PackReadiness, audit as audit_pack_readiness};
pub use panel::{
    PANEL_CHAT_SOURCE, PANEL_STATE_SCHEMA, PanelService, PanelSettings, chat_envelope,
    panel_settings,
};
pub use perception_service::*;
pub use phrase_benchmark::*;
pub use phrase_commit::{
    PHRASE_RESPONSE_RECORDS, PHRASE_START_WAIT, PhraseCause, PhraseCommitRecord, PhraseGateOutcome,
    PhraseMetrics, PhraseMode, PhraseResponseEnd, PhraseResponseRecord,
};
pub use profile::*;
pub use raw_ingress::*;
pub use recent_replies::{
    RECENT_REPLY_CAPACITY, RECENT_REPLY_MAX_CHARS, RecentReplies, ReplyRecord,
};
pub use repeat_guard::RepeatStatus;
pub use retention::*;
pub use routing::*;
pub use scenario::*;
pub use scene_context::*;
pub use semantic::*;
pub use session_log::SessionLogStatus;
pub use shadow::*;
pub use shadow_evidence::*;
pub use shadow_orchestrator::*;
pub use speech_barge_in::{
    BARGE_IN_RECORDS, BargeInConfig, BargeInEffect, BargeInMetrics, BargeInRecord,
};
pub use speech_conversation::{SpeechConversationConfig, SpeechConversationMetrics};
pub use speech_floor::SpeechFloorMetrics;
pub use speech_input::{
    SpeechInputController, SpeechInputError, SpeechInputStateName, SpeechInputStatus,
};
pub use speech_sidecar::*;
pub use support_bundle::*;
pub use task_plane::*;
pub use thinking_meter::{
    MAX_THINKING_COST_CAP_USD, MeteredThinking, THINKING_COST_CAP_VAR, ThinkingMeter,
    ThinkingUsageStatus, parse_cost_cap_usd,
};
pub use thinking_provider::{
    OPENROUTER_DEFAULT_TITLE, OPENROUTER_ENDPOINT, apply_thinking_provider_env,
    openrouter_preset_active,
};
pub use thinking_warmup::{
    DEFAULT_WARMUP_TIMEOUT_MS, WarmupDecision, WarmupMode, decide as decide_thinking_warmup,
    run as run_thinking_warmup, start as start_thinking_warmup, warmup_timeout,
};
pub use tool_execution::*;
pub use tts_sidecar::*;
pub use voice_control::{
    AIVIS_KEY_FILE_VAR, PreviewSlot, PreviewThinking, VOICE_SETTINGS_VAR, VoiceController,
    VoiceEnvironment, VoiceKeyStatus, VoicePaths, VoiceStatus, is_valid_key, read_key_file,
    read_voice_settings,
};
pub use voicevox_bridge::*;

use aivtuber_adaptation::{
    AdaptationEngine, AppliedAdaptation, ErasureReport, MemoryEntry, MemoryEvidence, WorkingMemory,
};
use aivtuber_adapters::AudioStopOutcome;
use aivtuber_asset_store::CacheTier;
use aivtuber_domain::{
    AuthenticatedControl, AuthenticatedControlCommand, AuthorizedStreamAction,
    DeadlineExhaustionReason, DeadlineStage, EngineError, EventEnvelope, FallbackReason,
    InteractionDeadline, Interruptibility, SceneContextSnapshot, TrustedInterruptibility,
};
use aivtuber_generative::{
    CancellationStage, FallbackDirective, GeneratedTextGateDecision,
    GenerationCancellationRegistry, GenerationCancellationToken, GenerationDisposition,
    GenerationRequest, GenerationResult, GenerationTrace, GenerativePipeline, PhraseOptions,
    select_fallback_directive,
};
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction};
use aivtuber_runtime::{
    AudioPlaybackCommand, AudioPlaybackSink, AvatarPlaybackCommand, AvatarPlaybackSink,
    CachedAssetSelection, CachedPerformer, CachedPlaybackError, CachedPlaybackOutcome,
    CachedPlaybackTiming, ContentAdmitDecision, ControlOutcome, DeferredRevalidation,
    FastPathPreloadReport, LocalVisemeStore, MemoryErasePermit, OutputVerdict, PublicOutput,
    PublicOutputPolicy, RuntimeError, SecurityRuntime, SpeechObservationTracker,
    SpeechTranscriptTracker, ViewerEraseDecision,
};
use aivtuber_scheduler::{
    BusyCause, DeferredOutcome, OutputFailureKind, OutputObservation, ReplyAvailability, Scheduler,
    Status, priority_for_event,
};
use aivtuber_telemetry::{
    CacheLevel, CausalTraceCollector, CausalTraceRetentionConfig, ComparisonMode,
    DegradedSubsystem, EventObservation, RouteClass, StageOutcome, StageReason, TelemetryCollector,
    TelemetryRetentionConfig, TraceStage,
};
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long before its logical start a continuation phrase is handed to an
/// output that [queues gaplessly](AudioOutput::gapless_queue) (#83 S3d).
/// A bounded constant, not a knob: about one tick plus the time a backend needs
/// to decode and append a phrase, far below the 400 ms barge-in confirm.
pub const AUDIO_LOOKAHEAD_MS: u64 = 250;

/// 先行受け渡し中の音声コマンド保持数の上限 (#354)。
const MAX_EARLY_AUDIO_COMMANDS: usize = 32;

pub trait AudioOutput: Send {
    fn execute(&mut self, command: &AudioPlaybackCommand) -> Result<(), EngineError>;

    /// Stop what this output started, fading over at most `fade_ms` when the
    /// backend can (#81 S2, ADR-0005 S-3). Not a new authority: only the
    /// app's own cancel path calls it, after the scheduler has cancelled the
    /// performance. The default reports that audio already started plays out.
    fn stop(&mut self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        let _ = fade_ms;
        Ok(AudioStopOutcome::Unsupported)
    }

    /// Whether this output queues a continuation phrase behind the audio in
    /// flight and starts it sample-exactly (#83 S3d). Only such an output is
    /// handed a continuation command [`AUDIO_LOOKAHEAD_MS`] before its logical
    /// start; the default (`false`) keeps the hand-over at the logical start.
    fn gapless_queue(&self) -> bool {
        false
    }

    /// ギャップレスバックエンドへ先行投入した未開始コマンドを識別子で取り消す (#354)。
    fn recall_queued(&mut self, generation: u64) -> bool {
        let _ = generation;
        false
    }
}

pub trait AvatarOutput: Send {
    /// Operator-facing name of this output, used in health/warning text so a
    /// message never names a different avatar output than the active one.
    fn name(&self) -> &'static str {
        "avatar output"
    }

    fn connect(&mut self) -> Result<(), EngineError> {
        Ok(())
    }
    fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError>;

    /// Whether an `Expression` can show a partial intensity. A hotkey-style
    /// output cannot, so the face settle sends it only the baseline step.
    fn graded_expression(&self) -> bool {
        true
    }
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
    Scenario(aivtuber_domain::ScenarioError),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "{error}"),
            Self::Playback(error) => write!(f, "{error}"),
            Self::Routing(message) => write!(f, "routing failed: {message}"),
            Self::Generation(message) => write!(f, "generation failed: {message}"),
            Self::Adaptation(message) => write!(f, "adaptation failed: {message}"),
            Self::Scenario(error) => write!(f, "scenario refused: {error}"),
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

impl From<aivtuber_domain::ScenarioError> for AppError {
    fn from(value: aivtuber_domain::ScenarioError) -> Self {
        Self::Scenario(value)
    }
}

/// One spoken final that was routed through the content path.
#[derive(Debug)]
pub struct SpokenFinalReport {
    /// The envelope as built; the runtime admitted or refused exactly this.
    pub event: EventEnvelope,
    pub result: Result<ContentProcessOutcome, AppError>,
    /// Runtime-timeline milliseconds from the end of the final's covered audio
    /// to the start of routing. Diagnostics only; no decision reads it.
    pub final_age_ms: u64,
    /// Wall time spent in the content path for this final (diagnostics only).
    pub route_micros: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentProcessOutcome {
    pub admission: ContentAdmitDecision,
    pub playback: Option<CachedPlaybackOutcome>,
    /// Set when the admitted event was a viewer self-erase request (#305): the
    /// event was consumed as a command, never routed as content. `None` when
    /// the request could not be honoured (no asserted actor, no memory
    /// configured) or the event was ordinary content.
    pub memory_erasure: Option<ErasureReport>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationExecutionConfig {
    /// Maximum provider jobs waiting behind the one active worker.
    pub queue_capacity: usize,
}

impl Default for GenerationExecutionConfig {
    fn default() -> Self {
        Self { queue_capacity: 8 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerationExecutionSnapshot {
    pub pending: usize,
    pub in_flight: usize,
    /// Completed results that are published to (or still blocked on) the completion
    /// mailbox and not yet taken by the composition thread. Issue #177.
    pub awaiting_drain: usize,
    pub pending_high_water: usize,
    pub in_flight_high_water: usize,
    pub saturated: u64,
    pub completed: u64,
    pub cancelled_or_stale: u64,
    pub failed: u64,
    pub queue_capacity: usize,
    pub max_in_flight: usize,
    pub shutting_down: bool,
    pub worker_running: bool,
}

impl GenerationExecutionSnapshot {
    /// True when no generation work is outstanding anywhere: not queued, not
    /// executing in the provider worker, and not published-but-undrained on the
    /// completion mailbox (issue #177).
    ///
    /// Callers that treat `pending == 0 && in_flight == 0` as "everything is
    /// accounted for" can still observe a false quiescent state, because the
    /// worker publishes its completion after leaving in-flight. Drain-window
    /// accounting lives here so every caller shares one sound predicate.
    pub fn is_quiescent(&self) -> bool {
        self.pending == 0 && self.in_flight == 0 && self.awaiting_drain == 0
    }
}

#[derive(Debug, Default)]
struct GenerationExecutionCounters {
    pending: AtomicUsize,
    in_flight: AtomicUsize,
    awaiting_drain: AtomicUsize,
    /// Test seam: widens the claim/release window of both handoffs so the
    /// intermediate snapshot state can be asserted deterministically.
    #[cfg(test)]
    handoff_pause_nanos: AtomicU64,
    pending_high_water: AtomicUsize,
    in_flight_high_water: AtomicUsize,
    saturated: AtomicU64,
    completed: AtomicU64,
    cancelled_or_stale: AtomicU64,
    failed: AtomicU64,
}

impl GenerationExecutionCounters {
    fn snapshot(
        &self,
        config: GenerationExecutionConfig,
        shutting_down: bool,
    ) -> GenerationExecutionSnapshot {
        GenerationExecutionSnapshot {
            pending: self.pending.load(Ordering::SeqCst),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            awaiting_drain: self.awaiting_drain.load(Ordering::SeqCst),
            pending_high_water: self.pending_high_water.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
            saturated: self.saturated.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            cancelled_or_stale: self.cancelled_or_stale.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            queue_capacity: config.queue_capacity,
            max_in_flight: 1,
            shutting_down,
            worker_running: false,
        }
    }
}

/// Outstanding generation work is a state machine with three states (queued,
/// in-flight, awaiting drain) and two transitions. Both transitions claim the
/// new state **before** releasing the old one, so a job is always counted in at
/// least one counter.
///
/// The three state counters and the loads that read them in
/// `GenerationExecutionCounters::snapshot` are `SeqCst`. Source ordering alone
/// would not be enough: with `Relaxed` there is no ordering between writes to
/// different atomics, so a snapshot could observe the released counter (0) while
/// still reading the pre-claim value of the claimed counter (0). `SeqCst` places
/// the writer's claim/release pair and the reader's loads in one total order, and
/// the reader reads the counters in transition order (pending, in_flight,
/// awaiting_drain). Observing a released counter therefore places every load
/// after the release in that order, which is after the corresponding claim — so
/// the claimed counter cannot still read its old value. `is_quiescent()` is
/// consequently sound for any interleaving (issue #177).
///
/// Issue #177: dequeue handoff, pending -> in_flight.
fn note_taken_into_flight(counters: &GenerationExecutionCounters) {
    claim_in_flight(counters);
    pause_handoff(counters);
    release_pending(counters);
}

fn claim_in_flight(counters: &GenerationExecutionCounters) -> usize {
    let in_flight = counters.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    update_high_water(&counters.in_flight_high_water, in_flight);
    in_flight
}

fn release_pending(counters: &GenerationExecutionCounters) {
    counters.pending.fetch_sub(1, Ordering::SeqCst);
}

/// Issue #177: publish handoff, in_flight -> awaiting drain. See
/// `note_taken_into_flight` for why the claim precedes the release.
fn note_ready_to_publish(counters: &GenerationExecutionCounters) {
    claim_drain_window(counters);
    pause_handoff(counters);
    release_in_flight(counters);
}

fn claim_drain_window(counters: &GenerationExecutionCounters) {
    counters.awaiting_drain.fetch_add(1, Ordering::SeqCst);
}

fn release_in_flight(counters: &GenerationExecutionCounters) {
    counters.in_flight.fetch_sub(1, Ordering::SeqCst);
}

/// Widens the handoff window between claim and release. Production builds never
/// pause; only tests use it to observe the intermediate state deterministically.
#[cfg(not(test))]
fn pause_handoff(_counters: &GenerationExecutionCounters) {}

#[cfg(test)]
fn pause_handoff(counters: &GenerationExecutionCounters) {
    let nanos = counters.handoff_pause_nanos.load(Ordering::Relaxed);
    if nanos > 0 {
        std::thread::sleep(Duration::from_nanos(nanos));
    }
}

fn update_high_water(counter: &AtomicUsize, value: usize) {
    let mut current = counter.load(Ordering::Relaxed);
    while value > current {
        match counter.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug, Clone)]
struct GenerationCompletionContext {
    /// Granted budget reservation for this generation (issue #69). Cloned
    /// shallowly into completions; settled once by the committing path via
    /// the `finished` flag, and its concurrency slot returns on drop even if
    /// the completion is never observed.
    budget_reservation: Option<BudgetReservation>,
    routing_latency_us: u64,
    decision: Option<DecisionReplayRecord>,
    template_fallback: Option<&'static str>,
    interaction_deadline: InteractionDeadline,
    remaining_at_route_ms: u64,
    remaining_at_generation_ms: u64,
    /// Causal trace identity for the originating event (#65).
    correlation_id: String,
}

impl GenerationCompletionContext {
    /// A copy for the composition thread's phrase bookkeeping. The budget
    /// reservation is deliberately not copied: dropping a copy would return
    /// the concurrency slot a second time.
    fn without_reservation(&self) -> Self {
        Self {
            budget_reservation: None,
            routing_latency_us: self.routing_latency_us,
            decision: self.decision.clone(),
            template_fallback: self.template_fallback,
            interaction_deadline: self.interaction_deadline,
            remaining_at_route_ms: self.remaining_at_route_ms,
            remaining_at_generation_ms: self.remaining_at_generation_ms,
            correlation_id: self.correlation_id.clone(),
        }
    }
}

struct GenerationWork {
    generation_id: u64,
    request: GenerationRequest,
    cancellation: GenerationCancellationToken,
    fallback: FallbackDirective,
    output_policy: PublicOutputPolicy,
    submitted_at: Instant,
    provider_deadline: Instant,
    context: GenerationCompletionContext,
    /// Set when this job runs as a phrased reply (issue #83).
    phrase: Option<phrase_commit::PhraseWork>,
}

struct GenerationCompletion {
    generation_id: u64,
    request: GenerationRequest,
    cancellation: GenerationCancellationToken,
    fallback: FallbackDirective,
    result: Result<GenerationResult, String>,
    output_decision: Option<PublicOutput>,
    generation_latency_us: u64,
    queue_wait_us: u64,
    provider_latency_us: u64,
    completed_at: Instant,
    provider_deadline: Instant,
    context: GenerationCompletionContext,
}

/// The worker-side output gate: the public-output policy snapshot's verdict as
/// the pipeline's decision, plus the full verdict for the commit-side check.
fn gate_generated_text(
    policy: &PublicOutputPolicy,
    generated_text: &str,
) -> (GeneratedTextGateDecision, PublicOutput) {
    let output = policy.evaluate(generated_text);
    let decision = if matches!(output.verdict, OutputVerdict::Allow | OutputVerdict::Redact) {
        output
            .text
            .clone()
            .map(GeneratedTextGateDecision::Publish)
            .unwrap_or(GeneratedTextGateDecision::Reject)
    } else {
        GeneratedTextGateDecision::Reject
    };
    (decision, output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationSubmitError {
    Saturated,
    ShuttingDown,
    Disconnected,
}

struct GenerationExecutor {
    config: GenerationExecutionConfig,
    work_tx: Option<SyncSender<GenerationWork>>,
    completion_rx: Receiver<GenerationCompletion>,
    counters: Arc<GenerationExecutionCounters>,
    shutting_down: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Lazy re-synthesis of stale generated speech (#222 S2, ADR-0004 D-4).
    resynthesis: speech_resynthesis::ResynthesisWorker,
    /// The projection identity the pipeline uses now.
    projection_key: String,
}

impl GenerationExecutor {
    fn new(
        pipeline: GenerativePipeline,
        config: GenerationExecutionConfig,
    ) -> Result<Self, &'static str> {
        if config.queue_capacity == 0 {
            return Err("generation queue_capacity must be greater than zero");
        }

        let (work_tx, work_rx) = mpsc::sync_channel::<GenerationWork>(config.queue_capacity);
        // One active worker plus every queued job can complete before the composition
        // thread drains the mailbox. Keep the mailbox bounded to that exact maximum.
        let completion_capacity = config.queue_capacity.saturating_add(1);
        let (completion_tx, completion_rx) =
            mpsc::sync_channel::<GenerationCompletion>(completion_capacity);
        let counters = Arc::new(GenerationExecutionCounters::default());
        let worker_counters = Arc::clone(&counters);
        let shutting_down = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutting_down);
        let pipeline = Arc::new(pipeline);
        let projection_key = pipeline.speech_lexicon().identity().key();
        let resynthesis = speech_resynthesis::ResynthesisWorker::spawn(Arc::clone(&pipeline));

        let worker = thread::Builder::new()
            .name("aivtuber-generation-worker".to_owned())
            .spawn(move || {
                while let Ok(work) = work_rx.recv() {
                    // Issue #177: the dequeue handoff must overlap. Claiming
                    // in-flight before releasing pending keeps the two states
                    // summing to at least one for a job that is dequeued but not
                    // yet started, which is otherwise observable as a false
                    // quiescent snapshot.
                    note_taken_into_flight(&worker_counters);
                    if worker_shutdown.load(Ordering::Acquire) {
                        // The job was claimed and then dropped without producing
                        // a completion, so in-flight is released on its own.
                        release_in_flight(&worker_counters);
                        worker_counters
                            .cancelled_or_stale
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let provider_started = Instant::now();
                    let queue_wait_us = elapsed_us(work.submitted_at);

                    let (result, output_decision) = match &work.phrase {
                        None => {
                            let mut output_decision = None;
                            let result = pipeline
                                .run_with_output_gate_and_fallback_deadline(
                                    &work.request,
                                    &work.cancellation,
                                    work.fallback.clone(),
                                    Some(work.provider_deadline),
                                    |generated_text| {
                                        let (decision, output) = gate_generated_text(
                                            &work.output_policy,
                                            generated_text,
                                        );
                                        output_decision = Some(output);
                                        decision
                                    },
                                )
                                .map_err(|error| error.to_string());
                            (result, output_decision)
                        }
                        Some(phrase) => {
                            // The gate's last decision is per phrase: every
                            // delivered phrase takes it, so the commit side
                            // checks the decision for exactly that text.
                            let last_decision = RefCell::new(None);
                            let mut delivery = phrase_commit::ChannelDelivery {
                                generation_id: work.generation_id,
                                work: phrase,
                                cancellation: &work.cancellation,
                                last_decision: &last_decision,
                                submitted_at: work.submitted_at,
                                provider_started,
                                queue_wait_us,
                                shutting_down: &worker_shutdown,
                            };
                            let result = pipeline
                                .run_phrased(
                                    &work.request,
                                    &work.cancellation,
                                    work.fallback.clone(),
                                    Some(work.provider_deadline),
                                    &PhraseOptions {
                                        response_id: work.generation_id,
                                        config: phrase.config,
                                    },
                                    |generated_text| {
                                        let (decision, output) = gate_generated_text(
                                            &work.output_policy,
                                            generated_text,
                                        );
                                        *last_decision.borrow_mut() = Some(output);
                                        decision
                                    },
                                    &mut delivery,
                                )
                                .map_err(|error| error.to_string());
                            (result, last_decision.into_inner())
                        }
                    };

                    let provider_latency_us = elapsed_us(provider_started);
                    let completed_at = Instant::now();
                    note_ready_to_publish(&worker_counters);
                    let completion = GenerationCompletion {
                        generation_id: work.generation_id,
                        request: work.request,
                        cancellation: work.cancellation,
                        fallback: work.fallback,
                        result,
                        output_decision,
                        generation_latency_us: elapsed_us(work.submitted_at),
                        queue_wait_us,
                        provider_latency_us,
                        completed_at,
                        provider_deadline: work.provider_deadline,
                        context: work.context,
                    };

                    // Issue #177: the drain window is claimed by
                    // `note_ready_to_publish` above, before the completion is handed
                    // to the composition thread.
                    //
                    // Blocking here is safe: only the provider worker can block, while
                    // the composition/control thread remains responsive. The bounded
                    // mailbox prevents unbounded completed-result retention.
                    if completion_tx.send(completion).is_err() {
                        // The mailbox receiver is gone, so nothing will ever drain
                        // this result; release the drain window before stopping.
                        worker_counters
                            .awaiting_drain
                            .fetch_sub(1, Ordering::SeqCst);
                        break;
                    }
                    worker_counters.completed.fetch_add(1, Ordering::Relaxed);
                }
            })
            .map_err(|_| "failed to spawn generation worker")?;

        Ok(Self {
            config,
            work_tx: Some(work_tx),
            completion_rx,
            counters,
            shutting_down,
            worker: Some(worker),
            resynthesis,
            projection_key,
        })
    }

    fn try_submit(&self, work: GenerationWork) -> Result<(), GenerationSubmitError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(GenerationSubmitError::ShuttingDown);
        }
        let Some(sender) = &self.work_tx else {
            return Err(GenerationSubmitError::ShuttingDown);
        };

        let pending = self.counters.pending.fetch_add(1, Ordering::SeqCst) + 1;
        match sender.try_send(work) {
            Ok(()) => {
                update_high_water(&self.counters.pending_high_water, pending);
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.counters.pending.fetch_sub(1, Ordering::SeqCst);
                self.counters.saturated.fetch_add(1, Ordering::Relaxed);
                Err(GenerationSubmitError::Saturated)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.counters.pending.fetch_sub(1, Ordering::SeqCst);
                Err(GenerationSubmitError::Disconnected)
            }
        }
    }

    fn try_recv(&self) -> Option<GenerationCompletion> {
        let completion = self.completion_rx.try_recv().ok()?;
        // The composition thread owns the completion from here on. The remaining
        // stale/cancelled, failure, and commit paths all record their observation
        // within the same tick, so draining here is enough for quiescence to mean
        // "no undrained result" (issue #177).
        self.counters.awaiting_drain.fetch_sub(1, Ordering::SeqCst);
        Some(completion)
    }

    fn snapshot(&self) -> GenerationExecutionSnapshot {
        let mut snapshot = self
            .counters
            .snapshot(self.config, self.shutting_down.load(Ordering::Acquire));
        snapshot.worker_running = self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished());
        snapshot
    }

    fn note_cancelled_or_stale(&self) {
        self.counters
            .cancelled_or_stale
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_failed(&self) {
        self.counters.failed.fetch_add(1, Ordering::Relaxed);
    }

    fn shutdown(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        self.resynthesis.shutdown();
        self.work_tx.take();
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}

impl Drop for GenerationExecutor {
    fn drop(&mut self) {
        self.shutdown();
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
        // If a provider ignores cancellation, dropping the JoinHandle detaches the
        // bounded worker until that provider call returns. The sender is already
        // closed and shutdown fencing prevents any late result from publishing.
    }
}

pub struct GenerativeRuntime {
    executor: GenerationExecutor,
    cancellation: Arc<GenerationCancellationRegistry>,
    next_generation_id: u64,
    active_generation_id: Option<u64>,
    /// Buffered whole-reply commit (default) or phrase-level commit (#83).
    phrase_mode: PhraseMode,
    phrase_tx: SyncSender<phrase_commit::PhraseMessage>,
    phrase_rx: Receiver<phrase_commit::PhraseMessage>,
    phrase: phrase_commit::PhraseBook,
    speech_projection: SpeechProjectionMetrics,
}

/// Cumulative display-to-speech projection counters (ADR-0004, #222). Counts
/// only; `passthrough_latin` is `projection_passthrough_latin`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeechProjectionMetrics {
    pub replies: u64,
    pub lexicon_hits: u64,
    pub passthrough_latin: u64,
}

impl GenerativeRuntime {
    pub fn new(pipeline: GenerativePipeline) -> Self {
        Self::with_execution_config(pipeline, GenerationExecutionConfig::default())
            .expect("default generation execution config must be valid")
    }

    pub fn with_execution_config(
        pipeline: GenerativePipeline,
        config: GenerationExecutionConfig,
    ) -> Result<Self, &'static str> {
        let (phrase_tx, phrase_rx) = mpsc::sync_channel::<phrase_commit::PhraseMessage>(
            phrase_commit::PHRASE_MAILBOX_CAPACITY,
        );
        Ok(Self {
            executor: GenerationExecutor::new(pipeline, config)?,
            cancellation: Arc::new(GenerationCancellationRegistry::default()),
            next_generation_id: 0,
            active_generation_id: None,
            phrase_mode: PhraseMode::Buffered,
            phrase_tx,
            phrase_rx,
            phrase: phrase_commit::PhraseBook::default(),
            speech_projection: SpeechProjectionMetrics::default(),
        })
    }

    /// Choose how generated replies are committed. Buffered whole-reply mode
    /// is the default and stays available; phrase mode commits each complete,
    /// gated, synthesized phrase atomically (issue #83).
    pub fn with_phrase_mode(mut self, mode: PhraseMode) -> Result<Self, String> {
        if let PhraseMode::Phrased(config) = mode {
            config.validate()?;
        }
        self.phrase_mode = mode;
        Ok(self)
    }

    pub fn phrase_mode(&self) -> PhraseMode {
        self.phrase_mode
    }

    fn try_next_phrase(&self) -> Option<phrase_commit::PhraseMessage> {
        self.phrase_rx.try_recv().ok()
    }

    pub fn cancellation_registry(&self) -> Arc<GenerationCancellationRegistry> {
        Arc::clone(&self.cancellation)
    }

    pub fn execution_snapshot(&self) -> GenerationExecutionSnapshot {
        self.executor.snapshot()
    }

    /// Test seam for issue #177: hold each handoff window open for `pause` so a
    /// test can assert the intermediate snapshot state. Not compiled in
    /// production builds.
    #[cfg(test)]
    pub fn set_test_handoff_pause(&self, pause: Duration) {
        self.executor
            .counters
            .handoff_pause_nanos
            .store(pause.as_nanos() as u64, Ordering::Relaxed);
    }

    fn submit(
        &mut self,
        request: GenerationRequest,
        fallback: FallbackDirective,
        output_policy: PublicOutputPolicy,
        provider_deadline: Instant,
        context: GenerationCompletionContext,
    ) -> Result<u64, GenerationSubmitError> {
        self.next_generation_id = self.next_generation_id.saturating_add(1);
        let generation_id = self.next_generation_id;
        let source_event = request.source_event.clone();
        let seed = request.seed;
        let cancellation = GenerationCancellationToken::default();
        let started = Arc::new(AtomicU32::new(0));
        let phrase = match self.phrase_mode {
            PhraseMode::Buffered => None,
            PhraseMode::Phrased(config) => Some(phrase_commit::PhraseWork {
                config,
                mailbox: self.phrase_tx.clone(),
                started: Arc::clone(&started),
            }),
        };
        // The composition thread keeps its own copy of the context for the
        // phrase commits that precede the worker's final completion; the
        // budget reservation stays with the one completion that settles it.
        let run_context = phrase
            .is_some()
            .then(|| (context.without_reservation(), fallback.clone()));
        let work = GenerationWork {
            generation_id,
            request,
            cancellation: cancellation.clone(),
            fallback,
            output_policy,
            submitted_at: Instant::now(),
            provider_deadline,
            context,
            phrase,
        };
        match self.executor.try_submit(work) {
            Ok(()) => {
                self.cancellation
                    .activate(&source_event, cancellation.clone());
                self.active_generation_id = Some(generation_id);
                if let Some((run_context, run_fallback)) = run_context {
                    self.phrase.runs.push(phrase_commit::PhraseRun::new(
                        generation_id,
                        source_event,
                        seed,
                        cancellation,
                        started,
                        provider_deadline,
                        run_context,
                        run_fallback,
                    ));
                }
                Ok(generation_id)
            }
            Err(error) => {
                cancellation.cancel();
                Err(error)
            }
        }
    }

    /// A generated reply is in flight (trusted busy state for #75).
    fn is_generating(&self) -> bool {
        self.active_generation_id.is_some()
    }

    fn is_current(&self, generation_id: u64, event_id: &str) -> bool {
        self.active_generation_id == Some(generation_id)
            && self.cancellation.active_event_id().as_deref() == Some(event_id)
    }

    /// Cancel and release the generation of `event_id` so it no longer counts
    /// as busy (#87: chat must not wait behind an idle initiative). Its late
    /// completion is then read as stale. Returns whether one was active.
    fn abandon_active(&mut self, event_id: &str) -> bool {
        if self.cancellation.active_event_id().as_deref() != Some(event_id) {
            return false;
        }
        self.cancellation.cancel_active();
        self.cancellation.finish(event_id);
        if let Some(generation_id) = self.active_generation_id.take() {
            self.phrase
                .runs
                .retain(|run| run.generation_id != generation_id);
        }
        true
    }

    fn finish(&mut self, generation_id: u64, event_id: &str) {
        if self.active_generation_id == Some(generation_id) {
            self.active_generation_id = None;
        }
        self.cancellation.finish(event_id);
    }

    fn try_next_completion(&self) -> Option<GenerationCompletion> {
        self.executor.try_recv()
    }

    /// The projection identity generated speech is produced under now.
    pub fn speech_projection_key(&self) -> &str {
        &self.executor.projection_key
    }

    fn note_cancelled_or_stale(&self) {
        self.executor.note_cancelled_or_stale();
    }

    fn note_failed(&self) {
        self.executor.note_failed();
    }

    fn shutdown(&mut self) {
        self.cancellation.cancel_active();
        self.active_generation_id = None;
        self.phrase.runs.clear();
        self.executor.shutdown();
    }

    fn is_shutting_down(&self) -> bool {
        self.executor.is_shutting_down()
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

#[derive(Debug, Clone)]
struct DeadlineObservation {
    deadline: InteractionDeadline,
    remaining_at_route_ms: u64,
    remaining_at_generation_ms: Option<u64>,
    exhaustion_stage: Option<DeadlineStage>,
    exhaustion_reason: Option<DeadlineExhaustionReason>,
    generation_queue_wait_us: Option<u64>,
    generation_provider_latency_us: Option<u64>,
    generation_commit_delay_us: Option<u64>,
}

impl DeadlineObservation {
    fn new(deadline: InteractionDeadline, remaining_at_route_ms: u64) -> Self {
        Self {
            deadline,
            remaining_at_route_ms,
            remaining_at_generation_ms: None,
            exhaustion_stage: None,
            exhaustion_reason: None,
            generation_queue_wait_us: None,
            generation_provider_latency_us: None,
            generation_commit_delay_us: None,
        }
    }
}

fn completion_deadline_observation(
    completion: &GenerationCompletion,
    exhaustion: Option<(DeadlineStage, DeadlineExhaustionReason)>,
) -> DeadlineObservation {
    let mut observation = DeadlineObservation::new(
        completion.context.interaction_deadline,
        completion.context.remaining_at_route_ms,
    );
    observation.remaining_at_generation_ms = Some(completion.context.remaining_at_generation_ms);
    observation.generation_queue_wait_us = Some(completion.queue_wait_us);
    observation.generation_provider_latency_us = Some(completion.provider_latency_us);
    observation.generation_commit_delay_us = Some(elapsed_us(completion.completed_at));
    if let Some((stage, reason)) = exhaustion {
        observation.exhaustion_stage = Some(stage);
        observation.exhaustion_reason = Some(reason);
    }
    observation
}

struct HandledEvent {
    playback: Option<CachedPlaybackOutcome>,
    route: RouteClass,
    routing_latency_us: u64,
    generation_latency_us: Option<u64>,
    generation_trace: Option<GenerationTrace>,
    decision: Option<DecisionReplayRecord>,
    /// Deterministic fallback recorded by the planner for a Template
    /// decision (issue #54); `None` when no template fallback applies.
    template_fallback: Option<&'static str>,
    cache_level: Option<CacheLevel>,
    deadline: Option<DeadlineObservation>,
    /// Generation was accepted by the bounded worker and will be observed
    /// when a completion is committed from tick().
    deferred_generation: bool,
    /// Admitted content bytes observed at ingress (bounded, size only).
    admitted_bytes: Option<u64>,
    /// Causal trace identity (#65); `None` on deferred completions that
    /// carry it through `GenerationCompletionContext` instead.
    correlation_id: Option<String>,
}

/// Counted generation fallbacks (issue #288): every generated reply that
/// ended on a typed fallback instead of speech. Keyed by the closed
/// `fallback_reason_name` vocabulary (`timeout`, `unavailable`,
/// `policy_override`, ...), so the map is bounded by that set; no provider
/// text, URL or error message is ever stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerationFallbackCounts {
    pub by_reason: std::collections::BTreeMap<&'static str, u64>,
    /// Fallbacks that put a cached reaction on the avatar (and audio, if the
    /// asset has any).
    pub visible: u64,
    /// Fallbacks that produced nothing because the pack has no usable asset
    /// for the fallback group (the typed `NonVerbalReaction` carries none).
    pub silent: u64,
    /// Chat turns ended on purpose because a newer line (or an operator stop)
    /// took over before they were answered (#469). Counted, never silent.
    pub superseded: u64,
}

impl GenerationFallbackCounts {
    fn note(&mut self, reason: &'static str, visible: bool) {
        *self.by_reason.entry(reason).or_insert(0) += 1;
        if visible {
            self.visible += 1;
        } else {
            self.silent += 1;
        }
    }
}

struct HandledGeneration {
    playback: Option<CachedPlaybackOutcome>,
    route: RouteClass,
    trace: GenerationTrace,
    cache_level: Option<CacheLevel>,
}

enum GenerationSubmission {
    Deferred,
    Immediate {
        handled: Box<HandledGeneration>,
        deadline_exhaustion: Option<(DeadlineStage, DeadlineExhaustionReason)>,
    },
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
    /// Post-turn memory consolidation (#306); `None` (default) writes nothing.
    memory_writer: Option<memory_writer::MemoryWriter>,
    /// Q4 falsifier counters and the loaded seed's identity (#321).
    memory_falsifier: memory_seed::MemoryFalsifier,
    /// Live editing of the operator memory seed (#402); `None` (default)
    /// composes nothing.
    memory_seed_editor: Option<memory_seed::MemorySeedEditor>,
    /// Saves applied so far; names the erase transaction of each one.
    memory_seed_edits: u64,
    speech_observations: Option<SpeechObservationTracker>,
    /// Opt-in live screen perception (#90 S1b); `None` (default) composes nothing.
    perception: Option<perception_service::PerceptionService>,
    /// Live switching of the local Thinking model (#357); `None` (default)
    /// composes nothing and the model is whatever the environment named.
    model_switcher: Option<model_switch::ModelSwitcher>,
    /// The operator's character sheet read at start (#404); `None` unless
    /// `AIVTUBER_PERSONA_FILE` was set, then the load outcome (the sheet, or
    /// why there is none). Changed while running only by
    /// `configure_persona` (#401), read when each Thinking request is built.
    persona: Option<persona::PersonaLoad>,
    /// Live editing of the sheet (#401); `None` (default) composes nothing.
    persona_editor: Option<persona::PersonaEditor>,
    /// The operation panel's launch URL, held only to answer an authenticated
    /// `panel.url` (#388); redacted `Debug`, never logged, audited or put in
    /// status or the support bundle.
    panel_url: Option<aivtuber_adapters::SecretString>,
    /// Set by an authenticated `app.shutdown` (#449) after the emergency stop
    /// ran; the composition loop reads it and exits. Never set by anything
    /// else, and never serialized.
    shutdown_requested: bool,
    thinking_meter: Option<thinking_meter::ThinkingMeter>,
    /// Counters of the opt-in System One affect judge (#426); `None` (default)
    /// when the judge is not composed.
    affect_judge_stats: Option<aivtuber_generative::AffectJudgeStats>,
    /// Live voice settings (#360); `None` (default) composes nothing and the
    /// voice is whatever the environment named.
    voice: Option<voice_control::VoiceController>,
    /// Stream-chat sidecars started from the panel (#361); `None` (default)
    /// composes nothing and chat arrives only through stdin.
    chat_ingress: Option<chat_ingress::ChatIngressController>,
    avatar_store: Option<avatar_control::AvatarController>,
    /// Scene context (#88 S1); `None` (default) composes nothing, and then no
    /// request carries a scene.
    scene: Option<scene_context::SceneContextService>,
    speech_transcripts: Option<SpeechTranscriptTracker>,
    speech_conversation: Option<speech_conversation::SpeechConversation>,
    speech_floor: Option<speech_floor::SpeechFloor>,
    /// Retains drained lifecycle events for replay when composed (#222 S3).
    speech_lifecycle_recorder: Option<aivtuber_runtime::SpeechLifecycleRecorder>,
    speech_barge_in: Option<speech_barge_in::BargeIn>,
    /// The speech sidecar and its on/off state (#400); `None` unless a speech
    /// sidecar is configured. Switchable only when composed with a settings
    /// file (`speech_input.control`).
    speech_input: Option<speech_input::SpeechInputController>,
    /// Live chat-burst selection and deferred replies (#75 S1, #78 S2); `None`
    /// unless composed, so the default per-event path is unchanged.
    chat_burst: Option<chat_burst::ChatBurstLane>,
    /// #79: public-chat comments the audience guard kept from a spoken reply.
    guarded_declined: u64,
    /// The bounded idle initiative (#87 S1); `None` (default) composes nothing
    /// and the performer stays silent when nobody writes.
    initiative: Option<initiative::InitiativeDirector>,
    /// The opt-in local session conversation log (#463); `None` (default)
    /// composes nothing and no turn is ever recorded.
    session_log: Option<session_log::SessionLog>,
    /// The performer's own recent turns for recall (#80 S1); `None` (default)
    /// composes nothing and a request is unchanged.
    recent_turns: Option<recent_turns::RecentTurns>,
    /// Logical sequence of minted initiative events.
    initiative_sequence: u64,
    /// The pending initiative's reply fell back to the idle gesture.
    initiative_fell: bool,
    /// Live editing of the operator topic list (#87 S2); `None` = not composed.
    topics_editor: Option<topics::TopicsEditor>,
    /// #79 S2: the crowd summary for the one reply being dispatched right now.
    /// Set by the crowd path immediately before `dispatch_admitted_event` and
    /// taken by `handle_event`, so it can never reach another event.
    crowd_context: Option<aivtuber_domain::CrowdSummary>,
    /// Runtime time the VAD sidecar's newest feed-liveness beat arrived.
    speech_feed_heartbeat_ms: Option<u64>,
    /// Performer playback intervals, for the half-duplex gate (#396).
    speech_playback: speech_duplex::PlaybackGate,
    audio: audio_stop::AudioChannel,
    avatar: Box<dyn AvatarOutput>,
    stream: Box<dyn StreamOutput>,
    pending_audio: PendingAudio,
    pending_avatar: PendingAvatar,
    /// Face settle after a reply (#447); `None` composes nothing.
    face_settle: Option<face_settle::FaceSettle>,
    /// lookahead により論理開始より前にバックエンドへ渡した音声コマンド (#354)。
    early_audio: Vec<AudioPlaybackCommand>,
    /// Text of the audio the output accepted, for the local panel (#335).
    recent_replies: RecentReplies,
    /// Text-free repeated-phrase measurement and initiative refusal (#89).
    repeat_guard: repeat_guard::RepeatGuard,
    max_dispatch_lateness_ms: u64,
    interaction_deadlines: InteractionDeadlinePolicy,
    /// Generative resource budget governor (issue #69); `None` when the
    /// budget feature is disabled (default) so existing behavior is
    /// untouched.
    budget: Option<GenerativeBudgetGovernor>,
    health: AdapterHealth,
    telemetry: TelemetryCollector,
    causal_traces: CausalTraceCollector,
    generation_fallbacks: GenerationFallbackCounts,
    comparison_mode: ComparisonMode,
    /// Bounded #164 shadow evidence drained from the orchestrator. Retained for
    /// operator diagnostics; never a production output path.
    shadow_comparisons: Vec<ShadowComparisonRecord>,
    /// Live composition identity for operator diagnostics/support bundles
    /// (#167): the active profile name (e.g. `cached`) and the #55
    /// non-secret config fingerprint. Defaults to `unknown` until the
    /// composition root wires the real values through
    /// [`ProductionApp::with_composition_identity`].
    composition_profile: String,
    config_fingerprint: String,
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
            memory_writer: None,
            memory_falsifier: memory_seed::MemoryFalsifier::default(),
            memory_seed_editor: None,
            memory_seed_edits: 0,
            speech_observations: None,
            perception: None,
            model_switcher: None,
            persona: None,
            persona_editor: None,
            panel_url: None,
            shutdown_requested: false,
            thinking_meter: None,
            affect_judge_stats: None,
            voice: None,
            chat_ingress: None,
            avatar_store: None,
            scene: None,
            speech_transcripts: None,
            speech_conversation: None,
            speech_floor: None,
            speech_lifecycle_recorder: None,
            speech_barge_in: None,
            speech_input: None,
            chat_burst: None,
            guarded_declined: 0,
            initiative: None,
            session_log: None,
            recent_turns: None,
            initiative_sequence: 0,
            initiative_fell: false,
            topics_editor: None,
            crowd_context: None,
            speech_feed_heartbeat_ms: None,
            speech_playback: speech_duplex::PlaybackGate::default(),
            audio: audio_stop::AudioChannel::new(audio),
            avatar,
            stream,
            pending_audio: PendingAudio::default(),
            pending_avatar: PendingAvatar::default(),
            face_settle: None,
            early_audio: Vec::new(),
            recent_replies: RecentReplies::default(),
            repeat_guard: repeat_guard::RepeatGuard::random(),
            max_dispatch_lateness_ms,
            interaction_deadlines: InteractionDeadlinePolicy::default(),
            budget: None,
            health: AdapterHealth::default(),
            telemetry: TelemetryCollector::default(),
            causal_traces: CausalTraceCollector::default(),
            generation_fallbacks: GenerationFallbackCounts::default(),
            comparison_mode: ComparisonMode::DeterministicOnly,
            shadow_comparisons: Vec::new(),
            composition_profile: "unknown".to_owned(),
            config_fingerprint: "unknown".to_owned(),
        }
    }

    /// Wire the live composition identity (#167) so `aivtuberctl diagnose`
    /// reports the actual profile and config fingerprint instead of
    /// placeholders. The daemon passes the same values it prints in its
    /// startup `composition profile:` line.
    pub fn with_composition_identity(mut self, profile: &str, config_fingerprint: &str) -> Self {
        self.composition_profile = profile.to_owned();
        self.config_fingerprint = config_fingerprint.to_owned();
        self
    }

    /// Non-secret composition identity for diagnostics/support bundles.
    pub fn composition_identity(&self) -> (&str, &str) {
        (&self.composition_profile, &self.config_fingerprint)
    }

    pub fn with_generation(mut self, generative: GenerativeRuntime) -> Self {
        // D-4: assets recorded under another projection are stale-but-playable.
        self.performer
            .speech_freshness_mut()
            .set_current(Some(generative.speech_projection_key().to_owned()));
        self.generative = Some(generative);
        self.comparison_mode = ComparisonMode::FullGenerative;
        self
    }

    pub fn with_interaction_deadline_policy(
        mut self,
        policy: InteractionDeadlinePolicy,
    ) -> Result<Self, AppError> {
        self.interaction_deadlines = policy.validate()?;
        Ok(self)
    }

    /// Enable the generative resource budget governor (issue #69). Disabled
    /// by default; when absent, admission always succeeds and no accounting
    /// state changes, preserving pre-#69 behavior exactly.
    pub fn with_generative_budget_policy(mut self, policy: GenerativeBudgetPolicy) -> Self {
        self.budget = Some(GenerativeBudgetGovernor::new(policy));
        self
    }

    /// Budget governor access for diagnostics/support bundles; `None` when
    /// the budget feature is disabled.
    pub fn generative_budget_governor(&self) -> Option<&GenerativeBudgetGovernor> {
        self.budget.as_ref()
    }

    /// Bounded budget snapshot for support bundles/operator diagnostics
    /// (issue #69); empty when the budget is disabled.
    pub fn generative_budget_snapshot(&self, now_ms: u64) -> Vec<GenerativeBudgetRecord> {
        self.budget
            .as_ref()
            .map(|governor| governor.snapshot(now_ms))
            .unwrap_or_default()
    }

    pub fn interaction_deadline_policy(&self) -> InteractionDeadlinePolicy {
        self.interaction_deadlines
    }

    pub fn with_adaptation(mut self, adaptation: AdaptationRuntime) -> Self {
        self.adaptation = Some(adaptation);
        self
    }

    /// Enable the permit-gated production memory writer (#306, ADR-0008 Q1).
    /// Off unless called. It writes only through the adaptation runtime, so
    /// without one it retains nothing.
    pub fn with_memory_writer(mut self, config: MemoryWriterConfig) -> Result<Self, String> {
        self.memory_writer = Some(memory_writer::MemoryWriter::new(config)?);
        Ok(self)
    }

    /// Writer counters for the ADR-0008 Q4 falsifier; `None` when disabled.
    pub fn memory_writer_metrics(&self) -> Option<MemoryWriterMetrics> {
        self.memory_writer.as_ref().map(|writer| writer.metrics())
    }

    /// Load the operator-owned memory seed file (#321, ADR-0008 S2) into
    /// working memory as operator-trusted, actorless durable entries.
    ///
    /// Call once at startup, after `with_adaptation`. Every entry is admitted
    /// by the security runtime's seed permit (the claim must pass the public
    /// output gate unchanged), so every permit is minted before anything is
    /// stored and one refused entry loads nothing. The entries are ordinary
    /// durable memory: the #305 operator erase removes them for the session
    /// (`erase_memory_seed_operator`), and an erase never touches the file.
    pub fn with_memory_seed(
        mut self,
        seed: &MemorySeed,
        now_ms: u64,
    ) -> Result<Self, MemorySeedError> {
        self.load_memory_seed(seed, now_ms)?;
        Ok(self)
    }

    /// The loading step of [`Self::with_memory_seed`], for a caller that keeps
    /// the app on refusal. Stores nothing unless every entry is admitted.
    pub fn load_memory_seed(
        &mut self,
        seed: &MemorySeed,
        now_ms: u64,
    ) -> Result<(), MemorySeedError> {
        let permits = self.admit_memory_seed(seed, false)?;
        self.store_memory_seed(seed, &permits, now_ms)
    }

    /// Every check a seed must pass before anything is stored, and its permits.
    /// `replacing` discounts the entries a live save is about to remove, so the
    /// capacity check sees the store as it will be. Changes no memory.
    fn admit_memory_seed(
        &mut self,
        seed: &MemorySeed,
        replacing: bool,
    ) -> Result<Vec<aivtuber_runtime::MemoryWritePermit>, MemorySeedError> {
        let Some(adaptation) = self.adaptation.as_ref() else {
            return Err(MemorySeedError::AdaptationUnavailable);
        };
        let config = adaptation.memory.config();
        let replaced = if replacing {
            adaptation
                .memory
                .entries()
                .iter()
                .filter(|entry| entry.source.source == aivtuber_runtime::OPERATOR_SEED_SOURCE)
                .count()
        } else {
            0
        };
        // Leave at least half of the store to the runtime's own memory.
        let limit = (config.max_entries / 2).min(
            config
                .max_entries
                .saturating_sub(adaptation.memory.len().saturating_sub(replaced)),
        );
        if seed.len() > limit {
            return Err(MemorySeedError::StoreCapacity {
                entries: seed.len(),
                limit,
            });
        }
        // Retention truncates an over-long claim; a seeded claim is never
        // shortened, so a store with a smaller claim bound refuses the seed.
        if let Some(index) = seed
            .entries()
            .iter()
            .position(|entry| entry.claim.len() > config.max_claim_bytes)
        {
            return Err(MemorySeedError::StoreClaimBound {
                index,
                limit: config.max_claim_bytes,
            });
        }
        let mut permits = Vec::with_capacity(seed.len());
        for (index, entry) in seed.entries().iter().enumerate() {
            let event_id = format!("{MEMORY_SEED_EVENT_PREFIX}{}", index + 1);
            let permit = self
                .security
                .authorize_operator_seed_memory_write(&event_id, &entry.claim)
                .ok_or(MemorySeedError::Refused { index })?;
            permits.push(permit);
        }
        Ok(permits)
    }

    fn store_memory_seed(
        &mut self,
        seed: &MemorySeed,
        permits: &[aivtuber_runtime::MemoryWritePermit],
        now_ms: u64,
    ) -> Result<(), MemorySeedError> {
        let adaptation = self
            .adaptation
            .as_mut()
            .ok_or(MemorySeedError::AdaptationUnavailable)?;
        for (entry, permit) in seed.entries().iter().zip(permits) {
            adaptation
                .memory
                .remember_durable(permit, &entry.claim, Some(entry.kind.topic()), now_ms)
                .map_err(|error| MemorySeedError::Store(error.to_string()))?;
        }
        self.memory_falsifier.note_seed(seed);
        Ok(())
    }

    /// Opt in to viewing and editing the memory seed while running (#402).
    /// Only the authenticated `memory.admin` capability can use it
    /// (`configure_memory_seed`); `editor` names the file a save is written to
    /// (none = in memory only). Needs the adaptation runtime.
    pub fn with_memory_seed_editor(mut self, editor: MemorySeedEditor) -> Self {
        self.memory_seed_editor = Some(editor);
        self
    }

    /// In-place form of [`Self::with_memory_seed_editor`] for the composition
    /// root, whose stack frame cannot afford another by-value move of the app.
    pub fn set_memory_seed_editor(&mut self, editor: MemorySeedEditor) {
        self.memory_seed_editor = Some(editor);
    }

    /// Whether the seed can be edited while running (the operator endpoint
    /// then holds `memory.admin` for the `memory.seed.*` actions).
    pub fn memory_seed_editor_composed(&self) -> bool {
        self.memory_seed_editor.is_some() && self.adaptation.is_some()
    }

    /// The seed as the operator last wrote it (loaded or saved).
    fn current_memory_seed(&self) -> MemorySeed {
        self.memory_falsifier
            .seed()
            .cloned()
            .unwrap_or_else(MemorySeed::empty)
    }

    /// Live-edit state for the operator surfaces; counts and the identity hash
    /// only. `None` unless composed.
    pub fn memory_seed_editor_status(&self) -> Option<MemorySeedEditorStatus> {
        let editor = self.memory_seed_editor.as_ref()?;
        self.adaptation.as_ref()?;
        let seed = self.current_memory_seed();
        Some(MemorySeedEditorStatus {
            persisted: editor.path().is_some(),
            thinking_remote: editor.thinking_remote(),
            entries: seed.len(),
            content_hash: seed.content_hash(),
        })
    }

    /// Apply one memory-seed operator request (#402). `authority` must hold
    /// the authenticated `memory.admin` capability. `action` is
    /// `memory.seed.get` or `memory.seed.set` (`argument` is the whole seed
    /// document, `schemas/memory-seed.schema.json`).
    ///
    /// A set is validated by the seed parser and admitted by the same gates as
    /// the start-up load before anything changes; then the file is written
    /// atomically when one is configured; only then are the live seed entries
    /// replaced. A refusal or a failed write leaves the file and the live
    /// entries unchanged. Details are fixed phrases plus counts and the hash,
    /// never a claim; only `get` returns the document, to the caller.
    pub fn configure_memory_seed(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
        now_ms: u64,
    ) -> Result<MemorySeedOutcome, String> {
        if !authority.has_capability(aivtuber_domain::Capability::MemoryAdmin) {
            return Err("memory seed editing is not authorized".to_owned());
        }
        if !self.memory_seed_editor_composed() {
            return Err("memory seed editing is not configured".to_owned());
        }
        match action {
            "memory.seed.get" => {
                let seed = self.current_memory_seed();
                Ok(MemorySeedOutcome {
                    detail: format!(
                        "memory seed: entries={} content_hash={}",
                        seed.len(),
                        seed.content_hash()
                    ),
                    seed: Some(seed),
                })
            }
            "memory.seed.set" => {
                let Some(text) = argument else {
                    return Err("memory seed not changed: no document given".to_owned());
                };
                let not_changed =
                    |error: MemorySeedError| format!("memory seed not changed: {error}");
                // The parser's messages can quote the document (serde) or echo
                // a supplied value, so those two are fixed phrases here.
                let seed = MemorySeed::parse(text).map_err(|error| match error {
                    MemorySeedError::Parse(_) => {
                        "memory seed not changed: the document is not valid seed JSON".to_owned()
                    }
                    MemorySeedError::Invalid { ref path, .. } if path == "schema_version" => {
                        "memory seed not changed: schema_version is not supported".to_owned()
                    }
                    other => not_changed(other),
                })?;
                if seed.to_json().len() > MAX_SEED_FILE_BYTES {
                    // The file is written pretty-printed; the next start would
                    // refuse one over the bound.
                    return Err(not_changed(MemorySeedError::TooLarge {
                        bytes: seed.to_json().len(),
                    }));
                }
                if !self
                    .adaptation
                    .as_ref()
                    .is_some_and(|a| a.memory.pending_erasure_ids().is_empty())
                {
                    return Err(not_changed(MemorySeedError::ErasePending));
                }
                let permits = self.admit_memory_seed(&seed, true).map_err(not_changed)?;
                let persisted = match self
                    .memory_seed_editor
                    .as_ref()
                    .and_then(|editor| editor.path())
                {
                    Some(path) => {
                        // The canonical form, the bytes `memory-export` also
                        // writes, so a restart loads exactly this seed.
                        if persona::write_persona_file_atomic(path, &seed.to_json()).is_err() {
                            return Err(
                                "memory seed not changed: the seed file could not be written"
                                    .to_owned(),
                            );
                        }
                        true
                    }
                    None => false,
                };
                self.memory_seed_edits += 1;
                let request_id = format!("memory-seed-set-{}", self.memory_seed_edits);
                self.erase_memory_seed_operator(&request_id, authority, now_ms)
                    .map_err(|error| format!("memory seed partly applied: {error}"))?;
                self.store_memory_seed(&seed, &permits, now_ms)
                    .map_err(|error| format!("memory seed partly applied: {error}"))?;
                let kept = if persisted {
                    "saved to the seed file"
                } else {
                    "applied in memory only (no seed file is configured)"
                };
                let detail = format!(
                    "memory seed updated, {kept}: entries={} content_hash={}",
                    seed.len(),
                    seed.content_hash()
                );
                eprintln!("memory_seed: {detail}");
                Ok(MemorySeedOutcome { detail, seed: None })
            }
            _ => Err("not a memory seed action".to_owned()),
        }
    }

    /// Identity (id, revision, content hash, entry count) of the loaded seed:
    /// the replay input for runs that loaded one. `None` when none was loaded.
    pub fn memory_seed_identity(&self) -> Option<&MemorySeedIdentity> {
        self.memory_falsifier.identity()
    }

    /// Q4 falsifier counters: seed-file claims versus writer claims the seed
    /// could not have reproduced, as delivered into generation requests.
    pub fn memory_falsifier_metrics(&self) -> MemoryFalsifierMetrics {
        self.memory_falsifier.metrics()
    }

    /// Operator erase of every seeded entry for the running session (#305).
    /// Needs the authenticated `memory.admin` capability. Runs one erase
    /// transaction per `MAX_ERASE_MEMORY_IDS` entries, named
    /// `{request_id}.{n}`; nothing seeded returns no reports. The seed file is
    /// operator-owned and untouched, so the next start reloads it.
    pub fn erase_memory_seed_operator(
        &mut self,
        request_id: &str,
        authority: &AuthenticatedControl,
        now_ms: u64,
    ) -> Result<Vec<ErasureReport>, AppError> {
        let ids: Vec<u64> = self
            .adaptation
            .as_ref()
            .map(|adaptation| {
                adaptation
                    .memory
                    .entries()
                    .iter()
                    .filter(|entry| entry.source.source == aivtuber_runtime::OPERATOR_SEED_SOURCE)
                    .map(|entry| entry.memory_id)
                    .collect()
            })
            .unwrap_or_default();
        let mut reports = Vec::new();
        for (chunk_index, chunk) in ids
            .chunks(aivtuber_runtime::MAX_ERASE_MEMORY_IDS)
            .enumerate()
        {
            reports.push(self.erase_memory_operator(
                &format!("{request_id}.{chunk_index}"),
                authority,
                None,
                chunk,
                now_ms,
            )?);
        }
        Ok(reports)
    }

    /// Export the #306 writer's live entries in seed-file format (#321). Needs
    /// the authenticated `memory.admin` capability. The runtime only *returns*
    /// the seed to the authenticated operator command: it never writes a file,
    /// so promoting an export into a real seed is the operator's own review and
    /// copy. The export names only the performer's own published sentences, as
    /// they were said; no entry carries an actor.
    pub fn export_memory_seed(
        &mut self,
        authority: &AuthenticatedControl,
        now_ms: u64,
    ) -> Result<MemorySeedExport, MemorySeedError> {
        if !authority.has_capability(aivtuber_domain::Capability::MemoryAdmin) {
            return Err(MemorySeedError::Unauthorized);
        }
        let Some(mut writer) = self.memory_writer.take() else {
            return Err(MemorySeedError::WriterDisabled);
        };
        let result = match self.adaptation.as_ref() {
            None => Err(MemorySeedError::AdaptationUnavailable),
            Some(adaptation) if !adaptation.memory.pending_erasure_ids().is_empty() => {
                Err(MemorySeedError::ErasePending)
            }
            Some(adaptation) => {
                writer.sweep(&adaptation.memory, now_ms);
                memory_seed::export_from_claims(writer.live_claims())
            }
        };
        self.memory_writer = Some(writer);
        if result.is_ok() {
            self.memory_falsifier.note_export();
        }
        result
    }

    /// Post-turn consolidation: retain stream-level facts from reply text the
    /// output gate already admitted and playback already committed. The
    /// candidate source is `committed_text` only; `event` supplies identifiers
    /// that can only remove viewer-naming sentences. Never fails the turn.
    fn consolidate_committed_reply(
        &mut self,
        event: &EventEnvelope,
        committed_text: &str,
        now_ms: u64,
    ) {
        // #87: what the performer says on its own is not a conversation turn.
        if event.kind == aivtuber_domain::EventKind::TimerTick {
            return;
        }
        let Some(mut writer) = self.memory_writer.take() else {
            return;
        };
        if let Some(adaptation) = self.adaptation.as_mut() {
            writer.sweep(&adaptation.memory, now_ms);
            writer.note_texts_considered();
            let mut viewer_names: Vec<String> = event.actor_id.iter().cloned().collect();
            for key in [
                "display_name",
                "author",
                "author_name",
                "username",
                "user",
                "name",
            ] {
                if let Some(value) = event.payload.get(key).and_then(serde_json::Value::as_str) {
                    viewer_names.push(value.to_owned());
                }
            }
            let extraction =
                memory_writer::extract_candidates(committed_text, &viewer_names, &writer.config());
            writer.note_extraction_rejections(&extraction);
            for candidate in extraction.candidates {
                if writer.quota_full() {
                    writer.reject_quota();
                    continue;
                }
                if writer.is_duplicate(&adaptation.memory, &candidate.claim, now_ms) {
                    writer.reject_duplicate();
                    continue;
                }
                let event_id = writer.next_event_id();
                let Some(permit) = self
                    .security
                    .authorize_performer_output_memory_write(&event_id, &candidate.claim)
                else {
                    writer.reject_gate();
                    continue;
                };
                match adaptation.memory.remember_durable(
                    &permit,
                    &candidate.claim,
                    Some(candidate.kind.topic()),
                    now_ms,
                ) {
                    Ok(entry) => writer.record_written(
                        entry.memory_id,
                        entry.expires_at_ms,
                        candidate.kind,
                        &candidate.claim,
                    ),
                    Err(_) => writer.reject_store(),
                }
            }
        }
        self.memory_writer = Some(writer);
    }

    /// Opt in to live screen perception (#90 S1b). The service starts
    /// **disabled**; only the authenticated `perception.control` capability can
    /// turn it on (`apply_perception_control`). Its observations are untrusted
    /// content-plane data; the service holds no scheduler or output handle.
    pub fn with_perception(mut self, service: perception_service::PerceptionService) -> Self {
        self.perception = Some(service);
        self
    }

    /// Opt in to the scene context (#88 S1): the ranked, closed-vocabulary
    /// snapshot of OBS state and perception. Off by default. Without it no
    /// request carries a scene, so a runtime without a source is byte-identical
    /// to one built before scene context existed.
    pub fn with_scene_context(mut self, service: scene_context::SceneContextService) -> Self {
        self.scene = Some(service);
        self
    }

    pub fn scene_context(&self) -> Option<&scene_context::SceneContextService> {
        self.scene.as_ref()
    }

    pub fn scene_context_mut(&mut self) -> Option<&mut scene_context::SceneContextService> {
        self.scene.as_mut()
    }

    /// Advance the scene context one loop tick. Never blocks: the OBS read is on
    /// the service's own thread.
    pub fn service_scene_context(&mut self, now_ms: u64) {
        let Some(scene) = self.scene.as_mut() else {
            return;
        };
        let perception = self
            .perception
            .as_mut()
            .map(perception_service::PerceptionService::tracker_and_alias);
        scene.service(now_ms, perception);
    }

    /// The scene snapshot a request at `now_ms` carries, or `None` when no
    /// scene source is composed or none has spoken.
    fn scene_snapshot_for_request(&mut self, now_ms: u64) -> Option<SceneContextSnapshot> {
        let scene = self.scene.as_mut()?;
        let perception = self
            .perception
            .as_mut()
            .map(perception_service::PerceptionService::tracker_and_alias);
        scene.snapshot_for_request(now_ms, perception)
    }

    pub fn perception(&self) -> Option<&perception_service::PerceptionService> {
        self.perception.as_ref()
    }

    pub fn perception_mut(&mut self) -> Option<&mut perception_service::PerceptionService> {
        self.perception.as_mut()
    }

    /// The yield-gate inputs for perception: whether Thinking is generating, or
    /// speech (performer playback, a speaking user, a spoken final waiting for
    /// its answer) is active. Read-only and text-free.
    pub fn perception_gates(&self) -> aivtuber_runtime::PerceptionGates {
        let thinking_generating = self
            .generation_execution_snapshot()
            .is_some_and(|snapshot| !snapshot.is_quiescent());
        let speech_active = !self.performer.scheduler().items().is_empty()
            || self
                .speech_observations
                .as_ref()
                .is_some_and(SpeechObservationTracker::any_speaker_active)
            || self
                .speech_transcripts
                .as_ref()
                .is_some_and(|tracker| tracker.pending_finals() > 0);
        aivtuber_runtime::PerceptionGates {
            thinking_generating,
            speech_active,
        }
    }

    /// Advance perception one loop tick: drain a finished capture into the
    /// tracker and start the next one if cadence, yield gate and latency probe
    /// allow. Never blocks (the slow work is on the service's worker thread).
    /// Call it after the operator control drain so a disable is already in
    /// effect before the next capture is considered.
    pub fn service_perception(&mut self, now_ms: u64) {
        if self.perception.is_none() {
            return;
        }
        let gates = self.perception_gates();
        if let Some(service) = self.perception.as_mut() {
            service.service(now_ms, gates);
        }
    }

    /// Apply an operator enable/disable of perception. `authority` must be the
    /// authenticated local ingress capability; anything else is refused and
    /// counted. Returns a short, text-free outcome for the operator.
    pub fn apply_perception_control(
        &mut self,
        authority: &AuthenticatedControl,
        enable: bool,
        now_ms: u64,
    ) -> Result<&'static str, &'static str> {
        let Some(service) = self.perception.as_mut() else {
            return Err("perception is not configured");
        };
        let result = if enable {
            service.enable(authority, now_ms)
        } else {
            service.disable(authority)
        };
        match result {
            Ok(()) if enable => Ok("perception enabled (probing)"),
            Ok(()) => Ok("perception disabled"),
            Err(aivtuber_runtime::PerceptionGateError::Unauthorized) => {
                Err("perception control is not authorized")
            }
            Err(aivtuber_runtime::PerceptionGateError::NotAllowlisted) => {
                Err("perception source is not allowlisted")
            }
            Err(aivtuber_runtime::PerceptionGateError::CapacityReached) => {
                Err("perception source capacity reached")
            }
        }
    }

    /// Share the Thinking usage totals with the operator surfaces (#367). The
    /// same meter must wrap the Thinking engine (`MeteredThinking`).
    pub fn with_thinking_meter(mut self, meter: thinking_meter::ThinkingMeter) -> Self {
        self.thinking_meter = Some(meter);
        self
    }

    /// Share the affect judge's counters with the operator surfaces (#426).
    /// The same runner must be on the compiler.
    pub fn with_affect_judge_stats(mut self, stats: aivtuber_generative::AffectJudgeStats) -> Self {
        self.affect_judge_stats = Some(stats);
        self
    }

    /// Judge counters (judged, fallback by reason, latency p50/p95); `None`
    /// when the judge is not composed.
    pub fn affect_judge_status(&self) -> Option<aivtuber_generative::AffectJudgeStatus> {
        self.affect_judge_stats.as_ref().map(|s| s.snapshot())
    }

    /// Session Thinking usage and cap state; `None` when no meter is composed.
    pub fn thinking_usage_status(&self) -> Option<thinking_meter::ThinkingUsageStatus> {
        self.thinking_meter.as_ref().map(|m| m.status())
    }

    /// Compose the operator's character sheet read at start (#404): `load` is
    /// the outcome of reading `AIVTUBER_PERSONA_FILE`. A loaded sheet is put
    /// first in every generated reply's curated context; any other outcome
    /// changes no request but is reported in the status.
    pub fn with_persona(mut self, load: persona::PersonaLoad) -> Self {
        self.persona = Some(load);
        self
    }

    /// The character sheet's identity (loaded or not, length, SHA-256 prefix;
    /// never the text) for the operator status and the support bundle; `None`
    /// unless `AIVTUBER_PERSONA_FILE` was set.
    pub fn persona_status(&self) -> Option<persona::PersonaStatus> {
        self.persona.as_ref().map(persona::PersonaLoad::status)
    }

    /// Opt in to editing the character sheet while running (#401). Only the
    /// authenticated `persona.configure` capability can use it
    /// (`configure_persona`); `editor` names the file a save is written to
    /// (none = in memory only). Without a sheet loaded the live slot starts
    /// empty.
    pub fn with_persona_editor(mut self, editor: persona::PersonaEditor) -> Self {
        self.persona_editor = Some(editor);
        self.persona.get_or_insert(persona::PersonaLoad::Absent);
        self
    }

    /// Whether an authenticated `app.shutdown` asked the app to end (#449).
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Latch the exit request. `authority` must be the authenticated local
    /// operator capability (`performer.stop`); the caller has already run the
    /// emergency stop. Only the composition loop acts on the flag, after the
    /// reply is flushed.
    pub fn request_shutdown(&mut self, authority: &AuthenticatedControl) -> Result<(), String> {
        if !authority.has_capability(aivtuber_domain::Capability::PerformerStop) {
            return Err("shutdown is not authorized".to_owned());
        }
        self.shutdown_requested = true;
        Ok(())
    }

    /// Remember the operation panel's launch URL so an authenticated
    /// `panel.url` can show it again (#388). Called once, after the panel
    /// started.
    pub fn set_panel_url(&mut self, url: String) {
        self.panel_url = Some(aivtuber_adapters::SecretString::new(url));
    }

    /// The panel's launch URL for `panel.url` (#388). `authority` must be the
    /// authenticated local operator capability; the URL (it carries the panel
    /// secret) goes only to the caller, never to a log or an audit record.
    pub fn reveal_panel_url(&self, authority: &AuthenticatedControl) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::PerformerStop) {
            return Err("panel url is not authorized".to_owned());
        }
        self.panel_url
            .as_ref()
            .map(|url| url.expose().to_owned())
            .ok_or_else(|| "the operation panel is not running".to_owned())
    }

    /// Whether the sheet can be edited while running (the operator endpoint
    /// then holds `persona.configure`).
    pub fn persona_editor_composed(&self) -> bool {
        self.persona_editor.is_some()
    }

    /// Live-edit state for the operator surfaces; `None` unless composed.
    pub fn persona_editor_status(&self) -> Option<persona::PersonaEditorStatus> {
        self.persona_editor
            .as_ref()
            .map(persona::PersonaEditor::status)
    }

    /// Apply one character-sheet operator request (#401). `authority` must be
    /// the authenticated local ingress capability. `action` is `persona.set`
    /// (`argument` is the whole new text; empty clears) or `persona.get`.
    ///
    /// A set is validated by the same parser as the start-up file, then
    /// written atomically when a file is configured, and only then becomes
    /// the live sheet: any refusal, or a failed write, leaves the live sheet
    /// unchanged. A reply already being generated keeps the old text; the next
    /// Thinking request carries the new one. Details are fixed phrases plus the
    /// sheet's length and hash prefix, never the text; only `persona.get`
    /// returns it, to the authenticated caller.
    pub fn configure_persona(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<persona::PersonaOutcome, String> {
        if !authority.has_capability(aivtuber_domain::Capability::PersonaConfigure) {
            return Err("character sheet editing is not authorized".to_owned());
        }
        let Some(editor) = self.persona_editor.as_ref() else {
            return Err("character sheet editing is not configured".to_owned());
        };
        match action {
            "persona.get" => {
                let current = self.persona.as_ref().and_then(persona::PersonaLoad::sheet);
                Ok(persona::PersonaOutcome {
                    detail: self
                        .persona_status()
                        .map_or_else(|| "none".to_owned(), |status| status.identity()),
                    text: Some(current.map_or_else(String::new, |s| s.text().to_owned())),
                })
            }
            "persona.set" => {
                let Some(text) = argument else {
                    return Err("persona not changed: no text given".to_owned());
                };
                let sheet = match aivtuber_domain::parse_persona(text.as_bytes()) {
                    Ok(persona) => persona.map(persona::PersonaSheet::new),
                    Err(error) => {
                        return Err(format!("persona not changed: {}", error.reason()));
                    }
                };
                let persisted = if let Some(path) = editor.path() {
                    let normalised = sheet.as_ref().map_or("", persona::PersonaSheet::text);
                    if persona::write_persona_file_atomic(path, normalised).is_err() {
                        return Err(
                            "persona not changed: the persona file could not be written".to_owned()
                        );
                    }
                    true
                } else {
                    false
                };
                let thinking_remote = editor.status().thinking_remote;
                let load = sheet.map_or(persona::PersonaLoad::Absent, persona::PersonaLoad::Loaded);
                let identity = load.status().identity();
                eprintln!(
                    "{}",
                    ready_guide::persona_line(
                        &load,
                        thinking_remote,
                        aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    )
                );
                self.persona = Some(load);
                let kept = if persisted {
                    "saved to the persona file"
                } else {
                    "applied in memory only (no persona file is configured)"
                };
                Ok(persona::PersonaOutcome {
                    detail: format!("persona updated, {kept}: {identity}"),
                    text: None,
                })
            }
            _ => Err("not a character sheet action".to_owned()),
        }
    }

    /// Opt in to switching the local Thinking model at runtime (#357). Only the
    /// authenticated `model.select` capability can use it
    /// (`select_thinking_model`).
    pub fn with_model_switcher(mut self, switcher: model_switch::ModelSwitcher) -> Self {
        self.model_switcher = Some(switcher);
        self
    }

    /// Apply an operator request to switch the Thinking model. `authority`
    /// must be the authenticated local ingress capability; the alias must be
    /// in the allowlist the launcher passed in. Returns a short outcome for
    /// the operator and the audit record. The model load itself runs on a
    /// background thread: this never waits for it.
    pub fn select_thinking_model(
        &mut self,
        authority: &AuthenticatedControl,
        alias: &str,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::ModelSelect) {
            return Err("model selection is not authorized".to_owned());
        }
        let Some(switcher) = self.model_switcher.as_mut() else {
            return Err("model switching is not configured".to_owned());
        };
        switcher.select(alias).map_err(|error| error.to_string())
    }

    /// The model selector's state for the operator surfaces; `None` when no
    /// switcher is composed.
    pub fn thinking_model_status(&self) -> Option<model_switch::ThinkingModelStatus> {
        self.model_switcher.as_ref().map(|s| s.status())
    }

    /// Collect a finished model warm-up (#357). Never blocks.
    pub fn service_model_switch(&mut self) {
        if let Some(switcher) = self.model_switcher.as_mut() {
            switcher.poll();
        }
    }

    /// Opt in to changing the voice at runtime (#360). Only the authenticated
    /// `voice.configure` capability can use it (`configure_voice`).
    pub fn with_voice_controller(mut self, controller: voice_control::VoiceController) -> Self {
        self.voice = Some(controller);
        self
    }

    /// Whether voice settings are composed (the operator endpoint then holds
    /// `voice.configure`).
    pub fn voice_configured(&self) -> bool {
        self.voice.is_some()
    }

    /// The Aivis Cloud origin the model search is proxied to; `None` when the
    /// voice settings are not composed.
    pub fn voice_search_endpoint(&self) -> Option<String> {
        self.voice.as_ref().map(|v| v.endpoint().to_owned())
    }

    /// The voice state for the operator surfaces; `None` when not composed.
    pub fn voice_status(&self) -> Option<voice_control::VoiceStatus> {
        self.voice.as_ref().map(|v| v.status())
    }

    /// Apply one voice operator request. `authority` must be the authenticated
    /// local ingress capability. `action` is one of `voice.apply` (the
    /// argument is a settings patch), `voice.key.set` (the argument is the
    /// key: never echoed or audited), `voice.key.remove`, `voice.key.check`
    /// and `voice.preview`. Returns a short outcome for the operator and the
    /// audit record, which never holds the key. Nothing here blocks: the key
    /// check runs on its own thread and the preview line is submitted by
    /// [`Self::service_voice`].
    pub fn configure_voice(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::VoiceConfigure) {
            return Err("voice settings are not authorized".to_owned());
        }
        let Some(voice) = self.voice.as_mut() else {
            return Err("voice settings are not configured".to_owned());
        };
        match action {
            "voice.apply" => voice.apply(argument.unwrap_or("")),
            "voice.key.set" => voice.set_key(argument.unwrap_or("")),
            "voice.key.remove" => voice.remove_key(),
            "voice.key.check" => voice.check_key(),
            "voice.preview" => voice.request_preview(),
            _ => Err("not a voice action".to_owned()),
        }
    }

    /// Collect a finished key check and submit a waiting preview line (#360).
    /// The preview is an ordinary panel chat line whose reply the Thinking
    /// override replaces with the fixed sentence, so it takes the normal
    /// generation, TTS, scheduling and audit path (mute and stop apply). Never
    /// blocks. Returns `true` when a key-check result was applied: the state
    /// the panel published earlier this turn is then stale, and the caller
    /// asks the panel to publish again (#373).
    pub fn service_voice(&mut self, now_ms: u64, sequence_seed: &mut u64) -> bool {
        let Some(voice) = self.voice.as_mut() else {
            return false;
        };
        let applied = voice.poll();
        let Some((event_id, sequence)) = voice.take_pending_preview() else {
            return applied;
        };
        let raw = panel::chat_envelope(&event_id, sequence, aivtuber_domain::VOICE_PREVIEW_TEXT);
        *sequence_seed = sequence_seed.saturating_add(1);
        let admitted = match self.process_content_bytes(&raw, now_ms, *sequence_seed) {
            Ok(outcome) => {
                eprintln!(
                    "voice: preview {event_id} admission={:?}",
                    outcome.admission
                );
                outcome.admission == aivtuber_runtime::ContentAdmitDecision::Queued
            }
            Err(error) => {
                eprintln!("voice: preview {event_id} rejected: {error}");
                false
            }
        };
        if !admitted && let Some(voice) = self.voice.as_mut() {
            voice.cancel_preview();
        }
        applied
    }

    /// Opt in to connecting stream chat at runtime (#361). Only the
    /// authenticated `ingress.control` capability can use it
    /// (`configure_ingress`).
    pub fn with_chat_ingress(mut self, controller: chat_ingress::ChatIngressController) -> Self {
        self.chat_ingress = Some(controller);
        self
    }

    /// Whether chat connections are composed (the operator endpoint then holds
    /// `ingress.control`).
    pub fn chat_ingress_configured(&self) -> bool {
        self.chat_ingress.is_some()
    }

    /// The chat connection state for the operator surfaces; `None` when not
    /// composed.
    pub fn chat_ingress_status(&self) -> Option<chat_ingress::ChatIngressStatus> {
        self.chat_ingress.as_ref().map(|c| c.status())
    }

    /// Apply one chat connection operator request. `authority` must be the
    /// authenticated local ingress capability. `action` is `ingress.connect`
    /// (the argument is `{"platform","target"}`), `ingress.disconnect`
    /// (`{"platform"}`), `ingress.key.set` (the argument is the YouTube key:
    /// never echoed or audited) or `ingress.key.remove`. The detail returned
    /// (and audited) names a platform and a validated target, never the key.
    /// Nothing here blocks: the sidecar is started (or ended) at once and read
    /// by [`Self::service_chat_ingress`].
    pub fn configure_ingress(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::IngressControl) {
            return Err("chat connections are not authorized".to_owned());
        }
        let Some(ingress) = self.chat_ingress.as_mut() else {
            return Err("chat connections are not configured".to_owned());
        };
        match action {
            "ingress.connect" => {
                let (platform, target) = chat_ingress::parse_connect_argument(argument)?;
                ingress.connect(platform, &target)
            }
            "ingress.disconnect" => {
                ingress.disconnect(chat_ingress::parse_disconnect_argument(argument)?)
            }
            "ingress.key.set" => ingress.set_key(argument.unwrap_or("")),
            "ingress.key.remove" => ingress.remove_key(),
            _ => Err("not an ingress action".to_owned()),
        }
    }

    /// Admit what the chat sidecars wrote, a few records per platform per
    /// turn, through the same path as a stdin line (#361): the app's ingress
    /// ceiling, envelope validation and rate limits all apply. Never blocks;
    /// a flood is dropped (and counted) in the bounded queue upstream.
    pub fn service_chat_ingress(&mut self, now_ms: u64, sequence_seed: &mut u64) {
        let Some(ingress) = self.chat_ingress.as_mut() else {
            return;
        };
        let records = ingress.take_records(chat_ingress::CHAT_RECORDS_PER_TICK);
        for record in records {
            *sequence_seed = sequence_seed.saturating_add(1);
            let accepted = match self.process_content_bytes(&record.raw, now_ms, *sequence_seed) {
                Ok(outcome) => outcome.admission == aivtuber_runtime::ContentAdmitDecision::Queued,
                Err(error) => {
                    eprintln!(
                        "chat_ingress: {} record rejected: {error}",
                        record.platform.name()
                    );
                    false
                }
            };
            if let Some(ingress) = self.chat_ingress.as_mut() {
                ingress.note_outcome(record.platform, accepted);
            }
        }
    }

    /// Opt in to choosing the VRM avatar at runtime (#362). Only the
    /// authenticated `avatar.configure` capability can use it
    /// (`configure_avatar`).
    pub fn with_avatar_controller(mut self, controller: avatar_control::AvatarController) -> Self {
        self.avatar_store = Some(controller);
        self
    }

    /// Whether the avatar store is composed (the operator endpoint then holds
    /// `avatar.configure`).
    pub fn avatar_configured(&self) -> bool {
        self.avatar_store.is_some()
    }

    /// The directory the panel stages model uploads in; `None` when the avatar
    /// store is not composed.
    pub fn avatar_staging_dir(&self) -> Option<std::path::PathBuf> {
        self.avatar_store.as_ref().map(|a| a.staging_dir())
    }

    /// The avatar choice for the operator surfaces; `None` when not composed.
    pub fn avatar_status(&self) -> Option<avatar_control::AvatarStatus> {
        self.avatar_store.as_ref().map(|a| a.status())
    }

    /// One line naming the active model and its credit, for the startup
    /// disclosure; `None` when not composed.
    pub fn avatar_summary(&self) -> Option<String> {
        self.avatar_store.as_ref().map(|a| a.active_summary())
    }

    /// Apply one avatar operator request. `authority` must be the authenticated
    /// local ingress capability. `action` is `avatar.upload` (the argument is
    /// the hash of a file the panel staged), `avatar.confirm` (the pending
    /// candidate's hash), `avatar.select` (`sample` or a stored model's hash),
    /// `avatar.delete` or `avatar.discard`. The detail returned (and audited)
    /// holds the model name (bounded untrusted text) and the content hash,
    /// never file bytes. Nothing here blocks beyond a rename and a header read.
    pub fn configure_avatar(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::AvatarConfigure) {
            return Err("avatar settings are not authorized".to_owned());
        }
        let Some(avatar) = self.avatar_store.as_mut() else {
            return Err("avatar settings are not configured".to_owned());
        };
        let argument = argument.unwrap_or("");
        match action {
            "avatar.upload" => avatar.upload(argument),
            "avatar.confirm" => avatar.confirm(argument),
            "avatar.select" => avatar.select(argument),
            "avatar.delete" => avatar.delete(argument),
            "avatar.discard" => avatar.discard(),
            _ => Err("not an avatar action".to_owned()),
        }
    }

    /// End the chat sidecars (and wait for them) when the app ends.
    pub fn shutdown_chat_ingress(&mut self) {
        if let Some(ingress) = self.chat_ingress.as_mut() {
            ingress.shutdown();
        }
    }

    /// Stop the Aivis bridge (and with it all egress) when the app ends.
    pub fn shutdown_voice(&mut self) {
        if let Some(voice) = self.voice.as_mut() {
            voice.shutdown();
        }
    }

    /// Opt in to speech observation tracking (#92). Observations are untrusted
    /// content-plane data and reach no scheduler or output; #81 reads the
    /// tracker's per-speaker state. Absent by default, so behaviour is unchanged.
    pub fn with_speech_observations(mut self, tracker: SpeechObservationTracker) -> Self {
        self.speech_observations = Some(tracker);
        self
    }

    pub fn speech_observations_mut(&mut self) -> Option<&mut SpeechObservationTracker> {
        self.speech_observations.as_mut()
    }

    /// Opt in to speech transcript tracking (#92 S3b). Transcripts are
    /// untrusted content: a partial only ever becomes a floor-hold
    /// observation, and only a final can be taken (`take_finals`) as input for
    /// the normalizer. Nothing is persisted and no permit is minted here.
    /// Absent by default, so behaviour is unchanged.
    pub fn with_speech_transcripts(mut self, tracker: SpeechTranscriptTracker) -> Self {
        self.speech_transcripts = Some(tracker);
        self
    }

    pub fn speech_transcripts_mut(&mut self) -> Option<&mut SpeechTranscriptTracker> {
        self.speech_transcripts.as_mut()
    }

    /// Both speech trackers at once, for a sidecar poll that feeds them.
    pub fn speech_trackers_mut(
        &mut self,
    ) -> (
        Option<&mut SpeechObservationTracker>,
        Option<&mut SpeechTranscriptTracker>,
    ) {
        (
            self.speech_observations.as_mut(),
            self.speech_transcripts.as_mut(),
        )
    }

    /// Opt in to routing spoken finals into the conversation input (#92 S3c).
    /// Off by default. Needs a transcript tracker to have anything to route.
    pub fn with_speech_conversation(
        mut self,
        config: SpeechConversationConfig,
    ) -> Result<Self, AppError> {
        let config = config
            .validate()
            .map_err(|message| AppError::Routing(message.to_owned()))?;
        self.speech_conversation = Some(speech_conversation::SpeechConversation::new(config));
        Ok(self)
    }

    /// Opt in to the deterministic conversational-floor gate (#81 S1) for
    /// spoken finals. Off by default; without it (or without a conversation
    /// route) finals are routed exactly as before. With it, a final waits while
    /// the user holds the floor, bounded in count and age. The gate touches no
    /// scheduler, output or operator path.
    pub fn with_speech_floor(
        mut self,
        config: aivtuber_domain::FloorConfig,
    ) -> Result<Self, AppError> {
        self.speech_floor = Some(
            speech_floor::SpeechFloor::new(config)
                .map_err(|error| AppError::Routing(error.to_string()))?,
        );
        Ok(self)
    }

    /// Opt in to barge-in (#81 S2, ADR-0005 S-3): a *confirmed* floor `Yield`
    /// stops the performer's spoken output with a fade of at most
    /// `config.fade_ms` (at most 150) and returns the avatar to neutral. It
    /// needs the floor gate (call `with_speech_floor` first) and is off by
    /// default. See [`Self::service_barge_in`].
    pub fn with_barge_in(mut self, config: BargeInConfig) -> Result<Self, AppError> {
        if self.speech_floor.is_none() {
            return Err(AppError::Routing(
                "barge-in requires the speech floor".to_owned(),
            ));
        }
        self.speech_barge_in =
            Some(speech_barge_in::BargeIn::new(config).map_err(AppError::Routing)?);
        Ok(self)
    }

    /// Own the speech sidecar and its on/off state (#400). The sidecar is
    /// polled by [`Self::service_speech_input`]; the operator endpoint holds
    /// `speech_input.control` only when the controller is composed (its
    /// settings file is named).
    pub fn with_speech_input(mut self, controller: speech_input::SpeechInputController) -> Self {
        self.speech_input = Some(controller);
        self
    }

    /// Whether voice input can be switched while running (the operator
    /// endpoint then holds `speech_input.control`).
    pub fn speech_input_composed(&self) -> bool {
        self.speech_input
            .as_ref()
            .is_some_and(speech_input::SpeechInputController::composed)
    }

    /// Whether voice input is running (#440): the panel publishes its state
    /// faster then, so the input meter moves.
    pub fn speech_input_is_on(&self) -> bool {
        self.speech_input
            .as_ref()
            .is_some_and(speech_input::SpeechInputController::is_on)
    }

    /// The voice-input state for the operator surfaces; `None` when no speech
    /// sidecar is configured.
    pub fn speech_input_status(&self) -> Option<speech_input::SpeechInputStatus> {
        self.speech_input.as_ref().map(|input| {
            let mut status = input.status();
            status.dropped_during_playback = self.speech_playback.dropped_during_playback();
            status
        })
    }

    /// Whether the performer is speaking now (#440): an audio item is playing,
    /// or a phrased reply that already spoke is between two phrases. Display
    /// data for the operator surfaces; no decision reads it.
    pub fn performer_speaking(&self) -> bool {
        self.performer.scheduler().playing_items().any(|item| {
            item.plan
                .channels
                .contains(&aivtuber_scheduler::BlendChannel::Audio)
        }) || self
            .generative
            .as_ref()
            .is_some_and(|generative| generative.phrase.has_active_spoken_run())
    }

    /// Whether the performer listens while it speaks (#396). Without a
    /// composed voice input (the app run directly) it always does, as before.
    fn speech_full_duplex(&self) -> bool {
        self.speech_input
            .as_ref()
            .is_none_or(|input| input.duplex() == aivtuber_domain::SpeechDuplex::Full)
    }

    /// Whether the playback history is kept: only a composed voice input has a
    /// duplex choice.
    fn speech_playback_tracked(&self) -> bool {
        self.speech_input
            .as_ref()
            .is_some_and(speech_input::SpeechInputController::composed)
    }

    /// Counters of the running speech sidecar, if there is one.
    pub fn speech_sidecar_stats(&self) -> Option<SpeechSidecarStats> {
        self.speech_input.as_ref().and_then(|input| input.stats())
    }

    /// Drain what the speech sidecar wrote into the trackers and note the
    /// feed heartbeat. Never blocks; does nothing without a running sidecar.
    pub fn service_speech_input(&mut self, now_ms: u64) {
        let Some(input) = self.speech_input.as_mut() else {
            return;
        };
        if let Some(observations) = self.speech_observations.as_mut() {
            input.poll(observations, self.speech_transcripts.as_mut(), now_ms);
        }
        if let Some(beat_ms) = input.feed_heartbeat_ms() {
            self.note_speech_feed_heartbeat(beat_ms);
        }
    }

    /// End the speech sidecar (and its STT worker) when the app ends. Changes
    /// neither the state file nor the reported state.
    pub fn shutdown_speech_input(&mut self) {
        if let Some(input) = self.speech_input.as_mut() {
            input.shutdown();
        }
    }

    /// Forget everything the old speech feed said (#400): both trackers'
    /// speakers, partial transcripts and pending finals, the feed heartbeat
    /// and the barge-in run that was already handled. After this nobody holds
    /// the floor or counts as speaking, no barge-in can be confirmed from
    /// stale edges (it needs a fresh heartbeat), and a final the floor was
    /// still deferring is discarded (counted `dropped_by_operator`): nothing
    /// heard before the switch is answered or sent to a model afterwards.
    /// It fabricates no end-of-speech event.
    pub fn clear_speech_state(&mut self) {
        if let Some(tracker) = self.speech_observations.as_mut() {
            tracker.clear();
        }
        if let Some(tracker) = self.speech_transcripts.as_mut() {
            tracker.clear();
        }
        if let Some(floor) = self.speech_floor.as_mut() {
            floor.clear();
        }
        self.speech_feed_heartbeat_ms = None;
        if let Some(barge_in) = self.speech_barge_in.as_mut() {
            barge_in.reset_handled();
        }
    }

    /// Apply one voice-input operator request. `authority` must be the
    /// authenticated local ingress capability. `action` is
    /// `speech_input.enable` or `speech_input.disable`. Disable always
    /// succeeds and is immediate; enable starts a fresh sidecar and a spawn
    /// failure is an `Err` with the state unchanged. The detail (and the audit
    /// record) is a fixed phrase, never a path or a device name.
    pub fn configure_speech_input(
        &mut self,
        authority: &AuthenticatedControl,
        action: &str,
        argument: Option<&str>,
    ) -> Result<String, String> {
        if !authority.has_capability(aivtuber_domain::Capability::SpeechInputControl) {
            return Err("voice input is not authorized".to_owned());
        }
        if !self.speech_input_composed() {
            return Err("voice input is not configured".to_owned());
        }
        let device_chosen = match action {
            "speech_input.device" => argument.is_some_and(|name| !name.is_empty()),
            _ => self
                .speech_input
                .as_ref()
                .is_some_and(|input| input.selected_device().is_some()),
        };
        let announce = |on: bool, remote: bool| {
            eprintln!(
                "{}",
                ready_guide::voice_input_text(
                    on,
                    remote,
                    aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    device_chosen,
                )
            );
        };
        match action {
            "speech_input.enable" => {
                if self.speech_input.as_ref().is_some_and(|i| i.is_on()) {
                    return Ok("voice input is already on".to_owned());
                }
                // The new instance starts from empty state (a failed one left
                // stale edges and segment numbers behind).
                self.clear_speech_state();
                let input = self.speech_input.as_mut().expect("composed");
                input.enable(announce).map_err(|error| error.to_string())
            }
            "speech_input.disable" => {
                let detail = self
                    .speech_input
                    .as_mut()
                    .expect("composed")
                    .disable(announce);
                self.clear_speech_state();
                Ok(detail)
            }
            "speech_input.device" => self.set_speech_input_device(argument, announce),
            "speech_input.duplex" => self.set_speech_input_duplex(argument),
            _ => Err("not a voice input action".to_owned()),
        }
    }

    /// `speech_input.device`: the argument is a capture device name, empty for
    /// the system default. The controller accepts only a name the sidecar
    /// listed (or any acceptable name before it listed any); the answer and
    /// the audit record never carry the name. A separate function so the
    /// request path through [`Self::configure_speech_input`] keeps a small
    /// frame (the app's main thread is a Windows 1 MiB stack).
    fn set_speech_input_device(
        &mut self,
        argument: Option<&str>,
        announce: impl FnOnce(bool, bool),
    ) -> Result<String, String> {
        let device = match argument {
            None | Some("") => None,
            Some(name) => Some(name.to_owned()),
        };
        // The new instance starts from empty state, like enable.
        self.clear_speech_state();
        let input = self.speech_input.as_mut().expect("composed");
        input
            .set_device(device, announce)
            .map_err(|error| error.to_string())
    }

    /// `speech_input.duplex`: `half` or `full`, applied at once and saved.
    fn set_speech_input_duplex(&mut self, argument: Option<&str>) -> Result<String, String> {
        let duplex = argument
            .and_then(aivtuber_domain::SpeechDuplex::parse)
            .ok_or_else(|| "expected half or full".to_owned())?;
        let detail = self
            .speech_input
            .as_mut()
            .expect("composed")
            .set_duplex(duplex);
        if duplex == aivtuber_domain::SpeechDuplex::Half {
            // Nothing heard before the switch can still confirm a barge-in.
            if let Some(barge_in) = self.speech_barge_in.as_mut() {
                barge_in.reset_handled();
            }
        }
        Ok(detail)
    }

    pub fn speech_barge_in_metrics(&self) -> Option<BargeInMetrics> {
        self.speech_barge_in.as_ref().map(|b| b.metrics())
    }

    /// Bounded recent barge-in records (oldest first); empty when not enabled.
    pub fn speech_barge_in_records(&self) -> Vec<BargeInRecord> {
        self.speech_barge_in
            .as_ref()
            .map(|b| b.recent())
            .unwrap_or_default()
    }

    /// The VAD sidecar delivered a feed-liveness beat at `at_ms` (the
    /// receipt time on the runtime timeline). Never moves backwards. Without
    /// any beat the floor's stale-feed rule stays dormant (the S1 behaviour);
    /// once beats have arrived, their silence makes the feed stale (ADR-0005
    /// D6): costly decisions wait, and after `vad_loss_ms` the floor degrades
    /// to the text-only path instead of holding replies forever.
    pub fn note_speech_feed_heartbeat(&mut self, at_ms: u64) {
        self.speech_feed_heartbeat_ms = Some(
            self.speech_feed_heartbeat_ms
                .map_or(at_ms, |previous| previous.max(at_ms)),
        );
    }

    /// Execute a confirmed barge-in, if there is one at `at_ms` (ADR-0005
    /// S-3). Call it each loop tick after the sidecar was polled.
    ///
    /// The trigger is the floor policy's `Yield` / `user_barge_in_confirmed`
    /// computed from raw VAD activity on a fresh feed, while an audio
    /// performance is playing. Tentative speech, a stale or dead feed, a
    /// partial transcript and model output never reach the effect. Once per
    /// user speech run it then:
    /// 1. cancels the audio performances the user's speech may preempt through
    ///    the scheduler (same arbitration as `schedule`, no wait for an
    ///    interrupt point, because stopping publishes nothing),
    /// 2. drops their pending audio/avatar commands,
    /// 3. asks the audio output to stop with the configured fade (backends
    ///    that cannot fade hard-stop and say so; a backend without stop is
    ///    reported as unsupported), and
    /// 4. sends the avatar a `Reset` through the normal avatar output.
    ///
    /// It needs the feed to have reported liveness at least once
    /// ([`Self::note_speech_feed_heartbeat`]); without that it does nothing,
    /// which is the S1 behaviour (a confirmed yield was recorded only).
    ///
    /// Operator stop is a separate path and is unaffected.
    pub fn service_barge_in(&mut self, at_ms: u64) -> Option<BargeInRecord> {
        let fade_ms = self.speech_barge_in.as_ref()?.config().fade_ms;
        self.speech_floor.as_ref()?;
        // Half duplex (#396): the performer's own audio can come back through
        // the microphone, so the user's voice does not stop it.
        if !self.speech_full_duplex() {
            return None;
        }
        self.pump_speech_lifecycle();
        // A phrased reply that already spoke is still "speaking" in the gap
        // between two phrases, when nothing is audible but more would follow.
        let phrase_pending = self
            .generative
            .as_ref()
            .is_some_and(|generative| generative.phrase.has_active_spoken_run());
        let speaking =
            self.speech_floor.as_ref()?.own_tracker().own() == aivtuber_domain::OwnSpeech::Speaking;
        if !phrase_pending && !speaking {
            return None;
        }
        // Stopping speech is the costly-to-reverse decision (ADR-0005 D6): it
        // needs a feed that reports liveness. A sidecar that never beats keeps
        // the S1 behaviour, where a confirmed yield was recorded, not executed.
        let beat_ms = self.speech_feed_heartbeat_ms?;
        let feed_age_ms = Some(at_ms.saturating_sub(beat_ms));
        let started = Instant::now();
        // #83 S3b: only the trusted scene accessor reaches the floor.
        let scene = self.floor_scene(at_ms);
        let reply_inbox = self.chat_burst.is_some();
        let speakers: Vec<String> = self
            .speech_observations
            .as_ref()?
            .speaker_ids()
            .map(str::to_owned)
            .collect();
        let mut confirmed = None;
        for speaker in speakers {
            let user = speech_floor::floor_evidence(
                self.speech_observations.as_ref(),
                &speaker,
                at_ms,
                feed_age_ms,
            );
            let run_start_ms = user.and_then(|evidence| evidence.last_edge_ms);
            let input = aivtuber_domain::FloorInput {
                now_ms: at_ms,
                user,
                hold: self
                    .speech_transcripts
                    .as_ref()
                    .map_or(aivtuber_domain::TranscriptHold::None, |transcripts| {
                        speech_floor::transcript_hold(transcripts, &speaker, at_ms)
                    }),
                own: aivtuber_domain::OwnSpeech::Speaking,
                reply_waiting: false,
                scene,
                reply_inbox,
            };
            let handled = self
                .speech_barge_in
                .as_ref()?
                .is_handled(&speaker, run_start_ms);
            if let Some(decision) = self
                .speech_floor
                .as_mut()?
                .confirm_barge_in(&input, handled)
            {
                confirmed = Some((decision, speaker, run_start_ms));
                break;
            }
        }
        let (decision, speaker, run_start_ms) = confirmed?;

        let stop = self.performer.scheduler_mut().stop_speaking_for(
            priority_for_event(aivtuber_domain::EventKind::SpeechInput),
            at_ms,
        );
        // Stopping what is queued is not enough for a phrased reply: its later
        // phrases are still being synthesized, and the cancellation is what
        // keeps them from ever being committed (issue #83).
        let cancelled_phrased = self.cancel_phrased_replies(PhraseCause::BargeIn);
        let mut record = BargeInRecord {
            decision_seq: decision.seq,
            decided_at_ms: decision.at_ms,
            effect: BargeInEffect::NothingToStop,
            cancelled_playing: stop.playing.len(),
            cancelled_queued: stop.queued.len(),
            fade_ms,
            audio: None,
            audio_error: false,
            avatar_reset: None,
            decision_to_stop_micros: 0,
        };
        if !stop.is_empty() || cancelled_phrased > 0 {
            record.effect = BargeInEffect::Stopped;
            // Commands of the cancelled generations must not dispatch later.
            self.purge_cancelled_pending();
            match self.stop_started_audio(fade_ms) {
                AudioStopReport::Outcome(outcome) => record.audio = Some(outcome),
                AudioStopReport::Failed(_) | AudioStopReport::Unconfirmed => {
                    record.audio_error = true;
                }
            }
            if let Some(generation) = stop.playing.first().map(|plan| plan.generation) {
                record.avatar_reset = Some(self.reset_avatar_neutral(generation, at_ms));
            }
        }
        record.decision_to_stop_micros = elapsed_us(started);
        let barge_in = self.speech_barge_in.as_mut()?;
        barge_in.mark_handled(&speaker, run_start_ms);
        barge_in.record(record.clone());
        Some(record)
    }

    /// Reading-lexicon counters (issue #222): replies projected, readings
    /// applied, Latin words left to the TTS. All zero when generation is off.
    pub fn speech_projection_metrics(&self) -> SpeechProjectionMetrics {
        self.generative
            .as_ref()
            .map(|generative| generative.speech_projection)
            .unwrap_or_default()
    }

    /// Counters of phrase-level commits (issue #83); all zero in buffered mode.
    pub fn phrase_metrics(&self) -> PhraseMetrics {
        self.generative
            .as_ref()
            .map(|generative| generative.phrase.metrics)
            .unwrap_or_default()
    }

    /// Bounded recent phrased replies (oldest first): the commit horizon and
    /// commit order of each, and how it ended. Empty in buffered mode.
    pub fn recent_phrase_responses(&self) -> Vec<PhraseResponseRecord> {
        self.generative
            .as_ref()
            .map(|generative| generative.phrase.recent())
            .unwrap_or_default()
    }

    /// Floor gate counters; `None` when not enabled.
    pub fn speech_floor_metrics(&self) -> Option<SpeechFloorMetrics> {
        self.speech_floor.as_ref().map(|f| f.metrics())
    }

    /// Bounded recent floor decisions (oldest first); empty when not enabled.
    pub fn speech_floor_decisions(&self) -> Vec<aivtuber_domain::FloorDecision> {
        self.speech_floor
            .as_ref()
            .map(|f| f.recent())
            .unwrap_or_default()
    }

    /// Counters for the speech-to-conversation route; `None` when not enabled.
    pub fn speech_conversation_metrics(&self) -> Option<SpeechConversationMetrics> {
        self.speech_conversation.as_ref().map(|c| c.metrics())
    }

    /// Take at most `max_finals_per_tick` finals from the transcript tracker
    /// and route each through [`Self::process_content_bytes`], the same
    /// normalizer path as chat. Returns one report per final that became an
    /// envelope. Without a conversation route the finals are drained and
    /// discarded so the tracker's bounded queue never wedges.
    pub fn route_spoken_finals(&mut self, at_ms: u64, seed: u64) -> Vec<SpokenFinalReport> {
        self.pump_speech_lifecycle();
        let half_duplex = self.speech_playback_tracked() && !self.speech_full_duplex();
        let Some(tracker) = self.speech_transcripts.as_mut() else {
            return Vec::new();
        };
        let Some(conversation) = self.speech_conversation.as_mut() else {
            let _ = tracker.take_finals(16);
            return Vec::new();
        };
        let max_finals = conversation.config().max_finals_per_tick;
        let mut finals = tracker.take_finals(max_finals);
        // Half duplex (#396): a segment that started while the performer's
        // audio played, or in the tail after it, is its own echo. It is
        // dropped here, before the floor, so it is never held, routed or sent
        // to a model. The start is the segment's end less the audio it covers
        // (pre-roll included, so slightly early, which errs toward dropping).
        if half_duplex {
            let before = finals.len();
            let playback = &self.speech_playback;
            finals.retain(|spoken| {
                !playback.covers(
                    spoken
                        .observed_at_ms
                        .saturating_sub(spoken.covered_audio_ms),
                )
            });
            self.speech_playback.count_dropped(before - finals.len());
        }
        if let Some(floor) = self.speech_floor.as_mut() {
            // Evidence enters only here, from the trackers and the scheduler's
            // own playing state. Missing evidence degrades to the text-only
            // path inside the policy, i.e. to the unchanged behaviour.
            let own = floor.own_tracker().own();
            let scene = self
                .scene
                .as_ref()
                .map_or(TrustedInterruptibility::unknown(), |scene| {
                    scene.scheduling_context(at_ms)
                });
            let reply_inbox = self.chat_burst.is_some();
            let observations = self.speech_observations.as_ref();
            let transcripts: &SpeechTranscriptTracker = tracker;
            // Feed liveness (#81 S2): `None` until the sidecar reports beats.
            let feed_age_ms = self
                .speech_feed_heartbeat_ms
                .map(|beat| at_ms.saturating_sub(beat));
            finals = floor.admit(at_ms, finals, max_finals, |speaker| {
                aivtuber_domain::FloorInput {
                    now_ms: at_ms,
                    user: speech_floor::floor_evidence(observations, speaker, at_ms, feed_age_ms),
                    hold: speech_floor::transcript_hold(transcripts, speaker, at_ms),
                    own,
                    reply_waiting: true,
                    scene,
                    reply_inbox,
                }
            });
        }
        let events: Vec<(EventEnvelope, u64)> = finals
            .iter()
            .filter_map(|spoken| {
                conversation
                    .envelope_for(spoken)
                    .map(|event| (event, spoken.observed_at_ms))
            })
            .collect();
        let mut reports = Vec::with_capacity(events.len());
        for (index, (event, observed_at_ms)) in events.into_iter().enumerate() {
            let route_started = Instant::now();
            let result = serde_json::to_vec(&event)
                .map_err(|error| AppError::Routing(error.to_string()))
                .and_then(|raw| {
                    self.process_content_bytes(&raw, at_ms, seed.wrapping_add(index as u64))
                });
            if let Some(conversation) = self.speech_conversation.as_mut() {
                let metrics = conversation.metrics_mut();
                match &result {
                    Ok(outcome) if outcome.admission == ContentAdmitDecision::Queued => {
                        metrics.admitted += 1;
                    }
                    Ok(_) => metrics.dropped_by_runtime += 1,
                    Err(_) => metrics.route_errors += 1,
                }
            }
            reports.push(SpokenFinalReport {
                event,
                result,
                final_age_ms: at_ms.saturating_sub(observed_at_ms),
                route_micros: route_started
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64,
            });
        }
        reports
    }

    pub fn with_comparison_mode(mut self, mode: ComparisonMode) -> Self {
        self.comparison_mode = mode;
        self
    }

    pub fn with_telemetry_retention(mut self, retention: TelemetryRetentionConfig) -> Self {
        self.telemetry.set_retention(retention);
        self
    }

    /// Configure causal trace retention from the unified runtime policy (#65,
    /// #51): trace retention never invents a separate policy contract.
    pub fn with_causal_trace_retention(mut self, retention: CausalTraceRetentionConfig) -> Self {
        self.causal_traces.set_retention(retention);
        self
    }

    /// Retained privacy-safe causal traces for diagnostics/benchmarks.
    pub fn causal_traces(&self) -> &CausalTraceCollector {
        &self.causal_traces
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
        self.remember_durable_memory_superseding(event, authority, claim, topic, now_ms, &[])
    }

    /// A durable write that also carries the caller's update evidence (#105):
    /// `supersedes` names the source event ids of retained durable memories
    /// this claim replaces. It is the same non-serializable permit path as a
    /// plain durable write — the security gate decides first, and an unknown,
    /// ambiguous, expired, or unauthorized target refuses the whole write, so
    /// declaring supersession can never widen the gate or store a link the
    /// claim itself would not have earned.
    pub fn remember_durable_memory_superseding(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
    ) -> Result<MemoryEntry, AppError> {
        self.remember_durable_memory_with_evidence(
            event,
            authority,
            claim,
            topic,
            now_ms,
            supersedes,
            &[],
        )
    }

    /// A durable write carrying both caller-owned relations (#105):
    /// `supersedes` names the retained claims this write replaces, and
    /// `caused_by` names the retained claims that brought it about. The
    /// security gate decides first on the same non-serializable permit path,
    /// and every target must resolve to exactly one live durable node before
    /// anything is stored, so a relation can never widen the gate or outlive a
    /// node retention removed.
    #[allow(clippy::too_many_arguments)]
    pub fn remember_durable_memory_with_evidence(
        &mut self,
        event: &EventEnvelope,
        authority: Option<&AuthenticatedControl>,
        claim: &str,
        topic: Option<&str>,
        now_ms: u64,
        supersedes: &[&str],
        caused_by: &[&str],
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
            .remember_durable_with_evidence(
                &permit,
                claim,
                topic,
                now_ms,
                MemoryEvidence {
                    supersedes,
                    caused_by,
                },
            )
            .cloned()
            .map_err(|error| AppError::Adaptation(error.to_string()))
    }

    /// Operator erase of retained memory (#305, ADR-0008 S1). Requires the
    /// authenticated `memory.admin` capability, names an actor
    /// `(source namespace, raw actor id)` and/or retained memory ids, and
    /// returns the typed report including every remnant the runtime cannot
    /// prove erased. The raw actor id is hashed to its pseudonym and never
    /// retained. Not rate-limited here; the authenticated control path owns
    /// its own limiter.
    pub fn erase_memory_operator(
        &mut self,
        request_id: &str,
        authority: &AuthenticatedControl,
        actor: Option<(&str, &str)>,
        memory_ids: &[u64],
        now_ms: u64,
    ) -> Result<ErasureReport, AppError> {
        let permit = self
            .security
            .authorize_operator_memory_erase(request_id, authority, actor, memory_ids)?;
        self.apply_memory_erase(&permit, now_ms)?.ok_or_else(|| {
            AppError::Adaptation(
                "memory erase requested without configured adaptation runtime".to_owned(),
            )
        })
    }

    /// Run a permitted erase and audit its outcome for the operator, without
    /// claim text or raw ids. `None` when no memory store is configured: there
    /// is nothing retained to erase.
    fn apply_memory_erase(
        &mut self,
        permit: &MemoryErasePermit,
        now_ms: u64,
    ) -> Result<Option<ErasureReport>, AppError> {
        // A held or deferred reply of an erasing actor must never dispatch
        // later, whether or not a memory store is composed.
        if let (Some(lane), Some((source, actor))) = (self.chat_burst.as_mut(), permit.actor()) {
            lane.erase_actor(source, actor, now_ms);
        }
        let Some(adaptation) = self.adaptation.as_mut() else {
            return Ok(None);
        };
        let report = adaptation
            .memory
            .erase(permit, now_ms)
            .map_err(|error| AppError::Adaptation(error.to_string()))?;
        let record = self.security.redactor_record(
            Some(&report.request_id),
            aivtuber_telemetry::AuditCategory::Memory,
            format!("erase_{:?}", report.outcome).to_lowercase(),
            format!(
                "requester={:?} scope={:?} entries_removed={} links_removed={} verified={}",
                report.requester,
                report.scope_kind,
                report.entries_removed,
                report.links_removed,
                report.verified
            ),
        );
        self.security.push_external_audit(record);
        Ok(Some(report))
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

    /// Counted generation fallbacks by typed reason (issue #288). The daemon
    /// logs a line when this changes; tests assert it directly.
    pub fn generation_fallback_counts(&self) -> &GenerationFallbackCounts {
        &self.generation_fallbacks
    }

    pub fn generation_execution_snapshot(&self) -> Option<GenerationExecutionSnapshot> {
        self.generative
            .as_ref()
            .map(GenerativeRuntime::execution_snapshot)
    }

    /// Bounded shadow orchestration snapshot (#164), or `None` when the router
    /// has no shadow orchestration attached.
    pub fn shadow_execution_snapshot(&self) -> Option<ShadowExecutionSnapshot> {
        self.router.shadow_execution_snapshot()
    }

    /// Comparison records most recently drained from the bounded shadow pool
    /// (#164). Evidence only: never a production output.
    pub fn shadow_comparisons(&self) -> &[ShadowComparisonRecord] {
        &self.shadow_comparisons
    }

    fn drain_shadow_completions(&mut self) {
        let completions = self.router.drain_shadow_comparisons();
        if completions.is_empty() {
            return;
        }
        // Bounded by the orchestrator's completion mailbox, but re-clamp here so
        // a caller that never drains the router cannot grow this record list.
        const MAX_RETAINED_SHADOW_COMPARISONS: usize = 256;
        for completion in completions {
            let Some(record) = completion.record else {
                continue;
            };
            if self.shadow_comparisons.len() == MAX_RETAINED_SHADOW_COMPARISONS {
                self.shadow_comparisons.remove(0);
            }
            self.shadow_comparisons.push(record);
        }
    }

    /// Consult the #69 governor for #164 shadow work and queue the staged job.
    ///
    /// Called only after the active route succeeded. `route_with_budget`
    /// staged the job without queueing or charging anything, so this is the
    /// first moment at which shadow work becomes real: a refusal here means the
    /// job is never queued, and an admitted job is charged exactly once. If the
    /// active route fails instead, `discard_shadow_submission` drops the staged
    /// job and neither the ledger nor a one-shot denial is touched.
    ///
    /// When nothing was staged (disabled, filtered, unsampled, saturated,
    /// shutting down) no budget is consumed, because there is no work to pay
    /// for.
    fn commit_shadow_budget(&mut self, at_ms: u64) {
        if !self.router.shadow_submission_staged() {
            return;
        }
        let admitted = if !self.router.shadow_charges_budget() {
            true
        } else {
            match self.budget.as_mut() {
                Some(governor) => match governor.try_admit(BudgetAdmission::shadow(), at_ms) {
                    Ok(mut reservation) => {
                        // Shadow evaluation performs no TTS character spend of
                        // its own; settle immediately so the concurrency slot is
                        // returned rather than held for a job the ledger never
                        // measures.
                        reservation.settle(governor, 0, 0);
                        true
                    }
                    Err(_) => false,
                },
                // No governor configured: nothing is charged, so nothing can
                // deny the work.
                None => true,
            }
        };
        self.router.commit_shadow_submission(admitted);
    }

    pub fn retention_snapshot(&self) -> RuntimeRetentionSnapshot {
        RuntimeRetentionSnapshot {
            telemetry: self.telemetry.retention_metrics(),
            causal_traces: self.causal_traces.retention_metrics(),
            security: self.security.retention_metrics(),
            scheduler: self.performer.scheduler().metrics(),
            hot_cache: self.performer.assets().hot_cache_metrics(),
            cached_playback: self.performer.retention_metrics(),
            speech: self
                .speech_observations
                .as_ref()
                .map(SpeechObservationTracker::metrics),
            speech_transcripts: self
                .speech_transcripts
                .as_ref()
                .map(SpeechTranscriptTracker::metrics),
            working_memory: self
                .adaptation
                .as_ref()
                .map(|adaptation| adaptation.memory.retention_metrics()),
            adaptation: self
                .adaptation
                .as_ref()
                .map(|adaptation| adaptation.engine.retention_metrics()),
            generation: self
                .generative
                .as_ref()
                .map(GenerativeRuntime::execution_snapshot),
            speech_lifecycle: self.performer.scheduler().speech_lifecycle().metrics(),
            speech_freshness: self.performer.speech_freshness().metrics(),
            chat_burst: self.chat_burst_metrics(),
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

    /// Name of the active avatar output (see [`AvatarOutput::name`]).
    pub fn avatar_output_name(&self) -> &'static str {
        self.avatar.name()
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
        let interaction_started = Instant::now();
        let admission = self.security.admit_content_bytes(raw, at_ms)?;
        if admission != ContentAdmitDecision::Queued {
            return Ok(ContentProcessOutcome {
                admission,
                playback: None,
                memory_erasure: None,
            });
        }

        let event = self.security.pop_content().ok_or_else(|| {
            AppError::Routing("queued content was unavailable for dispatch".to_owned())
        })?;
        // #87: chat or speech restarts the idle clock and cancels a pending
        // idle initiative before anything else can wait behind it.
        self.note_content_arrival(event.kind, at_ms);
        // #305: an exact viewer self-erase command is consumed here, before
        // routing, so it can neither be spoken, remembered, nor reach a model.
        // The scope is the envelope's own pseudonym (the runtime mints the
        // permit); the command only ever reduces retained data.
        match self.security.authorize_viewer_memory_erase(&event) {
            ViewerEraseDecision::NotACommand => {}
            ViewerEraseDecision::Denied(_) => {
                return Ok(ContentProcessOutcome {
                    admission,
                    playback: None,
                    memory_erasure: None,
                });
            }
            ViewerEraseDecision::Permitted(permit) => {
                let memory_erasure = self.apply_memory_erase(&permit, at_ms)?;
                return Ok(ContentProcessOutcome {
                    admission,
                    playback: None,
                    memory_erasure,
                });
            }
        }
        // #79: a public-chat comment carrying a strict link (a link lure) is
        // never answered aloud, burst or not (answering it invites viewers to
        // follow it). Scam words alone do not decline: they match ordinary
        // chat such as "チャンネル登録しました".
        // It is not deleted: the session log below still records it.
        let guarded = event.kind == aivtuber_domain::EventKind::ChatMessage
            && event.source_class == aivtuber_domain::SourceClass::PublicChat
            && event
                .payload
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(aivtuber_reflex::is_unsafe_to_engage);
        if guarded {
            self.session_log_input(&event, at_ms);
            self.guarded_declined += 1;
            return Ok(ContentProcessOutcome {
                admission,
                playback: None,
                memory_erasure: None,
            });
        }
        // #463: opt-in session log; a no-op unless the operator turned it on.
        self.session_log_input(&event, at_ms);
        if self.chat_burst.is_some() {
            return self.process_through_chat_burst(
                admission,
                event,
                at_ms,
                seed,
                interaction_started,
            );
        }
        let playback = self.dispatch_admitted_event(event, at_ms, seed, interaction_started)?;
        Ok(ContentProcessOutcome {
            admission,
            playback,
            memory_erasure: None,
        })
    }

    /// Route one admitted, non-command content event through the per-event
    /// path: deadline, generation cancellation, routing and playback. This is
    /// the single dispatch point for immediate chat, selected burst replies
    /// and deferred replies, so scheduler, operator stop, mute and output
    /// gates apply identically to all of them.
    fn dispatch_admitted_event(
        &mut self,
        event: EventEnvelope,
        at_ms: u64,
        seed: u64,
        interaction_started: Instant,
    ) -> Result<Option<CachedPlaybackOutcome>, AppError> {
        let event_id = event.event_id.clone();
        let interaction_deadline = self
            .interaction_deadlines
            .deadline_for(event.kind, at_ms)?
            .ok_or_else(|| {
                AppError::Routing(
                    "operator/control events must not enter the content deadline path".to_owned(),
                )
            })?;
        let provider_deadline = interaction_started
            .checked_add(Duration::from_millis(
                interaction_deadline.total_budget_ms(),
            ))
            .ok_or_else(|| AppError::Routing("interaction deadline overflow".to_owned()))?;
        if let Some(generative) = &self.generative {
            generative.cancellation.cancel_for_event(&event);
        }
        let handled =
            self.handle_event(event, at_ms, seed, interaction_deadline, provider_deadline)?;
        self.tick(at_ms);
        if !handled.deferred_generation {
            self.record_handled_event(&event_id, &handled, at_ms);
        }
        Ok(handled.playback)
    }

    /// Comments declined from spoken replies by the audience guard (#79).
    pub fn guarded_declined(&self) -> u64 {
        self.guarded_declined
    }

    /// Opt in to live chat-burst selection and deferred replies (#75 S1,
    /// #78 S2). Off by default: without this call `process_content_bytes` is
    /// exactly the per-event path.
    ///
    /// With it, public chat that arrives in a burst is batched and a bounded
    /// Top-K is selected deterministically; a selected (or quiet) comment is
    /// answered at once while the performer is available and otherwise waits
    /// in a bounded inbox until the performer frees up or the reply expires.
    /// Every comment that enters the lane ends in exactly one terminal
    /// outcome (see [`ChatOutcome`]). Busy state comes only from the
    /// scheduler/speech lifecycle, the generation runtime and the operator
    /// mute flag, never from chat.
    pub fn with_chat_burst(mut self, config: ChatBurstConfig) -> Result<Self, AppError> {
        self.chat_burst = Some(
            chat_burst::ChatBurstLane::new(config)
                .map_err(|error| AppError::Routing(error.to_string()))?,
        );
        Ok(self)
    }

    /// #78 S3: augment the lane's local score with semantic retrieval. The
    /// scorer only orders untrusted comments among themselves and falls back to
    /// the text heuristic whenever retrieval is unavailable. Requires
    /// [`Self::with_chat_burst`] first.
    pub fn with_chat_burst_retrieval_score(
        mut self,
        scorer: aivtuber_reflex::RetrievalScorer,
    ) -> Result<Self, AppError> {
        self.chat_burst
            .as_mut()
            .ok_or_else(|| {
                AppError::Routing("retrieval scoring requires the chat burst lane".to_owned())
            })?
            .set_scorer(scorer);
        Ok(self)
    }

    /// #78 S3: let a batch ranker (for example the Jev one) reorder each
    /// flushed batch. Its answer is data: it may only reorder the batch's own
    /// candidates, the deterministic policy re-applies Top-K, the actor cap
    /// and the cooldown, and any failure falls back to the deterministic
    /// order. Requires [`Self::with_chat_burst`] first.
    pub fn with_chat_burst_ranker(
        mut self,
        ranker: Box<dyn aivtuber_reflex::BatchRanker + Send>,
    ) -> Result<Self, AppError> {
        self.chat_burst
            .as_mut()
            .ok_or_else(|| {
                AppError::Routing("a batch ranker requires the chat burst lane".to_owned())
            })?
            .set_ranker(ranker);
        Ok(self)
    }

    /// Chat-burst and deferred-reply counters; `None` when not composed.
    pub fn chat_burst_metrics(&self) -> Option<ChatBurstMetrics> {
        self.chat_burst
            .as_ref()
            .map(chat_burst::ChatBurstLane::metrics)
    }

    /// Remove and return up to `max` terminal records (event id, logical
    /// time, outcome) in the order they happened.
    pub fn take_chat_burst_records(&mut self, max: usize) -> Vec<ChatBurstRecord> {
        self.chat_burst
            .as_mut()
            .map(|lane| lane.take_records(max))
            .unwrap_or_default()
    }

    /// Remove and return up to `max` audience signals (#79) produced by
    /// aggregating batched chat, oldest first. Empty unless
    /// [`ChatBurstConfig::audience`] is set. Ids and counts only.
    pub fn take_audience_signals(&mut self, max: usize) -> Vec<aivtuber_reflex::AudienceSignal> {
        self.chat_burst
            .as_mut()
            .map(|lane| lane.take_audience_signals(max))
            .unwrap_or_default()
    }

    /// Remove and return up to `max` routed crowd reactions (#79 S2) as
    /// (class, member count), oldest first. No text and no actor ids.
    pub fn take_crowd_routes(&mut self, max: usize) -> Vec<(aivtuber_domain::CrowdClass, u32)> {
        self.chat_burst
            .as_mut()
            .map(|lane| lane.take_crowd_routes(max))
            .unwrap_or_default()
    }

    /// Remove and return up to `max` reports of replies dispatched from the
    /// deferred inbox.
    pub fn take_deferred_reply_reports(&mut self, max: usize) -> Vec<DeferredReplyReport> {
        self.chat_burst
            .as_mut()
            .map(|lane| lane.take_reports(max))
            .unwrap_or_default()
    }

    /// The scene reading the floor may use: trusted (rank 1 to 3) and fresh,
    /// otherwise `unknown`. The single path from scene context to the floor.
    fn floor_scene(&self, now_ms: u64) -> TrustedInterruptibility {
        self.scene
            .as_ref()
            .map_or(TrustedInterruptibility::unknown(), |scene| {
                scene.scheduling_context(now_ms)
            })
    }

    /// #81 会話フロアが人間の発話または発話待機により保持されているか判定する。
    /// 信頼された音声観測・トランスクリプト・フロアキューのみを参照し、公開チャット入力は一切見ない。
    fn floor_holds_reply(&self, now_ms: u64) -> bool {
        let Some(floor) = self.speech_floor.as_ref() else {
            return false;
        };
        // 音声入力が構成されておりオフの場合はフロア保持とみなさない
        if self
            .speech_input
            .as_ref()
            .is_some_and(|i| i.composed() && !i.is_on())
        {
            return false;
        }
        if floor.metrics().queued > 0 {
            return true;
        }
        let mut speakers = std::collections::BTreeSet::new();
        if let Some(observations) = self.speech_observations.as_ref() {
            speakers.extend(observations.speaker_ids());
        }
        if let Some(transcripts) = self.speech_transcripts.as_ref() {
            speakers.extend(transcripts.speaker_ids());
        }
        if speakers.is_empty() {
            return false;
        }
        let feed_age_ms = self
            .speech_feed_heartbeat_ms
            .map(|beat| now_ms.saturating_sub(beat));
        let scene = self.floor_scene(now_ms);
        let reply_inbox = self.chat_burst.is_some();
        for speaker in speakers {
            let input = aivtuber_domain::FloorInput {
                now_ms,
                user: speech_floor::floor_evidence(
                    self.speech_observations.as_ref(),
                    speaker,
                    now_ms,
                    feed_age_ms,
                ),
                hold: self
                    .speech_transcripts
                    .as_ref()
                    .map_or(aivtuber_domain::TranscriptHold::None, |transcripts| {
                        speech_floor::transcript_hold(transcripts, speaker, now_ms)
                    }),
                own: aivtuber_domain::OwnSpeech::Idle,
                reply_waiting: true,
                scene,
                reply_inbox,
            };
            let evaluation = aivtuber_scheduler::evaluate(floor.config(), &input);
            // SceneProtected による延長は後続の SceneProtected 分岐に委ねる
            if evaluation.action != aivtuber_domain::FloorAction::TakeTurn
                && evaluation.reason != aivtuber_domain::FloorReason::SceneProtected
            {
                return true;
            }
        }
        false
    }

    /// パフォーマーが今返信を受け付けられるか判定する。信頼されたランタイム状態
    /// （オペレーターのミュートフラグ、スケジューラの発話ライフサイクル、生成ランタイム、
    /// #81 会話フロア、#88 信頼されたシーン割り込み可否）のみから構築し、
    /// イベントフィールドは一切入力にしない。
    fn reply_availability(&self, now_ms: u64) -> ReplyAvailability {
        if self.security.is_muted() {
            ReplyAvailability::Busy(BusyCause::Muted)
        } else if self.performer.scheduler().speech_lifecycle().metrics().live > 0 {
            ReplyAvailability::Busy(BusyCause::Speaking)
        } else if self
            .generative
            .as_ref()
            .is_some_and(GenerativeRuntime::is_generating)
        {
            ReplyAvailability::Busy(BusyCause::Generating)
        } else if self.floor_holds_reply(now_ms) {
            ReplyAvailability::Busy(BusyCause::FloorHeld)
        } else if self.scene.as_ref().is_some_and(|scene| {
            scene.scheduling_interruptibility(now_ms) == Interruptibility::Uninterruptible
        }) {
            // #88: only a trusted scene source (the operator's OBS table, rank
            // 1 to 3) can say so; a perceived scene yields `unknown` here by
            // construction and never reaches this branch.
            ReplyAvailability::Busy(BusyCause::SceneProtected)
        } else {
            ReplyAvailability::Available
        }
    }

    /// Chat-burst lane entry (see [`Self::with_chat_burst`]).
    fn process_through_chat_burst(
        &mut self,
        admission: ContentAdmitDecision,
        event: EventEnvelope,
        at_ms: u64,
        seed: u64,
        interaction_started: Instant,
    ) -> Result<ContentProcessOutcome, AppError> {
        let current_id = event.event_id.clone();
        // Older deferred replies go first once the performer is available.
        self.drain_deferred_replies(at_ms, seed);

        let mut current: Option<Result<Option<CachedPlaybackOutcome>, AppError>> = None;
        let offer = self
            .chat_burst
            .as_mut()
            .map(|lane| lane.offer(event, at_ms))
            .ok_or_else(|| AppError::Routing("chat burst lane is not composed".to_owned()))?;
        match offer {
            chat_burst::Offer::Bypass(event) => {
                // Not a batchable chat comment (donation, game event, ...): the
                // unchanged per-event path, never held and never deferred.
                current =
                    Some(self.dispatch_admitted_event(event, at_ms, seed, interaction_started));
            }
            chat_burst::Offer::Quiet(candidate) => {
                let id = candidate.event.event_id.clone();
                let result = self.reply_now_or_defer(candidate, at_ms, seed, interaction_started);
                if id == current_id {
                    current = result;
                }
            }
            chat_burst::Offer::Held | chat_burst::Offer::Refused => {}
        }

        // Flush a due window and resolve ready batches; selected comments
        // reply now or wait, everything else is accounted by the lane.
        let selected = self
            .chat_burst
            .as_mut()
            .map(|lane| lane.resolve_ready(at_ms))
            .unwrap_or_default();
        self.session_log_crowd_routes(at_ms);
        for (index, candidate) in selected.into_iter().enumerate() {
            let id = candidate.event.event_id.clone();
            let result = self.reply_now_or_defer(
                candidate,
                at_ms,
                seed.wrapping_add(1 + index as u64),
                Instant::now(),
            );
            match result {
                Some(result) if id == current_id => current = Some(result),
                Some(result) => self.report_selected_dispatch(&id, at_ms, &result),
                None => {}
            }
        }

        let playback = match current {
            Some(result) => result?,
            None => {
                self.tick(at_ms);
                None
            }
        };
        Ok(ContentProcessOutcome {
            admission,
            playback,
            memory_erasure: None,
        })
    }

    /// Reply now when the performer is available, otherwise defer. `None`
    /// means deferred; `Some` is the dispatch result.
    fn reply_now_or_defer(
        &mut self,
        candidate: chat_burst::Candidate,
        at_ms: u64,
        seed: u64,
        interaction_started: Instant,
    ) -> Option<Result<Option<CachedPlaybackOutcome>, AppError>> {
        // A comment that went stale while it waited in a batch is never
        // answered late; the lane accounts it.
        if !self
            .chat_burst
            .as_mut()
            .is_none_or(|lane| lane.still_fresh(&candidate, at_ms))
        {
            return None;
        }
        // #79 S5: a crowd carrier that a reviewed cached line answers never
        // touches the generation worker, so a generation already in flight
        // must not add its queue wait to the crowd's route-to-dispatch interval
        // (Track A 2026-11-26: 3,175 ms of that route's 4,736 ms). Speech,
        // mute, the conversation floor and a protected scene still defer it.
        let availability = match self.reply_availability(at_ms) {
            ReplyAvailability::Busy(BusyCause::Generating)
                if self.crowd_answered_from_pack(&candidate, seed) =>
            {
                ReplyAvailability::Available
            }
            other => other,
        };
        match availability {
            ReplyAvailability::Busy(cause) => {
                let crowd = candidate.crowd.is_some();
                // A crowd has no single actor (#79 S4): the carrier is one
                // member's event, so its actor must not gate or be stamped.
                let actor = if crowd {
                    None
                } else {
                    chat_burst::actor_key(&candidate.event)
                };
                let accepted = self
                    .chat_burst
                    .as_mut()
                    .is_some_and(|lane| lane.defer(candidate, at_ms, cause));
                if accepted {
                    self.ack_deferred_reply(cause, crowd, actor.as_deref(), at_ms);
                }
                None
            }
            ReplyAvailability::Available => {
                let chat_burst::Candidate { event, crowd, .. } = candidate;
                let id = event.event_id.clone();
                self.session_log_note(&id, "path", "direct", at_ms);
                let result =
                    self.dispatch_maybe_crowd(event, crowd, at_ms, seed, interaction_started);
                if let Some(lane) = self.chat_burst.as_mut() {
                    lane.dispatched_now(&id, at_ms, result.is_ok());
                }
                Some(result)
            }
        }
    }

    /// #75: one bounded non-verbal ack for a reply that had to wait. Executed
    /// directly on the avatar output like [`Self::service_face_settle`]: no
    /// scheduler item, so reaction spacing and `last_terminal_at` are untouched
    /// and the ack cannot delay the reply it acknowledges. A failure is only
    /// counted; it never sets `health.avatar_error`.
    fn ack_deferred_reply(
        &mut self,
        cause: BusyCause,
        crowd: bool,
        actor: Option<&str>,
        at_ms: u64,
    ) {
        let gesture_busy = self.performer.scheduler().items().iter().any(|item| {
            matches!(item.status, Status::Queued | Status::Playing)
                && item
                    .plan
                    .channels
                    .contains(&aivtuber_scheduler::BlendChannel::Body)
        });
        let context = deferred_ack::AckContext {
            cause,
            crowd,
            muted: self.security.is_muted(),
            gesture_busy,
            actor,
        };
        if !self
            .chat_burst
            .as_mut()
            .is_some_and(|lane| lane.ack_admit(&context, at_ms))
        {
            return;
        }
        let result = self.avatar.execute(&AvatarPlaybackCommand::Gesture {
            generation: 0,
            at_ms,
            preset: deferred_ack::ACK_GESTURE_PRESET.to_owned(),
            amplitude: if crowd {
                deferred_ack::ACK_CROWD_GESTURE_AMPLITUDE
            } else {
                deferred_ack::ACK_GESTURE_AMPLITUDE
            },
        });
        if result.is_err()
            && let Some(lane) = self.chat_burst.as_mut()
        {
            lane.ack_output_failed();
        }
    }

    /// Dispatch a selected comment. An ordinary comment takes the unchanged
    /// per-event path. A crowd carrier (#79 S2) is answered by the engine's
    /// deterministic routing: a reaction family plays a cached or non-verbal
    /// reaction of its group and never generates a reply to the carrier's text
    /// (when no fitting asset exists it falls back to the ordinary reply path,
    /// today's behaviour); a repeated question gets one representative reply
    /// with the typed summary in its context. The content gate already ran at
    /// admission (and again for a deferred reply); nothing here reorders it.
    fn dispatch_maybe_crowd(
        &mut self,
        event: EventEnvelope,
        crowd: Option<chat_burst::CrowdDispatch>,
        at_ms: u64,
        seed: u64,
        interaction_started: Instant,
    ) -> Result<Option<CachedPlaybackOutcome>, AppError> {
        let Some(crowd) = crowd else {
            return self.dispatch_admitted_event(event, at_ms, seed, interaction_started);
        };
        let fallback = matches!(crowd.action, aivtuber_reflex::CrowdAction::NonVerbal { .. });
        if let aivtuber_reflex::CrowdAction::NonVerbal { group } = crowd.action
            && let Some(result) = self.play_crowd_reaction(&event, group, at_ms, seed)
        {
            if let Some(lane) = self.chat_burst.as_mut() {
                lane.note_crowd_non_verbal();
            }
            return result;
        }
        if let Some(lane) = self.chat_burst.as_mut() {
            lane.note_crowd_reply(fallback);
        }
        self.crowd_context = Some(crowd.summary);
        let result = self.dispatch_admitted_event(event, at_ms, seed, interaction_started);
        // `handle_event` takes it; a dispatch that failed earlier must not
        // leave it for the next event.
        self.crowd_context = None;
        result
    }

    /// The reviewed crowd line this group would play, if the pack has one
    /// (#79 S4), most specific group first. Pure: it only reads the asset
    /// descriptors, so "needs no generation" and "plays from the pack" are the
    /// same decision.
    fn crowd_reaction_intent(
        &self,
        event: &EventEnvelope,
        group: aivtuber_reflex::CrowdReactionGroup,
        seed: u64,
    ) -> Option<&'static str> {
        let variant_seed = seed.wrapping_add(event.sequence);
        chat_burst::crowd_asset_groups(group)
            .iter()
            .copied()
            .find(|candidate| {
                self.performer
                    .assets()
                    .select_variant_id(candidate, variant_seed, &[])
                    .is_some()
            })
    }

    /// Whether a routed crowd carrier is answered from the pack alone (#79
    /// S5). Such an answer makes no provider call, so the single generation
    /// worker is not part of its latency.
    fn crowd_answered_from_pack(&self, candidate: &chat_burst::Candidate, seed: u64) -> bool {
        let Some(crowd) = candidate.crowd.as_ref() else {
            return false;
        };
        let aivtuber_reflex::CrowdAction::NonVerbal { group } = crowd.action else {
            return false;
        };
        self.crowd_reaction_intent(&candidate.event, group, seed)
            .is_some()
    }

    /// Play a cached or non-verbal reaction for a crowd moment, or `None` when
    /// the pack has no fitting asset (the caller then falls back). Bypasses the
    /// router on purpose: nothing is generated or retrieved from the carrier's
    /// text, so no router decision, shadow job or generation is created.
    fn play_crowd_reaction(
        &mut self,
        event: &EventEnvelope,
        group: aivtuber_reflex::CrowdReactionGroup,
        at_ms: u64,
        seed: u64,
    ) -> Option<Result<Option<CachedPlaybackOutcome>, AppError>> {
        let intent = self.crowd_reaction_intent(event, group, seed)?;
        let event_id = event.event_id.clone();
        let mut reaction = event.clone();
        reaction.payload.insert(
            "intent".to_owned(),
            serde_json::Value::String(intent.to_owned()),
        );
        let played = self.performer.handle_event(
            &reaction,
            at_ms,
            seed,
            &self.visemes,
            &mut self.pending_audio,
            &mut self.pending_avatar,
        );
        let outcome = match played {
            Ok(outcome) => outcome,
            Err(error) => return Some(Err(error.into())),
        };
        self.tick(at_ms);
        let cache_level = cache_level(Some(outcome.cache_tier));
        let handled = HandledEvent {
            playback: Some(outcome),
            route: RouteClass::Deterministic,
            routing_latency_us: 0,
            generation_latency_us: None,
            generation_trace: None,
            decision: None,
            template_fallback: None,
            cache_level,
            deadline: None,
            deferred_generation: false,
            admitted_bytes: None,
            correlation_id: Some(event.correlation_id.clone()),
        };
        self.record_handled_event(&event_id, &handled, at_ms);
        Some(Ok(handled.playback))
    }

    fn report_selected_dispatch(
        &mut self,
        event_id: &str,
        at_ms: u64,
        result: &Result<Option<CachedPlaybackOutcome>, AppError>,
    ) {
        if let Some(lane) = self.chat_burst.as_mut() {
            lane.push_report(DeferredReplyReport {
                event_id: event_id.to_owned(),
                at_ms,
                asset_id: result
                    .as_ref()
                    .ok()
                    .and_then(|playback| playback.as_ref())
                    .map(|playback| playback.asset_id.clone()),
                dispatched: result.is_ok(),
            });
        }
    }

    /// Dispatch deferred replies while the performer stays available. Expired
    /// items are swept first, and every item is revalidated (security gate
    /// re-run, availability re-read) immediately before it dispatches.
    fn drain_deferred_replies(&mut self, at_ms: u64, seed: u64) {
        let bound = self
            .chat_burst
            .as_ref()
            .map_or(0, |lane| lane.pending_deferred().saturating_add(1));
        for step in 0..bound {
            let availability = self.reply_availability(at_ms);
            let Some(ready) = self
                .chat_burst
                .as_mut()
                .and_then(|lane| lane.poll_inbox(at_ms, availability))
            else {
                break;
            };
            let chat_burst::DeferredReply { event, crowd } = ready.item.payload.clone();
            if self.security.revalidate_deferred_content(&event) != DeferredRevalidation::Permitted
            {
                if let Some(lane) = self.chat_burst.as_mut() {
                    lane.complete_deferred(ready, DeferredOutcome::RevalidationRefused, at_ms);
                }
                continue;
            }
            self.session_log_note(&ready.item.id, "path", "deferred", at_ms);
            let result = self.dispatch_maybe_crowd(
                event,
                crowd,
                at_ms,
                seed.wrapping_add(step as u64),
                Instant::now(),
            );
            let outcome = if result.is_ok() {
                DeferredOutcome::Dispatched
            } else {
                DeferredOutcome::DispatchFailed
            };
            let event_id = ready.item.id.clone();
            if let Some(lane) = self.chat_burst.as_mut() {
                lane.complete_deferred(ready, outcome, at_ms);
            }
            self.report_selected_dispatch(&event_id, at_ms, &result);
        }
    }

    /// Loop hook for the daemon: sweep deferred-reply expiry, flush a due
    /// burst window and dispatch deferred replies once the performer is
    /// available. Bounded by the inbox capacity, touches no operator path and
    /// is a no-op unless chat-burst mode is composed. Call it after draining
    /// operator requests so emergency control is never queued behind it.
    pub fn service_chat_burst(&mut self, now_ms: u64, seed: u64) {
        if self.chat_burst.is_none() {
            return;
        }
        self.drain_deferred_replies(now_ms, seed);
        let selected = self
            .chat_burst
            .as_mut()
            .map(|lane| lane.resolve_ready(now_ms))
            .unwrap_or_default();
        self.session_log_crowd_routes(now_ms);
        for (index, candidate) in selected.into_iter().enumerate() {
            let id = candidate.event.event_id.clone();
            if let Some(result) = self.reply_now_or_defer(
                candidate,
                now_ms,
                seed.wrapping_add(1 + index as u64),
                Instant::now(),
            ) {
                self.report_selected_dispatch(&id, now_ms, &result);
            }
        }
    }

    /// Fill a generation route's curated context with bounded retained memory
    /// (#105).
    ///
    /// Only memory the write gate already admitted can appear: this reads
    /// [`WorkingMemory`] and never writes it, so recall can neither mint a
    /// claim, promote a class, nor buy a durable write. The scope rule lives in
    /// [`WorkingMemory::context_for`] — the current actor's own claims plus
    /// actorless shared memory, never another viewer's claim.
    ///
    /// The budget is a prefix rule, not a truncation: the current event's text
    /// is charged first, then retained claims in rank order until the request's
    /// total context bytes are spent. A claim that does not fit is dropped
    /// whole, because shortening a retained claim is the semantic corruption
    /// #231 fixed for the store — a truncated claim would reach generation as
    /// if it were what the runtime ever admitted. Each item keeps the class it
    /// was classified under, so generation can phrase a viewer claim as one.
    /// The whole delivered-memory rule lives in [`retained_memory_context`],
    /// which the long-range evaluation calls as well.
    fn attach_memory_context(
        &mut self,
        event: &EventEnvelope,
        route: &mut PlaybackRoute,
        at_ms: u64,
    ) {
        let PlaybackRoute::Generate(generation) = route else {
            return;
        };
        let Some(adaptation) = self.adaptation.as_ref() else {
            return;
        };
        // Curated context a caller already supplied (operator curation) is not
        // displaced by retrieval: the operator owns that slot.
        if !generation.thinking.curated_context.is_empty() {
            return;
        }
        let topic = event
            .payload
            .get("topic")
            .and_then(serde_json::Value::as_str);
        // #404: the character sheet is placed first by `attach_persona` and is
        // never dropped, so memory fills only what it leaves of the item and
        // byte budgets.
        let reserve = self
            .persona
            .as_ref()
            .and_then(persona::PersonaLoad::sheet)
            .map_or(ContextReserve::NONE, |sheet| {
                ContextReserve::one_item(sheet.len())
            });
        let context = retained_memory_context_reserving(
            &adaptation.memory,
            &event.source,
            event.actor_id.as_deref(),
            topic,
            at_ms,
            generation.thinking.input.text.len(),
            reserve,
        );
        if context.is_empty() {
            return;
        }
        self.memory_falsifier.note_delivered(&context);
        if let Some(writer) = self.memory_writer.as_mut() {
            writer.sweep(&adaptation.memory, at_ms);
            writer.note_retrieved(
                context
                    .iter()
                    .filter(|item| {
                        item.event_id
                            .as_deref()
                            .is_some_and(|id| id.starts_with(MEMORY_WRITER_EVENT_PREFIX))
                    })
                    .count(),
            );
        }
        generation.thinking.curated_context = context;
        if generation.thinking.validate().is_err() {
            // Fail closed to "no memory context" rather than failing the event:
            // retrieval enriches a reply and is never a precondition for one.
            generation.thinking.curated_context = Vec::new();
        }
    }

    /// Put the operator's character sheet first in a generation route's
    /// curated context (#404).
    ///
    /// The sheet is one `operator_curated` item (see
    /// [`persona::PersonaSheet::thinking_content`]), placed at index 0 after
    /// memory was selected with its room reserved, so it is never dropped and
    /// memory plus the scene caption fill the rest. It is data in the Thinking
    /// request: this reads the sheet loaded at start and mints nothing, and
    /// the request carries no capability or permit to mint one from. With no
    /// sheet loaded this is a no-op and the request is byte-identical to one
    /// built before the character sheet existed.
    fn attach_persona(&self, route: &mut PlaybackRoute) {
        let Some(sheet) = self.persona.as_ref().and_then(persona::PersonaLoad::sheet) else {
            return;
        };
        let PlaybackRoute::Generate(generation) = route else {
            return;
        };
        let thinking = &mut generation.thinking;
        thinking.curated_context.insert(0, sheet.thinking_content());
        if thinking.validate().is_err() {
            // The reserved room should make this unreachable (the input is at
            // most one item and the sheet at most 3,000 bytes). If it ever
            // happens, drop memory, never the sheet.
            thinking.curated_context.truncate(1);
        }
    }

    /// Add the crowd summary to a generation route (#79 S2). The summary is
    /// closed vocabulary and counts (no member text, no actor id), built by the
    /// lane from the engine's own record; it is taken here whether or not the
    /// route generates, so it cannot outlive the event it was set for. A
    /// request that does not validate with it is kept exactly as routed.
    fn attach_crowd_context(&mut self, route: &mut PlaybackRoute) {
        let Some(summary) = self.crowd_context.take() else {
            return;
        };
        let PlaybackRoute::Generate(generation) = route else {
            return;
        };
        generation.thinking.context.crowd = Some(summary);
        if generation.thinking.validate().is_err() {
            generation.thinking.context.crowd = None;
        }
    }

    /// Add the scene context to a generation route (#88 S1, #90 S2).
    ///
    /// * `context.scene` is the closed-vocabulary snapshot only: a scene class,
    ///   closed labels, an interruptibility hint and ranks. It has no free text.
    /// * The one screen caption is the only free text that can arrive here. It
    ///   is built by the perception tracker as an untrusted, aged, delimited
    ///   item of at most 512 bytes, appended to `curated_context` after memory
    ///   and dropped whole if it does not fit the request budget (memory keeps
    ///   its room). It is never given to memory, TTS or the memory writer: this
    ///   function only ever touches the Thinking request.
    /// * Nothing here reads or sets a scheduling input. Busy and deferral read
    ///   the scene tracker's trusted interruptibility, which a perceived scene
    ///   cannot carry.
    ///
    /// With no scene service and no perception composed this is a no-op, so the
    /// request is byte-identical to one built before either existed.
    fn attach_scene_context(
        &mut self,
        route: &mut PlaybackRoute,
        scene: Option<SceneContextSnapshot>,
        at_ms: u64,
    ) {
        if scene.is_none() && self.perception.is_none() {
            return;
        }
        let PlaybackRoute::Generate(generation) = route else {
            return;
        };
        if let Some(scene) = scene {
            generation.thinking.context.scene = Some(scene);
        }
        let Some(perception) = self.perception.as_mut() else {
            return;
        };
        let Some(caption) = perception.tracker_mut().thinking_caption_item(at_ms) else {
            return;
        };
        generation.thinking.curated_context.push(caption);
        if generation.thinking.validate().is_err() {
            // The caption (or the scene) does not fit: keep the request exactly
            // as the route built it rather than failing the event.
            generation.thinking.curated_context.pop();
            if generation.thinking.validate().is_err() {
                generation.thinking.context.scene = None;
            }
            if let Some(scene) = self.scene.as_mut() {
                scene.note_caption_dropped();
            }
        }
    }

    fn handle_event(
        &mut self,
        mut event: EventEnvelope,
        at_ms: u64,
        seed: u64,
        interaction_deadline: InteractionDeadline,
        provider_deadline: Instant,
    ) -> Result<HandledEvent, AppError> {
        let remaining_at_route_ms = interaction_deadline.remaining_ms(at_ms);
        let mut deadline_observation =
            DeadlineObservation::new(interaction_deadline, remaining_at_route_ms);
        // Bounded size observation only: the serialized payload length, never
        // the payload content (privacy is structural in causal traces).
        let admitted_bytes = serde_json::to_vec(&event.payload)
            .map(|bytes| bytes.len() as u64)
            .ok();
        let causal_correlation_id = event.correlation_id.clone();
        let route_started = Instant::now();
        let route_budget = remaining_instant(provider_deadline).unwrap_or(Duration::ZERO);
        // #164: `route_with_budget` only *stages* the shadow job. If the active
        // route fails, the stage is dropped below so no denial and no charge
        // can leak into the next, unrelated event.
        // #88: the scene snapshot at this event's logical time, handed to the
        // planner before it decides so a Reflex/Jev request and its replay record
        // name the snapshot they used. Absent (and the planner untouched) when no
        // scene source is composed.
        let scene = self.scene_snapshot_for_request(at_ms);
        if self.scene.is_some() {
            self.router.set_scene_context(scene.clone());
        }
        let mut route = match self.router.route_with_budget(&event, Some(route_budget)) {
            Ok(route) => route,
            Err(error) => {
                self.router.discard_shadow_submission();
                return Err(error);
            }
        };
        // The active route succeeded, so this revision supersedes whatever the
        // same source chain was evaluating — independently of whether this event
        // produces shadow work at all. A filtered, unsampled, saturated, or
        // budget-denied revision still makes the previous one stale.
        self.router.supersede_shadow_source(&event);
        // The staged shadow job is now real: the #69 governor decides whether it
        // is queued. A refusal can never fail an active event.
        self.commit_shadow_budget(at_ms);
        // #105: a generated reply may draw on bounded retained memory that the
        // write gate already admitted. The composition root does this because
        // it owns both the memory store and the routed request; the planner
        // keeps producing a memory-free request.
        self.attach_memory_context(&event, &mut route, at_ms);
        // #404: the operator's character sheet, first and never dropped.
        self.attach_persona(&mut route);
        // #88 / #90 S2: closed-vocabulary scene for the Reflex and Thinking
        // context, and at most one untrusted screen caption for Thinking.
        self.attach_scene_context(&mut route, scene, at_ms);
        // #79 S2: the typed crowd summary, when this reply stands for a crowd.
        self.attach_crowd_context(&mut route);
        // #80 S1: the performer's own recent turns, for recall questions.
        self.attach_recent_turns(&event, &mut route, at_ms);
        let routing_latency_us = elapsed_us(route_started);
        let decision = self.router.decision_record().cloned();
        let template_fallback = self
            .router
            .template_fallback()
            .map(|fallback| fallback.reason());
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
                        template_fallback,
                        cache_level: None,
                        deadline: Some(deadline_observation.clone()),
                        deferred_generation: false,
                        admitted_bytes: None,
                        correlation_id: Some(causal_correlation_id.clone()),
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
                let context = GenerationCompletionContext {
                    routing_latency_us,
                    decision: decision.clone(),
                    template_fallback,
                    budget_reservation: None,
                    interaction_deadline,
                    remaining_at_route_ms,
                    remaining_at_generation_ms: remaining_instant(provider_deadline)
                        .map(|remaining| remaining.as_millis().min(u128::from(u64::MAX)) as u64)
                        .unwrap_or(0),
                    correlation_id: causal_correlation_id.clone(),
                };
                deadline_observation.remaining_at_generation_ms =
                    Some(context.remaining_at_generation_ms);
                match self.submit_generation(
                    event,
                    *route,
                    at_ms,
                    seed,
                    provider_deadline,
                    context,
                )? {
                    GenerationSubmission::Deferred => {
                        return Ok(HandledEvent {
                            playback: None,
                            route: RouteClass::Generated,
                            routing_latency_us,
                            generation_latency_us: None,
                            generation_trace: None,
                            decision: None,
                            template_fallback: None,
                            cache_level: None,
                            deadline: None,
                            deferred_generation: true,
                            admitted_bytes: None,
                            correlation_id: None,
                        });
                    }
                    GenerationSubmission::Immediate {
                        handled,
                        deadline_exhaustion,
                    } => {
                        generation_latency_us = Some(elapsed_us(generation_started));
                        if let Some((stage, reason)) = deadline_exhaustion {
                            deadline_observation.exhaustion_stage = Some(stage);
                            deadline_observation.exhaustion_reason = Some(reason);
                        }
                        let HandledGeneration {
                            playback,
                            route,
                            trace,
                            cache_level,
                        } = *handled;
                        generation_trace = Some(trace);
                        (playback, route, cache_level)
                    }
                }
            }
        };

        Ok(HandledEvent {
            playback,
            route: route_class,
            routing_latency_us,
            generation_latency_us,
            generation_trace,
            decision,
            template_fallback,
            cache_level,
            deadline: Some(deadline_observation),
            deferred_generation: false,
            admitted_bytes,
            correlation_id: Some(causal_correlation_id),
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

    fn submit_generation(
        &mut self,
        event: EventEnvelope,
        route: GenerationRoute,
        at_ms: u64,
        seed: u64,
        provider_deadline: Instant,
        mut context: GenerationCompletionContext,
    ) -> Result<GenerationSubmission, AppError> {
        if route.source_event != event {
            return Err(AppError::Generation(
                "generation route source event does not match dispatched event".to_owned(),
            ));
        }

        let request = GenerationRequest {
            source_event: event.clone(),
            thinking: route.thinking,
            routing_reason: route.routing_reason,
            intent: route.intent,
            style: route.style,
            fallback_variant_group: route.fallback_variant_group,
            seed,
        };
        let fallback = select_fallback_directive(&request, Some(self.performer.assets()));
        let output_policy = self.security.public_output_policy();
        let minimum_start =
            Duration::from_millis(self.interaction_deadlines.min_generation_start_ms);
        if remaining_instant(provider_deadline).is_none_or(|remaining| remaining < minimum_start) {
            // The deadline expired before the budget was consulted; release
            // any reservation granted above (never charged units).
            let handled = self.commit_generation_result(
                &event,
                GenerationResult {
                    trace: GenerationTrace {
                        fallback_reason: FallbackReason::Timeout,
                        ..GenerationTrace::default()
                    },
                    disposition: GenerationDisposition::Fallback {
                        directive: fallback.clone(),
                    },
                },
                &fallback,
                None,
                at_ms,
                seed,
                context.budget_reservation.as_mut(),
            )?;
            return Ok(GenerationSubmission::Immediate {
                handled: Box::new(handled),
                deadline_exhaustion: Some((
                    DeadlineStage::GenerationQueue,
                    DeadlineExhaustionReason::InsufficientBudget,
                )),
            });
        }
        // Issue #69: budget admission happens strictly before submit, so a
        // denial never allocates provider work that must then be cancelled.
        // Priority comes from the trusted #67 deadline class, never from
        // untrusted content fields. On submit failure the reservation is
        // released (rejected/degraded accounting); on success it travels
        // with the completion context and is settled at commit time.
        if let Some(governor) = self.budget.as_mut() {
            let admission = budget_admission_for(&context.interaction_deadline);
            match governor.try_admit(admission, at_ms) {
                Ok(reservation) => context.budget_reservation = Some(reservation),
                Err(denial) => {
                    governor.note_degraded();
                    let record = self.security.redactor_record(
                        Some(&event.event_id),
                        aivtuber_telemetry::AuditCategory::Generation,
                        "budget_denied",
                        format!("reason={}", denial.as_str()),
                    );
                    self.security.push_external_audit(record);
                    let trace = GenerationTrace {
                        fallback_reason: FallbackReason::BudgetExhausted,
                        budget_denial_reason: Some(denial.as_str().to_owned()),
                        ..GenerationTrace::default()
                    };
                    let handled = self.commit_generation_result(
                        &event,
                        GenerationResult {
                            trace,
                            disposition: GenerationDisposition::Fallback {
                                directive: fallback.clone(),
                            },
                        },
                        &fallback,
                        None,
                        at_ms,
                        seed,
                        None,
                    )?;
                    return Ok(GenerationSubmission::Immediate {
                        handled: Box::new(handled),
                        deadline_exhaustion: None,
                    });
                }
            }
        }
        let submit = self
            .generative
            .as_mut()
            .ok_or_else(|| {
                AppError::Generation(
                    "generative route requested without configured runtime".to_owned(),
                )
            })?
            .submit(
                request.clone(),
                fallback.clone(),
                output_policy,
                provider_deadline,
                context,
            );

        match submit {
            Ok(_) => Ok(GenerationSubmission::Deferred),
            Err(error) => {
                // The submit failed; the context (with its reservation) was
                // moved into the runtime, but `GenerationExecutor::try_submit`
                // dropped the work item on error, so the reservation's
                // concurrency slot was already returned by Drop. Nothing more
                // is owed to the budget here.
                let trace = GenerationTrace {
                    fallback_reason: match error {
                        GenerationSubmitError::Saturated => FallbackReason::Overloaded,
                        GenerationSubmitError::ShuttingDown
                        | GenerationSubmitError::Disconnected => FallbackReason::Unavailable,
                    },
                    ..GenerationTrace::default()
                };
                let handled = self.commit_generation_result(
                    &event,
                    GenerationResult {
                        trace,
                        disposition: GenerationDisposition::Fallback {
                            directive: fallback.clone(),
                        },
                    },
                    &fallback,
                    None,
                    at_ms,
                    seed,
                    None,
                )?;
                Ok(GenerationSubmission::Immediate {
                    handled: Box::new(handled),
                    deadline_exhaustion: None,
                })
            }
        }
    }

    fn record_generation_calls(&mut self, trace: &GenerationTrace) {
        if let (Some(projection), Some(generative)) =
            (&trace.speech_projection, self.generative.as_mut())
        {
            let metrics = &mut generative.speech_projection;
            metrics.replies = metrics.replies.saturating_add(1);
            metrics.lexicon_hits = metrics.lexicon_hits.saturating_add(projection.lexicon_hits);
            metrics.passthrough_latin = metrics
                .passthrough_latin
                .saturating_add(projection.passthrough_latin);
        }
        for call in &trace.llm_calls {
            self.security.record_generation_call(
                &call.event_id,
                call.routing_reason.as_str(),
                &call.backend.name,
                call.backend.model_alias.as_deref(),
                call.backend.model_version.as_deref(),
            );
        }
    }

    fn record_worker_output_decision(&mut self, output: Option<&PublicOutput>) {
        if let Some(output) = output {
            self.security.record_public_output_decision(output);
        }
    }

    /// Perform the typed fallback chosen before submission: a cached reaction
    /// from the pack goes through the normal cached performer path (scheduler,
    /// operator stop/mute, avatar and audio sinks); the typed non-verbal
    /// directive, which is only chosen when the pack has no usable asset for
    /// the group, carries no playback (issue #288).
    fn play_fallback_directive(
        &mut self,
        event: &EventEnvelope,
        directive: &FallbackDirective,
        trace: GenerationTrace,
        at_ms: u64,
        seed: u64,
    ) -> Result<HandledGeneration, AppError> {
        // #87: an idle initiative that cannot be generated is never replaced by
        // a spoken cached reaction; it is a visual gesture only.
        if self.is_initiative_event(event) {
            self.initiative_fell_back(at_ms);
            return Ok(HandledGeneration {
                playback: None,
                route: RouteClass::NonVerbalFallback,
                trace,
                cache_level: None,
            });
        }
        match directive {
            FallbackDirective::CachedReaction { asset_id, .. } => {
                let outcome = self.performer.handle_asset_id(
                    event,
                    asset_id,
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
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_generation_result(
        &mut self,
        event: &EventEnvelope,
        result: GenerationResult,
        fallback: &FallbackDirective,
        output_decision: Option<PublicOutput>,
        at_ms: u64,
        seed: u64,
        budget_reservation: Option<&mut BudgetReservation>,
    ) -> Result<HandledGeneration, AppError> {
        self.record_generation_calls(&result.trace);
        let mut trace = result.trace;

        // Issue #69: settle the budget reservation with observed TTS usage
        // (reply text length). The concurrency slot is released either way;
        // unit ledgers are topped up to actuals when a budget enforces them.
        if let Some(reservation) = budget_reservation {
            let actual_tts_chars = if trace.tts_attempted {
                result_text_chars(&trace, output_decision.as_ref())
            } else {
                0
            };
            if let Some(governor) = self.budget.as_mut() {
                reservation.settle(governor, actual_tts_chars, 0);
            }
        }

        match result.disposition {
            GenerationDisposition::Generated { asset } => {
                let generated_text = asset
                    .speech
                    .as_ref()
                    .and_then(|speech| speech.text.as_deref());

                let initial_gate_matches = output_decision.as_ref().is_some_and(|output| {
                    matches!(output.verdict, OutputVerdict::Allow | OutputVerdict::Redact)
                        && output.text.as_deref() == generated_text
                        && generated_text.is_some()
                });

                if !initial_gate_matches {
                    self.record_worker_output_decision(output_decision.as_ref());
                    trace.fallback_reason = FallbackReason::PolicyOverride;
                    return self.play_fallback_directive(event, fallback, trace, at_ms, seed);
                }

                let generated_text = generated_text.expect("checked above").to_owned();
                let rechecked = self
                    .security
                    .public_output_policy()
                    .evaluate(&generated_text);
                let recheck_allows_same_text = matches!(
                    rechecked.verdict,
                    OutputVerdict::Allow | OutputVerdict::Redact
                ) && rechecked.text.as_deref()
                    == Some(generated_text.as_str());

                if !recheck_allows_same_text {
                    self.security.record_public_output_decision(&rechecked);
                    trace.fallback_reason = FallbackReason::PolicyOverride;
                    return self.play_fallback_directive(event, fallback, trace, at_ms, seed);
                }

                self.record_worker_output_decision(output_decision.as_ref());

                // #89: an idle initiative that repeats a recent phrase is
                // refused like any other refused output: the idle gesture
                // replaces speech, with no regeneration.
                if self.initiative_repeat_refused(event, &generated_text, at_ms) {
                    trace.fallback_reason = FallbackReason::PolicyOverride;
                    return self.play_fallback_directive(event, fallback, trace, at_ms, seed);
                }
                if self.is_initiative_event(event) {
                    self.repeat_guard.stage(&generated_text, at_ms);
                }

                let asset_id = asset.id.clone();
                self.performer
                    .assets_mut()
                    .insert_hot(*asset)
                    .map_err(|error| AppError::Generation(error.to_string()))?;
                let outcome = self.performer.handle_asset_id(
                    event,
                    &asset_id,
                    CachedPlaybackTiming { at_ms, seed },
                    &self.visemes,
                    &mut self.pending_audio,
                    &mut self.pending_avatar,
                )?;
                // The reply is gated and scheduled: consolidation reads that
                // committed text, never the viewer's event.
                self.consolidate_committed_reply(event, &generated_text, at_ms);
                Ok(HandledGeneration {
                    playback: Some(outcome),
                    route: RouteClass::Generated,
                    trace,
                    cache_level: Some(CacheLevel::Generated),
                })
            }
            GenerationDisposition::Fallback { directive } => {
                self.record_worker_output_decision(output_decision.as_ref());
                self.play_fallback_directive(event, &directive, trace, at_ms, seed)
            }
            GenerationDisposition::Cancelled { .. } => {
                self.record_worker_output_decision(output_decision.as_ref());
                Ok(HandledGeneration {
                    playback: None,
                    route: RouteClass::Generated,
                    trace,
                    cache_level: None,
                })
            }
            // A phrased reply is committed phrase by phrase before its final
            // completion arrives; reaching here means no phrase was committed.
            GenerationDisposition::Phrased { .. } => {
                self.record_worker_output_decision(output_decision.as_ref());
                trace.fallback_reason = FallbackReason::PolicyOverride;
                Ok(HandledGeneration {
                    playback: None,
                    route: RouteClass::NonVerbalFallback,
                    trace,
                    cache_level: None,
                })
            }
        }
    }

    fn drain_generation_completions(&mut self, now_ms: u64) {
        loop {
            let completion = self
                .generative
                .as_ref()
                .and_then(GenerativeRuntime::try_next_completion);
            let Some(mut completion) = completion else {
                break;
            };

            // Every phrase the worker handed over before it finished is
            // committed (or refused) before its final completion is read.
            self.drain_phrase_messages(now_ms);
            let phrased_run = self
                .generative
                .as_mut()
                .and_then(|generative| generative.phrase.take_run(completion.generation_id));
            let cancelled_by_deadline = phrased_run
                .as_ref()
                .is_some_and(phrase_commit::PhraseRun::cancelled_by_deadline);
            if let Some(run) = phrased_run
                && run.first_handled
            {
                // Phrase 0 was already observed (committed or refused at the
                // commit boundary): the final completion only closes the reply.
                self.complete_phrased_reply(completion, run);
                continue;
            }

            let event_id = completion.request.source_event.event_id.clone();
            let deadline_expired = remaining_instant(completion.provider_deadline).is_none();
            let stale_or_cancelled = self.generative.as_ref().is_none_or(|generative| {
                generative.is_shutting_down()
                    || !generative.is_current(completion.generation_id, &event_id)
                    || completion.cancellation.is_cancelled()
                    || deadline_expired
            });

            if stale_or_cancelled {
                let mut trace = completion
                    .result
                    .as_ref()
                    .ok()
                    .map(|result| result.trace.clone())
                    .unwrap_or_default();
                self.record_generation_calls(&trace);
                self.record_worker_output_decision(completion.output_decision.as_ref());
                // Issue #69: a stale/cancelled completion releases its budget
                // reservation without charging unit ledgers (rejected/degraded
                // accounting); the concurrency slot returns either way.
                if let (Some(reservation), Some(governor)) = (
                    completion.context.budget_reservation.as_mut(),
                    self.budget.as_mut(),
                ) {
                    reservation.release(governor);
                }
                // #469: a reply that missed its interaction deadline is not an
                // operator stop or a newer line superseding it, so the viewer
                // must not be left with nothing: the typed fallback runs
                // (once, no retry) and is counted. Cancellation by anything
                // else, and initiatives (#87), stay silent.
                let missed_deadline = deadline_expired
                    && (!completion.cancellation.is_cancelled() || cancelled_by_deadline)
                    && self.generative.as_ref().is_some_and(|generative| {
                        !generative.is_shutting_down()
                            && generative.is_current(completion.generation_id, &event_id)
                    })
                    && !self.is_initiative_event(&completion.request.source_event);
                // The late reply itself is discarded either way.
                if trace.cancelled_stage.is_none() {
                    trace.cancelled_stage = Some(CancellationStage::BeforePublish);
                }
                let late_fallback = if missed_deadline {
                    trace.fallback_reason = FallbackReason::Timeout;
                    let played = self
                        .play_fallback_directive(
                            &completion.request.source_event,
                            &completion.fallback,
                            trace.clone(),
                            now_ms,
                            completion.request.seed,
                        )
                        .unwrap_or(HandledGeneration {
                            playback: None,
                            route: RouteClass::NonVerbalFallback,
                            trace: trace.clone(),
                            cache_level: None,
                        });
                    Some(played)
                } else {
                    None
                };
                let exhaustion = if deadline_expired {
                    (DeadlineStage::Commit, DeadlineExhaustionReason::Expired)
                } else if completion.cancellation.is_cancelled() {
                    (DeadlineStage::Commit, DeadlineExhaustionReason::Cancelled)
                } else {
                    (
                        DeadlineStage::Commit,
                        DeadlineExhaustionReason::StaleCompletion,
                    )
                };
                let deadline = completion_deadline_observation(&completion, Some(exhaustion));

                // #469: a chat turn ended on purpose by a newer line or an
                // operator stop is counted and logged, never silent.
                let ended_on_purpose = late_fallback.is_none()
                    && self
                        .generative
                        .as_ref()
                        .is_some_and(|generative| !generative.is_shutting_down())
                    && !self.is_initiative_event(&completion.request.source_event);
                let (playback, route, trace, cache_level) = match late_fallback {
                    Some(played) => (
                        played.playback,
                        played.route,
                        played.trace,
                        played.cache_level,
                    ),
                    None => (None, RouteClass::Generated, trace, None),
                };
                let handled = HandledEvent {
                    playback,
                    route,
                    routing_latency_us: completion.context.routing_latency_us,
                    generation_latency_us: Some(completion.generation_latency_us),
                    generation_trace: Some(trace),
                    decision: completion.context.decision,
                    template_fallback: completion.context.template_fallback,
                    cache_level,
                    deadline: Some(deadline),
                    deferred_generation: false,
                    admitted_bytes: None,
                    correlation_id: Some(completion.context.correlation_id.clone()),
                };
                self.record_handled_event(&event_id, &handled, now_ms);
                if ended_on_purpose {
                    self.generation_fallbacks.superseded += 1;
                    self.session_log_fallback(&event_id, "superseded", false, now_ms);
                }

                if let Some(generative) = self.generative.as_mut() {
                    generative.note_cancelled_or_stale();
                    generative.finish(completion.generation_id, &event_id);
                }
                continue;
            }

            let mut deadline = completion_deadline_observation(&completion, None);
            let result = match completion.result {
                Ok(result) => result,
                Err(_) => {
                    if let Some(generative) = self.generative.as_ref() {
                        generative.note_failed();
                    }
                    GenerationResult {
                        trace: GenerationTrace {
                            fallback_reason: FallbackReason::InvalidRequest,
                            ..GenerationTrace::default()
                        },
                        disposition: GenerationDisposition::Fallback {
                            directive: completion.fallback.clone(),
                        },
                    }
                }
            };

            let handled_generation = self.commit_generation_result(
                &completion.request.source_event,
                result,
                &completion.fallback,
                completion.output_decision,
                now_ms,
                completion.request.seed,
                completion.context.budget_reservation.as_mut(),
            );

            match handled_generation {
                Ok(handled_generation) => {
                    let HandledGeneration {
                        playback,
                        route,
                        trace,
                        cache_level,
                    } = handled_generation;
                    if trace.fallback_reason == FallbackReason::Timeout {
                        deadline.exhaustion_stage = Some(if trace.tts_attempted {
                            DeadlineStage::Tts
                        } else {
                            DeadlineStage::Thinking
                        });
                        deadline.exhaustion_reason =
                            Some(DeadlineExhaustionReason::ProviderTimeout);
                    }
                    let handled = HandledEvent {
                        playback,
                        route,
                        routing_latency_us: completion.context.routing_latency_us,
                        generation_latency_us: Some(completion.generation_latency_us),
                        generation_trace: Some(trace),
                        decision: completion.context.decision,
                        template_fallback: completion.context.template_fallback,
                        cache_level,
                        deadline: Some(deadline),
                        deferred_generation: false,
                        admitted_bytes: None,
                        correlation_id: Some(completion.context.correlation_id.clone()),
                    };
                    self.record_handled_event(&event_id, &handled, now_ms);
                }
                Err(_) => {
                    if let Some(generative) = self.generative.as_ref() {
                        generative.note_failed();
                    }
                }
            }

            if let Some(generative) = self.generative.as_mut() {
                generative.finish(completion.generation_id, &event_id);
            }
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

        // Template fallbacks are recorded by the planner itself (issue #54);
        // prefer that reason when the template route degraded to silence.
        if let Some(reason) = handled.template_fallback {
            observation.fallback_reason = Some(reason.to_owned());
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

        if let Some(deadline) = &handled.deadline {
            observation.deadline_class = Some(deadline.deadline.class);
            observation.deadline_budget_ms = Some(deadline.deadline.total_budget_ms());
            observation.deadline_remaining_at_route_ms = Some(deadline.remaining_at_route_ms);
            observation.deadline_remaining_at_generation_ms = deadline.remaining_at_generation_ms;
            observation.deadline_exhaustion_stage = deadline.exhaustion_stage;
            observation.deadline_exhaustion_reason = deadline.exhaustion_reason;
            observation.generation_queue_wait_us = deadline.generation_queue_wait_us;
            observation.generation_provider_latency_us = deadline.generation_provider_latency_us;
            observation.generation_commit_delay_us = deadline.generation_commit_delay_us;
        }

        if let Some(trace) = &handled.generation_trace {
            observation.llm_calls = trace.llm_calls.len().min(u32::MAX as usize) as u32;
            observation.tts_calls = u32::from(trace.tts_attempted);
            observation.cancelled = trace.cancelled_stage.is_some();
            if let Some(reason) = fallback_reason_name(trace.fallback_reason) {
                observation.fallback_reason = Some(reason.to_owned());
            }
            observation.budget_denial_reason = trace.budget_denial_reason.clone();
            // #367: audit data only; absent usage stays `None`, never zero.
            for usage in trace.llm_calls.iter().filter_map(|call| call.usage) {
                observation.add_thinking_usage(
                    u64::from(usage.prompt_tokens),
                    u64::from(usage.completion_tokens),
                    usage.cost_micro_usd,
                );
            }
            if matches!(
                handled.route,
                RouteClass::CachedFallback | RouteClass::NonVerbalFallback
            ) && let Some(reason) = fallback_reason_name(trace.fallback_reason)
            {
                self.generation_fallbacks
                    .note(reason, handled.playback.is_some());
                self.session_log_fallback(event_id, reason, handled.playback.is_some(), at_ms);
            }
        }

        // #463: how the turn was answered (a closed vocabulary, never text).
        if self.session_log_composed() {
            let fallback = observation.fallback_reason.clone();
            self.session_log_route(
                event_id,
                observation.route,
                observation.cache_level,
                fallback.as_deref(),
                at_ms,
            );
        }
        self.telemetry.record(observation);
        self.commit_causal_trace(event_id, handled);
    }

    /// Build and retain the privacy-safe causal trace for one handled event
    /// (#65): stage chain from ingress through dispatch, deterministic
    /// identity separate from observational timing, no payload content.
    fn commit_causal_trace(&mut self, event_id: &str, handled: &HandledEvent) {
        let correlation_id = handled
            .correlation_id
            .clone()
            .unwrap_or_else(|| event_id.to_owned());
        let mut trace = self.causal_traces.begin_trace(event_id, correlation_id);

        // Ingress/security admission: byte size only, never content.
        let ingress = trace.push_span(
            TraceStage::IngressNormalization,
            StageOutcome::Completed,
            StageReason::None,
            0,
            0,
        );
        if let Some(bytes) = handled.admitted_bytes {
            trace.with_bytes(ingress, bytes);
        }
        trace.push_span(
            TraceStage::SecurityAdmission,
            StageOutcome::Completed,
            StageReason::None,
            0,
            0,
        );

        // Retrieval/reflex stages from the decision evidence, when present.
        if let Some(decision) = &handled.decision {
            trace.push_span(
                TraceStage::CacheLookup,
                if decision.evidence.retrieval.candidates.is_empty() {
                    StageOutcome::Skipped
                } else {
                    StageOutcome::Completed
                },
                if decision.evidence.retrieval.candidates.is_empty() {
                    StageReason::CacheMiss
                } else {
                    StageReason::None
                },
                0,
                0,
            );
            let retrieval = trace.push_span(
                TraceStage::SemanticRetrieval,
                StageOutcome::Completed,
                StageReason::None,
                0,
                0,
            );
            trace.with_count(
                retrieval,
                decision
                    .evidence
                    .retrieval
                    .candidates
                    .len()
                    .min(u32::MAX as usize) as u32,
            );
            let jev = trace.push_span(
                TraceStage::ReflexJev,
                StageOutcome::Completed,
                StageReason::None,
                0,
                u64::from(decision.evidence.model.attempts),
            );
            let _ = jev;
        }

        // Template resolution / output gate / generation per route.
        if handled.template_fallback.is_some() {
            trace.push_span(
                TraceStage::TemplateResolution,
                StageOutcome::Degraded,
                StageReason::None,
                0,
                0,
            );
        }
        if let Some(generation_trace) = &handled.generation_trace {
            let cancelled = generation_trace.cancelled_stage;
            if let Some(stage) = cancelled {
                let reason = match stage {
                    aivtuber_generative::CancellationStage::BeforeThinking
                    | aivtuber_generative::CancellationStage::AfterThinking
                    | aivtuber_generative::CancellationStage::AfterTts
                    | aivtuber_generative::CancellationStage::BeforePublish => {
                        StageReason::GenerationCancelled
                    }
                };
                trace.push_span(
                    TraceStage::Generation,
                    StageOutcome::Cancelled,
                    reason,
                    0,
                    0,
                );
                trace.record_cancellation(reason, 0);
            } else {
                let outcome = if fallback_reason_name(generation_trace.fallback_reason).is_none() {
                    StageOutcome::Completed
                } else {
                    StageOutcome::Degraded
                };
                let generation = trace.push_span(
                    TraceStage::Generation,
                    outcome,
                    StageReason::None,
                    0,
                    handled
                        .generation_latency_us
                        .map(|micros| micros / 1_000)
                        .unwrap_or(0),
                );
                trace.with_count(
                    generation,
                    generation_trace.llm_calls.len().min(u32::MAX as usize) as u32,
                );
                if let Some(backend) = &generation_trace.tts_backend {
                    trace.with_provider(generation, backend.backend.name.clone());
                }
            }
        }

        // Dispatch stages: playback presence implies scheduler admission and
        // audio dispatch; degraded subsystems surface as failed stages.
        if handled.playback.is_some() {
            trace.push_span(
                TraceStage::SchedulerAdmission,
                StageOutcome::Completed,
                StageReason::None,
                0,
                0,
            );
        }
        let audio_degraded = self.health.audio_error.is_some();
        trace.push_span(
            TraceStage::AudioDispatch,
            if audio_degraded {
                StageOutcome::Failed
            } else if handled.playback.is_some() {
                StageOutcome::Completed
            } else {
                StageOutcome::Skipped
            },
            if audio_degraded {
                StageReason::AdapterError
            } else {
                StageReason::None
            },
            0,
            0,
        );

        // Deadline exhaustion appears in the same causal chain.
        if let Some(reason) = handled
            .deadline
            .as_ref()
            .and_then(|deadline| deadline.exhaustion_reason)
        {
            let stage_reason = match reason {
                DeadlineExhaustionReason::Expired => StageReason::DeadlineExhausted,
                DeadlineExhaustionReason::InsufficientBudget => {
                    StageReason::DeadlineInsufficientBudget
                }
                DeadlineExhaustionReason::ProviderTimeout => StageReason::DeadlineExhausted,
                DeadlineExhaustionReason::Cancelled => StageReason::GenerationCancelled,
                DeadlineExhaustionReason::StaleCompletion => StageReason::DeadlineExhausted,
            };
            trace.record_cancellation(stage_reason, 0);
        }

        self.causal_traces.commit(trace);
    }

    pub fn tick(&mut self, now_ms: u64) {
        self.drain_phrase_messages(now_ms);
        self.drain_generation_completions(now_ms);
        self.drain_shadow_completions();
        if let Some(governor) = self.budget.as_mut() {
            governor.tick(now_ms);
        }
        // A command that is already too late to be dispatched is a speech unit
        // whose audio never reaches the speaker: record that before the
        // timeline advance can report it finished (ADR-0004 truth rules).
        self.account_undeliverable_audio(now_ms);
        self.performer.scheduler_mut().advance_to(now_ms);
        self.reconcile_early_audio(now_ms);
        self.update_phrase_progress();
        self.dispatch_audio(now_ms);
        self.dispatch_avatar(now_ms);
        self.service_face_settle(now_ms);
        self.drive_resynthesis();
        self.pump_speech_lifecycle();
    }

    /// Report, to the speech lifecycle, the pending audio commands that
    /// `dispatch_audio` is about to drop as late or muted.
    fn account_undeliverable_audio(&mut self, now_ms: u64) {
        let muted = self.security.is_muted();
        for command in &self.pending_audio.commands {
            if command.at_ms > now_ms {
                continue;
            }
            let kind = if now_ms.saturating_sub(command.at_ms) > self.max_dispatch_lateness_ms {
                Some(OutputFailureKind::StaleOrLate)
            } else if muted {
                Some(OutputFailureKind::Muted)
            } else {
                None
            };
            if let Some(kind) = kind {
                self.performer.scheduler_mut().report_speech_output(
                    command.generation,
                    OutputObservation::Failed(kind),
                    now_ms,
                );
            }
        }
    }

    /// Apply a finished re-synthesis and start the next queued one, if any
    /// (ADR-0004 D-4). One job at a time, off the composition thread; every
    /// failure only marks the request failed, so a stale asset stays playable.
    fn drive_resynthesis(&mut self) {
        let Some(generative) = self.generative.as_mut() else {
            return;
        };
        if let Some(done) = generative.executor.resynthesis.poll() {
            let applied = match done.result {
                Ok(asset) => self
                    .performer
                    .assets_mut()
                    .replace_generated_asset(asset)
                    .is_ok(),
                Err(_) => false,
            };
            let freshness = self.performer.speech_freshness_mut();
            if applied {
                freshness.complete(&done.asset_id);
            } else {
                freshness.fail(&done.asset_id);
            }
        }
        if generative.executor.resynthesis.is_busy() || generative.is_shutting_down() {
            return;
        }
        let Some(asset_id) = self.performer.speech_freshness_mut().next_request() else {
            return;
        };
        let asset = self.performer.assets_mut().resolve(&asset_id).ok();
        let submitted = asset.is_some_and(|asset| {
            generative
                .executor
                .resynthesis
                .submit((*asset).clone(), self.security.public_output_policy())
        });
        if !submitted {
            self.performer.speech_freshness_mut().fail(&asset_id);
        }
    }

    /// Operator action (ADR-0004 D-4): force every generated asset whose
    /// display text contains the lexicon `surface` to be re-synthesized before
    /// it is played again. Composition-root and startup configuration only; no
    /// event, chat or model path reaches it.
    pub fn invalidate_speech_surface(
        &mut self,
        surface: &str,
    ) -> aivtuber_runtime::SurfaceInvalidation {
        self.performer.invalidate_speech_surface(surface)
    }

    /// Settle the face back to `baseline_preset` at `baseline_intensity` after
    /// a reply (#447). Off unless composed; see [`face_settle::FaceSettle`] for
    /// the guards and why this bypasses the scheduler.
    pub fn with_face_settle(
        mut self,
        baseline_preset: impl Into<String>,
        baseline_intensity: f64,
    ) -> Self {
        self.face_settle = Some(face_settle::FaceSettle::new(
            baseline_preset,
            baseline_intensity,
        ));
        self
    }

    /// Execute the settle step due at `now_ms`, directly on the avatar output,
    /// when the performer is idle (nothing queued or playing, no phrased reply
    /// still to commit) and not muted. Runs after the tick's drain and advance,
    /// so a reply committed this tick is already in the scheduler.
    fn service_face_settle(&mut self, now_ms: u64) {
        if self.face_settle.is_none() {
            return;
        }
        if self.security.is_muted() {
            if let Some(settle) = self.face_settle.as_mut() {
                settle.clear();
            }
            return;
        }
        let busy = self
            .performer
            .scheduler()
            .items()
            .iter()
            .any(|item| matches!(item.status, Status::Queued | Status::Playing))
            || self
                .generative
                .as_ref()
                .is_some_and(|generative| generative.phrase.has_active_spoken_run());
        if busy {
            return;
        }
        let idle_since_ms = self.performer.scheduler().last_terminal_at().unwrap_or(0);
        let graded = self.avatar.graded_expression();
        let Some(step) = self
            .face_settle
            .as_mut()
            .and_then(|settle| settle.due(idle_since_ms, now_ms, graded))
        else {
            return;
        };
        match self.avatar.execute(&AvatarPlaybackCommand::Expression {
            generation: 0,
            at_ms: step.at_ms,
            preset: step.preset,
            intensity: step.intensity,
        }) {
            Ok(()) => self.health.avatar_error = None,
            Err(error) => self.health.avatar_error = Some(error.to_string()),
        }
    }

    /// Opt in to retaining speech lifecycle events for replay (#222 S3): the
    /// app drains the scheduler's queue at every tick and records each event
    /// (digest and projection identity only, never text), newest `capacity`
    /// kept. Read them with [`Self::speech_lifecycle_records`].
    pub fn with_speech_lifecycle_recorder(mut self, capacity: usize) -> Self {
        self.speech_lifecycle_recorder =
            Some(aivtuber_runtime::SpeechLifecycleRecorder::new(capacity));
        self
    }

    /// Retained lifecycle records, oldest first; empty when not recording.
    pub fn speech_lifecycle_records(&self) -> Vec<aivtuber_domain::SpeechLifecycleEvent> {
        self.speech_lifecycle_recorder
            .as_ref()
            .map(|recorder| recorder.records().cloned().collect())
            .unwrap_or_default()
    }

    pub fn speech_lifecycle_recorder_metrics(
        &self,
    ) -> Option<aivtuber_runtime::LifecycleRecorderMetrics> {
        self.speech_lifecycle_recorder.as_ref().map(|r| r.metrics())
    }

    /// Floor-side view of the performer's own speech, derived from lifecycle
    /// events only; `None` when the floor is not enabled.
    pub fn speech_floor_own(&self) -> Option<aivtuber_domain::OwnSpeech> {
        self.speech_floor.as_ref().map(|f| f.own_tracker().own())
    }

    /// The internal lifecycle consumer (#222 S3): drains the scheduler's
    /// queue, in `seq` order, into the floor's own-speech tracker and the
    /// replay recorder, whichever are composed. Does nothing when neither is,
    /// so an external consumer of [`Self::drain_speech_lifecycle`] keeps the
    /// whole queue. A `seq` gap is repaired from the snapshot.
    fn pump_speech_lifecycle(&mut self) {
        let track_playback = self.speech_playback_tracked();
        if self.speech_floor.is_none()
            && self.speech_lifecycle_recorder.is_none()
            && !track_playback
        {
            return;
        }
        let events = self
            .performer
            .scheduler_mut()
            .drain_speech_lifecycle(usize::MAX);
        for event in &events {
            if track_playback {
                self.speech_playback.observe(event);
            }
            if let Some(floor) = self.speech_floor.as_mut() {
                floor.own_tracker_mut().observe(event);
            }
            if let Some(recorder) = self.speech_lifecycle_recorder.as_mut() {
                // A refusal is counted by the recorder; it never stops the performer.
                let _ = recorder.record(event);
            }
        }
        if let Some(floor) = self.speech_floor.as_mut()
            && floor.own_tracker().needs_resync()
        {
            let snapshot = self.performer.scheduler().speech_lifecycle().snapshot();
            floor.own_tracker_mut().resync(&snapshot);
        }
    }

    /// Remove and return up to `max` queued speech lifecycle events in `seq`
    /// order (ADR-0004 D5). A consumer that sees a `seq` gap re-synchronizes
    /// from [`Self::speech_lifecycle_snapshot`].
    pub fn drain_speech_lifecycle(
        &mut self,
        max: usize,
    ) -> Vec<aivtuber_domain::SpeechLifecycleEvent> {
        self.performer.scheduler_mut().drain_speech_lifecycle(max)
    }

    pub fn speech_lifecycle_snapshot(&self) -> aivtuber_scheduler::SpeechLifecycleSnapshot {
        self.performer.scheduler().speech_lifecycle().snapshot()
    }

    fn dispatch_audio(&mut self, now_ms: u64) {
        let mut keep = Vec::new();
        let mut commands = std::mem::take(&mut self.pending_audio.commands);
        commands.sort_by_key(|command| command.at_ms);

        // A continuation phrase is handed to a gapless backend a bounded
        // lookahead before its logical start so the backend can chain it onto
        // the audio in flight (#83 S3d). Only the hand-over moves: `at_ms`, the
        // scheduler's `Started` and the avatar stay on the logical clock, and
        // `Accepted` below never emits `Started` (ADR-0004 D-6).
        let lookahead_ms = if self.audio.gapless_queue() {
            AUDIO_LOOKAHEAD_MS
        } else {
            0
        };
        for command in commands {
            let early_ms = if command.continuation {
                lookahead_ms
            } else {
                0
            };
            if command.at_ms > now_ms.saturating_add(early_ms) {
                keep.push(command);
                continue;
            }
            if !command_is_live(
                self.performer.scheduler(),
                command.generation,
                command.at_ms,
                now_ms,
                early_ms,
                self.max_dispatch_lateness_ms,
            ) {
                continue;
            }
            if self.security.is_muted() {
                continue;
            }

            match self.audio.execute(&command) {
                Ok(()) => {
                    self.health.audio_error = None;
                    self.performer.scheduler_mut().report_speech_output(
                        command.generation,
                        OutputObservation::Accepted,
                        now_ms,
                    );
                    if now_ms >= command.at_ms {
                        self.record_spoken_text(&command);
                    } else if self.early_audio.len() < MAX_EARLY_AUDIO_COMMANDS {
                        // lookahead による先行受け渡し中は論理 Started に達してから記録する (#354)
                        self.early_audio.push(command);
                    }
                }
                Err(error) => {
                    self.health.audio_error = Some(error.to_string());
                    self.performer.scheduler_mut().report_speech_output(
                        command.generation,
                        OutputObservation::Failed(OutputFailureKind::AdapterError),
                        now_ms,
                    );
                    self.mark_degraded(&command.event_id, DegradedSubsystem::Audio);
                }
            }
        }
        self.pending_audio.commands = keep;
    }

    /// 先行受け渡し済みのコマンドを論理時刻およびスケジューラ状態と照合する (#354)。
    /// 論理開始に到達したものだけ発話テキストを記録し、開始前にキャンセルされたものは
    /// オーディオバックエンドからリコールする。
    fn reconcile_early_audio(&mut self, now_ms: u64) {
        if self.early_audio.is_empty() {
            return;
        }
        let commands = std::mem::take(&mut self.early_audio);
        let mut keep = Vec::new();
        for command in commands {
            if self.security.is_muted() {
                let _ = self.audio.recall_queued(command.generation);
                continue;
            }
            if let Some(item) = self
                .performer
                .scheduler()
                .live_item_by_generation(command.generation)
            {
                if item
                    .cancel_at_ms
                    .is_some_and(|cancel| command.at_ms >= cancel)
                {
                    let _ = self.audio.recall_queued(command.generation);
                } else if now_ms >= command.at_ms {
                    self.record_spoken_text(&command);
                } else {
                    keep.push(command);
                }
            } else {
                let started_in_history = now_ms >= command.at_ms
                    && self
                        .performer
                        .scheduler()
                        .history()
                        .filter(|entry| entry.item.plan.generation == command.generation)
                        .last()
                        .is_some_and(|entry| {
                            entry.item.status == Status::Completed
                                || entry
                                    .item
                                    .cancel_at_ms
                                    .is_some_and(|cancel| cancel > command.at_ms)
                        });
                if started_in_history {
                    self.record_spoken_text(&command);
                } else {
                    let _ = self.audio.recall_queued(command.generation);
                }
            }
        }
        self.early_audio = keep;
    }

    /// Remember the display text of audio the output just accepted, bounded
    /// (see [`RecentReplies`]). A failed resolve records nothing.
    fn record_spoken_text(&mut self, command: &AudioPlaybackCommand) {
        let Ok(asset) = self.performer.assets_mut().resolve(&command.asset_id) else {
            return;
        };
        if let Some(text) = asset.speech.as_ref().and_then(|s| s.text.as_deref()) {
            self.recent_replies.record(&command.event_id, text);
            self.note_recent_turn(&command.event_id, text, command.at_ms);
            let generated = asset
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.generated)
                .unwrap_or(false);
            let origin = if !generated {
                repeat_guard::PhraseOrigin::Cached
            } else if command.event_id.starts_with(initiative::EVENT_ID_PREFIX) {
                repeat_guard::PhraseOrigin::Initiative
            } else {
                repeat_guard::PhraseOrigin::ChatReply
            };
            // Measure only: a spoken phrase is never refused here (#89 S1).
            let repeat = self.repeat_guard.record_spoken(text, origin, command.at_ms);
            // #463: the same text and verdict, when the operator opted in.
            self.session_log_phrase(&command.event_id, generated, text, repeat, command.at_ms);
        }
    }

    /// The text-free repeated-phrase counters (#89).
    pub fn repeat_status(&self) -> RepeatStatus {
        self.repeat_guard.status()
    }

    /// An idle initiative phrase that repeats a recent one must not be
    /// committed (#89). Chat replies are never refused: answering the same
    /// question the same way can be correct. Counts and audits a refusal.
    pub(crate) fn initiative_repeat_refused(
        &mut self,
        event: &EventEnvelope,
        text: &str,
        now_ms: u64,
    ) -> bool {
        if !self.is_initiative_event(event) {
            return false;
        }
        let repeat = self.repeat_guard.check(text, now_ms);
        if repeat == repeat_guard::Repeat::None {
            return false;
        }
        self.repeat_guard.note_suppressed_repeat();
        let record = self.security.redactor_record(
            Some(&event.event_id),
            aivtuber_telemetry::AuditCategory::Generation,
            "initiative_repeat_suppressed",
            format!(
                "repeat={}",
                if repeat == repeat_guard::Repeat::Exact {
                    "exact"
                } else {
                    "near"
                }
            ),
        );
        self.security.push_external_audit(record);
        self.session_log_note(
            &event.event_id,
            "initiative_repeat_suppressed",
            if repeat == repeat_guard::Repeat::Exact {
                "exact"
            } else {
                "near"
            },
            now_ms,
        );
        true
    }

    /// The last few replies the performer spoke (newest last).
    pub fn recent_replies(&self) -> &RecentReplies {
        &self.recent_replies
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
                0,
                self.max_dispatch_lateness_ms,
            ) {
                continue;
            }

            // Mute latches silence and a neutral avatar: no lip-sync or
            // expression may re-animate it after the operator reset.
            if self.security.is_muted() {
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
                Ok(()) => {
                    self.health.avatar_error = None;
                    if let (
                        Some(settle),
                        AvatarPlaybackCommand::Expression {
                            preset, intensity, ..
                        },
                    ) = (self.face_settle.as_mut(), &command)
                    {
                        settle.note_expression(preset, *intensity, at_ms);
                    }
                }
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
        self.handle_control_reporting(command, at_ms)
            .map(|(outcome, _)| outcome)
    }

    /// [`Self::handle_control`] that also returns what an operator stop/mute
    /// did to already-started outputs (#277). `Some` exactly for `stop` and
    /// `mute`.
    ///
    /// Order: the scheduler cancels and pending commands are purged first (as
    /// before), then the single `AudioOutput::stop` primitive is called with a
    /// hard stop (fade 0), then the avatar is reset to neutral. None of it is
    /// behind a feature flag, and none of it depends on the content queue,
    /// the floor, barge-in or any model path. The audio stop is bounded
    /// ([`AUDIO_STOP_BOUND`]) so a stuck backend cannot wedge this handler;
    /// the audit record names the per-output outcome and never claims
    /// success for `Unsupported`, an error or a timeout.
    pub fn handle_control_reporting(
        &mut self,
        command: &AuthenticatedControlCommand,
        at_ms: u64,
    ) -> Result<(ControlOutcome, Option<OperatorStopReport>), AppError> {
        let outcome = self.security.handle_control_with_scheduler(
            command,
            at_ms,
            self.performer.scheduler_mut(),
        )?;
        let is_override = matches!(
            outcome,
            ControlOutcome::Stopped { .. } | ControlOutcome::Muted
        );
        if is_override {
            self.note_phrase_cancel_cause(PhraseCause::OperatorStop);
            self.note_operator_override(at_ms);
        }
        let generation_cancelled = if is_override {
            self.generative
                .as_ref()
                .is_some_and(|generative| generative.cancellation.cancel_active())
        } else {
            false
        };
        if matches!(outcome, ControlOutcome::Stopped { .. }) {
            self.purge_cancelled_pending();
            // Operator stop cancels every held and deferred reply with an
            // explicit outcome; nothing queued before the stop may speak
            // after it.
            if let Some(lane) = self.chat_burst.as_mut() {
                lane.cancel_all(at_ms);
            }
        } else if matches!(outcome, ControlOutcome::Muted) {
            for command in std::mem::take(&mut self.early_audio) {
                let _ = self.audio.recall_queued(command.generation);
            }
        }
        let stop_report = is_override.then(|| {
            let report = OperatorStopReport {
                audio: self.stop_started_audio(0),
                // Generation 0: the reset is executed directly and carries no
                // live generation, as the cancelled ones are no longer live.
                avatar_reset: self.reset_avatar_neutral(0, at_ms),
            };
            let record = self.security.redactor_record(
                Some(&command.event().event_id),
                aivtuber_telemetry::AuditCategory::Authorization,
                report.audit_decision(),
                report.detail(),
            );
            self.security.push_external_audit(record);
            report
        });
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

            // Operator override closes any active causal chain (#65): the
            // cancellation stage joins the same trace as the event stages.
            let mut trace = self.causal_traces.begin_trace(
                command.event().event_id.clone(),
                command.event().correlation_id.clone(),
            );
            trace.record_cancellation(StageReason::OperatorStop, 0);
            self.causal_traces.commit(trace);
        }
        self.tick(at_ms);
        Ok((outcome, stop_report))
    }

    /// End started audio through the one stop primitive, recording a failed
    /// or unconfirmed stop in the adapter health.
    fn stop_started_audio(&mut self, fade_ms: u32) -> AudioStopReport {
        let report = self.audio.stop(fade_ms, AUDIO_STOP_BOUND);
        match &report {
            AudioStopReport::Failed(error) => self.health.audio_error = Some(error.clone()),
            AudioStopReport::Unconfirmed => {
                self.health.audio_error =
                    Some("audio stop did not complete within its bound".to_owned());
            }
            AudioStopReport::Outcome(_) => {}
        }
        report
    }

    /// Return the avatar to neutral through its normal output, executed
    /// directly: the cancelled generation is no longer live, so a scheduled
    /// `Reset` would be (correctly) dropped as cancelled.
    fn reset_avatar_neutral(&mut self, generation: u64, at_ms: u64) -> bool {
        // Every reset path funnels here: no settle step may outlive it (#447).
        if let Some(settle) = self.face_settle.as_mut() {
            settle.clear();
        }
        if let Some(lane) = self.chat_burst.as_mut() {
            lane.clear_ack();
        }
        match self
            .avatar
            .execute(&AvatarPlaybackCommand::Reset { generation, at_ms })
        {
            Ok(()) => true,
            Err(error) => {
                self.health.avatar_error = Some(error.to_string());
                false
            }
        }
    }

    /// Whether the security runtime is latched into the operator mute state.
    pub fn is_muted(&self) -> bool {
        self.security.is_muted()
    }

    /// Current untrusted content queue depth (status snapshot only).
    pub fn content_queue_len(&self) -> usize {
        self.security.content_len()
    }

    /// Current scheduler item count (status snapshot only).
    pub fn scheduler_item_count(&self) -> usize {
        self.performer.scheduler().items().len()
    }

    /// Audit an accepted or rejected operator control action (issue #56).
    ///
    /// Audit records pass through the security redactor and never contain
    /// secret material: callers pass decision/detail strings only.
    pub fn audit_operator_action(
        &mut self,
        request_id: &str,
        action: &str,
        decision: &str,
        accepted: bool,
    ) {
        let record = self.security.redactor_record(
            Some(&format!("operator-{request_id}")),
            aivtuber_telemetry::AuditCategory::Authorization,
            if accepted {
                "operator_accepted"
            } else {
                "operator_rejected"
            },
            format!("action={action} decision={decision}"),
        );
        self.security.push_external_audit(record);
    }

    pub fn shutdown(&mut self, at_ms: u64) {
        if let Some(lane) = self.chat_burst.as_mut() {
            lane.shutdown(at_ms);
        }
        self.session_log_crowd_routes(at_ms);
        if let Some(generative) = self.generative.as_mut() {
            generative.shutdown();
        }
        // #164: join every shadow worker before returning. Shutdown that leaves
        // a shadow worker running cannot satisfy "no shadow work running after
        // shutdown", so this blocks until the pool has actually stopped.
        self.router.shutdown_shadow();
        self.performer
            .scheduler_mut()
            .stop_all_for(at_ms, aivtuber_domain::SpeechCancelReason::Shutdown);
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
        let mut keep_early = Vec::with_capacity(self.early_audio.len());
        for command in std::mem::take(&mut self.early_audio) {
            if command_not_cancelled(
                self.performer.scheduler(),
                command.generation,
                command.at_ms,
            ) {
                keep_early.push(command);
            } else {
                let _ = self.audio.recall_queued(command.generation);
            }
        }
        self.early_audio = keep_early;
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

fn remaining_instant(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

fn millis_to_micros(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some((value * 1_000.0).round().min(u64::MAX as f64) as u64)
}

/// Observed TTS character usage for budget settlement (issue #69): the
/// published/replayed text length when available, else the generated asset
/// text length. Bounded to u64; never content itself.
fn result_text_chars(trace: &GenerationTrace, output: Option<&PublicOutput>) -> u64 {
    let _ = trace;
    output
        .and_then(|output| output.text.as_deref())
        .map(|text| text.chars().count() as u64)
        .unwrap_or(0)
}

/// Derive the budget admission from the trusted #67 deadline class (issue
/// #69): only HighPriority/StrongReaction interactions may spend the
/// high-priority reserve; the class is runtime policy, never an untrusted
/// content field, so non-trusted content can never promote its priority.
fn budget_admission_for(deadline: &InteractionDeadline) -> BudgetAdmission {
    match deadline.class {
        aivtuber_domain::InteractionDeadlineClass::HighPriority
        | aivtuber_domain::InteractionDeadlineClass::StrongReaction => {
            BudgetAdmission::high_priority()
        }
        _ => BudgetAdmission::ordinary(),
    }
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
        FallbackReason::BudgetExhausted => Some("budget_exhausted"),
    }
}

fn avatar_generation(command: &AvatarPlaybackCommand) -> u64 {
    match command {
        AvatarPlaybackCommand::Expression { generation, .. }
        | AvatarPlaybackCommand::Gesture { generation, .. }
        | AvatarPlaybackCommand::Gaze { generation, .. }
        | AvatarPlaybackCommand::Viseme { generation, .. }
        | AvatarPlaybackCommand::Reset { generation, .. } => *generation,
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
            speech_text: None,
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
            compiler_version: aivtuber_asset_store::DEFAULT_COMPILER_VERSION.to_owned(),
            voice_model: None,
            avatar_profile: None,
            viseme_mapping: None,
            motion_library: None,
            speech_projection: None,
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
    lookahead_ms: u64,
    max_lateness_ms: u64,
) -> bool {
    if command_at_ms > now_ms.saturating_add(lookahead_ms)
        || now_ms.saturating_sub(command_at_ms) > max_lateness_ms
    {
        return false;
    }
    command_not_cancelled(scheduler, generation, command_at_ms)
}

#[cfg(test)]
mod test_dir;
#[cfg(test)]
mod tests;
