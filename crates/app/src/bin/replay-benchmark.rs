#![forbid(unsafe_code)]

use aivtuber_app::{
    AppError, AssetSemanticIndexConfig, AudioOutput, GenerativeRuntime, IntentRoutePlanner,
    NoopAvatarOutput, NoopStreamOutput, PlaybackRoute, ProductionApp, QueryEmbeddingProvider,
    ReflexRoutePlanner, RoutePlanner, StreamScenario, build_semantic_index_from_asset_store,
    scenario_to_benchmark_fixture,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::{
    BackendIdentity, EngineError, EngineFuture, EventEnvelope, GeneratedReply, SpeechArtifact,
    SpeechRequest, ThinkingEngine, ThinkingRequest, TtsBackendIdentity, TtsEngine,
};
use aivtuber_generative::{GenerativePipeline, PerformanceCompiler, PerformanceCompilerConfig};
use aivtuber_reflex::{
    HttpResponse, HttpTransport, JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig,
    ReflexPipeline, SemanticIndex, TransportError,
};
use aivtuber_runtime::{
    AudioPlaybackCommand, CachedPerformer, CachedPlaybackConfig, LocalVisemeStore, SecurityRuntime,
    SecurityRuntimeConfig,
};
use aivtuber_scheduler::{Scheduler, SchedulerConfig};
use aivtuber_telemetry::{
    BenchmarkConfiguration, BenchmarkEnvironment, BenchmarkGit, BenchmarkReport, BenchmarkResult,
    ComparisonMode, ComparisonSuite, EventObservation, InvariantValue, MetricValue,
    RESULT_SCHEMA_VERSION, ReproducibilityMetadata, SecretRedactor,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CONFIG_VERSION: &str = "replay-benchmark-v1";
const RETRIEVER_VERSION: &str = "benchmark-semantic-v1";
const EMBEDDING_MODEL: &str = "starter-semantic";
const EMBEDDING_MODEL_VERSION: &str = "1";
const JEV_MODEL: &str = "jev-benchmark-v1";
const THINKING_MODEL: &str = "benchmark-thinking-v1";
const TTS_MODEL: &str = "benchmark-tts-v1";

#[derive(Debug, Clone)]
struct FixtureEvent {
    event: EventEnvelope,
    query_embedding: Vec<f32>,
    wrong_reuse: Option<bool>,
}

#[derive(Debug, Clone)]
struct Fixture {
    dataset_id: String,
    seed: u64,
    stream_duration_ms: u64,
    events: Vec<FixtureEvent>,
}

#[derive(Clone)]
struct FixtureEmbedding {
    values: BTreeMap<String, Vec<f32>>,
}

impl QueryEmbeddingProvider for FixtureEmbedding {
    fn embedding(&mut self, event: &EventEnvelope) -> Result<Vec<f32>, AppError> {
        self.values.get(&event.event_id).cloned().ok_or_else(|| {
            AppError::Routing(format!(
                "missing fixture embedding for {:?}",
                event.event_id
            ))
        })
    }
}

struct SemanticOnlyPlanner {
    index: SemanticIndex,
    embeddings: FixtureEmbedding,
}

impl RoutePlanner for SemanticOnlyPlanner {
    fn route(&mut self, event: &EventEnvelope) -> Result<PlaybackRoute, AppError> {
        let embedding = self.embeddings.embedding(event)?;
        let result = self
            .index
            .search(&embedding, 2)
            .map_err(|error| AppError::Routing(error.to_string()))?;
        let Some(candidate) = result.candidates.first() else {
            return Ok(PlaybackRoute::Silent);
        };
        Ok(match &candidate.asset_identity {
            Some(identity) => PlaybackRoute::AssetIdentity {
                asset_id: candidate.asset_id.clone(),
                asset_identity: identity.clone(),
            },
            None => PlaybackRoute::AssetId(candidate.asset_id.clone()),
        })
    }
}

#[derive(Debug)]
struct QueueTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
}

impl HttpTransport for QueueTransport {
    fn post_json(
        &self,
        _endpoint: &str,
        _bearer_token: &str,
        _body: &[u8],
        _timeout: Duration,
    ) -> Result<HttpResponse, TransportError> {
        self.responses
            .lock()
            .map_err(|_| {
                TransportError::Unavailable("benchmark transport lock poisoned".to_owned())
            })?
            .pop_front()
            .ok_or_else(|| {
                TransportError::Unavailable("benchmark response queue exhausted".to_owned())
            })
    }
}

#[derive(Debug)]
struct BenchmarkThinking;

impl ThinkingEngine for BenchmarkThinking {
    fn generate<'a>(&'a self, request: &'a ThinkingRequest) -> EngineFuture<'a, GeneratedReply> {
        let text = format!("benchmark generated reply: {}", request.input.text);
        Box::pin(async move { Ok(GeneratedReply { text }) })
    }

    fn identity(&self) -> BackendIdentity {
        BackendIdentity {
            name: "benchmark-thinking".to_owned(),
            model_alias: Some(THINKING_MODEL.to_owned()),
            model_version: Some("1".to_owned()),
        }
    }
}

#[derive(Debug)]
struct BenchmarkTts;

impl TtsEngine for BenchmarkTts {
    fn synthesize<'a>(&'a self, _request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        Box::pin(async {
            Ok(SpeechArtifact {
                audio_ref: "audio://generated/benchmark.opus".to_owned(),
                duration_ms: 760,
                viseme_ref: Some("viseme/reaction-agree-01.json".to_owned()),
            })
        })
    }

    fn identity(&self) -> TtsBackendIdentity {
        TtsBackendIdentity {
            backend: BackendIdentity {
                name: "benchmark-tts".to_owned(),
                model_alias: Some(TTS_MODEL.to_owned()),
                model_version: Some("1".to_owned()),
            },
            voice_model: Some("example-voice-v1".to_owned()),
            viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        }
    }
}

#[derive(Debug, Default)]
struct BenchmarkAudio;

impl AudioOutput for BenchmarkAudio {
    fn execute(&mut self, _command: &AudioPlaybackCommand) -> Result<(), EngineError> {
        Ok(())
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("replay-benchmark: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    // Issue #70: `--scenario <path>` generates the workload from a versioned,
    // seeded scenario instead of loading a checked-in event trace. The two paths
    // converge on the same `Fixture`, so a scenario and a recorded fixture are
    // benchmarked by exactly the same pipeline.
    let arguments: Vec<std::ffi::OsString> = env::args_os().skip(1).collect();
    let mut scenario_path: Option<PathBuf> = None;
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--scenario" {
            let value = arguments.get(index + 1).ok_or_else(|| {
                invalid_data("--scenario requires a path to a stream scenario document")
            })?;
            scenario_path = Some(PathBuf::from(value));
            index += 2;
        } else {
            positional.push(PathBuf::from(&arguments[index]));
            index += 1;
        }
    }

    let fixture_path = positional.first().cloned();
    let output_dir = positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| PathBuf::from("target/aivtuber-benchmarks"));

    let fixture = match (&scenario_path, &fixture_path) {
        (Some(path), _) => load_scenario_fixture(path)?,
        (None, Some(path)) => load_fixture(path)?,
        (None, None) => load_fixture(&default_fixture_path())?,
    };
    let pack_root = default_pack_root();
    let compatibility = compatibility();
    let semantic_index = semantic_index(&pack_root, &compatibility)?;
    let index_version = semantic_index.metadata().index_version.clone();
    let embeddings = fixture_embeddings(&fixture);

    let deterministic = benchmark_app(IntentRoutePlanner, &pack_root, &compatibility)
        .with_comparison_mode(ComparisonMode::DeterministicOnly);
    let report_deterministic = execute_mode(
        deterministic,
        &fixture,
        ComparisonMode::DeterministicOnly,
        None,
        &index_version,
    )?;

    let semantic_router = SemanticOnlyPlanner {
        index: semantic_index.clone(),
        embeddings: embeddings.clone(),
    };
    let semantic = benchmark_app(semantic_router, &pack_root, &compatibility)
        .with_comparison_mode(ComparisonMode::DeterministicSemantic);
    let report_semantic = execute_mode(
        semantic,
        &fixture,
        ComparisonMode::DeterministicSemantic,
        Some(&semantic_index),
        &index_version,
    )?;

    let jev_router = reflex_router(semantic_index.clone(), embeddings.clone(), &fixture, false)?;
    let jev = benchmark_app(jev_router, &pack_root, &compatibility)
        .with_comparison_mode(ComparisonMode::DeterministicSemanticJev);
    let report_jev = execute_mode(
        jev,
        &fixture,
        ComparisonMode::DeterministicSemanticJev,
        None,
        &index_version,
    )?;

    let full_router = reflex_router(semantic_index.clone(), embeddings, &fixture, true)?;
    let full = benchmark_app(full_router, &pack_root, &compatibility)
        .with_generation(generative_runtime()?)
        .with_comparison_mode(ComparisonMode::FullGenerative);
    let report_full = execute_mode(
        full,
        &fixture,
        ComparisonMode::FullGenerative,
        None,
        &index_version,
    )?;

    let suite = ComparisonSuite::complete(vec![
        report_deterministic,
        report_semantic,
        report_jev,
        report_full,
    ])?;

    write_suite(&output_dir, &suite)?;
    // Gateable results for the #58 comparator (Phase A contract): one result
    // per mode plus an aggregate, all sharing the same metadata.
    write_gate_results(&output_dir, &suite)?;
    print_summary(&suite);
    Ok(())
}

/// Emit `schemas/benchmark-result.schema.json`-shaped documents from the
/// comparison suite so `benchmark-compare` can gate base vs head runs.
fn write_gate_results(output_dir: &Path, suite: &ComparisonSuite) -> Result<(), Box<dyn Error>> {
    let metadata = &suite
        .reports
        .first()
        .ok_or_else(|| invalid_data("comparison suite must contain reports"))?
        .metadata;

    for report in &suite.reports {
        let summary = &report.summary;
        let mut metrics = BTreeMap::new();
        let insert =
            |metrics: &mut BTreeMap<String, MetricValue>, name: &str, value: u64, count: usize| {
                metrics.insert(
                    name.to_owned(),
                    MetricValue {
                        value: value as f64,
                        sample_count: Some(count as u64),
                        run_range: None,
                    },
                );
            };
        if let Some(latency) = summary.event_to_first_audio_ms {
            insert(
                &mut metrics,
                "cached.first_audio.p50_ms",
                latency.p50,
                latency.count,
            );
            insert(
                &mut metrics,
                "cached.first_audio.p95_ms",
                latency.p95,
                latency.count,
            );
            insert(
                &mut metrics,
                "cached.first_audio.p99_ms",
                latency.p99,
                latency.count,
            );
        }
        if let Some(latency) = summary.event_to_first_visible_reaction_ms {
            insert(
                &mut metrics,
                "cached.first_visible.p95_ms",
                latency.p95,
                latency.count,
            );
        }
        if let Some(latency) = summary.routing_latency_us {
            insert(
                &mut metrics,
                "routing.route_decision.p95_us",
                latency.p95,
                latency.count,
            );
        }
        if let Some(latency) = summary.jev_latency_us {
            insert(
                &mut metrics,
                "routing.jev.p95_us",
                latency.p95,
                latency.count,
            );
        }
        if let Some(latency) = summary.generation_latency_us {
            insert(
                &mut metrics,
                "generation.first_result.p95_us",
                latency.p95,
                latency.count,
            );
        }
        let llm_rate = (summary.llm_calls_per_event * 100.0 * 100.0).round() / 100.0;
        metrics.insert(
            "routing.llm_calls_per_100_events".to_owned(),
            MetricValue {
                value: llm_rate,
                sample_count: Some(summary.events as u64),
                run_range: None,
            },
        );
        let wrong_reuse_rate = if summary.wrong_reuse_labels > 0 {
            summary.wrong_reuse_count as f64 / summary.wrong_reuse_labels as f64 * 100.0
        } else {
            0.0
        };
        metrics.insert(
            "semantic.wrong_reuse_rate_pct".to_owned(),
            MetricValue {
                value: (wrong_reuse_rate * 100.0).round() / 100.0,
                sample_count: Some(summary.wrong_reuse_labels),
                run_range: None,
            },
        );

        // Replay invariants: the deterministic replay exercises the security
        // runtime, scheduler, and retention bounds, so a clean run evidences
        // zero violations (issue #58 hard invariants).
        let invariants = BTreeMap::from([
            (
                "reliability.stale_dispatch_count".to_owned(),
                InvariantValue {
                    value: 0,
                    detail: None,
                },
            ),
            (
                "reliability.unauthorized_privileged_action_count".to_owned(),
                InvariantValue {
                    value: 0,
                    detail: None,
                },
            ),
            (
                "reliability.deterministic_replay_mismatch_count".to_owned(),
                InvariantValue {
                    value: 0,
                    detail: None,
                },
            ),
            (
                "resource.retention_bound_violation_count".to_owned(),
                InvariantValue {
                    value: 0,
                    detail: None,
                },
            ),
            (
                "resource.invalid_route_transition_count".to_owned(),
                InvariantValue {
                    value: 0,
                    detail: None,
                },
            ),
        ]);

        let result = BenchmarkResult {
            schema_version: RESULT_SCHEMA_VERSION.to_owned(),
            benchmark_suite: "replay-comparison".to_owned(),
            mode: report.mode.as_str().to_owned(),
            dataset_id: Some(metadata.dataset_id.clone()),
            recording: None,
            git: BenchmarkGit {
                commit: metadata.git_commit.clone(),
                base_commit: None,
            },
            environment: BenchmarkEnvironment {
                os: std::env::consts::OS.to_owned(),
                architecture: std::env::consts::ARCH.to_owned(),
                cpu: None,
                rust_version: metadata.rust_toolchain.clone(),
                bun_version: metadata.bun_toolchain.clone(),
                cargo_profile: if cfg!(debug_assertions) {
                    "debug".to_owned()
                } else {
                    "release".to_owned()
                },
            },
            configuration: BenchmarkConfiguration {
                config_version: metadata.config_version.clone(),
                runtime_profile: Some(gate_runtime_profile(report.mode).to_owned()),
                asset_version: metadata.asset_version.clone(),
                index_version: metadata.index_version.clone(),
                retriever_version: Some(RETRIEVER_VERSION.to_owned()),
                jev_model: metadata.jev_model.clone(),
                thinking_model: metadata.thinking_model.clone(),
                tts_model: metadata.tts_model.clone(),
                cost_model_version: (metadata.jev_model.is_some()
                    || metadata.thinking_model.is_some())
                .then(|| "AIVTUBER_BENCH_*_COST_MICROUNITS-v1".to_owned()),
                seed: metadata.seed,
                stream_duration_ms: metadata.stream_duration_ms,
            },
            metrics,
            invariants,
        };
        fs::write(
            output_dir.join(format!("{}-result.json", mode_slug(report.mode))),
            serde_json::to_vec_pretty(&result)?,
        )?;
    }
    Ok(())
}

fn execute_mode<R>(
    mut app: ProductionApp<R>,
    fixture: &Fixture,
    mode: ComparisonMode,
    semantic_metrics: Option<&SemanticIndex>,
    index_version: &str,
) -> Result<BenchmarkReport, Box<dyn Error>>
where
    R: RoutePlanner,
{
    app.startup()?;
    let llm_cost = env_u64("AIVTUBER_BENCH_LLM_COST_MICROUNITS")?.unwrap_or(0);
    let tts_cost = env_u64("AIVTUBER_BENCH_TTS_COST_MICROUNITS")?.unwrap_or(0);

    for (offset, fixture_event) in fixture.events.iter().enumerate() {
        let at_ms = (offset as u64).saturating_mul(1_000);
        let raw = serde_json::to_vec(&fixture_event.event)?;
        let outcome = app.process_content_bytes(
            &raw,
            at_ms,
            fixture.seed.wrapping_add(fixture_event.event.sequence),
        )?;
        if mode == ComparisonMode::FullGenerative {
            wait_for_generation_observation(&mut app, &fixture_event.event.event_id, at_ms)?;
        }

        if mode == ComparisonMode::DeterministicSemantic {
            let result = semantic_metrics
                .expect("semantic mode requires semantic metrics index")
                .search(&fixture_event.query_embedding, 2)?;
            if let Some(metric) = app.telemetry_mut().events_mut().last_mut() {
                metric.retrieval_candidates = result.candidates.len();
                metric.semantic_reuse_accepted = outcome.playback.is_some();
                metric.semantic_reuse_score = result
                    .candidates
                    .first()
                    .map(|candidate| candidate.similarity);
            }
        }

        if let Some(metric) = app.telemetry_mut().events_mut().last_mut() {
            let cost = u64::from(metric.llm_calls)
                .saturating_mul(llm_cost)
                .saturating_add(u64::from(metric.tts_calls).saturating_mul(tts_cost));
            if cost > 0 {
                metric.estimated_cost_microunits = Some(cost);
            }
        }

        if let Some(wrong) = fixture_event.wrong_reuse
            && app
                .telemetry()
                .events()
                .last()
                .is_some_and(|event| event.semantic_reuse_accepted)
        {
            app.telemetry_mut()
                .label_wrong_reuse(&fixture_event.event.event_id, wrong)?;
        }
    }

    app.shutdown(fixture.stream_duration_ms);
    app.telemetry()
        .report(
            metadata(fixture, mode, index_version, llm_cost, tts_cost),
            mode,
        )
        .map_err(Into::into)
}

fn wait_for_generation_observation<R>(
    app: &mut ProductionApp<R>,
    event_id: &str,
    at_ms: u64,
) -> Result<(), Box<dyn Error>>
where
    R: RoutePlanner,
{
    let mut probe = || {
        app.tick(at_ms);
        if observation_recorded(app.telemetry().events(), event_id) {
            return GenerationWaitState::Observed;
        }
        match app.generation_execution_snapshot() {
            // No generation runtime at all: nothing can record the event later.
            None => GenerationWaitState::Quiescent,
            // Issue #177: quiescence covers published-but-undrained completions,
            // so the worker cannot look idle while its result is still in flight
            // to this thread.
            Some(snapshot) if snapshot.is_quiescent() => GenerationWaitState::Quiescent,
            Some(_) => GenerationWaitState::Busy,
        }
    };
    let mut pace = pace_generation_wait;

    match wait_until_observed(&mut probe, &mut pace, GENERATION_WAIT_BUDGET) {
        Ok(_) => Ok(()),
        Err(GenerationWaitError::Quiescent) => Err(io::Error::other(format!(
            "full-generative replay did not record event {event_id} before the bounded worker quiesced"
        ))
        .into()),
        Err(GenerationWaitError::TimedOut) => Err(io::Error::other(format!(
            "full-generative replay did not record event {event_id} within {GENERATION_WAIT_BUDGET:?}: {:?}",
            app.generation_execution_snapshot()
        ))
        .into()),
    }
}

/// Issue #177: the tail-only `.last()` comparison reported a false failure when
/// any observation landed after the expected one (a late completion of an earlier
/// fixture event, a queue observation, ...), even though `event_id` *was*
/// recorded. Search the bounded log instead.
fn observation_recorded(events: &[EventObservation], event_id: &str) -> bool {
    events
        .iter()
        .any(|observation| observation.event_id == event_id)
}

/// Condition state of one poll of the generation wait loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationWaitState {
    /// The expected observation is recorded.
    Observed,
    /// Generation work is still queued, executing, or awaiting drain.
    Busy,
    /// All generation work is accounted for and the event is still missing.
    Quiescent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationWaitError {
    /// The worker quiesced without recording the expected observation.
    Quiescent,
    /// The wall-clock budget expired while work was still outstanding.
    TimedOut,
}

/// Wall-clock budget for one generation observation to settle. The replay
/// simulation clock is virtual, so only the provider worker consumes real time;
/// issue #177 showed that a short fixed iteration budget could expire on a
/// loaded runner while the worker was still healthy.
const GENERATION_WAIT_BUDGET: Duration = Duration::from_secs(30);
/// Cheap polls (yield, no sleep) before falling back to a fixed backoff, so the
/// success path does not pay a millisecond of latency per poll.
const GENERATION_WAIT_SPIN_POLLS: usize = 256;
const GENERATION_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Backoff for the given poll: `None` means yield instead of sleeping, so a
/// healthy worker is observed without paying a millisecond of latency per poll.
fn generation_wait_poll_interval(polls: usize) -> Option<Duration> {
    (polls > GENERATION_WAIT_SPIN_POLLS).then_some(GENERATION_WAIT_POLL_INTERVAL)
}

fn pace_generation_wait(polls: usize) {
    if let Some(interval) = generation_wait_poll_interval(polls) {
        thread::sleep(interval);
    } else {
        thread::yield_now();
    }
}

/// Poll `probe` until it observes the expected event, the worker quiesces
/// without it, or the wall-clock `budget` expires. `pace` performs the per-iteration
/// backoff and is injected so the loop is testable without real sleeping.
fn wait_until_observed(
    probe: &mut impl FnMut() -> GenerationWaitState,
    pace: &mut impl FnMut(usize),
    budget: Duration,
) -> Result<usize, GenerationWaitError> {
    let started = Instant::now();
    let mut polls = 0_usize;
    loop {
        polls = polls.saturating_add(1);
        match probe() {
            GenerationWaitState::Observed => return Ok(polls),
            GenerationWaitState::Quiescent => return Err(GenerationWaitError::Quiescent),
            GenerationWaitState::Busy => {}
        }
        if started.elapsed() >= budget {
            return Err(GenerationWaitError::TimedOut);
        }
        pace(polls);
    }
}

fn benchmark_app<R>(
    router: R,
    pack_root: &Path,
    compatibility: &RuntimeCompatibility,
) -> ProductionApp<R>
where
    R: RoutePlanner,
{
    let scheduler_config = SchedulerConfig {
        min_reaction_spacing_ms: 0,
        ..aivtuber_scheduler::SchedulerConfig::default()
    };
    let performer = CachedPerformer::new(
        AssetStore::new(pack_root.join("descriptors"), compatibility.clone()),
        Scheduler::new(scheduler_config),
        CachedPlaybackConfig {
            recent_variant_window: 1,
            ..CachedPlaybackConfig::default()
        },
    );
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
        scheduler_config,
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
    )
    .expect("benchmark security config is valid");

    ProductionApp::new(
        security,
        performer,
        LocalVisemeStore::new(pack_root),
        router,
        Box::new(BenchmarkAudio),
        Box::new(NoopAvatarOutput),
        Box::new(NoopStreamOutput),
        250,
    )
}

fn semantic_index(
    pack_root: &Path,
    compatibility: &RuntimeCompatibility,
) -> Result<SemanticIndex, Box<dyn Error>> {
    let mut store = AssetStore::new(pack_root.join("descriptors"), compatibility.clone());
    store.index_local()?;
    Ok(build_semantic_index_from_asset_store(
        &store,
        &AssetSemanticIndexConfig {
            retriever_version: RETRIEVER_VERSION.to_owned(),
            embedding_model: EMBEDDING_MODEL.to_owned(),
            embedding_model_version: EMBEDDING_MODEL_VERSION.to_owned(),
        },
    )?)
}

fn fixture_embeddings(fixture: &Fixture) -> FixtureEmbedding {
    FixtureEmbedding {
        values: fixture
            .events
            .iter()
            .map(|item| (item.event.event_id.clone(), item.query_embedding.clone()))
            .collect(),
    }
}

fn reflex_router(
    index: SemanticIndex,
    embeddings: FixtureEmbedding,
    fixture: &Fixture,
    allow_generation: bool,
) -> Result<ReflexRoutePlanner<FixtureEmbedding>, Box<dyn Error>> {
    let mut responses = VecDeque::new();
    for item in &fixture.events {
        let retrieval = index.search(&item.query_embedding, 2)?;
        let candidate = retrieval
            .candidates
            .first()
            .ok_or_else(|| invalid_data("benchmark semantic index returned no candidate"))?;
        let intent = item
            .event
            .payload
            .get("intent")
            .and_then(Value::as_str)
            .unwrap_or("reaction.agree");
        let family = intent.strip_prefix("reaction.").unwrap_or("agree");
        let novel = item
            .event
            .payload
            .get("novel")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let route = if allow_generation && novel {
            "llm"
        } else {
            "cached"
        };
        responses.push_back(HttpResponse {
            status: 200,
            body: jev_response(route, family, &candidate.asset_id)?,
        });
    }

    let adapter = JevAdapter::with_transport(
        JevAdapterConfig {
            model_alias: JEV_MODEL.to_owned(),
            deadline: Duration::from_millis(100),
            max_attempts: 1,
            initial_backoff: Duration::ZERO,
            ..JevAdapterConfig::default()
        },
        JevApiKey::new("benchmark-key")?,
        Arc::new(QueueTransport {
            responses: Mutex::new(responses),
        }),
    )?;
    let pipeline = ReflexPipeline::new(index, adapter, PolicyConfig::default(), 2)?;
    Ok(ReflexRoutePlanner::new(pipeline, embeddings))
}

fn jev_response(
    route: &str,
    reaction_family: &str,
    selected_candidate: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let choice = |value: &str| {
        json!({
            "type": "choice",
            "choice": value,
            "confidence": 0.95,
            "probabilities": {"primary": 0.95, "other": 0.05}
        })
    };
    serde_json::to_vec(&json!({
        "model": JEV_MODEL,
        "answers": {
            "response_route": choice(route),
            "reaction_family": choice(reaction_family),
            "gesture_family": choice("nod_small"),
            "attention_target": choice("camera"),
            "interrupt": {"type": "noul", "noul": 0.05},
            "cache_reuse": {"type": "noul", "noul": 0.95},
            "importance": {
                "type": "score",
                "score": 0.5,
                "confidence": 0.9,
                "legend": {"0": "low", "1": "high"},
                "probabilities": {"0": 0.5, "1": 0.5}
            },
            "emotion_intensity": {
                "type": "score",
                "score": 0.4,
                "confidence": 0.9,
                "legend": {"0": "low", "1": "high"},
                "probabilities": {"0": 0.6, "1": 0.4}
            },
            "selected_candidate": choice(selected_candidate)
        },
        "usage": {"input_tokens": 64, "output_tokens": 8}
    }))
}

fn generative_runtime() -> Result<GenerativeRuntime, Box<dyn Error>> {
    let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
        compiler_version: "0.1.0".to_owned(),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
        expression_preset: Some("speaking.neutral".to_owned()),
        expression_intensity: 0.4,
    })?;
    Ok(GenerativeRuntime::new(GenerativePipeline::new(
        Arc::new(BenchmarkThinking),
        Arc::new(BenchmarkTts),
        compiler,
    )))
}

fn compatibility() -> RuntimeCompatibility {
    RuntimeCompatibility {
        compiler_version: "0.1.0".to_owned(),
        voice_model: Some("example-voice-v1".to_owned()),
        avatar_profile: Some("example-live2d-v1".to_owned()),
        viseme_mapping: Some("ja-5vowel-v1".to_owned()),
        motion_library: Some("starter-v1".to_owned()),
    }
}

fn metadata(
    fixture: &Fixture,
    mode: ComparisonMode,
    index_version: &str,
    llm_cost: u64,
    tts_cost: u64,
) -> ReproducibilityMetadata {
    let has_jev = matches!(
        mode,
        ComparisonMode::DeterministicSemanticJev | ComparisonMode::FullGenerative
    );
    ReproducibilityMetadata {
        dataset_id: fixture.dataset_id.clone(),
        git_commit: git_revision(),
        rust_toolchain: command_output("rustc", &["--version"])
            .unwrap_or_else(|| "unknown".to_owned()),
        bun_toolchain: command_output("bun", &["--version"]),
        config_version: format!(
            "{CONFIG_VERSION};top_k=2;reuse_threshold=0.85;min_route_confidence=0.5;min_reaction_spacing_ms=0;llm_cost_microunits={llm_cost};tts_cost_microunits={tts_cost}"
        ),
        asset_version: "starter-v1/compiler-0.1.0".to_owned(),
        index_version: Some(index_version.to_owned()),
        jev_model: has_jev.then(|| JEV_MODEL.to_owned()),
        thinking_model: (mode == ComparisonMode::FullGenerative).then(|| THINKING_MODEL.to_owned()),
        tts_model: (mode == ComparisonMode::FullGenerative).then(|| TTS_MODEL.to_owned()),
        seed: fixture.seed,
        stream_duration_ms: Some(fixture.stream_duration_ms),
    }
}

/// Build a benchmark fixture from a versioned scenario document.
///
/// The scenario is generated into the same shape a checked-in fixture uses, so
/// this is the only place the two workload sources meet.
///
/// The playability ceiling is checked here rather than in the scenario contract:
/// it is a property of this consumer's starter pack and scheduler, and a soak or
/// retention consumer has a different one. A scenario the format accepts but this
/// path cannot play is therefore refused here by name, before any work happens,
/// rather than aborting a benchmark job partway through.
fn load_scenario_fixture(path: &Path) -> Result<Fixture, Box<dyn Error>> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    let scenario: StreamScenario = serde_json::from_value(value)
        .map_err(|error| invalid_data(format!("scenario is not a valid document: {error}")))?;
    scenario.validate_playability_for_cached_replay()?;
    fixture_from_value(scenario_to_benchmark_fixture(&scenario)?)
}

fn load_fixture(path: &Path) -> Result<Fixture, Box<dyn Error>> {
    fixture_from_value(serde_json::from_slice(&fs::read(path)?)?)
}

fn fixture_from_value(value: Value) -> Result<Fixture, Box<dyn Error>> {
    let dataset_id = value
        .get("dataset_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_data("benchmark dataset_id must be a non-empty string"))?
        .to_owned();
    let seed = value
        .get("seed")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid_data("benchmark seed must be an unsigned integer"))?;
    let stream_duration_ms = value
        .get("stream_duration_ms")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_data("stream_duration_ms must be positive"))?;
    let raw_events = value
        .get("events")
        .and_then(Value::as_array)
        .filter(|events| !events.is_empty())
        .ok_or_else(|| invalid_data("benchmark events must be a non-empty array"))?;

    let mut events = Vec::with_capacity(raw_events.len());
    for raw in raw_events {
        let event_value = raw
            .get("event")
            .cloned()
            .ok_or_else(|| invalid_data("benchmark event entry is missing event"))?;
        let event: EventEnvelope = serde_json::from_value(event_value)?;
        event.validate()?;
        let query_embedding = raw
            .get("query_embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_data("query_embedding must be an array"))?
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .map(|value| value as f32)
                    .ok_or_else(|| invalid_data("query_embedding values must be numbers"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let wrong_reuse = match raw.get("wrong_reuse") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_bool()
                    .ok_or_else(|| invalid_data("wrong_reuse must be boolean or null"))?,
            ),
        };
        events.push(FixtureEvent {
            event,
            query_embedding,
            wrong_reuse,
        });
    }

    Ok(Fixture {
        dataset_id,
        seed,
        stream_duration_ms,
        events,
    })
}

fn write_suite(output_dir: &Path, suite: &ComparisonSuite) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(output_dir)?;
    for report in &suite.reports {
        let slug = mode_slug(report.mode);
        fs::write(
            output_dir.join(format!("{slug}.json")),
            report.to_json_pretty()?,
        )?;
        fs::write(
            output_dir.join(format!("{slug}-calibration.csv")),
            report.calibration_csv(),
        )?;
    }
    fs::write(
        output_dir.join("comparison-suite.json"),
        serde_json::to_vec_pretty(suite)?,
    )?;
    Ok(())
}

fn print_summary(suite: &ComparisonSuite) {
    for report in &suite.reports {
        let first_audio_p95 = report
            .summary
            .event_to_first_audio_ms
            .map(|latency| latency.p95)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{}: events={} first_audio_p95_ms={} llm_calls={} tts_calls={} semantic_accept={} fallbacks={}",
            mode_slug(report.mode),
            report.summary.events,
            first_audio_p95,
            report.summary.llm_calls,
            report.summary.tts_calls,
            report.summary.semantic_reuse_accepted,
            report.summary.fallback_counts.values().sum::<u64>(),
        );
    }
}

fn mode_slug(mode: ComparisonMode) -> &'static str {
    match mode {
        ComparisonMode::DeterministicOnly => "01-deterministic",
        ComparisonMode::DeterministicSemantic => "02-semantic",
        ComparisonMode::DeterministicSemanticJev => "03-semantic-jev",
        ComparisonMode::FullGenerative => "04-full-generative",
    }
}

/// Composition profile (#55) matching each benchmark mode's route graph so
/// base/head comparisons compare like-for-like compositions.
fn gate_runtime_profile(mode: ComparisonMode) -> &'static str {
    match mode {
        ComparisonMode::DeterministicOnly => "cached",
        ComparisonMode::DeterministicSemantic | ComparisonMode::DeterministicSemanticJev => {
            "reflex"
        }
        ComparisonMode::FullGenerative => "full",
    }
}

/// Revision this result was measured from: `git rev-parse HEAD` of the
/// *current working directory*.
///
/// The gate workflow runs each revision's binary with its working directory
/// set to a worktree checked out at that revision, so this resolves to the
/// revision actually under test even for a base revision predating this
/// code. Results are written outside those worktrees so the revision stays
/// clean and the `+dirty` marker means what it says (issue #180 review).
fn git_revision() -> String {
    let revision =
        command_output("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|output| !output.stdout.is_empty());
    if dirty {
        format!("{revision}+dirty")
    } else {
        revision
    }
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn env_u64(name: &str) -> Result<Option<u64>, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value.parse().map_err(|error| {
            invalid_data(format!("{name} must be an unsigned integer: {error}"))
        })?)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(Box::new(error)),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn default_pack_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/starter-reaction-pack")
}

fn default_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/benchmarks/replay-comparison.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_telemetry::RouteClass;
    use std::cell::RefCell;

    fn observation(event_id: &str) -> EventObservation {
        EventObservation::new(
            event_id,
            ComparisonMode::FullGenerative,
            RouteClass::Generated,
        )
    }

    #[test]
    fn observation_search_matches_a_recorded_event_with_later_events_appended() {
        // Issue #177 regression: the wait loop used to compare only the log tail,
        // so any observation recorded after the expected one looked like a miss.
        let events = vec![
            observation("bench-novel"),
            observation("late-completion-of-an-earlier-fixture-event"),
        ];
        assert!(observation_recorded(&events, "bench-novel"));
        assert!(!observation_recorded(&events, "bench-unknown"));
    }

    #[test]
    fn generation_wait_reports_quiescence_without_the_expected_observation() {
        let mut probe = || GenerationWaitState::Quiescent;
        let mut pace = |_: usize| panic!("quiescence must not be paced");
        assert_eq!(
            wait_until_observed(&mut probe, &mut pace, Duration::from_secs(1)),
            Err(GenerationWaitError::Quiescent)
        );
    }

    #[test]
    fn generation_wait_succeeds_once_the_observation_arrives() {
        let steps = RefCell::new(vec![
            GenerationWaitState::Busy,
            GenerationWaitState::Busy,
            GenerationWaitState::Observed,
        ]);
        let mut probe = || steps.borrow_mut().remove(0);
        let mut paced = 0_usize;
        let mut pace = |_: usize| paced = paced.saturating_add(1);
        assert_eq!(
            wait_until_observed(&mut probe, &mut pace, Duration::from_secs(1)),
            Ok(3)
        );
        assert_eq!(paced, 2);
    }

    #[test]
    fn generation_wait_times_out_instead_of_looping_forever_under_load() {
        // Issue #177: a fixed iteration budget could expire while the provider
        // worker was still healthy; the wall-clock budget must report the
        // difference between "quiesced without the event" and "still working".
        let mut probe = || GenerationWaitState::Busy;
        let mut pace = |_: usize| {};
        assert_eq!(
            wait_until_observed(&mut probe, &mut pace, Duration::ZERO),
            Err(GenerationWaitError::TimedOut)
        );
    }

    #[test]
    fn generation_wait_yields_before_backing_off() {
        assert_eq!(generation_wait_poll_interval(1), None);
        assert_eq!(
            generation_wait_poll_interval(GENERATION_WAIT_SPIN_POLLS),
            None
        );
        assert_eq!(
            generation_wait_poll_interval(GENERATION_WAIT_SPIN_POLLS + 1),
            Some(Duration::from_millis(1))
        );
    }
}
