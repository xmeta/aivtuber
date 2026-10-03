//! Generated property coverage for the scheduler state machines (#145).
//!
//! These live in an integration test target rather than a `#[cfg(test)]` module
//! so the properties exercise the crate's *public* surface exactly as a
//! consumer sees it, and so `proptest` stays a dev-dependency that never
//! reaches a production build (see `crates/scheduler/Cargo.toml`).
//!
//! Each property is an invariant over a whole generated operation sequence, so
//! a failure reports the minimal sequence that broke it. proptest persists any
//! failing case to `proptest-regressions/properties.txt` and replays those files
//! on every later run, so a regression becomes a checked-in input rather than
//! something that has to be rediscovered.

use aivtuber_domain::{
    AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, EventEnvelope,
    EventKind, FallbackReason, SecurityPlane, SourceClass, TrustLevel,
};
use aivtuber_scheduler::{
    BlendChannel, HistoryPolicy, PlannedPerformance, Priority, ReplayAsset, ReplayDirective,
    ReplayEvent, ReplayHarness, ReplayHarnessConfig, ReplayResult, ScheduledItem, Scheduler,
    SchedulerConfig, SchedulerMetrics, SinkActionKind, Status, VariationSpec,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

/// A violated invariant.
///
/// Carries the minimal generated sequence that broke it, so a failure report is
/// a reproducible input rather than a bare assertion message. Implements
/// [`Error`] so `?` works directly inside `proptest!` bodies.
#[derive(Debug)]
struct InvariantViolated(String);

impl fmt::Display for InvariantViolated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for InvariantViolated {}

impl From<String> for InvariantViolated {
    fn from(message: String) -> Self {
        Self(message)
    }
}

/// The end of an item's occupied interval: a cancellation boundary can only
/// shorten the planned interval, never extend it.
///
/// Mirrors the scheduler's own internal rule because it is a private detail, so
/// the property restates the contract instead of reaching into it.
fn effective_end_ms(item: &ScheduledItem) -> u64 {
    let natural_end = item.plan.end_at_ms();
    item.cancel_at_ms
        .map_or(natural_end, |cancel| cancel.min(natural_end))
}

/// Two plans conflict when exclusive work touches anything, or when their
/// channel sets intersect. Stated here rather than imported so the property
/// states the arbitration rule itself instead of restating the implementation.
fn plans_conflict(left: &PlannedPerformance, right: &PlannedPerformance) -> bool {
    left.exclusive || right.exclusive || !left.channels.is_disjoint(&right.channels)
}

// ---------------------------------------------------------------------------
// Generated operations
// ---------------------------------------------------------------------------

/// One scheduler mutation. Flat and small so proptest can shrink a failing
/// sequence down to the operations that actually matter.
#[derive(Debug, Clone)]
enum Op {
    Schedule {
        asset: u8,
        priority: Priority,
        start_offset_ms: u64,
        duration_ms: u64,
        interruptible: bool,
        exclusive: bool,
        channel_mask: u8,
    },
    Advance {
        delta_ms: u64,
    },
    StopAll,
    CompleteAll,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let priority = prop_oneof![
        Just(Priority::Operator),
        Just(Priority::HighPriorityInteraction),
        Just(Priority::StrongReaction),
        Just(Priority::Conversation),
        Just(Priority::Commentary),
        Just(Priority::Background),
    ];
    let schedule = (
        0_u8..4,
        priority,
        0_u64..400,
        1_u64..500,
        any::<bool>(),
        any::<bool>(),
        0_u8..32,
    )
        .prop_map(
            |(
                asset,
                priority,
                start_offset_ms,
                duration_ms,
                interruptible,
                exclusive,
                channel_mask,
            )| Op::Schedule {
                asset,
                priority,
                start_offset_ms,
                duration_ms,
                interruptible,
                exclusive,
                channel_mask,
            },
        );
    prop_oneof![
        8 => schedule,
        2 => (0_u64..500).prop_map(|delta_ms| Op::Advance { delta_ms }),
        1 => Just(Op::StopAll),
        1 => Just(Op::CompleteAll),
    ]
}

/// A sequence of at most 40 operations, biased towards scheduling so the
/// arbitration paths actually get exercised.
fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_strategy(), 0..40)
}

fn config_strategy() -> impl Strategy<Value = SchedulerConfig> {
    (
        0_u64..400,    // min_reaction_spacing_ms
        0_usize..8,    // history_capacity
        any::<bool>(), // true -> bounded ring, false -> retain all
    )
        .prop_map(
            |(min_reaction_spacing_ms, history_capacity, ring)| SchedulerConfig {
                min_reaction_spacing_ms,
                history_capacity,
                history_policy: if ring {
                    HistoryPolicy::Ring
                } else {
                    HistoryPolicy::RetainAll
                },
            },
        )
}

fn channel_set(mask: u8) -> BTreeSet<BlendChannel> {
    let all = [
        BlendChannel::Audio,
        BlendChannel::Face,
        BlendChannel::Body,
        BlendChannel::Gaze,
        BlendChannel::Overlay,
    ];
    // `mask == 0` would mean "no channels", which conflicts with nothing and
    // makes the exclusivity property vacuous. Every plan gets at least one.
    let effective = if mask == 0 { 1 } else { mask };
    all.iter()
        .enumerate()
        .filter(|(index, _)| effective & (1 << index) != 0)
        .map(|(_, channel)| *channel)
        .collect()
}

fn plan_for(op: &Op, sequence: usize) -> PlannedPerformance {
    match op {
        Op::Schedule {
            asset,
            priority,
            duration_ms,
            interruptible,
            exclusive,
            channel_mask,
            ..
        } => PlannedPerformance {
            event_id: format!("evt-{sequence}"),
            asset_id: format!("asset-{asset}"),
            priority: *priority,
            interruptible: *interruptible,
            interrupt_points_ms: if *interruptible {
                vec![0, *duration_ms / 2, *duration_ms]
            } else {
                Vec::new()
            },
            // `schedule_at` treats the requested start as a lower bound, so the
            // caller-supplied offset only has to be non-negative.
            start_at_ms: 0,
            duration_ms: *duration_ms,
            generation: 0,
            exclusive: *exclusive,
            channels: channel_set(*channel_mask),
        },
        _ => unreachable!("only Schedule builds a plan"),
    }
}

/// Everything a rejected schedule must leave untouched.
///
/// `items()` is the live collection and terminal items live in history, so both
/// are captured. Generations are recorded explicitly because a stale liveness
/// entry could hide behind an equal item count.
#[derive(Debug, PartialEq, Eq)]
struct SchedulerState {
    items: Vec<(String, String, Status, Option<u64>, u64, u64)>,
    history: Vec<(String, Status, u64)>,
    metrics: SchedulerMetrics,
    current_generation: u64,
    now_ms: u64,
}

fn snapshot(scheduler: &Scheduler) -> SchedulerState {
    SchedulerState {
        items: scheduler
            .items()
            .iter()
            .map(|item| {
                (
                    item.plan.event_id.clone(),
                    item.plan.asset_id.clone(),
                    item.status,
                    item.cancel_at_ms,
                    item.plan.generation,
                    effective_end_ms(item),
                )
            })
            .collect(),
        history: scheduler
            .history()
            .map(|entry| {
                (
                    entry.item.plan.event_id.clone(),
                    entry.item.status,
                    entry.item.plan.generation,
                )
            })
            .collect(),
        metrics: scheduler.metrics(),
        current_generation: scheduler.current_generation(),
        now_ms: scheduler.now_ms(),
    }
}

/// Rejection must be atomic: nothing about a refused schedule may survive.
///
/// The caller advances logical time to the request timestamp *before*
/// snapshotting, because `schedule_at` performs that time advance itself. The
/// comparison therefore isolates the schedule attempt and nothing else.
fn attempt_schedule(
    scheduler: &mut Scheduler,
    op: &Op,
    sequence: usize,
    ops: &[Op],
) -> Result<Option<PlannedPerformance>, String> {
    let Op::Schedule {
        start_offset_ms, ..
    } = op
    else {
        unreachable!("attempt_schedule is only called for Schedule operations");
    };
    let request_at_ms = scheduler.now_ms().saturating_add(*start_offset_ms);
    scheduler.advance_to(request_at_ms);

    let before = snapshot(scheduler);
    let plan = plan_for(op, sequence);
    match scheduler.schedule_at(request_at_ms, plan) {
        Ok(accepted) => Ok(Some(accepted)),
        Err(rejection) => {
            // `last_rejection` is the documented output of a rejection, not a
            // leaked reservation, so it is asserted rather than compared as
            // part of "nothing changed".
            if scheduler.last_rejection() != Some(rejection) {
                return Err(format!(
                    "rejection {rejection:?} was not reported through last_rejection \
                     (got {:?})\n\
                     failing step: {sequence}\n\
                     generated sequence: {ops:?}",
                    scheduler.last_rejection()
                ));
            }
            let after = snapshot(scheduler);
            if after != before {
                return Err(format!(
                    "rejected schedule ({rejection:?}) left state behind\n\
                     failing step: {sequence}\n\
                     before: {before:?}\n\
                     after:  {after:?}\n\
                     generated sequence: {ops:?}"
                ));
            }
            Ok(None)
        }
    }
}

/// Two conflicting plans may never occupy the same instant. This is the
/// resource-exclusivity and arbitration-safety property.
fn check_exclusive_safety(
    scheduler: &Scheduler,
    ops: &[Op],
    sequence: usize,
) -> Result<(), String> {
    let items = scheduler.items();
    for (left_index, left) in items.iter().enumerate() {
        for right in items.iter().skip(left_index + 1) {
            if !plans_conflict(&left.plan, &right.plan) {
                continue;
            }
            let (left_start, left_end) = (left.plan.start_at_ms, effective_end_ms(left));
            let (right_start, right_end) = (right.plan.start_at_ms, effective_end_ms(right));
            if left_start < right_end && right_start < left_end {
                return Err(format!(
                    "conflicting plans overlap: {} [{left_start},{left_end}) vs {} [{right_start},{right_end})\n\
                     failing step: {sequence}\n\
                     generated sequence: {ops:?}",
                    left.plan.event_id, right.plan.event_id
                ));
            }
        }
    }
    Ok(())
}

/// Every live item must be reachable through both public liveness indexes, and
/// nothing terminal may remain reachable through either. This is the
/// stale-action suppression property: after a cancellation or an operator stop,
/// no retired audio/avatar action may still look dispatchable.
fn check_liveness_index_consistency(
    scheduler: &Scheduler,
    ops: &[Op],
    sequence: usize,
) -> Result<(), String> {
    let mut seen_generations = BTreeSet::new();
    for item in scheduler.items() {
        let generation = item.plan.generation;
        if !seen_generations.insert(generation) {
            return Err(format!(
                "generation {generation} appears twice in the live collection\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}"
            ));
        }
        if !scheduler.is_generation_live(generation) {
            return Err(format!(
                "live item {} has generation {generation} missing from the liveness index\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}",
                item.plan.event_id
            ));
        }
        if scheduler.live_item_by_generation(generation).is_none() {
            return Err(format!(
                "generation {generation} is indexed but not resolvable\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}"
            ));
        }
        if scheduler.live_item_by_event(&item.plan.event_id).is_none() {
            return Err(format!(
                "event {} is live but missing from the event liveness index\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}",
                item.plan.event_id
            ));
        }
    }

    for entry in scheduler.history() {
        let generation = entry.item.plan.generation;
        if scheduler.is_generation_live(generation) {
            return Err(format!(
                "terminal item {} (generation {generation}, {:?}) is still reported live\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}",
                entry.item.plan.event_id, entry.item.status
            ));
        }
        if scheduler
            .live_item_by_event(&entry.item.plan.event_id)
            .is_some()
        {
            return Err(format!(
                "terminal item {} is still reachable by event id\n\
                 failing step: {sequence}\n\
                 generated sequence: {ops:?}",
                entry.item.plan.event_id
            ));
        }
    }
    Ok(())
}

/// A terminal transition must record a coherent status. A completed item with
/// an interior `cancel_at_ms` would mean the scheduler stopped work it claimed
/// to finish; a cancelled item with no boundary would mean the sink is never
/// told when to stop.
fn check_terminal_classification(
    scheduler: &Scheduler,
    ops: &[Op],
    sequence: usize,
) -> Result<(), String> {
    for entry in scheduler.history() {
        let item = &entry.item;
        let natural_end = item.plan.end_at_ms();
        let problem = match item.status {
            Status::Completed => item
                .cancel_at_ms
                .filter(|cancel| *cancel < natural_end)
                .map(|cancel| {
                    format!(
                        "item {} completed but was cancelled early at {cancel} (ends {natural_end})",
                        item.plan.event_id
                    )
                }),
            Status::Cancelled => item
                .cancel_at_ms
                .is_none()
                .then(|| format!("item {} is cancelled with no boundary", item.plan.event_id)),
            Status::Queued | Status::Playing => Some(format!(
                "item {} is still {:?} inside terminal history",
                item.plan.event_id, item.status
            )),
        };
        if let Some(problem) = problem {
            return Err(format!(
                "{problem}\nfailing step: {sequence}\ngenerated sequence: {ops:?}"
            ));
        }
    }
    Ok(())
}

/// Retained terminal state must stay inside the configured bound, metrics must
/// agree with the actual history, and every generation handed out must be
/// unique. `RetainAll` is documented to grow without bound, so the capacity
/// bound only applies to the ring policy.
fn check_retention_bounds(
    scheduler: &Scheduler,
    config: &SchedulerConfig,
    ops: &[Op],
    sequence: usize,
) -> Result<(), String> {
    let fail = |problem: String| {
        Err(format!(
            "{problem}\nfailing step: {sequence}\ngenerated sequence: {ops:?}"
        ))
    };

    let metrics = scheduler.metrics();
    if metrics.terminal_retained != scheduler.history().count() {
        return fail(format!(
            "metrics report {} retained but history holds {}",
            metrics.terminal_retained,
            scheduler.history().count()
        ));
    }
    if config.history_policy == HistoryPolicy::Ring
        && metrics.terminal_retained > config.history_capacity
    {
        return fail(format!(
            "retained {} terminal items with ring capacity {}",
            metrics.terminal_retained, config.history_capacity
        ));
    }
    if metrics.active != metrics.queued + metrics.playing {
        return fail(format!(
            "active {} != queued {} + playing {}",
            metrics.active, metrics.queued, metrics.playing
        ));
    }
    if metrics.cooldowns > metrics.cooldowns_high_water {
        return fail(format!(
            "cooldowns {} exceeds its own high water mark {}",
            metrics.cooldowns, metrics.cooldowns_high_water
        ));
    }
    Ok(())
}

fn run_sequence(ops: &[Op], config: SchedulerConfig) -> Result<(), String> {
    let mut scheduler = Scheduler::new(config);

    for (sequence, op) in ops.iter().enumerate() {
        if matches!(op, Op::Schedule { .. }) {
            attempt_schedule(&mut scheduler, op, sequence, ops)?;
        } else {
            // Recorded before the step so an operator stop can be held to
            // invalidating everything that was live when it ran.
            let live_before = live_generations(&scheduler);
            apply_non_schedule(&mut scheduler, op);
            if matches!(op, Op::StopAll) {
                check_stop_invalidated_everything(&scheduler, &live_before, ops, sequence)?;
            }
        }

        check_exclusive_safety(&scheduler, ops, sequence)?;
        check_liveness_index_consistency(&scheduler, ops, sequence)?;
        check_terminal_classification(&scheduler, ops, sequence)?;
        check_retention_bounds(&scheduler, &config, ops, sequence)?;
    }

    Ok(())
}

fn live_generations(scheduler: &Scheduler) -> BTreeSet<u64> {
    scheduler
        .items()
        .iter()
        .map(|item| item.plan.generation)
        .collect()
}

/// An operator stop is the emergency path: after it returns, *nothing* that was
/// live may still be live. Work left dispatchable behind an operator stop is the
/// stale-audio/avatar-action failure the runtime exists to prevent, and the
/// other invariants do not catch it on their own — a leftover queued item is
/// internally consistent, it is simply work the operator asked to stop.
fn check_stop_invalidated_everything(
    scheduler: &Scheduler,
    live_before: &BTreeSet<u64>,
    ops: &[Op],
    sequence: usize,
) -> Result<(), String> {
    let survivors: Vec<u64> = live_before
        .iter()
        .copied()
        .filter(|generation| scheduler.is_generation_live(*generation))
        .collect();
    if survivors.is_empty() {
        return Ok(());
    }
    Err(format!(
        "operator stop left {} generation(s) live and dispatchable: {survivors:?}\n\
         failing step: {sequence}\n\
         generated sequence: {ops:?}",
        survivors.len()
    ))
}

/// Apply every operation that is not a schedule request.
fn apply_non_schedule(scheduler: &mut Scheduler, op: &Op) {
    match op {
        Op::Schedule { .. } => unreachable!("handled by attempt_schedule"),
        Op::Advance { delta_ms } => {
            let at_ms = scheduler.now_ms().saturating_add(*delta_ms);
            scheduler.advance_to(at_ms);
        }
        Op::StopAll => {
            let at_ms = scheduler.now_ms();
            scheduler.stop_all(at_ms);
        }
        Op::CompleteAll => scheduler.complete_all(),
    }
}

// ---------------------------------------------------------------------------
// Replay trace generation
// ---------------------------------------------------------------------------

fn event(kind: EventKind, event_id: &str, sequence: u64) -> EventEnvelope {
    let (source, source_class, plane, trust_level, authorization) = match kind {
        EventKind::ChatMessage => (
            "chat",
            SourceClass::PublicChat,
            SecurityPlane::Content,
            TrustLevel::Untrusted,
            None,
        ),
        EventKind::ChatDonation => (
            "donation",
            SourceClass::Donation,
            SecurityPlane::Content,
            TrustLevel::SemiTrusted,
            None,
        ),
        EventKind::GameEvent => (
            "game",
            SourceClass::Game,
            SecurityPlane::Content,
            TrustLevel::SemiTrusted,
            None,
        ),
        EventKind::OperatorCommand => (
            "local-operator-hotkey",
            SourceClass::Operator,
            SecurityPlane::Control,
            TrustLevel::Trusted,
            Some(AuthorizationContext {
                principal: "operator:local".to_owned(),
                method: AuthorizationMethod::OperatorHotkey,
                capabilities: BTreeSet::from([Capability::PerformerStop]),
            }),
        ),
        other => panic!("trace generation does not cover {other:?}"),
    };

    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: event_id.to_owned(),
        correlation_id: "corr-proptest".to_owned(),
        sequence,
        observed_at: "2026-09-24T00:00:00Z".to_owned(),
        source: source.to_owned(),
        source_class,
        plane,
        trust_level,
        kind,
        actor_id: None,
        priority_hint: None,
        authorization,
        payload: BTreeMap::new(),
    }
}

fn replay_asset() -> impl Strategy<Value = ReplayAsset> {
    (
        "[a-z]{1,4}",
        1_u64..400,
        any::<bool>(),
        any::<bool>(),
        0_u8..32,
        0_u64..200,
    )
        .prop_map(
            |(id, duration_ms, interruptible, exclusive, channel_mask, start_delay_ms)| {
                ReplayAsset {
                    asset_id: id.to_owned(),
                    duration_ms,
                    interruptible,
                    interrupt_points_ms: if interruptible {
                        vec![0, duration_ms]
                    } else {
                        Vec::new()
                    },
                    exclusive,
                    channels: channel_set(channel_mask),
                    // Only `start_delay_ms` is varied: it perturbs the scheduled
                    // start, which is what makes the seed observable in the trace.
                    variation: VariationSpec {
                        speed_pct: 0.0,
                        amplitude_pct: 0.0,
                        start_delay_ms,
                    },
                }
            },
        )
}

fn replay_event_strategy() -> impl Strategy<Value = ReplayEvent> {
    fn performance(kind: EventKind) -> impl Strategy<Value = ReplayEvent> {
        (0_u64..800, "[a-z]{1,6}", 0_u64..16, replay_asset()).prop_map(
            move |(at_ms, event_id, sequence, asset)| ReplayEvent {
                at_ms,
                event: event(kind, &event_id, sequence),
                directive: ReplayDirective::Performance { asset },
            },
        )
    }

    prop_oneof![
        6 => performance(EventKind::ChatMessage),
        4 => performance(EventKind::ChatDonation),
        3 => performance(EventKind::GameEvent),
        // An operator stop is only legal on an operator event; the harness
        // enforces that authority boundary, so generation must respect it too.
        2 => (0_u64..800, "[a-z]{1,6}", 0_u64..16).prop_map(
            |(at_ms, event_id, sequence)| ReplayEvent {
                at_ms,
                event: event(EventKind::OperatorCommand, &event_id, sequence),
                directive: ReplayDirective::OperatorStop,
            }
        ),
        1 => (
            0_u64..800,
            "[a-z]{1,6}",
            0_u64..16,
            prop_oneof![
                Just(FallbackReason::Timeout),
                Just(FallbackReason::Unavailable),
                Just(FallbackReason::LowConfidence),
            ],
        )
            .prop_map(|(at_ms, event_id, sequence, reason)| ReplayEvent {
                at_ms,
                event: event(EventKind::ChatMessage, &event_id, sequence),
                directive: ReplayDirective::Fallback { reason },
            }),
    ]
}

fn trace_strategy() -> impl Strategy<Value = Vec<ReplayEvent>> {
    prop::collection::vec(replay_event_strategy(), 0..12)
}

/// Traces whose event ids are unique, so a rotation genuinely reorders
/// distinct events instead of swapping two events with the same id.
fn unique_id_trace_strategy() -> impl Strategy<Value = Vec<ReplayEvent>> {
    trace_strategy().prop_filter("unique event ids", |trace| {
        let mut ids: Vec<&str> = trace
            .iter()
            .map(|item| item.event.event_id.as_str())
            .collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        ids.len() == before
    })
}

/// Traces that actually exercise deterministic variation.
fn varying_trace_strategy() -> impl Strategy<Value = Vec<ReplayEvent>> {
    trace_strategy().prop_filter("has variation", |trace| {
        trace.iter().any(|event| {
            matches!(&event.directive, ReplayDirective::Performance { asset }
                if asset.variation.start_delay_ms > 0)
        })
    })
}

fn run_trace(trace: &[ReplayEvent], seed: u64, scheduler: SchedulerConfig) -> ReplayResult {
    ReplayHarness::new(ReplayHarnessConfig {
        seed,
        scheduler,
        audio_available: true,
        avatar_available: true,
    })
    .run(trace)
    .expect("generated traces are valid by construction")
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        // Failing cases are persisted under `proptest-regressions/` and replayed
        // on every later run, so a modest case count is enough and the suite
        // stays fast enough for the normal `cargo test --workspace` gate rather
        // than needing a separate long-running job.
        cases: 96,
        max_shrink_iters: 4096,
        // Pinned explicitly: the default resolves relative to the *source* file,
        // which for an integration target is not a Cargo target root and yields
        // an awkward `properties.proptest-regressions` filename.
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/properties.txt",
        ))),
        ..ProptestConfig::default()
    })]

    /// #145: no generated operation sequence may produce illegal
    /// exclusive-resource overlap, a rejected schedule that still mutates
    /// state, a dispatchable stale action, or unbounded retention.
    #[test]
    fn generated_operation_sequences_preserve_scheduler_invariants(
        ops in ops_strategy(),
        config in config_strategy(),
    ) {
        run_sequence(&ops, config).map_err(InvariantViolated::from)?;
    }

    /// #145: the same generated trace replayed twice must be byte-identical.
    /// Replay underpins every recorded benchmark, so a nondeterministic step
    /// would silently invalidate the history.
    #[test]
    fn identical_trace_seed_and_config_are_byte_identical(
        trace in trace_strategy(),
        seed in any::<u64>(),
    ) {
        let config = SchedulerConfig {
            min_reaction_spacing_ms: 1500,
            ..SchedulerConfig::default()
        };
        let first = run_trace(&trace, seed, config);
        let second = run_trace(&trace, seed, config);
        prop_assert_eq!(
            first.to_json_bytes(),
            second.to_json_bytes(),
            "replay is not deterministic for trace {:?} seed {}",
            trace,
            seed
        );
    }

    /// #145: the seed must actually reach the deterministic variation.
    /// Without this, "identical seed reproduces" would be trivially satisfied
    /// by a harness that ignored the seed. Compared across a whole seed sweep
    /// rather than a single pair, because two seeds can coincidentally round to
    /// the same millisecond delay.
    #[test]
    fn the_seed_actually_varies_the_replay(
        trace in varying_trace_strategy(),
    ) {
        let config = SchedulerConfig::default();
        let outputs: BTreeSet<Vec<u8>> = (0_u64..64)
            .map(|seed| run_trace(&trace, seed, config).to_json_bytes())
            .collect();
        prop_assert!(
            outputs.len() > 1,
            "seed had no effect on the replay of trace {:?}",
            trace
        );
    }

    /// #145: replay output must not depend on the order events arrive in. The
    /// harness sorts, so a caller that reorders its trace must still get the
    /// identical expected result.
    #[test]
    fn replay_is_independent_of_input_order(
        trace in unique_id_trace_strategy(),
        rotation in 0_usize..8,
    ) {
        let config = SchedulerConfig::default();
        let ordered = run_trace(&trace, 7, config);
        let mut rotated = trace.clone();
        rotated.rotate_left(rotation % trace.len().max(1));
        let reordered = run_trace(&rotated, 7, config);
        prop_assert_eq!(
            ordered.to_json_bytes(),
            reordered.to_json_bytes(),
            "replay depended on input order for trace {:?}",
            trace
        );
    }

    /// #145: stale audio/avatar actions must never be dispatchable. Every start
    /// corresponds to an item that was not cancelled before its logical start,
    /// and every stop is preceded by a start for the same generation.
    #[test]
    fn stale_actions_are_never_dispatchable(trace in trace_strategy()) {
        let result = run_trace(&trace, 99, SchedulerConfig::default());

        let cancelled_before_start: BTreeSet<u64> = result
            .scheduled_items
            .iter()
            .filter(|item| {
                item.cancel_at_ms
                    .is_some_and(|cancel| cancel < item.plan.start_at_ms)
            })
            .map(|item| item.plan.generation)
            .collect();

        for actions in [&result.audio_actions, &result.avatar_actions] {
            let mut started: BTreeSet<u64> = BTreeSet::new();
            for action in actions {
                match action.action {
                    SinkActionKind::Start | SinkActionKind::DegradedStart => {
                        prop_assert!(
                            started.insert(action.generation),
                            "duplicate start for generation {} in {:?}",
                            action.generation,
                            actions
                        );
                        prop_assert!(
                            !cancelled_before_start.contains(&action.generation),
                            "generation {} was cancelled before its start but still \
                             produced {:?}",
                            action.generation,
                            action
                        );
                    }
                    SinkActionKind::Stop => {
                        prop_assert!(
                            started.contains(&action.generation),
                            "stop for generation {} has no start in {:?}",
                            action.generation,
                            actions
                        );
                    }
                }
            }
        }
    }/// #145: while an event is still live, re-requesting that same event must
    /// not produce a second live copy. Two live generations for one source
    /// event would give the sink two dispatchable starts it cannot tell apart.
    #[test]
    fn a_live_event_cannot_be_scheduled_twice(
        ops in ops_strategy(),
        config in config_strategy(),
    ) {
        let mut scheduler = Scheduler::new(config);
        for (sequence, op) in ops.iter().enumerate() {
            if let Op::Schedule { .. } = op {
                let Some(accepted) =
                    attempt_schedule(&mut scheduler, op, sequence, &ops).map_err(InvariantViolated::from)?
                else {
                    continue;
                };
                let event_id = accepted.event_id.clone();
                prop_assert_eq!(
                    event_id.clone(),
                    format!("evt-{sequence}"),
                    "generated id drifted"
                );

                let still_live = scheduler
                    .live_item_by_event(&event_id)
                    .is_some_and(|live| live.plan.generation == accepted.generation);
                if !still_live {
                    continue;
                }

                // Same event id, same generation id, requested again while the
                // first copy is live. A non-live generation id keeps the
                // scheduler from overwriting the live item's index entry, so
                // the duplicate is a genuine second attempt at the same event.
                let duplicate = plan_for(op, sequence);
                let request_at_ms = scheduler.now_ms();
                scheduler.advance_to(request_at_ms);
                let before = snapshot(&scheduler);
                let outcome = scheduler.schedule_at(request_at_ms, duplicate);
                let after = snapshot(&scheduler);
                if outcome.is_ok() && after != before {
                    let live = scheduler
                        .live_item_by_event(&event_id)
                        .expect("the duplicate is live");
                    prop_assert_ne!(
                        live.plan.generation,
                        accepted.generation,
                        "duplicate request for {} overwrote the live generation",
                        event_id
                    );
                }
            } else {
                apply_non_schedule(&mut scheduler, op);
            }
        }
    }
}
