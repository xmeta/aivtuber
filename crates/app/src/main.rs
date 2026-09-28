use aivtuber_adaptation::{
    ActorPseudonymizer, AdaptationEngine, PromotionPolicy, WorkingMemory, WorkingMemoryConfig,
};
use aivtuber_adapters::{
    NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig, ObsWebSocketAdapter, ObsWebSocketConfig,
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ProcessAudioConfig, ProcessAudioPlayer,
    SecretString, VTubeStudioAdapter, VTubeStudioConfig,
};
use aivtuber_app::{
    AdaptationRuntime, AdapterHealth, AvatarOutput, CompositionProfile, GenerativeRuntime,
    IntentRoutePlanner, NoopAvatarOutput, NoopStreamOutput, ObsStreamOutput, OperatorControlServer,
    OperatorRateLimiter, ProductionApp, ProfileSummary, RawIngressConfig, RawIngressMetrics,
    RawIngressMetricsSnapshot, RoutePlanner, RuntimeRetentionPolicy, StreamOutput, VtsAvatarOutput,
    VtsPlaybackConfig, apply_dispatched_request, pump_bounded_records,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, LocalControlIngress, OperatorCommandInput,
};
use aivtuber_generative::{GenerativePipeline, PerformanceCompiler, PerformanceCompilerConfig};
use aivtuber_reflex::{JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig, ReflexPipeline};
use aivtuber_runtime::{
    CachedPerformer, CachedPlaybackConfig, LocalVisemeStore, SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::{Scheduler, SchedulerConfig};
use aivtuber_telemetry::SecretRedactor;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    if let Err(error) = run() {
        eprintln!("aivtuber-app: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let pack_root = env_path("AIVTUBER_PACK_ROOT")
        .unwrap_or_else(|| PathBuf::from("examples/starter-reaction-pack"));
    let descriptors = pack_root.join("descriptors");
    let visemes = LocalVisemeStore::new(&pack_root);

    let compatibility = RuntimeCompatibility {
        compiler_version: env_string("AIVTUBER_COMPILER_VERSION", "0.1.0"),
        voice_model: env_optional("AIVTUBER_VOICE_MODEL")
            .or_else(|| Some("example-voice-v1".to_owned())),
        avatar_profile: env_optional("AIVTUBER_AVATAR_PROFILE")
            .or_else(|| Some("example-live2d-v1".to_owned())),
        viseme_mapping: env_optional("AIVTUBER_VISEME_MAPPING")
            .or_else(|| Some("ja-5vowel-v1".to_owned())),
        motion_library: env_optional("AIVTUBER_MOTION_LIBRARY")
            .or_else(|| Some("starter-v1".to_owned())),
    };

    let retention = build_runtime_retention_policy()?;

    // Issue #55: the composition profile — not disconnected env flags —
    // names the active route graph. Resolve and validate before composing.
    let profile = match env_optional("AIVTUBER_MODE") {
        Some(mode) => CompositionProfile::parse(&mode)?,
        None => CompositionProfile::Cached,
    };
    let generative_configured = build_generative_runtime(&compatibility, retention)?;
    let jev_api_key_present =
        env_optional("AIVTUBER_JEV_API_KEY").is_some_and(|key| !key.trim().is_empty());
    profile.validate(generative_configured.is_some(), jev_api_key_present)?;
    // Profiles that cannot reach generative routes must not compose the
    // generative runtime (a configured-but-unreachable backend is rejected
    // above as misleading).
    let generative = if profile.generative_route() {
        generative_configured
    } else {
        None
    };
    let adaptation = build_adaptation_runtime(retention)?;
    let scheduler_config = retention.scheduler_config(SchedulerConfig::default());
    let security_config = retention.security_config(SecurityRuntimeConfig::default());
    let raw_ingress_config = RawIngressConfig::from_security(security_config)?;
    let mut assets = AssetStore::new(descriptors, compatibility.clone());
    assets.set_hot_cache_config(retention.hot_cache_config());
    let performer = CachedPerformer::new(
        assets,
        Scheduler::new(scheduler_config),
        retention.cached_playback_config(CachedPlaybackConfig::default()),
    );
    let security = SecurityRuntime::new(
        security_config,
        scheduler_config,
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
    )?;

    let mut audio_config = ProcessAudioConfig {
        root: env_path("AIVTUBER_AUDIO_ROOT").unwrap_or_else(|| pack_root.join("audio")),
        ..ProcessAudioConfig::default()
    };
    if let Some(program) = env_optional("AIVTUBER_AUDIO_PROGRAM") {
        audio_config.program = program;
    }
    let audio = ProcessAudioPlayer::new(audio_config)?;

    let avatar = build_avatar_output()?;
    let stream = build_stream_output()?;
    let max_lateness_ms = env_u64("AIVTUBER_MAX_DISPATCH_LATENESS_MS", 250)?;

    // Compose the planner from the selected profile (issue #55): the mode
    // unambiguously determines the routing stack.
    let startup_summary;
    let mut app = match profile {
        CompositionProfile::Cached => {
            startup_summary = ProfileSummary::for_profile(
                profile,
                generative.is_some(),
                aivtuber_app::config_fingerprint(profile, generative.is_some()),
            );
            ProductionApp::new(
                security,
                performer,
                visemes,
                Box::new(IntentRoutePlanner) as Box<dyn RoutePlanner>,
                Box::new(audio),
                avatar,
                stream,
                max_lateness_ms,
            )
        }
        CompositionProfile::Reflex | CompositionProfile::Full => {
            let semantic_pack_root = env_path("AIVTUBER_PACK_ROOT")
                .unwrap_or_else(|| PathBuf::from("examples/starter-reaction-pack"));
            let mut semantic_assets = AssetStore::new(
                semantic_pack_root.join("descriptors"),
                compatibility.clone(),
            );
            semantic_assets.index_local()?;
            let semantic_index = aivtuber_app::build_semantic_index_from_asset_store(
                &semantic_assets,
                &aivtuber_app::AssetSemanticIndexConfig {
                    retriever_version: "production-v1".to_owned(),
                    embedding_model: "starter-semantic".to_owned(),
                    embedding_model_version: "1".to_owned(),
                },
            )?;
            let embedding_provider =
                aivtuber_app::PayloadIntentEmbedding::new(semantic_index.dimension());
            let jev_adapter = JevAdapter::new(
                JevAdapterConfig {
                    endpoint: env_string(
                        "AIVTUBER_JEV_ENDPOINT",
                        "https://api.typesafe.ai/v1/systemone",
                    ),
                    model_alias: env_string("AIVTUBER_JEV_MODEL", "jev-latest"),
                    ..JevAdapterConfig::default()
                },
                JevApiKey::new(env_optional("AIVTUBER_JEV_API_KEY").unwrap_or_default())?,
            )?;
            let pipeline =
                ReflexPipeline::new(semantic_index, jev_adapter, PolicyConfig::default(), 2)?;
            startup_summary = ProfileSummary::for_profile(
                profile,
                generative.is_some(),
                aivtuber_app::config_fingerprint(profile, generative.is_some()),
            );
            ProductionApp::new(
                security,
                performer,
                visemes,
                Box::new(
                    aivtuber_app::ReflexRoutePlanner::new(pipeline, embedding_provider)
                        .with_template_pack(aivtuber_app::starter_template_pack()),
                ) as Box<dyn RoutePlanner>,
                Box::new(audio),
                avatar,
                stream,
                max_lateness_ms,
            )
        }
    };
    app = app
        .with_adaptation(adaptation)
        .with_telemetry_retention(retention.telemetry_config())
        .with_causal_trace_retention(retention.causal_trace_config());
    if let Some(generative) = generative {
        app = app.with_generation(generative);
    }
    // Issue #55 acceptance: startup states the active routing capabilities
    // without leaking secrets.
    eprintln!("{}", startup_summary.log_line());
    let preload = app.startup()?;
    eprintln!(
        "aivtuber-app ready: indexed={} usable={} preloaded={}",
        preload.indexed.indexed, preload.indexed.usable, preload.preloaded
    );
    report_degraded(app.health());
    eprintln!(
        "raw_ingress: max_record_bytes={} queue_capacity={}",
        raw_ingress_config.max_record_bytes, raw_ingress_config.queue_capacity
    );

    // Issue #56: authenticated local operator control endpoint. The endpoint
    // is opt-in (AIVTUBER_CONTROL_ENDPOINT), local-machine scope only, and
    // terminates in LocalControlIngress so privileged commands can never be
    // forged through the untrusted content ingress.
    let operator_secret = match env_optional("AIVTUBER_CONTROL_SECRET") {
        Some(_) => fixed_control_secret("AIVTUBER_CONTROL_SECRET")?,
        None => {
            let mut generated = [0_u8; 32];
            fill_random(&mut generated);
            generated
        }
    };
    let operator_endpoint = env_string("AIVTUBER_CONTROL_ENDPOINT", "aivtuber-operator-control");
    let (operator_server, mut operator_receiver) =
        OperatorControlServer::new(&operator_endpoint, operator_secret)?;
    let operator_ingress = operator_server.ingress();
    let operator_rate = Arc::new(Mutex::new(OperatorRateLimiter::new(16, 8)));
    {
        let rate = operator_rate.clone();
        let server = operator_server;
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(runtime) = runtime
                && let Err(error) = runtime.block_on(server.serve(rate))
            {
                eprintln!("operator_control: server_error={error}");
            }
        });
    }
    eprintln!("operator_control: endpoint={operator_endpoint} unmute=restart-only");

    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(raw_ingress_config.queue_capacity);
    let raw_ingress_metrics = RawIngressMetrics::default();
    let reader_metrics = raw_ingress_metrics.clone();
    thread::spawn(move || {
        let stdin = io::stdin();
        if let Err(error) =
            pump_bounded_records(stdin.lock(), &sender, raw_ingress_config, &reader_metrics)
        {
            eprintln!("raw_ingress: reader_error={error}");
        }
    });

    let started = Instant::now();
    let mut sequence_seed = 0_u64;
    let mut next_maintenance_ms = 0_u64;
    let mut last_raw_ingress_metrics = RawIngressMetricsSnapshot::default();

    loop {
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(raw) => {
                if raw.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let now_ms = elapsed_ms(started);
                sequence_seed = sequence_seed.saturating_add(1);
                match app.process_content_bytes(&raw, now_ms, sequence_seed) {
                    Ok(outcome) => {
                        eprintln!(
                            "content: admission={:?} playback={}",
                            outcome.admission,
                            outcome
                                .playback
                                .as_ref()
                                .map(|value| value.asset_id.as_str())
                                .unwrap_or("none")
                        );
                    }
                    Err(error) => eprintln!("content rejected: {error}"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The stdin producer closed the pipe. Without this log, an
                // operator who launches the daemon in the background only
                // sees a fast exit after startup — the local operator
                // control endpoint is bound and immediately torn down.
                eprintln!("raw_ingress: stdin closed; shutting down");
                report_raw_ingress_metrics(
                    &raw_ingress_metrics,
                    &mut last_raw_ingress_metrics,
                    true,
                );
                let now_ms = elapsed_ms(started);
                app.shutdown(now_ms);
                break;
            }
        }

        let now_ms = elapsed_ms(started);
        // Issue #56: drain dispatched operator requests on the runtime thread.
        // The control endpoint has its own queue and never contends with the
        // untrusted content ingress, so stop/mute stay responsive even while
        // content is saturated and generation workers are blocked.
        while let Ok(dispatched) = operator_receiver.try_recv() {
            let response =
                apply_dispatched_request(&operator_ingress, &mut app, &dispatched, now_ms);
            let _ = dispatched.respond.send(response);
        }
        app.tick(now_ms);
        if now_ms >= next_maintenance_ms {
            app.maintain_adapters();
            report_raw_ingress_metrics(&raw_ingress_metrics, &mut last_raw_ingress_metrics, false);
            next_maintenance_ms = now_ms.saturating_add(1_000);
        }
    }

    Ok(())
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn report_raw_ingress_metrics(
    metrics: &RawIngressMetrics,
    previous: &mut RawIngressMetricsSnapshot,
    final_report: bool,
) {
    let current = metrics.snapshot();
    let drops_changed = current.dropped_queue_full != previous.dropped_queue_full
        || current.dropped_oversize != previous.dropped_oversize
        || current.read_errors != previous.read_errors;
    if drops_changed || final_report {
        eprintln!(
            "raw_ingress_metrics: enqueued={} dropped_queue_full={} dropped_oversize={} read_errors={} max_retained_record_bytes={}",
            current.enqueued,
            current.dropped_queue_full,
            current.dropped_oversize,
            current.read_errors,
            current.max_retained_record_bytes,
        );
    }
    *previous = current;
}

fn build_runtime_retention_policy() -> Result<RuntimeRetentionPolicy, Box<dyn Error>> {
    let defaults = RuntimeRetentionPolicy::default();
    let policy = RuntimeRetentionPolicy {
        max_telemetry_events: env_usize(
            "AIVTUBER_RETENTION_MAX_TELEMETRY_EVENTS",
            defaults.max_telemetry_events,
        )?,
        max_audit_records: env_usize(
            "AIVTUBER_RETENTION_MAX_AUDIT_RECORDS",
            defaults.max_audit_records,
        )?,
        max_rate_limit_sources: env_usize(
            "AIVTUBER_RETENTION_MAX_RATE_LIMIT_SOURCES",
            defaults.max_rate_limit_sources,
        )?,
        rate_limit_source_ttl_ms: env_u64(
            "AIVTUBER_RETENTION_RATE_LIMIT_SOURCE_TTL_MS",
            defaults.rate_limit_source_ttl_ms,
        )?,
        max_scheduler_history: env_usize(
            "AIVTUBER_RETENTION_MAX_SCHEDULER_HISTORY",
            defaults.max_scheduler_history,
        )?,
        max_hot_generated_assets: env_usize(
            "AIVTUBER_RETENTION_MAX_HOT_GENERATED_ASSETS",
            defaults.max_hot_generated_assets,
        )?,
        max_hot_generated_bytes: env_usize(
            "AIVTUBER_RETENTION_MAX_HOT_GENERATED_BYTES",
            defaults.max_hot_generated_bytes,
        )?,
        max_promotion_metadata: env_usize(
            "AIVTUBER_RETENTION_MAX_PROMOTION_METADATA",
            defaults.max_promotion_metadata,
        )?,
        max_cached_variant_groups: env_usize(
            "AIVTUBER_RETENTION_MAX_CACHED_VARIANT_GROUPS",
            defaults.max_cached_variant_groups,
        )?,
        max_working_memory_entries: env_usize(
            "AIVTUBER_MEMORY_MAX_ENTRIES",
            defaults.max_working_memory_entries,
        )?,
        max_memory_compaction_records: env_usize(
            "AIVTUBER_MEMORY_MAX_COMPACTION_RECORDS",
            defaults.max_memory_compaction_records,
        )?,
        max_adaptation_feedback_assets: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_FEEDBACK_ASSETS",
            defaults.max_adaptation_feedback_assets,
        )?,
        max_adaptation_recent_groups: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_RECENT_GROUPS",
            defaults.max_adaptation_recent_groups,
        )?,
        max_adaptation_decisions: env_usize(
            "AIVTUBER_RETENTION_MAX_ADAPTATION_DECISIONS",
            defaults.max_adaptation_decisions,
        )?,
        generation_queue_capacity: env_usize(
            "AIVTUBER_RETENTION_GENERATION_QUEUE_CAPACITY",
            defaults.generation_queue_capacity,
        )?,
        max_causal_traces: env_usize(
            "AIVTUBER_RETENTION_MAX_CAUSAL_TRACES",
            defaults.max_causal_traces,
        )?,
    };
    if policy.max_telemetry_events == 0
        || policy.max_audit_records == 0
        || policy.max_rate_limit_sources == 0
        || policy.rate_limit_source_ttl_ms == 0
        || policy.max_scheduler_history == 0
        || policy.max_hot_generated_assets == 0
        || policy.max_hot_generated_bytes == 0
        || policy.max_promotion_metadata == 0
        || policy.max_cached_variant_groups == 0
        || policy.max_working_memory_entries == 0
        || policy.max_memory_compaction_records == 0
        || policy.max_adaptation_feedback_assets == 0
        || policy.max_adaptation_recent_groups == 0
        || policy.max_adaptation_decisions == 0
        || policy.generation_queue_capacity == 0
        || policy.max_causal_traces == 0
    {
        return Err(io::Error::other("runtime retention limits must be positive").into());
    }
    Ok(policy)
}

fn build_generative_runtime(
    compatibility: &RuntimeCompatibility,
    retention: RuntimeRetentionPolicy,
) -> Result<Option<GenerativeRuntime>, Box<dyn Error>> {
    if !env_bool("AIVTUBER_GENERATIVE_ENABLED", false)? {
        return Ok(None);
    }

    let mut thinking_config = OpenAiResponsesConfig::default();
    if let Some(endpoint) = env_optional("AIVTUBER_OPENAI_ENDPOINT") {
        thinking_config.endpoint = endpoint;
    }
    if let Some(model) = env_optional("AIVTUBER_OPENAI_MODEL") {
        thinking_config.model_alias = model;
    }
    thinking_config.model_version = env_optional("AIVTUBER_OPENAI_MODEL_VERSION");
    thinking_config.max_output_tokens = env_u64(
        "AIVTUBER_OPENAI_MAX_OUTPUT_TOKENS",
        thinking_config.max_output_tokens,
    )?;
    thinking_config.timeout = Duration::from_millis(env_u64(
        "AIVTUBER_OPENAI_TIMEOUT_MS",
        thinking_config
            .timeout
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    )?);
    let thinking = OpenAiResponsesAdapter::new(
        thinking_config,
        SecretString::new(required_env("AIVTUBER_OPENAI_API_KEY")?),
    )?;

    let tts = NormalizedHttpTtsAdapter::new(
        NormalizedHttpTtsConfig {
            endpoint: required_env("AIVTUBER_TTS_ENDPOINT")?,
            backend_name: env_string("AIVTUBER_TTS_BACKEND_NAME", "normalized-http-tts"),
            model_alias: env_optional("AIVTUBER_TTS_MODEL_ALIAS"),
            model_version: env_optional("AIVTUBER_TTS_MODEL_VERSION"),
            voice_model: compatibility.voice_model.clone(),
            viseme_mapping: compatibility.viseme_mapping.clone(),
            timeout: Duration::from_millis(env_u64("AIVTUBER_TTS_TIMEOUT_MS", 15_000)?),
        },
        env_optional("AIVTUBER_TTS_API_KEY").map(SecretString::new),
    )?;

    let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
        compiler_version: compatibility.compiler_version.clone(),
        avatar_profile: compatibility.avatar_profile.clone(),
        motion_library: compatibility.motion_library.clone(),
        expression_preset: Some(env_string(
            "AIVTUBER_GENERATED_EXPRESSION_PRESET",
            "speaking.neutral",
        )),
        expression_intensity: 0.4,
    })?;

    let pipeline = GenerativePipeline::new(Arc::new(thinking), Arc::new(tts), compiler);
    let runtime =
        GenerativeRuntime::with_execution_config(pipeline, retention.generation_execution_config())
            .map_err(io::Error::other)?;
    Ok(Some(runtime))
}

fn build_adaptation_runtime(
    retention: RuntimeRetentionPolicy,
) -> Result<AdaptationRuntime, Box<dyn Error>> {
    let seed = env_u64("AIVTUBER_ADAPTATION_SEED", 0)?;
    let memory_config = retention.working_memory_config(WorkingMemoryConfig {
        working_ttl_ms: env_u64("AIVTUBER_MEMORY_WORKING_TTL_MS", 15 * 60 * 1_000)?,
        durable_ttl_ms: env_u64("AIVTUBER_MEMORY_DURABLE_TTL_MS", 24 * 60 * 60 * 1_000)?,
        max_claim_bytes: env_usize("AIVTUBER_MEMORY_MAX_CLAIM_BYTES", 1_024)?,
        max_topic_bytes: env_usize("AIVTUBER_MEMORY_MAX_TOPIC_BYTES", 128)?,
        ..WorkingMemoryConfig::default()
    });
    let pseudonym_key = fixed_hex_key("AIVTUBER_MEMORY_PSEUDONYM_KEY_HEX")?;
    let pseudonymizer = ActorPseudonymizer::new(
        env_string("AIVTUBER_MEMORY_PSEUDONYM_KEY_VERSION", "v1"),
        pseudonym_key,
    )?;
    let memory = WorkingMemory::new(memory_config, pseudonymizer)?;
    let engine = AdaptationEngine::with_retention(
        PromotionPolicy {
            min_uses: env_u64("AIVTUBER_PROMOTION_MIN_USES", 3)?,
            min_quality_labels: env_u64("AIVTUBER_PROMOTION_MIN_QUALITY_LABELS", 2)?,
            min_quality_ratio: env_f64("AIVTUBER_PROMOTION_MIN_QUALITY_RATIO", 0.8)?,
            invalidate_after_negative_labels: env_u64(
                "AIVTUBER_PROMOTION_INVALIDATE_NEGATIVE_LABELS",
                3,
            )?,
            recent_variant_window: env_usize("AIVTUBER_ADAPTATION_RECENT_VARIANT_WINDOW", 2)?,
        },
        env_string("AIVTUBER_ADAPTATION_POLICY_VERSION", "adaptation-v1"),
        seed,
        retention.adaptation_config(),
    )?;
    Ok(AdaptationRuntime::new(memory, engine))
}

fn build_avatar_output() -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
    if !env_bool("AIVTUBER_VTS_ENABLED", false)? {
        return Ok(Box::new(NoopAvatarOutput));
    }

    let mut config = VTubeStudioConfig::default();
    config.endpoint = env_string("AIVTUBER_VTS_ENDPOINT", &config.endpoint);
    config.plugin_name = env_string("AIVTUBER_VTS_PLUGIN_NAME", &config.plugin_name);
    config.plugin_developer = env_string("AIVTUBER_VTS_PLUGIN_DEVELOPER", &config.plugin_developer);
    config.authentication_token = env_optional("AIVTUBER_VTS_TOKEN").map(SecretString::new);

    let adapter = VTubeStudioAdapter::new(config)?;
    let authority = runtime_avatar_authority()?;
    Ok(Box::new(VtsAvatarOutput::new(
        adapter,
        authority,
        VtsPlaybackConfig::default(),
    )))
}

fn build_stream_output() -> Result<Box<dyn StreamOutput>, Box<dyn Error>> {
    if !env_bool("AIVTUBER_OBS_ENABLED", false)? {
        return Ok(Box::new(NoopStreamOutput));
    }

    let mut config = ObsWebSocketConfig::default();
    config.endpoint = env_string("AIVTUBER_OBS_ENDPOINT", &config.endpoint);
    config.password = env_optional("AIVTUBER_OBS_PASSWORD").map(SecretString::new);
    let adapter = ObsWebSocketAdapter::new(config)?;
    Ok(Box::new(ObsStreamOutput::new(adapter)))
}

fn runtime_avatar_authority() -> Result<aivtuber_domain::AuthenticatedControl, Box<dyn Error>> {
    let secret = fixed_control_secret("AIVTUBER_CONTROL_SECRET")?;
    let ingress = LocalControlIngress::new(
        "local-runtime",
        "runtime:motor",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([Capability::AvatarControl]),
        ControlSecret::new(secret),
    )?;
    let command = ingress.authenticate(
        OperatorCommandInput {
            event_id: "bootstrap-avatar-authority".to_owned(),
            correlation_id: "bootstrap-runtime".to_owned(),
            sequence: 0,
            observed_at: "1970-01-01T00:00:00Z".to_owned(),
            action: "avatar.control".to_owned(),
            payload: BTreeMap::new(),
        },
        &secret,
    )?;
    Ok(command.authority().clone())
}

fn fixed_hex_key(name: &str) -> Result<[u8; 32], Box<dyn Error>> {
    let value = env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required for actor pseudonymization"),
        )
    })?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain exactly 64 hexadecimal characters"),
        )
        .into());
    }

    let mut key = [0_u8; 32];
    for (index, slot) in key.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = u8::from_str_radix(&value[offset..offset + 2], 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} contains invalid hexadecimal key material: {error}"),
            )
        })?;
    }
    Ok(key)
}

fn fill_random(bytes: &mut [u8; 32]) {
    getrandom::fill(bytes).expect("OS entropy source failed");
}

fn fixed_control_secret(name: &str) -> Result<[u8; 32], Box<dyn Error>> {
    let value = env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required when VTube Studio output is enabled"),
        )
    })?;
    let bytes = value.as_bytes();
    if bytes.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain exactly 32 UTF-8 bytes"),
        )
        .into());
    }

    let mut secret = [0_u8; 32];
    secret.copy_from_slice(bytes);
    Ok(secret)
}

fn report_degraded(health: &AdapterHealth) {
    if let Some(error) = &health.avatar_error {
        eprintln!("VTube Studio degraded: {error}");
    }
    if let Some(error) = &health.stream_error {
        eprintln!("OBS degraded: {error}");
    }
}

fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    env_optional(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required when generative fallback is enabled"),
        )
        .into()
    })
}

fn env_optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_string(name: &str, default: &str) -> String {
    env_optional(name).unwrap_or_else(|| default.to_owned())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env_optional(name).map(PathBuf::from)
}

fn env_bool(name: &str, default: bool) -> Result<bool, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be true/false, 1/0, yes/no, or on/off"),
        )
        .into()),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    value.parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an unsigned integer: {error}"),
        )
        .into()
    })
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    let value = env_u64(name, default as u64)?;
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is too large for this platform"),
        )
        .into()
    })
}

fn env_f64(name: &str, default: f64) -> Result<f64, Box<dyn Error>> {
    let Some(value) = env_optional(name) else {
        return Ok(default);
    };
    value.parse::<f64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a number: {error}"),
        )
        .into()
    })
}
