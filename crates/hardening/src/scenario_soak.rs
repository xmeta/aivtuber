//! Issue #70 (Stage 1 closure): drive the #40 failure/soak path from the same
//! versioned scenario the #58 benchmark runs.
//!
//! `run_core_soak` is a synthetic resource soak: it invents its own flat event
//! stream (`content_event(index)`, one kind, 20 events/second). That is exactly
//! the "each suite invents a different unrealistic workload" problem #70 exists
//! to remove. This module consumes a [`StreamScenario`]'s generated trace - the
//! same trace `replay-benchmark --scenario` plays - and overlays a versioned
//! fault plan tied to that scenario's identity, so the soak and the benchmark
//! measure the *same* declared workload.
//!
//! Three properties are load-bearing:
//!
//! 1. **Same identity, not a similar one.** The overlay names the scenario id
//!    *and* version; a mismatch is refused rather than applied to a different
//!    workload.
//! 2. **Only observable faults.** The soak consults the injector at the call
//!    points it actually drives (content ingress, the generative path a semantic
//!    miss would take, and hot-asset insertion). An overlay naming a subsystem
//!    this path never calls is refused, because an injected fault nobody can
//!    observe is decoration.
//! 3. **Degrade, never freeze.** An injected fault skips the failed step and is
//!    counted; the deterministic performer keeps advancing, which is the #40
//!    invariant.

use crate::soak::{generated_fixture, generated_runtime, growth_findings, snapshot};
use crate::{
    FaultPlan, FaultSpec, FaultSubsystem, GrowthFinding, HardeningError, SoakConfig, StateSnapshot,
};
use aivtuber_adaptation::{ActorPseudonymizer, WorkingMemory, WorkingMemoryConfig};
use aivtuber_asset_store::{AssetStore, HotCacheConfig};
use aivtuber_domain::{
    EventEnvelope, ScenarioClass, ScenarioEvent, SecurityPlane, StreamScenario,
    generate_scenario_trace, scenario_instant_ms,
};
use aivtuber_runtime::{ContentAdmitDecision, SecurityRuntime, SecurityRuntimeConfig};
use aivtuber_scheduler::{
    BlendChannel, PlannedPerformance, Scheduler, SchedulerConfig, priority_for_event,
};
use aivtuber_telemetry::{
    BenchmarkSummary, ComparisonMode, EventObservation, ReproducibilityMetadata, RouteClass,
    SecretRedactor, TelemetryCollector, TelemetryRetentionConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Version of the fault-overlay document contract.
pub const FAULT_OVERLAY_SCHEMA_VERSION: &str = "1";

/// Subsystems this soak path actually calls.
///
/// `ContentIngress` at each content event's admission, `Thinking`/`Tts` on the
/// generative path a semantic miss would take, and `AssetStore` at each hot
/// insertion. A fault in any other subsystem could be *planned* but never
/// observed here, so [`FaultOverlay::validate`] refuses it rather than record a
/// degradation that did not happen.
pub const SCENARIO_SOAK_FAULT_SUBSYSTEMS: &[FaultSubsystem] = &[
    FaultSubsystem::ContentIngress,
    FaultSubsystem::AssetStore,
    FaultSubsystem::Thinking,
    FaultSubsystem::Tts,
];

/// A versioned fault plan bound to one scenario identity.
///
/// The overlay is a separate document from the scenario on purpose: the scenario
/// format stays consumer-neutral and free of privileged/provider machinery
/// (`operator.command` and provider failures are refused there), while failures
/// belong to the #40 fault slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultOverlay {
    pub schema_version: String,
    pub scenario_id: String,
    pub scenario_version: u32,
    pub seed: u64,
    pub faults: Vec<FaultSpec>,
}

impl FaultOverlay {
    pub fn validate(&self) -> Result<(), HardeningError> {
        if self.schema_version != FAULT_OVERLAY_SCHEMA_VERSION {
            return Err(HardeningError::InvalidScenario(format!(
                "unsupported fault-overlay schema_version {:?}; this build reads {FAULT_OVERLAY_SCHEMA_VERSION:?}",
                self.schema_version
            )));
        }
        if self.scenario_id.trim().is_empty() {
            return Err(HardeningError::InvalidScenario(
                "fault overlay must name the scenario_id it applies to".to_owned(),
            ));
        }
        if self.scenario_version == 0 {
            return Err(HardeningError::InvalidScenario(
                "fault overlay scenario_version must start at 1".to_owned(),
            ));
        }
        if self.faults.is_empty() {
            return Err(HardeningError::InvalidScenario(format!(
                "fault overlay for {:?} plans no faults, which would silently mean \"run the scenario with no failures\"",
                self.scenario_id
            )));
        }
        for fault in &self.faults {
            if !SCENARIO_SOAK_FAULT_SUBSYSTEMS.contains(&fault.subsystem) {
                return Err(HardeningError::InvalidScenario(format!(
                    "fault overlay names subsystem {:?}, which this soak path never calls; an injected fault nobody can observe is decoration. Observable here: {:?}",
                    fault.subsystem, SCENARIO_SOAK_FAULT_SUBSYSTEMS
                )));
            }
        }
        // Occurrence numbering and duplicate detection are the shared fault-plan
        // rules; reuse them rather than restating them.
        FaultPlan::new(self.seed, self.faults.clone())?;
        Ok(())
    }

    /// Whether this overlay targets `scenario` (id *and* version).
    pub fn matches(&self, scenario: &StreamScenario) -> bool {
        self.scenario_id == scenario.scenario_id
            && self.scenario_version == scenario.scenario_version
    }

    pub fn plan(&self) -> Result<FaultPlan, HardeningError> {
        self.validate()?;
        FaultPlan::new(self.seed, self.faults.clone())
    }

    /// Stable identity of the behaviour this overlay describes.
    ///
    /// Derived from the fault plan, not from `seed`: injection depends only on
    /// the explicit faults, so the comparison baseline must too.
    pub fn plan_id(&self) -> Result<String, HardeningError> {
        Ok(self.plan()?.plan_id())
    }
}

/// What the overlay did to a scenario run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultInjectionReport {
    pub planned: usize,
    pub consumed: Vec<crate::InjectedFault>,
    pub content_ingress_degraded: u64,
    pub generative_degraded: u64,
    pub asset_store_degraded: u64,
}

impl FaultInjectionReport {
    pub fn observed(&self) -> usize {
        self.consumed.len()
    }
}

/// Result of running the #40 soak path over a scenario trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioSoakReport {
    pub metadata: ReproducibilityMetadata,
    pub scenario_id: String,
    pub scenario_version: u32,
    pub scenario_class: ScenarioClass,
    /// Scenario-derived dataset identity (`scenario-<id>-v<version>`).
    pub scenario_dataset_id: String,
    /// Identity of the overlayed fault plan, from a canonical hash of its faults.
    pub fault_plan_id: String,
    /// The overlay's declared seed, recorded for provenance only; it does not
    /// influence injection.
    pub fault_seed: u64,
    /// Comparison identity: scenario dataset id plus fault-plan id, so history
    /// storage never mixes a workload with another workload *or* another plan.
    pub dataset_id: String,
    pub trace_events: u64,
    pub stream_duration_ms: u64,
    /// Logical offset of the first generated event from the scenario's declared
    /// `logical_start`. A leading idle phase makes this greater than zero.
    pub first_event_at_ms: u64,
    /// Logical offset of the last generated event; the declared stream may run
    /// beyond it (a trailing idle phase).
    pub last_event_at_ms: u64,
    pub config: SoakConfig,
    pub midpoint: StateSnapshot,
    pub final_state: StateSnapshot,
    pub telemetry_summary: BenchmarkSummary,
    pub growth: Vec<GrowthFinding>,
    pub faults: FaultInjectionReport,
}

impl ScenarioSoakReport {
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, HardeningError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| HardeningError::Serialization(error.to_string()))
    }

    pub fn finding(&self, metric: &str) -> Option<&GrowthFinding> {
        self.growth.iter().find(|finding| finding.metric == metric)
    }
}

/// Run the deterministic soak over a scenario trace with a fault overlay.
///
/// The trace is generated from the scenario (never read from disk here), so the
/// workload is a pure function of the scenario identity/version/seed. The overlay
/// must target that same identity or the run is refused.
pub fn run_scenario_soak(
    scenario: &StreamScenario,
    overlay: &FaultOverlay,
    config: SoakConfig,
    mut metadata: ReproducibilityMetadata,
) -> Result<ScenarioSoakReport, HardeningError> {
    config.validate()?;
    metadata
        .validate()
        .map_err(|error| HardeningError::InvalidMetadata(error.to_string()))?;
    scenario
        .validate()
        .map_err(|error| HardeningError::InvalidScenario(error.to_string()))?;
    overlay.validate()?;
    if !overlay.matches(scenario) {
        return Err(HardeningError::InvalidScenario(format!(
            "fault overlay targets {:?} v{} but the scenario is {:?} v{}; a fault plan must name the identity it was written for, not a similar workload",
            overlay.scenario_id,
            overlay.scenario_version,
            scenario.scenario_id,
            scenario.scenario_version
        )));
    }

    let trace = generate_scenario_trace(scenario)
        .map_err(|error| HardeningError::InvalidScenario(error.to_string()))?;
    if trace.len() < 2 {
        return Err(HardeningError::InvalidScenario(format!(
            "scenario {:?} generates {} event(s); the soak needs at least two to observe a midpoint before a final state",
            scenario.scenario_id,
            trace.len()
        )));
    }

    // A plan is only meaningful if the workload can actually reach it. Count the
    // call points this trace will make and refuse any occurrence beyond them,
    // rather than reporting a run that silently left a planned fault unfired.
    let reachable = reachable_call_counts(&trace, config.generated_asset_every);
    for fault in &overlay.faults {
        let reachable_occurrences = reachable.get(&fault.subsystem).copied().unwrap_or(0);
        if fault.occurrence > reachable_occurrences {
            return Err(HardeningError::InvalidScenario(format!(
                "fault overlay plans {:?} occurrence {} but scenario {:?} only calls that subsystem {} time(s); a planned fault the workload can never reach would turn a failure test into a false green",
                fault.subsystem, fault.occurrence, scenario.scenario_id, reachable_occurrences
            )));
        }
    }

    // Comparison identity is the workload *and* the fault plan, never the seed.
    let plan = overlay.plan()?;
    let fault_plan_id = plan.plan_id();
    let dataset_id = format!("{}/faults-{fault_plan_id}", scenario.dataset_id());
    metadata.dataset_id = dataset_id.clone();

    let scheduler_config = SchedulerConfig {
        min_reaction_spacing_ms: 0,
        history_capacity: config.scheduler_history_limit,
        history_policy: aivtuber_scheduler::HistoryPolicy::Ring,
    };
    let mut scheduler = Scheduler::new(scheduler_config);
    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_rate_per_second: 1_000_000.0,
            content_burst: 1_000_000,
            content_queue_limit: config.ingress_queue_limit,
            max_audit_records: config.audit_limit,
            ..SecurityRuntimeConfig::default()
        },
        scheduler_config,
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| HardeningError::Runtime(error.to_string()))?;
    let mut memory = WorkingMemory::new(
        WorkingMemoryConfig {
            max_entries: config.working_memory_limit,
            working_ttl_ms: config.logical_duration_ms().saturating_add(1),
            max_compaction_records: config.memory_compaction_limit,
            ..WorkingMemoryConfig::default()
        },
        ActorPseudonymizer::new("scenario-soak-v1", [0x40; 32])
            .map_err(|error| HardeningError::Adaptation(error.to_string()))?,
    )
    .map_err(|error| HardeningError::Adaptation(error.to_string()))?;
    let mut telemetry = TelemetryCollector::with_retention(TelemetryRetentionConfig {
        max_events: config.telemetry_limit,
    });
    let generated_fixture = generated_fixture()?;
    let mut assets = AssetStore::new(PathBuf::from("."), generated_runtime());
    assets.set_hot_cache_config(HotCacheConfig {
        max_dynamic_assets: config.generated_asset_limit,
        max_promotion_metadata: config.promotion_metadata_limit,
        ..HotCacheConfig::default()
    });

    let mut injector = plan.injector();
    // Origin is the scenario's *declared* logical start, not the first event: a
    // leading idle phase is part of the declared stream and must not be
    // normalised away.
    let start_ms = scenario
        .logical_start_ms()
        .map_err(|error| HardeningError::InvalidScenario(error.to_string()))?;
    let midpoint_index = trace.len() / 2;
    let mut first_event_at_ms: Option<u64> = None;
    let mut last_event_at_ms = 0u64;
    let mut midpoint = None;
    let mut faults = FaultInjectionReport {
        planned: overlay.faults.len(),
        consumed: Vec::new(),
        content_ingress_degraded: 0,
        generative_degraded: 0,
        asset_store_degraded: 0,
    };

    for (index, entry) in trace.iter().enumerate() {
        let event = &entry.event;
        let at_ms = scenario_instant_ms(&event.observed_at)
            .map_err(|error| HardeningError::InvalidScenario(error.to_string()))?
            .saturating_sub(start_ms);
        first_event_at_ms.get_or_insert(at_ms);
        last_event_at_ms = at_ms;

        // 1. Content ingress: the runtime admits content-plane events through the
        //    security runtime. A system/control event does not arrive this way, so
        //    it is scheduled directly rather than forced through an ingress it
        //    never uses.
        if event.plane == SecurityPlane::Content {
            match injector.next(FaultSubsystem::ContentIngress) {
                Some(_) => {
                    faults.content_ingress_degraded =
                        faults.content_ingress_degraded.saturating_add(1);
                }
                None => {
                    let raw = serde_json::to_vec(event)
                        .map_err(|error| HardeningError::Serialization(error.to_string()))?;
                    let decision = security
                        .admit_content_bytes(&raw, at_ms)
                        .map_err(|error| HardeningError::Runtime(error.to_string()))?;
                    if decision == ContentAdmitDecision::Queued {
                        let _ = security.pop_content();
                    }
                }
            }
        }

        // 2. Scheduler: the same deterministic timeline the resource soak drives.
        scheduler
            .schedule_at(at_ms, scenario_plan(event, at_ms))
            .map_err(|error| HardeningError::Invariant(format!("scheduler rejected: {error:?}")))?;
        if index % 10 == 0 {
            scheduler.stop_all(at_ms);
        }
        scheduler.advance_to(at_ms.saturating_add(1));

        // 3. Adaptation.
        memory
            .remember_working(event, "scenario soak event", Some("scenario"), at_ms)
            .map_err(|error| HardeningError::Adaptation(error.to_string()))?;

        // 4. Telemetry.
        let mut observation = EventObservation::new(
            event.event_id.clone(),
            ComparisonMode::FullGenerative,
            RouteClass::Deterministic,
        );
        observation.routing_latency_us = 1;
        observation.event_to_first_visible_reaction_ms = Some(0);
        telemetry.record(observation);

        // 5. The generative path a semantic miss would take. A hit resolves to a
        //    cached asset and never reaches a provider; a near-miss depends on the
        //    retriever's ranking, so only a miss is counted as generative here.
        if event
            .payload
            .get("semantic_band")
            .and_then(|value| value.as_str())
            == Some("miss")
        {
            // Consult both independently so a Thinking fault cannot change how
            // many times Tts is called: occurrence counts must depend only on the
            // trace, or reachability would be unprovable before the run.
            let thinking_fault = injector.next(FaultSubsystem::Thinking).is_some();
            let tts_fault = injector.next(FaultSubsystem::Tts).is_some();
            if thinking_fault || tts_fault {
                faults.generative_degraded = faults.generative_degraded.saturating_add(1);
            }
        }

        // 6. Hot-asset churn, which is what exercises the asset cache bounds (#53).
        if (index as u64).is_multiple_of(config.generated_asset_every) {
            match injector.next(FaultSubsystem::AssetStore) {
                Some(_) => {
                    faults.asset_store_degraded = faults.asset_store_degraded.saturating_add(1);
                }
                None => {
                    let mut generated = generated_fixture.clone();
                    generated.id = format!("dynamic.scenario.{index:016x}");
                    let generated_id = generated.id.clone();
                    assets
                        .insert_hot(generated)
                        .map_err(|error| HardeningError::AssetStore(error.to_string()))?;
                    assets.note_hot_use(&generated_id, at_ms);
                }
            }
        }

        if index + 1 == midpoint_index {
            midpoint = Some(snapshot(
                &scheduler, &security, &memory, &telemetry, &assets,
            ));
        }
    }

    // Advance the deterministic timeline to the scenario's declared end. A
    // trailing idle phase still lets logical-time state (scheduler cooldowns,
    // terminal retirement) settle, so the final snapshot reflects the declared
    // stream rather than stopping at the last event.
    scheduler.advance_to(scenario.stream_duration_ms.saturating_add(1));

    let midpoint = midpoint.ok_or_else(|| {
        HardeningError::Invariant("scenario soak midpoint was not captured".to_owned())
    })?;
    let final_state = snapshot(&scheduler, &security, &memory, &telemetry, &assets);
    let telemetry_summary = telemetry.summary(metadata.stream_duration_ms);
    let growth = growth_findings(&config, &midpoint, &final_state);
    faults.consumed = injector.consumed().to_vec();
    // Reachability was checked before the run; this guards the invariant that no
    // planned fault can slip through as a false pass.
    if faults.consumed.len() != faults.planned {
        return Err(HardeningError::Invariant(format!(
            "scenario soak planned {} faults but only {} fired; unreachable plans must be refused, not reported as a clean run",
            faults.planned,
            faults.consumed.len()
        )));
    }

    Ok(ScenarioSoakReport {
        metadata,
        scenario_id: scenario.scenario_id.clone(),
        scenario_version: scenario.scenario_version,
        scenario_class: scenario.scenario_class,
        scenario_dataset_id: scenario.dataset_id(),
        fault_plan_id,
        fault_seed: overlay.seed,
        dataset_id,
        trace_events: trace.len() as u64,
        stream_duration_ms: scenario.stream_duration_ms,
        first_event_at_ms: first_event_at_ms.unwrap_or(0),
        last_event_at_ms,
        config,
        midpoint,
        final_state,
        telemetry_summary,
        growth,
        faults,
    })
}

/// How many times this trace will call each faultable subsystem.
///
/// Content ingress is called once per content-plane event; Thinking and Tts once
/// per semantic miss; the asset store once per generated-asset slot. These are
/// the exact call points the loop below consults, so an overlay occurrence can be
/// checked for reachability before any work happens.
fn reachable_call_counts(
    trace: &[ScenarioEvent],
    generated_asset_every: u64,
) -> BTreeMap<FaultSubsystem, u64> {
    let content = trace
        .iter()
        .filter(|entry| entry.event.plane == SecurityPlane::Content)
        .count() as u64;
    let misses = trace
        .iter()
        .filter(|entry| {
            entry
                .event
                .payload
                .get("semantic_band")
                .and_then(|value| value.as_str())
                == Some("miss")
        })
        .count() as u64;
    let insertions = trace
        .iter()
        .enumerate()
        .filter(|(index, _)| (*index as u64).is_multiple_of(generated_asset_every))
        .count() as u64;
    BTreeMap::from([
        (FaultSubsystem::ContentIngress, content),
        (FaultSubsystem::Thinking, misses),
        (FaultSubsystem::Tts, misses),
        (FaultSubsystem::AssetStore, insertions),
    ])
}

fn scenario_plan(event: &EventEnvelope, at_ms: u64) -> PlannedPerformance {
    PlannedPerformance {
        event_id: event.event_id.clone(),
        asset_id: "scenario.reaction".to_owned(),
        priority: priority_for_event(event.kind),
        interruptible: true,
        interrupt_points_ms: vec![1],
        start_at_ms: at_ms,
        duration_ms: 1,
        generation: 0,
        exclusive: false,
        channels: BTreeSet::from([BlendChannel::Face]),
    }
}
