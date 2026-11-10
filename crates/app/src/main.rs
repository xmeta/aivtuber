use aivtuber_adaptation::{
    ActorPseudonymizer, AdaptationEngine, PromotionPolicy, WorkingMemory, WorkingMemoryConfig,
};
use aivtuber_adapters::speech_tts::pins::{
    SUPERTONIC_3_INT8, SUPERTONIC_MODEL_ALIAS, VOICE_LICENCE_NOTICE,
};
use aivtuber_adapters::speech_tts::supervisor::{LivenessGatedTts, TtsSidecar};
use aivtuber_adapters::{
    LocalAvatarConfig, LocalAvatarServer, NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig,
    ObsWebSocketAdapter, ObsWebSocketConfig, OpenAiResponsesAdapter, OpenAiResponsesConfig,
    ProcessAudioConfig, ProcessAudioPlayer, SecretString, StreamLimits, SystemHostResolver,
    ThinkingAuthentication, VTubeStudioAdapter, VTubeStudioConfig, endpoint_origin,
    verify_loopback_endpoint,
};
use aivtuber_app::{
    AdaptationRuntime, AdapterHealth, AvatarOutput, BargeInConfig, BargeInMetrics,
    CompositionProfile, GenerationFallbackCounts, GenerativeBudgetPolicy, GenerativeRuntime,
    IntentRoutePlanner, IntentShadowPolicy, LocalAvatarOutput, LocalRoutePlanner,
    MemoryWriterConfig, MemoryWriterMetrics, NoopAvatarOutput, NoopStreamOutput, ObsStreamOutput,
    OperatorControlServer, OperatorRateLimiter, PhraseMetrics, PhraseMode, ProductionApp,
    ProfileSummary, RawIngressConfig, RawIngressMetrics, RawIngressMetricsSnapshot, RoutePlanner,
    RuntimeRetentionPolicy, SHADOW_ORCHESTRATOR_SCHEMA_VERSION, ShadowOrchestratorConfig,
    ShadowPolicyIdentity, ShadowingRoutePlanner, SpeechConversationConfig,
    SpeechConversationMetrics, SpeechFloorMetrics, SpeechSidecar, SpeechSidecarConfig,
    SpeechSidecarStats, StreamOutput, TtsSidecarSettings, VoicevoxBridge, VoicevoxSettings,
    VtsAvatarOutput, VtsPlaybackConfig, apply_dispatched_request, audit_pack_readiness,
    pump_bounded_records, stt_sidecar_args,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EventKind, FloorConfig, LocalControlIngress,
    OperatorCommandInput,
};
use aivtuber_generative::{
    GenerativePipeline, PerformanceCompiler, PerformanceCompilerConfig, PhraseConfig,
    SpeechLexicon, StreamPolicy,
};
use aivtuber_reflex::{JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig, ReflexPipeline};
use aivtuber_runtime::{
    CachedPerformer, CachedPlaybackConfig, IngressPolicy, LocalVisemeStore, SecurityRuntime,
    SecurityRuntimeConfig, SpeechObservationConfig, SpeechObservationTracker,
    SpeechTranscriptConfig, SpeechTranscriptTracker,
};
use aivtuber_scheduler::{Scheduler, SchedulerConfig};
use aivtuber_telemetry::SecretRedactor;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    // The player resolves `audio://` references under this root, and a
    // supervised TTS sidecar is told to write its files under the same one.
    let audio_root = env_path("AIVTUBER_AUDIO_ROOT").unwrap_or_else(|| pack_root.join("audio"));

    // Issue #55: the composition profile — not disconnected env flags —
    // names the active route graph. Resolve and validate before composing.
    let profile = match env_optional("AIVTUBER_MODE") {
        Some(mode) => CompositionProfile::parse(&mode)?,
        None => CompositionProfile::Cached,
    };
    // #295: optional VOICEVOX backend through the in-process loopback bridge.
    // Held for the whole run; dropping it stops the listener. Resolved before
    // the sidecar so a conflicting setting fails before a child is spawned.
    let voicevox_bridge = start_voicevox_bridge(profile, &audio_root, &pack_root)?;
    // #220 S2b: optional supervised local TTS sidecar (local profile only).
    let mut tts_sidecar = start_tts_sidecar(profile, &audio_root, &pack_root)?;
    let generative_configured = build_generative_runtime(
        &compatibility,
        retention,
        profile,
        tts_sidecar.as_ref().map(|handle| handle.alive.clone()),
        voicevox_bridge.as_ref(),
    )?;
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
    let mut security = SecurityRuntime::new(
        security_config,
        scheduler_config,
        SecretRedactor::default(),
        Some("safe cached reaction".to_owned()),
    )?;
    // Issue #257: operator-owned ceiling on what the stdin content ingress may
    // declare. Unset = unchanged behaviour (everything envelope validation
    // admits). Records above it are rejected and counted, never rewritten.
    security.set_ingress_policy(IngressPolicy::parse(
        env_optional("AIVTUBER_INGRESS_MAX_TRUST").as_deref(),
        env_optional("AIVTUBER_INGRESS_SOURCE_CLASSES").as_deref(),
    )?);

    let mut audio_config = ProcessAudioConfig {
        root: audio_root.clone(),
        ..ProcessAudioConfig::default()
    };
    if let Some(program) = env_optional("AIVTUBER_AUDIO_PROGRAM") {
        audio_config.program = program;
    }
    let audio = ProcessAudioPlayer::new(audio_config)?;

    let avatar = build_avatar_output()?;
    let stream = build_stream_output()?;
    let max_lateness_ms = env_u64("AIVTUBER_MAX_DISPATCH_LATENESS_MS", 250)?;

    // Compose the active planner from the selected profile (issue #55): the
    // mode unambiguously determines the routing stack. The planner is built
    // first so #164 can optionally wrap it in bounded shadow orchestration
    // before the app is constructed.
    let startup_summary = ProfileSummary::for_profile(
        profile,
        generative.is_some(),
        aivtuber_app::config_fingerprint_with_phrases(
            profile,
            generative.is_some(),
            generative
                .as_ref()
                .is_some_and(|generative| generative.phrase_mode().is_phrased()),
        ),
    );
    let active_planner: Box<dyn RoutePlanner> = match profile {
        CompositionProfile::Cached => Box::new(IntentRoutePlanner) as Box<dyn RoutePlanner>,
        // #220 S1: deterministic routing, no decision model, no Jev.
        CompositionProfile::Local => Box::new(LocalRoutePlanner) as Box<dyn RoutePlanner>,
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
            Box::new(
                aivtuber_app::ReflexRoutePlanner::new(pipeline, embedding_provider)
                    .with_template_pack(aivtuber_app::starter_template_pack()),
            ) as Box<dyn RoutePlanner>
        }
    };
    // #164: bounded shadow orchestration. Disabled by default; the shadow half
    // can never become executable output and never changes the active route.
    let (active_planner, shadow_summary) =
        wrap_with_shadow_orchestration(active_planner, &startup_summary)?;
    let mut app = ProductionApp::new(
        security,
        performer,
        visemes,
        active_planner,
        Box::new(audio),
        avatar,
        stream,
        max_lateness_ms,
    );
    app = app
        .with_adaptation(adaptation)
        .with_telemetry_retention(retention.telemetry_config())
        .with_causal_trace_retention(retention.causal_trace_config())
        // #167: operator diagnostics report the same profile/fingerprint the
        // startup summary prints, never placeholders.
        .with_composition_identity(
            &startup_summary.profile,
            &startup_summary.config_fingerprint,
        );
    if let Some(generative) = generative {
        app = app.with_generation(generative);
    }
    // #306: the production memory writer is opt-in. Default off writes nothing.
    if env_bool("AIVTUBER_MEMORY_WRITER", false)? {
        app = app
            .with_memory_writer(MemoryWriterConfig {
                max_live_entries: env_usize(
                    "AIVTUBER_MEMORY_WRITER_MAX_LIVE_ENTRIES",
                    MemoryWriterConfig::default().max_live_entries,
                )?,
                ..MemoryWriterConfig::default()
            })
            .map_err(|error| format!("AIVTUBER_MEMORY_WRITER: {error}"))?;
    }
    // Issue #69: generative resource budgets are opt-in (disabled by default
    // so existing deployments keep identical behavior). All limits are
    // optional; an enabled budget with no limits records nothing enforced.
    if env_bool("AIVTUBER_BUDGET_ENABLED", false)? {
        let opt_u64 = |name: &str| -> Result<Option<u64>, Box<dyn Error>> {
            if env_optional(name).is_some() {
                Ok(Some(env_u64(name, 1)?))
            } else {
                Ok(None)
            }
        };
        let opt_u32 = |name: &str| -> Result<Option<u32>, Box<dyn Error>> {
            match opt_u64(name)? {
                Some(value) => Ok(Some(u32::try_from(value).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is too large"))
                })?)),
                None => Ok(None),
            }
        };
        let policy = GenerativeBudgetPolicy {
            enabled: true,
            max_concurrent_generations: opt_u32("AIVTUBER_BUDGET_MAX_CONCURRENT_GENERATIONS")?,
            max_llm_calls_per_interval: opt_u32("AIVTUBER_BUDGET_MAX_LLM_CALLS")?,
            max_tts_calls_per_interval: opt_u32("AIVTUBER_BUDGET_MAX_TTS_CALLS")?,
            max_llm_units_per_interval: opt_u64("AIVTUBER_BUDGET_MAX_LLM_UNITS")?,
            max_tts_chars_per_interval: opt_u64("AIVTUBER_BUDGET_MAX_TTS_CHARS")?,
            max_estimated_cost_milliunits_per_interval: opt_u64(
                "AIVTUBER_BUDGET_MAX_COST_MILLIUNITS",
            )?,
            pricing_input_version: env_optional("AIVTUBER_BUDGET_PRICING_INPUT_VERSION")
                .map(|version| Box::leak(version.into_boxed_str()) as &'static str),
            interval_ms: env_u64("AIVTUBER_BUDGET_INTERVAL_MS", 60_000)?,
            high_priority_reserve_percent: env_u64("AIVTUBER_BUDGET_RESERVE_PERCENT", 0)?.min(100)
                as u32,
            charge_shadow_to_budget: env_bool("AIVTUBER_BUDGET_CHARGE_SHADOW", false)?,
        };
        let policy = policy.validate().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "AIVTUBER_BUDGET_* configuration invalid: {} ({})",
                    error,
                    error.field()
                ),
            )
        })?;
        eprintln!(
            "generative_budget: enabled interval_ms={} reserve_percent={}",
            policy.interval_ms, policy.high_priority_reserve_percent
        );
        app = app.with_generative_budget_policy(policy);
    }
    // Issue #55 acceptance: startup states the active routing capabilities
    // without leaking secrets.
    eprintln!("{}", startup_summary.log_line());
    if let Some(line) = shadow_summary {
        eprintln!("{line}");
    }
    let preload = app.startup()?;
    eprintln!(
        "aivtuber-app ready: indexed={} usable={} preloaded={}",
        preload.indexed.indexed, preload.indexed.usable, preload.preloaded
    );
    // #289: one counted warning when cached reactions would be silent or skipped.
    for line in audit_pack_readiness(app.performer().assets(), &audio_root)
        .warning_lines(compatibility.voice_model.as_deref(), &audio_root)
    {
        eprintln!("{line}");
    }
    // #222 S2 (ADR-0004 D-4): operator-forced invalidation. Operator
    // configuration only, read once at start like the lexicon itself.
    for surface in env_string("AIVTUBER_SPEECH_INVALIDATE", "")
        .split(',')
        .map(str::trim)
        .filter(|surface| !surface.is_empty())
    {
        let report = app.invalidate_speech_surface(surface);
        eprintln!(
            "speech invalidate {surface:?}: scanned={} forced={} skipped_not_generated={} truncated={}",
            report.scanned, report.forced, report.skipped_not_generated, report.truncated
        );
    }
    report_degraded(app.avatar_output_name(), app.health());
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
    // Issue #92 S2: optional local speech-activity sidecar. Off unless
    // AIVTUBER_SPEECH_SIDECAR names the program. Its observations feed the S1
    // tracker only (untrusted content-plane data, no scheduler or output
    // path); a sidecar that cannot start or later fails degrades to no speech
    // observations and never blocks this loop.
    let mut speech_sidecar: Option<SpeechSidecar> = None;
    let mut speech_conversation_enabled = false;
    let mut speech_floor_enabled = false;
    let mut speech_barge_in_enabled = false;
    let mut last_floor_metrics = SpeechFloorMetrics::default();
    let mut last_barge_in_metrics = BargeInMetrics::default();
    let mut last_phrase_metrics = PhraseMetrics::default();
    let mut last_memory_writer_metrics = MemoryWriterMetrics::default();
    let mut last_projection_metrics = aivtuber_app::SpeechProjectionMetrics::default();
    let mut last_phrase_response_logged: Option<u64> = None;
    let mut last_conversation_metrics = SpeechConversationMetrics::default();
    if let Some(program) = env_optional("AIVTUBER_SPEECH_SIDECAR") {
        let mut tracker = SpeechObservationTracker::new(
            SpeechObservationConfig::default(),
            IngressPolicy::parse(
                env_optional("AIVTUBER_INGRESS_MAX_TRUST").as_deref(),
                env_optional("AIVTUBER_INGRESS_SOURCE_CLASSES").as_deref(),
            )?,
        )?;
        let mut sidecar_args: Vec<String> = env_optional("AIVTUBER_SPEECH_SIDECAR_ARGS")
            .map(|args| args.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        // Issue #92 S3b: optional local Japanese transcripts. The sidecar
        // spawns and supervises the worker; transcripts come back on the same
        // pipe into their own tracker (untrusted content, partials reduced to
        // a floor-hold observation, only finals surfaced). Off unless
        // AIVTUBER_SPEECH_STT_WORKER names the worker program.
        if let Some(worker) = env_optional("AIVTUBER_SPEECH_STT_WORKER") {
            let model = env_string("AIVTUBER_SPEECH_STT_MODEL", "k2v2-int8");
            let model_dir = env_optional("AIVTUBER_SPEECH_STT_MODEL_DIR").ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AIVTUBER_SPEECH_STT_MODEL_DIR is required with AIVTUBER_SPEECH_STT_WORKER",
                )
            })?;
            let threads = match env_optional("AIVTUBER_SPEECH_STT_THREADS") {
                Some(_) => Some(env_u64("AIVTUBER_SPEECH_STT_THREADS", 4)?),
                None => None,
            };
            sidecar_args.extend(stt_sidecar_args(&worker, &model, &model_dir, threads));
            let defaults = SpeechTranscriptConfig::default();
            let transcript_config = SpeechTranscriptConfig {
                partial_max_age_ms: env_u64(
                    "AIVTUBER_SPEECH_STT_PARTIAL_MAX_AGE_MS",
                    defaults.partial_max_age_ms,
                )?,
                final_max_age_ms: env_u64(
                    "AIVTUBER_SPEECH_STT_FINAL_MAX_AGE_MS",
                    defaults.final_max_age_ms,
                )?,
                floor_hold_ms: env_u64(
                    "AIVTUBER_SPEECH_STT_FLOOR_HOLD_MS",
                    defaults.floor_hold_ms,
                )?,
                ..defaults
            };
            app = app.with_speech_transcripts(SpeechTranscriptTracker::new(
                transcript_config,
                IngressPolicy::parse(
                    env_optional("AIVTUBER_INGRESS_MAX_TRUST").as_deref(),
                    env_optional("AIVTUBER_INGRESS_SOURCE_CLASSES").as_deref(),
                )?,
            )?);
            // Issue #92 S3c: route spoken finals into the conversation input.
            // Off unless AIVTUBER_SPEECH_CONVERSATION is true. A final then
            // takes the same normalizer path as chat (untrusted `speech`
            // content, the ingress ceiling and rate limit apply).
            if env_bool("AIVTUBER_SPEECH_CONVERSATION", false)? {
                let defaults = SpeechConversationConfig::default();
                let conversation = SpeechConversationConfig {
                    max_finals_per_tick: usize::try_from(env_u64(
                        "AIVTUBER_SPEECH_CONVERSATION_MAX_FINALS_PER_TICK",
                        defaults.max_finals_per_tick as u64,
                    )?)
                    .unwrap_or(usize::MAX),
                    max_text_chars: usize::try_from(env_u64(
                        "AIVTUBER_SPEECH_CONVERSATION_MAX_TEXT_CHARS",
                        defaults.max_text_chars as u64,
                    )?)
                    .unwrap_or(usize::MAX),
                };
                app = app.with_speech_conversation(conversation)?;
                speech_conversation_enabled = true;
                // Issue #81 S1: deterministic conversational floor. Off unless
                // AIVTUBER_SPEECH_FLOOR is true. A final then waits, bounded
                // and counted, while the user holds the floor. Needs the
                // conversation route; touches no scheduler or operator path.
                if env_bool("AIVTUBER_SPEECH_FLOOR", false)? {
                    let defaults = FloorConfig::default();
                    let floor = FloorConfig {
                        take_turn_silence_ms: env_u64(
                            "AIVTUBER_SPEECH_FLOOR_TAKE_TURN_SILENCE_MS",
                            defaults.take_turn_silence_ms,
                        )?,
                        max_defer_ms: env_u64(
                            "AIVTUBER_SPEECH_FLOOR_MAX_DEFER_MS",
                            defaults.max_defer_ms,
                        )?,
                        max_deferred: usize::try_from(env_u64(
                            "AIVTUBER_SPEECH_FLOOR_MAX_DEFERRED",
                            defaults.max_deferred as u64,
                        )?)
                        .unwrap_or(usize::MAX),
                        ..defaults
                    };
                    app = app.with_speech_floor(floor)?;
                    speech_floor_enabled = true;
                    eprintln!(
                        "speech_floor: enabled take_turn_silence_ms={} max_defer_ms={} max_deferred={}",
                        floor.take_turn_silence_ms, floor.max_defer_ms, floor.max_deferred
                    );
                    // Issue #81 S2: a confirmed barge-in stops the performer's
                    // speech with a bounded fade. Off unless
                    // AIVTUBER_SPEECH_BARGE_IN is true; layered on the floor.
                    if env_bool("AIVTUBER_SPEECH_BARGE_IN", false)? {
                        let fade_ms = u32::try_from(env_u64(
                            "AIVTUBER_SPEECH_BARGE_IN_FADE_MS",
                            u64::from(BargeInConfig::default().fade_ms),
                        )?)
                        .unwrap_or(u32::MAX);
                        app = app.with_barge_in(BargeInConfig { fade_ms })?;
                        speech_barge_in_enabled = true;
                        eprintln!("speech_barge_in: enabled fade_ms={fade_ms}");
                    }
                }
                eprintln!(
                    "speech_conversation: enabled max_finals_per_tick={} max_text_chars={}",
                    conversation.max_finals_per_tick, conversation.max_text_chars
                );
            }
        } else if env_bool("AIVTUBER_SPEECH_CONVERSATION", false)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AIVTUBER_SPEECH_CONVERSATION requires AIVTUBER_SPEECH_STT_WORKER",
            )
            .into());
        }
        if env_bool("AIVTUBER_SPEECH_FLOOR", false)? && !speech_floor_enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AIVTUBER_SPEECH_FLOOR requires AIVTUBER_SPEECH_CONVERSATION",
            )
            .into());
        }
        if env_bool("AIVTUBER_SPEECH_BARGE_IN", false)? && !speech_barge_in_enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AIVTUBER_SPEECH_BARGE_IN requires AIVTUBER_SPEECH_FLOOR",
            )
            .into());
        }
        // Issue #81 S2: with the floor on, the sidecar reports feed liveness
        // so a stalled feed is told from a quiet room (ADR-0005 D6). An
        // explicit --feed-heartbeat-ms in the sidecar arguments wins.
        if speech_floor_enabled && !sidecar_args.iter().any(|arg| arg == "--feed-heartbeat-ms") {
            sidecar_args.push("--feed-heartbeat-ms".to_owned());
            sidecar_args.push("64".to_owned());
        }
        let mut config = SpeechSidecarConfig::new(program, sidecar_args);
        config.resync_interval =
            Duration::from_millis(env_u64("AIVTUBER_SPEECH_SIDECAR_RESYNC_MS", 5_000)?);
        match SpeechSidecar::spawn(config, started) {
            Ok(sidecar) => {
                eprintln!("speech_sidecar: started");
                speech_sidecar = Some(sidecar);
            }
            Err(error) => {
                eprintln!("speech_sidecar: could not start: {error}; no speech observations");
                tracker.record_provider_unavailable();
            }
        }
        app = app.with_speech_observations(tracker);
    }
    let mut last_speech_stats = SpeechSidecarStats::default();
    let mut last_generation_fallbacks = GenerationFallbackCounts::default();
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
                eprintln!(
                    "raw_ingress: stdin closed; shutting down ingress_policy_rejected={}",
                    app.security().ingress_policy_rejected()
                );
                report_raw_ingress_metrics(
                    &raw_ingress_metrics,
                    &mut last_raw_ingress_metrics,
                    true,
                );
                let now_ms = elapsed_ms(started);
                app.shutdown(now_ms);
                if let Some(sidecar) = speech_sidecar.as_mut() {
                    sidecar.shutdown();
                }
                break;
            }
        }

        if let Some(handle) = tts_sidecar.as_mut() {
            handle.poll();
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
        if let Some(sidecar) = speech_sidecar.as_mut()
            && let (Some(tracker), transcripts) = app.speech_trackers_mut()
        {
            sidecar.poll_with_transcripts(tracker, transcripts, now_ms, |_, _| {}, |_, _| {});
        }
        // Issue #81 S2: feed liveness first, then the confirmed barge-in, so a
        // stalled feed is already stale when the barge-in is judged. Only
        // lengths and counts are logged.
        if let Some(beat_ms) = speech_sidecar
            .as_ref()
            .and_then(SpeechSidecar::feed_heartbeat_ms)
        {
            app.note_speech_feed_heartbeat(beat_ms);
        }
        if speech_barge_in_enabled && let Some(record) = app.service_barge_in(now_ms) {
            eprintln!(
                "speech_barge_in: decision_seq={} at_ms={} effect={:?} cancelled_playing={} cancelled_queued={} fade_ms={} audio={:?} audio_error={} avatar_reset={:?} decision_to_stop_micros={}",
                record.decision_seq,
                record.decided_at_ms,
                record.effect,
                record.cancelled_playing,
                record.cancelled_queued,
                record.fade_ms,
                record.audio,
                record.audio_error,
                record.avatar_reset,
                record.decision_to_stop_micros
            );
        }
        // Finals are the only transcript text that leaves the tracker. With
        // AIVTUBER_SPEECH_CONVERSATION they go through process_content_bytes
        // (the chat path); otherwise they are drained so the bounded queue
        // never wedges. Only lengths are logged (what a person said is never
        // written to a log). Partials only ever hold the floor.
        if speech_conversation_enabled {
            sequence_seed = sequence_seed.saturating_add(16);
            for report in app.route_spoken_finals(now_ms, sequence_seed) {
                let chars = report
                    .event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map_or(0, |text| text.chars().count());
                match &report.result {
                    Ok(outcome) => eprintln!(
                        "speech_conversation: final chars={chars} final_age_ms={} route_micros={} admission={:?} playback={}",
                        report.final_age_ms,
                        report.route_micros,
                        outcome.admission,
                        outcome
                            .playback
                            .as_ref()
                            .map(|value| value.asset_id.as_str())
                            .unwrap_or("none")
                    ),
                    Err(error) => {
                        eprintln!("speech_conversation: final chars={chars} rejected: {error}")
                    }
                }
            }
        } else if let Some(tracker) = app.speech_transcripts_mut() {
            for utterance in tracker.take_finals(16) {
                eprintln!(
                    "speech_transcript: final segment={} chars={} (conversation routing off)",
                    utterance.segment_id,
                    utterance.text.chars().count()
                );
            }
        }
        app.tick(now_ms);
        report_generation_fallbacks(
            app.generation_fallback_counts(),
            &mut last_generation_fallbacks,
        );
        if now_ms >= next_maintenance_ms {
            app.maintain_adapters();
            if let Some(tracker) = app.speech_observations_mut() {
                tracker.sweep(now_ms);
            }
            if let Some(tracker) = app.speech_transcripts_mut() {
                tracker.sweep(now_ms);
            }
            if let Some(sidecar) = speech_sidecar.as_ref() {
                let stats = sidecar.stats();
                if stats.lines_dropped_queue_full != last_speech_stats.lines_dropped_queue_full
                    || stats.lines_dropped_oversize != last_speech_stats.lines_dropped_oversize
                    || stats.lines_rejected != last_speech_stats.lines_rejected
                {
                    eprintln!(
                        "speech_sidecar_metrics: ingested={} rejected={} dropped_queue_full={} dropped_oversize={}",
                        stats.batches_ingested,
                        stats.lines_rejected,
                        stats.lines_dropped_queue_full,
                        stats.lines_dropped_oversize
                    );
                    last_speech_stats = stats;
                }
            }
            if let Some(metrics) = app.speech_conversation_metrics()
                && metrics != last_conversation_metrics
            {
                eprintln!(
                    "speech_conversation_metrics: finals={} admitted={} dropped_oversize={} dropped_blank={} dropped_by_runtime={} route_errors={}",
                    metrics.finals_seen,
                    metrics.admitted,
                    metrics.dropped_oversize,
                    metrics.dropped_blank,
                    metrics.dropped_by_runtime,
                    metrics.route_errors
                );
                last_conversation_metrics = metrics;
            }
            if let Some(metrics) = app.speech_floor_metrics()
                && (metrics.policy != last_floor_metrics.policy
                    || metrics.deferred != last_floor_metrics.deferred
                    || metrics.dropped_expired != last_floor_metrics.dropped_expired
                    || metrics.dropped_overflow != last_floor_metrics.dropped_overflow)
            {
                eprintln!(
                    "speech_floor_metrics: decisions={} take_turn={} wait={} hold={} yield_recorded={} fallback_take_turn={} deferred={} released={} dropped_expired={} dropped_overflow={} queued={}",
                    metrics.policy.decisions,
                    metrics.policy.take_turn,
                    metrics.policy.wait,
                    metrics.policy.hold,
                    metrics.policy.yielded,
                    metrics.policy.fallback_take_turn,
                    metrics.deferred,
                    metrics.released_after_deferral,
                    metrics.dropped_expired,
                    metrics.dropped_overflow,
                    metrics.queued
                );
                last_floor_metrics = metrics;
            }
            if let Some(metrics) = app.speech_barge_in_metrics()
                && metrics != last_barge_in_metrics
            {
                eprintln!(
                    "speech_barge_in_metrics: confirmed={} stopped={} nothing_to_stop={} audio_faded={} audio_hard_stopped={} audio_stop_unsupported={} audio_stop_errors={} avatar_resets={} avatar_reset_errors={} max_decision_to_stop_micros={}",
                    metrics.confirmed,
                    metrics.stopped,
                    metrics.nothing_to_stop,
                    metrics.audio_faded,
                    metrics.audio_hard_stopped,
                    metrics.audio_stop_unsupported,
                    metrics.audio_stop_errors,
                    metrics.avatar_resets,
                    metrics.avatar_reset_errors,
                    metrics.max_decision_to_stop_micros
                );
                last_barge_in_metrics = metrics;
            }
            let projection_metrics = app.speech_projection_metrics();
            if projection_metrics != last_projection_metrics {
                eprintln!(
                    "speech_projection: replies={} lexicon_hits={} projection_passthrough_latin={}",
                    projection_metrics.replies,
                    projection_metrics.lexicon_hits,
                    projection_metrics.passthrough_latin
                );
                last_projection_metrics = projection_metrics;
            }
            if let Some(metrics) = app.memory_writer_metrics()
                && metrics != last_memory_writer_metrics
            {
                eprintln!(
                    "memory_writer_metrics: considered={} written={} retrieved_used={} expired={} removed_early={} live={} rejected_viewer_attributed={} rejected_oversize={} rejected_quota={} rejected_duplicate={} rejected_gate={} rejected_store={}",
                    metrics.texts_considered,
                    metrics.written,
                    metrics.retrieved_used,
                    metrics.expired,
                    metrics.removed_early,
                    metrics.live,
                    metrics.rejected_viewer_attributed,
                    metrics.rejected_oversize,
                    metrics.rejected_quota,
                    metrics.rejected_duplicate,
                    metrics.rejected_gate,
                    metrics.rejected_store
                );
                last_memory_writer_metrics = metrics;
            }
            let phrase_metrics = app.phrase_metrics();
            if phrase_metrics != last_phrase_metrics {
                eprintln!(
                    "phrase_metrics: responses={} completed={} partial={} phrases_committed={} gate_rejected_before_first={} gate_rejected_after_commit={} scheduler_rejected={} synthesis_failed_after_commit={} cancelled_after_commit={} underruns={} max_gap_ms={} max_synthesis_ms={} late_dropped={}",
                    phrase_metrics.responses,
                    phrase_metrics.completed,
                    phrase_metrics.partial,
                    phrase_metrics.phrases_committed,
                    phrase_metrics.gate_rejected_before_first_commit,
                    phrase_metrics.gate_rejected_after_commit,
                    phrase_metrics.scheduler_rejected,
                    phrase_metrics.synthesis_failed_after_commit,
                    phrase_metrics.cancelled_after_commit,
                    phrase_metrics.underruns,
                    phrase_metrics.max_gap_ms,
                    phrase_metrics.max_synthesis_ms,
                    phrase_metrics.late_phrases_dropped
                );
                last_phrase_metrics = phrase_metrics;
            }
            for record in app.recent_phrase_responses() {
                if last_phrase_response_logged.is_some_and(|logged| record.response_id <= logged) {
                    continue;
                }
                eprintln!(
                    "phrase_response: response={} event={} planned={} committed={} end={:?} commits=[{}]",
                    record.response_id,
                    record.event_id,
                    record.planned_phrases,
                    record.commit_horizon(),
                    record.end,
                    record
                        .commits
                        .iter()
                        .map(|commit| format!(
                            "{}@{}..{}(gap {}, synth {}ms)",
                            commit.seq,
                            commit.start_at_ms,
                            commit.end_at_ms,
                            commit.gap_before_ms,
                            commit.synthesis_ms
                        ))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                last_phrase_response_logged = Some(record.response_id);
            }
            report_raw_ingress_metrics(&raw_ingress_metrics, &mut last_raw_ingress_metrics, false);
            next_maintenance_ms = now_ms.saturating_add(1_000);
        }
    }

    Ok(())
}

/// One line per newly counted generation fallback, naming the typed reason
/// (`timeout`, `unavailable`, `policy_override`, ...) from the closed
/// fallback vocabulary. Bounded by construction: it prints only when a counter
/// moved, never carries provider text or error messages, and the totals are
/// the process-lifetime counters (issue #288).
fn report_generation_fallbacks(
    current: &GenerationFallbackCounts,
    previous: &mut GenerationFallbackCounts,
) {
    if current == previous {
        return;
    }
    for (reason, count) in &current.by_reason {
        if previous.by_reason.get(reason) != Some(count) {
            eprintln!(
                "generation_fallback: reason={reason} count={count} visible_total={} unseen_total={}",
                current.visible, current.silent
            );
        }
    }
    previous.clone_from(current);
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
        max_memory_links: env_usize("AIVTUBER_MEMORY_MAX_LINKS", defaults.max_memory_links)?,
        max_memory_links_per_write: env_usize(
            "AIVTUBER_MEMORY_MAX_LINKS_PER_WRITE",
            defaults.max_memory_links_per_write,
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
        || policy.max_memory_links == 0
        || policy.max_memory_links_per_write == 0
        || policy.max_adaptation_feedback_assets == 0
        || policy.max_adaptation_recent_groups == 0
        || policy.max_adaptation_decisions == 0
        || policy.generation_queue_capacity == 0
        || policy.max_causal_traces == 0
    {
        return Err(io::Error::other("runtime retention limits must be positive").into());
    }
    // The store must be able to hold one maximum-size write, or the store
    // bound would drop an explicit supersession the same write just had
    // accepted (WorkingMemory::new refuses this shape; fail here with the
    // env-var names instead).
    if policy.max_memory_links < policy.max_memory_links_per_write {
        return Err(io::Error::other(
            "AIVTUBER_MEMORY_MAX_LINKS must cover one maximum-size write \
             (>= AIVTUBER_MEMORY_MAX_LINKS_PER_WRITE)",
        )
        .into());
    }
    Ok(policy)
}

/// The supervised local TTS sidecar and what the app knows of its health.
struct TtsSidecarHandle {
    child: Option<TtsSidecar>,
    /// False once the sidecar failed to start or exited; the TTS engine is
    /// gated on it so replies degrade at once instead of timing out.
    alive: Arc<AtomicBool>,
    reported_exit: bool,
}

impl TtsSidecarHandle {
    /// Report a mid-run exit once. Requests degrade on their own.
    fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if !self.reported_exit && (!child.is_alive() || !child.output_open()) {
            self.reported_exit = true;
            self.alive.store(false, Ordering::Release);
            eprintln!(
                "tts_sidecar: exited; speech degrades to non-verbal reactions (no restart; restart the app to retry)"
            );
        }
    }
}

/// `AIVTUBER_TTS_SIDECAR` (off by default): spawn and own `local-tts-sidecar`.
/// Misconfiguration is a startup error. A sidecar that cannot start or become
/// ready is not: the app carries on and every reply degrades to the typed
/// non-verbal reaction, the same as any TTS failure.
fn start_tts_sidecar(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<TtsSidecarHandle>, Box<dyn Error>> {
    let Some(program) = env_optional("AIVTUBER_TTS_SIDECAR") else {
        return Ok(None);
    };
    if profile != CompositionProfile::Local {
        return Err(
            "AIVTUBER_TTS_SIDECAR is supported with AIVTUBER_MODE=local only; start the sidecar yourself and point AIVTUBER_TTS_ENDPOINT at it"
                .into(),
        );
    }
    if !env_bool("AIVTUBER_GENERATIVE_ENABLED", false)? {
        return Err("AIVTUBER_TTS_SIDECAR requires AIVTUBER_GENERATIVE_ENABLED=true".into());
    }
    let model_dir = env_path("AIVTUBER_TTS_SIDECAR_MODEL_DIR")
        .ok_or("AIVTUBER_TTS_SIDECAR_MODEL_DIR is required with AIVTUBER_TTS_SIDECAR")?;
    let extra_args: Vec<String> = env_optional("AIVTUBER_TTS_SIDECAR_ARGS")
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let settings = TtsSidecarSettings {
        program: PathBuf::from(&program),
        model_dir,
        audio_root: audio_root.to_path_buf(),
        viseme_root: env_bool("AIVTUBER_TTS_SIDECAR_AMPLITUDE_VISEMES", false)?
            .then(|| pack_root.to_path_buf()),
        endpoint: required_env("AIVTUBER_TTS_ENDPOINT")?,
        speaker_id: u32::try_from(env_u64("AIVTUBER_TTS_SIDECAR_SPEAKER_ID", 0)?)
            .map_err(|_| "AIVTUBER_TTS_SIDECAR_SPEAKER_ID is too large")?,
        threads: u32::try_from(env_u64("AIVTUBER_TTS_SIDECAR_THREADS", 4)?)
            .map_err(|_| "AIVTUBER_TTS_SIDECAR_THREADS is too large")?,
        ready_timeout: Duration::from_millis(env_u64(
            "AIVTUBER_TTS_SIDECAR_READY_TIMEOUT_MS",
            60_000,
        )?),
        extra_args,
    };
    let config = settings.sidecar_config()?;
    // Operator-facing notice of the voice licence (S2a decision Q1).
    eprintln!("tts_sidecar: {VOICE_LICENCE_NOTICE}");
    let started = Instant::now();
    let alive = Arc::new(AtomicBool::new(false));
    match TtsSidecar::spawn(&config) {
        Ok(child) => {
            alive.store(true, Ordering::Release);
            eprintln!(
                "tts_sidecar: ready listen={} model={} load_ms={} visemes={}",
                child.ready().listen,
                child.ready().model_id,
                started.elapsed().as_millis(),
                if settings.viseme_root.is_some() {
                    "amplitude"
                } else {
                    "off"
                }
            );
            Ok(Some(TtsSidecarHandle {
                child: Some(child),
                alive,
                reported_exit: false,
            }))
        }
        Err(message) => {
            eprintln!(
                "tts_sidecar: unavailable: {message}; speech degrades to non-verbal reactions"
            );
            Ok(Some(TtsSidecarHandle {
                child: None,
                alive,
                reported_exit: true,
            }))
        }
    }
}

/// `AIVTUBER_TTS_BACKEND=voicevox` (off by default): validate the settings and
/// start the in-process bridge to a separately started VOICEVOX ENGINE.
/// Misconfiguration (no speaker, non-loopback engine, ...) is a startup error;
/// an engine that is not running yet is not.
fn start_voicevox_bridge(
    profile: CompositionProfile,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<VoicevoxBridge>, Box<dyn Error>> {
    let generative = env_bool("AIVTUBER_GENERATIVE_ENABLED", false)?;
    let Some(settings) =
        VoicevoxSettings::resolve(&|name| env_optional(name), profile, generative)?
    else {
        return Ok(None);
    };
    let bridge = VoicevoxBridge::start(settings, audio_root, pack_root)?;
    for line in bridge.startup_lines() {
        eprintln!("{line}");
    }
    Ok(Some(bridge))
}

fn build_generative_runtime(
    compatibility: &RuntimeCompatibility,
    retention: RuntimeRetentionPolicy,
    profile: CompositionProfile,
    supervised_tts: Option<Arc<AtomicBool>>,
    voicevox: Option<&VoicevoxBridge>,
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
    // #220 S1: explicit authentication policy. The default still requires a
    // remote credential; an unauthenticated Thinking endpoint must be spelled
    // out and is only constructible against a verified loopback host.
    let thinking_auth = ThinkingAuthentication::from_policy(
        env_optional("AIVTUBER_OPENAI_AUTH").as_deref(),
        env_optional("AIVTUBER_OPENAI_API_KEY"),
    )?;
    let tts_endpoint = match voicevox {
        Some(bridge) => bridge.tts_endpoint().to_owned(),
        None => required_env("AIVTUBER_TTS_ENDPOINT")?,
    };
    // The loopback-only profile must not be pointed at a remote provider.
    let resolver = SystemHostResolver;
    profile.validate_provider_endpoints(&[
        (
            "thinking",
            verify_loopback_endpoint(&thinking_config.endpoint, &resolver).is_ok(),
        ),
        (
            "tts",
            verify_loopback_endpoint(&tts_endpoint, &resolver).is_ok(),
        ),
    ])?;
    let mut thinking = OpenAiResponsesAdapter::with_authentication(thinking_config, thinking_auth)?;
    if let Some(name) = env_optional("AIVTUBER_OPENAI_BACKEND_NAME") {
        thinking = thinking.with_backend_name(name);
    }
    // Issue #83 S2: optional provider token streaming into phrase commit.
    let mut phrase_mode = resolve_phrase_mode(profile)?;
    if env_bool("AIVTUBER_THINKING_STREAM", false)? {
        let PhraseMode::Phrased(config) = &mut phrase_mode else {
            return Err(
                "AIVTUBER_THINKING_STREAM=on needs phrase mode (AIVTUBER_PHRASE_MODE=phrase, or                  the local profile)"
                    .into(),
            );
        };
        let defaults = StreamLimits::default();
        let limits = StreamLimits {
            idle_timeout: Duration::from_millis(env_u64(
                "AIVTUBER_THINKING_STREAM_IDLE_MS",
                defaults.idle_timeout.as_millis() as u64,
            )?),
            max_reply_chars: env_usize(
                "AIVTUBER_THINKING_STREAM_MAX_REPLY_CHARS",
                defaults.max_reply_chars,
            )?,
            ..defaults
        };
        thinking = thinking.with_streaming(limits)?;
        let policy = StreamPolicy {
            max_duration: Duration::from_millis(env_u64(
                "AIVTUBER_THINKING_STREAM_MAX_MS",
                StreamPolicy::default().max_duration.as_millis() as u64,
            )?),
        };
        config.stream = Some(policy);
        config.validate()?;
        eprintln!(
            "thinking stream: on (idle {} ms, max {} ms, max reply {} chars)",
            limits.idle_timeout.as_millis(),
            policy.max_duration.as_millis(),
            limits.max_reply_chars,
        );
    }
    // Non-secret, replay-relevant identity: where Thinking runs and how it is
    // authenticated. Origin only; no path/query that could carry a token.
    eprintln!(
        "thinking: endpoint={} auth={} backend={}",
        endpoint_origin(thinking.endpoint()),
        thinking.authentication_label(),
        aivtuber_domain::ThinkingEngine::identity(&thinking).name,
    );

    // A supervised sidecar has a known identity; record it in provenance
    // unless the operator overrode it.
    let (default_backend, default_alias, default_version) = if supervised_tts.is_some() {
        (
            aivtuber_adapters::speech_tts::BACKEND_NAME,
            Some(SUPERTONIC_MODEL_ALIAS.to_owned()),
            Some(aivtuber_adapters::speech_stt::pins::bundle_sha256(
                &SUPERTONIC_3_INT8,
            )),
        )
    } else if let Some(bridge) = voicevox {
        (
            aivtuber_adapters::speech_tts::voicevox::BACKEND_NAME,
            Some(bridge.settings().model_alias()),
            None,
        )
    } else {
        ("normalized-http-tts", None, None)
    };
    let tts = NormalizedHttpTtsAdapter::new(
        NormalizedHttpTtsConfig {
            endpoint: tts_endpoint,
            backend_name: env_string("AIVTUBER_TTS_BACKEND_NAME", default_backend),
            model_alias: env_optional("AIVTUBER_TTS_MODEL_ALIAS").or(default_alias),
            model_version: env_optional("AIVTUBER_TTS_MODEL_VERSION").or(default_version),
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

    let tts: Arc<dyn aivtuber_domain::TtsEngine> = match supervised_tts {
        Some(alive) => Arc::new(LivenessGatedTts::new(Arc::new(tts), alive)),
        None => Arc::new(tts),
    };
    let speech_lexicon = build_speech_lexicon()?;
    let pipeline = GenerativePipeline::new(Arc::new(thinking), tts, compiler)
        .with_speech_lexicon(Arc::new(speech_lexicon));
    eprintln!(
        "generated replies: {}",
        match phrase_mode {
            PhraseMode::Buffered => "buffered (one commit per reply)",
            PhraseMode::Phrased(_) => "phrase mode (one atomic commit per phrase)",
        }
    );
    let runtime =
        GenerativeRuntime::with_execution_config(pipeline, retention.generation_execution_config())
            .map_err(io::Error::other)?
            .with_phrase_mode(phrase_mode)
            .map_err(io::Error::other)?;
    Ok(Some(runtime))
}

/// ADR-0004 / issue #222 S1: the reading lexicon applied between the gated
/// display text and TTS. `AIVTUBER_SPEECH_LEXICON` names an operator lexicon
/// file (schema `speech-lexicon.schema.json`) merged over the committed starter
/// lexicon, which `AIVTUBER_SPEECH_LEXICON_DEFAULTS=false` turns off. A file that
/// cannot be read or violates a bound is a startup error naming the entry;
/// speech never silently runs without the lexicon the operator asked for.
fn build_speech_lexicon() -> Result<SpeechLexicon, Box<dyn Error>> {
    let include_builtin = env_bool("AIVTUBER_SPEECH_LEXICON_DEFAULTS", true)?;
    let operator_file = env_path("AIVTUBER_SPEECH_LEXICON");
    let lexicon = SpeechLexicon::load(include_builtin, operator_file.as_deref())
        .map_err(|error| format!("AIVTUBER_SPEECH_LEXICON: {error}"))?;
    // The identity is not secret and is replay-relevant; entries are not logged.
    eprintln!(
        "speech lexicon: {} entries, projection {}",
        lexicon.len(),
        lexicon.identity().key()
    );
    Ok(lexicon)
}

/// `AIVTUBER_PHRASE_MODE=buffered|phrase|auto` (issue #83 S1). `auto` (the
/// default) follows the composition profile; `AIVTUBER_PHRASE_TTS_TIMEOUT_MS`
/// bounds the synthesis of one phrase (default 12000), not of the whole reply.
fn resolve_phrase_mode(profile: CompositionProfile) -> Result<PhraseMode, Box<dyn Error>> {
    let phrased = match env_string("AIVTUBER_PHRASE_MODE", "auto")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => profile.phrased_replies_by_default(),
        "phrase" => true,
        "buffered" => false,
        other => {
            return Err(format!(
                "AIVTUBER_PHRASE_MODE must be auto, phrase or buffered (got {other:?})"
            )
            .into());
        }
    };
    if !phrased {
        return Ok(PhraseMode::Buffered);
    }
    let config = PhraseConfig {
        tts_timeout: Duration::from_millis(env_u64("AIVTUBER_PHRASE_TTS_TIMEOUT_MS", 12_000)?),
        ..PhraseConfig::default()
    };
    config.validate()?;
    Ok(PhraseMode::Phrased(config))
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

/// `AIVTUBER_AVATAR_OUTPUT=local|vts|none`. Unset keeps the historical
/// behavior (`AIVTUBER_VTS_ENABLED` selects VTube Studio, otherwise none).
fn build_avatar_output() -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
    match env_optional("AIVTUBER_AVATAR_OUTPUT").as_deref() {
        None => build_vts_avatar_output(env_bool("AIVTUBER_VTS_ENABLED", false)?),
        Some("vts") => build_vts_avatar_output(true),
        Some("none") => Ok(Box::new(NoopAvatarOutput)),
        Some("local") => build_local_avatar_output(),
        Some(other) => {
            Err(format!("AIVTUBER_AVATAR_OUTPUT must be local, vts or none (got {other:?})").into())
        }
    }
}

fn build_local_avatar_output() -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
    let port = env_u64("AIVTUBER_LOCAL_AVATAR_PORT", 8765)?;
    let defaults = LocalAvatarConfig::default();
    let config = LocalAvatarConfig {
        port: u16::try_from(port).map_err(|_| "AIVTUBER_LOCAL_AVATAR_PORT must fit in u16")?,
        page_dir: env_path("AIVTUBER_LOCAL_AVATAR_PAGE_DIR"),
        queue_capacity: env_usize("AIVTUBER_LOCAL_AVATAR_QUEUE", defaults.queue_capacity)?,
        max_command_age: std::time::Duration::from_millis(env_u64(
            "AIVTUBER_MAX_DISPATCH_LATENESS_MS",
            250,
        )?),
    };
    let server = LocalAvatarServer::start(config)?;
    let output = LocalAvatarOutput::new(server, runtime_avatar_authority()?);
    // Deliberate operator console output: the launch URL carries the
    // per-launch secret and is the only way to hand it to OBS. Do not capture
    // stderr into shared logs.
    eprintln!(
        "local avatar renderer URL (contains a per-launch secret, do not share or log): {}",
        output.launch_url()
    );
    Ok(Box::new(output))
}

fn build_vts_avatar_output(enabled: bool) -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
    if !enabled {
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

fn report_degraded(avatar_output: &str, health: &AdapterHealth) {
    if let Some(error) = &health.avatar_error {
        eprintln!("{avatar_output} degraded: {error}");
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

/// The planner to install plus a non-secret startup line describing the
/// configured shadow bounds.
type ShadowWrappedPlanner = (Box<dyn RoutePlanner>, Option<String>);

/// #164: optionally wrap the active planner in bounded shadow orchestration.
///
/// Returns the planner to install plus a non-secret startup line describing the
/// configured bounds. With `AIVTUBER_SHADOW_ENABLED` unset or false the active
/// planner is returned unchanged, so a default deployment is unaffected.
fn wrap_with_shadow_orchestration(
    active: Box<dyn RoutePlanner>,
    startup_summary: &ProfileSummary,
) -> Result<ShadowWrappedPlanner, Box<dyn Error>> {
    let enabled = env_bool("AIVTUBER_SHADOW_ENABLED", false)?;
    if !enabled {
        return Ok((active, None));
    }

    let defaults = ShadowOrchestratorConfig::default();
    let config = ShadowOrchestratorConfig {
        schema_version: SHADOW_ORCHESTRATOR_SCHEMA_VERSION.to_owned(),
        enabled: true,
        sample_rate_per_10k: env_u32("AIVTUBER_SHADOW_SAMPLE_RATE", defaults.sample_rate_per_10k)?,
        seed: env_u64("AIVTUBER_SHADOW_SEED", 0)?,
        event_kinds: parse_shadow_event_kinds()?,
        max_concurrent: env_usize("AIVTUBER_SHADOW_MAX_CONCURRENT", defaults.max_concurrent)?,
        queue_capacity: env_usize("AIVTUBER_SHADOW_QUEUE_CAPACITY", defaults.queue_capacity)?,
        deadline_ms: env_u64("AIVTUBER_SHADOW_DEADLINE_MS", defaults.deadline_ms)?,
        allow_provider_calls: env_bool("AIVTUBER_SHADOW_ALLOW_PROVIDER", false)?,
        charge_shadow_to_budget: env_bool("AIVTUBER_SHADOW_CHARGE_BUDGET", false)?,
    };
    config.validate().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("AIVTUBER_SHADOW_* configuration invalid: {error}"),
        )
    })?;

    let policy_id = env_string("AIVTUBER_SHADOW_POLICY_ID", "intent-shadow");
    let active_identity = ShadowPolicyIdentity {
        policy_id: "active-production".to_owned(),
        policy_version: env_string("AIVTUBER_SHADOW_ACTIVE_VERSION", "v1"),
        config_fingerprint: startup_summary.config_fingerprint.clone(),
        runtime_profile: startup_summary.profile.clone(),
        dataset_id: None,
    };
    let shadow_identity = ShadowPolicyIdentity {
        policy_id: policy_id.clone(),
        policy_version: env_string("AIVTUBER_SHADOW_POLICY_VERSION", "v1"),
        config_fingerprint: env_string(
            "AIVTUBER_SHADOW_CONFIG_FINGERPRINT",
            &startup_summary.config_fingerprint,
        ),
        runtime_profile: startup_summary.profile.clone(),
        dataset_id: env_optional("AIVTUBER_SHADOW_DATASET_ID"),
    };

    // No production provider bridge is constructed here. Live provider
    // evaluation stays disabled by default and is not reachable from the
    // daemon; enabling the flag alone still performs no network call because
    // `provider` is `None`.
    let planner = ShadowingRoutePlanner::with_orchestrator(
        active,
        IntentShadowPolicy,
        active_identity,
        shadow_identity,
        None,
        config.clone(),
    )?;

    let summary = format!(
        "shadow_orchestration: enabled=true policy={policy_id} schema={} sample_rate_per_10k={} max_concurrent={} queue_capacity={} deadline_ms={} provider_calls={} charge_budget={}",
        config.schema_version,
        config.sample_rate_per_10k,
        config.max_concurrent,
        config.queue_capacity,
        config.deadline_ms,
        config.allow_provider_calls,
        config.charge_shadow_to_budget,
    );
    Ok((Box::new(planner) as Box<dyn RoutePlanner>, Some(summary)))
}

/// Parse the optional `AIVTUBER_SHADOW_EVENT_KINDS` comma-separated filter.
/// An empty or unset value means every kind is eligible.
fn parse_shadow_event_kinds() -> Result<Vec<EventKind>, Box<dyn Error>> {
    let Some(raw) = env_optional("AIVTUBER_SHADOW_EVENT_KINDS") else {
        return Ok(Vec::new());
    };
    let mut kinds = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let kind: EventKind = serde_json::from_value(serde_json::Value::String(token.to_owned()))
            .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("AIVTUBER_SHADOW_EVENT_KINDS contains unknown event kind: {token}"),
            )
        })?;
        kinds.push(kind);
    }
    Ok(kinds)
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

/// Checked `u32` environment parsing. A narrowing `as u32` would silently wrap
/// (e.g. `4294967296` becoming `0`), which contradicts the promise that invalid
/// shadow configuration is rejected rather than clamped.
fn env_u32(name: &str, default: u32) -> Result<u32, Box<dyn Error>> {
    let value = env_u64(name, u64::from(default))?;
    narrow_u32(name, value)
}

fn narrow_u32(name: &str, value: u64) -> Result<u32, Box<dyn Error>> {
    u32::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must fit in 32 bits: {value}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #164 review: shadow bounds must be rejected, never silently wrapped.
    /// `4294967296 as u32` is `0`, which would then pass `validate()`.
    #[test]
    fn shadow_bounds_reject_narrowing_overflow_instead_of_wrapping() {
        assert_eq!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", 10_000).expect("in range"),
            10_000
        );
        assert_eq!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", u64::from(u32::MAX)).expect("boundary"),
            u32::MAX
        );
        for overflowing in [u64::from(u32::MAX) + 1, 4_294_967_296, u64::MAX] {
            let error = narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", overflowing)
                .expect_err("must reject, never wrap");
            assert!(
                error.to_string().contains("must fit in 32 bits"),
                "unexpected error: {error}"
            );
        }
        // The value that used to wrap to 0 would otherwise have been read as
        // "sample nothing" rather than rejected as an invalid configuration.
        assert_ne!(
            narrow_u32("AIVTUBER_SHADOW_SAMPLE_RATE", 4_294_967_296).ok(),
            Some(0)
        );

        assert!(
            usize::try_from(u64::MAX).is_err() || usize::BITS == 64,
            "the usize conversion must stay platform-safe"
        );
    }
}
