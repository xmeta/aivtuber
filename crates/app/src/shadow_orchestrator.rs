use crate::AppError;
use crate::shadow::{
    ShadowComparisonRecord, ShadowDecision, ShadowEvaluationFailure, ShadowEvaluationOutcome,
    ShadowOrchestratorConfig, ShadowPolicy, ShadowPolicyIdentity, ShadowPolicyInput,
    ShadowProvider, ShadowSkipReason, ShadowSubmission, compare_target,
};
use aivtuber_domain::EventEnvelope;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
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

    /// Whether two handles cancel the same unit of work.
    ///
    /// A worker uses this to retire its own entry from the supersession
    /// registry without evicting a newer job that replaced it.
    pub fn is_same_work(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

/// Identity of the source work a shadow job belongs to.
///
/// Two events with the same key are the *same* revision chain, so a newer one
/// supersedes an older one. Different keys are independent source work and must
/// never cancel each other.
fn supersession_key(event: &EventEnvelope) -> String {
    event.correlation_id.clone()
}

/// Test-only barrier that pauses a worker after `run_job` returns and before it
/// enters the publication fence (#164).
///
/// The defect this exists for is an interleaving, not a value: without a hook,
/// a test cannot force a supersession to land in that window, so it can only
/// stress the scheduler and hope. Compiled out of non-test builds entirely.
#[cfg(test)]
mod publish_hook {
    use super::{Arc, Condvar, Mutex};

    /// `(reached, reached_signal, open, open_signal)` — `reached` is signalled
    /// when a worker pauses, `open` is what lets the test let it proceed.
    pub type Pause = Arc<(Mutex<bool>, Condvar, Mutex<bool>, Condvar)>;

    /// Armed by the test, read by every worker thread. A global rather than a
    /// thread-local: the pause happens on a worker, the arming happens on the
    /// composition thread.
    pub static ARMED: Mutex<Option<Pause>> = Mutex::new(None);

    pub fn new_pause() -> Pause {
        Arc::new((
            Mutex::new(false),
            Condvar::new(),
            Mutex::new(false),
            Condvar::new(),
        ))
    }

    /// Wait until a worker has paused, bounded so a missing arrival fails the
    /// test loudly instead of hanging the worker threads of every later test.
    pub fn wait_until_paused(pause: &Pause) {
        let (reached, reached_signal, _, _) = &**pause;
        let mut seen = reached.lock().expect("publish hook reached");
        for _ in 0..2_000 {
            if *seen {
                return;
            }
            let (guard, timeout) = reached_signal
                .wait_timeout(seen, std::time::Duration::from_millis(5))
                .expect("publish hook wait");
            seen = guard;
            if timeout.timed_out() {
                panic!("no shadow worker reached the publication hook within 10s");
            }
        }
    }

    pub fn release(pause: &Pause) {
        let (_, _, open, open_signal) = &**pause;
        *open.lock().expect("publish hook open") = true;
        open_signal.notify_all();
    }

    /// Called by every worker between `run_job` and the publication fence.
    pub fn pause_if_armed() {
        let armed = ARMED.lock().expect("publish hook arm lock").clone();
        let Some(pause) = armed else { return };
        let (reached, reached_signal, open, open_signal) = &*pause;
        {
            let mut seen = reached.lock().expect("publish hook reached");
            *seen = true;
            reached_signal.notify_all();
        }
        let mut open = open.lock().expect("publish hook open");
        while !*open {
            open = open_signal.wait(open).expect("publish hook wait");
        }
    }
}

#[cfg(test)]
fn publish_hook_pause() {
    publish_hook::pause_if_armed();
}

/// Opaque handle for an armed publication hook.
///
/// Deliberately opaque: the underlying `Pause` stays private to this module, so
/// a test can only wait for the arrival and release it, not forge a barrier.
#[cfg(test)]
pub(crate) struct ArmedPublishHook {
    pause: publish_hook::Pause,
}

#[cfg(test)]
impl ArmedPublishHook {
    /// Block until a shadow worker parks between evaluation and publication.
    pub(crate) fn wait_until_worker_paused(&self) {
        publish_hook::wait_until_paused(&self.pause);
    }
}

#[cfg(test)]
impl Drop for ArmedPublishHook {
    /// Always release and disarm, even on panic, so a failing test cannot wedge
    /// the worker threads of every later test in the suite.
    fn drop(&mut self) {
        publish_hook::release(&self.pause);
        *publish_hook::ARMED.lock().expect("publish hook arm lock") = None;
    }
}

/// Arm the publication hook until the returned handle is dropped (#164).
#[cfg(test)]
pub(crate) fn arm_publish_hook() -> ArmedPublishHook {
    let pause = publish_hook::new_pause();
    *publish_hook::ARMED.lock().expect("publish hook arm lock") = Some(pause.clone());
    ArmedPublishHook { pause }
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
    /// A job that passed the deterministic gates and is waiting for the budget
    /// authority. It holds a cloned `EventEnvelope`, so it is outstanding work
    /// even though no worker owns it yet.
    pub staged: usize,
    /// Supersession chains with a live cancellation token, i.e. evaluations
    /// that have been enqueued and have not yet finished or been superseded.
    ///
    /// Must return to 0 once everything drains: a leaked entry is a retained
    /// `EventEnvelope` and an unbounded registry.
    pub live_chains: usize,
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
    /// True when no shadow work is outstanding anywhere: not staged, not
    /// queued, not executing in a worker, and published-but-undrained on the
    /// completion mailbox. Same false-quiescent trap #177 fixed for generations.
    pub fn is_quiescent(&self) -> bool {
        self.pending == 0 && self.in_flight == 0 && self.awaiting_drain == 0 && self.staged == 0
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
        staged: bool,
        live_chains: usize,
    ) -> ShadowExecutionSnapshot {
        ShadowExecutionSnapshot {
            pending: self.pending.load(Ordering::SeqCst),
            in_flight: self.in_flight.load(Ordering::SeqCst),
            awaiting_drain: self.awaiting_drain.load(Ordering::SeqCst),
            staged: usize::from(staged),
            live_chains,
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
/// Why a non-blocking admission did not take the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushRefusal {
    /// The bounded queue is at capacity; the work is dropped, not backpressured.
    Saturated,
    /// The queue is closed because shutdown began; nothing will ever run it.
    Closed,
}

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

    /// Non-blocking admission, distinguishing *why* it failed.
    ///
    /// A closed queue is a separate outcome from a full one: after shutdown
    /// every worker has been joined, so a job pushed then would sit in the
    /// queue forever with nothing to run it. #164 requires that shutdown
    /// leaves no shadow work outstanding, so a closed queue must refuse.
    /// Non-blocking admission, with supersession bookkeeping made atomic with
    /// the enqueue (#164).
    ///
    /// The queue lock is held across `capacity check -> token swap/cancel ->
    /// enqueue`, and workers pop under the same lock, so a worker can never
    /// observe the job before its supersession entry is registered. Doing the
    /// registry update after the push left a window where a fast policy could
    /// pop, evaluate, and retire before the entry existed: the retire then found
    /// nothing and the commit inserted a stale token afterwards, leaking one
    /// entry per distinct correlation, while an older same-chain worker could
    /// publish evidence for a revision that was already superseded.
    ///
    /// `live` is locked *inside* the queue lock, never the other way round.
    fn try_push_superseding(
        &self,
        job: ShadowJob,
        live: &Mutex<HashMap<String, ShadowCancellationToken>>,
    ) -> Result<(), PushRefusal> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PushRefusal::Closed);
        }
        let mut queue = self.state.lock().expect("shadow queue lock");
        // Re-check under the lock: `close()` can land between the load above
        // and this point.
        if self.closed.load(Ordering::Acquire) {
            return Err(PushRefusal::Closed);
        }
        if queue.len() >= self.capacity {
            return Err(PushRefusal::Saturated);
        }
        let key = supersession_key(&job.event);
        let cancellation = job.cancellation.clone();
        let mut live = live.lock().expect("shadow token lock");
        if let Some(previous) = live.insert(key, cancellation) {
            previous.cancel();
        }
        drop(live);
        queue.push_back(job);
        drop(queue);
        self.available.notify_one();
        Ok(())
    }

    /// Publication fence for one finished job (#164).
    ///
    /// Runs under the queue lock — the same lock `supersede` takes — so
    /// supersession and evidence publication are serialised. In one critical
    /// section it re-checks whether this job is still the chain's current
    /// revision, downgrades the completion to discarded if it is not, publishes
    /// it, and retires the chain's entry.
    ///
    /// Checking outside this section was not enough: `run_job`'s last
    /// cancellation check, the token retirement, and the publish were three
    /// separate steps, so a newer revision could supersede in the gap and a
    /// stale comparison would still reach the mailbox. Retiring first was
    /// worse still — supersession then found no entry to cancel.
    ///
    /// The publish stays non-blocking: a worker must never wait on the
    /// composition thread, or shutdown could not join it.
    #[allow(clippy::too_many_arguments)]
    fn publish_fenced(
        &self,
        live: &Mutex<HashMap<String, ShadowCancellationToken>>,
        key: &str,
        token: &ShadowCancellationToken,
        shutting_down: &AtomicBool,
        tx: &mpsc::SyncSender<ShadowCompletion>,
        mut completion: ShadowCompletion,
        counters: &ShadowCounters,
    ) {
        let _queue = self.state.lock().expect("shadow queue lock");
        let mut live = live.lock().expect("shadow token lock");
        let still_current = !token.is_cancelled()
            && !shutting_down.load(Ordering::Acquire)
            && live
                .get(key)
                .is_some_and(|current| current.is_same_work(token));
        if !still_current && completion.record.is_some() {
            // Superseded (or shutting down) after `run_job` decided: the record
            // describes work the runtime no longer wants, so drop the evidence.
            completion.record = None;
            counters.cancelled_or_stale.fetch_add(1, Ordering::Relaxed);
        }
        // Retire this chain's entry so the registry stays bounded by in-flight
        // chains. Guarded by token identity: a newer job of the same chain has
        // already replaced the entry and must not be evicted.
        if live
            .get(key)
            .is_some_and(|current| current.is_same_work(token))
        {
            live.remove(key);
        }
        drop(live);
        if tx.try_send(completion).is_err() {
            counters.awaiting_drain.fetch_sub(1, Ordering::SeqCst);
            counters
                .dropped_completion_mailbox_full
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Cancel the live token of one supersession chain without starting any
    /// new work (#164).
    ///
    /// Supersession is a property of *source work*, not of shadow admission: a
    /// newer revision that is filtered, unsampled, saturated, or budget-denied
    /// still supersedes the revision before it, whose evaluation is now stale.
    /// Locked under the queue lock so a worker cannot slip a pop between the
    /// check and the cancel.
    fn supersede(&self, key: &str, live: &Mutex<HashMap<String, ShadowCancellationToken>>) {
        let _queue = self.state.lock().expect("shadow queue lock");
        if let Some(previous) = live.lock().expect("shadow token lock").remove(key) {
            previous.cancel();
        }
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
    /// A job that passed every deterministic gate and is waiting for the #69
    /// budget authority before it is queued (#164).
    ///
    /// Staging rather than queueing is what makes the governor a real admission
    /// authority without leaking: nothing is queued, no worker is occupied and
    /// no budget is spent until the composition thread confirms the active
    /// route succeeded, so a failed route cannot leave a denial or a charge
    /// behind for an unrelated later event.
    staged: Option<ShadowJob>,
    last_submission: Option<ShadowSubmission>,
    last_comparison: Option<ShadowComparisonRecord>,
    /// Live cancellation token per supersession chain (#164).
    ///
    /// Keyed by `correlation_id`, not a single global slot: #164 scopes
    /// cancellation to *superseded source work*, so admitting a job cancels the
    /// previous job of the same chain and nothing else. A global latest-wins
    /// slot would make unrelated events destroy each other's evidence under
    /// sustained traffic, biasing sampling results toward arrival order.
    ///
    /// Shared with the workers so a finished job retires its own entry; the
    /// map is therefore bounded by the number of in-flight chains rather than
    /// growing with every event.
    live_tokens: Arc<Mutex<HashMap<String, ShadowCancellationToken>>>,
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
        let live_tokens = Arc::new(Mutex::new(HashMap::<String, ShadowCancellationToken>::new()));

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
            let worker_live_tokens = Arc::clone(&live_tokens);
            let handle = thread::Builder::new()
                .name(format!("aivtuber-shadow-worker-{index}"))
                .spawn(move || {
                    while let Some(job) = worker_queue.pop() {
                        let job_id = job.job_id;
                        let finished_key = supersession_key(&job.event);
                        let finished_token = job.cancellation.clone();
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
                        #[cfg(test)]
                        publish_hook_pause();
                        // Claim the drain window before publishing so a
                        // quiescence check cannot miss a completion that is
                        // already on its way to the mailbox.
                        worker_counters.note_published();
                        // The publication fence. Re-checking cancellation here
                        // and retiring the chain happen inside one critical
                        // section that `supersede` also takes, so a supersession
                        // can never land between the check and the publish: a
                        // revision that was superseded in that window publishes
                        // as discarded instead of as stale evidence.
                        worker_queue.publish_fenced(
                            worker_live_tokens.as_ref(),
                            &finished_key,
                            &finished_token,
                            worker_shutdown.as_ref(),
                            &worker_completion_tx,
                            completion,
                            worker_counters.as_ref(),
                        );
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
            staged: None,
            last_submission: None,
            last_comparison: None,
            live_tokens: Arc::clone(&live_tokens),
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
        if !self.stage(
            event,
            remaining_budget,
            active_identity,
            shadow_identity,
            active,
        ) {
            return self
                .last_submission
                .clone()
                .expect("a rejected stage records its own skip");
        }
        self.commit()
    }

    /// Phase 1 of admission: run the deterministic gates and build the job,
    /// but queue nothing yet.
    ///
    /// Returns whether a job was staged. When it was not, the skip reason is
    /// already recorded in `last_submission`. Nothing observable has happened:
    /// no queue slot is taken, no counter moves, and no budget can be spent,
    /// so the caller may discard the stage freely if the active route fails.
    pub fn stage(
        &mut self,
        event: &EventEnvelope,
        remaining_budget: Option<Duration>,
        active_identity: &ShadowPolicyIdentity,
        shadow_identity: &ShadowPolicyIdentity,
        active: ShadowDecision,
    ) -> bool {
        // Only one job can be staged at a time: staging is always followed by
        // a commit or a discard on the same thread, before the next event.
        self.staged = None;
        if let Some(reason) = self.config.skip_reason(event) {
            self.skip(reason);
            return false;
        }
        if self.shutting_down.load(Ordering::Acquire) {
            self.skip(ShadowSkipReason::ShuttingDown);
            return false;
        }
        if !self.work.has_room() {
            self.skip(ShadowSkipReason::Saturated);
            return false;
        }
        self.next_job_id = self.next_job_id.saturating_add(1);
        self.staged = Some(ShadowJob {
            job_id: self.next_job_id,
            event: event.clone(),
            remaining_budget,
            deadline: Instant::now() + Duration::from_millis(self.config.deadline_ms),
            cancellation: ShadowCancellationToken::default(),
            active_identity: active_identity.clone(),
            shadow_identity: shadow_identity.clone(),
            active,
        });
        true
    }

    /// Phase 2 of admission: queue the staged job.
    ///
    /// `budget_admitted` is the #69 governor's decision. The job is only ever
    /// queued when the governor admitted it, so denied work is never executed,
    /// and `last_submission` is written by the same attempt that skipped, so
    /// it cannot describe something that never happened.
    pub fn commit(&mut self) -> ShadowSubmission {
        let Some(job) = self.staged.take() else {
            return self
                .last_submission
                .clone()
                .unwrap_or(ShadowSubmission::Skipped {
                    reason: ShadowSkipReason::Disabled,
                });
        };
        let job_id = job.job_id;
        // `note_queued` is reverted on every refusal, so a refused push never
        // inflates the pending gauge.
        self.counters.note_queued();
        // Enqueue and supersession registration happen together under the queue
        // lock, so no worker can see the job before its registry entry exists.
        if let Err(refusal) = self
            .work
            .try_push_superseding(job, self.live_tokens.as_ref())
        {
            self.counters.pending.fetch_sub(1, Ordering::SeqCst);
            return self.skip(match refusal {
                PushRefusal::Saturated => ShadowSkipReason::Saturated,
                // Every worker has been joined, so queueing now would leave the
                // job outstanding forever.
                PushRefusal::Closed => ShadowSkipReason::ShuttingDown,
            });
        }
        self.counters.submitted.fetch_add(1, Ordering::Relaxed);
        let submission = ShadowSubmission::Admitted { job_id };
        self.last_submission = Some(submission.clone());
        submission
    }

    /// Refuse the staged job: the #69 governor denied it.
    ///
    /// The job was never queued, so nothing needs cancelling or unwinding, and
    /// no worker is occupied at any point.
    pub fn commit_denied(&mut self) -> ShadowSubmission {
        self.staged = None;
        self.skip(ShadowSkipReason::BudgetDenied)
    }

    /// Phase 2 driven by the #69 governor's verdict.
    pub fn commit_with_budget(&mut self, budget_admitted: bool) -> ShadowSubmission {
        if budget_admitted {
            self.commit()
        } else {
            self.commit_denied()
        }
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

    /// Invalidate the live evaluation of this event's supersession chain
    /// without starting any new shadow work (#164).
    ///
    /// #164 scopes cancellation to superseded *source work*, so it must not
    /// depend on whether the new revision produces shadow work. A newer
    /// revision that is filtered, unsampled, saturated, or budget-denied still
    /// supersedes the one before it; otherwise that older evaluation runs to
    /// completion and publishes evidence the runtime no longer wants.
    pub fn supersede_source(&mut self, event: &EventEnvelope) {
        self.work
            .supersede(&supersession_key(event), self.live_tokens.as_ref());
    }

    /// Whether a job is staged and waiting for the budget authority.
    ///
    /// Lets the composition thread skip the #69 probe entirely when there is
    /// nothing to pay for.
    pub fn has_staged(&self) -> bool {
        self.staged.is_some()
    }

    /// Drop a staged job without queueing it.
    ///
    /// Used when the active route fails after staging: the comparison would
    /// describe a decision the runtime never made, and no budget was charged.
    pub fn discard_staged(&mut self) {
        self.staged = None;
    }

    pub fn snapshot(&self) -> ShadowExecutionSnapshot {
        let workers_running = self
            .workers
            .iter()
            .filter(|worker| !worker.is_finished())
            .count();
        let live_chains = self.live_tokens.lock().expect("shadow token lock").len();
        self.counters.snapshot(
            &self.config,
            self.shutting_down.load(Ordering::Acquire),
            workers_running,
            self.staged.is_some(),
            live_chains,
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
        // A staged job was never queued, so nothing else would release it. Its
        // `EventEnvelope` would otherwise outlive shutdown and stay invisible
        // to the snapshot, leaving `is_quiescent()` lying about the runtime.
        self.staged = None;
        // Every chain must stop, not just one: shutdown is global.
        for (_, token) in std::mem::take(&mut *self.live_tokens.lock().expect("shadow token lock"))
        {
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
        // Same reason as `shutdown`: never let a staged envelope outlive the
        // orchestrator.
        self.staged = None;
        for (_, token) in std::mem::take(&mut *self.live_tokens.lock().expect("shadow token lock"))
        {
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
