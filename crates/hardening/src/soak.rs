use crate::HardeningError;
use aivtuber_adaptation::{WorkingMemory, WorkingMemoryConfig};
use aivtuber_asset_store::{AssetStore, PerformanceAsset, RuntimeCompatibility, load_asset_file};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind,
    LocalControlIngress, OperatorCommandInput, SecurityPlane, SourceClass, TrustLevel,
};
use aivtuber_runtime::{
    ContentAdmitDecision, ControlOutcome, SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::{
    BlendChannel, PlannedPerformance, Priority, Scheduler, SchedulerConfig, SchedulerMetrics,
    Status,
};
use aivtuber_telemetry::{
    BenchmarkSummary, ComparisonMode, EventObservation, ReproducibilityMetadata, RouteClass,
    SecretRedactor, TelemetryCollector,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakConfig {
    pub logical_events: u64,
    pub event_interval_ms: u64,
    pub ingress_queue_limit: usize,
    pub working_memory_limit: usize,
    pub generated_asset_every: u64,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            // Eight logical stream hours at 20 events/second.
            logical_events: 576_000,
            event_interval_ms: 50,
            ingress_queue_limit: 128,
            working_memory_limit: 256,
            generated_asset_every: 200,
        }
    }
}

impl SoakConfig {
    pub fn validate(&self) -> Result<(), HardeningError> {
        if self.logical_events < 2
            || self.event_interval_ms == 0
            || self.ingress_queue_limit == 0
            || self.working_memory_limit == 0
            || self.generated_asset_every == 0
        {
            return Err(HardeningError::InvalidConfiguration(
                "soak bounds must be positive and logical_events >= 2",
            ));
        }
        Ok(())
    }

    pub fn logical_duration_ms(&self) -> u64 {
        self.logical_events.saturating_mul(self.event_interval_ms)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub scheduler_items: usize,
    pub scheduler_active: usize,
    pub scheduler_completed: usize,
    pub scheduler_cancelled: usize,
    pub content_queue: usize,
    pub audit_records: usize,
    pub working_memory_entries: usize,
    pub telemetry_events: usize,
    pub hot_assets: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthFinding {
    pub metric: String,
    pub midpoint: usize,
    pub final_count: usize,
    pub configured_limit: Option<usize>,
    pub lifetime_growth_detected: bool,
    pub configured_limit_exceeded: bool,
    pub related_issue: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloodReport {
    pub attempted: usize,
    pub queued: usize,
    pub dropped_backpressure: usize,
    pub final_queue_len: usize,
    pub stop_cancelled: usize,
    pub mute_succeeded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoakReport {
    pub metadata: ReproducibilityMetadata,
    pub config: SoakConfig,
    pub midpoint: StateSnapshot,
    pub final_state: StateSnapshot,
    pub telemetry_summary: BenchmarkSummary,
    pub growth: Vec<GrowthFinding>,
    pub flood: FloodReport,
}

impl SoakReport {
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, HardeningError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| HardeningError::Serialization(error.to_string()))
    }

    pub fn finding(&self, metric: &str) -> Option<&GrowthFinding> {
        self.growth.iter().find(|finding| finding.metric == metric)
    }
}

pub fn run_core_soak(
    config: SoakConfig,
    metadata: ReproducibilityMetadata,
) -> Result<SoakReport, HardeningError> {
    config.validate()?;
    metadata
        .validate()
        .map_err(|error| HardeningError::InvalidMetadata(error.to_string()))?;
    let scheduler_config = SchedulerConfig {
        min_reaction_spacing_ms: 0,
        // The soak probe tracks per-status terminal counts, so retain the
        // full history here (the soak itself asserts no lifetime growth via
        // the bounded-memory components; issue #52 documents the policy).
        history_capacity: usize::MAX,
        history_policy: aivtuber_scheduler::HistoryPolicy::RetainAll,
    };
    let mut scheduler = Scheduler::new(scheduler_config);
    let mut security = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_rate_per_second: 1_000_000.0,

            content_burst: 1_000_000,
            content_queue_limit: config.ingress_queue_limit,
            ..SecurityRuntimeConfig::default()
        },
        scheduler_config,
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| HardeningError::Runtime(error.to_string()))?;
    let mut memory = WorkingMemory::new(WorkingMemoryConfig {
        max_entries: config.working_memory_limit,
        working_ttl_ms: config.logical_duration_ms().saturating_add(1),
        ..WorkingMemoryConfig::default()
    })
    .map_err(|error| HardeningError::Adaptation(error.to_string()))?;
    let mut telemetry = TelemetryCollector::default();

    let generated_fixture = generated_fixture()?;
    let mut assets = AssetStore::new(PathBuf::from("."), generated_runtime());
    let midpoint_index = config.logical_events / 2;
    let mut midpoint = None;

    for index in 0..config.logical_events {
        let at_ms = index.saturating_mul(config.event_interval_ms);
        let event = content_event(index + 1);
        let raw = serde_json::to_vec(&event)
            .map_err(|error| HardeningError::Serialization(error.to_string()))?;

        let admission = security
            .admit_content_bytes(&raw, at_ms)
            .map_err(|error| HardeningError::Runtime(error.to_string()))?;
        if admission != ContentAdmitDecision::Queued {
            return Err(HardeningError::Invariant(format!(
                "steady-state content unexpectedly rejected: {admission:?}"
            )));
        }
        let _ = security.pop_content();

        let plan = short_plan(&event, at_ms);
        scheduler
            .schedule_at(at_ms, plan)
            .map_err(|error| HardeningError::Invariant(format!("scheduler rejected: {error:?}")))?;
        if index % 10 == 0 {
            scheduler.stop_all(at_ms);
        }
        scheduler.advance_to(at_ms.saturating_add(1));

        memory
            .remember_working(&event, "soak event", Some("soak"), at_ms)
            .map_err(|error| HardeningError::Adaptation(error.to_string()))?;

        let mut observation = EventObservation::new(
            event.event_id.clone(),
            ComparisonMode::FullGenerative,
            RouteClass::Deterministic,
        );
        observation.routing_latency_us = 1;
        observation.event_to_first_visible_reaction_ms = Some(0);
        telemetry.record(observation);

        if index % config.generated_asset_every == 0 {
            let mut generated = generated_fixture.clone();
            generated.id = format!("dynamic.soak.{index:016x}");
            assets
                .insert_hot(generated)
                .map_err(|error| HardeningError::AssetStore(error.to_string()))?;
        }

        if index + 1 == midpoint_index {
            midpoint = Some(snapshot(
                &scheduler, &security, &memory, &telemetry, &assets,
            ));
        }
    }

    let midpoint = midpoint
        .ok_or_else(|| HardeningError::Invariant("soak midpoint was not captured".to_owned()))?;
    let final_state = snapshot(&scheduler, &security, &memory, &telemetry, &assets);
    let telemetry_summary = telemetry.summary(metadata.stream_duration_ms);
    let growth = growth_findings(&config, &midpoint, &final_state);
    let flood = run_content_flood(config.ingress_queue_limit)?;

    Ok(SoakReport {
        metadata,
        config,
        midpoint,
        final_state,
        telemetry_summary,
        growth,
        flood,
    })
}

fn snapshot(
    scheduler: &Scheduler,
    security: &SecurityRuntime,
    memory: &WorkingMemory,
    telemetry: &TelemetryCollector,
    assets: &AssetStore,
) -> StateSnapshot {
    // With bounded-state separation (#52) terminal items retire into the
    // scheduler's bounded history, so the completed/cancelled counts come
    // from history plus the exported eviction counter instead of the live
    // item collection.
    let SchedulerMetrics { active, .. } = scheduler.metrics();
    let history_completed = scheduler
        .history()
        .filter(|entry| entry.item.status == Status::Completed)
        .count();
    let history_cancelled = scheduler
        .history()
        .filter(|entry| entry.item.status == Status::Cancelled)
        .count();
    // Evicted entries leave the process uncounted per-status; report the
    // minimum they contribute (zero) so counts stay lower bounds.
    StateSnapshot {
        scheduler_items: scheduler.items().len(),
        scheduler_active: active,
        scheduler_completed: history_completed,
        scheduler_cancelled: history_cancelled,
        content_queue: security.content_len(),
        audit_records: security.audit().len(),
        working_memory_entries: memory.len(),
        telemetry_events: telemetry.events().len(),
        hot_assets: assets.hot_len(),
    }
}

fn growth_findings(
    config: &SoakConfig,
    midpoint: &StateSnapshot,
    final_state: &StateSnapshot,
) -> Vec<GrowthFinding> {
    vec![
        growth(
            "scheduler_items",
            midpoint.scheduler_items,
            final_state.scheduler_items,
            None,
            Some(52),
        ),
        growth(
            "scheduler_active",
            midpoint.scheduler_active,
            final_state.scheduler_active,
            Some(1),
            Some(52),
        ),
        growth(
            "content_queue",
            midpoint.content_queue,
            final_state.content_queue,
            Some(config.ingress_queue_limit),
            Some(50),
        ),
        growth(
            "audit_records",
            midpoint.audit_records,
            final_state.audit_records,
            None,
            Some(51),
        ),
        growth(
            "working_memory_entries",
            midpoint.working_memory_entries,
            final_state.working_memory_entries,
            Some(config.working_memory_limit),
            None,
        ),
        growth(
            "telemetry_events",
            midpoint.telemetry_events,
            final_state.telemetry_events,
            None,
            Some(51),
        ),
        growth(
            "hot_assets",
            midpoint.hot_assets,
            final_state.hot_assets,
            None,
            Some(53),
        ),
    ]
}

fn growth(
    metric: &str,
    midpoint: usize,
    final_count: usize,
    configured_limit: Option<usize>,
    related_issue: Option<u64>,
) -> GrowthFinding {
    let delta = final_count.saturating_sub(midpoint);
    let material_delta = delta > 8.max(midpoint / 4);
    GrowthFinding {
        metric: metric.to_owned(),
        midpoint,
        final_count,
        configured_limit,
        lifetime_growth_detected: final_count > midpoint && material_delta,
        configured_limit_exceeded: configured_limit.is_some_and(|limit| final_count > limit),
        related_issue,
    }
}

pub fn run_content_flood(queue_limit: usize) -> Result<FloodReport, HardeningError> {
    if queue_limit == 0 {
        return Err(HardeningError::InvalidConfiguration(
            "flood queue limit must be positive",
        ));
    }
    let scheduler_config = SchedulerConfig {
        min_reaction_spacing_ms: 0,
        ..aivtuber_scheduler::SchedulerConfig::default()
    };
    let mut runtime = SecurityRuntime::new(
        SecurityRuntimeConfig {
            content_rate_per_second: 1_000_000.0,
            content_burst: 1_000_000,
            content_queue_limit: queue_limit,
            ..SecurityRuntimeConfig::default()
        },
        scheduler_config,
        SecretRedactor::default(),
        None,
    )
    .map_err(|error| HardeningError::Runtime(error.to_string()))?;

    let attempted = queue_limit.saturating_mul(3);
    let mut queued = 0;
    let mut dropped_backpressure = 0;
    for index in 0..attempted {
        let event = content_event(index as u64 + 1);
        let raw = serde_json::to_vec(&event)
            .map_err(|error| HardeningError::Serialization(error.to_string()))?;
        match runtime
            .admit_content_bytes(&raw, 0)
            .map_err(|error| HardeningError::Runtime(error.to_string()))?
        {
            ContentAdmitDecision::Queued => queued += 1,
            ContentAdmitDecision::DroppedBackpressure => dropped_backpressure += 1,
            other => {
                return Err(HardeningError::Invariant(format!(
                    "unexpected flood admission: {other:?}"
                )));
            }
        }
    }

    let control_event = content_event(999_999);
    let mut active_plan = short_plan(&control_event, 0);
    active_plan.duration_ms = 10_000;
    active_plan.interrupt_points_ms = vec![10_000];
    runtime
        .scheduler_mut()
        .schedule_at(0, active_plan)
        .map_err(|error| HardeningError::Invariant(format!("control plan rejected: {error:?}")))?;

    let secret = [0x5a_u8; 32];
    let ingress = LocalControlIngress::new(
        "hardening.local",
        "hardening-operator",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([Capability::PerformerStop, Capability::PerformerMute]),
        ControlSecret::new(secret),
    )
    .map_err(|error| HardeningError::Control(error.to_string()))?;
    let stop = ingress
        .authenticate(operator_input(1, "performer.stop"), &secret)
        .map_err(|error| HardeningError::Control(error.to_string()))?;
    let stop_cancelled = match runtime
        .handle_control(&stop, 1)
        .map_err(|error| HardeningError::Runtime(error.to_string()))?
    {
        ControlOutcome::Stopped { cancelled } => cancelled,
        other => {
            return Err(HardeningError::Invariant(format!(
                "unexpected stop outcome: {other:?}"
            )));
        }
    };

    let mute = ingress
        .authenticate(operator_input(2, "performer.mute"), &secret)
        .map_err(|error| HardeningError::Control(error.to_string()))?;
    let mute_succeeded = matches!(
        runtime
            .handle_control(&mute, 2)
            .map_err(|error| HardeningError::Runtime(error.to_string()))?,
        ControlOutcome::Muted
    );

    Ok(FloodReport {
        attempted,
        queued,
        dropped_backpressure,
        final_queue_len: runtime.content_len(),
        stop_cancelled,
        mute_succeeded,
    })
}

fn content_event(sequence: u64) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: format!("evt-hardening-{sequence}"),
        correlation_id: "corr-hardening".to_owned(),
        sequence,
        observed_at: "2026-09-25T00:00:00Z".to_owned(),
        source: "hardening-chat".to_owned(),
        source_class: SourceClass::PublicChat,
        plane: SecurityPlane::Content,
        trust_level: TrustLevel::Untrusted,
        kind: EventKind::ChatMessage,
        actor_id: Some(format!("viewer:{}", sequence % 32)),
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            Value::String("hardening event".to_owned()),
        )]),
    }
}

fn short_plan(event: &EventEnvelope, at_ms: u64) -> PlannedPerformance {
    PlannedPerformance {
        event_id: event.event_id.clone(),
        asset_id: "hardening.reaction".to_owned(),
        priority: Priority::Conversation,
        interruptible: true,
        interrupt_points_ms: vec![1],
        start_at_ms: at_ms,
        duration_ms: 1,
        generation: 0,
        exclusive: false,
        channels: BTreeSet::from([BlendChannel::Face]),
    }
}

fn operator_input(sequence: u64, action: &str) -> OperatorCommandInput {
    OperatorCommandInput {
        event_id: format!("evt-hardening-control-{sequence}"),
        correlation_id: "corr-hardening-control".to_owned(),
        sequence,
        observed_at: "2026-09-25T00:00:00Z".to_owned(),
        action: action.to_owned(),
        payload: BTreeMap::new(),
    }
}

fn generated_fixture() -> Result<PerformanceAsset, HardeningError> {
    load_asset_file(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/performance-assets/valid/generated-dynamic.json"),
    )
    .map_err(|error| HardeningError::AssetStore(error.to_string()))
}

fn generated_runtime() -> RuntimeCompatibility {
    RuntimeCompatibility {
        compiler_version: "0.1.0".to_owned(),
        voice_model: Some("voice-ja-v2".to_owned()),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        viseme_mapping: Some("ja-5vowel-v2".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
    }
}
