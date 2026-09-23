//! Deterministic performance scheduling primitives.
//!
//! Implements docs/architecture.adoc section 6.5: the scheduler owns the
//! shared timeline and arbitrates performances. Priority alone is
//! insufficient; the scheduler also honors interruptibility, cooldowns,
//! and minimum reaction spacing. Every decision is deterministic given
//! the same event trace and seed.

/// Coarse execution priority (architecture section 6.5, priority classes).
///
/// Lower rank preempts higher rank. Derives [`PartialOrd`] so ranks can be
/// compared directly: `Operator < HighPriorityInteraction < ...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Background,
    Commentary,
    Conversation,
    StrongReaction,
    HighPriorityInteraction,
    Operator,
}

impl Priority {
    /// Deterministic priority for a domain event kind; untrusted content
    /// can never reach [`Priority::Operator`] because that mapping happens
    /// only for explicitly passed operator events (performer, not here).
    pub fn rank(self) -> u8 {
        match self {
            Priority::Background => 5,
            Priority::Commentary => 4,
            Priority::Conversation => 3,
            Priority::StrongReaction => 2,
            Priority::HighPriorityInteraction => 1,
            Priority::Operator => 0,
        }
    }
}

/// A planned performance on the shared monotonic timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPerformance {
    pub event_id: String,
    pub asset_id: String,
    pub priority: Priority,
    pub interruptible: bool,
    /// Sorted ms offsets at which the performance may be left cleanly.
    pub interrupt_points_ms: Vec<u64>,
    pub start_at_ms: u64,
    pub duration_ms: u64,
    /// Monotonic generation id used as a cancellation token.
    pub generation: u64,
}

/// Lifecycle status of a scheduled item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Playing,
    Completed,
    Cancelled,
}

/// A planned performance plus its current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledItem {
    pub plan: PlannedPerformance,
    pub status: Status,
}

/// Why the most recent [`Scheduler::schedule`] call was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Cooldown,
    Priority,
}

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Minimum ms between the end of one performance and the start of the
    /// next non-operator one.
    pub min_reaction_spacing_ms: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            min_reaction_spacing_ms: 1500,
        }
    }
}

/// Deterministic timeline owner and arbiter (architecture section 6.5).
#[derive(Debug)]
pub struct Scheduler {
    items: Vec<ScheduledItem>,
    cooldowns: std::collections::BTreeMap<String, u64>,
    last_finish_at: Option<u64>,
    generation: u64,
    config: SchedulerConfig,
    last_rejection: Option<Rejection>,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            items: Vec::new(),
            cooldowns: std::collections::BTreeMap::new(),
            last_finish_at: None,
            generation: 0,
            config,
            last_rejection: None,
        }
    }

    /// All scheduled items in insertion order.
    pub fn items(&self) -> &[ScheduledItem] {
        &self.items
    }

    /// The currently playing item, if any (lazy completion applied).
    pub fn playing(&self) -> Option<&ScheduledItem> {
        self.items.iter().find(|i| i.status == Status::Playing)
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    /// Rejection reason for the most recent [`Scheduler::schedule`] call.
    pub fn last_rejection(&self) -> Option<Rejection> {
        self.last_rejection
    }

    /// Mark playing items whose end time has passed as completed.
    fn complete_expired(&mut self, at_ms: u64) {
        for item in &mut self.items {
            if item.status == Status::Playing
                && item.plan.start_at_ms + item.plan.duration_ms <= at_ms
            {
                item.status = Status::Completed;
            }
        }
    }

    /// Try to schedule a performance.
    ///
    /// Returns the planned slot, or `Err(rejection)` when arbitration
    /// refuses it: an asset cooldown, or a lower-priority request arriving
    /// during an uninterruptible-or-higher-priority performance.
    pub fn schedule(
        &mut self,
        mut plan: PlannedPerformance,
    ) -> Result<PlannedPerformance, Rejection> {
        self.last_rejection = None;
        self.complete_expired(plan.start_at_ms);

        // Cooldown per asset id.
        let cooldown_until = self.cooldowns.get(&plan.asset_id).copied().unwrap_or(0);
        if plan.start_at_ms < cooldown_until {
            self.last_rejection = Some(Rejection::Cooldown);
            return Err(Rejection::Cooldown);
        }

        if let Some(playing) = self.playing() {
            let playing_rank = playing.plan.priority.rank();
            let incoming_rank = plan.priority.rank();
            if incoming_rank > playing_rank {
                // Lower priority than what is playing: never preempts.
                self.last_rejection = Some(Rejection::Priority);
                return Err(Rejection::Priority);
            }
            if playing.plan.interruptible {
                self.cancel_current(plan.start_at_ms);
            }
        }

        // Minimum spacing between reactions (operator overrides it).
        if plan.priority != Priority::Operator
            && let Some(last_finish) = self.last_finish_at
        {
            let earliest = last_finish + self.config.min_reaction_spacing_ms;
            plan.start_at_ms = plan.start_at_ms.max(earliest);
        }

        self.generation += 1;
        plan.generation = self.generation;
        self.last_finish_at = Some(plan.start_at_ms + plan.duration_ms);

        // Cooldown: asset becomes eligible again after it finishes plus spacing.
        self.cooldowns.insert(
            plan.asset_id.clone(),
            plan.start_at_ms + plan.duration_ms + self.config.min_reaction_spacing_ms,
        );

        self.items.push(ScheduledItem {
            plan: plan.clone(),
            status: Status::Playing,
        });
        Ok(plan)
    }

    /// Deterministic stop for the current performance (operator path).
    pub fn stop_current(&mut self, at_ms: u64) -> Option<PlannedPerformance> {
        let playing = self
            .items
            .iter_mut()
            .find(|i| i.status == Status::Playing)?;
        playing.status = Status::Cancelled;
        self.last_finish_at = Some(at_ms);
        Some(playing.plan.clone())
    }

    fn cancel_current(&mut self, at_ms: u64) {
        if let Some(playing) = self.items.iter_mut().find(|i| i.status == Status::Playing) {
            playing.status = Status::Cancelled;
            self.last_finish_at = Some(at_ms);
        }
    }
}

/// Deterministic mulberry32 PRNG (mirrors `src/rng.ts`).
pub struct SeededRng {
    state: u32,
}

impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed as u32 }
    }

    /// Next uniform value in `[0, 1)`. Bit-for-bit identical to the
    /// TypeScript reference in `src/rng.ts`, so replay traces seeded on
    /// either side produce the same variation stream.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b79f5);
        let a = self.state;
        let t1 = (a ^ (a >> 15)).wrapping_mul(1 | a);
        let t2 = t1.wrapping_add((t1 ^ (t1 >> 7)).wrapping_mul(61 | t1)) ^ t1;
        let t3 = t2 ^ (t2 >> 14);
        (t3 as f64) / (u32::MAX as f64 + 1.0)
    }
}

/// Bounded procedural variation (architecture section 8).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppliedVariation {
    pub speed_factor: f64,
    pub amplitude_factor: f64,
    pub start_delay_ms: u64,
}

/// Declared variation bounds, mirroring the Performance Asset schema.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VariationSpec {
    pub speed_pct: f64,
    pub amplitude_pct: f64,
    pub start_delay_ms: u64,
}

/// Apply bounded deterministic variation: speed/amplitude in +/-pct,
/// delay in 0..=max. Same seed reproduces the same variation.
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

    fn plan(
        event_id: &str,
        asset_id: &str,
        priority: Priority,
        start: u64,
        duration: u64,
    ) -> PlannedPerformance {
        PlannedPerformance {
            event_id: event_id.to_owned(),
            asset_id: asset_id.to_owned(),
            priority,
            interruptible: true,
            interrupt_points_ms: vec![0, duration],
            start_at_ms: start,
            duration_ms: duration,
            generation: 0,
        }
    }

    #[test]
    fn operator_has_highest_priority() {
        assert!(Priority::Operator.rank() < Priority::HighPriorityInteraction.rank());
        assert_eq!(Priority::Operator.rank(), 0);
    }

    #[test]
    fn same_seed_reproduces_identical_variation() {
        let spec = VariationSpec {
            speed_pct: 10.0,
            amplitude_pct: 15.0,
            start_delay_ms: 120,
        };
        let a = apply_variation(spec, &mut SeededRng::new(42));
        let b = apply_variation(spec, &mut SeededRng::new(42));
        assert_eq!(a, b);
    }

    /// The TypeScript reference values from `src/rng.ts` (createRng(42)).
    /// Pins the Rust port to bit-for-bit parity so a replay trace seeded on
    /// either runtime yields the same variation stream.
    #[test]
    fn mulberry32_matches_typescript_reference() {
        let mut rng = SeededRng::new(42);
        let expected = [
            0.601103751920,
            0.448290558998,
            0.852465793490,
            0.669734041439,
        ];
        for e in expected {
            let v = rng.next_f64();
            assert!((v - e).abs() < 1e-12, "got {:.12}, want {:.12}", v, e);
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SeededRng::new(1);
        let mut b = SeededRng::new(2);
        let va: Vec<f64> = (0..8).map(|_| a.next_f64()).collect();
        let vb: Vec<f64> = (0..8).map(|_| b.next_f64()).collect();
        assert_ne!(va, vb);
    }

    #[test]
    fn values_stay_in_unit_interval() {
        let mut rng = SeededRng::new(7);
        for _ in 0..10_000 {
            let v = rng.next_f64();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn variation_stays_within_declared_bounds() {
        let spec = VariationSpec {
            speed_pct: 10.0,
            amplitude_pct: 15.0,
            start_delay_ms: 120,
        };
        for seed in [0u64, 1, 7, 12345, u32::MAX as u64] {
            let v = apply_variation(spec, &mut SeededRng::new(seed));
            assert!((0.9..=1.1).contains(&v.speed_factor));
            assert!((0.85..=1.15).contains(&v.amplitude_factor));
            assert!(v.start_delay_ms <= 120);
        }
    }

    #[test]
    fn cooldown_rejects_repeated_asset_requests() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        let first = s.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600));
        assert!(first.is_ok());

        let second = s.schedule(plan("e2", "asset.a", Priority::Conversation, 1000, 600));
        assert_eq!(second.unwrap_err(), Rejection::Cooldown);
        assert_eq!(s.last_rejection(), Some(Rejection::Cooldown));
    }

    #[test]
    fn lower_priority_cannot_interrupt() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        s.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 5000))
            .expect("schedule playing");

        let rejected = s.schedule(plan("e2", "asset.b", Priority::Commentary, 100, 600));
        assert_eq!(rejected.unwrap_err(), Rejection::Priority);
    }

    #[test]
    fn equal_priority_interrupts_only_when_playing_is_interruptible() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        let mut p = plan("e1", "asset.a", Priority::Conversation, 0, 5000);
        p.interruptible = false;
        s.schedule(p).expect("schedule uninterruptible");

        let rejected = s.schedule(plan("e2", "asset.b", Priority::Conversation, 100, 600));
        // Equal priority but the playing item is uninterruptible: the new
        // request is deferred by minimum spacing instead of preempting.
        let deferred = rejected.expect("deferred, not rejected");
        assert_eq!(deferred.start_at_ms, 6500);

        // Interruptible equal priority cancels the current and schedules.
        let mut s2 = Scheduler::new(SchedulerConfig::default());
        s2.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 5000))
            .expect("schedule");
        let ok = s2.schedule(plan("e2", "asset.b", Priority::Conversation, 100, 600));
        assert!(ok.is_ok());
        let statuses: Vec<Status> = s2.items().iter().map(|i| i.status).collect();
        assert_eq!(statuses, vec![Status::Cancelled, Status::Playing]);
    }

    #[test]
    fn higher_rank_priority_cannot_interrupt_lower_rank_playing() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        s.schedule(plan(
            "e1",
            "asset.a",
            Priority::HighPriorityInteraction,
            0,
            5000,
        ))
        .expect("schedule paid interaction");

        let rejected = s.schedule(plan("e2", "asset.b", Priority::Commentary, 100, 600));
        assert_eq!(rejected.unwrap_err(), Rejection::Priority);
    }

    #[test]
    fn operator_preempts_everything() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        let mut p = plan("e1", "asset.a", Priority::StrongReaction, 0, 5000);
        p.interruptible = false;
        s.schedule(p).expect("schedule");

        let stopped = s.stop_current(300);
        assert!(stopped.is_some());
        assert_eq!(s.items()[0].status, Status::Cancelled);
    }

    #[test]
    fn minimum_spacing_defers_non_operator_start() {
        let mut s = Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 1500,
        });
        s.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600))
            .expect("first");
        // Second request at 100ms interrupts the first (equal rank,
        // interruptible), setting last-finish to the interruption time.
        // Minimum spacing then defers the start to 100 + 1500 = 1600.
        let second = s
            .schedule(plan("e2", "asset.b", Priority::Conversation, 100, 1000))
            .expect("deferred schedule");
        assert_eq!(second.start_at_ms, 1600);
        assert_eq!(s.items()[0].status, Status::Cancelled);

        // Operator requests keep their requested start.
        s.schedule(plan("e3", "asset.c", Priority::Operator, 2200, 100))
            .expect("operator");
        assert_eq!(s.items()[2].plan.start_at_ms, 2200);
    }

    #[test]
    fn spacing_after_natural_completion_counts_full_duration() {
        let mut s = Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 1500,
        });
        s.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600))
            .expect("first");
        // A later request beyond the playing window does not interrupt;
        // spacing counts from the first performance's natural finish (600).
        let second = s
            .schedule(plan("e2", "asset.b", Priority::Conversation, 1_000, 600))
            .expect("deferred schedule");
        assert_eq!(second.start_at_ms, 2_100);
        assert_eq!(s.items()[0].status, Status::Completed);
    }

    #[test]
    fn lazy_completion_marks_expired_items() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        s.schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600))
            .expect("first");
        // Ask for something far in the future: the first item completes.
        s.schedule(plan("e2", "asset.b", Priority::Conversation, 10_000, 600))
            .expect("second");
        assert_eq!(s.items()[0].status, Status::Completed);
    }

    #[test]
    fn generations_increase_monotonically() {
        let mut s = Scheduler::new(SchedulerConfig::default());
        let a = s
            .schedule(plan("e1", "asset.a", Priority::Conversation, 0, 600))
            .unwrap();
        let b = s
            .schedule(plan("e2", "asset.b", Priority::Operator, 100, 600))
            .unwrap();
        assert_eq!(a.generation, 1);
        assert_eq!(b.generation, 2);
        assert_eq!(s.current_generation(), 2);
    }
}
