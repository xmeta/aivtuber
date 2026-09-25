#![forbid(unsafe_code)]

use aivtuber_app::{
    AppError, AssetSemanticIndexConfig, AudioOutput, GenerativeRuntime, IntentRoutePlanner,
    NoopAvatarOutput, NoopStreamOutput, PlaybackRoute, ProductionApp, QueryEmbeddingProvider,
    ReflexRoutePlanner, RoutePlanner, build_semantic_index_from_asset_store,
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
    BenchmarkReport, ComparisonMode, ComparisonSuite, ReproducibilityMetadata, SecretRedactor,
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
use std::time::Duration;

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
    let fixture_path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_fixture_path);
    let output_dir = env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/aivtuber-benchmarks"));

    let fixture = load_fixture(&fixture_path)?;
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
    print_summary(&suite);
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

fn load_fixture(path: &Path) -> Result<Fixture, Box<dyn Error>> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
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
