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
//! The budget is charged when the director fires, not when the reply
//! succeeds, so a failing backend can never turn into a retry loop.
//!
//! # Bounds
//!
//! The firing window holds at most [`WINDOW_MAX`] times, the decision log
//! [`LOG_CAPACITY`] records, the remembered initiative texts
//! [`RECENT_TEXTS_MAX`] entries of at most [`TEXT_MAX_CHARS`] characters, and
//! every counter saturates. Nothing grows with run length.
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
    TrustLevel,
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
/// How long an operator stop keeps the initiative quiet.
pub const STOP_HOLD_MS: u64 = 300_000;
/// Decision records kept for replay and diagnostics.
pub const LOG_CAPACITY: usize = 32;
/// Own initiative texts remembered so a request can avoid repeating them.
pub const RECENT_TEXTS_MAX: usize = 3;
/// Characters kept per remembered text and per reply summary.
pub const TEXT_MAX_CHARS: usize = 160;
/// Reply summaries a request carries (the last N).
pub const SUMMARY_COUNT: usize = 3;
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
    /// There is at least one earlier reply a revisit could refer to.
    pub have_summaries: bool,
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
    },
}

/// One recorded decision, for replay comparison and diagnostics. Edge
/// triggered: a suppression is recorded when its reason changes, not on every
/// evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Fired(InitiativeKind),
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
    window: VecDeque<u64>,
    stop_hold_until_ms: Option<u64>,
    pending: Option<Pending>,
    last_suppression: Option<Suppression>,
    recent_texts: VecDeque<String>,
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
            window: VecDeque::new(),
            stop_hold_until_ms: None,
            pending: None,
            last_suppression: None,
            recent_texts: VecDeque::new(),
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
        self.last_suppression = None;
    }

    /// The operator stopped the performer: stay quiet for [`STOP_HOLD_MS`].
    pub fn note_operator_stop(&mut self, at_ms: u64) {
        self.last_chat_ms = self.last_chat_ms.max(at_ms);
        self.stop_hold_until_ms = Some(at_ms.saturating_add(STOP_HOLD_MS));
        self.last_suppression = None;
    }

    /// Forget the firing history (the avatar was reset or erasure ran). The
    /// counters and the decision log are kept.
    pub fn clear(&mut self) {
        self.last_fired_ms = None;
        self.window.clear();
        self.recent_texts.clear();
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
            self.budget_reason(now)
        };
        if let Some(reason) = reason {
            self.note_suppressed(reason, now);
            return Decision::Suppressed(reason);
        }
        self.last_suppression = None;
        let kind = self.select_kind(inputs.have_summaries);
        self.window.push_back(now);
        self.last_fired_ms = Some(now);
        self.metrics.fired = self.metrics.fired.saturating_add(1);
        let event_id = format!("{EVENT_ID_PREFIX}{:06}", self.metrics.fired);
        self.pending = Some(Pending {
            event_id: event_id.clone(),
            fired_at_ms: now,
        });
        self.record(now, Outcome::Fired(kind));
        Decision::Fire { event_id, kind }
    }

    fn budget_reason(&mut self, now: u64) -> Option<Suppression> {
        while self
            .window
            .front()
            .is_some_and(|first| now.saturating_sub(*first) >= WINDOW_MS)
        {
            self.window.pop_front();
        }
        if self
            .last_fired_ms
            .is_some_and(|last| now.saturating_sub(last) < MIN_GAP_MS)
        {
            return Some(Suppression::BudgetGap);
        }
        if self.window.len() >= WINDOW_MAX {
            return Some(Suppression::BudgetWindow);
        }
        None
    }

    /// The kind rotates with the number fired so far, so two runs of the same
    /// inputs choose the same kinds. A revisit needs something to revisit.
    fn select_kind(&self, have_summaries: bool) -> InitiativeKind {
        match self.metrics.fired % 3 {
            0 => InitiativeKind::IdleObservation,
            1 => InitiativeKind::AudienceQuestion,
            _ if have_summaries => InitiativeKind::RevisitTopic,
            _ => InitiativeKind::IdleObservation,
        }
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
/// `summaries` are the performer's own last replies and `recent_initiatives`
/// its own earlier initiative texts; both are cut to [`TEXT_MAX_CHARS`] and
/// capped in number, and travel as data items, never in `input`. No raw
/// history map, no viewer text and no event payload is copied.
pub fn build_request(
    event: &EventEnvelope,
    kind: InitiativeKind,
    summaries: &[String],
    recent_initiatives: &[String],
) -> Result<ThinkingRequest, DomainValidationError> {
    let mut curated = Vec::new();
    let skip = summaries.len().saturating_sub(SUMMARY_COUNT);
    for text in summaries.iter().skip(skip) {
        curated.push(data_item("recent_reply", text));
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

    #[test]
    fn kinds_rotate_and_a_revisit_needs_something_to_revisit() {
        let mut d = director();
        let mut kinds = Vec::new();
        let mut t = 45 * SEC;
        for have_summaries in [false, false, false, true, true, true] {
            let decision = d.evaluate(&InitiativeInputs {
                have_summaries,
                ..idle(t)
            });
            let Decision::Fire { kind, .. } = decision else {
                panic!("expected fire, got {decision:?}")
            };
            kinds.push(kind);
            d.completed(t, None);
            t += MIN_GAP_MS;
        }
        use InitiativeKind::*;
        assert_eq!(
            kinds,
            [
                IdleObservation,
                AudienceQuestion,
                IdleObservation,
                IdleObservation,
                AudienceQuestion,
                RevisitTopic
            ]
        );
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
                let inputs = InitiativeInputs {
                    now_ms,
                    muted: (150..160).contains(&step),
                    floor_held: step % 37 == 0,
                    speaking: step % 53 == 0,
                    have_summaries: step > 120,
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
    fn the_request_is_typed_bounded_and_carries_no_event_payload() {
        let event = mint_event("initiative-000001", InitiativeKind::RevisitTopic, 5_000, 1);
        event.validate().expect("the minted event is valid");
        assert_eq!(event.kind, EventKind::TimerTick);
        assert_eq!(event.trust_level, TrustLevel::Trusted);
        let many: Vec<String> = (0..10)
            .map(|i| format!("返答{i}{}", "あ".repeat(500)))
            .collect();
        let request = build_request(&event, InitiativeKind::RevisitTopic, &many, &many)
            .expect("a bounded request validates");
        assert_eq!(request.input.text, input_text(InitiativeKind::RevisitTopic));
        assert_eq!(
            request.context.initiative,
            Some(InitiativeContext::new(InitiativeKind::RevisitTopic))
        );
        assert_eq!(
            request.curated_context.len(),
            SUMMARY_COUNT + RECENT_TEXTS_MAX
        );
        for item in &request.curated_context {
            assert!(item.text.chars().count() <= TEXT_MAX_CHARS + 20);
            assert_eq!(item.source, ThinkingContextSource::RecentInteraction);
            assert_ne!(item.trust_level, TrustLevel::Trusted);
        }
        // The newest summaries are the ones kept.
        assert!(request.curated_context[0].text.contains("返答7"));
        assert!(request.curated_context[2].text.contains("返答9"));
    }

    #[test]
    fn config_bounds_are_enforced() {
        assert!(InitiativeConfig::with_idle_secs(true, 45).is_ok());
        assert!(InitiativeConfig::with_idle_secs(true, 4).is_err());
        assert!(InitiativeConfig::with_idle_secs(true, 3_601).is_err());
    }
}
