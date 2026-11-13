//! Live chat-burst selection and deferred replies (issue #75 S1, #78 S2).
//!
//! This is the composition-side state machine behind
//! `ProductionApp::process_content_bytes` when chat-burst mode is enabled. It
//! joins two pure, clock-free pieces:
//!
//! * [`BurstEngine`] (reflex, #78): groups admitted public chat into bounded
//!   batches while chat is bursting and picks a Top-K deterministically;
//! * [`DeferredInbox`] (scheduler, #75): holds a selected reply while the
//!   performer is busy, for a bounded logical time.
//!
//! The lane owns no thread, timer or clock: every instant is the caller's
//! monotonic `at_ms`. It never dispatches anything itself; it returns events
//! for the app to run through the same `handle_event` path every other chat
//! event uses, so scheduler, operator stop, mute and output gates all apply.
//!
//! Trust boundaries (tested):
//! * *Busy state is never read from an event.* The lane is told whether the
//!   performer is available by the app, from the scheduler/speech lifecycle,
//!   the generation runtime and the operator mute flag.
//! * *Viewer text cannot pick a lane.* Classification is by the plane/kind the
//!   normalizer assigned (see `BurstEngine::offer`); a `priority_hint` is
//!   ignored for ranking and only counted.
//! * *Operator control never enters here.* Control commands use the
//!   authenticated ingress and `handle_control_reporting`; the lane only sees
//!   events that already passed content admission. Operator stop cancels
//!   everything held here with an explicit terminal outcome.
//! * *Every chat comment that enters the lane gets exactly one terminal
//!   outcome* ([`ChatOutcome`]); `admitted` must equal terminal outcomes plus
//!   what is still held, and a violation is counted, never repaired silently.

use aivtuber_domain::{EventEnvelope, ReflexContext};
use aivtuber_reflex::{
    AudienceConfig, AudienceConfigError, AudienceSignal, BatchRanker, BurstConfig,
    BurstConfigError, BurstEngine, Disposition, RetrievalScorer, TerminalOutcome,
};
use aivtuber_scheduler::{
    BusyCause, DeferredConfig, DeferredConfigError, DeferredInbox, DeferredItem, DeferredMetrics,
    DeferredOutcome, DeferredTerminal, ReadyDeferred, ReplyAvailability, ResponseDisposition,
    TimingReason,
};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

/// Hard ceiling on the logical lifetime of a deferred reply.
pub const MAX_REPLY_TTL_MS: u64 = aivtuber_scheduler::MAX_DEFERRED_TTL_MS;
/// Retained terminal records (oldest dropped, counted).
pub const MAX_CHAT_BURST_RECORDS: usize = 256;
/// Retained dispatch reports for deferred replies (oldest dropped, counted).
pub const MAX_DEFERRED_REPORTS: usize = 64;
/// Retained audience signals (#79; oldest dropped, counted).
pub const MAX_AUDIENCE_SIGNALS: usize = 64;

/// Operator-facing configuration. Validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatBurstConfig {
    pub burst: BurstConfig,
    pub deferred: DeferredConfig,
    /// How long a selected reply may wait for the performer.
    pub reply_ttl_ms: u64,
    /// #79: audience-signal aggregation of batched chat. `None` (the default)
    /// leaves batch resolution exactly as it was.
    pub audience: Option<AudienceConfig>,
}

impl Default for ChatBurstConfig {
    fn default() -> Self {
        Self {
            burst: BurstConfig::default(),
            deferred: DeferredConfig::default(),
            reply_ttl_ms: 20_000,
            audience: None,
        }
    }
}

/// Refusal raised for an out-of-bounds [`ChatBurstConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBurstConfigError(pub String);

impl std::fmt::Display for ChatBurstConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChatBurstConfigError {}

impl From<BurstConfigError> for ChatBurstConfigError {
    fn from(error: BurstConfigError) -> Self {
        Self(error.to_string())
    }
}

impl From<AudienceConfigError> for ChatBurstConfigError {
    fn from(error: AudienceConfigError) -> Self {
        Self(error.to_string())
    }
}

impl From<DeferredConfigError> for ChatBurstConfigError {
    fn from(error: DeferredConfigError) -> Self {
        Self(error.to_string())
    }
}

impl ChatBurstConfig {
    pub fn validate(&self) -> Result<(), ChatBurstConfigError> {
        self.burst.validate()?;
        self.deferred.validate()?;
        if let Some(audience) = &self.audience {
            audience.validate()?;
        }
        if self.reply_ttl_ms == 0 || self.reply_ttl_ms > self.deferred.max_ttl_ms {
            return Err(ChatBurstConfigError(
                "invalid chat burst configuration: reply_ttl_ms".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The single terminal outcome of a comment that entered the lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatOutcome {
    /// Handed to the performer path at once (quiet chat or a selected reply
    /// while the performer was available).
    DispatchedNow,
    /// Dispatch was attempted at once and the performer path failed.
    DispatchFailed,
    /// Valid but beyond the batch's Top-K / request capacity.
    NotSelectedCapacity,
    /// Its actor was selected too recently.
    SuppressedActorCooldown,
    /// Its actor already filled their per-batch share.
    SuppressedActorBatchCap,
    /// Its batch was shed because resolution fell behind.
    ShedPendingOverflow,
    /// Its actor erased their data while it was batched (#305).
    ErasedBeforeSelection,
    /// Operator stop cancelled it while it was batched.
    CancelledBeforeSelection,
    /// The app shut down before it was selected.
    ShutdownBeforeSelection,
    /// Selected (or quiet) but already older than the reply time-to-live when
    /// the performer path was reached: never answered late.
    ExpiredBeforeDispatch,
    /// #79: absorbed into an audience signal; the cluster's carrier comment
    /// stands for the crowd and has its own outcome.
    RepresentedByAggregate,
    /// It was deferred and reached the inbox's terminal outcome.
    Deferred(DeferredOutcome),
}

impl ChatOutcome {
    const IMMEDIATE_COUNT: usize = 11;

    /// Category among the outcomes the lane itself accounts. `Deferred(_)`
    /// outcomes are accounted by the inbox.
    fn immediate_category(self) -> Option<usize> {
        Some(match self {
            Self::DispatchedNow => 0,
            Self::DispatchFailed => 1,
            Self::NotSelectedCapacity => 2,
            Self::SuppressedActorCooldown => 3,
            Self::SuppressedActorBatchCap => 4,
            Self::ShedPendingOverflow => 5,
            Self::ErasedBeforeSelection => 6,
            Self::CancelledBeforeSelection => 7,
            Self::ShutdownBeforeSelection => 8,
            Self::ExpiredBeforeDispatch => 9,
            Self::RepresentedByAggregate => 10,
            Self::Deferred(_) => return None,
        })
    }
}

/// One terminal record. Event id and logical time only: no text, no actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatBurstRecord {
    pub event_id: String,
    pub at_ms: u64,
    pub outcome: ChatOutcome,
}

/// A reply that was dispatched from the inbox (not the event whose call
/// triggered the drain). Ids and the chosen asset only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeferredReplyReport {
    pub event_id: String,
    pub at_ms: u64,
    pub asset_id: Option<String>,
    pub dispatched: bool,
}

/// Observable counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatBurstMetrics {
    /// Chat comments that entered the lane (quiet or batched).
    pub admitted: u64,
    /// Terminal outcomes the lane accounts itself, indexed like
    /// [`ChatOutcome`] (dispatched now, failed, capacity, cooldown, batch cap,
    /// shed, erased, cancelled, shutdown, expired before dispatch,
    /// represented by aggregate).
    pub terminal: [u64; ChatOutcome::IMMEDIATE_COUNT],
    /// Not batchable (donation, game event, ...): the unchanged per-event path.
    pub bypassed: u64,
    /// Refused by the burst classifier or as a duplicate id of a held comment.
    pub refused: u64,
    /// Comments currently batched or waiting to be resolved.
    pub held: usize,
    /// Comments handed to the caller to dispatch or defer that have no
    /// outcome yet (selected by a batch, or quiet chat). Zero between calls.
    pub in_flight: u64,
    pub held_high_water: usize,
    /// Inbox counters (deferred replies).
    pub deferred: DeferredMetrics,
    /// Records or reports dropped because their bounded ring was full.
    pub records_dropped: u64,
    pub reports_dropped: u64,
    /// Selected replies that were deferred because the performer was busy,
    /// by trusted cause.
    pub deferred_speaking: u64,
    pub deferred_generating: u64,
    pub deferred_muted: u64,
    /// Deferred because a trusted source marked the live scene uninterruptible (#88).
    pub deferred_scene_protected: u64,
    /// Times `admitted != terminal + inbox.offered + held` was observed. Must
    /// be 0.
    pub invariant_violations: u64,
    /// Burst-engine counters worth surfacing.
    pub burst_batches_flushed: u64,
    pub burst_invariant_violations: u64,
    pub burst_priority_claims_ignored: u64,
    /// #79 audience aggregation: signals produced, comments they absorbed
    /// (their terminal outcome is `represented_by_aggregate`), and comments
    /// the safety guard kept individual.
    pub audience_signals: u64,
    pub audience_members_represented: u64,
    pub audience_guarded: u64,
    /// Signals dropped because their bounded ring was full.
    pub audience_signals_dropped: u64,
}

impl ChatBurstMetrics {
    pub fn terminal_total(&self) -> u64 {
        self.terminal.iter().sum()
    }

    /// Comments that ended in any terminal outcome, lane or inbox.
    pub fn accounted(&self) -> u64 {
        self.terminal_total() + self.deferred.terminal_total()
    }

    pub fn dispatched(&self) -> u64 {
        self.terminal[0] + self.deferred.count(DeferredOutcome::Dispatched)
    }
}

/// A comment handed to the caller to answer now or defer, with the logical
/// instant it arrived. The reply time-to-live is measured from arrival, so a
/// comment that waited in a batch (or whose batch was polled late) cannot be
/// answered after it has gone stale.
#[derive(Debug)]
pub(crate) struct Candidate {
    pub event: EventEnvelope,
    pub arrived_ms: u64,
}

/// What the lane decided for an offered event.
#[derive(Debug)]
pub(crate) enum Offer {
    /// Not a batchable chat comment: take the unchanged per-event path.
    Bypass(EventEnvelope),
    /// Chat is quiet: reply now or defer, as the performer allows.
    Quiet(Candidate),
    /// Held in a batch; it will get exactly one terminal outcome.
    Held,
    /// Refused; nothing is dispatched.
    Refused,
}

#[derive(Debug)]
enum Held {
    Envelope {
        event: Box<EventEnvelope>,
        arrived_ms: u64,
    },
    Erased,
    Cancelled,
}

pub(crate) struct ChatBurstLane {
    engine: BurstEngine,
    /// #78 S3: optional retrieval-augmented local score. `None` keeps the
    /// text heuristic alone.
    scorer: Option<RetrievalScorer>,
    /// #78 S3: optional batch ranker (Jev). At most one call per
    /// [`ChatBurstLane::resolve_ready`] step, and none at shutdown.
    ranker: Option<Box<dyn BatchRanker + Send>>,
    held: BTreeMap<String, Held>,
    inbox: DeferredInbox<EventEnvelope>,
    reply_ttl_ms: u64,
    metrics: ChatBurstMetrics,
    records: VecDeque<ChatBurstRecord>,
    reports: VecDeque<DeferredReplyReport>,
    signals: VecDeque<AudienceSignal>,
    last_ms: u64,
}

/// Source-scoped fairness key. Length-prefixed so two different
/// `(source, actor)` pairs can never produce the same key.
fn actor_key(event: &EventEnvelope) -> Option<String> {
    event
        .actor_id
        .as_ref()
        .map(|actor| format!("{}:{}:{}", event.source.len(), event.source, actor))
}

/// Deterministic local engagement score in `0..=1000`.
///
/// A cheap text heuristic, not a model: a question is worth more than a
/// statement, a short-but-not-trivial comment more than a one-word reaction,
/// and a comment made of one or two repeated characters scores zero so spam
/// cannot outrank real chat. It reads only the bounded comment text; nothing
/// the viewer writes can raise it above 1000 or buy bypass of any gate.
pub(crate) fn local_score_milli(text: &str) -> u16 {
    let chars = text.chars().count();
    if chars == 0 {
        return 0;
    }
    let distinct = text
        .chars()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if chars >= 6 && distinct <= 2 {
        return 0;
    }
    let mut score = 100_u32;
    if text.contains('?') || text.contains('？') {
        score += 400;
    }
    score += (chars.min(60) as u32) * 5;
    score.min(1000) as u16
}

impl ChatBurstLane {
    pub(crate) fn new(config: ChatBurstConfig) -> Result<Self, ChatBurstConfigError> {
        config.validate()?;
        let mut engine = BurstEngine::new(config.burst)?;
        engine.set_audience(config.audience)?;
        Ok(Self {
            engine,
            scorer: None,
            ranker: None,
            held: BTreeMap::new(),
            inbox: DeferredInbox::new(config.deferred)?,
            reply_ttl_ms: config.reply_ttl_ms,
            metrics: ChatBurstMetrics::default(),
            records: VecDeque::new(),
            reports: VecDeque::new(),
            signals: VecDeque::new(),
            last_ms: 0,
        })
    }

    pub(crate) fn set_scorer(&mut self, scorer: RetrievalScorer) {
        self.scorer = Some(scorer);
    }

    pub(crate) fn set_ranker(&mut self, ranker: Box<dyn BatchRanker + Send>) {
        self.ranker = Some(ranker);
    }

    pub(crate) fn metrics(&self) -> ChatBurstMetrics {
        let burst = self.engine.metrics();
        ChatBurstMetrics {
            held: self.held.len(),
            deferred: self.inbox.metrics(),
            burst_batches_flushed: burst.batches_flushed,
            burst_invariant_violations: burst.invariant_violations,
            burst_priority_claims_ignored: burst.priority_claims_ignored,
            audience_signals: burst.audience_signals,
            audience_members_represented: burst.audience_members_represented,
            audience_guarded: burst.audience_guarded.values().sum(),
            ..self.metrics
        }
    }

    pub(crate) fn take_records(&mut self, max: usize) -> Vec<ChatBurstRecord> {
        let take = max.min(self.records.len());
        self.records.drain(..take).collect()
    }

    pub(crate) fn take_reports(&mut self, max: usize) -> Vec<DeferredReplyReport> {
        let take = max.min(self.reports.len());
        self.reports.drain(..take).collect()
    }

    /// Remove and return up to `max` audience signals (#79), oldest first.
    /// Signals hold ids and counts only, never viewer text.
    pub(crate) fn take_audience_signals(&mut self, max: usize) -> Vec<AudienceSignal> {
        let take = max.min(self.signals.len());
        self.signals.drain(..take).collect()
    }

    pub(crate) fn push_report(&mut self, report: DeferredReplyReport) {
        if self.reports.len() >= MAX_DEFERRED_REPORTS {
            self.reports.pop_front();
            self.metrics.reports_dropped += 1;
        }
        self.reports.push_back(report);
    }

    fn advance(&mut self, now_ms: u64) -> u64 {
        // Logical ordering only: a regressing clock is clamped.
        self.last_ms = self.last_ms.max(now_ms);
        self.last_ms
    }

    fn record(&mut self, event_id: String, at_ms: u64, outcome: ChatOutcome) {
        if let Some(category) = outcome.immediate_category() {
            self.metrics.terminal[category] += 1;
        }
        if self.records.len() >= MAX_CHAT_BURST_RECORDS {
            self.records.pop_front();
            self.metrics.records_dropped += 1;
        }
        self.records.push_back(ChatBurstRecord {
            event_id,
            at_ms,
            outcome,
        });
    }

    fn record_deferred(&mut self, terminals: Vec<DeferredTerminal>, at_ms: u64) {
        for terminal in terminals {
            self.record(terminal.id, at_ms, ChatOutcome::Deferred(terminal.outcome));
        }
    }

    fn check_invariant(&mut self) {
        let accounted = self.metrics.terminal_total()
            + self.inbox.metrics().offered
            + self.held.len() as u64
            + self.metrics.in_flight;
        if self.metrics.admitted != accounted {
            self.metrics.invariant_violations += 1;
        }
    }

    /// Classify one admitted content event.
    pub(crate) fn offer(&mut self, event: EventEnvelope, now_ms: u64) -> Offer {
        let now_ms = self.advance(now_ms);
        // A comment whose id is already held would make the held map ambiguous
        // for the engine's outcome; the duplicate is refused, never merged.
        if self.held.contains_key(&event.event_id) {
            self.metrics.refused += 1;
            return Offer::Refused;
        }
        let text = event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str);
        let mut score = text.map_or(0, local_score_milli);
        if let (Some(scorer), Some(text)) = (self.scorer.as_mut(), text) {
            score = scorer.score(text, score, now_ms);
        }
        let (disposition, shed) = self.engine.offer(&event, now_ms, score);
        self.apply_resolutions(shed.into_iter().map(|shed| shed.record), now_ms, None);
        match disposition {
            Disposition::Immediate(_) => {
                self.metrics.bypassed += 1;
                Offer::Bypass(event)
            }
            Disposition::Refused(_) => {
                self.metrics.refused += 1;
                Offer::Refused
            }
            Disposition::QuietPassthrough => {
                self.metrics.admitted += 1;
                self.metrics.in_flight += 1;
                self.check_invariant();
                Offer::Quiet(Candidate {
                    event,
                    arrived_ms: now_ms,
                })
            }
            Disposition::Batched => {
                self.metrics.admitted += 1;
                self.held.insert(
                    event.event_id.clone(),
                    Held::Envelope {
                        event: Box::new(event),
                        arrived_ms: now_ms,
                    },
                );
                self.metrics.held_high_water = self.metrics.held_high_water.max(self.held.len());
                self.check_invariant();
                Offer::Held
            }
        }
    }

    /// Flush a window that has expired, resolve every ready batch with the
    /// deterministic selector, and return the selected events in order. Every
    /// other comment of those batches gets its terminal outcome here.
    pub(crate) fn resolve_ready(&mut self, now_ms: u64) -> Vec<Candidate> {
        let now_ms = self.advance(now_ms);
        let shed = self.engine.poll(now_ms);
        let mut selected = Vec::new();
        self.apply_resolutions(shed.into_iter().map(|shed| shed.record), now_ms, None);
        let context = ReflexContext {
            now_ms: Some(now_ms),
            ..ReflexContext::default()
        };
        // A ranker call blocks this step for at most its deadline, so only the
        // first ready batch of a step may use it; later batches resolve with
        // the deterministic order. The ranker only reorders; policy re-applies.
        let mut ranker_available = true;
        loop {
            let ranker = if ranker_available {
                self.ranker
                    .as_mut()
                    .map(|ranker| &mut **ranker as &mut dyn BatchRanker)
            } else {
                None
            };
            let used = ranker.is_some();
            let Some(resolution) = self.engine.resolve_next(&context, ranker) else {
                break;
            };
            if used {
                ranker_available = false;
            }
            self.apply_resolutions(
                std::iter::once(resolution.record),
                now_ms,
                Some(&mut selected),
            );
        }
        self.check_invariant();
        selected
    }

    fn apply_resolutions(
        &mut self,
        records: impl Iterator<Item = aivtuber_reflex::BatchReplayRecord>,
        now_ms: u64,
        mut selected: Option<&mut Vec<Candidate>>,
    ) {
        for record in records {
            for signal in &record.audience_signals {
                if self.signals.len() >= MAX_AUDIENCE_SIGNALS {
                    self.signals.pop_front();
                    self.metrics.audience_signals_dropped += 1;
                }
                self.signals.push_back(signal.clone());
            }
            let order: BTreeMap<&str, usize> = record
                .selected
                .iter()
                .enumerate()
                .map(|(rank, id)| (id.as_str(), rank))
                .collect();
            let mut picked: Vec<(usize, Candidate)> = Vec::new();
            for outcome in &record.outcomes {
                let state = self.held.remove(&outcome.event_id);
                let terminal = match (state, outcome.outcome) {
                    (Some(Held::Erased), _) => Some(ChatOutcome::ErasedBeforeSelection),
                    (Some(Held::Cancelled), _) => Some(ChatOutcome::CancelledBeforeSelection),
                    (
                        Some(Held::Envelope { event, arrived_ms }),
                        TerminalOutcome::Selected { .. },
                    ) => {
                        if let Some(&rank) = order.get(outcome.event_id.as_str()) {
                            picked.push((
                                rank,
                                Candidate {
                                    event: *event,
                                    arrived_ms,
                                },
                            ));
                        }
                        None
                    }
                    (_, TerminalOutcome::NotSelectedCapacity) => {
                        Some(ChatOutcome::NotSelectedCapacity)
                    }
                    (_, TerminalOutcome::SuppressedActorCooldown) => {
                        Some(ChatOutcome::SuppressedActorCooldown)
                    }
                    (_, TerminalOutcome::SuppressedActorBatchCap) => {
                        Some(ChatOutcome::SuppressedActorBatchCap)
                    }
                    (_, TerminalOutcome::ShedPendingOverflow) => {
                        Some(ChatOutcome::ShedPendingOverflow)
                    }
                    (_, TerminalOutcome::RepresentedByAggregate { .. }) => {
                        Some(ChatOutcome::RepresentedByAggregate)
                    }
                    // A quiet passthrough is never held, and a selected
                    // comment without its envelope cannot happen: the lane
                    // owns both. Counted as a violation rather than guessed.
                    (_, TerminalOutcome::QuietPassthrough | TerminalOutcome::Selected { .. }) => {
                        self.metrics.invariant_violations += 1;
                        Some(ChatOutcome::ShedPendingOverflow)
                    }
                };
                if let Some(terminal) = terminal {
                    self.record(outcome.event_id.clone(), now_ms, terminal);
                }
            }
            picked.sort_by_key(|(rank, _)| *rank);
            match selected.as_deref_mut() {
                Some(sink) => {
                    // Handed to the caller: in flight until it dispatches or
                    // defers each one.
                    self.metrics.in_flight += picked.len() as u64;
                    sink.extend(picked.into_iter().map(|(_, candidate)| candidate));
                }
                None => {
                    // Shed batches select nothing; anything picked here would
                    // be a classifier bug, so account it instead of dropping.
                    for (_, candidate) in picked {
                        self.record(
                            candidate.event.event_id,
                            now_ms,
                            ChatOutcome::ShedPendingOverflow,
                        );
                    }
                }
            }
        }
    }

    /// Whether a handed-out comment is still young enough to answer. A stale
    /// one gets its terminal outcome here and must not be dispatched.
    pub(crate) fn still_fresh(&mut self, candidate: &Candidate, now_ms: u64) -> bool {
        let now_ms = self.advance(now_ms);
        if now_ms < candidate.arrived_ms.saturating_add(self.reply_ttl_ms) {
            return true;
        }
        self.metrics.in_flight = self.metrics.in_flight.saturating_sub(1);
        self.record(
            candidate.event.event_id.clone(),
            now_ms,
            ChatOutcome::ExpiredBeforeDispatch,
        );
        self.check_invariant();
        false
    }

    /// The comment was handed to the performer path now.
    pub(crate) fn dispatched_now(&mut self, event_id: &str, now_ms: u64, ok: bool) {
        let now_ms = self.advance(now_ms);
        self.metrics.in_flight = self.metrics.in_flight.saturating_sub(1);
        let outcome = if ok {
            ChatOutcome::DispatchedNow
        } else {
            ChatOutcome::DispatchFailed
        };
        self.record(event_id.to_owned(), now_ms, outcome);
        self.check_invariant();
    }

    /// Hold a selected or quiet reply until the performer is available. The
    /// expiry is `arrival + reply_ttl`; the cause is a trusted runtime input.
    /// The caller has already checked [`Self::still_fresh`].
    pub(crate) fn defer(&mut self, candidate: Candidate, now_ms: u64, cause: BusyCause) {
        let Candidate { event, arrived_ms } = candidate;
        let now_ms = self.advance(now_ms);
        self.metrics.in_flight = self.metrics.in_flight.saturating_sub(1);
        match cause {
            BusyCause::Speaking => self.metrics.deferred_speaking += 1,
            BusyCause::Generating => self.metrics.deferred_generating += 1,
            BusyCause::Muted => self.metrics.deferred_muted += 1,
            BusyCause::SceneProtected => self.metrics.deferred_scene_protected += 1,
        }
        let reason = match cause {
            BusyCause::Muted => TimingReason::OperatorMuted,
            _ => TimingReason::PerformerBusy,
        };
        let item = DeferredItem {
            id: event.event_id.clone(),
            actor: actor_key(&event),
            disposition: ResponseDisposition::when_available(
                arrived_ms.saturating_add(self.reply_ttl_ms),
                // #80 (topic context) is not available yet: supersession is
                // keyed by the event id only, so a revised/replayed event id
                // replaces its own earlier deferral.
                Some(event.event_id.clone()),
                reason,
            ),
            payload: event,
        };
        let (_result, terminals) = self.inbox.defer(item, now_ms);
        self.record_deferred(terminals, now_ms);
        self.check_invariant();
    }

    /// Sweep expiry and hand out the best deferred reply when the performer
    /// is available.
    pub(crate) fn poll_inbox(
        &mut self,
        now_ms: u64,
        availability: ReplyAvailability,
    ) -> Option<ReadyDeferred<EventEnvelope>> {
        let now_ms = self.advance(now_ms);
        let (terminals, ready) = self.inbox.poll(now_ms, availability);
        self.record_deferred(terminals, now_ms);
        self.check_invariant();
        ready
    }

    pub(crate) fn complete_deferred(
        &mut self,
        ready: ReadyDeferred<EventEnvelope>,
        outcome: DeferredOutcome,
        now_ms: u64,
    ) {
        let now_ms = self.advance(now_ms);
        let terminal = self.inbox.complete(ready, outcome, now_ms);
        self.record_deferred(vec![terminal], now_ms);
        self.check_invariant();
    }

    /// An actor erased their data: nothing of theirs may dispatch later.
    pub(crate) fn erase_actor(&mut self, source: &str, actor: &str, now_ms: u64) {
        let now_ms = self.advance(now_ms);
        let key = format!("{}:{}:{}", source.len(), source, actor);
        let terminals = self.inbox.cancel_actor(&key, DeferredOutcome::Erased);
        self.record_deferred(terminals, now_ms);
        for state in self.held.values_mut() {
            if let Held::Envelope { event, .. } = state
                && event.source == source
                && event.actor_id.as_deref() == Some(actor)
            {
                *state = Held::Erased;
            }
        }
        self.engine.forget_actor(actor);
        self.check_invariant();
    }

    /// Operator stop: cancel everything held, with an explicit outcome.
    pub(crate) fn cancel_all(&mut self, now_ms: u64) {
        let now_ms = self.advance(now_ms);
        let terminals = self.inbox.cancel_all(DeferredOutcome::OperatorCancelled);
        self.record_deferred(terminals, now_ms);
        for state in self.held.values_mut() {
            if matches!(state, Held::Envelope { .. }) {
                *state = Held::Cancelled;
            }
        }
        self.check_invariant();
    }

    /// Shutdown: nothing dispatches again. Held comments and deferred replies
    /// all end with an explicit outcome.
    pub(crate) fn shutdown(&mut self, now_ms: u64) {
        let now_ms = self.advance(now_ms);
        let terminals = self.inbox.cancel_all(DeferredOutcome::ShutdownDropped);
        self.record_deferred(terminals, now_ms);
        let context = ReflexContext::default();
        let resolutions = self.engine.shutdown(now_ms, &context);
        let ids: Vec<String> = resolutions
            .iter()
            .flat_map(|resolution| resolution.record.outcomes.iter())
            .map(|outcome| outcome.event_id.clone())
            .collect();
        for id in ids {
            if self.held.remove(&id).is_some() {
                self.record(id, now_ms, ChatOutcome::ShutdownBeforeSelection);
            }
        }
        // Anything still held would be a lost comment; account it.
        for id in std::mem::take(&mut self.held).into_keys() {
            self.metrics.invariant_violations += 1;
            self.record(id, now_ms, ChatOutcome::ShutdownBeforeSelection);
        }
        self.check_invariant();
    }

    pub(crate) fn pending_deferred(&self) -> usize {
        self.inbox.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_score_prefers_questions_and_demotes_spam() {
        assert_eq!(local_score_milli(""), 0);
        assert_eq!(local_score_milli("aaaaaaaa"), 0);
        assert_eq!(local_score_milli("abababab"), 0);
        let statement = local_score_milli("that was a great fight");
        let question = local_score_milli("how did you beat that boss?");
        assert!(question > statement);
        assert!(statement > local_score_milli("lol"));
        assert!(local_score_milli(&"?".repeat(500)) <= 1000);
        assert!(local_score_milli(&"a? ".repeat(500)) <= 1000);
    }

    #[test]
    fn config_validation_refuses_out_of_bounds_fields() {
        assert!(ChatBurstConfig::default().validate().is_ok());
        let zero_ttl = ChatBurstConfig {
            reply_ttl_ms: 0,
            ..ChatBurstConfig::default()
        };
        assert!(zero_ttl.validate().is_err());
        let beyond_horizon = ChatBurstConfig {
            reply_ttl_ms: ChatBurstConfig::default().deferred.max_ttl_ms + 1,
            ..ChatBurstConfig::default()
        };
        assert!(beyond_horizon.validate().is_err());
        let bad_burst = ChatBurstConfig {
            burst: BurstConfig {
                burst_enter_count: 1,
                ..BurstConfig::default()
            },
            ..ChatBurstConfig::default()
        };
        assert!(bad_burst.validate().is_err());
    }

    #[test]
    fn actor_keys_never_collide_across_sources() {
        let mut a = sample("a1");
        a.source = "ab".to_owned();
        a.actor_id = Some("c".to_owned());
        let mut b = sample("a2");
        b.source = "a".to_owned();
        b.actor_id = Some("bc".to_owned());
        assert_ne!(actor_key(&a), actor_key(&b));
    }

    use aivtuber_reflex::{BatchEngagementRequest, BatchRankFailure};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Reverse(Arc<AtomicU64>);

    impl BatchRanker for Reverse {
        fn rank(
            &mut self,
            request: &BatchEngagementRequest,
        ) -> Result<Vec<String>, BatchRankFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(request
                .candidates
                .iter()
                .rev()
                .map(|candidate| candidate.event_id.clone())
                .collect())
        }
    }

    struct Hostile;

    impl BatchRanker for Hostile {
        fn rank(&mut self, _: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure> {
            // Unknown ids and repeats: nothing here may add or revive anyone.
            Ok(vec!["not-in-batch".to_owned(), "also-not".to_owned()])
        }
    }

    fn small_lane() -> ChatBurstLane {
        ChatBurstLane::new(ChatBurstConfig {
            burst: BurstConfig {
                window_ms: 1_000,
                burst_enter_count: 2,
                max_candidates: 3,
                top_k: 1,
                max_per_actor: 1,
                actor_cooldown_ms: 0,
                ..BurstConfig::default()
            },
            ..ChatBurstConfig::default()
        })
        .expect("lane")
    }

    fn feed(lane: &mut ChatBurstLane, count: u64) {
        for i in 1..=count {
            let mut event = sample(&format!("e{i}"));
            event.sequence = i;
            event.actor_id = Some(format!("viewer{i}"));
            let _ = lane.offer(event, i);
        }
    }

    fn selected_ids(lane: &mut ChatBurstLane) -> Vec<String> {
        lane.resolve_ready(10)
            .into_iter()
            .map(|candidate| candidate.event.event_id)
            .collect()
    }

    #[test]
    fn ranker_is_used_once_per_step_and_only_reorders() {
        let calls = Arc::new(AtomicU64::new(0));
        let mut lane = small_lane();
        lane.set_ranker(Box::new(Reverse(calls.clone())));
        feed(&mut lane, 7);
        // e1 was quiet; e2..e4 and e5..e7 are two ready batches.
        let selected = selected_ids(&mut lane);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one ranker call per step");
        // The ranked batch promoted its last comment; the other batch used the
        // deterministic order. Top-K 1 per batch still holds.
        assert_eq!(selected, vec!["e4".to_owned(), "e5".to_owned()]);
        assert_eq!(lane.metrics().invariant_violations, 0);
    }

    #[test]
    fn hostile_ranker_cannot_add_or_change_the_deterministic_selection() {
        let mut ranked = small_lane();
        ranked.set_ranker(Box::new(Hostile));
        feed(&mut ranked, 4);
        let mut plain = small_lane();
        feed(&mut plain, 4);
        assert_eq!(selected_ids(&mut ranked), selected_ids(&mut plain));
        assert_eq!(ranked.metrics().invariant_violations, 0);
    }

    #[test]
    fn retrieval_score_is_deterministic_and_never_revives_spam() {
        use aivtuber_reflex::{
            IndexedAsset, ReactionAxisEmbedder, RetrievalMetadata, RetrievalScoreConfig,
            SemanticIndex, SimilarityMetric,
        };
        let build = || {
            let index = SemanticIndex::new(
                RetrievalMetadata {
                    retriever_version: "t".to_owned(),
                    embedding_model: "starter-semantic".to_owned(),
                    index_version: "1".to_owned(),
                    asset_compiler_version: "1".to_owned(),
                    similarity_metric: SimilarityMetric::Cosine,
                    tie_break_rule: "similarity_desc_then_asset_id_asc".to_owned(),
                },
                vec![IndexedAsset {
                    asset_id: "agree".to_owned(),
                    asset_identity: None,
                    embedding: vec![1.0, 0.0, 0.0],
                }],
            )
            .expect("index");
            RetrievalScorer::new(
                index,
                Box::new(ReactionAxisEmbedder::new(3)),
                RetrievalScoreConfig::default(),
            )
            .expect("scorer")
        };
        let run = || {
            let mut lane = small_lane();
            lane.set_scorer(build());
            for (i, text) in ["hi", "wow!", "aaaaaaaa", "what is that?", "nice one", "ok"]
                .iter()
                .enumerate()
            {
                let mut event = sample(&format!("e{i}"));
                event.sequence = i as u64;
                event.actor_id = Some(format!("viewer{i}"));
                event
                    .payload
                    .insert("text".to_owned(), serde_json::json!(text));
                let _ = lane.offer(event, i as u64);
            }
            selected_ids(&mut lane)
        };
        assert_eq!(run(), run());
        let mut scorer = build();
        assert_eq!(
            scorer.score("aaaaaaaa", local_score_milli("aaaaaaaa"), 0),
            0
        );
    }

    fn sample(id: &str) -> EventEnvelope {
        use aivtuber_domain::{
            EVENT_SCHEMA_VERSION, EventKind, SecurityPlane, SourceClass, TrustLevel,
        };
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: id.to_owned(),
            correlation_id: "corr".to_owned(),
            sequence: 1,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([("text".to_owned(), serde_json::json!("hello there"))]),
        }
    }
}
