use crate::HardeningError;
use aivtuber_adaptation::{ActorPseudonymizer, WorkingMemory, WorkingMemoryConfig};
use aivtuber_asset_store::{
    AssetStore, HotCacheConfig, PerformanceAsset, RuntimeCompatibility, load_asset_file,
};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind,
    LocalControlIngress, OperatorCommandInput, SecurityPlane, SourceClass, TrustLevel,
};
use aivtuber_runtime::{
    ContentAdmitDecision, ControlOutcome, SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::{
    BlendChannel, PlannedPerformance, Priority, Scheduler, SchedulerConfig, Status,
};
use aivtuber_telemetry::{
    BenchmarkConfiguration, BenchmarkEnvironment, BenchmarkGit, BenchmarkResult, BenchmarkSummary,
    ComparisonMode, EventObservation, InvariantValue, MetricValue, RESULT_SCHEMA_VERSION,
    ReproducibilityMetadata, RouteClass, SecretRedactor, TelemetryCollector,
    TelemetryRetentionConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Suite identity emitted by the high-volume resource benchmark (issue #58
/// Phase D). History storage keys trend series on (suite, mode, dataset).
pub const RESOURCE_BENCH_SUITE: &str = "resource-soak";
/// Dataset identity of the logical-time soak workload; changing the workload
/// shape materially requires changing this id so history series never mix.
/// v2: the timed loop gained the #105 memory/link operations (permit-gated
/// durable writes, supersession resolution, link creation and pruning), so
/// pre-link v1 measurements must never pool with the new workload.
pub const RESOURCE_BENCH_DATASET: &str = "hardening-soak-v2";
/// Result `mode` for resource-soak runs (schemas/benchmark-result.schema.json).
pub const RESOURCE_BENCH_MODE: &str = "resource_soak";
/// Config identity prefix; the CLI appends the workload parameters it ran so
/// recorded history remains reproducible. Bumped alongside
/// [`RESOURCE_BENCH_DATASET`] for the same #105 workload-shape change.
pub const RESOURCE_BENCH_CONFIG_VERSION: &str = "resource-bench-v2";

/// Wall-clock context for a soak run, measured around `run_core_soak`.
/// The soak itself uses logical time and stays deterministic; wall clock is
/// reported as throughput context only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBenchTiming {
    pub wall_clock_ms: u64,
    /// Peak resident set size in KiB when the platform exposes it
    /// (Linux `/proc/self/status` VmHWM); `None` elsewhere. Diagnostic only:
    /// hosted-runner RSS is too noisy for strict gating (issue #58 noise
    /// calibration), so this never becomes a hard invariant.
    pub peak_rss_kib: Option<u64>,
}

impl ResourceBenchTiming {
    /// Linux-only peak-RSS read without unsafe code or new dependencies.
    pub fn peak_rss_kib_from_proc() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let value = rest.split_whitespace().next()?;
                return value.parse().ok();
            }
        }
        None
    }
}

/// Build the `schemas/benchmark-result.schema.json` result for a completed
/// soak. Retained-state sizes become metrics with explicit `_count` units;
/// plateau/limit findings from the existing mid-vs-final comparison become
/// hard invariants so the #58 comparator gates them exactly.
pub fn resource_bench_result(
    report: &SoakReport,
    timing: ResourceBenchTiming,
    environment: BenchmarkEnvironment,
) -> Result<BenchmarkResult, HardeningError> {
    let events = report.config.logical_events;
    let seconds = timing.wall_clock_ms as f64 / 1_000.0;
    let throughput = if seconds > 0.0 {
        events as f64 / seconds
    } else {
        0.0
    };

    let metric = |value: usize| MetricValue {
        value: value as f64,
        sample_count: Some(events),
        run_range: None,
    };
    let mut metrics = BTreeMap::from([
        (
            "resource.throughput_events_per_s".to_owned(),
            MetricValue {
                value: (throughput * 100.0).round() / 100.0,
                sample_count: Some(events),
                run_range: None,
            },
        ),
        (
            "resource.scheduler_history_retained_count".to_owned(),
            metric(report.final_state.scheduler_history),
        ),
        (
            "resource.audit_records_retained_count".to_owned(),
            metric(report.final_state.audit_records),
        ),
        (
            "resource.rate_limit_sources_count".to_owned(),
            metric(report.final_state.rate_limit_sources),
        ),
        (
            "resource.working_memory_entries_count".to_owned(),
            metric(report.final_state.working_memory_entries),
        ),
        (
            "resource.memory_compaction_records_count".to_owned(),
            metric(report.final_state.memory_compaction_records),
        ),
        (
            "resource.telemetry_events_retained_count".to_owned(),
            metric(report.final_state.telemetry_events),
        ),
        (
            "resource.hot_assets_resident_count".to_owned(),
            metric(report.final_state.hot_assets),
        ),
        (
            "resource.promotion_metadata_retained_count".to_owned(),
            metric(report.final_state.promotion_metadata),
        ),
        (
            "resource.content_queue_final_count".to_owned(),
            metric(report.final_state.content_queue),
        ),
    ]);
    if let Some(peak_rss_kib) = timing.peak_rss_kib {
        metrics.insert(
            "resource.peak_rss_kib".to_owned(),
            MetricValue {
                value: peak_rss_kib as f64,
                sample_count: Some(1),
                run_range: None,
            },
        );
    }

    // Hard invariants: limit violations (the existing failure-soak gate) and
    // non-plateauing retained state both fail the comparator regardless of
    // any timing improvement (issue #58 hard-invariant principle).
    let limit_violations = report
        .growth
        .iter()
        .filter(|finding| finding.configured_limit_exceeded)
        .count();
    let non_plateau = report
        .growth
        .iter()
        .filter(|finding| finding.lifetime_growth_detected)
        .count();
    let invariant = |value: u64, detail: Option<String>| InvariantValue { value, detail };
    let invariants = BTreeMap::from([
        (
            "resource.retention_bound_violation_count".to_owned(),
            invariant(
                limit_violations as u64,
                (limit_violations > 0).then(|| {
                    growth_detail(&report.growth, |finding| finding.configured_limit_exceeded)
                }),
            ),
        ),
        (
            "resource.non_plateau_state_count".to_owned(),
            invariant(
                non_plateau as u64,
                (non_plateau > 0).then(|| {
                    growth_detail(&report.growth, |finding| finding.lifetime_growth_detected)
                }),
            ),
        ),
    ]);

    Ok(BenchmarkResult {
        schema_version: RESULT_SCHEMA_VERSION.to_owned(),
        benchmark_suite: RESOURCE_BENCH_SUITE.to_owned(),
        mode: RESOURCE_BENCH_MODE.to_owned(),
        dataset_id: Some(RESOURCE_BENCH_DATASET.to_owned()),
        recording: None,
        git: BenchmarkGit {
            commit: report.metadata.git_commit.clone(),
            base_commit: None,
        },
        environment,
        configuration: BenchmarkConfiguration {
            config_version: format!(
                "{};events={};interval_ms={}",
                RESOURCE_BENCH_CONFIG_VERSION, events, report.config.event_interval_ms
            ),
            runtime_profile: Some("full".to_owned()),
            asset_version: report.metadata.asset_version.clone(),
            index_version: report.metadata.index_version.clone(),
            retriever_version: None,
            jev_model: report.metadata.jev_model.clone(),
            thinking_model: report.metadata.thinking_model.clone(),
            tts_model: report.metadata.tts_model.clone(),
            cost_model_version: None,
            seed: report.metadata.seed,
            stream_duration_ms: report.metadata.stream_duration_ms,
        },
        metrics,
        invariants,
    })
}

/// Compact violation listing for invariant details, e.g.
/// `scheduler_history(final=600>limit=512)`. Empty when nothing matched.
fn growth_detail(findings: &[GrowthFinding], predicate: impl Fn(&GrowthFinding) -> bool) -> String {
    let parts: Vec<String> = findings
        .iter()
        .filter(|finding| predicate(finding))
        .map(|finding| match finding.configured_limit {
            Some(limit) => format!(
                "{}(final={} > limit={})",
                finding.metric, finding.final_count, limit
            ),
            None => format!(
                "{}(midpoint={} final={})",
                finding.metric, finding.midpoint, finding.final_count
            ),
        })
        .collect();
    parts.join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoakConfig {
    pub logical_events: u64,
    pub event_interval_ms: u64,
    pub ingress_queue_limit: usize,
    pub scheduler_history_limit: usize,
    pub audit_limit: usize,
    pub telemetry_limit: usize,
    pub working_memory_limit: usize,
    pub memory_compaction_limit: usize,
    /// Store-wide edge bound the retained link graph may reach, mirroring
    /// `WorkingMemoryConfig::max_links` for the probe (production default is
    /// 2,048). Watched like every other retention bound so a regression that
    /// lets the link store grow — or stops pruning it — is visible here.
    pub memory_links_limit: usize,
    pub generated_asset_limit: usize,
    pub promotion_metadata_limit: usize,
    pub generated_asset_every: u64,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            // Eight logical stream hours at 20 events/second.
            logical_events: 576_000,
            event_interval_ms: 50,
            ingress_queue_limit: 128,
            // Probe limits are intentionally smaller than production defaults
            // so the 5k-event CI smoke reaches a retention plateau before its midpoint.
            scheduler_history_limit: 512,
            audit_limit: 1_024,
            telemetry_limit: 1_024,
            working_memory_limit: 256,
            memory_compaction_limit: 128,
            memory_links_limit: 128,
            generated_asset_limit: 8,
            promotion_metadata_limit: 8,
            generated_asset_every: 200,
        }
    }
}

impl SoakConfig {
    pub fn validate(&self) -> Result<(), HardeningError> {
        if self.logical_events < 2
            || self.event_interval_ms == 0
            || self.ingress_queue_limit == 0
            || self.scheduler_history_limit == 0
            || self.audit_limit == 0
            || self.telemetry_limit == 0
            || self.working_memory_limit == 0
            || self.memory_compaction_limit == 0
            || self.memory_links_limit == 0
            || self.generated_asset_limit == 0
            || self.promotion_metadata_limit == 0
            || self.generated_asset_every == 0
        {
            return Err(HardeningError::InvalidConfiguration(
                "soak bounds must be positive and logical_events >= 2",
            ));
        }
        // A link store that cannot hold one maximum-size write is refused by
        // `WorkingMemory::new` — which would fail the experiment at
        // construction rather than report an invalid configuration here, so
        // the same rule the adapter enforces is checked up front.
        if self.memory_links_limit < WorkingMemoryConfig::default().max_links_per_memory {
            return Err(HardeningError::InvalidConfiguration(
                "soak memory_links_limit must cover one maximum-size write: it has to be at \
                 least the per-write link budget (WorkingMemoryConfig::max_links_per_memory), or \
                 the probe cannot perform a single durable supersession write",
            ));
        }
        Ok(())
    }

    /// Core-soak link-probe rules, on top of the common bounds in
    /// [`SoakConfig::validate`].
    ///
    /// The scenario soak deliberately performs only working-memory writes
    /// and reports `growth_findings(..., false)`, so it calls `validate`
    /// alone: a one- or two-entry window is a valid bounded scenario
    /// experiment, not a broken link probe (#105 review round 6). The core
    /// soak, by contrast, must be a *runnable* link experiment — these are
    /// the rules that keep it from aborting mid-run or reporting link checks
    /// it never exercised.
    pub fn validate_link_probe(&self) -> Result<(), HardeningError> {
        self.validate()?;
        // A configuration accepted as a link-store probe must be able to
        // retain a link. Every durable slot also writes one working entry,
        // so the supersession target is only still retained at link creation
        // when the window holds all three nodes: the working entry, the
        // target, and the new durable entry. With fewer node slots the
        // target is always the oldest entry the insertion evicts, the new
        // edge is pruned before it counts, and the run would report zero
        // links while claiming to probe the store (#105 review round 3).
        if self.working_memory_limit < 3 {
            return Err(HardeningError::InvalidConfiguration(
                "soak working_memory_limit must leave room to retain a link: the window has \
                 to hold the working entry, the supersession target, and the new durable entry \
                 (at least 3 node slots), or the probe cannot exercise the link store it \
                 reports on",
            ));
        }
        // The probe mints its first supersession edge only on the *second*
        // durable write. A run no longer than one cadence gap performs just
        // the initial durable write, creates no edge at all, and would still
        // report clean `memory_links = 0` checks — a bounded-state check
        // that cannot fail (#105 review round 6). Refuse the configuration
        // instead of emitting a false green.
        if self.logical_events <= self.durable_every() {
            return Err(HardeningError::InvalidConfiguration(
                "soak logical_events must reach the second durable write: the probe needs \
                 more than one durable_every of events before it can mint its first \
                 supersession link, or the report would claim link checks it never \
                 exercised",
            ));
        }
        // An accepted configuration must also *complete*: explicit
        // supersession evidence is never shed, so a workload whose retained
        // chain outgrows `memory_links_limit` deterministically aborts
        // mid-run with an invalid-adaptation error. `durable_every` slows
        // the cadence down to fit the link budget, and this check pins that
        // projection — if a future cadence change breaks it, the config is
        // refused up front rather than failing halfway through the
        // experiment (#105 review round 6).
        let chain = self.max_retained_supersession_chain();
        if chain > self.memory_links_limit {
            return Err(HardeningError::InvalidConfiguration(
                "soak memory_links_limit cannot hold this workload's retained supersession \
                 chain at its configured cadence and window: an accepted configuration must \
                 run to completion, never exhaust explicit evidence mid-run",
            ));
        }
        Ok(())
    }

    /// Cadence of the core soak's permit-gated durable supersession write:
    /// one durable claim every this-many events, the first at event 0.
    ///
    /// Two constraints shape it (#105 review round 6):
    ///
    /// * window/16 pacing keeps the supersession target comfortably inside
    ///   its TTL and the retained chain a small fraction of the window, and
    /// * link-budget pacing keeps the *retained explicit chain* below
    ///   `memory_links_limit`. Edges are born every `durable_every` events
    ///   and pruned only when FIFO retention evicts their older endpoint,
    ///   so the chain converges to `ceil(window / (durable_every + 1)) - 1`
    ///   concurrent edges; requiring `durable_every + 1 >= ceil(window /
    ///   (links + 1))` holds it to the configured bound. Without that, a
    ///   small link store over a large window would exhaust explicit
    ///   evidence and abort mid-run.
    pub fn durable_every(&self) -> u64 {
        let by_window = (self.working_memory_limit / 16) as u64;
        // ceil(window / (links + 1)) - 1, without a division-by-links+1
        // subtlety: ceil(a / b) == (a + b - 1) / b.
        let by_links = ((self.working_memory_limit + self.memory_links_limit)
            / (self.memory_links_limit + 1))
            .saturating_sub(1) as u64;
        by_window.max(by_links).max(1)
    }

    /// Upper bound on explicit supersession edges this configuration's core
    /// workload can hold in the store at once: edges are born once per
    /// durable cadence and pruned when retention evicts their older
    /// endpoint, so the count saturates at `ceil(window / (cadence + 1)) - 1`
    /// and is additionally capped by how many edges the run can ever mint.
    /// The exact maximum, used by `validate_link_probe` to refuse
    /// configurations that would abort mid-run (#105 review round 6).
    pub fn max_retained_supersession_chain(&self) -> usize {
        let durable_every = self.durable_every();
        let window = self.working_memory_limit as u64;
        let by_window = (window + durable_every) / (durable_every + 1);
        let by_run = self.logical_events.saturating_sub(1) / durable_every;
        by_window.saturating_sub(1).min(by_run) as usize
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
    pub scheduler_history: usize,
    pub scheduler_cooldowns: usize,
    pub content_queue: usize,
    pub audit_records: usize,
    pub rate_limit_sources: usize,
    pub working_memory_entries: usize,
    pub memory_compaction_records: usize,
    pub memory_links: usize,
    pub memory_links_high_water: usize,
    pub telemetry_events: usize,
    pub hot_assets: usize,
    pub promotion_metadata: usize,
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
    config.validate_link_probe()?;
    metadata
        .validate()
        .map_err(|error| HardeningError::InvalidMetadata(error.to_string()))?;
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
            // Durable targets must outlive the run: a supersession target
            // that expired mid-soak would refuse the write instead of
            // exercising the link store.
            durable_ttl_ms: config.logical_duration_ms().saturating_add(1),
            max_compaction_records: config.memory_compaction_limit,
            max_links: config.memory_links_limit,
            ..WorkingMemoryConfig::default()
        },
        ActorPseudonymizer::new("hardening-test-v1", [0x24; 32])
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
    let midpoint_index = config.logical_events / 2;
    let mut midpoint = None;
    // The edge store is bounded state too, so the soak has to populate it:
    // a system-source durable claim every `durable_every` node slot
    // supersedes the previous one through the same permit gate production
    // uses, creating supersession edges plus the deterministic same-topic
    // temporal spine — often enough that the store plateaus before the
    // midpoint, which is what the growth findings watch. `durable_every`
    // derives from both the window and the link budget, so the retained
    // explicit chain fits `memory_links_limit` instead of exhausting it
    // mid-run, and `SoakConfig::validate_link_probe` refuses windows too
    // small to retain a link, runs too short to mint one, and cadences whose
    // projected chain outgrows the budget — so an accepted run really
    // exercises the link store it reports on (#105 review round 6).
    let durable_every = config.durable_every();
    let supersede_targets = config.working_memory_limit >= 2;
    let mut previous_durable: Option<String> = None;

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

        if index % durable_every == 0 {
            let durable = system_event(index + 1);
            let (_, permit) = security.authorize_memory_write(&durable, None);
            let permit = permit.ok_or_else(|| {
                HardeningError::Runtime("durable soak write denied by security gate".to_owned())
            })?;
            let supersedes: Vec<&str> = if supersede_targets {
                previous_durable.as_deref().into_iter().collect()
            } else {
                Vec::new()
            };
            memory
                .remember_durable_superseding(
                    &permit,
                    "soak durable claim",
                    Some("soak-durable"),
                    at_ms,
                    &supersedes,
                )
                .map_err(|error| HardeningError::Adaptation(error.to_string()))?;
            previous_durable = Some(durable.event_id.clone());
        }

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
            let generated_id = generated.id.clone();
            assets
                .insert_hot(generated)
                .map_err(|error| HardeningError::AssetStore(error.to_string()))?;
            assets.note_hot_use(&generated_id, at_ms);
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
    // `validate_link_probe` guaranteed this run reaches its second durable
    // write, so the link checks below always describe link activity that
    // actually happened — `true` is a promise the configuration already
    // kept, never a hope (#105 review round 6).
    let growth = growth_findings(&config, &midpoint, &final_state, true);
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

pub(crate) fn snapshot(
    scheduler: &Scheduler,
    security: &SecurityRuntime,
    memory: &WorkingMemory,
    telemetry: &TelemetryCollector,
    assets: &AssetStore,
) -> StateSnapshot {
    // With bounded-state separation (#52) terminal items retire into the
    // scheduler's bounded history. Per-status counts below cover only retained
    // history; exact retained-state sizes come from subsystem metrics.
    let scheduler_metrics = scheduler.metrics();
    let history_completed = scheduler
        .history()
        .filter(|entry| entry.item.status == Status::Completed)
        .count();
    let history_cancelled = scheduler
        .history()
        .filter(|entry| entry.item.status == Status::Cancelled)
        .count();
    let security_metrics = security.retention_metrics();
    let memory_metrics = memory.retention_metrics();
    let hot_metrics = assets.hot_cache_metrics();
    // Evicted scheduler entries leave the process uncounted per-status; the
    // status counts remain lower bounds while retained-state sizes stay exact.
    StateSnapshot {
        scheduler_items: scheduler.items().len(),
        scheduler_active: scheduler_metrics.active,
        scheduler_completed: history_completed,
        scheduler_cancelled: history_cancelled,
        scheduler_history: scheduler_metrics.terminal_retained,
        scheduler_cooldowns: scheduler_metrics.cooldowns,
        content_queue: security_metrics.content_queue,
        audit_records: security_metrics.audit_retained,
        rate_limit_sources: security_metrics.rate_limit_sources,
        working_memory_entries: memory_metrics.entries,
        memory_compaction_records: memory_metrics.compactions,
        memory_links: memory_metrics.links,
        memory_links_high_water: memory_metrics.links_high_water,
        telemetry_events: telemetry.retention_metrics().retained,
        hot_assets: hot_metrics.dynamic_resident,
        promotion_metadata: hot_metrics.promotion_metadata_retained,
    }
}

/// Build the retained-state growth findings a run reports on.
///
/// `exercises_links` marks workloads that actually mint links (the core
/// soak's permit-gated durable writes). A run whose workload structurally
/// cannot create a link — the scenario trace only records working memory —
/// must not emit `memory_links` checks: a bound that reads zero because the
/// subsystem was never exercised is a check that cannot fail, and would
/// pass even if link creation or pruning were completely broken (#105
/// review round 4). Those runs still carry the raw counters in their state
/// snapshots as observations; they just do not claim to have probed a bound
/// they never touched.
pub(crate) fn growth_findings(
    config: &SoakConfig,
    midpoint: &StateSnapshot,
    final_state: &StateSnapshot,
    exercises_links: bool,
) -> Vec<GrowthFinding> {
    let mut findings = vec![
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
            "scheduler_history",
            midpoint.scheduler_history,
            final_state.scheduler_history,
            Some(config.scheduler_history_limit),
            Some(51),
        ),
        growth(
            "scheduler_cooldowns",
            midpoint.scheduler_cooldowns,
            final_state.scheduler_cooldowns,
            None,
            Some(51),
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
            Some(config.audit_limit),
            Some(51),
        ),
        growth(
            "rate_limit_sources",
            midpoint.rate_limit_sources,
            final_state.rate_limit_sources,
            Some(SecurityRuntimeConfig::default().max_rate_limit_sources),
            Some(51),
        ),
        growth(
            "working_memory_entries",
            midpoint.working_memory_entries,
            final_state.working_memory_entries,
            Some(config.working_memory_limit),
            Some(51),
        ),
        growth(
            "memory_compaction_records",
            midpoint.memory_compaction_records,
            final_state.memory_compaction_records,
            Some(config.memory_compaction_limit),
            Some(51),
        ),
    ];
    if exercises_links {
        findings.extend([
            growth(
                "memory_links",
                midpoint.memory_links,
                final_state.memory_links,
                Some(config.memory_links_limit),
                Some(51),
            ),
            growth(
                "memory_links_high_water",
                midpoint.memory_links_high_water,
                final_state.memory_links_high_water,
                Some(config.memory_links_limit),
                Some(51),
            ),
        ]);
    }
    findings.extend([
        growth(
            "telemetry_events",
            midpoint.telemetry_events,
            final_state.telemetry_events,
            Some(config.telemetry_limit),
            Some(51),
        ),
        growth(
            "hot_assets",
            midpoint.hot_assets,
            final_state.hot_assets,
            Some(config.generated_asset_limit),
            Some(53),
        ),
        growth(
            "promotion_metadata",
            midpoint.promotion_metadata,
            final_state.promotion_metadata,
            Some(config.promotion_metadata_limit),
            Some(51),
        ),
    ]);
    findings
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

/// A trusted system-plane event: the soak's durable writes go through the
/// same permit gate production uses, and only system-source (or
/// memory-admin) evidence earns a durable permit — public chat never does.
fn system_event(sequence: u64) -> EventEnvelope {
    EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION.to_owned(),
        event_id: format!("evt-hardening-system-{sequence}"),
        correlation_id: "corr-hardening".to_owned(),
        sequence,
        observed_at: "2026-09-25T00:00:00Z".to_owned(),
        source: "hardening-system".to_owned(),
        source_class: SourceClass::System,
        plane: SecurityPlane::System,
        trust_level: TrustLevel::Trusted,
        kind: EventKind::SystemHealth,
        actor_id: None,
        priority_hint: None,
        authorization: None,
        payload: BTreeMap::from([(
            "text".to_owned(),
            Value::String("hardening system event".to_owned()),
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

pub(crate) fn generated_fixture() -> Result<PerformanceAsset, HardeningError> {
    load_asset_file(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/performance-assets/valid/generated-dynamic.json"),
    )
    .map_err(|error| HardeningError::AssetStore(error.to_string()))
}

pub(crate) fn generated_runtime() -> RuntimeCompatibility {
    RuntimeCompatibility {
        compiler_version: "0.1.0".to_owned(),
        voice_model: Some("voice-ja-v2".to_owned()),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        viseme_mapping: Some("ja-5vowel-v2".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
    }
}
