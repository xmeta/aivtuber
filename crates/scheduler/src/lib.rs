//! Deterministic performance scheduling primitives.
//!
//! The scheduler owns logical time, lifecycle, safe-point preemption,
//! resource arbitration, cooldowns, and deterministic variation.

#![forbid(unsafe_code)]

mod replay;
pub use replay::*;

use aivtuber_domain::EventKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Coarse execution priority. Lower rank preempts higher rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Background,
    Commentary,
    Conversation,
    StrongReaction,
    HighPriorityInteraction,
    Operator,
}

impl Priority {
    pub fn rank(self) -> u8 {
        match self {
            Self::Background => 5,
            Self::Commentary => 4,
            Self::Conversation => 3,
            Self::StrongReaction => 2,
            Self::HighPriorityInteraction => 1,
            Self::Operator => 0,
        }
    }
}

pub fn priority_for_event(kind: EventKind) -> Priority {
    match kind {
        EventKind::OperatorCommand => Priority::Operator,
        EventKind::ChatDonation => Priority::HighPriorityInteraction,
        EventKind::GameEvent => Priority::StrongReaction,
        EventKind::ChatMessage | EventKind::SpeechInput => Priority::Conversation,
        EventKind::StreamEvent => Priority::Commentary,
        EventKind::TimerTick | EventKind::SystemHealth => Priority::Background,
    }
}

/// Independently blendable execution resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlendChannel {
    Audio,
    Face,
    Body,
    Gaze,
    Overlay,
}

/// A planned performance on the shared monotonic timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPerformance {
    pub event_id: String,
    pub asset_id: String,
    pub priority: Priority,
    pub interruptible: bool,
    /// Sorted millisecond offsets from start_at_ms that are safe to leave.
    pub interrupt_points_ms: Vec<u64>,
    pub start_at_ms: u64,
    pub duration_ms: u64,
    /// Monotonic generation id used as a cancellation token.
    pub generation: u64,
    /// Exclusive work conflicts with every other active/reserved performance.
    pub exclusive: bool,
    /// Non-exclusive work may overlap only when channel sets are disjoint.
    pub channels: BTreeSet<BlendChannel>,
}

impl PlannedPerformance {
    pub fn end_at_ms(&self) -> u64 {
        self.start_at_ms.saturating_add(self.duration_ms)
    }
}

/// Lifecycle state of a scheduled item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Playing,
    Completed,
    Cancelled,
}

/// A planned performance plus scheduler-owned lifecycle metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledItem {
    pub plan: PlannedPerformance,
    pub status: Status,
    /// Deterministic future cancellation boundary for safe-point preemption.
    pub cancel_at_ms: Option<u64>,
}

impl ScheduledItem {
    fn effective_end_ms(&self) -> u64 {
        self.cancel_at_ms
            .unwrap_or_else(|| self.plan.end_at_ms())
            .min(self.plan.end_at_ms())
    }

    fn occupies_timeline(&self) -> bool {
        matches!(self.status, Status::Queued | Status::Playing)
    }
}

/// Why the most recent schedule request was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    Cooldown,
    Priority,
}

/// How terminal (completed/cancelled) items are retained for replay/debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryPolicy {
    /// Keep terminal items in a FIFO ring of at most `history_capacity`
    /// entries. The production default: memory is bounded by the ring, not
    /// by total scheduled events. Evicted entries are counted in
    /// [`SchedulerMetrics::terminal_evicted`]; export them to an external
    /// telemetry/replay sink first via [`Scheduler::drain_history`] to keep a
    /// complete lifetime record.
    Ring,
    /// Keep every terminal item. Intended for deterministic replay tooling,
    /// which needs complete expected traces; memory grows with lifetime
    /// event count.
    RetainAll,
}

/// A terminal (completed/cancelled) scheduler record retained for replay and
/// debugging. Retention is bounded by [`SchedulerConfig::history_capacity`]
/// under [`HistoryPolicy::Ring`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Logical time at which the item reached its terminal state.
    pub terminal_at_ms: u64,
    /// Logical time at which the item was scheduled.
    pub scheduled_at_ms: u64,
    pub item: ScheduledItem,
}

/// Live-state counters for metrics/telemetry export.
///
/// `queued`/`playing`/`active` are bounded by live and future work.
/// `terminal_retained` is bounded by [`SchedulerConfig::history_capacity`]
/// under [`HistoryPolicy::Ring`]. `terminal_evicted` counts history entries
/// dropped by ring compaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerMetrics {
    pub queued: usize,
    pub playing: usize,
    pub active: usize,
    pub terminal_retained: usize,
    pub terminal_evicted: usize,
}

/// Scheduler configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    pub min_reaction_spacing_ms: u64,
    /// Maximum number of terminal items retained in the live process under
    /// [`HistoryPolicy::Ring`]. Older entries are evicted (export them first
    /// via [`Scheduler::drain_history`]) once the ring is full.
    pub history_capacity: usize,
    /// Terminal-history retention policy; defaults to the bounded ring.
    pub history_policy: HistoryPolicy,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            min_reaction_spacing_ms: 1500,
            history_capacity: 1024,
            history_policy: HistoryPolicy::Ring,
        }
    }
}

impl SchedulerConfig {
    /// Config for deterministic replay harnesses that must retain a complete
    /// expected trace, including every terminal transition.
    pub fn replay() -> Self {
        Self {
            history_capacity: usize::MAX,
            history_policy: HistoryPolicy::RetainAll,
            ..Self::default()
        }
    }
}

/// Deterministic timeline owner and arbiter.
#[derive(Debug)]
pub struct Scheduler {
    items: Vec<ScheduledItem>,
    /// O(log n) generation -> item-position liveness index. Only live
    /// (queued/playing) items are present; entries are removed atomically at
    /// every terminal transition, so operator stop and cancellations cannot
    /// leave stale entries and compacted history cannot resurrect stale
    /// audio/avatar actions.
    live_by_generation: BTreeMap<u64, usize>,
    /// O(log n) event id -> item-position liveness index.
    live_by_event: BTreeMap<String, usize>,
    /// Bounded terminal history (ring under [`HistoryPolicy::Ring`], unbounded
    /// under [`HistoryPolicy::RetainAll`]).
    history: std::collections::VecDeque<HistoryEntry>,
    cooldowns: BTreeMap<String, u64>,
    generation: u64,
    config: SchedulerConfig,
    last_rejection: Option<Rejection>,
    now_ms: u64,
    last_terminal_at: Option<u64>,
    terminal_evicted: usize,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            items: Vec::new(),
            live_by_generation: BTreeMap::new(),
            live_by_event: BTreeMap::new(),
            history: std::collections::VecDeque::new(),
            cooldowns: BTreeMap::new(),
            generation: 0,
            config,
            last_rejection: None,
            now_ms: 0,
            last_terminal_at: None,
            terminal_evicted: 0,
        }
    }

    pub fn items(&self) -> &[ScheduledItem] {
        &self.items
    }

    /// Bounded terminal history retained for replay/debugging.
    pub fn history(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.history.iter()
    }

    /// Export and drop all retained terminal history, keeping the live-state
    /// ring bounded. Feed the returned entries to an external telemetry/replay
    /// sink; evicted entries are counted in [`Scheduler::metrics`].
    pub fn drain_history(&mut self) -> Vec<HistoryEntry> {
        self.history.drain(..).collect()
    }

    /// Live-state metrics: queued/playing/active plus terminal retention and
    /// eviction counters.
    pub fn metrics(&self) -> SchedulerMetrics {
        let queued = self
            .items
            .iter()
            .filter(|item| item.status == Status::Queued)
            .count();
        let playing = self
            .items
            .iter()
            .filter(|item| item.status == Status::Playing)
            .count();
        SchedulerMetrics {
            queued,
            playing,
            active: queued + playing,
            terminal_retained: self.history.len(),
            terminal_evicted: self.terminal_evicted,
        }
    }

    /// O(log n) liveness: is the generation still queued or playing?
    pub fn is_generation_live(&self, generation: u64) -> bool {
        self.live_by_generation.contains_key(&generation)
    }

    /// O(log n) liveness lookup by generation. Only live items are indexed.
    pub fn live_item_by_generation(&self, generation: u64) -> Option<&ScheduledItem> {
        let index = *self.live_by_generation.get(&generation)?;
        self.items.get(index)
    }

    /// O(log n) liveness lookup by event id. Only live items are indexed.
    pub fn live_item_by_event(&self, event_id: &str) -> Option<&ScheduledItem> {
        let index = *self.live_by_event.get(event_id)?;
        self.items.get(index)
    }

    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn last_rejection(&self) -> Option<Rejection> {
        self.last_rejection
    }

    pub fn playing(&self) -> Option<&ScheduledItem> {
        self.items
            .iter()
            .find(|item| item.status == Status::Playing)
    }

    pub fn playing_items(&self) -> impl Iterator<Item = &ScheduledItem> {
        self.items
            .iter()
            .filter(|item| item.status == Status::Playing)
    }

    /// Retire one terminal item: move it into bounded history, remove its
    /// liveness-index entries atomically, and swap-remove the slot from the
    /// live collection. The previously-last item is re-indexed at `index` so
    /// both indexes stay consistent with the compacted live collection.
    fn retire(&mut self, index: usize, terminal_at_ms: u64) {
        let item = self.items.swap_remove(index);
        self.live_by_generation.remove(&item.plan.generation);
        self.live_by_event.remove(&item.plan.event_id);

        self.history.push_back(HistoryEntry {
            terminal_at_ms,
            scheduled_at_ms: item.plan.start_at_ms,
            item,
        });
        self.trim_history();

        if let Some(moved) = self.items.get(index) {
            self.live_by_generation.insert(moved.plan.generation, index);
            self.live_by_event
                .insert(moved.plan.event_id.clone(), index);
        }
    }

    fn trim_history(&mut self) {
        if self.config.history_policy == HistoryPolicy::RetainAll {
            return;
        }
        while self.history.len() > self.config.history_capacity {
            self.history.pop_front();
            self.terminal_evicted = self.terminal_evicted.saturating_add(1);
        }
    }

    /// Advance logical time without consulting wall-clock time.
    pub fn advance_to(&mut self, at_ms: u64) {
        let target = at_ms.max(self.now_ms);

        // Retiring items swap-removes slots, so re-check the slot at the same
        // index (without bumping `index`) after every terminal transition.
        let mut index = 0;
        while index < self.items.len() {
            let natural_end = self.items[index].plan.end_at_ms();
            let effective_end = self.items[index].effective_end_ms();

            match self.items[index].status {
                Status::Playing => {
                    if effective_end <= target {
                        let cancelled = self.items[index]
                            .cancel_at_ms
                            .is_some_and(|cancel| cancel < natural_end);
                        self.items[index].status = if cancelled {
                            Status::Cancelled
                        } else {
                            Status::Completed
                        };
                        self.last_terminal_at = Some(
                            self.last_terminal_at
                                .map_or(effective_end, |previous| previous.max(effective_end)),
                        );
                        self.retire(index, effective_end);
                        continue;
                    }
                }
                Status::Queued => {
                    if self.items[index].cancel_at_ms.is_some_and(|cancel| {
                        cancel < self.items[index].plan.start_at_ms && cancel <= target
                    }) {
                        self.retire(index, target);
                        continue;
                    }

                    if self.items[index].plan.start_at_ms <= target {
                        if effective_end <= target {
                            let cancelled = self.items[index]
                                .cancel_at_ms
                                .is_some_and(|cancel| cancel < natural_end);
                            self.items[index].status = if cancelled {
                                Status::Cancelled
                            } else {
                                Status::Completed
                            };
                            self.last_terminal_at = Some(
                                self.last_terminal_at
                                    .map_or(effective_end, |previous| previous.max(effective_end)),
                            );
                            self.retire(index, effective_end);
                            continue;
                        } else {
                            self.items[index].status = Status::Playing;
                        }
                    }
                }
                Status::Completed | Status::Cancelled => {}
            }

            index += 1;
        }

        self.now_ms = target;
    }

    /// Compatibility wrapper: the plan's requested start is also its arrival time.
    pub fn schedule(&mut self, plan: PlannedPerformance) -> Result<PlannedPerformance, Rejection> {
        let request_at_ms = plan.start_at_ms;
        self.schedule_at(request_at_ms, plan)
    }

    /// Schedule a plan whose source event arrived at request_at_ms.
    pub fn schedule_at(
        &mut self,
        request_at_ms: u64,
        mut plan: PlannedPerformance,
    ) -> Result<PlannedPerformance, Rejection> {
        self.last_rejection = None;
        self.advance_to(request_at_ms);

        plan.start_at_ms = plan.start_at_ms.max(request_at_ms).max(self.now_ms);
        normalize_interrupt_points(&mut plan);

        let cooldown_until = self.cooldowns.get(&plan.asset_id).copied().unwrap_or(0);
        if plan.start_at_ms < cooldown_until {
            self.last_rejection = Some(Rejection::Cooldown);
            return Err(Rejection::Cooldown);
        }

        let incoming_rank = plan.priority.rank();
        let mut candidate_start = plan.start_at_ms;
        let mut handoff_terminal = None;

        // First arbitrate against work that is playing at the event timestamp.
        let active_indices: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.status == Status::Playing)
            .filter(|(_, item)| plans_conflict(&item.plan, &plan))
            .map(|(index, _)| index)
            .collect();

        let mut pending_cancellations = Vec::new();
        for index in active_indices {
            let playing_rank = self.items[index].plan.priority.rank();
            if incoming_rank > playing_rank {
                self.last_rejection = Some(Rejection::Priority);
                return Err(Rejection::Priority);
            }

            let natural_end = self.items[index].plan.end_at_ms();
            let boundary = if self.items[index].plan.interruptible {
                next_safe_boundary(&self.items[index].plan, request_at_ms)
            } else {
                natural_end
            };

            if boundary < natural_end {
                pending_cancellations.push((index, boundary));
            }

            candidate_start = candidate_start.max(boundary);
            handoff_terminal = Some(handoff_terminal.map_or(boundary, |v: u64| v.max(boundary)));
        }

        for (index, boundary) in pending_cancellations {
            let current = self.items[index].cancel_at_ms;
            self.items[index].cancel_at_ms =
                Some(current.map_or(boundary, |value| value.min(boundary)));
        }

        // Preserve the existing minimum-spacing invariant for non-operator handoffs.
        if plan.priority != Priority::Operator {
            if let Some(terminal) = handoff_terminal {
                candidate_start = candidate_start
                    .max(terminal.saturating_add(self.config.min_reaction_spacing_ms));
            } else if let Some(last_terminal) = self.last_terminal_at {
                candidate_start = candidate_start
                    .max(last_terminal.saturating_add(self.config.min_reaction_spacing_ms));
            }
        }

        plan.start_at_ms = candidate_start;

        // Resolve future reservations. A later higher-priority event may supersede
        // lower-priority queued work, but never moves before its own event time.
        loop {
            let mut changed = false;
            let incoming_end = plan.end_at_ms();
            let mut queued_indices: Vec<usize> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.status == Status::Queued)
                .filter(|(_, item)| plans_conflict(&item.plan, &plan))
                .filter(|(_, item)| {
                    intervals_overlap(
                        plan.start_at_ms,
                        incoming_end,
                        item.plan.start_at_ms,
                        item.effective_end_ms(),
                    )
                })
                .map(|(index, _)| index)
                .collect();

            if queued_indices.is_empty() {
                break;
            }

            // cancel_queued retires (swap-removes) slots, which shifts later
            // indices down by one. Process candidates in descending order so
            // each retirement cannot invalidate the indices still pending.
            queued_indices.sort_unstable_by(|a, b| b.cmp(a));
            for index in queued_indices {
                let queued_rank = self.items[index].plan.priority.rank();
                if incoming_rank < queued_rank {
                    self.cancel_queued(index, request_at_ms);
                    changed = true;
                    continue;
                }

                let queued_end = self.items[index].effective_end_ms();
                plan.start_at_ms = plan.start_at_ms.max(queued_end);
                if plan.priority != Priority::Operator {
                    plan.start_at_ms = plan
                        .start_at_ms
                        .saturating_add(self.config.min_reaction_spacing_ms);
                }
                changed = true;
            }

            if !changed {
                break;
            }
        }

        self.generation = self.generation.saturating_add(1);
        plan.generation = self.generation;

        let status = if plan.start_at_ms <= self.now_ms {
            Status::Playing
        } else {
            Status::Queued
        };
        let cooldown_until = plan
            .end_at_ms()
            .saturating_add(self.config.min_reaction_spacing_ms);
        self.cooldowns.insert(plan.asset_id.clone(), cooldown_until);

        let index = self.items.len();
        self.live_by_generation.insert(plan.generation, index);
        self.live_by_event.insert(plan.event_id.clone(), index);
        self.items.push(ScheduledItem {
            plan: plan.clone(),
            status,
            cancel_at_ms: None,
        });

        Ok(plan)
    }

    /// Emergency/operator path: stop active work and cancel queued future
    /// work. Every retirement removes its own liveness-index entries in the
    /// same pass, so no stale generation/event entry survives a stop and no
    /// stale audio/avatar action can remain live afterwards.
    pub fn stop_all(&mut self, at_ms: u64) -> Vec<PlannedPerformance> {
        self.advance_to(at_ms);
        let mut stopped = Vec::new();

        // Retiring swap-removes slots, so re-check the same index after each
        // terminal transition.
        let mut index = 0;
        while index < self.items.len() {
            match self.items[index].status {
                Status::Playing => {
                    let plan = self.items[index].plan.clone();
                    self.items[index].status = Status::Cancelled;
                    self.items[index].cancel_at_ms = Some(at_ms);
                    self.last_terminal_at = Some(
                        self.last_terminal_at
                            .map_or(at_ms, |previous| previous.max(at_ms)),
                    );
                    self.retire(index, at_ms);
                    stopped.push(plan);
                }
                Status::Queued => {
                    let plan = self.items[index].plan.clone();
                    self.cancel_queued(index, at_ms);
                    stopped.push(plan);
                }
                Status::Completed | Status::Cancelled => {
                    // Only advance past slots that were not retired: retire()
                    // swap-removes, so the next item now sits at `index` and
                    // must be re-checked.
                    index += 1;
                }
            }
        }

        stopped
    }

    /// Backward-compatible single-current stop.
    pub fn stop_current(&mut self, at_ms: u64) -> Option<PlannedPerformance> {
        self.advance_to(at_ms);
        let index = self
            .items
            .iter()
            .position(|item| item.status == Status::Playing)?;
        let plan = self.items[index].plan.clone();
        self.items[index].status = Status::Cancelled;
        self.items[index].cancel_at_ms = Some(at_ms);
        self.last_terminal_at = Some(
            self.last_terminal_at
                .map_or(at_ms, |previous| previous.max(at_ms)),
        );
        self.retire(index, at_ms);
        Some(plan)
    }

    /// Finish every remaining reservation at deterministic logical time.
    pub fn complete_all(&mut self) {
        let end = self
            .items
            .iter()
            .filter(|item| item.occupies_timeline())
            .map(ScheduledItem::effective_end_ms)
            .max()
            .unwrap_or(self.now_ms);
        self.advance_to(end);
    }

    /// Cancel one queued reservation and retire it atomically: the slot is
    /// removed, its liveness-index entries are invalidated, and the item
    /// moves to bounded history.
    fn cancel_queued(&mut self, index: usize, at_ms: u64) {
        let planned_cooldown = self.items[index]
            .plan
            .end_at_ms()
            .saturating_add(self.config.min_reaction_spacing_ms);
        let asset_id = self.items[index].plan.asset_id.clone();

        self.items[index].status = Status::Cancelled;
        self.items[index].cancel_at_ms = Some(at_ms);

        if self.cooldowns.get(&asset_id).copied() == Some(planned_cooldown) {
            self.cooldowns.remove(&asset_id);
        }
        self.retire(index, at_ms);
    }
}

fn plans_conflict(a: &PlannedPerformance, b: &PlannedPerformance) -> bool {
    a.exclusive || b.exclusive || !a.channels.is_disjoint(&b.channels)
}

fn intervals_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

fn next_safe_boundary(plan: &PlannedPerformance, request_at_ms: u64) -> u64 {
    let natural_end = plan.end_at_ms();
    let elapsed = request_at_ms.saturating_sub(plan.start_at_ms);

    plan.interrupt_points_ms
        .iter()
        .copied()
        .filter(|offset| *offset >= elapsed)
        .map(|offset| plan.start_at_ms.saturating_add(offset))
        .find(|boundary| *boundary >= request_at_ms && *boundary < natural_end)
        .unwrap_or(natural_end)
}

fn normalize_interrupt_points(plan: &mut PlannedPerformance) {
    plan.interrupt_points_ms
        .retain(|offset| *offset <= plan.duration_ms);
    plan.interrupt_points_ms.sort_unstable();
    plan.interrupt_points_ms.dedup();
}

/// Deterministic mulberry32 PRNG mirroring src/rng.ts.
pub struct SeededRng {
    state: u32,
}

impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed as u32 }
    }

    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b79f5);
        let a = self.state;
        let t1 = (a ^ (a >> 15)).wrapping_mul(1 | a);
        let t2 = t1.wrapping_add((t1 ^ (t1 >> 7)).wrapping_mul(61 | t1)) ^ t1;
        let t3 = t2 ^ (t2 >> 14);
        (t3 as f64) / (u32::MAX as f64 + 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AppliedVariation {
    pub speed_factor: f64,
    pub amplitude_factor: f64,
    pub start_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct VariationSpec {
    pub speed_pct: f64,
    pub amplitude_pct: f64,
    pub start_delay_ms: u64,
}

pub fn apply_variation(spec: VariationSpec, rng: &mut SeededRng) -> AppliedVariation {
    let speed_factor = 1.0 + (rng.next_f64() * 2.0 - 1.0) * (spec.speed_pct / 100.0);
    let amplitude_factor = 1.0 + (rng.next_f64() * 2.0 - 1.0) * (spec.amplitude_pct / 100.0);
    let start_delay_ms = (rng.next_f64() * spec.start_delay_ms as f64).round() as u64;
    AppliedVariation {
        speed_factor,
        amplitude_factor,
        start_delay_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(values: &[BlendChannel]) -> BTreeSet<BlendChannel> {
        values.iter().copied().collect()
    }

    fn plan(
        event_id: &str,
        asset_id: &str,
        priority: Priority,
        start_at_ms: u64,
        duration_ms: u64,
    ) -> PlannedPerformance {
        PlannedPerformance {
            event_id: event_id.to_owned(),
            asset_id: asset_id.to_owned(),
            priority,
            interruptible: true,
            interrupt_points_ms: vec![0, duration_ms],
            start_at_ms,
            duration_ms,
            generation: 0,
            exclusive: false,
            channels: channels(&[BlendChannel::Audio]),
        }
    }

    fn no_spacing() -> Scheduler {
        Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..SchedulerConfig::default()
        })
    }

    #[test]
    fn operator_has_highest_priority() {
        assert_eq!(Priority::Operator.rank(), 0);
        assert!(Priority::Operator.rank() < Priority::HighPriorityInteraction.rank());
    }

    #[test]
    fn mulberry32_matches_typescript_reference() {
        let mut rng = SeededRng::new(42);
        let expected = [
            0.601103751920,
            0.448290558998,
            0.852465793490,
            0.669734041439,
        ];
        for expected_value in expected {
            let actual = rng.next_f64();
            assert!(
                (actual - expected_value).abs() < 1e-12,
                "got {actual:.12}, want {expected_value:.12}"
            );
        }
    }

    #[test]
    fn same_seed_reproduces_identical_variation() {
        let spec = VariationSpec {
            speed_pct: 10.0,
            amplitude_pct: 15.0,
            start_delay_ms: 120,
        };
        assert_eq!(
            apply_variation(spec, &mut SeededRng::new(42)),
            apply_variation(spec, &mut SeededRng::new(42))
        );
    }

    #[test]
    fn cooldown_rejects_repeated_asset_requests() {
        let mut scheduler = Scheduler::new(SchedulerConfig::default());
        scheduler
            .schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600))
            .expect("first");
        let rejected = scheduler.schedule(plan("e2", "asset.a", Priority::Conversation, 1000, 600));
        assert_eq!(rejected.unwrap_err(), Rejection::Cooldown);
    }

    #[test]
    fn future_deferred_item_is_queued_until_logical_start() {
        let mut scheduler = no_spacing();
        let mut current = plan("e1", "asset.a", Priority::Conversation, 0, 1000);
        current.interruptible = false;
        scheduler.schedule(current).expect("current");

        let incoming = scheduler
            .schedule_at(100, plan("e2", "asset.b", Priority::Conversation, 100, 300))
            .expect("deferred");
        assert_eq!(incoming.start_at_ms, 1000);
        assert_eq!(scheduler.items()[0].status, Status::Playing);
        assert_eq!(scheduler.items()[1].status, Status::Queued);

        scheduler.advance_to(999);
        assert_eq!(scheduler.items()[1].status, Status::Queued);
        scheduler.advance_to(1000);
        // e1 reached its terminal state and was retired into bounded history;
        // the live collection keeps only e2 (issue #52).
        let retired = scheduler.history().next().expect("retired history entry");
        assert_eq!(retired.item.status, Status::Completed);
        assert_eq!(retired.item.plan.event_id, "e1");
        assert_eq!(scheduler.items().len(), 1);
        assert_eq!(scheduler.items()[0].plan.event_id, "e2");
        assert_eq!(scheduler.items()[0].status, Status::Playing);
    }

    #[test]
    fn preemption_waits_for_next_safe_interrupt_point() {
        let mut scheduler = no_spacing();
        let mut current = plan("e1", "asset.a", Priority::Conversation, 0, 1000);
        current.interrupt_points_ms = vec![0, 400, 800, 1000];
        scheduler.schedule(current).expect("current");

        let incoming = scheduler
            .schedule_at(
                250,
                plan("e2", "asset.b", Priority::HighPriorityInteraction, 250, 300),
            )
            .expect("queued at safe point");
        assert_eq!(incoming.start_at_ms, 400);
        assert_eq!(scheduler.items()[0].cancel_at_ms, Some(400));
        assert_eq!(scheduler.items()[0].status, Status::Playing);
        assert_eq!(scheduler.items()[1].status, Status::Queued);

        scheduler.advance_to(399);
        assert_eq!(scheduler.items()[0].status, Status::Playing);
        scheduler.advance_to(400);
        // e1 was cancelled at its safe point and retired into bounded history;
        // the live collection keeps only e2 (issue #52).
        let retired = scheduler.history().next().expect("retired history entry");
        assert_eq!(retired.item.status, Status::Cancelled);
        assert_eq!(retired.item.cancel_at_ms, Some(400));
        assert_eq!(scheduler.items().len(), 1);
        assert_eq!(scheduler.items()[0].plan.event_id, "e2");
        assert_eq!(scheduler.items()[0].status, Status::Playing);
    }

    #[test]
    fn disjoint_channels_blend_but_same_channel_uses_priority() {
        let mut scheduler = no_spacing();
        scheduler
            .schedule(plan("e1", "audio.a", Priority::Conversation, 0, 1000))
            .expect("audio");

        let mut face = plan("e2", "face.a", Priority::Background, 100, 500);
        face.channels = channels(&[BlendChannel::Face]);
        scheduler.schedule_at(100, face).expect("face may blend");

        assert_eq!(scheduler.playing_items().count(), 2);

        let same_audio = plan("e3", "audio.b", Priority::Background, 200, 300);
        let rejected = scheduler.schedule_at(200, same_audio);
        assert_eq!(rejected.unwrap_err(), Rejection::Priority);
    }

    #[test]
    fn rejected_multi_conflict_request_leaves_existing_cancellations_untouched() {
        let mut scheduler = no_spacing();

        let mut background = plan("e1", "face.bg", Priority::Background, 0, 1000);
        background.channels = channels(&[BlendChannel::Face]);
        background.interrupt_points_ms = vec![0, 500, 1000];
        scheduler.schedule(background).expect("background");

        let conversation = plan("e2", "audio.conv", Priority::Conversation, 0, 1000);
        scheduler.schedule(conversation).expect("conversation");

        let mut incoming = plan("e3", "exclusive", Priority::Commentary, 100, 200);
        incoming.channels = channels(&[BlendChannel::Overlay]);
        incoming.exclusive = true;

        assert_eq!(
            scheduler.schedule_at(100, incoming).unwrap_err(),
            Rejection::Priority
        );
        assert_eq!(scheduler.items()[0].cancel_at_ms, None);
        assert_eq!(scheduler.items()[1].cancel_at_ms, None);
    }

    #[test]
    fn exclusive_plan_conflicts_even_with_disjoint_channels() {
        let mut scheduler = no_spacing();
        let mut face = plan("e1", "face.a", Priority::Conversation, 0, 1000);
        face.channels = channels(&[BlendChannel::Face]);
        face.interrupt_points_ms = vec![0, 500, 1000];
        scheduler.schedule(face).expect("face");

        let mut exclusive = plan(
            "e2",
            "overlay.a",
            Priority::HighPriorityInteraction,
            100,
            300,
        );
        exclusive.channels = channels(&[BlendChannel::Overlay]);
        exclusive.exclusive = true;

        let scheduled = scheduler.schedule_at(100, exclusive).expect("exclusive");
        assert_eq!(scheduled.start_at_ms, 500);
        assert_eq!(scheduler.items()[0].cancel_at_ms, Some(500));
    }

    #[test]
    fn higher_priority_later_event_can_supersede_future_reservation() {
        let mut scheduler = no_spacing();
        let mut blocker = plan("e1", "blocker", Priority::Conversation, 0, 1000);
        blocker.interruptible = false;
        scheduler.schedule(blocker).expect("blocker");

        let queued = scheduler
            .schedule_at(100, plan("e2", "queued", Priority::Conversation, 100, 500))
            .expect("queued");
        assert_eq!(queued.start_at_ms, 1000);
        assert_eq!(scheduler.items()[1].status, Status::Queued);

        let mut incoming = plan("e3", "urgent", Priority::HighPriorityInteraction, 200, 300);
        incoming.channels = channels(&[BlendChannel::Audio]);
        let urgent = scheduler.schedule_at(200, incoming).expect("urgent");
        assert_eq!(urgent.start_at_ms, 1000);
        // The superseded reservation was cancelled and retired into bounded
        // history; the live collection keeps e1 and the urgent plan (#52).
        let superseded = scheduler.history().next().expect("retired history entry");
        assert_eq!(superseded.item.status, Status::Cancelled);
        assert_eq!(superseded.item.cancel_at_ms, Some(200));
        assert_eq!(superseded.item.plan.event_id, "e2");
        assert_eq!(scheduler.items().len(), 2);
        assert_eq!(scheduler.items()[1].plan.event_id, "e3");
    }

    #[test]
    fn operator_stop_cancels_playing_and_queued_work_at_arrival_time() {
        let mut scheduler = no_spacing();
        let mut current = plan("e1", "current", Priority::Conversation, 0, 1000);
        current.interruptible = false;
        scheduler.schedule(current).expect("current");
        scheduler
            .schedule_at(100, plan("e2", "future", Priority::Conversation, 100, 200))
            .expect("future");

        let stopped = scheduler.stop_all(300);
        assert_eq!(stopped.len(), 2);
        assert!(
            scheduler
                .items()
                .iter()
                .all(|item| item.status == Status::Cancelled)
        );
        assert!(
            scheduler
                .items()
                .iter()
                .all(|item| item.cancel_at_ms == Some(300))
        );
    }

    /// Regression (issue #52): retire() swap-removes slots, so stop_all must
    /// re-check the same index after each retirement. The pre-fix loop
    /// advanced past every retired slot and skipped the item that swapped
    /// into it, leaving it live (audio/avatar could still dispatch) and
    /// undercounting `stopped`.
    #[test]
    fn operator_stop_does_not_skip_items_after_swap_removal() {
        let mut scheduler = no_spacing();
        // Disjoint channels: all three keep playing (no safe-point
        // cancellation interferes) so the swap-removal chain is exercised.
        let mut first = plan("e1", "asset.a", Priority::Conversation, 0, 1000);
        first.channels = channels(&[BlendChannel::Audio]);
        let mut second = plan("e2", "asset.b", Priority::Conversation, 0, 1000);
        second.channels = channels(&[BlendChannel::Face]);
        let mut third = plan("e3", "asset.c", Priority::Conversation, 0, 1000);
        third.channels = channels(&[BlendChannel::Body]);
        scheduler.schedule(first).expect("e1");
        scheduler.schedule(second).expect("e2");
        scheduler.schedule(third).expect("e3");
        assert_eq!(scheduler.items().len(), 3, "three playing pre-stop");

        let stopped = scheduler.stop_all(500);

        assert_eq!(stopped.len(), 3, "every playing item must be stopped");
        assert!(
            scheduler.items().is_empty(),
            "no live item may survive an operator stop"
        );
        assert!(scheduler.live_item_by_event("e1").is_none());
        assert!(scheduler.live_item_by_event("e2").is_none());
        assert!(scheduler.live_item_by_event("e3").is_none());
        assert!(!scheduler.is_generation_live(1));
        assert!(!scheduler.is_generation_live(2));
        assert!(!scheduler.is_generation_live(3));
        let metrics = scheduler.metrics();
        assert_eq!(metrics.active, 0);
        assert_eq!(metrics.terminal_retained, 3);
        assert_eq!(metrics.terminal_evicted, 0);
    }

    /// Regression (issue #52): cancel_queued retires by swap-remove during
    /// supersession. The pre-fix loop collected candidate indices ascending
    /// and invalidated indices still pending in the same pass, so only the
    /// first conflicting reservation was actually cancelled.
    #[test]
    fn multi_supersession_cancels_every_conflicting_lower_priority_reservation() {
        let mut scheduler = no_spacing();
        let mut blocker = plan("e1", "asset.a", Priority::Conversation, 0, 1000);
        blocker.interruptible = false;
        scheduler.schedule(blocker).expect("e1");

        // Two queued reservations behind the non-interruptible blocker.
        scheduler
            .schedule_at(100, plan("e2", "asset.b", Priority::Conversation, 100, 500))
            .expect("e2 queued");
        scheduler
            .schedule_at(120, plan("e3", "asset.c", Priority::Conversation, 120, 500))
            .expect("e3 queued");

        // A long high-priority plan overlapping BOTH queued reservations.
        let urgent = scheduler
            .schedule_at(
                200,
                plan(
                    "e4",
                    "asset.d",
                    Priority::HighPriorityInteraction,
                    200,
                    1200,
                ),
            )
            .expect("e4 urgent");

        // Both superseded reservations must be retired into history.
        let cancelled_events: Vec<&str> = scheduler
            .history()
            .filter(|entry| entry.item.status == Status::Cancelled)
            .map(|entry| entry.item.plan.event_id.as_str())
            .collect();
        assert_eq!(cancelled_events.len(), 2, "both reservations cancelled");
        assert!(cancelled_events.contains(&"e2"));
        assert!(cancelled_events.contains(&"e3"));
        assert_eq!(scheduler.items().len(), 2);
        assert_eq!(scheduler.items()[1].plan.event_id, "e4");
        assert_eq!(urgent.start_at_ms, 1000);
    }

    #[test]
    fn generation_ids_increase_monotonically() {
        let mut scheduler = no_spacing();
        let first = scheduler
            .schedule(plan("e1", "asset.a", Priority::Conversation, 0, 300))
            .expect("first");
        let second = scheduler
            .schedule_at(
                0,
                plan("e2", "asset.b", Priority::HighPriorityInteraction, 0, 300),
            )
            .expect("second");
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert_eq!(scheduler.current_generation(), 2);
    }

    #[test]
    fn minimum_spacing_is_applied_after_safe_handoff() {
        let mut scheduler = Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 1500,
            ..SchedulerConfig::default()
        });
        let mut current = plan("e1", "asset.a", Priority::Conversation, 0, 600);
        current.interrupt_points_ms = vec![0, 100, 600];
        scheduler.schedule(current).expect("current");

        let incoming = scheduler
            .schedule_at(100, plan("e2", "asset.b", Priority::Conversation, 100, 300))
            .expect("incoming");
        assert_eq!(scheduler.items()[0].cancel_at_ms, Some(100));
        assert_eq!(incoming.start_at_ms, 1600);
        assert_eq!(scheduler.items()[1].status, Status::Queued);
    }

    #[test]
    fn rng_values_and_variation_stay_within_declared_bounds() {
        let spec = VariationSpec {
            speed_pct: 10.0,
            amplitude_pct: 15.0,
            start_delay_ms: 120,
        };
        for seed in [0_u64, 1, 7, 12_345, u32::MAX as u64] {
            let mut rng = SeededRng::new(seed);
            for _ in 0..16 {
                let value = rng.next_f64();
                assert!((0.0..1.0).contains(&value));
            }

            let variation = apply_variation(spec, &mut SeededRng::new(seed));
            assert!((0.9..=1.1).contains(&variation.speed_factor));
            assert!((0.85..=1.15).contains(&variation.amplitude_factor));
            assert!(variation.start_delay_ms <= 120);
        }
    }

    #[test]
    fn different_rng_seeds_diverge() {
        let mut first = SeededRng::new(1);
        let mut second = SeededRng::new(2);
        let a: Vec<_> = (0..8).map(|_| first.next_f64()).collect();
        let b: Vec<_> = (0..8).map(|_| second.next_f64()).collect();
        assert_ne!(a, b);
    }
}
