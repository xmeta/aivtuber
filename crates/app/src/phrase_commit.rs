//! Phrase-level atomic commit of generated replies (issue #83 S1).
//!
//! Buffered mode hands the composition thread one completed asset per reply.
//! Phrase mode hands it one complete, gated, synthesized phrase at a time while
//! the provider worker keeps synthesizing the next one:
//!
//! ```text
//! worker:       Thinking -> segment -> [gate -> TTS -> compile] -> deliver  (repeat)
//! composition:  drain -> currency + gate recheck -> commit to the shared scheduler
//! ```
//!
//! The worker never touches the scheduler, the asset store or the audit log.
//! The composition thread commits a phrase only after the generation is still
//! current and uncancelled and the output gate has been re-applied to that
//! phrase's text, the same two checks a buffered reply gets. The scheduler
//! chains accepted phrases (see `Scheduler::schedule_phrase_at`), so the
//! existing safe-point, operator-stop and barge-in machinery acts on them like
//! on any other item, and a cancelled response simply never commits its later
//! phrases.
//!
//! Lookahead is bounded to one phrase: the worker starts synthesizing phrase
//! `n` only once phrase `n - 1` has started playing, so synthesis of a phrase
//! overlaps the playback of the one before it and never of older ones. The
//! counter it waits on is written only by the composition thread.

use super::{
    DeadlineObservation, GenerationCompletion, GenerationCompletionContext, HandledEvent,
    ProductionApp, RoutePlanner, elapsed_us, remaining_instant,
};
use aivtuber_domain::{EventEnvelope, FallbackReason};
use aivtuber_generative::{
    Delivery, FallbackDirective, GenerationCancellationToken, GenerationDisposition,
    GenerationTrace, PhraseConfig, PhraseDelivery, PhraseEnd, PhraseFailure, PhraseSummary,
    PhraseUnit, SlotWait, wait_for_started,
};
use aivtuber_runtime::{CachedPlaybackError, CachedPlaybackTiming, OutputVerdict, PublicOutput};
use aivtuber_scheduler::{PhraseLink, Rejection};
use aivtuber_telemetry::{AuditCategory, CacheLevel, RouteClass};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

/// Retained [`PhraseResponseRecord`]s.
pub const PHRASE_RESPONSE_RECORDS: usize = 32;

/// Messages the worker may have handed over but the composition thread has
/// not yet committed. Two is one committed-to-be plus one being handed over,
/// which is all the one-phrase lookahead can ever produce.
pub(crate) const PHRASE_MAILBOX_CAPACITY: usize = 2;

/// How long the worker waits for the previous phrase to start playing before
/// it ends the reply (`LookaheadStalled`). A phrase committed behind other
/// speech can legitimately wait for it, so this is generous; cancellation and
/// shutdown end the wait sooner.
pub const PHRASE_START_WAIT: Duration = Duration::from_secs(30);

/// How generated replies are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseMode {
    /// One asset per reply, committed when the whole reply is synthesized.
    Buffered,
    /// One atomic commit per phrase.
    Phrased(PhraseConfig),
}

impl PhraseMode {
    pub fn is_phrased(&self) -> bool {
        matches!(self, Self::Phrased(_))
    }
}

/// Everything the worker needs to run one job phrased.
pub(crate) struct PhraseWork {
    pub(crate) config: PhraseConfig,
    pub(crate) mailbox: SyncSender<PhraseMessage>,
    pub(crate) started: Arc<AtomicU32>,
}

/// One complete phrase on its way to the commit boundary.
pub(crate) struct PhraseMessage {
    pub(crate) generation_id: u64,
    pub(crate) unit: PhraseUnit,
    /// The output gate's last decision for this phrase's display text.
    pub(crate) output_decision: Option<PublicOutput>,
    /// Trace so far (only phrase 0's is used, for the audit of the LLM call).
    pub(crate) trace: GenerationTrace,
    pub(crate) queue_wait_us: u64,
    pub(crate) provider_latency_us: u64,
    pub(crate) generation_latency_us: u64,
    pub(crate) delivered_at: Instant,
}

/// Worker-side [`PhraseDelivery`] over the bounded mailbox.
pub(crate) struct ChannelDelivery<'a> {
    pub(crate) generation_id: u64,
    pub(crate) work: &'a PhraseWork,
    pub(crate) cancellation: &'a GenerationCancellationToken,
    pub(crate) last_decision: &'a RefCell<Option<PublicOutput>>,
    pub(crate) submitted_at: Instant,
    pub(crate) provider_started: Instant,
    pub(crate) queue_wait_us: u64,
    pub(crate) shutting_down: &'a AtomicBool,
}

impl PhraseDelivery for ChannelDelivery<'_> {
    fn wait_for_slot(&mut self, seq: u32, cancellation: &GenerationCancellationToken) -> SlotWait {
        wait_for_started(cancellation, seq, PHRASE_START_WAIT, || {
            if self.shutting_down.load(Ordering::Acquire) {
                // A runtime that is shutting down will commit nothing more.
                cancellation.cancel();
            }
            self.work.started.load(Ordering::Acquire)
        })
    }

    fn deliver(&mut self, unit: PhraseUnit, trace: &GenerationTrace) -> Delivery {
        let mut message = PhraseMessage {
            generation_id: self.generation_id,
            unit,
            output_decision: self.last_decision.borrow_mut().take(),
            trace: trace.clone(),
            queue_wait_us: self.queue_wait_us,
            provider_latency_us: elapsed_us(self.provider_started),
            generation_latency_us: elapsed_us(self.submitted_at),
            delivered_at: Instant::now(),
        };
        loop {
            match self.work.mailbox.try_send(message) {
                Ok(()) => return Delivery::Accepted,
                Err(TrySendError::Disconnected(_)) => return Delivery::Closed,
                Err(TrySendError::Full(returned)) => {
                    // The composition thread is behind: wait, but never past a
                    // cancellation or a shutdown.
                    if self.cancellation.is_cancelled()
                        || self.shutting_down.load(Ordering::Acquire)
                    {
                        return Delivery::Closed;
                    }
                    message = returned;
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
}

/// Why a phrased reply ended before every planned phrase was committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseCause {
    OperatorStop,
    BargeIn,
    /// A newer generation or a higher-priority event took over, or the token
    /// was cancelled for a reason the app did not record.
    Superseded,
    Shutdown,
    /// The interaction deadline passed before phrase 0 could be committed.
    DeadlineExpired,
    /// The output gate (worker or composition recheck) refused a phrase.
    GateRejected,
    /// The shared scheduler refused a phrase (the reply could not continue).
    SchedulerRejected(Rejection),
    SynthesisFailed(FallbackReason),
    CompileFailed,
    NothingSpeakable,
    LookaheadStalled,
    /// The provider token stream failed after phrases were committed.
    StreamFailed(FallbackReason),
}

impl PhraseCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OperatorStop => "operator_stop",
            Self::BargeIn => "barge_in",
            Self::Superseded => "superseded",
            Self::Shutdown => "shutdown",
            Self::DeadlineExpired => "deadline_expired",
            Self::GateRejected => "gate_rejected",
            Self::SchedulerRejected(_) => "scheduler_rejected",
            Self::SynthesisFailed(_) => "synthesis_failed",
            Self::CompileFailed => "compile_failed",
            Self::NothingSpeakable => "nothing_speakable",
            Self::LookaheadStalled => "lookahead_stalled",
            Self::StreamFailed(_) => "stream_failed",
        }
    }

    fn from_failure(failure: PhraseFailure) -> Self {
        match failure {
            PhraseFailure::GateRejected => Self::GateRejected,
            PhraseFailure::NothingSpeakable => Self::NothingSpeakable,
            PhraseFailure::Synthesis(reason) => Self::SynthesisFailed(reason),
            PhraseFailure::Compile => Self::CompileFailed,
            PhraseFailure::SchedulerRejected => Self::SchedulerRejected(Rejection::Priority),
            PhraseFailure::LookaheadStalled => Self::LookaheadStalled,
            PhraseFailure::Stream(reason) => Self::StreamFailed(reason),
        }
    }
}

/// How a phrased reply ended. Never "complete" unless every planned phrase was
/// committed (the issue's "do not claim the full answer completed").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseResponseEnd {
    Completed,
    /// At least one phrase was spoken and the reply stopped early.
    Partial {
        cause: PhraseCause,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseGateOutcome {
    Allowed,
    Redacted,
}

/// One accepted phrase commit, in commit order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseCommitRecord {
    pub seq: u32,
    pub final_phrase: bool,
    /// Scheduler generation of the committed item.
    pub scheduler_generation: u64,
    /// Logical time the commit was accepted.
    pub committed_at_ms: u64,
    pub start_at_ms: u64,
    pub end_at_ms: u64,
    /// Silence between the previous phrase's end and this one's start: `0` when
    /// the phrases are gapless, positive when synthesis was late.
    pub gap_before_ms: u64,
    pub gate: PhraseGateOutcome,
    /// Characters of the approved display text.
    pub text_chars: usize,
    /// Wall time TTS took for this phrase (measurement only).
    pub synthesis_ms: u64,
}

/// The replayable record of one phrased reply: which phrases passed the
/// irreversible commit horizon, in which order, and how the reply ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhraseResponseRecord {
    /// Generation/cancel identity of the reply.
    pub response_id: u64,
    pub event_id: String,
    pub segmentation_version: String,
    pub planned_phrases: u32,
    /// Accepted commits in order. `commits.len()` is the commit horizon.
    pub commits: Vec<PhraseCommitRecord>,
    pub end: PhraseResponseEnd,
}

impl PhraseResponseRecord {
    /// Number of phrases that crossed the irreversible commit horizon.
    pub fn commit_horizon(&self) -> usize {
        self.commits.len()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhraseMetrics {
    /// Replies for which at least one phrase was committed.
    pub responses: u64,
    pub completed: u64,
    pub partial: u64,
    pub phrases_committed: u64,
    /// Gate refusal of phrase 0 (nothing spoken yet).
    pub gate_rejected_before_first_commit: u64,
    /// Gate refusal of a later phrase (earlier phrases already spoken).
    pub gate_rejected_after_commit: u64,
    pub scheduler_rejected: u64,
    pub synthesis_failed_after_commit: u64,
    pub cancelled_after_commit: u64,
    /// Phrases that started after silence because synthesis was late.
    pub underruns: u64,
    pub max_gap_ms: u64,
    pub total_synthesis_ms: u64,
    pub max_synthesis_ms: u64,
    /// Phrase messages dropped because their reply had ended or was stale.
    pub late_phrases_dropped: u64,
}

pub(crate) struct CommittedPhrase {
    seq: u32,
    end_at_ms: u64,
    text_chars: usize,
}

/// Composition-thread state of one phrased generation.
pub(crate) struct PhraseRun {
    pub(crate) generation_id: u64,
    pub(crate) event: EventEnvelope,
    pub(crate) seed: u64,
    pub(crate) cancellation: GenerationCancellationToken,
    pub(crate) started: Arc<AtomicU32>,
    pub(crate) provider_deadline: Instant,
    /// A copy of the completion context without the budget reservation, which
    /// stays with the one completion that settles it.
    pub(crate) context: GenerationCompletionContext,
    /// The fallback chosen at submission. Used only when phrase 0 is refused
    /// at the commit boundary, so nothing was heard (issue #288); once any
    /// phrase has been spoken the reply ends without fallback speech.
    pub(crate) fallback: FallbackDirective,
    pub(crate) segmentation_version: String,
    planned_phrases: u32,
    commits: Vec<PhraseCommitRecord>,
    committed: Vec<CommittedPhrase>,
    /// Phrase 0 was handled (committed or refused) and recorded as the
    /// event's observation.
    pub(crate) first_handled: bool,
    /// Set by the commit side when it ended the reply (the worker's own
    /// summary can only say what it saw).
    ended: Option<PhraseCause>,
    /// Why the app cancelled the generation, when it did.
    cancel_cause: Option<PhraseCause>,
}

impl PhraseRun {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        generation_id: u64,
        event: EventEnvelope,
        seed: u64,
        cancellation: GenerationCancellationToken,
        started: Arc<AtomicU32>,
        provider_deadline: Instant,
        context: GenerationCompletionContext,
        fallback: FallbackDirective,
    ) -> Self {
        Self {
            generation_id,
            event,
            seed,
            cancellation,
            started,
            provider_deadline,
            context,
            fallback,
            segmentation_version: String::new(),
            planned_phrases: 0,
            commits: Vec::new(),
            committed: Vec::new(),
            first_handled: false,
            ended: None,
            cancel_cause: None,
        }
    }

    pub(crate) fn note_cancel_cause(&mut self, cause: PhraseCause) {
        if self.cancel_cause.is_none() {
            self.cancel_cause = Some(cause);
        }
    }

    pub(crate) fn has_committed(&self) -> bool {
        !self.committed.is_empty()
    }

    /// Characters committed so far (budget settlement when the worker could
    /// not report a total).
    pub(crate) fn committed_chars(&self) -> u64 {
        self.committed.iter().map(|c| c.text_chars as u64).sum()
    }

    fn previous_end_ms(&self) -> Option<u64> {
        self.committed.last().map(|phrase| phrase.end_at_ms)
    }
}

/// Bounded store of the live runs, the mailbox receiver side's bookkeeping,
/// the retained records and the counters.
#[derive(Default)]
pub(crate) struct PhraseBook {
    pub(crate) runs: Vec<PhraseRun>,
    records: VecDeque<PhraseResponseRecord>,
    pub(crate) metrics: PhraseMetrics,
}

impl PhraseBook {
    pub(crate) fn run_mut(&mut self, generation_id: u64) -> Option<&mut PhraseRun> {
        self.runs
            .iter_mut()
            .find(|run| run.generation_id == generation_id)
    }

    pub(crate) fn take_run(&mut self, generation_id: u64) -> Option<PhraseRun> {
        let index = self
            .runs
            .iter()
            .position(|run| run.generation_id == generation_id)?;
        Some(self.runs.remove(index))
    }

    pub(crate) fn recent(&self) -> Vec<PhraseResponseRecord> {
        self.records.iter().cloned().collect()
    }

    /// True while a reply that already spoke a phrase may still commit more.
    pub(crate) fn has_active_spoken_run(&self) -> bool {
        self.runs.iter().any(|run| {
            run.has_committed() && run.ended.is_none() && !run.cancellation.is_cancelled()
        })
    }

    pub(crate) fn note_cancel_cause(&mut self, cause: PhraseCause) {
        for run in &mut self.runs {
            if !run.cancellation.is_cancelled() {
                run.note_cancel_cause(cause);
            }
        }
    }

    fn push_record(&mut self, record: PhraseResponseRecord) {
        if self.records.len() == PHRASE_RESPONSE_RECORDS {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }
}

fn gate_outcome(output: &PublicOutput) -> PhraseGateOutcome {
    if output.verdict == OutputVerdict::Redact {
        PhraseGateOutcome::Redacted
    } else {
        PhraseGateOutcome::Allowed
    }
}

fn gate_allows_same_text(output: Option<&PublicOutput>, text: &str) -> bool {
    output.is_some_and(|output| {
        matches!(output.verdict, OutputVerdict::Allow | OutputVerdict::Redact)
            && output.text.as_deref() == Some(text)
    })
}

impl<R> ProductionApp<R>
where
    R: RoutePlanner,
{
    /// Publish the leading-started count to the worker so it may synthesize
    /// the next phrase. Runs after the scheduler advanced to `now`.
    pub(crate) fn update_phrase_progress(&mut self) {
        let scheduler = self.performer.scheduler();
        let Some(generative) = self.generative.as_mut() else {
            return;
        };
        for run in &mut generative.phrase.runs {
            // Phrases already counted stay counted; only look at the rest.
            let mut started = run.started.load(Ordering::Acquire);
            for phrase in run.committed.iter().skip(started as usize) {
                // `None` means the item left the retained history: it is long
                // over, so it no longer holds the worker back.
                if scheduler
                    .phrase_started(run.generation_id, phrase.seq)
                    .unwrap_or(true)
                {
                    started += 1;
                } else {
                    break;
                }
            }
            run.started.store(started, Ordering::Release);
        }
    }

    /// Mark every live phrased reply as cancelled for `cause`. The token
    /// itself is cancelled by the caller's own path.
    pub(crate) fn note_phrase_cancel_cause(&mut self, cause: PhraseCause) {
        if let Some(generative) = self.generative.as_mut() {
            generative.phrase.note_cancel_cause(cause);
        }
    }

    /// Cancel replies that have spoken a phrase and may still commit more (a
    /// confirmed barge-in during a gap between phrases). Returns how many.
    pub(crate) fn cancel_phrased_replies(&mut self, cause: PhraseCause) -> usize {
        let Some(generative) = self.generative.as_mut() else {
            return 0;
        };
        let mut cancelled = 0;
        for run in &mut generative.phrase.runs {
            if run.has_committed() && !run.cancellation.is_cancelled() {
                run.note_cancel_cause(cause);
                run.cancellation.cancel();
                cancelled += 1;
            }
        }
        cancelled
    }

    pub(crate) fn drain_phrase_messages(&mut self, now_ms: u64) {
        loop {
            let message = self
                .generative
                .as_ref()
                .and_then(|generative| generative.try_next_phrase());
            let Some(message) = message else { break };
            self.commit_phrase(message, now_ms);
        }
    }

    fn audit_phrase(&mut self, event_id: &str, decision: &str, detail: String) {
        let record = self.security.redactor_record(
            Some(event_id),
            AuditCategory::Generation,
            decision,
            detail,
        );
        self.security.push_external_audit(record);
    }

    /// The commit boundary for one phrase. Everything that can refuse a phrase
    /// is decided here, on the composition thread, before the scheduler sees it.
    fn commit_phrase(&mut self, message: PhraseMessage, now_ms: u64) {
        let generation_id = message.generation_id;
        let seq = message.unit.seq();
        let Some(generative) = self.generative.as_mut() else {
            return;
        };
        let Some((event_id, ended, cancelled, deadline_expired)) =
            generative.phrase.run_mut(generation_id).map(|run| {
                (
                    run.event.event_id.clone(),
                    run.ended.is_some(),
                    run.cancellation.is_cancelled(),
                    remaining_instant(run.provider_deadline).is_none(),
                )
            })
        else {
            generative.phrase.metrics.late_phrases_dropped += 1;
            return;
        };
        let shutting_down = generative.is_shutting_down();
        let current = generative.is_current(generation_id, &event_id);
        if ended || cancelled || shutting_down || !current {
            // The reply ended or was superseded/cancelled: the phrase is
            // dropped uncommitted and the worker is told to stop.
            generative.phrase.metrics.late_phrases_dropped += 1;
            if let Some(run) = generative.phrase.run_mut(generation_id) {
                run.cancellation.cancel();
                run.note_cancel_cause(if shutting_down {
                    PhraseCause::Shutdown
                } else {
                    PhraseCause::Superseded
                });
            }
            return;
        }

        // Phrase 0 must still beat the interaction deadline, like a buffered
        // reply. Later phrases are already being heard and are not bound by it.
        if seq == 0 && deadline_expired {
            if let Some(run) = generative.phrase.run_mut(generation_id) {
                run.note_cancel_cause(PhraseCause::DeadlineExpired);
                run.cancellation.cancel();
            }
            generative.phrase.metrics.late_phrases_dropped += 1;
            return;
        }

        let PhraseMessage {
            unit,
            output_decision,
            trace,
            queue_wait_us,
            provider_latency_us,
            generation_latency_us,
            delivered_at,
            ..
        } = message;
        let text = unit.display_text().to_owned();
        let final_phrase = unit.is_final();
        let synthesis_ms = unit.synthesis_ms();

        // The worker's decision must be for exactly this text, and the
        // current policy must still publish exactly this text.
        let rechecked = self.security.public_output_policy().evaluate(&text);
        let worker_ok = gate_allows_same_text(output_decision.as_ref(), &text);
        let recheck_ok = gate_allows_same_text(Some(&rechecked), &text);
        if !worker_ok || !recheck_ok {
            if worker_ok {
                self.security.record_public_output_decision(&rechecked);
            } else {
                self.record_worker_output_decision(output_decision.as_ref());
            }
            self.refuse_phrase(generation_id, seq, PhraseCause::GateRejected, trace, now_ms);
            return;
        }
        self.record_worker_output_decision(output_decision.as_ref());
        let gate = output_decision
            .as_ref()
            .map_or(PhraseGateOutcome::Allowed, gate_outcome);

        let asset = unit.into_asset();
        let asset_id = asset.id.clone();
        let link = PhraseLink {
            response_id: generation_id,
            seq,
            final_phrase,
        };
        let (event, seed) = {
            let run = self
                .generative
                .as_mut()
                .and_then(|generative| generative.phrase.run_mut(generation_id))
                .expect("run checked above");
            (run.event.clone(), run.seed)
        };
        let committed = self
            .performer
            .assets_mut()
            .insert_hot(asset)
            .map_err(|error| error.to_string())
            .and_then(|_| {
                self.performer
                    .handle_phrase_asset_id(
                        &event,
                        &asset_id,
                        link,
                        CachedPlaybackTiming {
                            at_ms: now_ms,
                            seed,
                        },
                        &self.visemes,
                        &mut self.pending_audio,
                        &mut self.pending_avatar,
                    )
                    .map_err(|error| match error {
                        CachedPlaybackError::Scheduler(rejection) => {
                            format!("scheduler:{rejection:?}")
                        }
                        other => other.to_string(),
                    })
            });
        let outcome = match committed {
            Ok(outcome) => outcome,
            Err(error) => {
                let cause = if error.starts_with("scheduler:") {
                    let rejection = self
                        .performer
                        .scheduler()
                        .last_rejection()
                        .unwrap_or(Rejection::ContinuationBroken);
                    PhraseCause::SchedulerRejected(rejection)
                } else {
                    PhraseCause::CompileFailed
                };
                self.refuse_phrase(generation_id, seq, cause, trace, now_ms);
                return;
            }
        };

        // Accepted: this phrase is now history.
        self.consolidate_committed_reply(&event, &text, now_ms);
        let plan = &outcome.plan;
        let scheduler_generation = plan.generation;
        let (end_at_ms, start_at_ms) = (plan.end_at_ms(), plan.start_at_ms);
        let (previous_end, first) = {
            let run = self
                .generative
                .as_mut()
                .and_then(|generative| generative.phrase.run_mut(generation_id))
                .expect("run checked above");
            let previous_end = run.previous_end_ms();
            let gap_before_ms = previous_end.map_or(0, |end| start_at_ms.saturating_sub(end));
            run.commits.push(PhraseCommitRecord {
                seq,
                final_phrase,
                scheduler_generation,
                committed_at_ms: now_ms,
                start_at_ms,
                end_at_ms,
                gap_before_ms,
                gate,
                text_chars: text.chars().count(),
                synthesis_ms,
            });
            run.committed.push(CommittedPhrase {
                seq,
                end_at_ms,
                text_chars: text.chars().count(),
            });
            let first = !run.first_handled;
            run.first_handled = true;
            (previous_end, first)
        };
        if let Some(generative) = self.generative.as_mut() {
            let metrics = &mut generative.phrase.metrics;
            metrics.phrases_committed += 1;
            metrics.total_synthesis_ms += synthesis_ms;
            metrics.max_synthesis_ms = metrics.max_synthesis_ms.max(synthesis_ms);
            if let Some(previous_end) = previous_end {
                let gap = start_at_ms.saturating_sub(previous_end);
                if gap > 0 {
                    metrics.underruns += 1;
                    metrics.max_gap_ms = metrics.max_gap_ms.max(gap);
                }
            }
        }
        if first {
            self.record_first_phrase(
                &event,
                generation_id,
                outcome,
                trace,
                queue_wait_us,
                provider_latency_us,
                generation_latency_us,
                delivered_at,
                now_ms,
            );
        }
        self.audit_phrase(
            &event.event_id,
            "phrase_committed",
            format!(
                "response={generation_id} seq={seq} final={final_phrase} gate={gate:?} generation={} start_ms={start_at_ms} end_ms={end_at_ms}",
                scheduler_generation
            ),
        );
    }

    /// Record phrase 0 as the event's observation: it is the first audible
    /// output of the reply, so `event_to_first_audio_ms` is the first-phrase
    /// latency, which is what issue #58 compares against buffered mode.
    #[allow(clippy::too_many_arguments)]
    fn record_first_phrase(
        &mut self,
        event: &EventEnvelope,
        generation_id: u64,
        outcome: aivtuber_runtime::CachedPlaybackOutcome,
        trace: GenerationTrace,
        queue_wait_us: u64,
        provider_latency_us: u64,
        generation_latency_us: u64,
        delivered_at: Instant,
        now_ms: u64,
    ) {
        let Some(context) = self
            .generative
            .as_mut()
            .and_then(|generative| generative.phrase.run_mut(generation_id))
            .map(|run| run.context.clone())
        else {
            return;
        };
        self.record_generation_calls(&trace);
        let mut deadline =
            DeadlineObservation::new(context.interaction_deadline, context.remaining_at_route_ms);
        deadline.remaining_at_generation_ms = Some(context.remaining_at_generation_ms);
        deadline.generation_queue_wait_us = Some(queue_wait_us);
        deadline.generation_provider_latency_us = Some(provider_latency_us);
        deadline.generation_commit_delay_us = Some(elapsed_us(delivered_at));
        let handled = HandledEvent {
            playback: Some(outcome),
            route: RouteClass::Generated,
            routing_latency_us: context.routing_latency_us,
            generation_latency_us: Some(generation_latency_us),
            generation_trace: Some(trace),
            decision: context.decision.clone(),
            template_fallback: context.template_fallback,
            cache_level: Some(CacheLevel::Generated),
            deadline: Some(deadline),
            deferred_generation: false,
            admitted_bytes: None,
            correlation_id: Some(context.correlation_id.clone()),
        };
        self.record_handled_event(&event.event_id, &handled, now_ms);
    }

    /// A phrase was refused at the commit boundary. The reply ends here; the
    /// worker is cancelled so it stops synthesizing.
    fn refuse_phrase(
        &mut self,
        generation_id: u64,
        seq: u32,
        cause: PhraseCause,
        mut trace: GenerationTrace,
        now_ms: u64,
    ) {
        let (event, first_handled) = {
            let Some(run) = self
                .generative
                .as_mut()
                .and_then(|generative| generative.phrase.run_mut(generation_id))
            else {
                return;
            };
            run.ended = Some(cause);
            run.cancellation.cancel();
            (run.event.clone(), run.first_handled)
        };
        if let Some(generative) = self.generative.as_mut() {
            let metrics = &mut generative.phrase.metrics;
            match cause {
                PhraseCause::GateRejected if seq == 0 => {
                    metrics.gate_rejected_before_first_commit += 1;
                }
                PhraseCause::GateRejected => metrics.gate_rejected_after_commit += 1,
                PhraseCause::SchedulerRejected(_) => metrics.scheduler_rejected += 1,
                _ => {}
            }
        }
        self.audit_phrase(
            &event.event_id,
            "phrase_refused",
            format!(
                "response={generation_id} seq={seq} cause={} horizon_open={}",
                cause.as_str(),
                first_handled
            ),
        );
        if seq == 0 {
            // Nothing was spoken: the event is observed exactly like a buffered
            // reply whose commit refused (a policy override has no playback).
            if cause == PhraseCause::GateRejected {
                trace.fallback_reason = FallbackReason::PolicyOverride;
                self.record_unspoken_phrased_reply(generation_id, trace, now_ms);
            } else if let Some(generative) = self.generative.as_ref() {
                generative.note_failed();
            }
            if let Some(run) = self
                .generative
                .as_mut()
                .and_then(|generative| generative.phrase.run_mut(generation_id))
            {
                run.first_handled = true;
            }
        }
    }

    fn record_unspoken_phrased_reply(
        &mut self,
        generation_id: u64,
        trace: GenerationTrace,
        now_ms: u64,
    ) {
        let Some((event, context, fallback, seed)) = self
            .generative
            .as_mut()
            .and_then(|generative| generative.phrase.run_mut(generation_id))
            .map(|run| {
                (
                    run.event.clone(),
                    run.context.clone(),
                    run.fallback.clone(),
                    run.seed,
                )
            })
        else {
            return;
        };
        self.record_generation_calls(&trace);
        let deadline =
            DeadlineObservation::new(context.interaction_deadline, context.remaining_at_route_ms);
        // Nothing was spoken (phrase 0 refused at the commit boundary): the
        // same visible fallback as a buffered reply that ended on the gate
        // (issue #288). If playing it fails the event is observed as unseen.
        let (playback, route, cache_level, trace) =
            match self.play_fallback_directive(&event, &fallback, trace.clone(), now_ms, seed) {
                Ok(played) => (
                    played.playback,
                    played.route,
                    played.cache_level,
                    played.trace,
                ),
                Err(_) => (None, RouteClass::NonVerbalFallback, None, trace),
            };
        let handled = HandledEvent {
            playback,
            route,
            routing_latency_us: context.routing_latency_us,
            generation_latency_us: None,
            generation_trace: Some(trace),
            decision: context.decision.clone(),
            template_fallback: context.template_fallback,
            cache_level,
            deadline: Some(deadline),
            deferred_generation: false,
            admitted_bytes: None,
            correlation_id: Some(context.correlation_id.clone()),
        };
        self.record_handled_event(&event.event_id, &handled, now_ms);
    }

    /// The worker's final completion of a phrased reply that already spoke:
    /// the reply ends here, recorded as what it was, never as complete unless
    /// every planned phrase was committed.
    pub(crate) fn complete_phrased_reply(
        &mut self,
        mut completion: GenerationCompletion,
        run: PhraseRun,
    ) {
        let event_id = completion.request.source_event.event_id.clone();
        // A gate refusal of a later phrase leaves its decision as the last one.
        self.record_worker_output_decision(completion.output_decision.as_ref());
        let summary = match &completion.result {
            Ok(result) => match &result.disposition {
                GenerationDisposition::Phrased { summary } => Some(summary.clone()),
                _ => None,
            },
            Err(_) => None,
        };
        let spoke = run.has_committed();
        let chars = if spoke {
            self.finish_phrase_run(run, summary.as_ref())
        } else {
            // Phrase 0 was refused at the commit boundary: the event was
            // observed then, and nothing was spoken, so there is no reply to
            // record.
            0
        };
        if let (Some(reservation), Some(governor)) = (
            completion.context.budget_reservation.as_mut(),
            self.budget.as_mut(),
        ) {
            if spoke {
                reservation.settle(governor, chars, 0);
            } else {
                reservation.release(governor);
            }
        }
        if let Some(generative) = self.generative.as_mut() {
            if completion.result.is_err() {
                generative.note_failed();
            } else if completion.cancellation.is_cancelled() {
                generative.note_cancelled_or_stale();
            }
            generative.finish(completion.generation_id, &event_id);
        }
    }

    /// Close a phrased reply whose first phrase was already handled, once the
    /// worker's own completion arrives. Returns the characters synthesized so
    /// the caller can settle the budget.
    pub(crate) fn finish_phrase_run(
        &mut self,
        mut run: PhraseRun,
        summary: Option<&PhraseSummary>,
    ) -> u64 {
        if let Some(summary) = summary {
            run.planned_phrases = summary.planned_phrases;
            run.segmentation_version = summary.segmentation_version.clone();
        }
        let committed = run.commits.len() as u32;
        let all_committed = summary
            .is_some_and(|summary| summary.completed() && committed == summary.planned_phrases);
        let cause = if all_committed {
            None
        } else {
            Some(
                run.ended
                    .or_else(|| {
                        // The worker's account, when the commit side did not end it.
                        summary.and_then(|summary| match summary.end {
                            PhraseEnd::Completed => None,
                            PhraseEnd::Failed { failure, .. } => {
                                Some(PhraseCause::from_failure(failure))
                            }
                            PhraseEnd::Cancelled { .. } => run.cancel_cause,
                        })
                    })
                    .or(run.cancel_cause)
                    .unwrap_or(PhraseCause::Superseded),
            )
        };
        let end = match cause {
            None => PhraseResponseEnd::Completed,
            Some(cause) => PhraseResponseEnd::Partial { cause },
        };

        let event_id = run.event.event_id.clone();
        let response_id = run.generation_id;
        let chars = summary.map_or_else(|| run.committed_chars(), |summary| summary.tts_chars);
        let record = PhraseResponseRecord {
            response_id,
            event_id: event_id.clone(),
            segmentation_version: run.segmentation_version.clone(),
            planned_phrases: run.planned_phrases,
            commits: run.commits,
            end,
        };
        let detail = format!(
            "response={response_id} planned={} committed={committed} end={}",
            record.planned_phrases,
            match end {
                PhraseResponseEnd::Completed => "completed",
                PhraseResponseEnd::Partial { cause } => cause.as_str(),
            }
        );
        if let Some(generative) = self.generative.as_mut() {
            let metrics = &mut generative.phrase.metrics;
            metrics.responses += 1;
            match end {
                PhraseResponseEnd::Completed => metrics.completed += 1,
                PhraseResponseEnd::Partial { cause } => {
                    metrics.partial += 1;
                    match cause {
                        PhraseCause::SynthesisFailed(_)
                        | PhraseCause::CompileFailed
                        | PhraseCause::LookaheadStalled
                        | PhraseCause::StreamFailed(_)
                        | PhraseCause::NothingSpeakable => {
                            metrics.synthesis_failed_after_commit += 1;
                        }
                        PhraseCause::OperatorStop
                        | PhraseCause::BargeIn
                        | PhraseCause::Superseded
                        | PhraseCause::Shutdown
                        | PhraseCause::DeadlineExpired => metrics.cancelled_after_commit += 1,
                        PhraseCause::GateRejected | PhraseCause::SchedulerRejected(_) => {}
                    }
                }
            }
            generative.phrase.push_record(record);
        }
        self.audit_phrase(&event_id, "phrase_response_ended", detail);
        chars
    }
}
