//! Issue #78 (S1): deterministic adaptive micro-batched selection for chat
//! bursts.
//!
//! One Jev call per comment does not survive a chat burst, and the local
//! profile has no Jev at all. This module groups admitted public chat into
//! small bounded batches *only while chat is actually bursting*, picks a Top-K
//! deterministically, and gives every admitted comment exactly one terminal
//! outcome.
//!
//! Load-bearing properties:
//!
//! 1. **No wait in quiet chat.** Until `burst_enter_count` arrivals fall inside
//!    `window_ms`, an offered comment is `QuietPassthrough`: the caller handles
//!    it exactly as before and the batcher holds nothing.
//! 2. **Pure and replayable.** There is no clock, no thread and no timer in
//!    here. Every instant is a caller-supplied monotonic millisecond value, and
//!    a window flush is stamped with its logical deadline (`opened + window`),
//!    not with whenever it was polled, so the same offers produce byte-identical
//!    [`BatchReplayRecord`]s whatever the polling cadence.
//! 3. **Bypass is structural, never claimed.** Control/system-plane events are
//!    `Immediate` and never enter (or wait behind) a batch. Content that merely
//!    *claims* control authority is `Refused`, never promoted. A viewer-supplied
//!    `priority_hint` is ignored for ranking, so untrusted text cannot
//!    self-escalate into the Top-K.
//! 4. **Bounded.** Pending candidates, pending bytes, ready batches, request
//!    candidates, tracked actors and the burst-density ring all have hard caps
//!    ([`BurstConfig::validate`]). A full ready queue sheds its oldest batch
//!    with an explicit terminal outcome rather than growing.
//! 5. **Every admitted comment is accounted exactly once.** `admitted` must
//!    equal `terminal + pending + ready` after every operation; a violation is
//!    counted in [`BurstMetrics::invariant_violations`] (tests require zero)
//!    and cannot be repaired silently.
//! 6. **Jev is data, not authority.** An optional [`BatchRanker`] may only
//!    reorder candidates that are in the request. The deterministic policy then
//!    re-applies Top-K, the per-actor cap and the per-actor cooldown, so a
//!    ranking can neither add a candidate nor revive a suppressed one. Any
//!    ranker failure falls back to the deterministic order.
//!
//! S1 deliberately ships the *seam* for Jev batch ranking, not a Jev batch
//! prompt: [`BatchEngagementRequest`] is `Serialize`-only (no `Deserialize`,
//! no JSON Schema) so it cannot become a second implicit wire contract before
//! the adapter slice defines one.

use aivtuber_domain::{
    EventEnvelope, EventKind, MAX_SNAPSHOT_STRING_BYTES, ReflexContext, SecurityPlane, SourceClass,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

/// Version of the (Rust-internal) batch request/replay shape.
pub const BATCH_ENGAGEMENT_SCHEMA_VERSION: &str = "0.1.0";
/// Hard ceiling on candidates held in one pending batch.
pub const MAX_BATCH_CANDIDATES: usize = 64;
/// Hard ceiling on retained bytes of one pending batch.
pub const MAX_BATCH_BYTES: usize = 32 * 1024;
/// Hard ceiling on flushed batches awaiting resolution.
pub const MAX_READY_BATCHES: usize = 16;
/// Hard ceiling on candidates shown to a ranker in one request.
pub const MAX_BATCH_REQUEST_CANDIDATES: usize = 16;
/// Hard ceiling on the batch window.
pub const MAX_BATCH_WINDOW_MS: u64 = 5_000;
/// Hard ceiling on the number of actors whose cooldown is remembered.
pub const MAX_TRACKED_ACTORS: usize = 4_096;
/// Bytes of comment text retained per candidate (UTF-8 boundary safe).
pub const MAX_CANDIDATE_TEXT_BYTES: usize = 256;
/// Hard ceiling on the burst-density ring.
pub const MAX_BURST_ENTER_COUNT: usize = 64;

/// Operator-facing configuration. All bounds are validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstConfig {
    /// Longest a candidate can wait for its batch to flush.
    pub window_ms: u64,
    /// Arrivals within `window_ms` that switch from passthrough to batching.
    /// `1` would batch everything and is refused: quiet chat must not wait.
    pub burst_enter_count: usize,
    /// A batch flushes when it holds this many candidates.
    pub max_candidates: usize,
    /// A batch flushes before it would exceed this many retained bytes.
    pub max_bytes: usize,
    /// Flushed batches awaiting resolution before the oldest is shed.
    pub max_ready_batches: usize,
    /// Comments selected per batch.
    pub top_k: usize,
    /// Comments one actor may have selected per batch.
    pub max_per_actor: usize,
    /// After an actor is selected, its later comments are suppressed until this
    /// much logical time has passed. `0` disables the cooldown.
    pub actor_cooldown_ms: u64,
    /// Candidates a ranker is shown (the deterministic leaders).
    pub request_candidates: usize,
    /// Actors whose cooldown is remembered; the oldest is evicted beyond it.
    pub max_tracked_actors: usize,
}

impl Default for BurstConfig {
    fn default() -> Self {
        Self {
            window_ms: 1_500,
            burst_enter_count: 4,
            max_candidates: 32,
            max_bytes: 16 * 1024,
            max_ready_batches: 4,
            top_k: 3,
            max_per_actor: 1,
            actor_cooldown_ms: 10_000,
            request_candidates: 12,
            max_tracked_actors: 1_024,
        }
    }
}

impl BurstConfig {
    pub fn validate(&self) -> Result<(), BurstConfigError> {
        let check = |ok: bool, field: &'static str| {
            if ok {
                Ok(())
            } else {
                Err(BurstConfigError { field })
            }
        };
        check(
            (1..=MAX_BATCH_WINDOW_MS).contains(&self.window_ms),
            "window_ms",
        )?;
        check(
            (2..=MAX_BURST_ENTER_COUNT).contains(&self.burst_enter_count),
            "burst_enter_count",
        )?;
        check(
            (1..=MAX_BATCH_CANDIDATES).contains(&self.max_candidates),
            "max_candidates",
        )?;
        check((1..=MAX_BATCH_BYTES).contains(&self.max_bytes), "max_bytes")?;
        check(
            (1..=MAX_READY_BATCHES).contains(&self.max_ready_batches),
            "max_ready_batches",
        )?;
        check(
            self.top_k >= 1 && self.top_k <= self.request_candidates,
            "top_k",
        )?;
        check(
            self.max_per_actor >= 1 && self.max_per_actor <= self.top_k,
            "max_per_actor",
        )?;
        check(
            (1..=MAX_BATCH_REQUEST_CANDIDATES).contains(&self.request_candidates),
            "request_candidates",
        )?;
        check(
            (1..=MAX_TRACKED_ACTORS).contains(&self.max_tracked_actors),
            "max_tracked_actors",
        )
    }
}

/// Refusal raised for an out-of-bounds [`BurstConfig`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstConfigError {
    pub field: &'static str,
}

impl std::fmt::Display for BurstConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid burst configuration: {}", self.field)
    }
}

impl std::error::Error for BurstConfigError {}

/// Why an offered event was not batched and must take the existing per-event
/// path without waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmediateReason {
    /// Control plane: never delayed by, or queued behind, a batch.
    ControlPlane,
    /// System plane (stream/timer/health).
    SystemPlane,
    /// Content that is not a chat message (donation, game, speech).
    NonChatKind,
    /// Identifiers too large to retain in a bounded candidate.
    OversizedIdentity,
    /// The engine is shut down and holds nothing.
    ShuttingDown,
}

/// Why an offered event was refused instead of batched or bypassed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// Content-plane event carrying an authorization claim.
    AuthorizationClaimOnContent,
    /// Content-plane event declaring an operator kind or source class.
    ControlClaimOnContent,
    /// A chat message whose source class contradicts its kind.
    InconsistentClassification,
}

/// What [`BurstEngine::offer`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Chat was quiet: handle it now, nothing is held.
    QuietPassthrough,
    /// Held in the pending batch; it will get exactly one terminal outcome.
    Batched,
    /// Not batchable; handle it now on the existing path.
    Immediate(ImmediateReason),
    /// Not batchable and not trustworthy as classified.
    Refused(RefusalReason),
}

/// Why a batch was flushed. Derived only from event time, sequence and
/// configuration, so it replays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushReason {
    Window,
    Count,
    Bytes,
    Shutdown,
}

/// The single terminal outcome of an admitted comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// Chosen; `rank` is its zero-based position in the final selection.
    Selected { rank: usize },
    /// Quiet chat: handled on the existing per-event path, never held.
    QuietPassthrough,
    /// Valid but beyond the Top-K / request capacity.
    NotSelectedCapacity,
    /// Its actor was selected too recently.
    SuppressedActorCooldown,
    /// Its actor already filled their per-batch share.
    SuppressedActorBatchCap,
    /// Its batch was shed because resolution fell behind.
    ShedPendingOverflow,
}

impl TerminalOutcome {
    fn category(self) -> usize {
        match self {
            Self::Selected { .. } => 0,
            Self::QuietPassthrough => 1,
            Self::NotSelectedCapacity => 2,
            Self::SuppressedActorCooldown => 3,
            Self::SuppressedActorBatchCap => 4,
            Self::ShedPendingOverflow => 5,
        }
    }
}

/// Where the final order came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingSource {
    Deterministic,
    Ranker,
    /// A ranker was consulted and failed; the deterministic order was used.
    RankerFallback,
    /// The batch was shed or resolved at shutdown without consulting a ranker.
    Skipped,
}

/// Failure reported by a [`BatchRanker`]. All of them fall back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRankFailure {
    Timeout,
    Unavailable,
    Malformed,
}

/// Seam for Jev (or any other) batch ranking, behind the reflex boundary.
///
/// The ranker receives only the bounded [`BatchEngagementRequest`] and returns
/// event ids in preferred order. The output is a *suggestion*: ids outside the
/// request or repeated are ignored, and Top-K / fairness are re-applied.
pub trait BatchRanker {
    fn rank(&mut self, request: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure>;
}

/// A bounded view of one admitted comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchCandidate {
    pub event_id: String,
    pub actor_id: Option<String>,
    pub sequence: u64,
    /// Monotonic arrival instant supplied by the caller.
    pub arrival_ms: u64,
    /// Deterministic local score in `0..=1000`, e.g. retrieval similarity.
    pub local_score_milli: u16,
    text: String,
    retained_bytes: usize,
}

impl BatchCandidate {
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Candidate as shown to a ranker. Public chat is always untrusted data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestCandidate {
    pub event_id: String,
    pub actor_id: Option<String>,
    pub sequence: u64,
    pub text: String,
    pub local_score_milli: u16,
}

/// Limits the deterministic policy will enforce on whatever comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapacityEnvelope {
    pub max_selected: usize,
    pub max_per_actor: usize,
    pub request_candidates: usize,
}

/// Bounded request for one batch: shared context once, bounded candidates, and
/// the capacity envelope the answer will be trimmed to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchEngagementRequest {
    pub schema_version: String,
    pub batch_id: u64,
    pub context: ReflexContext,
    pub candidates: Vec<RequestCandidate>,
    pub capacity: CapacityEnvelope,
}

/// Refusal raised by [`BatchEngagementRequest::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchRequestError {
    pub field: &'static str,
    pub message: String,
}

impl BatchRequestError {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BatchRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for BatchRequestError {}

impl BatchEngagementRequest {
    pub fn validate(&self) -> Result<(), BatchRequestError> {
        if self.schema_version != BATCH_ENGAGEMENT_SCHEMA_VERSION {
            return Err(BatchRequestError::new(
                "batch_request.schema_version",
                format!("expected {BATCH_ENGAGEMENT_SCHEMA_VERSION}"),
            ));
        }
        self.context
            .validate()
            .map_err(|error| BatchRequestError::new("batch_request.context", error.to_string()))?;
        if self.candidates.is_empty() || self.candidates.len() > MAX_BATCH_REQUEST_CANDIDATES {
            return Err(BatchRequestError::new(
                "batch_request.candidates",
                format!("must contain 1..={MAX_BATCH_REQUEST_CANDIDATES} candidates"),
            ));
        }
        let mut seen = BTreeSet::new();
        for candidate in &self.candidates {
            if candidate.text.len() > MAX_CANDIDATE_TEXT_BYTES
                || candidate.event_id.is_empty()
                || candidate.event_id.len() > MAX_SNAPSHOT_STRING_BYTES
                || !seen.insert(candidate.event_id.as_str())
            {
                return Err(BatchRequestError::new(
                    "batch_request.candidates",
                    "candidate text/id out of bounds or duplicated",
                ));
            }
        }
        if self.capacity.max_selected == 0 {
            return Err(BatchRequestError::new(
                "batch_request.capacity",
                "max_selected must be positive",
            ));
        }
        Ok(())
    }
}

/// A flushed batch awaiting resolution.
#[derive(Debug, Clone)]
struct FlushedBatch {
    batch_id: u64,
    reason: FlushReason,
    opened_ms: u64,
    flushed_ms: u64,
    candidates: Vec<BatchCandidate>,
}

/// Terminal outcome of one candidate, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateOutcome {
    pub event_id: String,
    pub outcome: TerminalOutcome,
}

/// Replayable record of one resolved batch. Contains no wall-clock values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchReplayRecord {
    pub schema_version: String,
    pub batch_id: u64,
    pub flush_reason: FlushReason,
    pub opened_ms: u64,
    pub flushed_ms: u64,
    pub ranking_source: RankingSource,
    /// Selected event ids in final order.
    pub selected: Vec<String>,
    /// Every candidate's terminal outcome, in arrival order.
    pub outcomes: Vec<CandidateOutcome>,
}

impl BatchReplayRecord {
    pub fn deterministic_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("batch replay record is JSON-safe")
    }
}

/// Result of resolving (or shedding) one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResolution {
    pub record: BatchReplayRecord,
}

/// Observable counters. Wall-clock fields are observational only and never
/// enter a replay record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BurstMetrics {
    /// Chat offered into the batching lane (quiet or batched).
    pub admitted: u64,
    /// Terminal outcomes by category: selected, quiet, capacity, cooldown,
    /// per-batch cap, shed.
    pub terminal: [u64; 6],
    pub immediate: u64,
    pub refused: u64,
    pub priority_claims_ignored: u64,
    pub clock_regressions: u64,
    pub batches_flushed: u64,
    pub flushed_window: u64,
    pub flushed_count: u64,
    pub flushed_bytes: u64,
    pub flushed_shutdown: u64,
    pub batches_shed: u64,
    /// Batch ranker invocations. The headline cost metric is
    /// [`BurstMetrics::ranker_calls_per_comment`].
    pub ranker_calls: u64,
    pub ranker_failures: u64,
    pub ranker_unknown_ids_ignored: u64,
    /// Candidate wait (flush instant minus arrival), logical milliseconds.
    pub wait_ms_total: u64,
    pub wait_ms_max: u64,
    pub waited_candidates: u64,
    /// Observational wall-clock time spent inside rankers.
    pub ranker_wall_us_total: u64,
    pub ranker_wall_us_max: u64,
    pub pending_candidates_high_water: usize,
    pub pending_bytes_high_water: usize,
    pub ready_batches_high_water: usize,
    pub tracked_actors_high_water: usize,
    /// Times `admitted != terminal + pending + ready` was observed. Must be 0.
    pub invariant_violations: u64,
}

impl BurstMetrics {
    pub fn terminal_total(&self) -> u64 {
        self.terminal.iter().sum()
    }

    /// Ranker (Jev) calls per admitted comment. A per-comment design is 1.0.
    pub fn ranker_calls_per_comment(&self) -> f64 {
        if self.admitted == 0 {
            0.0
        } else {
            self.ranker_calls as f64 / self.admitted as f64
        }
    }

    pub fn mean_wait_ms(&self) -> f64 {
        if self.waited_candidates == 0 {
            0.0
        } else {
            self.wait_ms_total as f64 / self.waited_candidates as f64
        }
    }
}

enum Lane {
    Batchable,
    Immediate(ImmediateReason),
    Refused(RefusalReason),
}

/// Classify by the structure the normalizer assigned, never by what the
/// payload or its hints claim.
fn classify(event: &EventEnvelope) -> Lane {
    if event.plane == SecurityPlane::Control {
        return Lane::Immediate(ImmediateReason::ControlPlane);
    }
    if event.plane == SecurityPlane::System {
        return Lane::Immediate(ImmediateReason::SystemPlane);
    }
    // Content plane from here on: it can never carry control authority.
    if event.authorization.is_some() {
        return Lane::Refused(RefusalReason::AuthorizationClaimOnContent);
    }
    if event.kind == EventKind::OperatorCommand || event.source_class == SourceClass::Operator {
        return Lane::Refused(RefusalReason::ControlClaimOnContent);
    }
    if event.kind != EventKind::ChatMessage {
        return Lane::Immediate(ImmediateReason::NonChatKind);
    }
    if event.source_class != SourceClass::PublicChat {
        return Lane::Refused(RefusalReason::InconsistentClassification);
    }
    Lane::Batchable
}

fn truncate_to_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Debug)]
struct Pending {
    opened_ms: u64,
    bytes: usize,
    candidates: Vec<BatchCandidate>,
}

/// The deterministic micro-batching engine. Single-owner and synchronous: it
/// owns no thread, timer or lock, so shutdown has nothing to wait on.
#[derive(Debug)]
pub struct BurstEngine {
    config: BurstConfig,
    pending: Option<Pending>,
    ready: VecDeque<FlushedBatch>,
    arrivals: VecDeque<u64>,
    cooldowns: BTreeMap<String, u64>,
    next_batch_id: u64,
    last_ms: u64,
    shut_down: bool,
    metrics: BurstMetrics,
}

impl BurstEngine {
    pub fn new(config: BurstConfig) -> Result<Self, BurstConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            pending: None,
            ready: VecDeque::new(),
            arrivals: VecDeque::new(),
            cooldowns: BTreeMap::new(),
            next_batch_id: 1,
            last_ms: 0,
            shut_down: false,
            metrics: BurstMetrics::default(),
        })
    }

    pub fn config(&self) -> &BurstConfig {
        &self.config
    }

    pub fn metrics(&self) -> &BurstMetrics {
        &self.metrics
    }

    pub fn pending_len(&self) -> usize {
        self.pending
            .as_ref()
            .map_or(0, |pending| pending.candidates.len())
    }

    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    /// Forget the retained cooldown of an actor who erased their data (#305).
    /// Candidates already pending or ready are not edited (that would break
    /// the exactly-one-outcome accounting); they resolve within one window and
    /// the caller is expected to refuse any selection of an erased actor.
    pub fn forget_actor(&mut self, actor_id: &str) -> bool {
        self.cooldowns.remove(actor_id).is_some()
    }

    /// Offer one admitted event. `local_score_milli` is a caller-computed
    /// deterministic score (for example retrieval similarity) in `0..=1000`;
    /// larger values are clamped. Returns the disposition plus the resolutions
    /// of any batch shed by the bounded ready queue.
    pub fn offer(
        &mut self,
        event: &EventEnvelope,
        now_ms: u64,
        local_score_milli: u16,
    ) -> (Disposition, Vec<BatchResolution>) {
        let now_ms = self.advance(now_ms);
        let mut shed = Vec::new();

        if self.shut_down {
            self.metrics.immediate += 1;
            return (Disposition::Immediate(ImmediateReason::ShuttingDown), shed);
        }
        match classify(event) {
            Lane::Immediate(reason) => {
                self.metrics.immediate += 1;
                return (Disposition::Immediate(reason), shed);
            }
            Lane::Refused(reason) => {
                self.metrics.refused += 1;
                return (Disposition::Refused(reason), shed);
            }
            Lane::Batchable => {}
        }
        if event.event_id.len() > MAX_SNAPSHOT_STRING_BYTES
            || event
                .actor_id
                .as_ref()
                .is_some_and(|actor| actor.len() > MAX_SNAPSHOT_STRING_BYTES)
        {
            self.metrics.immediate += 1;
            return (
                Disposition::Immediate(ImmediateReason::OversizedIdentity),
                shed,
            );
        }

        // A window that has already expired flushes first, at its own logical
        // deadline, so the result does not depend on when anyone polled.
        self.expire_window(now_ms, &mut shed);

        if event.priority_hint.is_some_and(|hint| hint > 0.5) {
            self.metrics.priority_claims_ignored += 1;
        }
        self.metrics.admitted += 1;

        if self.pending.is_none() && !self.in_burst(now_ms) {
            self.metrics.terminal[TerminalOutcome::QuietPassthrough.category()] += 1;
            self.check_invariant();
            return (Disposition::QuietPassthrough, shed);
        }

        let text = event
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(|text| truncate_to_boundary(text, MAX_CANDIDATE_TEXT_BYTES))
            .unwrap_or("");
        let retained_bytes =
            event.event_id.len() + event.actor_id.as_ref().map_or(0, String::len) + text.len();
        let candidate = BatchCandidate {
            event_id: event.event_id.clone(),
            actor_id: event.actor_id.clone(),
            sequence: event.sequence,
            arrival_ms: now_ms,
            local_score_milli: local_score_milli.min(1000),
            text: text.to_owned(),
            retained_bytes,
        };

        // Bytes: flush what is held before this candidate would overflow it.
        let would_overflow = self.pending.as_ref().is_some_and(|pending| {
            !pending.candidates.is_empty()
                && pending.bytes + candidate.retained_bytes > self.config.max_bytes
        });
        if would_overflow {
            self.flush(FlushReason::Bytes, now_ms, &mut shed);
        }

        let pending = self.pending.get_or_insert_with(|| Pending {
            opened_ms: now_ms,
            bytes: 0,
            candidates: Vec::new(),
        });
        pending.bytes += candidate.retained_bytes;
        pending.candidates.push(candidate);
        let held = pending.candidates.len();
        let held_bytes = pending.bytes;
        self.metrics.pending_candidates_high_water =
            self.metrics.pending_candidates_high_water.max(held);
        self.metrics.pending_bytes_high_water =
            self.metrics.pending_bytes_high_water.max(held_bytes);

        // A single oversized candidate still flushes at once rather than
        // sitting over the byte bound.
        if held >= self.config.max_candidates {
            self.flush(FlushReason::Count, now_ms, &mut shed);
        } else if held_bytes >= self.config.max_bytes {
            self.flush(FlushReason::Bytes, now_ms, &mut shed);
        }
        self.check_invariant();
        (Disposition::Batched, shed)
    }

    /// Flush a window that has expired by `now_ms`. Calling it more or less
    /// often never changes a replay record; it only bounds how long a quiet
    /// tail waits in wall time.
    pub fn poll(&mut self, now_ms: u64) -> Vec<BatchResolution> {
        let now_ms = self.advance(now_ms);
        let mut shed = Vec::new();
        self.expire_window(now_ms, &mut shed);
        self.check_invariant();
        shed
    }

    /// Resolve the oldest flushed batch. `ranker` is optional; `None` or any
    /// ranker failure yields the deterministic selection.
    pub fn resolve_next(
        &mut self,
        context: &ReflexContext,
        ranker: Option<&mut dyn BatchRanker>,
    ) -> Option<BatchResolution> {
        let batch = self.ready.pop_front()?;
        let resolution = self.resolve(batch, context, ranker);
        self.check_invariant();
        Some(resolution)
    }

    /// Stop accepting batched work, flush the partial batch and resolve
    /// everything deterministically without consulting a ranker. Nothing
    /// blocks, so shutdown cannot deadlock on a partial batch.
    pub fn shutdown(&mut self, now_ms: u64, context: &ReflexContext) -> Vec<BatchResolution> {
        let now_ms = self.advance(now_ms);
        let mut resolutions = Vec::new();
        self.shut_down = true;
        // Drain what is already flushed first so the final partial batch never
        // pushes the ready queue past its bound.
        while let Some(batch) = self.ready.pop_front() {
            resolutions.push(self.resolve(batch, context, None));
        }
        self.flush(FlushReason::Shutdown, now_ms, &mut resolutions);
        while let Some(batch) = self.ready.pop_front() {
            resolutions.push(self.resolve(batch, context, None));
        }
        self.check_invariant();
        resolutions
    }

    fn advance(&mut self, now_ms: u64) -> u64 {
        // Replay ordering is logical: a regressing clock is clamped and
        // counted, never allowed to reorder decisions.
        if now_ms < self.last_ms {
            self.metrics.clock_regressions += 1;
            return self.last_ms;
        }
        self.last_ms = now_ms;
        now_ms
    }

    fn in_burst(&mut self, now_ms: u64) -> bool {
        self.arrivals.push_back(now_ms);
        while self.arrivals.len() > self.config.burst_enter_count {
            self.arrivals.pop_front();
        }
        self.arrivals.len() == self.config.burst_enter_count
            && self
                .arrivals
                .front()
                .is_some_and(|first| now_ms - first <= self.config.window_ms)
    }

    fn expire_window(&mut self, now_ms: u64, sink: &mut Vec<BatchResolution>) {
        let deadline = self
            .pending
            .as_ref()
            .map(|pending| pending.opened_ms + self.config.window_ms);
        if let Some(deadline) = deadline
            && now_ms >= deadline
        {
            self.flush(FlushReason::Window, deadline, sink);
        }
    }

    fn flush(&mut self, reason: FlushReason, at_ms: u64, shed: &mut Vec<BatchResolution>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.candidates.is_empty() {
            return;
        }
        let batch = FlushedBatch {
            batch_id: self.next_batch_id,
            reason,
            opened_ms: pending.opened_ms,
            flushed_ms: at_ms.max(pending.opened_ms),
            candidates: pending.candidates,
        };
        self.next_batch_id += 1;
        self.metrics.batches_flushed += 1;
        match reason {
            FlushReason::Window => self.metrics.flushed_window += 1,
            FlushReason::Count => self.metrics.flushed_count += 1,
            FlushReason::Bytes => self.metrics.flushed_bytes += 1,
            FlushReason::Shutdown => self.metrics.flushed_shutdown += 1,
        }
        for candidate in &batch.candidates {
            let wait = batch.flushed_ms.saturating_sub(candidate.arrival_ms);
            self.metrics.wait_ms_total += wait;
            self.metrics.wait_ms_max = self.metrics.wait_ms_max.max(wait);
            self.metrics.waited_candidates += 1;
        }
        if self.ready.len() >= self.config.max_ready_batches
            && let Some(oldest) = self.ready.pop_front()
        {
            shed.push(self.shed(oldest));
        }
        self.ready.push_back(batch);
        self.metrics.ready_batches_high_water =
            self.metrics.ready_batches_high_water.max(self.ready.len());
    }

    fn shed(&mut self, batch: FlushedBatch) -> BatchResolution {
        self.metrics.batches_shed += 1;
        let outcomes = batch
            .candidates
            .iter()
            .map(|candidate| {
                self.metrics.terminal[TerminalOutcome::ShedPendingOverflow.category()] += 1;
                CandidateOutcome {
                    event_id: candidate.event_id.clone(),
                    outcome: TerminalOutcome::ShedPendingOverflow,
                }
            })
            .collect();
        BatchResolution {
            record: BatchReplayRecord {
                schema_version: BATCH_ENGAGEMENT_SCHEMA_VERSION.to_owned(),
                batch_id: batch.batch_id,
                flush_reason: batch.reason,
                opened_ms: batch.opened_ms,
                flushed_ms: batch.flushed_ms,
                ranking_source: RankingSource::Skipped,
                selected: Vec::new(),
                outcomes,
            },
        }
    }

    /// Deterministic order: local score descending, then arrival sequence.
    fn deterministic_order(candidates: &[BatchCandidate]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by(|&a, &b| {
            candidates[b]
                .local_score_milli
                .cmp(&candidates[a].local_score_milli)
                .then(candidates[a].sequence.cmp(&candidates[b].sequence))
                .then(a.cmp(&b))
        });
        order
    }

    fn resolve(
        &mut self,
        batch: FlushedBatch,
        context: &ReflexContext,
        ranker: Option<&mut dyn BatchRanker>,
    ) -> BatchResolution {
        let FlushedBatch {
            batch_id,
            reason,
            opened_ms,
            flushed_ms,
            candidates,
        } = batch;
        let order = Self::deterministic_order(&candidates);
        let shortlist_len = order.len().min(self.config.request_candidates);
        let mut shortlist: Vec<usize> = order[..shortlist_len].to_vec();
        let mut outcomes: Vec<Option<TerminalOutcome>> = vec![None; candidates.len()];
        // Past the request capacity nothing is ever shown to a ranker, so those
        // candidates are decided without it.
        for &index in &order[shortlist_len..] {
            outcomes[index] = Some(TerminalOutcome::NotSelectedCapacity);
        }

        let mut source = RankingSource::Deterministic;
        if let Some(ranker) = ranker {
            let request = BatchEngagementRequest {
                schema_version: BATCH_ENGAGEMENT_SCHEMA_VERSION.to_owned(),
                batch_id,
                context: context.clone(),
                candidates: shortlist
                    .iter()
                    .map(|&index| {
                        let candidate = &candidates[index];
                        RequestCandidate {
                            event_id: candidate.event_id.clone(),
                            actor_id: candidate.actor_id.clone(),
                            sequence: candidate.sequence,
                            text: candidate.text.clone(),
                            local_score_milli: candidate.local_score_milli,
                        }
                    })
                    .collect(),
                capacity: CapacityEnvelope {
                    max_selected: self.config.top_k,
                    max_per_actor: self.config.max_per_actor,
                    request_candidates: self.config.request_candidates,
                },
            };
            self.metrics.ranker_calls += 1;
            let started = Instant::now();
            let answer = if request.validate().is_err() {
                Err(BatchRankFailure::Malformed)
            } else {
                ranker.rank(&request)
            };
            let wall = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            self.metrics.ranker_wall_us_total += wall;
            self.metrics.ranker_wall_us_max = self.metrics.ranker_wall_us_max.max(wall);
            match answer {
                Ok(ids) => {
                    source = RankingSource::Ranker;
                    let mut preferred: Vec<usize> = Vec::new();
                    let mut seen = BTreeSet::new();
                    for id in &ids {
                        match shortlist
                            .iter()
                            .copied()
                            .find(|&index| candidates[index].event_id == *id)
                        {
                            Some(index) if seen.insert(index) => preferred.push(index),
                            _ => self.metrics.ranker_unknown_ids_ignored += 1,
                        }
                    }
                    // Ranked ids first, everything else in deterministic order:
                    // the ranker can reorder, never add or drop a candidate.
                    let rest: Vec<usize> = shortlist
                        .iter()
                        .copied()
                        .filter(|index| !seen.contains(index))
                        .collect();
                    preferred.extend(rest);
                    shortlist = preferred;
                }
                Err(_) => {
                    self.metrics.ranker_failures += 1;
                    source = RankingSource::RankerFallback;
                }
            }
        }

        // Deterministic policy has the last word: Top-K, per-batch actor cap
        // and per-actor cooldown are applied to whatever order we ended up with.
        let mut selected: Vec<usize> = Vec::new();
        let mut per_actor: BTreeMap<&str, usize> = BTreeMap::new();
        for &index in &shortlist {
            let candidate = &candidates[index];
            let outcome = if selected.len() >= self.config.top_k {
                TerminalOutcome::NotSelectedCapacity
            } else if let Some(actor) = candidate.actor_id.as_deref() {
                let cooling = self.config.actor_cooldown_ms > 0
                    && self
                        .cooldowns
                        .get(actor)
                        .is_some_and(|&last| flushed_ms < last + self.config.actor_cooldown_ms);
                let taken = per_actor.get(actor).copied().unwrap_or(0);
                if cooling {
                    TerminalOutcome::SuppressedActorCooldown
                } else if taken >= self.config.max_per_actor {
                    TerminalOutcome::SuppressedActorBatchCap
                } else {
                    *per_actor.entry(actor).or_insert(0) += 1;
                    selected.push(index);
                    TerminalOutcome::Selected {
                        rank: selected.len() - 1,
                    }
                }
            } else {
                selected.push(index);
                TerminalOutcome::Selected {
                    rank: selected.len() - 1,
                }
            };
            outcomes[index] = Some(outcome);
        }
        for &index in &selected {
            if let Some(actor) = candidates[index].actor_id.clone() {
                self.stamp_cooldown(actor, flushed_ms);
            }
        }

        let mut records = Vec::with_capacity(candidates.len());
        for (index, candidate) in candidates.iter().enumerate() {
            // Every index was assigned above (shortlist or beyond capacity).
            let outcome = outcomes[index].unwrap_or(TerminalOutcome::NotSelectedCapacity);
            self.metrics.terminal[outcome.category()] += 1;
            records.push(CandidateOutcome {
                event_id: candidate.event_id.clone(),
                outcome,
            });
        }
        BatchResolution {
            record: BatchReplayRecord {
                schema_version: BATCH_ENGAGEMENT_SCHEMA_VERSION.to_owned(),
                batch_id,
                flush_reason: reason,
                opened_ms,
                flushed_ms,
                ranking_source: source,
                selected: selected
                    .iter()
                    .map(|&index| candidates[index].event_id.clone())
                    .collect(),
                outcomes: records,
            },
        }
    }

    fn stamp_cooldown(&mut self, actor: String, at_ms: u64) {
        if !self.cooldowns.contains_key(&actor)
            && self.cooldowns.len() >= self.config.max_tracked_actors
        {
            let cooldown = self.config.actor_cooldown_ms;
            self.cooldowns.retain(|_, last| at_ms < *last + cooldown);
            if self.cooldowns.len() >= self.config.max_tracked_actors {
                // Still full of live cooldowns: evict the oldest stamp, ties
                // broken by key, so eviction is deterministic.
                let victim = self
                    .cooldowns
                    .iter()
                    .min_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)))
                    .map(|(key, _)| key.clone());
                if let Some(victim) = victim {
                    self.cooldowns.remove(&victim);
                }
            }
        }
        self.cooldowns.insert(actor, at_ms);
        self.metrics.tracked_actors_high_water = self
            .metrics
            .tracked_actors_high_water
            .max(self.cooldowns.len());
    }

    fn check_invariant(&mut self) {
        let held = self.pending_len() as u64
            + self
                .ready
                .iter()
                .map(|batch| batch.candidates.len() as u64)
                .sum::<u64>();
        if self.metrics.admitted != self.metrics.terminal_total() + held {
            self.metrics.invariant_violations += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, TrustLevel,
    };

    fn chat(seq: u64, actor: &str) -> EventEnvelope {
        let mut payload = BTreeMap::new();
        payload.insert(
            "text".to_owned(),
            serde_json::Value::String(format!("hello {seq}")),
        );
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: format!("e{seq}"),
            correlation_id: "c".to_owned(),
            sequence: seq,
            observed_at: "2026-10-01T00:00:00Z".to_owned(),
            source: "twitch".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some(actor.to_owned()),
            priority_hint: None,
            authorization: None,
            payload,
        }
    }

    fn ctx() -> ReflexContext {
        ReflexContext::default()
    }

    fn engine(config: BurstConfig) -> BurstEngine {
        BurstEngine::new(config).expect("config")
    }

    #[test]
    fn forgetting_an_actor_drops_their_retained_cooldown() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        for seq in 1..=3u64 {
            engine.offer(&chat(seq, "erased"), seq, 500);
        }
        assert!(engine.resolve_next(&ctx(), None).is_some());
        assert!(engine.forget_actor("erased"), "cooldown was retained");
        assert!(!engine.forget_actor("erased"), "and is now gone");
        assert_eq!(engine.metrics().invariant_violations, 0);
    }

    #[test]
    fn quiet_chat_never_waits() {
        let mut engine = engine(BurstConfig::default());
        for (i, at) in [0u64, 5_000, 10_000, 15_000].iter().enumerate() {
            let (disposition, _) = engine.offer(&chat(i as u64 + 1, "a"), *at, 0);
            assert_eq!(disposition, Disposition::QuietPassthrough);
        }
        assert_eq!(engine.pending_len(), 0);
        assert_eq!(engine.metrics().invariant_violations, 0);
        assert_eq!(engine.metrics().terminal_total(), 4);
    }

    #[test]
    fn burst_batches_then_flushes_on_count() {
        let config = BurstConfig {
            max_candidates: 5,
            burst_enter_count: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        let mut dispositions = Vec::new();
        for seq in 1..=6u64 {
            dispositions.push(engine.offer(&chat(seq, &format!("a{seq}")), seq, 0).0);
        }
        assert_eq!(dispositions[0], Disposition::QuietPassthrough);
        assert!(dispositions[1..].iter().all(|d| *d == Disposition::Batched));
        assert_eq!(engine.ready_len(), 1);
        let resolution = engine.resolve_next(&ctx(), None).expect("ready");
        assert_eq!(resolution.record.flush_reason, FlushReason::Count);
        assert_eq!(resolution.record.selected.len(), 3);
    }

    #[test]
    fn window_flush_uses_logical_deadline_independent_of_poll() {
        let run = |poll: bool| {
            let config = BurstConfig {
                burst_enter_count: 2,
                window_ms: 100,
                ..BurstConfig::default()
            };
            let mut engine = engine(config);
            engine.offer(&chat(1, "a"), 0, 0);
            engine.offer(&chat(2, "b"), 10, 0);
            engine.offer(&chat(3, "c"), 20, 0);
            if poll {
                engine.poll(500);
            }
            engine.offer(&chat(4, "d"), 900, 0);
            let mut bytes = Vec::new();
            while let Some(resolution) = engine.resolve_next(&ctx(), None) {
                bytes.extend(resolution.record.deterministic_bytes());
            }
            bytes
        };
        assert_eq!(run(true), run(false));
    }

    #[test]
    fn control_plane_and_claims_never_batch() {
        let mut engine = engine(BurstConfig::default());
        let mut control = chat(1, "op");
        control.plane = SecurityPlane::Control;
        control.kind = EventKind::OperatorCommand;
        control.source_class = SourceClass::Operator;
        assert_eq!(
            engine.offer(&control, 0, 0).0,
            Disposition::Immediate(ImmediateReason::ControlPlane)
        );

        let mut claim = chat(2, "viewer");
        claim.authorization = Some(AuthorizationContext {
            principal: "x".to_owned(),
            method: AuthorizationMethod::OperatorUi,
            capabilities: [Capability::PerformerStop].into_iter().collect(),
        });
        assert_eq!(
            engine.offer(&claim, 1, 0).0,
            Disposition::Refused(RefusalReason::AuthorizationClaimOnContent)
        );

        let mut spoof = chat(3, "viewer");
        spoof.kind = EventKind::OperatorCommand;
        assert_eq!(
            engine.offer(&spoof, 2, 0).0,
            Disposition::Refused(RefusalReason::ControlClaimOnContent)
        );
        assert_eq!(engine.metrics().admitted, 0);
    }

    #[test]
    fn viewer_priority_hint_cannot_outrank() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 4,
            top_k: 1,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        engine.offer(&chat(1, "a"), 0, 0);
        for seq in 2..=5u64 {
            let mut event = chat(seq, &format!("v{seq}"));
            if seq == 5 {
                event.priority_hint = Some(1.0);
            }
            engine.offer(&event, seq, 0);
        }
        let resolution = engine.resolve_next(&ctx(), None).expect("ready");
        assert_eq!(resolution.record.selected, vec!["e2".to_owned()]);
        assert_eq!(engine.metrics().priority_claims_ignored, 1);
    }

    #[test]
    fn fairness_cap_and_cooldown_apply() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 5,
            top_k: 3,
            max_per_actor: 1,
            actor_cooldown_ms: 1_000,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        engine.offer(&chat(1, "q"), 0, 0);
        // actor "a" floods; "b" and "c" each speak once.
        for (seq, actor) in [(2u64, "a"), (3, "a"), (4, "b"), (5, "a"), (6, "c")] {
            engine.offer(&chat(seq, actor), seq, 0);
        }
        let first = engine.resolve_next(&ctx(), None).expect("ready");
        assert_eq!(first.record.selected, ["e2", "e4", "e6"]);
        assert_eq!(
            first.record.outcomes[1].outcome,
            TerminalOutcome::SuppressedActorBatchCap
        );
        // a second batch inside the cooldown suppresses actor "a" outright.
        for seq in 7..=11u64 {
            engine.offer(&chat(seq, if seq == 7 { "a" } else { "z" }), 10 + seq, 0);
        }
        let second = engine.resolve_next(&ctx(), None).expect("ready");
        assert!(
            second
                .record
                .outcomes
                .iter()
                .any(|o| o.outcome == TerminalOutcome::SuppressedActorCooldown)
        );
        assert_eq!(engine.metrics().invariant_violations, 0);
    }

    struct Reverse;
    impl BatchRanker for Reverse {
        fn rank(&mut self, r: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure> {
            let mut ids: Vec<String> = r.candidates.iter().map(|c| c.event_id.clone()).collect();
            ids.reverse();
            ids.push("not-in-request".to_owned());
            Ok(ids)
        }
    }
    struct Failing;
    impl BatchRanker for Failing {
        fn rank(&mut self, _: &BatchEngagementRequest) -> Result<Vec<String>, BatchRankFailure> {
            Err(BatchRankFailure::Timeout)
        }
    }

    fn burst_of(engine: &mut BurstEngine, n: u64) {
        for seq in 1..=n {
            engine.offer(&chat(seq, &format!("a{seq}")), seq, 0);
        }
    }

    #[test]
    fn ranker_reorders_but_policy_bounds_it() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 6,
            top_k: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        burst_of(&mut engine, 7);
        let mut ranker = Reverse;
        let resolution = engine
            .resolve_next(&ctx(), Some(&mut ranker))
            .expect("ready");
        assert_eq!(resolution.record.ranking_source, RankingSource::Ranker);
        assert_eq!(resolution.record.selected, ["e7", "e6"]);
        assert_eq!(engine.metrics().ranker_unknown_ids_ignored, 1);
        assert_eq!(engine.metrics().ranker_calls, 1);
    }

    #[test]
    fn ranker_failure_falls_back_deterministically() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 6,
            top_k: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        burst_of(&mut engine, 7);
        let mut ranker = Failing;
        let resolution = engine
            .resolve_next(&ctx(), Some(&mut ranker))
            .expect("ready");
        assert_eq!(
            resolution.record.ranking_source,
            RankingSource::RankerFallback
        );
        assert_eq!(resolution.record.selected, ["e2", "e3"]);
        assert_eq!(engine.metrics().ranker_failures, 1);
    }

    #[test]
    fn ready_queue_overflow_sheds_oldest_with_accounting() {
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 2,
            max_ready_batches: 1,
            top_k: 1,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        let mut shed = Vec::new();
        for seq in 1..=9u64 {
            shed.extend(engine.offer(&chat(seq, &format!("a{seq}")), seq, 0).1);
        }
        assert!(!shed.is_empty());
        assert!(engine.metrics().batches_shed > 0);
        assert!(engine.ready_len() <= 1);
        let all = engine.shutdown(100, &ctx());
        assert_eq!(engine.pending_len() + engine.ready_len(), 0);
        assert!(!all.is_empty());
        assert_eq!(engine.metrics().admitted, engine.metrics().terminal_total());
        assert_eq!(engine.metrics().invariant_violations, 0);
    }

    #[test]
    fn shutdown_flushes_partial_batch() {
        let config = BurstConfig {
            burst_enter_count: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        burst_of(&mut engine, 4);
        assert!(engine.pending_len() > 0);
        let resolutions = engine.shutdown(10, &ctx());
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].record.flush_reason, FlushReason::Shutdown);
        assert_eq!(
            engine.offer(&chat(9, "a"), 11, 0).0,
            Disposition::Immediate(ImmediateReason::ShuttingDown)
        );
        assert_eq!(engine.metrics().invariant_violations, 0);
    }

    #[test]
    fn invalid_config_is_refused() {
        for config in [
            BurstConfig {
                burst_enter_count: 1,
                ..BurstConfig::default()
            },
            BurstConfig {
                window_ms: 0,
                ..BurstConfig::default()
            },
            BurstConfig {
                max_candidates: MAX_BATCH_CANDIDATES + 1,
                ..BurstConfig::default()
            },
            BurstConfig {
                top_k: 13,
                ..BurstConfig::default()
            },
            BurstConfig {
                max_per_actor: 4,
                ..BurstConfig::default()
            },
        ] {
            assert!(BurstEngine::new(config).is_err());
        }
    }

    #[test]
    fn request_is_bounded_and_text_truncates_on_char_boundary() {
        let mut event = chat(1, "a");
        event.payload.insert(
            "text".to_owned(),
            serde_json::Value::String("あ".repeat(200)),
        );
        let config = BurstConfig {
            burst_enter_count: 2,
            max_candidates: 2,
            ..BurstConfig::default()
        };
        let mut engine = engine(config);
        engine.offer(&chat(0, "q"), 0, 0);
        engine.offer(&event, 1, 0);
        engine.offer(&chat(2, "b"), 2, 0);
        let batch = engine.ready.front().expect("ready");
        assert!(batch.candidates[0].text().len() <= MAX_CANDIDATE_TEXT_BYTES);
        assert!(batch.candidates[0].text().chars().all(|c| c == 'あ'));
    }
}
