use crate::AppError;
use crate::shadow::{
    ShadowComparisonRecord, ShadowDecision, ShadowEvaluationFailure, ShadowEvaluationOutcome,
    ShadowOrchestratorConfig, ShadowPolicy, ShadowPolicyIdentity, ShadowPolicyInput,
    ShadowProvider, ShadowSkipReason, ShadowSubmission, compare_target,
};
use aivtuber_domain::EventEnvelope;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Cancellation for a single unit of shadow work (#164).
///
/// Deliberately independent of `GenerationCancellationToken`: shadow work is
/// not a generation, is not provider-billed by default, and must not be able to
/// cancel or be cancelled by active generation state.
#[derive(Debug, Default, Clone)]
pub struct ShadowCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl ShadowCancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Bounded view of outstanding shadow work, mirroring the #177 generation
/// execution snapshot so operators and tests read both subsystems the same way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowExecutionSnapshot {
    pub pending: usize,
    pub in_flight: usize,
    /// Completed records published to (or blocked on) the completion mailbox and
    /// not yet taken by the composition thread.
    pub awaiting_drain: usize,
    pub pending_high_water: usize,
    pub in_flight_high_water: usize,
    pub submitted: u64,
    pub completed: u64,
    pub dropped_disabled: u64,
    pub dropped_filtered: u64,
    pub dropped_not_sampled: u64,
    pub dropped_saturated: u64,
    pub dropped_shutting_down: u64,
    pub dropped_budget_denied: u64,
    pub cancelled_or_stale: u64,
    pub deadline_exceeded: u64,
    pub planner_failed: u64,
    pub provider_calls: u64,
    pub provider_calls_blocked: u64,
    pub dropped_completion_mailbox_full: u64,
    pub queue_capacity: usize,
    pub max_concurrent: usize,
    pub shutting_down: bool,
    pub worker_running: usize,
}

impl ShadowExecutionSnapshot {
    /// True when no shadow work is outstanding anywhere: not queued, not
    /// executing in a worker, and published-but-undrained on the completion
    /// mailbox. Same false-quiescent trap #177 fixed for generations.
    pub fn is_quiescent(&self) -> bool {
        self.pending == 0 && self.in_flight == 0 && self.awaiting_drain == 0
    }
}

#[derive(Debug, Default)]
struct ShadowCounters {
    pending: AtomicUsize,
    in_flight: AtomicUsize,
    awaiting_drain: AtomicUsize,
    pending_high_water: AtomicUsize,
    in_flight_high_water: AtomicUsize,
    submitted: AtomicU64,
    completed: AtomicU64,
    dropped_disabled: AtomicU64,
    dropped_filtered: AtomicU64,
    dropped_not_sampled: AtomicU64,
    dropped_saturated: AtomicU64,
    dropped_shutting_down: AtomicU64,
    dropped_budget_denied: AtomicU64,
    cancelled_or_stale: AtomicU64,
    deadline_exceeded: AtomicU64,
    planner_failed: AtomicU64,
    provider_calls: AtomicU64,
    provider_calls_blocked: AtomicU64,
    dropped_completion_mailbox_full: AtomicU64,
}

impl ShadowCounters {
    fn update_high_water(counter: &AtomicUsize, value: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        while value > current {
            match counter.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn note_queued(&self) {
        let value = self.pending.fetch_add(1, Ordering::SeqCst) + 1;
        Self::update_high_water(&self.pending_high_water, value);
    }

    /// Claims in-flight before releasing pending so a job that is dequeued but
    /// not yet started is still counted, matching #177 for generations.
    fn note_claimed(&self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        let value = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        Self::update_high_water(&self.in_flight_high_water, value);
    }

    /// Every claimed job publishes exactly one completion, discarded or not,
    /// so this is the single place in-flight is released.
    fn note_published(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.awaiting_drain.fetch_add(1, Ordering::SeqCst);
    }

    fn snapshot(
        &self,
        config: &ShadowOrchestratorConfig,
        shutting_down: bool,
        workers_running: usize,
    ) -> ShadowExecutionSnapshot {
        ShadowExecutionSnapshot {
            pending: self.pending.load(Ordering::SeqCst),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            awaiting_drain: self.awaiting_drain.load(Ordering::SeqCst),
            pending_high_water: self.pending_high_water.load(Ordering::Relaxed),
            in_flight_high_water: self.in_flight_high_water.load(Ordering::Relaxed),
            submitted: self.submitted.load(Ordering::SeqCst),
            completed: self.completed.load(Ordering::SeqCst),
            dropped_disabled: self.dropped_disabled.load(Ordering::SeqCst),
            dropped_filtered: self.dropped_filtered.load(Ordering::SeqCst),
            dropped_not_sampled: self.dropped_not_sampled.load(Ordering::SeqCst),
            dropped_saturated: self.dropped_saturated.load(Ordering::SeqCst),
            dropped_shutting_down: self.dropped_shutting_down.load(Ordering::SeqCst),
            dropped_budget_denied: self.dropped_budget_denied.load(Ordering::SeqCst),
            cancelled_or_stale: self.cancelled_or_stale.load(Ordering::SeqCst),
            deadline_exceeded: self.deadline_exceeded.load(Ordering::SeqCst),
            planner_failed: self.planner_failed.load(Ordering::SeqCst),
            provider_calls: self.provider_calls.load(Ordering::SeqCst),
            provider_calls_blocked: self.provider_calls_blocked.load(Ordering::SeqCst),
            dropped_completion_mailbox_full: self
                .dropped_completion_mailbox_full
                .load(Ordering::SeqCst),
            queue_capacity: config.queue_capacity,
            max_concurrent: config.max_concurrent,
            shutting_down,
            worker_running: workers_running,
        }
    }
}

/// One unit of admitted shadow work.
///
/// Carries the already-computed active decision so a worker only runs the
/// shadow side: active routing never happens off the composition thread.
struct ShadowJob {
    job_id: u64,
    event: EventEnvelope,
    remaining_budget: Option<Duration>,
    deadline: Instant,
    cancellation: ShadowCancellationToken,
    active_identity: ShadowPolicyIdentity,
    shadow_identity: ShadowPolicyIdentity,
    active: ShadowDecision,
}

enum JobOutcome {
    Record(Box<ShadowComparisonRecord>),
    /// Discarded without producing evidence: superseded or shut down
    /// mid-flight. Still reported to the drain so a budget reservation held for
    /// the job is released rather than leaked.
    Discarded,
}

/// Bounded MPMC work queue for the shadow pool.
///
/// A `std::sync::mpsc` receiver cannot be shared across workers without a lock,
/// and blocking in `recv()` while holding that lock would serialize the whole
/// pool: one idle worker would prevent every other worker from taking a job,
/// which would make `max_concurrent` unobservable. A mutex + condvar queue
/// admits waiters without blocking any of them.
struct ShadowQueue {
    state: Mutex<VecDeque<ShadowJob>>,
    available: Condvar,
    capacity: usize,
    closed: AtomicBool,
}

impl ShadowQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(VecDeque::with_capacity(capacity)),
            available: Condvar::new(),
            capacity,
            closed: AtomicBool::new(false),
        }
    }

    /// Non-blocking admission. `None` means the bounded queue is full.
    fn push(&self, job: ShadowJob) -> Option<()> {
        let mut queue = self.state.lock().expect("shadow queue lock");
        if queue.len() >= self.capacity {
            return None;
        }
        queue.push_back(job);
        drop(queue);
        self.available.notify_one();
        Some(())
    }

    /// Block until a job is available, returning `None` once the queue is
    /// closed and drained.
    fn pop(&self) -> Option<ShadowJob> {
        let mut queue = self.state.lock().expect("shadow queue lock");
        loop {
            if let Some(job) = queue.pop_front() {
                return Some(job);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            queue = self.available.wait(queue).expect("shadow queue wait");
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.available.notify_all();
    }

    /// Whether a job could be admitted right now, without taking one.
    ///
    /// Lets the composition thread consult the #69 governor *before* a job is
    /// queued, so a denial is a real admission decision rather than a
    /// bookkeeping edit applied to work that is already running.
    fn has_room(&self) -> bool {
        self.state.lock().expect("shadow queue lock").len() < self.capacity
    }
}

/// One finished shadow job.
///
/// Every admitted job produces exactly one completion, whether or not it
/// yielded evidence, so callers holding per-job resources (the #69 budget
/// reservation) always see the job finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowCompletion {
    pub job_id: u64,
    /// `None` when the job was superseded or shut down before it could decide.
    pub record: Option<ShadowComparisonRecord>,
}

impl ShadowCompletion {
    pub fn discarded(job_id: u64) -> Self {
        Self {
            job_id,
            record: None,
        }
    }
}

/// Bounded production-compatible shadow orchestration (#164).
///
/// Work never blocks the composition thread. Admission is filtered/sampled
/// deterministically, the queue and worker pool are bounded, every job carries
/// its own deadline and cancellation token, and the completion mailbox is
/// bounded so evidence cannot accumulate without being drained.
pub struct ShadowOrchestrator<S>
where
    S: ShadowPolicy,
{
    config: ShadowOrchestratorConfig,
    /// The configured policy instance. Workers each own a `Clone` of it, so
    /// evaluation genuinely runs concurrently rather than serializing behind a
    /// shared lock; this field is the template and the diagnostic identity of
    /// the pool's policy.
    policy: S,
    provider: Option<Arc<dyn ShadowProvider>>,
    work: Arc<ShadowQueue>,
    completion_rx: mpsc::Receiver<ShadowCompletion>,
    counters: Arc<ShadowCounters>,
    shutting_down: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    next_job_id: u64,
    /// One-shot #69 budget refusal for the next submission (#164).
    ///
    /// Armed by the composition thread *before* it routes, so the governor is
    /// the admission authority: a refused event is never queued, never
    /// executed, and never needs cancelling after the fact.
    pending_budget_denial: bool,
    last_submission: Option<ShadowSubmission>,
    last_comparison: Option<ShadowComparisonRecord>,
    /// Token of the most recently admitted job. Superseded when a newer job is
    /// admitted so a stale evaluation cannot outlive the work it compares.
    active_token: Option<ShadowCancellationToken>,
}

impl<S> ShadowOrchestrator<S>
where
    S: ShadowPolicy + Clone + 'static,
{
    pub fn new(
        policy: S,
        provider: Option<Arc<dyn ShadowProvider>>,
        config: ShadowOrchestratorConfig,
    ) -> Result<Self, AppError> {
        config.validate()?;
        let work = Arc::new(ShadowQueue::new(config.queue_capacity));
        // Every queued job plus every executing job can publish before the
        // composition thread drains the mailbox. Bound it to that maximum.
        let completion_capacity = config.queue_capacity.saturating_add(config.max_concurrent);
        let (completion_tx, completion_rx) =
            mpsc::sync_channel::<ShadowCompletion>(completion_capacity);
        let counters = Arc::new(ShadowCounters::default());
        let shutting_down = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(config.max_concurrent);
        for index in 0..config.max_concurrent {
            // One policy instance per worker: a shared lock would make
            // `max_concurrent` unobservable and turn the bound into a lie.
            let mut worker_policy = policy.clone();
            let worker_provider = provider.clone();
            let worker_config = config.clone();
            let worker_counters = Arc::clone(&counters);
            let worker_shutdown = Arc::clone(&shutting_down);
            let worker_queue = Arc::clone(&work);
            let worker_completion_tx = completion_tx.clone();
            let handle = thread::Builder::new()
                .name(format!("aivtuber-shadow-worker-{index}"))
                .spawn(move || {
                    while let Some(job) = worker_queue.pop() {
                        let job_id = job.job_id;
                        worker_counters.note_claimed();
                        let outcome = run_job(
                            &mut worker_policy,
                            worker_provider.as_deref(),
                            &worker_config,
                            worker_counters.as_ref(),
                            worker_shutdown.as_ref(),
                            job,
                        );
                        let completion = match outcome {
                            JobOutcome::Discarded => {
                                worker_counters
                                    .cancelled_or_stale
                                    .fetch_add(1, Ordering::Relaxed);
                                ShadowCompletion::discarded(job_id)
                            }
                            JobOutcome::Record(record) => ShadowCompletion {
                                job_id,
                                record: Some(*record),
                            },
                        };
                        // Claim the drain window before publishing so a
                        // quiescence check cannot miss a completion that is
                        // already on its way to the mailbox.
                        worker_counters.note_published();
                        // Non-blocking publish: a worker must never wait on the
                        // composition thread, or shutdown could not join it.
                        if worker_completion_tx.try_send(completion).is_err() {
                            worker_counters
                                .awaiting_drain
                                .fetch_sub(1, Ordering::SeqCst);
                            worker_counters
                                .dropped_completion_mailbox_full
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
                .map_err(|error| {
                    AppError::Routing(format!("shadow worker spawn failed: {error}"))
                })?;
            workers.push(handle);
        }
        // Drop the original so the channel closes once every worker exits.
        drop(completion_tx);

        Ok(Self {
            config,
            policy,
            provider,
            work,
            completion_rx,
            counters,
            shutting_down,
            workers,
            next_job_id: 0,
            pending_budget_denial: false,
            last_submission: None,
            last_comparison: None,
            active_token: None,
        })
    }

    /// The configured policy instance the pool was built from. Workers each own
    /// a `Clone`, so this is the template rather than the instance doing work.
    pub fn policy(&self) -> &S {
        &self.policy
    }
}

impl<S> ShadowOrchestrator<S>
where
    S: ShadowPolicy + 'static,
{
    pub fn config(&self) -> &ShadowOrchestratorConfig {
        &self.config
    }

    pub fn provider(&self) -> Option<&Arc<dyn ShadowProvider>> {
        self.provider.as_ref()
    }

    /// Whether shadow work is admitted through the #69 budget ledger (#164).
    pub fn charges_budget(&self) -> bool {
        self.config.charge_shadow_to_budget
    }

    /// Outcome of the most recent submission attempt, so the composition thread
    /// can charge budget for exactly the work that was admitted.
    pub fn last_submission(&self) -> Option<&ShadowSubmission> {
        self.last_submission.as_ref()
    }

    /// Most recent comparison taken from the completion mailbox.
    pub fn last_comparison(&self) -> Option<&ShadowComparisonRecord> {
        self.last_comparison.as_ref()
    }

    /// Take every completion published since the last drain.
    ///
    /// Bounded by the completion mailbox capacity: nothing here can grow
    /// without the composition thread calling it.
    pub fn drain_comparisons(&mut self) -> Vec<ShadowCompletion> {
        let mut drained = Vec::new();
        while let Ok(completion) = self.completion_rx.try_recv() {
            self.counters.awaiting_drain.fetch_sub(1, Ordering::SeqCst);
            self.counters.completed.fetch_add(1, Ordering::Relaxed);
            if let Some(record) = &completion.record {
                self.last_comparison = Some(record.clone());
            }
            drained.push(completion);
        }
        drained
    }

    /// Admit one event for bounded shadow evaluation.
    ///
    /// Never blocks the caller: a saturated queue, a shutting-down orchestrator,
    /// a disabled config, or a filter/sample rejection all return `Skipped`
    /// rather than applying backpressure to active routing.
    pub fn submit(
        &mut self,
        event: &EventEnvelope,
        remaining_budget: Option<Duration>,
        active_identity: &ShadowPolicyIdentity,
        shadow_identity: &ShadowPolicyIdentity,
        active: ShadowDecision,
    ) -> ShadowSubmission {
        if self.pending_budget_denial {
            // Consume the refusal: this attempt is denied, and it was never
            // queued, so nothing needs cancelling or unwinding.
            self.pending_budget_denial = false;
            return self.skip(ShadowSkipReason::BudgetDenied);
        }
        if let Some(reason) = self.config.skip_reason(event) {
            return self.skip(reason);
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return self.skip(ShadowSkipReason::ShuttingDown);
        }
        self.next_job_id = self.next_job_id.saturating_add(1);
        let job_id = self.next_job_id;
        let cancellation = ShadowCancellationToken::default();
        let job = ShadowJob {
            job_id,
            event: event.clone(),
            remaining_budget,
            deadline: Instant::now() + Duration::from_millis(self.config.deadline_ms),
            cancellation: cancellation.clone(),
            active_identity: active_identity.clone(),
            shadow_identity: shadow_identity.clone(),
            active,
        };

        self.counters.note_queued();
        if self.work.push(job).is_none() {
            self.counters.pending.fetch_sub(1, Ordering::SeqCst);
            return self.skip(ShadowSkipReason::Saturated);
        }
        // Supersede the previous in-flight evaluation: the newest event is the
        // one whose comparison is worth keeping.
        if let Some(previous) = self.active_token.replace(cancellation) {
            previous.cancel();
        }
        self.counters.submitted.fetch_add(1, Ordering::Relaxed);
        let submission = ShadowSubmission::Admitted { job_id };
        self.last_submission = Some(submission.clone());
        submission
    }

    fn skip(&mut self, reason: ShadowSkipReason) -> ShadowSubmission {
        let counter = match reason {
            ShadowSkipReason::Disabled => &self.counters.dropped_disabled,
            ShadowSkipReason::Filtered => &self.counters.dropped_filtered,
            ShadowSkipReason::NotSampled => &self.counters.dropped_not_sampled,
            ShadowSkipReason::Saturated => &self.counters.dropped_saturated,
            ShadowSkipReason::ShuttingDown => &self.counters.dropped_shutting_down,
            ShadowSkipReason::BudgetDenied => &self.counters.dropped_budget_denied,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let submission = ShadowSubmission::Skipped { reason };
        self.last_submission = Some(submission.clone());
        submission
    }

    /// Whether the deterministic admission gates (enabled, kind filter, sample)
    /// and the queue capacity would admit this event right now.
    ///
    /// Read-only: it takes no slot, moves no counter, and cannot queue work. The
    /// composition thread uses it to ask the #69 governor first, so a budget
    /// denial stops the job from being queued at all.
    pub fn would_admit(&self, event: &EventEnvelope) -> bool {
        !self.shutting_down.load(Ordering::Acquire)
            && self.config.skip_reason(event).is_none()
            && self.work.has_room()
    }

    /// Refuse the next admission attempt, before it is queued (#164).
    ///
    /// Called by the composition thread after the #69 governor denies the
    /// event. The next `submit` reports `Skipped { BudgetDenied }` without
    /// touching the queue, so denied work is never executed and the recorded
    /// submission matches what actually happened.
    pub fn deny_next_submission(&mut self) {
        self.pending_budget_denial = true;
    }

    pub fn snapshot(&self) -> ShadowExecutionSnapshot {
        let workers_running = self
            .workers
            .iter()
            .filter(|worker| !worker.is_finished())
            .count();
        self.counters.snapshot(
            &self.config,
            self.shutting_down.load(Ordering::Acquire),
            workers_running,
        )
    }

    /// Cancel outstanding work and join every worker.
    ///
    /// Blocking is intentional: the #164 acceptance criterion is that no shadow
    /// work is still running once shutdown returns, which cannot be proven about
    /// a detached worker.
    pub fn shutdown(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        self.work.close();
        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        for worker in std::mem::take(&mut self.workers) {
            let _ = worker.join();
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}

impl<S> Drop for ShadowOrchestrator<S>
where
    S: ShadowPolicy,
{
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        self.work.close();
        if let Some(token) = self.active_token.take() {
            token.cancel();
        }
        // A policy that ignores its cancellation token cannot be joined safely
        // from Drop. The sender is already closed and the shutdown fence keeps
        // late results from being published as new evidence.
        for worker in std::mem::take(&mut self.workers) {
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

fn run_job<S: ShadowPolicy>(
    policy: &mut S,
    provider: Option<&dyn ShadowProvider>,
    config: &ShadowOrchestratorConfig,
    counters: &ShadowCounters,
    shutting_down: &AtomicBool,
    job: ShadowJob,
) -> JobOutcome {
    let ShadowJob {
        event,
        remaining_budget,
        deadline,
        cancellation,
        active_identity,
        shadow_identity,
        active,
        ..
    } = job;
    let event_id = event.event_id.clone();
    let correlation_id = event.correlation_id.clone();
    let input = ShadowPolicyInput {
        event: &event,
        remaining_budget,
        deadline,
        cancellation: cancellation.clone(),
    };

    // A job that is already stale when it starts, or that starts after
    // shutdown began, must not produce evidence: the record would describe a
    // comparison the runtime no longer cares about.
    if shutting_down.load(Ordering::Acquire) || input.is_cancelled() {
        return JobOutcome::Discarded;
    }
    if let Some(reason) = input.abort_reason() {
        if reason == ShadowEvaluationFailure::DeadlineExceeded {
            counters.deadline_exceeded.fetch_add(1, Ordering::Relaxed);
        }
        return JobOutcome::Record(Box::new(failed_record(
            &event_id,
            &correlation_id,
            &active_identity,
            &shadow_identity,
            &active,
            reason,
        )));
    }
    let evaluated = if config.allow_provider_calls {
        match provider {
            Some(provider) => {
                counters.provider_calls.fetch_add(1, Ordering::Relaxed);
                provider.evaluate(&input)
            }
            None => policy.evaluate(&input),
        }
    } else {
        if provider.is_some() {
            // A configured provider is never consulted while calls are disabled.
            counters
                .provider_calls_blocked
                .fetch_add(1, Ordering::Relaxed);
        }
        policy.evaluate(&input)
    };

    // Re-checked after `evaluate` returns: the policy was handed this same
    // deadline and token so it could stop itself, but only this check can
    // guarantee a late decision is never published as evidence.
    if shutting_down.load(Ordering::Acquire) || input.is_cancelled() {
        return JobOutcome::Discarded;
    }
    if let Some(reason) = input.abort_reason() {
        if reason == ShadowEvaluationFailure::DeadlineExceeded {
            counters.deadline_exceeded.fetch_add(1, Ordering::Relaxed);
        }
        return JobOutcome::Record(Box::new(failed_record(
            &event_id,
            &correlation_id,
            &active_identity,
            &shadow_identity,
            &active,
            reason,
        )));
    }

    let outcome = match evaluated {
        Ok(decision) => ShadowEvaluationOutcome::Evaluated { decision },
        Err(_) => {
            counters.planner_failed.fetch_add(1, Ordering::Relaxed);
            ShadowEvaluationOutcome::Failed {
                reason: ShadowEvaluationFailure::PlannerError,
            }
        }
    };

    JobOutcome::Record(Box::new(build_record(
        &event_id,
        &correlation_id,
        &active_identity,
        &shadow_identity,
        &active,
        outcome,
    )))
}

fn failed_record(
    event_id: &str,
    correlation_id: &str,
    active_identity: &ShadowPolicyIdentity,
    shadow_identity: &ShadowPolicyIdentity,
    active: &ShadowDecision,
    reason: ShadowEvaluationFailure,
) -> ShadowComparisonRecord {
    build_record(
        event_id,
        correlation_id,
        active_identity,
        shadow_identity,
        active,
        ShadowEvaluationOutcome::Failed { reason },
    )
}

fn build_record(
    event_id: &str,
    correlation_id: &str,
    active_identity: &ShadowPolicyIdentity,
    shadow_identity: &ShadowPolicyIdentity,
    active: &ShadowDecision,
    shadow: ShadowEvaluationOutcome,
) -> ShadowComparisonRecord {
    let (route_diverged, target_diverged, fallback_diverged) = match &shadow {
        ShadowEvaluationOutcome::Evaluated { decision } => (
            Some(active.route != decision.route),
            compare_target(active, decision),
            Some(active.fallback_reason != decision.fallback_reason),
        ),
        ShadowEvaluationOutcome::Failed { .. } => (None, None, None),
    };
    ShadowComparisonRecord {
        schema_version: crate::SHADOW_COMPARISON_SCHEMA_VERSION.to_owned(),
        event_id: event_id.to_owned(),
        correlation_id: correlation_id.to_owned(),
        active_policy: active_identity.clone(),
        shadow_policy: shadow_identity.clone(),
        active: active.clone(),
        shadow,
        route_diverged,
        target_diverged,
        fallback_diverged,
    }
}
