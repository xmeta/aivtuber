//! Bounded idle initiative (issue #87 S1).
//!
//! When no chat arrives for a while the performer would otherwise sit silent.
//! The [`InitiativeDirector`] decides, in deterministic code, *whether* to
//! speak on its own; *what* is said comes from the existing generative path
//! through a typed request with a closed [`InitiativeKind`]
//! (`aivtuber_domain::InitiativeContext`). Model output is data: it goes
//! through the same publication containment gate and scheduler admission as a
//! chat reply and gains no authority.
//!
//! # Decision
//!
//! [`InitiativeDirector::evaluate`] is a pure function of its inputs and its
//! own bounded state. All time is the logical/monotonic `at_ms` the caller
//! passes (never a wall clock), so a replay of the same input trace makes the
//! same decisions. It fires only when every one of these holds, checked in
//! this order (the first failing one is the suppression reason):
//!
//! 1. the operator toggle is on (an off toggle is not counted: nothing is
//!    being suppressed that the operator wanted);
//! 2. no chat or speech arrived for `idle_ms` (not counted: this is waiting,
//!    not suppression);
//! 3. the performer is not muted and no operator stop is recent;
//! 4. the conversation floor is free (#81), nobody is speaking, no playback or
//!    generation is live, the content queue and the deferred inbox are empty;
//! 5. the performer has been quiet for a short settle time;
//! 6. the budget allows it: at most one per [`MIN_GAP_MS`] and
//!    [`WINDOW_MAX`] per [`WINDOW_MS`].
//!
//! # Kind and topic (S2)
//!
//! The kind alternates between `idle_observation` and `audience_question`.
//! `revisit_topic` stays in the wire enum but is never chosen: there is no
//! real topic state to revisit yet (#80), and a model asked to "come back to
//! something said earlier" invents it. The request carries no reply summaries
//! for the same reason (the first invented topic fed back into every later
//! request). What an initiative may talk about is the operator's topic list
//! (`set_topics`): one topic per initiative, round-robin in logical time,
//! skipping the topics used in the last N = min([`TOPIC_RECENT_MAX`], len-1)
//! initiatives. With no topic the initiative runs without one and is counted.
//! The choice is charged when the director fires, like the budget, so a failed
//! generation never re-picks.
//!
//! The budget is charged when the director fires, not when the reply
//! succeeds, so a failing backend can never turn into a retry loop.
//!
//! # One early re-fire after a silent repeat refusal (#541)
//!
//! A repeat refused before anything was spoken (#89, phrase 0) is the one
//! failure that is not a backend fault, and with the budget charged at fire it
//! would cost a whole extra [`MIN_GAP_MS`] of dead air. The host reports it
//! through [`InitiativeDirector::repeat_refused`], which grants a single early
//! fire: the next evaluation may fire once [`REFIRE_GAP_MS`] has passed since
//! the refusal instead of [`MIN_GAP_MS`] since the fire. The grant is
//! one-shot and never chains: a refused replacement grants nothing and waits
//! the normal gap, and `failed` (any backend or gate failure) never grants.
//! The early fire still passes every other gate in the list above, still
//! counts in the window, and is dropped by chat, an operator stop or `clear`.
//! The replacement is the next kind and topic in the usual rotation, and the
//! refused text joins the remembered texts so the request asks the model to
//! avoid it.
//!
//! # Bounds
//!
//! The firing window holds at most [`WINDOW_MAX`] times, the decision log
//! [`LOG_CAPACITY`] records, the remembered initiative texts
//! [`RECENT_TEXTS_MAX`] entries of at most [`TEXT_MAX_CHARS`] characters, the
//! topic list [`aivtuber_domain::MAX_TOPICS`] entries and the used-topic ring
//! [`TOPIC_RECENT_MAX`], and every counter saturates. Nothing grows with run length.
//!
//! # Preemption
//!
//! The director never preempts. Chat or speech arriving calls
//! [`InitiativeDirector::note_chat`], and the host cancels whatever initiative
//! is still pending through the existing cancellation and scheduler paths
//! (the initiative is a `Background` item, below every chat reply).

use std::collections::VecDeque;

use aivtuber_domain::{
    DomainValidationError, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, InitiativeContext,
    InitiativeKind, MAX_THINKING_TEXT_BYTES, PrivacyClass, ReflexContext, RetrievalSnapshot,
    SecurityPlane, SourceClass, ThinkingContent, ThinkingContextSource, ThinkingRequest,
    TrustLevel, topic_thinking_content,
};

/// Default seconds without chat before the performer may speak on its own.
pub const DEFAULT_IDLE_SECS: u64 = 45;
/// Longest idle threshold the environment may set (one hour).
pub const MAX_IDLE_SECS: u64 = 3_600;
/// Shortest idle threshold the environment may set.
pub const MIN_IDLE_SECS: u64 = 5;
/// Minimum logical gap between two initiatives.
pub const MIN_GAP_MS: u64 = 60_000;
/// Length of the sliding window the initiative budget applies to.
pub const WINDOW_MS: u64 = 600_000;
/// Most initiatives inside one [`WINDOW_MS`]. Ten, so at the one-a-minute
/// gap the window never binds by itself (#456: six left 3-minute silences);
/// it stays as a ceiling should the gap ever be shortened.
pub const WINDOW_MAX: usize = 10;
/// How long the performer must have been quiet before it may speak again.
pub const SETTLE_MS: u64 = 4_000;
/// After a silent repeat refusal, how long until the one early re-fire may
/// happen (it still needs the [`SETTLE_MS`] quiet time and every other gate).
pub const REFIRE_GAP_MS: u64 = 5_000;
/// How long an operator stop keeps the initiative quiet.
pub const STOP_HOLD_MS: u64 = 300_000;
/// Decision records kept for replay and diagnostics.
pub const LOG_CAPACITY: usize = 32;
/// Own initiative texts remembered so a request can avoid repeating them.
pub const RECENT_TEXTS_MAX: usize = 3;
/// Characters kept per remembered text and per reply summary.
pub const TEXT_MAX_CHARS: usize = 160;
/// Most recently used topics remembered; the skip window is
/// `min(TOPIC_RECENT_MAX, topics - 1)`.
pub const TOPIC_RECENT_MAX: usize = 5;
/// The prefix of an initiative event id; also how a reply is recognised.
pub const EVENT_ID_PREFIX: &str = "initiative-";
/// The gesture played when no reply can be generated. Code-owned: nothing a
/// viewer or a model writes can select it.
pub const IDLE_GESTURE_PRESET: &str = "tilt_head_small";
pub const IDLE_GESTURE_AMPLITUDE: f64 = 0.7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitiativeConfig {
    /// The operator toggle at start (the local profile defaults it on).
    pub enabled: bool,
    pub idle_ms: u64,
}

impl Default for InitiativeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_ms: DEFAULT_IDLE_SECS * 1_000,
        }
    }
}

impl InitiativeConfig {
    /// A config from the idle seconds, refusing a value outside the bounds.
    pub fn with_idle_secs(enabled: bool, secs: u64) -> Result<Self, String> {
        if !(MIN_IDLE_SECS..=MAX_IDLE_SECS).contains(&secs) {
            return Err(format!(
                "AIVTUBER_INITIATIVE_IDLE_SECS must be in {MIN_IDLE_SECS}..={MAX_IDLE_SECS}"
            ));
        }
        Ok(Self {
            enabled,
            idle_ms: secs * 1_000,
        })
    }
}

/// Trusted facts the host reads from runtime state for one evaluation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InitiativeInputs {
    pub now_ms: u64,
    pub muted: bool,
    /// The #81 floor is held by a person (speaking, finishing or queued).
    pub floor_held: bool,
    /// A performer speech item is live or queued.
    pub speaking: bool,
    /// A generated reply is in flight.
    pub generating: bool,
    /// Untrusted content is waiting to be processed.
    pub content_queued: bool,
    /// A deferred chat reply is waiting for the performer.
    pub deferred_pending: bool,
}

/// Why an initiative did not fire although the idle time had elapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Suppression {
    Muted,
    OperatorStopped,
    FloorHeld,
    Speaking,
    Generating,
    ContentQueued,
    DeferredPending,
    Settling,
    BudgetGap,
    BudgetWindow,
}

impl Suppression {
    pub const ALL: [Self; 10] = [
        Self::Muted,
        Self::OperatorStopped,
        Self::FloorHeld,
        Self::Speaking,
        Self::Generating,
        Self::ContentQueued,
        Self::DeferredPending,
        Self::Settling,
        Self::BudgetGap,
        Self::BudgetWindow,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Muted => "muted",
            Self::OperatorStopped => "operator_stopped",
            Self::FloorHeld => "floor_held",
            Self::Speaking => "speaking",
            Self::Generating => "generating",
            Self::ContentQueued => "content_queued",
            Self::DeferredPending => "deferred_pending",
            Self::Settling => "settling",
            Self::BudgetGap => "budget_gap",
            Self::BudgetWindow => "budget_window",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|reason| *reason == self)
            .unwrap_or(0)
    }
}

/// What preempted a pending initiative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preemption {
    Chat,
    Speech,
    OperatorStop,
}

impl Preemption {
    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Speech => "speech",
            Self::OperatorStop => "operator_stop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Not idle long enough, disabled, or an initiative is already pending.
    Wait,
    Suppressed(Suppression),
    Fire {
        event_id: String,
        kind: InitiativeKind,
        /// The operator topic chosen for it, if the list is not empty.
        topic: Option<TopicPick>,
    },
}

/// The topic a firing initiative talks about: its position in the list (what
/// the decision log and audit record) and its text (what the request carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicPick {
    pub index: usize,
    pub text: String,
}

/// One recorded decision, for replay comparison and diagnostics. Edge
/// triggered: a suppression is recorded when its reason changes, not on every
/// evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `topic` is the index in the operator list, never the text.
    Fired {
        kind: InitiativeKind,
        topic: Option<usize>,
    },
    Suppressed(Suppression),
    Preempted(Preemption),
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    pub seq: u64,
    pub at_ms: u64,
    pub outcome: Outcome,
}

/// Counters for status and diagnose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InitiativeMetrics {
    pub fired: u64,
    pub completed: u64,
    pub failed: u64,
    pub preempted_chat: u64,
    pub preempted_speech: u64,
    pub preempted_stop: u64,
    /// Initiatives that fired with an operator topic.
    pub topics_used: u64,
    /// Initiatives that fired with no topic (the list was empty).
    pub without_topic: u64,
    pub suppressed: [u64; Suppression::ALL.len()],
}

impl InitiativeMetrics {
    pub fn suppressed_by(&self, reason: Suppression) -> u64 {
        self.suppressed[reason.index()]
    }

    pub fn suppressed_total(&self) -> u64 {
        self.suppressed
            .iter()
            .fold(0_u64, |sum, count| sum.saturating_add(*count))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    event_id: String,
    fired_at_ms: u64,
    /// It fired early because of a repeat refusal: if it is refused too, no
    /// further early fire is granted.
    replacement: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitiativeDirector {
    config: InitiativeConfig,
    enabled: bool,
    /// Last chat or speech arrival (or the start of the run).
    last_chat_ms: u64,
    /// The last time the performer was seen busy; the settle time counts
    /// from it.
    last_busy_ms: Option<u64>,
    last_fired_ms: Option<u64>,
    /// When a silent repeat refusal granted the one early re-fire (#541).
    refire_from_ms: Option<u64>,
    window: VecDeque<u64>,
    stop_hold_until_ms: Option<u64>,
    pending: Option<Pending>,
    last_suppression: Option<Suppression>,
    recent_texts: VecDeque<String>,
    /// The operator's topic list (at most `MAX_TOPICS`).
    topics: Vec<String>,
    /// Where the next round-robin pick starts.
    topic_cursor: usize,
    /// Topics used lately, oldest first (at most [`TOPIC_RECENT_MAX`]).
    recent_topics: VecDeque<String>,
    log: VecDeque<DecisionRecord>,
    next_seq: u64,
    metrics: InitiativeMetrics,
}

impl InitiativeDirector {
    pub fn new(config: InitiativeConfig, started_at_ms: u64) -> Self {
        Self {
            config,
            enabled: config.enabled,
            last_chat_ms: started_at_ms,
            last_busy_ms: None,
            last_fired_ms: None,
            refire_from_ms: None,
            window: VecDeque::new(),
            stop_hold_until_ms: None,
            pending: None,
            last_suppression: None,
            recent_texts: VecDeque::new(),
            topics: Vec::new(),
            topic_cursor: 0,
            recent_topics: VecDeque::new(),
            log: VecDeque::new(),
            next_seq: 0,
            metrics: InitiativeMetrics::default(),
        }
    }

    pub fn config(&self) -> InitiativeConfig {
        self.config
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn metrics(&self) -> InitiativeMetrics {
        self.metrics
    }

    pub fn log(&self) -> impl ExactSizeIterator<Item = &DecisionRecord> {
        self.log.iter()
    }

    pub fn pending_event_id(&self) -> Option<&str> {
        self.pending
            .as_ref()
            .map(|pending| pending.event_id.as_str())
    }

    /// How many topics the operator list holds.
    pub fn topic_count(&self) -> usize {
        self.topics.len()
    }

    /// The live list as the file holds it (one topic per line). Only the
    /// authenticated `topics.get` reads it.
    pub fn topic_text(&self) -> String {
        self.topics.join("\n")
    }

    /// Replace the operator's topic list (already validated by
    /// `aivtuber_domain::parse_topics`; capped again here). A different list
    /// restarts the round-robin at its first entry; the recently-used ring is
    /// kept, by text, so a topic still in the list is not repeated at once. The
    /// same list changes nothing.
    pub fn set_topics(&mut self, topics: &[String]) {
        let capped = &topics[..topics.len().min(aivtuber_domain::MAX_TOPICS)];
        if self.topics.as_slice() == capped {
            return;
        }
        self.topics = capped.to_vec();
        self.topic_cursor = 0;
    }

    pub fn recent_texts(&self) -> impl ExactSizeIterator<Item = &str> {
        self.recent_texts.iter().map(String::as_str)
    }

    /// The operator toggle. Turning it off cancels nothing by itself; the host
    /// preempts a pending initiative separately.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.last_suppression = None;
    }

    /// Chat or speech arrived: restart the idle clock. A pending initiative is
    /// left for the host to cancel and report through [`Self::preempted`].
    pub fn note_chat(&mut self, at_ms: u64) {
        self.last_chat_ms = self.last_chat_ms.max(at_ms);
        self.refire_from_ms = None;
        self.last_suppression = None;
    }

    /// The operator stopped the performer: stay quiet for [`STOP_HOLD_MS`].
    pub fn note_operator_stop(&mut self, at_ms: u64) {
        self.last_chat_ms = self.last_chat_ms.max(at_ms);
        self.stop_hold_until_ms = Some(at_ms.saturating_add(STOP_HOLD_MS));
        self.refire_from_ms = None;
        self.last_suppression = None;
    }

    /// Forget the firing history (the avatar was reset or erasure ran). The
    /// counters and the decision log are kept.
    pub fn clear(&mut self) {
        self.last_fired_ms = None;
        self.refire_from_ms = None;
        self.window.clear();
        self.recent_texts.clear();
        self.recent_topics.clear();
        self.topic_cursor = 0;
        self.stop_hold_until_ms = None;
        self.last_suppression = None;
    }

    /// Decide whether to speak now. A `Fire` charges the budget and marks the
    /// initiative pending; the host must report its end through
    /// [`Self::completed`], [`Self::failed`] or [`Self::preempted`].
    pub fn evaluate(&mut self, inputs: &InitiativeInputs) -> Decision {
        let now = inputs.now_ms;
        let busy = inputs.speaking
            || inputs.generating
            || inputs.content_queued
            || inputs.deferred_pending;
        if busy {
            self.last_busy_ms = Some(now);
        }
        if !self.enabled || self.pending.is_some() {
            self.last_suppression = None;
            return Decision::Wait;
        }
        if now.saturating_sub(self.last_chat_ms) < self.config.idle_ms {
            self.last_suppression = None;
            return Decision::Wait;
        }
        let stop_hold = self.stop_hold_until_ms.is_some_and(|until| now < until);
        let reason = if inputs.muted {
            Some(Suppression::Muted)
        } else if stop_hold {
            Some(Suppression::OperatorStopped)
        } else if inputs.floor_held {
            Some(Suppression::FloorHeld)
        } else if inputs.speaking {
            Some(Suppression::Speaking)
        } else if inputs.generating {
            Some(Suppression::Generating)
        } else if inputs.content_queued {
            Some(Suppression::ContentQueued)
        } else if inputs.deferred_pending {
            Some(Suppression::DeferredPending)
        } else if self
            .last_busy_ms
            .is_some_and(|busy_ms| now.saturating_sub(busy_ms) < SETTLE_MS)
        {
            Some(Suppression::Settling)
        } else {
            self.budget_reason(now, self.refire_open(now))
        };
        if let Some(reason) = reason {
            self.note_suppressed(reason, now);
            return Decision::Suppressed(reason);
        }
        self.last_suppression = None;
        let kind = self.select_kind();
        let topic = self.select_topic();
        match topic {
            Some(_) => {
                self.metrics.topics_used = self.metrics.topics_used.saturating_add(1);
            }
            None => {
                self.metrics.without_topic = self.metrics.without_topic.saturating_add(1);
            }
        }
        // A replacement is a fire the normal gap would still have refused.
        let replacement = self.refire_from_ms.take().is_some() && !self.gap_elapsed(now);
        self.window.push_back(now);
        self.last_fired_ms = Some(now);
        self.metrics.fired = self.metrics.fired.saturating_add(1);
        let event_id = format!("{EVENT_ID_PREFIX}{:06}", self.metrics.fired);
        self.pending = Some(Pending {
            event_id: event_id.clone(),
            fired_at_ms: now,
            replacement,
        });
        self.record(
            now,
            Outcome::Fired {
                kind,
                topic: topic.as_ref().map(|pick| pick.index),
            },
        );
        Decision::Fire {
            event_id,
            kind,
            topic,
        }
    }

    /// Whether the one early re-fire granted by a repeat refusal is open.
    fn refire_open(&self, now: u64) -> bool {
        self.refire_from_ms
            .is_some_and(|from| now.saturating_sub(from) >= REFIRE_GAP_MS)
    }

    fn gap_elapsed(&self, now: u64) -> bool {
        self.last_fired_ms
            .is_none_or(|last| now.saturating_sub(last) >= MIN_GAP_MS)
    }

    fn budget_reason(&mut self, now: u64, early: bool) -> Option<Suppression> {
        while self
            .window
            .front()
            .is_some_and(|first| now.saturating_sub(*first) >= WINDOW_MS)
        {
            self.window.pop_front();
        }
        if !early && !self.gap_elapsed(now) {
            return Some(Suppression::BudgetGap);
        }
        if self.window.len() >= WINDOW_MAX {
            return Some(Suppression::BudgetWindow);
        }
        None
    }

    /// The kind alternates with the number fired so far, so two runs of the
    /// same inputs choose the same kinds. `RevisitTopic` is never chosen (#87
    /// S2): there is no real topic state to revisit until #80.
    fn select_kind(&self) -> InitiativeKind {
        if self.metrics.fired.is_multiple_of(2) {
            InitiativeKind::IdleObservation
        } else {
            InitiativeKind::AudienceQuestion
        }
    }

    /// Round-robin from the cursor, skipping the topics used in the last
    /// N = min([`TOPIC_RECENT_MAX`], len - 1) initiatives, so with one topic
    /// it repeats and with two it alternates. Pure in the list, the cursor and
    /// the ring: nothing here reads a clock or randomness.
    fn select_topic(&mut self) -> Option<TopicPick> {
        let len = self.topics.len();
        if len == 0 {
            return None;
        }
        let window = TOPIC_RECENT_MAX.min(len - 1);
        let start = self.topic_cursor % len;
        let index = (0..len)
            .map(|offset| (start + offset) % len)
            .find(|candidate| {
                !self
                    .recent_topics
                    .iter()
                    .rev()
                    .take(window)
                    .any(|used| *used == self.topics[*candidate])
            })
            .unwrap_or(start);
        let text = self.topics[index].clone();
        self.topic_cursor = (index + 1) % len;
        if self.recent_topics.len() == TOPIC_RECENT_MAX {
            self.recent_topics.pop_front();
        }
        self.recent_topics.push_back(text.clone());
        Some(TopicPick { index, text })
    }

    fn note_suppressed(&mut self, reason: Suppression, now: u64) {
        if self.last_suppression == Some(reason) {
            return;
        }
        self.last_suppression = Some(reason);
        let slot = &mut self.metrics.suppressed[reason.index()];
        *slot = slot.saturating_add(1);
        self.record(now, Outcome::Suppressed(reason));
    }

    fn record(&mut self, at_ms: u64, outcome: Outcome) {
        if self.log.len() == LOG_CAPACITY {
            self.log.pop_front();
        }
        self.next_seq = self.next_seq.saturating_add(1);
        self.log.push_back(DecisionRecord {
            seq: self.next_seq,
            at_ms,
            outcome,
        });
    }

    /// The pending initiative reached the performer (its reply was committed
    /// or finished playing). `text` is what it said, kept to avoid repeats.
    pub fn completed(&mut self, at_ms: u64, text: Option<&str>) {
        if self.pending.take().is_none() {
            return;
        }
        self.metrics.completed = self.metrics.completed.saturating_add(1);
        if let Some(text) = text {
            self.remember_text(text);
        }
        self.record(at_ms, Outcome::Completed);
    }

    /// Generation failed or was unavailable; the host plays the idle gesture.
    /// The budget stays charged, so this is never retried.
    pub fn failed(&mut self, at_ms: u64) {
        if self.pending.take().is_none() {
            return;
        }
        self.metrics.failed = self.metrics.failed.saturating_add(1);
        self.record(at_ms, Outcome::Failed);
    }

    /// The pending initiative's only phrase was refused as a repeat (#89)
    /// before anything was spoken. Counted as failed like any fallback, but
    /// it grants the one early re-fire (see the module docs) unless the
    /// initiative was itself that early re-fire. `text` is remembered so the
    /// next request asks the model to avoid it.
    pub fn repeat_refused(&mut self, at_ms: u64, text: Option<&str>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.metrics.failed = self.metrics.failed.saturating_add(1);
        if !pending.replacement {
            self.refire_from_ms = Some(at_ms);
        }
        if let Some(text) = text {
            self.remember_text(text);
        }
        self.record(at_ms, Outcome::Failed);
    }

    /// The pending initiative was cancelled by chat, speech or an operator
    /// stop. Only a pending initiative counts.
    pub fn preempted(&mut self, at_ms: u64, cause: Preemption) {
        if self.pending.take().is_none() {
            return;
        }
        let slot = match cause {
            Preemption::Chat => &mut self.metrics.preempted_chat,
            Preemption::Speech => &mut self.metrics.preempted_speech,
            Preemption::OperatorStop => &mut self.metrics.preempted_stop,
        };
        *slot = slot.saturating_add(1);
        self.record(at_ms, Outcome::Preempted(cause));
    }

    fn remember_text(&mut self, text: &str) {
        let text = bounded_chars(text.trim());
        if text.is_empty() {
            return;
        }
        // A text already remembered becomes the newest, never a duplicate.
        self.recent_texts.retain(|seen| *seen != text);
        if self.recent_texts.len() == RECENT_TEXTS_MAX {
            self.recent_texts.pop_front();
        }
        self.recent_texts.push_back(text);
    }

    /// When the pending initiative fired, for the host's own bound.
    pub fn pending_since_ms(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.fired_at_ms)
    }
}

/// The code-owned sentence that stands where a viewer's text would be. It is
/// not viewer text and carries nothing but the closed kind.
pub fn input_text(kind: InitiativeKind) -> &'static str {
    match kind {
        InitiativeKind::AudienceQuestion => {
            "（配信が静かになりました。視聴者に向けて、自分から短い質問をひとつ投げかけてください。）"
        }
        InitiativeKind::IdleObservation => {
            "（配信が静かになりました。いまの様子について、自分から一言つぶやいてください。）"
        }
        InitiativeKind::RevisitTopic => {
            "（配信が静かになりました。さっき話したことをひとつ取り上げて、自分から話を続けてください。）"
        }
    }
}

/// The trusted event an initiative is dispatched as: a runtime timer tick,
/// minted by the director's host, never by ingress. Its priority is
/// `Background`, so every chat or speech reply outranks it, and the logical
/// stamp keeps replay deterministic.
pub fn mint_event(
    event_id: &str,
    kind: InitiativeKind,
    at_ms: u64,
    sequence: u64,
) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: event_id.to_owned(),
        correlation_id: format!("corr-{event_id}"),
        sequence,
        observed_at: crate::speech_conversation::logical_timestamp(at_ms),
        source: "runtime-initiative".to_owned(),
        source_class: SourceClass::Timer,
        plane: SecurityPlane::System,
        trust_level: TrustLevel::Trusted,
        kind: EventKind::TimerTick,
        actor_id: None,
        priority_hint: None,
        authorization: None,
        payload: std::collections::BTreeMap::from([
            (
                "timer_id".to_owned(),
                serde_json::Value::String("initiative".to_owned()),
            ),
            (
                "initiative_kind".to_owned(),
                serde_json::Value::String(kind.name().to_owned()),
            ),
            (
                "intent".to_owned(),
                serde_json::Value::String(format!("initiative.{}", kind.name())),
            ),
        ]),
    }
}

/// The typed, bounded Thinking request for one initiative.
///
/// `topic` is the operator's chosen topic (an `operator_curated` item, data
/// like the character sheet) and `recent_initiatives` the performer's own
/// earlier initiative texts; the latter are cut to [`TEXT_MAX_CHARS`] and
/// capped in number, and travel as data items, never in `input`. There are no
/// reply summaries (#87 S2), no raw history map, no viewer text and no event
/// payload.
pub fn build_request(
    event: &EventEnvelope,
    kind: InitiativeKind,
    topic: Option<&str>,
    recent_initiatives: &[String],
) -> Result<ThinkingRequest, DomainValidationError> {
    let mut curated = Vec::new();
    if let Some(topic) = topic {
        curated.push(topic_thinking_content(topic));
    }
    let skip = recent_initiatives.len().saturating_sub(RECENT_TEXTS_MAX);
    for text in recent_initiatives.iter().skip(skip) {
        curated.push(data_item("recent_initiative", text));
    }
    let context = ReflexContext {
        initiative: Some(InitiativeContext::new(kind)),
        ..ReflexContext::default()
    };
    let request = ThinkingRequest::from_event(
        event,
        input_text(kind),
        PrivacyClass::Public,
        context,
        RetrievalSnapshot::default(),
        curated,
    );
    debug_assert!(input_text(kind).len() <= MAX_THINKING_TEXT_BYTES);
    request.validate()?;
    Ok(request)
}

fn data_item(label: &str, text: &str) -> ThinkingContent {
    ThinkingContent {
        source: ThinkingContextSource::RecentInteraction,
        event_id: None,
        event_kind: None,
        source_class: None,
        // The performer's own gated output, not a viewer's words, and still
        // never an instruction.
        trust_level: TrustLevel::SemiTrusted,
        privacy: PrivacyClass::Public,
        epistemic_class: None,
        text: format!("{label}: {}", bounded_chars(text.trim())),
    }
}

/// `text` cut to [`TEXT_MAX_CHARS`] characters.
pub fn bounded_chars(text: &str) -> String {
    text.chars().take(TEXT_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000;

    fn director() -> InitiativeDirector {
        InitiativeDirector::new(InitiativeConfig::default(), 0)
    }

    fn idle(now_ms: u64) -> InitiativeInputs {
        InitiativeInputs {
            now_ms,
            ..InitiativeInputs::default()
        }
    }

    fn fire(d: &mut InitiativeDirector, now_ms: u64) -> String {
        match d.evaluate(&idle(now_ms)) {
            Decision::Fire { event_id, .. } => event_id,
            other => panic!("expected a fire at {now_ms}, got {other:?}"),
        }
    }

    #[test]
    fn it_waits_until_the_idle_time_has_passed_then_fires() {
        let mut d = director();
        assert_eq!(d.evaluate(&idle(10 * SEC)), Decision::Wait);
        assert_eq!(d.evaluate(&idle(44_999)), Decision::Wait);
        assert!(matches!(
            d.evaluate(&idle(45 * SEC)),
            Decision::Fire {
                kind: InitiativeKind::IdleObservation,
                ..
            }
        ));
        assert_eq!(d.metrics().fired, 1);
        assert_eq!(d.pending_event_id(), Some("initiative-000001"));
    }

    #[test]
    fn chat_restarts_the_idle_clock() {
        let mut d = director();
        d.note_chat(40 * SEC);
        assert_eq!(d.evaluate(&idle(80 * SEC)), Decision::Wait);
        assert!(matches!(d.evaluate(&idle(85 * SEC)), Decision::Fire { .. }));
    }

    #[test]
    fn a_disabled_director_never_fires_and_counts_nothing() {
        let mut d = InitiativeDirector::new(
            InitiativeConfig {
                enabled: false,
                ..InitiativeConfig::default()
            },
            0,
        );
        for t in [50, 100, 500] {
            assert_eq!(d.evaluate(&idle(t * SEC)), Decision::Wait);
        }
        assert_eq!(d.metrics(), InitiativeMetrics::default());
        d.set_enabled(true);
        assert!(matches!(
            d.evaluate(&idle(500 * SEC)),
            Decision::Fire { .. }
        ));
    }

    #[test]
    fn each_suppression_reason_blocks_and_is_counted_once_per_episode() {
        let at = 100 * SEC;
        let cases: [(Suppression, InitiativeInputs); 7] = [
            (
                Suppression::Muted,
                InitiativeInputs {
                    muted: true,
                    ..idle(at)
                },
            ),
            (
                Suppression::FloorHeld,
                InitiativeInputs {
                    floor_held: true,
                    ..idle(at)
                },
            ),
            (
                Suppression::Speaking,
                InitiativeInputs {
                    speaking: true,
                    ..idle(at)
                },
            ),
            (
                Suppression::Generating,
                InitiativeInputs {
                    generating: true,
                    ..idle(at)
                },
            ),
            (
                Suppression::ContentQueued,
                InitiativeInputs {
                    content_queued: true,
                    ..idle(at)
                },
            ),
            (
                Suppression::DeferredPending,
                InitiativeInputs {
                    deferred_pending: true,
                    ..idle(at)
                },
            ),
            // Busy a moment ago, quiet now: the settle time still holds.
            (Suppression::Settling, idle(at + 2 * SEC)),
        ];
        for (reason, inputs) in cases {
            let mut d = director();
            if reason == Suppression::Settling {
                let _ = d.evaluate(&InitiativeInputs {
                    speaking: true,
                    ..idle(at)
                });
            }
            assert_eq!(
                d.evaluate(&inputs),
                Decision::Suppressed(reason),
                "{reason:?}"
            );
            assert_eq!(d.evaluate(&inputs), Decision::Suppressed(reason));
            assert_eq!(d.metrics().suppressed_by(reason), 1);
            assert_eq!(d.metrics().fired, 0, "{reason:?} must not fire");
        }
    }

    #[test]
    fn an_operator_stop_holds_the_initiative_for_a_while() {
        let mut d = director();
        d.note_operator_stop(10 * SEC);
        // Idle again from the stop, but the hold is still on.
        assert_eq!(
            d.evaluate(&idle(70 * SEC)),
            Decision::Suppressed(Suppression::OperatorStopped)
        );
        assert_eq!(
            d.evaluate(&idle(10 * SEC + STOP_HOLD_MS - 1)),
            Decision::Suppressed(Suppression::OperatorStopped)
        );
        assert!(matches!(
            d.evaluate(&idle(10 * SEC + STOP_HOLD_MS)),
            Decision::Fire { .. }
        ));
        assert_eq!(d.metrics().suppressed_by(Suppression::OperatorStopped), 1);
    }

    #[test]
    fn the_budget_allows_one_per_minute_and_the_window_never_binds_at_that_pace() {
        let mut d = director();
        let mut t = 45 * SEC;
        fire(&mut d, t);
        d.completed(t + 6 * SEC, Some("one"));
        // Inside the minute: gap.
        assert_eq!(
            d.evaluate(&idle(t + 59_999)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
        // Ten in ten minutes, one a minute apart: the window is full.
        for _ in 1..WINDOW_MAX {
            t += MIN_GAP_MS;
            fire(&mut d, t);
            d.completed(t + SEC, None);
        }
        assert_eq!(d.metrics().fired as usize, WINDOW_MAX);
        // One more a minute later is allowed: the first has just left the
        // ten-minute window, so a steady one-a-minute pace never stalls.
        assert!(matches!(
            d.evaluate(&idle(t + MIN_GAP_MS)),
            Decision::Fire { .. }
        ));
        assert_eq!(d.metrics().suppressed_by(Suppression::BudgetWindow), 0);
    }

    #[test]
    fn a_pending_initiative_blocks_the_next_one() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        assert_eq!(d.evaluate(&idle(500 * SEC)), Decision::Wait);
        d.failed(500 * SEC);
        assert_eq!(d.metrics().failed, 1);
        assert_eq!(d.pending_event_id(), None);
    }

    #[test]
    fn a_failure_keeps_the_budget_charged_so_it_is_not_a_retry_loop() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.failed(46 * SEC);
        assert_eq!(
            d.evaluate(&idle(60 * SEC)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
        assert_eq!(d.metrics().fired, 1);
    }

    #[test]
    fn a_silent_repeat_refusal_grants_one_early_refire_after_the_short_gap() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, Some("said before"));
        assert_eq!(d.metrics().failed, 1);
        // The normal gap would hold until 105 s; the early re-fire opens at
        // refusal + REFIRE_GAP_MS.
        assert_eq!(
            d.evaluate(&idle(46 * SEC + REFIRE_GAP_MS - 1)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
        assert!(matches!(
            d.evaluate(&idle(46 * SEC + REFIRE_GAP_MS)),
            Decision::Fire { .. }
        ));
        assert_eq!(d.metrics().fired, 2);
    }

    #[test]
    fn the_replacement_uses_the_next_kind_and_asks_to_avoid_the_refused_text() {
        let mut d = director();
        let Decision::Fire { kind: first, .. } = d.evaluate(&idle(45 * SEC)) else {
            panic!("expected a fire");
        };
        d.repeat_refused(46 * SEC, Some("refused line"));
        let Decision::Fire { kind: second, .. } = d.evaluate(&idle(60 * SEC)) else {
            panic!("expected the early re-fire");
        };
        assert_ne!(first, second, "the rotation moved on");
        assert_eq!(d.recent_texts().collect::<Vec<_>>(), ["refused line"]);
        // Remembering the same text again keeps one copy, as the newest.
        d.completed(62 * SEC, Some("other"));
        d.remember_text("refused line");
        assert_eq!(
            d.recent_texts().collect::<Vec<_>>(),
            ["other", "refused line"]
        );
    }

    #[test]
    fn a_refused_replacement_does_not_chain_another_early_fire() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        fire(&mut d, 60 * SEC);
        d.repeat_refused(61 * SEC, None);
        // Nothing early now: the normal gap runs from the replacement's fire.
        assert_eq!(
            d.evaluate(&idle(120 * SEC - 1)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
        assert!(matches!(
            d.evaluate(&idle(120 * SEC)),
            Decision::Fire { .. }
        ));
    }

    #[test]
    fn a_voiced_replacement_makes_the_next_refusal_eligible_again() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        fire(&mut d, 60 * SEC);
        d.completed(62 * SEC, Some("fresh"));
        fire(&mut d, 120 * SEC);
        d.repeat_refused(121 * SEC, None);
        assert!(matches!(
            d.evaluate(&idle(126 * SEC)),
            Decision::Fire { .. }
        ));
    }

    #[test]
    fn a_normal_fire_after_a_late_refusal_is_not_marked_a_replacement() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        // Refused so late that the normal gap has already passed.
        d.repeat_refused(110 * SEC, None);
        fire(&mut d, 111 * SEC);
        d.repeat_refused(112 * SEC, None);
        assert!(matches!(
            d.evaluate(&idle(117 * SEC)),
            Decision::Fire { .. }
        ));
    }

    #[test]
    fn a_failure_never_grants_the_early_fire_and_a_stray_report_changes_nothing() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.failed(46 * SEC);
        assert_eq!(
            d.evaluate(&idle(60 * SEC)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
        let mut d = director();
        d.repeat_refused(1, Some("x"));
        assert_eq!(d.metrics().failed, 0);
        assert_eq!(d.recent_texts().count(), 0);
        assert!(matches!(d.evaluate(&idle(45 * SEC)), Decision::Fire { .. }));
    }

    #[test]
    fn chat_drops_the_early_fire() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        d.note_chat(47 * SEC);
        // Idle again at 92 s, the normal gap (105 s) still rules.
        assert_eq!(
            d.evaluate(&idle(95 * SEC)),
            Decision::Suppressed(Suppression::BudgetGap)
        );
    }

    #[test]
    fn an_operator_stop_drops_the_early_fire_and_holds_the_initiative() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        d.note_operator_stop(47 * SEC);
        assert_eq!(
            d.evaluate(&idle(120 * SEC)),
            Decision::Suppressed(Suppression::OperatorStopped)
        );
        assert!(d.refire_from_ms.is_none());
    }

    #[test]
    fn clear_forgets_the_early_fire() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        d.clear();
        assert!(d.refire_from_ms.is_none());
    }

    #[test]
    fn the_early_fire_still_obeys_the_settle_time_and_the_mute() {
        let mut d = director();
        fire(&mut d, 45 * SEC);
        d.repeat_refused(46 * SEC, None);
        let busy = InitiativeInputs {
            now_ms: 52 * SEC,
            speaking: true,
            ..InitiativeInputs::default()
        };
        assert_eq!(
            d.evaluate(&busy),
            Decision::Suppressed(Suppression::Speaking)
        );
        assert_eq!(
            d.evaluate(&idle(52 * SEC + 1)),
            Decision::Suppressed(Suppression::Settling)
        );
        let muted = InitiativeInputs {
            now_ms: 60 * SEC,
            muted: true,
            ..InitiativeInputs::default()
        };
        assert_eq!(d.evaluate(&muted), Decision::Suppressed(Suppression::Muted));
        assert!(matches!(
            d.evaluate(&idle(52 * SEC + SETTLE_MS)),
            Decision::Fire { .. }
        ));
    }

    #[test]
    fn the_early_fire_cannot_exceed_the_window_ceiling() {
        let mut d = director();
        let mut t = 45 * SEC;
        for _ in 1..WINDOW_MAX {
            fire(&mut d, t);
            d.completed(t + 1, None);
            t += MIN_GAP_MS;
        }
        // The tenth fire fills the window and is refused as a repeat.
        fire(&mut d, t);
        assert_eq!(d.window.len(), WINDOW_MAX);
        d.repeat_refused(t + 1, None);
        assert_eq!(
            d.evaluate(&idle(t + 1 + REFIRE_GAP_MS)),
            Decision::Suppressed(Suppression::BudgetWindow)
        );
    }

    #[test]
    fn repeat_refusals_replay_to_the_same_decisions() {
        fn run() -> Vec<DecisionRecord> {
            let mut d = director();
            let mut t = 45 * SEC;
            for _ in 0..12 {
                if let Decision::Fire { .. } = d.evaluate(&idle(t)) {
                    d.repeat_refused(t + 2 * SEC, Some("same"));
                }
                t += 3 * SEC;
            }
            d.log().cloned().collect()
        }
        assert_eq!(run(), run());
        assert!(run().len() <= LOG_CAPACITY);
    }

    #[test]
    fn preemption_is_counted_by_cause_and_only_while_pending() {
        let mut d = director();
        d.preempted(1, Preemption::Chat);
        assert_eq!(d.metrics().preempted_chat, 0);
        fire(&mut d, 45 * SEC);
        d.preempted(46 * SEC, Preemption::Chat);
        fire(&mut d, 200 * SEC);
        d.preempted(201 * SEC, Preemption::Speech);
        fire(&mut d, 400 * SEC);
        d.preempted(401 * SEC, Preemption::OperatorStop);
        let m = d.metrics();
        assert_eq!(
            (m.preempted_chat, m.preempted_speech, m.preempted_stop),
            (1, 1, 1)
        );
        assert_eq!(d.pending_event_id(), None);
    }

    fn topics(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// Fire at `*t`, finish the initiative and advance one budget gap.
    fn fire_one(d: &mut InitiativeDirector, t: &mut u64) -> (InitiativeKind, Option<TopicPick>) {
        let decision = d.evaluate(&idle(*t));
        let Decision::Fire { kind, topic, .. } = decision else {
            panic!("expected a fire at {t}, got {decision:?}");
        };
        d.completed(*t, None);
        *t += MIN_GAP_MS;
        (kind, topic)
    }

    fn picked(d: &mut InitiativeDirector, t: &mut u64, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| {
                fire_one(d, t)
                    .1
                    .map_or_else(|| "(none)".to_owned(), |pick| pick.text)
            })
            .collect()
    }

    #[test]
    fn the_kind_alternates_and_a_revisit_is_never_chosen() {
        let mut d = director();
        let mut t = 45 * SEC;
        let kinds: Vec<_> = (0..8).map(|_| fire_one(&mut d, &mut t).0).collect();
        use InitiativeKind::*;
        assert_eq!(
            kinds,
            [
                IdleObservation,
                AudienceQuestion,
                IdleObservation,
                AudienceQuestion,
                IdleObservation,
                AudienceQuestion,
                IdleObservation,
                AudienceQuestion
            ]
        );
        assert!(!kinds.contains(&RevisitTopic));
    }

    #[test]
    fn topics_rotate_round_robin_in_logical_time() {
        let mut d = director();
        d.set_topics(&topics(&["a", "b", "c"]));
        let mut t = 45 * SEC;
        assert_eq!(
            picked(&mut d, &mut t, 7),
            ["a", "b", "c", "a", "b", "c", "a"]
        );
        assert_eq!(d.metrics().topics_used, 7);
        assert_eq!(d.metrics().without_topic, 0);
        // The log names the index, never the text.
        let indexes: Vec<_> = d
            .log()
            .filter_map(|record| match record.outcome {
                Outcome::Fired { topic, .. } => Some(topic),
                _ => None,
            })
            .collect();
        assert_eq!(indexes, [0, 1, 2, 0, 1, 2, 0].map(Some).to_vec());
    }

    #[test]
    fn recently_used_topics_are_skipped_after_the_list_is_edited() {
        let mut d = director();
        d.set_topics(&topics(&["a", "b", "c", "d", "e", "f"]));
        let mut t = 45 * SEC;
        assert_eq!(picked(&mut d, &mut t, 3), ["a", "b", "c"]);
        // Reordered: the round-robin restarts at its first entry, but c and a
        // were used within the last five, so the first free one is d.
        d.set_topics(&topics(&["c", "a", "d", "e", "f", "b"]));
        assert_eq!(picked(&mut d, &mut t, 3), ["d", "e", "f"]);
        // Wrapping round: b is blocked by its own recent use, c is blocked, a
        // is the first topic not among the last five (b c d e f).
        assert_eq!(picked(&mut d, &mut t, 1), ["a"]);
        // The same list saved again changes nothing (no restart).
        let before = d.topic_cursor;
        d.set_topics(&topics(&["c", "a", "d", "e", "f", "b"]));
        assert_eq!(d.topic_cursor, before);
    }

    #[test]
    fn short_lists_use_a_window_of_len_minus_one() {
        let mut one = director();
        one.set_topics(&topics(&["a"]));
        let mut t = 45 * SEC;
        assert_eq!(picked(&mut one, &mut t, 3), ["a", "a", "a"]);
        let mut two = director();
        two.set_topics(&topics(&["a", "b"]));
        let mut t = 45 * SEC;
        assert_eq!(picked(&mut two, &mut t, 4), ["a", "b", "a", "b"]);
        // The window never exceeds TOPIC_RECENT_MAX: with a long list the
        // ring holds five and no more.
        let mut long = director();
        long.set_topics(&topics(&["a", "b", "c", "d", "e", "f", "g", "h"]));
        let mut t = 45 * SEC;
        let _ = picked(&mut long, &mut t, 40);
        assert!(long.recent_topics.len() <= TOPIC_RECENT_MAX);
    }

    #[test]
    fn an_empty_list_runs_without_a_topic_and_counts_it() {
        let mut d = director();
        let mut t = 45 * SEC;
        assert_eq!(picked(&mut d, &mut t, 2), ["(none)", "(none)"]);
        assert_eq!(d.metrics().without_topic, 2);
        assert_eq!(d.metrics().topics_used, 0);
        // Emptying the list again goes back to no topic.
        d.set_topics(&topics(&["a"]));
        assert_eq!(picked(&mut d, &mut t, 1), ["a"]);
        d.set_topics(&[]);
        assert_eq!(picked(&mut d, &mut t, 1), ["(none)"]);
        assert_eq!(d.topic_count(), 0);
    }

    #[test]
    fn a_failed_initiative_has_still_used_its_topic() {
        let mut d = director();
        d.set_topics(&topics(&["a", "b"]));
        let first = fire(&mut d, 45 * SEC);
        assert_eq!(first, "initiative-000001");
        d.failed(46 * SEC);
        let mut t = 45 * SEC + MIN_GAP_MS;
        assert_eq!(picked(&mut d, &mut t, 1), ["b"]);
    }

    #[test]
    fn the_topic_list_is_capped_and_clear_forgets_the_rotation() {
        let mut d = director();
        let many: Vec<String> = (0..100).map(|i| format!("t{i}")).collect();
        d.set_topics(&many);
        assert_eq!(d.topic_count(), aivtuber_domain::MAX_TOPICS);
        let mut t = 45 * SEC;
        assert_eq!(picked(&mut d, &mut t, 2), ["t0", "t1"]);
        d.clear();
        assert!(d.recent_topics.is_empty());
        assert_eq!(d.topic_count(), aivtuber_domain::MAX_TOPICS);
    }

    #[test]
    fn state_stays_bounded_over_a_very_long_run() {
        let mut d = director();
        let mut t = 45 * SEC;
        for _ in 0..5_000 {
            if let Decision::Fire { .. } = d.evaluate(&idle(t)) {
                d.completed(t, Some(&"あ".repeat(1_000)));
            }
            t += MIN_GAP_MS;
        }
        assert!(d.log().len() <= LOG_CAPACITY);
        assert!(d.window.len() <= WINDOW_MAX);
        assert!(d.recent_texts().len() <= RECENT_TEXTS_MAX);
        assert!(
            d.recent_texts()
                .all(|text| text.chars().count() <= TEXT_MAX_CHARS)
        );
        assert!(d.metrics().fired > 100);
    }

    #[test]
    fn the_same_input_trace_gives_the_same_decisions_and_log() {
        fn run() -> (Vec<Decision>, Vec<DecisionRecord>, InitiativeMetrics) {
            let mut d = director();
            let mut decisions = Vec::new();
            for step in 0..400_u64 {
                let now_ms = step * 2_500;
                if step == 90 {
                    d.note_chat(now_ms);
                }
                if step == 250 {
                    d.note_operator_stop(now_ms);
                }
                if step == 60 {
                    d.set_topics(&topics(&["a", "b", "c", "d"]));
                }
                if step == 300 {
                    d.set_topics(&topics(&["c", "a"]));
                }
                let inputs = InitiativeInputs {
                    now_ms,
                    muted: (150..160).contains(&step),
                    floor_held: step % 37 == 0,
                    speaking: step % 53 == 0,
                    ..InitiativeInputs::default()
                };
                let decision = d.evaluate(&inputs);
                match &decision {
                    Decision::Fire { .. } if step % 3 == 0 => d.failed(now_ms),
                    Decision::Fire { .. } if step % 3 == 1 => {
                        d.preempted(now_ms, Preemption::Speech)
                    }
                    Decision::Fire { .. } => d.completed(now_ms, Some("text")),
                    _ => {}
                }
                decisions.push(decision);
            }
            (decisions, d.log().cloned().collect(), d.metrics())
        }
        let (a, b) = (run(), run());
        assert_eq!(a, b);
        assert!(a.2.fired >= 3, "the trace must exercise firing");
        assert!(a.2.suppressed_total() > 0);
    }

    #[test]
    fn the_request_is_typed_bounded_and_carries_no_reply_summaries_or_event_payload() {
        let event = mint_event(
            "initiative-000001",
            InitiativeKind::AudienceQuestion,
            5_000,
            1,
        );
        event.validate().expect("the minted event is valid");
        assert_eq!(event.kind, EventKind::TimerTick);
        assert_eq!(event.trust_level, TrustLevel::Trusted);
        // The event payload names the closed kind only: no topic text.
        assert!(!format!("{:?}", event.payload).contains("好きな食べ物"));
        let many: Vec<String> = (0..10)
            .map(|i| format!("発話{i}{}", "あ".repeat(500)))
            .collect();
        let request = build_request(
            &event,
            InitiativeKind::AudienceQuestion,
            Some("好きな食べ物"),
            &many,
        )
        .expect("a bounded request validates");
        assert_eq!(
            request.input.text,
            input_text(InitiativeKind::AudienceQuestion)
        );
        assert_eq!(
            request.context.initiative,
            Some(InitiativeContext::new(InitiativeKind::AudienceQuestion))
        );
        // One topic, then at most RECENT_TEXTS_MAX recent initiatives; no
        // `recent_reply` item exists any more.
        assert_eq!(request.curated_context.len(), 1 + RECENT_TEXTS_MAX);
        let topic = &request.curated_context[0];
        assert_eq!(topic.source, ThinkingContextSource::OperatorCurated);
        assert_eq!(topic.source_class, Some(SourceClass::Operator));
        assert_eq!(topic.text, "好きな食べ物");
        for item in &request.curated_context[1..] {
            assert!(item.text.starts_with("recent_initiative: "));
            assert!(item.text.chars().count() <= TEXT_MAX_CHARS + 20);
            assert_eq!(item.source, ThinkingContextSource::RecentInteraction);
            assert_ne!(item.trust_level, TrustLevel::Trusted);
        }
        assert!(
            !request
                .curated_context
                .iter()
                .any(|item| item.text.contains("recent_reply"))
        );
        // The newest recent initiatives are the ones kept.
        assert!(request.curated_context[1].text.contains("発話7"));
        assert!(request.curated_context[3].text.contains("発話9"));
        // With no topic the request is just the closed kind and the avoid-list.
        let bare = build_request(&event, InitiativeKind::IdleObservation, None, &[])
            .expect("a bare request validates");
        assert!(bare.curated_context.is_empty());
    }

    #[test]
    fn config_bounds_are_enforced() {
        assert!(InitiativeConfig::with_idle_secs(true, 45).is_ok());
        assert!(InitiativeConfig::with_idle_secs(true, 4).is_err());
        assert!(InitiativeConfig::with_idle_secs(true, 3_601).is_err());
    }
}
