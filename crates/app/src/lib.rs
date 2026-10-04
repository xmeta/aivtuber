#![forbid(unsafe_code)]

mod budget;
mod deadline;
mod moderation_evidence;
mod operator_control;
mod outputs;
mod profile;
mod raw_ingress;
mod retention;
mod routing;
mod scenario;
mod semantic;
mod shadow;
mod shadow_evidence;
mod shadow_orchestrator;
mod support_bundle;

pub use budget::*;
pub use deadline::*;
pub use moderation_evidence::*;
pub use operator_control::*;
pub use outputs::*;
pub use profile::*;
pub use raw_ingress::*;
pub use retention::*;
pub use routing::*;
pub use scenario::*;
pub use semantic::*;
pub use shadow::*;
pub use shadow_evidence::*;
pub use shadow_orchestrator::*;
pub use support_bundle::*;

use aivtuber_adaptation::{AdaptationEngine, AppliedAdaptation, MemoryEntry, WorkingMemory};
use aivtuber_asset_store::CacheTier;
use aivtuber_domain::{
    AuthenticatedControl, AuthenticatedControlCommand, AuthorizedStreamAction,
    DeadlineExhaustionReason, DeadlineStage, EngineError, EventEnvelope, FallbackReason,
    InteractionDeadline,
};
use aivtuber_generative::{
    CancellationStage, FallbackDirective, GeneratedTextGateDecision,
    GenerationCancellationRegistry, GenerationCancellationToken, GenerationDisposition,
    GenerationRequest, GenerationResult, GenerationTrace, GenerativePipeline,
    select_fallback_directive,
};
use aivtuber_reflex::{DecisionReplayRecord, ExecutedAction};
use aivtuber_runtime::{
    AudioPlaybackCommand, AudioPlaybackSink, AvatarPlaybackCommand, AvatarPlaybackSink,
    CachedAssetSelection, CachedPerformer, CachedPlaybackError, CachedPlaybackOutcome,
    CachedPlaybackTiming, ContentAdmitDecision, ControlOutcome, FastPathPreloadReport,
    LocalVisemeStore, OutputVerdict, PublicOutput, PublicOutputPolicy, RuntimeError,
    SecurityRuntime,
};
use aivtuber_scheduler::{Scheduler, Status};
use aivtuber_telemetry::{
    CacheLevel, CausalTraceCollector, CausalTraceRetentionConfig, ComparisonMode,
    DegradedSubsystem, EventObservation, RouteClass, StageOutcome, StageReason, TelemetryCollector,
    TelemetryRetentionConfig, TraceStage,
};
use std::error::Error;
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

struct GenerationWork {
    generation_id: u64,
    request: GenerationRequest,
    cancellation: GenerationCancellationToken,
    fallback: FallbackDirective,
    output_policy: PublicOutputPolicy,
    submitted_at: Instant,
    provider_deadline: Instant,
    context: GenerationCompletionContext,
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

                    let mut output_decision = None;
                    let result = pipeline
                        .run_with_output_gate_and_fallback_deadline(
                            &work.request,
                            &work.cancellation,
                            work.fallback.clone(),
                            Some(work.provider_deadline),
                            |generated_text| {
                                let output = work.output_policy.evaluate(generated_text);
                                let decision = if matches!(
                                    output.verdict,
                                    OutputVerdict::Allow | OutputVerdict::Redact
                                ) {
                                    output
                                        .text
                                        .clone()
                                        .map(GeneratedTextGateDecision::Publish)
                                        .unwrap_or(GeneratedTextGateDecision::Reject)
                                } else {
                                    GeneratedTextGateDecision::Reject
                                };
                                output_decision = Some(output);
                                decision
                            },
                        )
                        .map_err(|error| error.to_string());

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
        Ok(Self {
            executor: GenerationExecutor::new(pipeline, config)?,
            cancellation: Arc::new(GenerationCancellationRegistry::default()),
            next_generation_id: 0,
            active_generation_id: None,
        })
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
        let cancellation = GenerationCancellationToken::default();
        let work = GenerationWork {
            generation_id,
            request,
            cancellation: cancellation.clone(),
            fallback,
            output_policy,
            submitted_at: Instant::now(),
            provider_deadline,
            context,
        };
        match self.executor.try_submit(work) {
            Ok(()) => {
                self.cancellation.activate(&source_event, cancellation);
                self.active_generation_id = Some(generation_id);
                Ok(generation_id)
            }
            Err(error) => {
                cancellation.cancel();
                Err(error)
            }
        }
    }

    fn is_current(&self, generation_id: u64, event_id: &str) -> bool {
        self.active_generation_id == Some(generation_id)
            && self.cancellation.active_event_id().as_deref() == Some(event_id)
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

    fn note_cancelled_or_stale(&self) {
        self.executor.note_cancelled_or_stale();
    }

    fn note_failed(&self) {
        self.executor.note_failed();
    }

    fn shutdown(&mut self) {
        self.cancellation.cancel_active();
        self.active_generation_id = None;
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
    audio: Box<dyn AudioOutput>,
    avatar: Box<dyn AvatarOutput>,
    stream: Box<dyn StreamOutput>,
    pending_audio: PendingAudio,
    pending_avatar: PendingAvatar,
    max_dispatch_lateness_ms: u64,
    interaction_deadlines: InteractionDeadlinePolicy,
    /// Generative resource budget governor (issue #69); `None` when the
    /// budget feature is disabled (default) so existing behavior is
    /// untouched.
    budget: Option<GenerativeBudgetGovernor>,
    health: AdapterHealth,
    telemetry: TelemetryCollector,
    causal_traces: CausalTraceCollector,
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
            audio,
            avatar,
            stream,
            pending_audio: PendingAudio::default(),
            pending_avatar: PendingAvatar::default(),
            max_dispatch_lateness_ms,
            interaction_deadlines: InteractionDeadlinePolicy::default(),
            budget: None,
            health: AdapterHealth::default(),
            telemetry: TelemetryCollector::default(),
            causal_traces: CausalTraceCollector::default(),
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
            });
        }

        let event = self.security.pop_content().ok_or_else(|| {
            AppError::Routing("queued content was unavailable for dispatch".to_owned())
        })?;
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
        let route = match self.router.route_with_budget(&event, Some(route_budget)) {
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
                        directive: fallback,
                    },
                },
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
                                directive: fallback,
                            },
                        },
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
                            directive: fallback,
                        },
                    },
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

    fn commit_generation_result(
        &mut self,
        event: &EventEnvelope,
        result: GenerationResult,
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
                    return Ok(HandledGeneration {
                        playback: None,
                        route: RouteClass::NonVerbalFallback,
                        trace,
                        cache_level: None,
                    });
                }

                let generated_text = generated_text.expect("checked above");
                let rechecked = self
                    .security
                    .public_output_policy()
                    .evaluate(generated_text);
                let recheck_allows_same_text = matches!(
                    rechecked.verdict,
                    OutputVerdict::Allow | OutputVerdict::Redact
                ) && rechecked.text.as_deref()
                    == Some(generated_text);

                if !recheck_allows_same_text {
                    self.security.record_public_output_decision(&rechecked);
                    trace.fallback_reason = FallbackReason::PolicyOverride;
                    return Ok(HandledGeneration {
                        playback: None,
                        route: RouteClass::NonVerbalFallback,
                        trace,
                        cache_level: None,
                    });
                }

                self.record_worker_output_decision(output_decision.as_ref());

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
                Ok(HandledGeneration {
                    playback: Some(outcome),
                    route: RouteClass::Generated,
                    trace,
                    cache_level: Some(CacheLevel::Generated),
                })
            }
            GenerationDisposition::Fallback { directive } => {
                self.record_worker_output_decision(output_decision.as_ref());
                match directive {
                    FallbackDirective::CachedReaction { asset_id, .. } => {
                        let outcome = self.performer.handle_asset_id(
                            event,
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
                }
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
                if trace.cancelled_stage.is_none() {
                    trace.cancelled_stage = Some(CancellationStage::BeforePublish);
                }
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

                let handled = HandledEvent {
                    playback: None,
                    route: RouteClass::Generated,
                    routing_latency_us: completion.context.routing_latency_us,
                    generation_latency_us: Some(completion.generation_latency_us),
                    generation_trace: Some(trace),
                    decision: completion.context.decision,
                    template_fallback: completion.context.template_fallback,
                    cache_level: None,
                    deadline: Some(deadline),
                    deferred_generation: false,
                    admitted_bytes: None,
                    correlation_id: Some(completion.context.correlation_id.clone()),
                };
                self.record_handled_event(&event_id, &handled, now_ms);

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
                            directive: completion.fallback,
                        },
                    }
                }
            };

            let handled_generation = self.commit_generation_result(
                &completion.request.source_event,
                result,
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
        self.drain_generation_completions(now_ms);
        self.drain_shadow_completions();
        if let Some(governor) = self.budget.as_mut() {
            governor.tick(now_ms);
        }
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
        Ok(outcome)
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
        if let Some(generative) = self.generative.as_mut() {
            generative.shutdown();
        }
        // #164: join every shadow worker before returning. Shutdown that leaves
        // a shadow worker running cannot satisfy "no shadow work running after
        // shutdown", so this blocks until the pool has actually stopped.
        self.router.shutdown_shadow();
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
    use aivtuber_adaptation::{ActorPseudonymizer, PromotionPolicy, WorkingMemoryConfig};
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
                ..CachedPlaybackConfig::default()
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
            *generation == plan.generation
                && *at_ms >= plan.start_at_ms
                && *at_ms <= plan.end_at_ms()
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

    fn wait_until<R>(
        app: &ProductionApp<R>,
        condition: impl Fn(GenerationExecutionSnapshot) -> bool,
    ) where
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
            OperatorStatusSnapshot::from_app(&app).generation_active,
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
        .with_causal_trace_retention(aivtuber_telemetry::CausalTraceRetentionConfig {
            max_traces: 2,
        });
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

    /// Issue #55: profile selection is covered by integration tests using
    /// mocks — each profile must compose the route graph its capabilities
    /// advertise, and validate() must reject misleading configurations.
    mod profile_composition {
        use super::*;

        fn profile_app() -> ProductionApp<Box<dyn RoutePlanner>> {
            app(
                Box::new(FixedSilentRoute) as Box<dyn RoutePlanner>,
                Box::new(RecordingAudio::default()),
                Box::new(RecordingAvatar::default()),
                Box::new(NoopStreamOutput),
            )
        }

        #[test]
        fn cached_profile_composes_intent_planner_behind_dyn_planner() {
            // The cached profile composes the deterministic IntentRoutePlanner
            // behind the same `Box<dyn RoutePlanner>` the executable uses.
            let boxed: Box<dyn RoutePlanner> = Box::new(IntentRoutePlanner);
            let mut app = app(
                boxed,
                Box::new(RecordingAudio::default()),
                Box::new(RecordingAvatar::default()),
                Box::new(NoopStreamOutput),
            );
            app.startup().expect("startup");

            let mut event = chat_event(70);
            event.payload.insert(
                "intent".to_owned(),
                serde_json::Value::String("reaction.agree".to_owned()),
            );
            let raw = serde_json::to_vec(&event).expect("event json");
            let outcome = app
                .process_content_bytes(&raw, 0, 70)
                .expect("cached profile routing");
            // IntentRoutePlanner routes by intent only; without a matching
            // cached asset resolution the route degrades deterministically.
            let _ = outcome.playback;
        }

        #[test]
        fn box_dyn_route_planner_forwards_decision_and_fallback_accessors() {
            let mut boxed: Box<dyn RoutePlanner> = Box::new(template_planner());
            let mut event = chat_event(71);
            event.payload.insert(
                "template_id".to_owned(),
                serde_json::Value::String("thanks.donation".to_owned()),
            );
            event.payload.insert(
                "name".to_owned(),
                serde_json::Value::String("テスター".to_owned()),
            );
            // The forwarding impl must expose the same accessors as the inner
            // planner so executables can treat either composition uniformly.
            assert!(boxed.route(&event).is_ok());
            assert!(boxed.decision_record().is_some());
            assert_eq!(boxed.template_fallback(), None);
        }

        #[test]
        fn profile_fingerprint_is_recorded_for_each_profile() {
            for profile in CompositionProfile::ALL {
                let fingerprint = config_fingerprint(profile, profile.generative_route());
                assert!(fingerprint.starts_with("profile-v1-"));
                let summary = ProfileSummary::for_profile(
                    profile,
                    profile.generative_route(),
                    fingerprint.clone(),
                );
                assert_eq!(summary.config_fingerprint, fingerprint);
                assert!(summary.log_line().contains(&fingerprint));
            }
        }

        #[test]
        fn profile_validation_gates_match_composed_route_graph() {
            // The executable composes the generative runtime only when the
            // profile can reach it; validate() must enforce the same rule.
            CompositionProfile::Cached
                .validate(false, false)
                .expect("cached needs nothing");
            CompositionProfile::Reflex
                .validate(false, true)
                .expect("reflex needs jev credentials");
            CompositionProfile::Full
                .validate(true, true)
                .expect("full needs jev + generative configuration");
            for profile in [CompositionProfile::Reflex, CompositionProfile::Full] {
                assert!(profile.validate(false, false).is_err());
            }
        }

        #[test]
        fn silent_startup_app_composes_and_shuts_down_under_any_profile_choice() {
            // Every profile shares one ProductionApp; the planner is the only
            // profile-specific component. Verify composition + shutdown works
            // with the boxed planner the executables actually pass.
            let mut app = profile_app();
            app.startup().expect("startup");
            let event = chat_event(72);
            let raw = serde_json::to_vec(&event).expect("event json");
            app.process_content_bytes(&raw, 0, 72).expect("content");
            app.shutdown(1_000);
        }
    }

    // Issue #56: authenticated local operator control.

    mod operator_control_helpers {
        use super::*;

        pub(super) const SECRET: [u8; 32] = [0x5A_u8; 32];

        pub(super) fn operator_ingress() -> LocalControlIngress {
            LocalControlIngress::new(
                "operator-control",
                "operator:local",
                AuthorizationMethod::SignedLocalApi,
                BTreeSet::from([Capability::PerformerStop, Capability::PerformerMute]),
                ControlSecret::new(SECRET),
            )
            .expect("operator ingress")
        }

        pub(super) fn request(action: &str, secret: &[u8]) -> super::OperatorRequest {
            super::OperatorRequest {
                action: action.to_owned(),
                secret: String::from_utf8(secret.to_vec()).expect("utf8 secret"),
                request_id: Some("req-test".to_owned()),
            }
        }

        pub(super) fn dispatched(
            request: super::OperatorRequest,
        ) -> super::DispatchedOperatorRequest {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            super::DispatchedOperatorRequest {
                request,
                respond: tx,
            }
        }
    }

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
        let bundle: serde_json::Value =
            serde_json::from_str(&response.detail).expect("bundle json");
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
        let bundle: serde_json::Value =
            serde_json::from_str(&response.detail).expect("bundle json");
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
}
