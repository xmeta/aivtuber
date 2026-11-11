//! Issue #75 (S1): typed reply timing and a bounded deferred-response inbox.
//!
//! "Worth replying to" is not "must reply right now". A comment that deserves
//! an answer while the performer is busy waits here, for a bounded time, and
//! is either dispatched when the performer becomes available or ends with an
//! explicit terminal outcome. Nothing is retained without a bound and nothing
//! is dropped without a reason.
//!
//! Load-bearing properties:
//!
//! 1. **Pure and replayable.** No clock, thread or timer. Every instant is a
//!    caller-supplied monotonic millisecond value; a regressing clock is
//!    clamped and counted, never allowed to reorder decisions.
//! 2. **Busy state is a trusted input, never a claim.** [`ReplyAvailability`]
//!    is built by the composition root from the scheduler, the speech
//!    lifecycle and operator control. It has no `Deserialize`, and nothing in
//!    an [`EventEnvelope`](aivtuber_domain::EventEnvelope) can produce one:
//!    chat can neither fake an idle performer (to jump the queue) nor a busy
//!    one (to starve replies).
//! 3. **Expired or cancelled items never dispatch.** [`DeferredInbox::poll`]
//!    sweeps expiry (`expires_at_ms <= now`) before it chooses anything, and
//!    cancellation removes the entry, so no later `poll` can return it.
//! 4. **Bounded.** Capacity, per-actor share and the expiry horizon all have
//!    hard validated caps ([`DeferredConfig::validate`]). A full inbox evicts
//!    its least useful entry with an explicit outcome; it never grows.
//! 5. **Every offered item is accounted exactly once.**
//!    `offered == terminal + pending + in_flight` after every operation; a
//!    violation is counted in [`DeferredMetrics::invariant_violations`] (tests
//!    require zero).
//! 6. **Model output is data.** A timing recommendation (from Jev or a
//!    heuristic) only fills a [`ResponseDisposition`]; this module enforces
//!    the bounds and the expiry regardless of what it recommends.
//!
//! The types here are Rust-internal: [`ResponseDisposition`] is `Serialize`
//! only (no `Deserialize`, no JSON Schema), so it cannot become a second
//! implicit wire contract before an adapter slice defines one.

use serde::Serialize;
use std::collections::BTreeMap;

/// Hard ceiling on inbox capacity.
pub const MAX_DEFERRED_CAPACITY: usize = 256;
/// Hard ceiling on how far ahead of "now" an expiry may be.
pub const MAX_DEFERRED_TTL_MS: u64 = 600_000;
/// Hard ceiling on id / actor / supersession key length.
pub const MAX_DEFERRED_KEY_BYTES: usize = 512;

/// When a reply should happen, independent of its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseTiming {
    /// Answer now; never held.
    Immediate,
    /// Answer once the performer is available, until the item expires.
    WhenAvailable,
    /// Like `WhenAvailable`, but with an explicit deadline and priority over
    /// `WhenAvailable` items when the performer frees up.
    BeforeDeadline,
    /// Not worth a reply; accounted, never held.
    NoReply,
}

/// Why a timing was chosen. A closed set so it can be counted and replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingReason {
    PerformerAvailable,
    PerformerBusy,
    OperatorMuted,
    NotSelected,
    DeadlineBound,
}

/// A typed timing recommendation with its bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResponseDisposition {
    pub timing: ResponseTiming,
    /// The reply is not useful before this logical time.
    pub earliest_at_ms: Option<u64>,
    /// The reply is stale at and after this logical time. Required for every
    /// deferrable timing.
    pub expires_at_ms: Option<u64>,
    /// A later item with the same key replaces this one.
    pub supersession_key: Option<String>,
    pub reason: TimingReason,
}

impl ResponseDisposition {
    pub fn immediate(reason: TimingReason) -> Self {
        Self {
            timing: ResponseTiming::Immediate,
            earliest_at_ms: None,
            expires_at_ms: None,
            supersession_key: None,
            reason,
        }
    }

    pub fn no_reply(reason: TimingReason) -> Self {
        Self {
            timing: ResponseTiming::NoReply,
            earliest_at_ms: None,
            expires_at_ms: None,
            supersession_key: None,
            reason,
        }
    }

    pub fn when_available(
        expires_at_ms: u64,
        supersession_key: Option<String>,
        reason: TimingReason,
    ) -> Self {
        Self {
            timing: ResponseTiming::WhenAvailable,
            earliest_at_ms: None,
            expires_at_ms: Some(expires_at_ms),
            supersession_key,
            reason,
        }
    }

    pub fn before_deadline(deadline_ms: u64, supersession_key: Option<String>) -> Self {
        Self {
            timing: ResponseTiming::BeforeDeadline,
            earliest_at_ms: None,
            expires_at_ms: Some(deadline_ms),
            supersession_key,
            reason: TimingReason::DeadlineBound,
        }
    }
}

/// Whether the performer can take a reply now. Built only from trusted
/// runtime state; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReplyAvailability {
    Available,
    Busy(BusyCause),
}

/// The trusted source of a busy performer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BusyCause {
    /// The scheduler/speech lifecycle reports a live speech unit.
    Speaking,
    /// A generated reply is already being produced.
    Generating,
    /// The operator muted output.
    Muted,
}

impl ReplyAvailability {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Inbox limits. Validated, never clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredConfig {
    /// Items held at once.
    pub capacity: usize,
    /// Items one actor may hold at once.
    pub per_actor_cap: usize,
    /// Longest an item may be asked to wait.
    pub max_ttl_ms: u64,
}

impl Default for DeferredConfig {
    fn default() -> Self {
        Self {
            capacity: 32,
            per_actor_cap: 2,
            max_ttl_ms: 60_000,
        }
    }
}

/// Refusal raised for an out-of-bounds [`DeferredConfig`] field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredConfigError {
    pub field: &'static str,
}

impl std::fmt::Display for DeferredConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid deferred inbox configuration: {}", self.field)
    }
}

impl std::error::Error for DeferredConfigError {}

impl DeferredConfig {
    pub fn validate(&self) -> Result<(), DeferredConfigError> {
        let check = |ok: bool, field: &'static str| {
            if ok {
                Ok(())
            } else {
                Err(DeferredConfigError { field })
            }
        };
        check(
            (1..=MAX_DEFERRED_CAPACITY).contains(&self.capacity),
            "capacity",
        )?;
        check(
            self.per_actor_cap >= 1 && self.per_actor_cap <= self.capacity,
            "per_actor_cap",
        )?;
        check(
            (1..=MAX_DEFERRED_TTL_MS).contains(&self.max_ttl_ms),
            "max_ttl_ms",
        )
    }
}

/// The single terminal outcome of an offered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredOutcome {
    /// Handed to the performer path.
    Dispatched,
    /// Its expiry passed before the performer was available.
    Expired,
    /// A later item with the same supersession key replaced it.
    Superseded,
    /// Its actor erased their data (#305).
    Erased,
    /// Operator stop cancelled it.
    OperatorCancelled,
    /// The security gate refused it when it was re-run before dispatch.
    RevalidationRefused,
    /// Dispatch was attempted and the performer path failed.
    DispatchFailed,
    /// Dropped to make room for a newer item.
    EvictedForCapacity,
    /// Its actor already held their share.
    RejectedActorCap,
    /// Not deferrable (timing, expiry or key out of bounds).
    RejectedInvalid,
    /// The app is shutting down.
    ShutdownDropped,
}

impl DeferredOutcome {
    pub const COUNT: usize = 11;

    pub fn category(self) -> usize {
        match self {
            Self::Dispatched => 0,
            Self::Expired => 1,
            Self::Superseded => 2,
            Self::Erased => 3,
            Self::OperatorCancelled => 4,
            Self::RevalidationRefused => 5,
            Self::DispatchFailed => 6,
            Self::EvictedForCapacity => 7,
            Self::RejectedActorCap => 8,
            Self::RejectedInvalid => 9,
            Self::ShutdownDropped => 10,
        }
    }
}

/// An item offered to the inbox.
#[derive(Debug, Clone)]
pub struct DeferredItem<T> {
    /// Stable id (the event id); reported in terminal outcomes.
    pub id: String,
    /// Fairness key (source + actor); `None` is exempt from the per-actor cap.
    pub actor: Option<String>,
    pub disposition: ResponseDisposition,
    pub payload: T,
}

/// A terminal outcome for one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeferredTerminal {
    pub id: String,
    pub outcome: DeferredOutcome,
}

/// What [`DeferredInbox::defer`] did with the offered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferResult {
    /// Held; it will get exactly one terminal outcome later.
    Accepted,
    /// Not held; the terminal outcome is in the returned list.
    Rejected(DeferredOutcome),
}

/// Counters. Wall-clock free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeferredMetrics {
    pub offered: u64,
    pub accepted: u64,
    pub terminal: [u64; DeferredOutcome::COUNT],
    pub pending: usize,
    pub in_flight: usize,
    pub pending_high_water: usize,
    pub clock_regressions: u64,
    /// Logical wait (dispatch instant minus enqueue instant) of dispatched
    /// items, milliseconds.
    pub wait_ms_total: u64,
    pub wait_ms_max: u64,
    /// Times `offered != terminal + pending + in_flight` was observed. Must be 0.
    pub invariant_violations: u64,
}

impl DeferredMetrics {
    pub fn terminal_total(&self) -> u64 {
        self.terminal.iter().sum()
    }

    pub fn count(&self, outcome: DeferredOutcome) -> u64 {
        self.terminal[outcome.category()]
    }
}

#[derive(Debug)]
struct Entry<T> {
    seq: u64,
    enqueued_ms: u64,
    item: DeferredItem<T>,
}

impl<T> Entry<T> {
    fn expires(&self) -> u64 {
        self.item.disposition.expires_at_ms.unwrap_or(u64::MAX)
    }

    /// Dispatch order: explicit deadlines first (soonest first), then the
    /// rest oldest-first.
    fn dispatch_key(&self) -> (u8, u64, u64) {
        let class = match self.item.disposition.timing {
            ResponseTiming::BeforeDeadline => 0,
            _ => 1,
        };
        let when = if class == 0 { self.expires() } else { 0 };
        (class, when, self.seq)
    }

    /// Eviction order: the item that would expire first goes first.
    fn eviction_key(&self) -> (u64, u64) {
        (self.expires(), self.seq)
    }
}

/// A dequeued item that must be resolved with [`DeferredInbox::complete`].
#[derive(Debug)]
#[must_use = "a ready deferred item must be completed so it is accounted"]
pub struct ReadyDeferred<T> {
    pub item: DeferredItem<T>,
    enqueued_ms: u64,
}

/// Bounded deferred-response inbox. Single-owner and synchronous.
#[derive(Debug)]
pub struct DeferredInbox<T> {
    config: DeferredConfig,
    entries: Vec<Entry<T>>,
    next_seq: u64,
    last_ms: u64,
    metrics: DeferredMetrics,
}

impl<T> DeferredInbox<T> {
    pub fn new(config: DeferredConfig) -> Result<Self, DeferredConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            entries: Vec::new(),
            next_seq: 0,
            last_ms: 0,
            metrics: DeferredMetrics::default(),
        })
    }

    pub fn config(&self) -> &DeferredConfig {
        &self.config
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn metrics(&self) -> DeferredMetrics {
        let mut metrics = self.metrics;
        metrics.pending = self.entries.len();
        metrics
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.item.id == id)
    }

    fn advance(&mut self, now_ms: u64) -> u64 {
        if now_ms < self.last_ms {
            self.metrics.clock_regressions += 1;
            return self.last_ms;
        }
        self.last_ms = now_ms;
        now_ms
    }

    fn record(&mut self, id: String, outcome: DeferredOutcome, sink: &mut Vec<DeferredTerminal>) {
        self.metrics.terminal[outcome.category()] += 1;
        sink.push(DeferredTerminal { id, outcome });
    }

    fn check_invariant(&mut self) {
        let terminal = self.metrics.terminal_total();
        let held = self.entries.len() as u64 + self.metrics.in_flight as u64;
        if self.metrics.offered != terminal + held {
            self.metrics.invariant_violations += 1;
        }
    }

    /// Offer an item. Returns whether it was held plus the terminal outcomes
    /// this call produced (the item itself when rejected, and any item it
    /// replaced or evicted).
    pub fn defer(
        &mut self,
        item: DeferredItem<T>,
        now_ms: u64,
    ) -> (DeferResult, Vec<DeferredTerminal>) {
        let now_ms = self.advance(now_ms);
        let mut terminals = Vec::new();
        self.metrics.offered += 1;

        let disposition = &item.disposition;
        let valid = matches!(
            disposition.timing,
            ResponseTiming::WhenAvailable | ResponseTiming::BeforeDeadline
        ) && disposition
            .expires_at_ms
            .is_some_and(|expires| expires > now_ms && expires - now_ms <= self.config.max_ttl_ms)
            && disposition
                .earliest_at_ms
                .zip(disposition.expires_at_ms)
                .is_none_or(|(earliest, expires)| earliest < expires)
            && !item.id.is_empty()
            && item.id.len() <= MAX_DEFERRED_KEY_BYTES
            && item
                .actor
                .as_ref()
                .is_none_or(|actor| actor.len() <= MAX_DEFERRED_KEY_BYTES)
            && disposition
                .supersession_key
                .as_ref()
                .is_none_or(|key| key.len() <= MAX_DEFERRED_KEY_BYTES);
        if !valid {
            self.record(item.id, DeferredOutcome::RejectedInvalid, &mut terminals);
            self.check_invariant();
            return (
                DeferResult::Rejected(DeferredOutcome::RejectedInvalid),
                terminals,
            );
        }

        // A revision of an already-deferred item replaces it. This frees the
        // slot and the actor's share first.
        if let Some(key) = item.disposition.supersession_key.as_deref()
            && let Some(position) = self
                .entries
                .iter()
                .position(|entry| entry.item.disposition.supersession_key.as_deref() == Some(key))
        {
            let old = self.entries.remove(position);
            self.record(old.item.id, DeferredOutcome::Superseded, &mut terminals);
        }

        if let Some(actor) = item.actor.as_deref() {
            let held = self
                .entries
                .iter()
                .filter(|entry| entry.item.actor.as_deref() == Some(actor))
                .count();
            if held >= self.config.per_actor_cap {
                self.record(item.id, DeferredOutcome::RejectedActorCap, &mut terminals);
                self.check_invariant();
                return (
                    DeferResult::Rejected(DeferredOutcome::RejectedActorCap),
                    terminals,
                );
            }
        }

        if self.entries.len() >= self.config.capacity
            && let Some(position) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.eviction_key())
                .map(|(position, _)| position)
        {
            let evicted = self.entries.remove(position);
            self.record(
                evicted.item.id,
                DeferredOutcome::EvictedForCapacity,
                &mut terminals,
            );
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push(Entry {
            seq,
            enqueued_ms: now_ms,
            item,
        });
        self.metrics.accepted += 1;
        self.metrics.pending_high_water = self.metrics.pending_high_water.max(self.entries.len());
        self.check_invariant();
        (DeferResult::Accepted, terminals)
    }

    /// Sweep expiry, then, if the performer is available and nothing is
    /// already in flight, hand out the best ready item. Expiry is checked
    /// before availability so a stale item can never be chosen.
    pub fn poll(
        &mut self,
        now_ms: u64,
        availability: ReplyAvailability,
    ) -> (Vec<DeferredTerminal>, Option<ReadyDeferred<T>>) {
        let now_ms = self.advance(now_ms);
        let mut terminals = Vec::new();

        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].expires() <= now_ms {
                let entry = self.entries.remove(index);
                self.record(entry.item.id, DeferredOutcome::Expired, &mut terminals);
            } else {
                index += 1;
            }
        }

        let ready = if availability.is_available() && self.metrics.in_flight == 0 {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    entry
                        .item
                        .disposition
                        .earliest_at_ms
                        .is_none_or(|earliest| earliest <= now_ms)
                })
                .min_by_key(|(_, entry)| entry.dispatch_key())
                .map(|(position, _)| position)
                .map(|position| {
                    let entry = self.entries.remove(position);
                    self.metrics.in_flight += 1;
                    ReadyDeferred {
                        enqueued_ms: entry.enqueued_ms,
                        item: entry.item,
                    }
                })
        } else {
            None
        };
        self.check_invariant();
        (terminals, ready)
    }

    /// Resolve a handed-out item. `Dispatched` records its logical wait.
    pub fn complete(
        &mut self,
        ready: ReadyDeferred<T>,
        outcome: DeferredOutcome,
        now_ms: u64,
    ) -> DeferredTerminal {
        let now_ms = self.advance(now_ms);
        self.metrics.in_flight = self.metrics.in_flight.saturating_sub(1);
        if outcome == DeferredOutcome::Dispatched {
            let wait = now_ms.saturating_sub(ready.enqueued_ms);
            self.metrics.wait_ms_total += wait;
            self.metrics.wait_ms_max = self.metrics.wait_ms_max.max(wait);
        }
        let mut sink = Vec::new();
        self.record(ready.item.id, outcome, &mut sink);
        self.check_invariant();
        sink.remove(0)
    }

    /// Cancel every held item (operator stop, shutdown).
    pub fn cancel_all(&mut self, outcome: DeferredOutcome) -> Vec<DeferredTerminal> {
        let mut terminals = Vec::new();
        for entry in std::mem::take(&mut self.entries) {
            self.record(entry.item.id, outcome, &mut terminals);
        }
        self.check_invariant();
        terminals
    }

    /// Cancel every held item of one actor (erasure, #305).
    pub fn cancel_actor(&mut self, actor: &str, outcome: DeferredOutcome) -> Vec<DeferredTerminal> {
        let mut terminals = Vec::new();
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|entry| entry.item.actor.as_deref() == Some(actor));
        self.entries = kept;
        for entry in gone {
            self.record(entry.item.id, outcome, &mut terminals);
        }
        self.check_invariant();
        terminals
    }

    /// Held ids in dispatch order, for diagnostics and tests.
    pub fn pending_ids(&self) -> Vec<&str> {
        let mut ordered: BTreeMap<(u8, u64, u64), &str> = BTreeMap::new();
        for entry in &self.entries {
            ordered.insert(entry.dispatch_key(), entry.item.id.as_str());
        }
        ordered.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, actor: Option<&str>, expires: u64) -> DeferredItem<()> {
        DeferredItem {
            id: id.to_owned(),
            actor: actor.map(str::to_owned),
            disposition: ResponseDisposition::when_available(
                expires,
                Some(id.to_owned()),
                TimingReason::PerformerBusy,
            ),
            payload: (),
        }
    }

    fn inbox(capacity: usize, per_actor: usize) -> DeferredInbox<()> {
        DeferredInbox::new(DeferredConfig {
            capacity,
            per_actor_cap: per_actor,
            max_ttl_ms: 10_000,
        })
        .expect("config")
    }

    #[test]
    fn config_bounds_are_validated_not_clamped() {
        for (config, field) in [
            (
                DeferredConfig {
                    capacity: 0,
                    ..DeferredConfig::default()
                },
                "capacity",
            ),
            (
                DeferredConfig {
                    capacity: MAX_DEFERRED_CAPACITY + 1,
                    ..DeferredConfig::default()
                },
                "capacity",
            ),
            (
                DeferredConfig {
                    per_actor_cap: 0,
                    ..DeferredConfig::default()
                },
                "per_actor_cap",
            ),
            (
                DeferredConfig {
                    per_actor_cap: 33,
                    ..DeferredConfig::default()
                },
                "per_actor_cap",
            ),
            (
                DeferredConfig {
                    max_ttl_ms: 0,
                    ..DeferredConfig::default()
                },
                "max_ttl_ms",
            ),
            (
                DeferredConfig {
                    max_ttl_ms: MAX_DEFERRED_TTL_MS + 1,
                    ..DeferredConfig::default()
                },
                "max_ttl_ms",
            ),
        ] {
            assert_eq!(config.validate().unwrap_err().field, field);
        }
        assert!(DeferredConfig::default().validate().is_ok());
    }

    #[test]
    fn a_busy_performer_holds_and_an_available_one_dispatches_oldest_first() {
        let mut inbox = inbox(4, 4);
        for (id, at) in [("a", 0), ("b", 1), ("c", 2)] {
            let (result, _) = inbox.defer(item(id, None, 5_000), at);
            assert_eq!(result, DeferResult::Accepted);
        }
        let busy = ReplyAvailability::Busy(BusyCause::Speaking);
        let (terminals, ready) = inbox.poll(100, busy);
        assert!(terminals.is_empty() && ready.is_none(), "busy holds");
        let (_, ready) = inbox.poll(200, ReplyAvailability::Available);
        let ready = ready.expect("ready");
        assert_eq!(ready.item.id, "a");
        // One at a time: nothing else is handed out until this is resolved.
        let (_, second) = inbox.poll(201, ReplyAvailability::Available);
        assert!(second.is_none());
        inbox.complete(ready, DeferredOutcome::Dispatched, 210);
        let (_, next) = inbox.poll(211, ReplyAvailability::Available);
        assert_eq!(next.expect("next").item.id, "b");
    }

    #[test]
    fn an_expired_item_never_dispatches_even_when_the_performer_is_free() {
        let mut inbox = inbox(4, 4);
        inbox.defer(item("old", None, 1_000), 0);
        inbox.defer(item("fresh", None, 9_000), 10);
        // Exactly at the expiry instant the item is already stale.
        let (terminals, ready) = inbox.poll(1_000, ReplyAvailability::Available);
        assert_eq!(
            terminals,
            vec![DeferredTerminal {
                id: "old".into(),
                outcome: DeferredOutcome::Expired
            }]
        );
        assert_eq!(ready.expect("fresh").item.id, "fresh");
        assert_eq!(inbox.metrics().count(DeferredOutcome::Expired), 1);
    }

    #[test]
    fn before_deadline_items_jump_ahead_soonest_first() {
        let mut inbox = inbox(8, 8);
        inbox.defer(item("plain", None, 9_000), 0);
        inbox.defer(
            DeferredItem {
                id: "late".into(),
                actor: None,
                disposition: ResponseDisposition::before_deadline(8_000, Some("late".into())),
                payload: (),
            },
            1,
        );
        inbox.defer(
            DeferredItem {
                id: "soon".into(),
                actor: None,
                disposition: ResponseDisposition::before_deadline(3_000, Some("soon".into())),
                payload: (),
            },
            2,
        );
        assert_eq!(inbox.pending_ids(), ["soon", "late", "plain"]);
    }

    #[test]
    fn invalid_dispositions_are_rejected_with_a_terminal_outcome() {
        let mut inbox = inbox(4, 4);
        let now = 100;
        let mut earliest = item("earliest", None, 5_000);
        earliest.disposition.earliest_at_ms = Some(5_000);
        let bad = vec![
            // Not deferrable timings.
            DeferredItem {
                id: "imm".into(),
                actor: None,
                disposition: ResponseDisposition::immediate(TimingReason::PerformerAvailable),
                payload: (),
            },
            DeferredItem {
                id: "no".into(),
                actor: None,
                disposition: ResponseDisposition::no_reply(TimingReason::NotSelected),
                payload: (),
            },
            // Already expired, expiring now, and beyond the horizon.
            item("past", None, 50),
            item("now", None, now),
            item("far", None, now + 10_001),
            // Oversized key.
            item(&"k".repeat(MAX_DEFERRED_KEY_BYTES + 1), None, 5_000),
            // earliest not before expiry.
            earliest,
        ];
        let count = bad.len() as u64;
        for item in bad {
            let (result, terminals) = inbox.defer(item, now);
            assert_eq!(
                result,
                DeferResult::Rejected(DeferredOutcome::RejectedInvalid)
            );
            assert_eq!(terminals.len(), 1);
        }
        assert!(inbox.is_empty());
        let metrics = inbox.metrics();
        assert_eq!(metrics.count(DeferredOutcome::RejectedInvalid), count);
        assert_eq!(metrics.invariant_violations, 0);
    }

    #[test]
    fn the_per_actor_cap_rejects_the_newcomer_and_spares_other_actors() {
        let mut inbox = inbox(8, 2);
        assert_eq!(
            inbox.defer(item("a1", Some("a"), 5_000), 0).0,
            DeferResult::Accepted
        );
        assert_eq!(
            inbox.defer(item("a2", Some("a"), 5_000), 1).0,
            DeferResult::Accepted
        );
        let (result, terminals) = inbox.defer(item("a3", Some("a"), 5_000), 2);
        assert_eq!(
            result,
            DeferResult::Rejected(DeferredOutcome::RejectedActorCap)
        );
        assert_eq!(terminals[0].id, "a3");
        assert_eq!(
            inbox.defer(item("b1", Some("b"), 5_000), 3).0,
            DeferResult::Accepted
        );
        assert_eq!(inbox.len(), 3);
    }

    #[test]
    fn a_full_inbox_evicts_the_item_that_would_expire_first() {
        let mut inbox = inbox(2, 2);
        inbox.defer(item("soon", None, 1_000), 0);
        inbox.defer(item("later", None, 8_000), 1);
        let (result, terminals) = inbox.defer(item("new", None, 5_000), 2);
        assert_eq!(result, DeferResult::Accepted);
        assert_eq!(
            terminals,
            vec![DeferredTerminal {
                id: "soon".into(),
                outcome: DeferredOutcome::EvictedForCapacity
            }]
        );
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn a_revision_with_the_same_key_supersedes_the_older_item() {
        let mut inbox = inbox(4, 1);
        let mut first = item("evt-1", Some("a"), 5_000);
        first.disposition.supersession_key = Some("topic".into());
        let mut second = item("evt-2", Some("a"), 6_000);
        second.disposition.supersession_key = Some("topic".into());
        inbox.defer(first, 0);
        // The actor is at their cap of 1, but the replacement frees the share.
        let (result, terminals) = inbox.defer(second, 1);
        assert_eq!(result, DeferResult::Accepted);
        assert_eq!(
            terminals,
            vec![DeferredTerminal {
                id: "evt-1".into(),
                outcome: DeferredOutcome::Superseded
            }]
        );
        assert_eq!(inbox.pending_ids(), ["evt-2"]);
    }

    #[test]
    fn cancelled_items_can_never_be_dispatched() {
        let mut inbox = inbox(8, 8);
        inbox.defer(item("a1", Some("a"), 5_000), 0);
        inbox.defer(item("b1", Some("b"), 5_000), 1);
        inbox.defer(item("c1", Some("c"), 5_000), 2);
        let erased = inbox.cancel_actor("a", DeferredOutcome::Erased);
        assert_eq!(erased.len(), 1);
        let stopped = inbox.cancel_all(DeferredOutcome::OperatorCancelled);
        assert_eq!(stopped.len(), 2);
        let (_, ready) = inbox.poll(10, ReplyAvailability::Available);
        assert!(ready.is_none());
        let metrics = inbox.metrics();
        assert_eq!(metrics.count(DeferredOutcome::Erased), 1);
        assert_eq!(metrics.count(DeferredOutcome::OperatorCancelled), 2);
        assert_eq!(metrics.count(DeferredOutcome::Dispatched), 0);
        assert_eq!(metrics.invariant_violations, 0);
    }

    #[test]
    fn a_regressing_clock_is_clamped_and_counted_never_reordering() {
        let mut inbox = inbox(4, 4);
        inbox.defer(item("a", None, 2_000), 1_000);
        // Time going backwards must not resurrect an expired decision.
        let (terminals, ready) = inbox.poll(2_000, ReplyAvailability::Busy(BusyCause::Muted));
        assert_eq!(terminals.len(), 1);
        assert!(ready.is_none());
        inbox.defer(item("b", None, 9_000), 10);
        let metrics = inbox.metrics();
        assert_eq!(metrics.clock_regressions, 1);
        assert_eq!(metrics.invariant_violations, 0);
    }

    #[test]
    fn every_offered_item_is_accounted_exactly_once_under_churn() {
        let mut inbox = inbox(5, 2);
        let mut now = 0_u64;
        for round in 0..400_u64 {
            now += 7;
            let actor = format!("actor-{}", round % 9);
            let _ = inbox.defer(
                item(
                    &format!("evt-{round}"),
                    Some(&actor),
                    now + 100 + (round % 13) * 40,
                ),
                now,
            );
            let availability = if round % 5 == 0 {
                ReplyAvailability::Available
            } else {
                ReplyAvailability::Busy(BusyCause::Speaking)
            };
            let (_, ready) = inbox.poll(now, availability);
            if let Some(ready) = ready {
                let outcome = if round % 3 == 0 {
                    DeferredOutcome::RevalidationRefused
                } else {
                    DeferredOutcome::Dispatched
                };
                inbox.complete(ready, outcome, now);
            }
            if round % 97 == 0 {
                inbox.cancel_actor(&actor, DeferredOutcome::Erased);
            }
            assert!(inbox.len() <= 5);
        }
        inbox.cancel_all(DeferredOutcome::ShutdownDropped);
        let metrics = inbox.metrics();
        assert_eq!(metrics.invariant_violations, 0);
        assert_eq!(metrics.offered, metrics.terminal_total());
        assert_eq!(metrics.pending, 0);
        assert_eq!(metrics.in_flight, 0);
        assert!(metrics.pending_high_water <= 5);
    }

    #[test]
    fn the_same_inputs_replay_to_the_same_outcomes() {
        let run = || {
            let mut inbox = inbox(3, 2);
            let mut log = Vec::new();
            for round in 0..60_u64 {
                let (_, mut terminals) = inbox.defer(
                    item(
                        &format!("e{round}"),
                        Some(&format!("a{}", round % 4)),
                        round * 10 + 150,
                    ),
                    round * 10,
                );
                let availability = if round % 4 == 3 {
                    ReplyAvailability::Available
                } else {
                    ReplyAvailability::Busy(BusyCause::Generating)
                };
                let (expired, ready) = inbox.poll(round * 10, availability);
                terminals.extend(expired);
                if let Some(ready) = ready {
                    terminals.push(inbox.complete(ready, DeferredOutcome::Dispatched, round * 10));
                }
                log.push(terminals);
            }
            serde_json::to_vec(&log).expect("json")
        };
        assert_eq!(run(), run());
    }
}
