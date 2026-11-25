mod bootstrap;
use aivtuber_adapters::LocalAvatarHandle;
use aivtuber_app::{
    AudioBackendKind, BargeInConfig, BargeInMetrics, ChatBurstConfig, CompositionProfile,
    GenerationFallbackCounts, GenerativeBudgetPolicy, IntentRoutePlanner, LocalRoutePlanner,
    MemoryFalsifierMetrics, MemorySeed, MemoryWriterConfig, MemoryWriterMetrics,
    OperatorControlServer, OperatorEndpointOptions, OperatorRateLimiter, PhraseMetrics,
    ProductionApp, ProfileSummary, RawIngressConfig, RawIngressMetrics, RawIngressMetricsSnapshot,
    RoutePlanner, SpeechConversationConfig, SpeechConversationMetrics, SpeechFloorMetrics,
    SpeechInputController, SpeechSidecarConfig, SpeechSidecarStats, apply_dispatched_request,
    audit_pack_readiness, build_audio_output, pump_bounded_records, stt_sidecar_args,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::FloorConfig;
use aivtuber_reflex::{
    BurstConfig, JevAdapter, JevAdapterConfig, JevApiKey, PolicyConfig, ReflexPipeline,
    SemanticIndex,
};
use aivtuber_runtime::{
    CachedPerformer, CachedPlaybackConfig, IngressPolicy, LocalVisemeStore, PerceptionStatus,
    SecurityRuntime, SecurityRuntimeConfig, SpeechObservationConfig, SpeechObservationTracker,
    SpeechTranscriptConfig, SpeechTranscriptTracker,
};
use aivtuber_scheduler::{DeferredConfig, Scheduler, SchedulerConfig};
use aivtuber_telemetry::SecretRedactor;
use bootstrap::*;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// `run` composes the whole app in one frame; a debug build of it already
/// takes most of a Windows main thread's 1 MiB stack, so a request path that
/// is deep (a failed process spawn) overflowed it as soon as the app struct
/// grew a field. It runs on a thread with a stack that cannot be the limit.
const RUN_STACK_BYTES: usize = 32 * 1024 * 1024;

/// After an `app.shutdown` (#449): how long the composition loop keeps running
/// so the answer reaches the caller before the app ends.
const SHUTDOWN_REPLY_GRACE_MS: u64 = 500;
/// The hard bound on the graceful stop; then the process exits anyway.
const SHUTDOWN_DEADLINE_MS: u64 = 5_000;

fn main() {
    let worker = thread::Builder::new()
        .name("aivtuber-app".to_owned())
        .stack_size(RUN_STACK_BYTES)
        .spawn(|| run().map_err(|error| error.to_string()));
    let result = match worker {
        Ok(handle) => handle.join().unwrap_or_else(|_| Err("panicked".to_owned())),
        Err(error) => Err(format!("could not start the main thread: {error}")),
    };
    if let Err(error) = result {
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
    // #327: optional Aivis Cloud backend (internet egress, explicit opt-in).
    let aivis_bridge = start_aivis_bridge(profile, &audio_root, &pack_root)?;
    // #411: optional experimental Irodori-TTS backend (operator-run server, loopback).
    let irodori_bridge = start_irodori_bridge(profile, &audio_root, &pack_root)?;
    // #220 S2b: optional supervised local TTS sidecar (local and hybrid profiles only).
    let mut tts_sidecar = start_tts_sidecar(profile, &audio_root, &pack_root)?;
    // #404: the operator's character sheet, read once and bounded. Unset = no
    // persona and nothing changes; the outcome is announced below once the
    // generative path and the Thinking endpoint are known.
    let persona_path = env_path(aivtuber_app::persona::PERSONA_FILE_VAR);
    let persona_load = persona_path
        .as_deref()
        .map(aivtuber_app::persona::load_persona_file);
    let GenerativeParts {
        runtime: generative_configured,
        model_switcher,
        voice: voice_controller,
        thinking_egress_host,
        thinking_endpoint_is_loopback,
        thinking_meter,
        affect_judge_stats,
        face_settle_baseline,
    } = build_generative_runtime(
        &compatibility,
        retention,
        profile,
        tts_sidecar.as_ref().map(|handle| handle.alive.clone()),
        persona_load
            .as_ref()
            .and_then(aivtuber_app::persona::PersonaLoad::sheet),
        TtsBackends {
            voicevox: voicevox_bridge.as_ref(),
            aivis: aivis_bridge.as_ref(),
            irodori: irodori_bridge.as_ref(),
            audio_root: &audio_root,
            pack_root: &pack_root,
        },
    )?;
    // #397: whether Thinking leaves this machine, for the voice-input line:
    // the same loopback decision the `thinking:` line uses (#393).
    let thinking_leaves_machine = !thinking_endpoint_is_loopback;
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
    // #404: a character sheet needs the generative path. One always-printed
    // line says whether it is in effect and, under a remote Thinking endpoint,
    // that it is sent to the cloud model with each reply (never the text).
    let persona_load = persona_load.filter(|_| generative.is_some());
    if let Some(load) = &persona_load {
        eprintln!(
            "{}",
            aivtuber_app::ready_guide::persona_line(
                load,
                thinking_leaves_machine,
                aivtuber_adapters::log_verbosity::diagnostics_enabled(),
            )
        );
    }
    // #357: the model selector only exists next to a composed generative path.
    let model_switcher = model_switcher.filter(|_| generative.is_some());
    // #360: the voice settings only exist next to a composed generative path.
    let voice_controller = voice_controller.filter(|_| generative.is_some());
    // #367: the usage totals only exist next to a composed generative path.
    let thinking_meter = thinking_meter.filter(|_| generative.is_some());
    let affect_judge_stats = affect_judge_stats.filter(|_| generative.is_some());
    let face_settle_baseline = face_settle_baseline.filter(|_| generative.is_some());
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

    // Issue #354: audio backend selection. Default is "process" (ProcessAudioPlayer).
    // "rodio-gapless" composes RodioAudioPlayer with gapless queueing under the native-audio feature.
    let audio_backend_str =
        env_optional("AIVTUBER_AUDIO_BACKEND").unwrap_or_else(|| "process".to_owned());
    let audio_backend = AudioBackendKind::parse(&audio_backend_str)?;
    let audio_program = env_optional("AIVTUBER_AUDIO_PROGRAM");
    let selection = build_audio_output(audio_backend, &audio_root, audio_program.as_deref())?;
    if let Some(reason) = &selection.fallback_reason {
        eprintln!(
            "audio: requested backend {} not used, falling back to {}: {reason}",
            audio_backend.as_str(),
            selection.active.as_str()
        );
    }
    let audio = selection.output;
    let audio_active = selection.active;
    let audio_supports_gapless = audio.gapless_queue();

    let mut avatar_launch_url: Option<String> = None;
    let mut avatar_server: Option<(LocalAvatarHandle, Option<PathBuf>)> = None;
    let avatar = build_avatar_output(&mut avatar_launch_url, &mut avatar_server)?;
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
    )
    .with_thinking_egress_host(thinking_egress_host);
    // #78 S3: the reflex/full profiles already own a semantic index and a Jev
    // adapter; the chat-burst lane may reuse (clones of) them. Local and cached
    // compose neither, so those stay Jev-free.
    let mut chat_score_index: Option<SemanticIndex> = None;
    let mut chat_jev_adapter: Option<JevAdapter> = None;
    let active_planner: Box<dyn RoutePlanner> = match profile {
        CompositionProfile::Cached => Box::new(IntentRoutePlanner) as Box<dyn RoutePlanner>,
        // #220 S1: deterministic routing, no decision model, no Jev. #364: hybrid
        // composes the same graph; only its Thinking egress policy differs.
        CompositionProfile::Local | CompositionProfile::Hybrid => {
            Box::new(LocalRoutePlanner) as Box<dyn RoutePlanner>
        }
        CompositionProfile::Reflex | CompositionProfile::Full => {
            let semantic_index = build_starter_semantic_index(&compatibility)?;
            chat_score_index = Some(semantic_index.clone());
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
            chat_jev_adapter = Some(jev_adapter.clone());
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
        audio,
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
    if let Some(load) = persona_load {
        app = app.with_persona(load);
    }
    // #401: the sheet can be replaced while running whenever the generative
    // path is composed; a save goes to the file when one is named.
    let generative_composed = generative.is_some();
    if generative.is_some() {
        app = app.with_persona_editor(aivtuber_app::persona::PersonaEditor::new(
            persona_path,
            thinking_leaves_machine,
        ));
    }
    if let Some(generative) = generative {
        app = app.with_generation(generative);
    }
    if let Some(switcher) = model_switcher {
        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
            eprintln!(
                "thinking model: {} (switchable: {})",
                switcher.status().active,
                switcher.catalog().render_choices()
            );
        }
        app = app.with_model_switcher(switcher);
    }
    if let Some(meter) = thinking_meter {
        app = app.with_thinking_meter(meter);
    }
    if let Some(stats) = affect_judge_stats {
        app = app.with_affect_judge_stats(stats);
    }
    // #447: only with affect expression on; `AIVTUBER_AFFECT_EXPRESSION=false`
    // keeps the previous fixed behavior.
    if let Some((preset, intensity)) = face_settle_baseline {
        app = app.with_face_settle(preset, intensity);
    }
    if let Some(controller) = voice_controller {
        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
            eprintln!(
                "voice settings: composed (backend={}; the operation panel and aivtuberctl can change it)",
                controller.status().backend
            );
        }
        app = app.with_voice_controller(controller);
    }
    // #361: stream chat connected from the panel. Off unless the launcher names
    // the settings file.
    if let Some(controller) = build_chat_ingress(raw_ingress_config)? {
        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
            eprintln!(
                "chat ingress: composed (the operation panel and aivtuberctl can connect Twitch and YouTube chat; records are admitted as untrusted public chat under the ingress ceiling)"
            );
        }
        app = app.with_chat_ingress(controller);
    }
    // #362: the VRM avatar choice. Off unless the launcher names the settings
    // file and the store directory, and the local avatar renderer is composed.
    if let Some((handle, page_dir)) = avatar_server.take()
        && let Some(controller) = build_avatar_controller(handle, page_dir.as_deref())?
    {
        eprintln!(
            "avatar model: {} (the operation panel and aivtuberctl can change it; show any credit where viewers see it)",
            controller.active_summary()
        );
        app = app.with_avatar_controller(controller);
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
    // #321: the operator memory seed file is opt-in. It loads at start (and is
    // replaced only by the authenticated `memory.seed.set`, #402), through the permit-gated path; an invalid or refused file stops
    // startup rather than running with silently different memory.
    let memory_seed_path = env_path(aivtuber_app::MEMORY_SEED_FILE_VAR);
    if let Some(path) = memory_seed_path.as_deref() {
        let seed =
            MemorySeed::load(path).map_err(|error| format!("AIVTUBER_MEMORY_SEED: {error}"))?;
        app = app
            .with_memory_seed(&seed, 0)
            .map_err(|error| format!("AIVTUBER_MEMORY_SEED: {error}"))?;
        let identity = seed.identity();
        eprintln!(
            "memory_seed: loaded id={} revision={} entries={} content_hash={}",
            identity.seed_id, identity.revision, identity.entries, identity.content_hash
        );
    }
    // #402: the seed can be viewed and replaced while running whenever the
    // adaptation runtime and the generative path are composed; a save goes to the file when one is named.
    if app.adaptation().is_some() && app.generation_execution_snapshot().is_some() {
        app.set_memory_seed_editor(aivtuber_app::MemorySeedEditor::new(
            memory_seed_path,
            thinking_leaves_machine,
        ));
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
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!("{}", startup_summary.log_line());
        eprintln!(
            "audio: backend={} supports_gapless={audio_supports_gapless}",
            audio_active.as_str()
        );
    }
    if let Some(notice) = startup_summary.egress_notice() {
        eprintln!("{notice}");
    }
    if let Some(line) = shadow_summary {
        eprintln!("{line}");
    }
    let preload = app.startup()?;
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!(
            "aivtuber-app ready: indexed={} usable={} preloaded={}",
            preload.indexed.indexed, preload.indexed.usable, preload.preloaded
        );
    }
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
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!(
            "raw_ingress: max_record_bytes={} queue_capacity={}",
            raw_ingress_config.max_record_bytes, raw_ingress_config.queue_capacity
        );
    }

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
    // #90 S1b: live screen perception is opt-in. Only then does the operator
    // endpoint's ingress also hold `perception.control`.
    let perception_opt_in = env_bool("AIVTUBER_PERCEPTION", false)?;
    // #397/#400: the speech sidecar program and the voice-input state file.
    // With both, voice input is composed and the operator endpoint holds
    // `speech_input.control`; without the file (the app run directly) the
    // sidecar starts as before and cannot be switched.
    let speech_program = env_optional("AIVTUBER_SPEECH_SIDECAR");
    let speech_settings = env_path(aivtuber_app::speech_input::SPEECH_INPUT_SETTINGS_VAR);
    // #463: the opt-in session conversation log. Composed only when the launcher
    // names both the directory and the remembered-choice file; it starts off
    // unless the operator turned it on earlier, writes nothing and creates
    // nothing while off, and is only ever switched by the operator endpoint.
    if let (Some(dir), Some(settings)) = (
        env_path(aivtuber_app::session_log::SESSION_LOG_DIR_VAR),
        env_path(aivtuber_app::session_log::SESSION_LOG_SETTINGS_VAR),
    ) {
        let log = aivtuber_app::session_log::SessionLog::from_settings(dir, settings);
        eprintln!(
            "session log: {} (a local file of the conversation, switch in the panel or with aivtuberctl log)",
            if log.enabled() { "on" } else { "off" }
        );
        app = app.with_session_log(log);
    }
    // #321: the memory export needs the writer's live entries, so the endpoint
    // holds `memory.admin` only when the writer is composed.
    let (operator_server, mut operator_receiver) = OperatorControlServer::with_options(
        &operator_endpoint,
        operator_secret,
        OperatorEndpointOptions {
            perception: perception_opt_in,
            memory_export: app.memory_writer_metrics().is_some(),
            // #357: `model.select` only when a model allowlist is composed.
            model_select: app.thinking_model_status().is_some(),
            // #360: `voice.configure` only when the voice settings are composed.
            voice_configure: app.voice_configured(),
            // #361: `ingress.control` only when chat connections are composed.
            ingress_control: app.chat_ingress_configured(),
            // #362: `avatar.configure` only when the avatar store is composed.
            avatar_configure: app.avatar_configured(),
            // #400: `speech_input.control` only when voice input is composed.
            speech_input_control: speech_program.is_some() && speech_settings.is_some(),
            // #401: `persona.configure` only when live editing is composed (it
            // also covers the #87 S2 topic list, composed on the same condition).
            persona_configure: app.persona_editor_composed(),
            // #402: `memory.admin` also for the `memory.seed.*` actions when
            // live seed editing is composed.
            memory_seed_edit: app.memory_seed_editor_composed(),
            // #87: `initiative.control` only when the initiative is composed.
            // It is composed whenever the generative path is, but later than
            // this point, so ask the same question here.
            initiative_control: generative_composed,
            // #463: `session_log.control` only when the session log is composed.
            session_log_control: app.session_log_composed(),
        },
    )?;
    let operator_ingress = operator_server.ingress();
    // #335: the local operation panel. Its control actions are the operator
    // endpoint's own (same ingress, same secret, same audit); the page itself
    // holds only a separate per-launch panel secret.
    let mut panel = start_panel(
        profile,
        &operator_ingress,
        &operator_secret,
        avatar_launch_url,
        app.voice_search_endpoint(),
        app.avatar_staging_dir(),
    )?;
    // #388: kept (redacted, in memory only) so `aivtuberctl panel-url` can show
    // it again to the authenticated operator endpoint.
    if let Some(panel) = &panel {
        app.set_panel_url(panel.launch_url());
    }
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
    // #393: quiet start-up ends with a short guide; it does not wait for the
    // microphone or speech recognition.
    if !aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        for line in aivtuber_app::ready_guide::ready_lines(panel.is_some()) {
            eprintln!("{line}");
        }
    }

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
    let mut speech_conversation_enabled = false;
    let mut chat_burst_enabled = false;
    // Issues #75 S1 / #78 S2: live chat-burst selection and deferred
    // replies. Off unless AIVTUBER_CHAT_BURST_MODE is true, so the default
    // per-event path is unchanged. Every bound is validated, never clamped.
    // Issue #79 S1: audience-signal aggregation of batched chat. Its own
    // default-off flag; it only has an effect inside the burst lane, so asking
    // for it without AIVTUBER_CHAT_BURST_MODE refuses startup rather than
    // silently doing nothing.
    let audience_requested = env_bool("AIVTUBER_CHAT_AUDIENCE_MODE", false)?;
    let chat_burst_mode = env_bool("AIVTUBER_CHAT_BURST_MODE", false)?;
    if audience_requested && !chat_burst_mode {
        return Err("AIVTUBER_CHAT_AUDIENCE_MODE requires AIVTUBER_CHAT_BURST_MODE=true".into());
    }
    // Issue #79 S2: crowd-level reaction routing of the clusters S1 produces.
    // Its own default-off flag; it needs audience aggregation, so asking for it
    // without AIVTUBER_CHAT_AUDIENCE_MODE refuses startup.
    let crowd_requested = env_bool("AIVTUBER_CHAT_CROWD_MODE", false)?;
    if crowd_requested && !audience_requested {
        return Err("AIVTUBER_CHAT_CROWD_MODE requires AIVTUBER_CHAT_AUDIENCE_MODE=true".into());
    }
    // Issue #78 S3: both default off and only act inside the burst lane, so
    // asking for them without it refuses startup rather than doing nothing.
    let retrieval_score_requested = env_bool("AIVTUBER_CHAT_BURST_RETRIEVAL_SCORE", false)?;
    let jev_ranker_requested = env_bool("AIVTUBER_CHAT_BURST_JEV_RANKER", false)?;
    if (retrieval_score_requested || jev_ranker_requested) && !chat_burst_mode {
        return Err(
            "AIVTUBER_CHAT_BURST_RETRIEVAL_SCORE and AIVTUBER_CHAT_BURST_JEV_RANKER require AIVTUBER_CHAT_BURST_MODE=true"
                .into(),
        );
    }
    if chat_burst_mode {
        let defaults = ChatBurstConfig::default();
        let config = ChatBurstConfig {
            burst: BurstConfig {
                window_ms: env_u64("AIVTUBER_CHAT_BURST_WINDOW_MS", defaults.burst.window_ms)?,
                burst_enter_count: env_usize(
                    "AIVTUBER_CHAT_BURST_ENTER_COUNT",
                    defaults.burst.burst_enter_count,
                )?,
                top_k: env_usize("AIVTUBER_CHAT_BURST_TOP_K", defaults.burst.top_k)?,
                max_per_actor: env_usize(
                    "AIVTUBER_CHAT_BURST_MAX_PER_ACTOR",
                    defaults.burst.max_per_actor,
                )?,
                actor_cooldown_ms: env_u64(
                    "AIVTUBER_CHAT_BURST_ACTOR_COOLDOWN_MS",
                    defaults.burst.actor_cooldown_ms,
                )?,
                ..defaults.burst
            },
            deferred: DeferredConfig {
                capacity: env_usize("AIVTUBER_CHAT_DEFER_CAPACITY", defaults.deferred.capacity)?,
                per_actor_cap: env_usize(
                    "AIVTUBER_CHAT_DEFER_PER_ACTOR",
                    defaults.deferred.per_actor_cap,
                )?,
                max_ttl_ms: env_u64(
                    "AIVTUBER_CHAT_DEFER_MAX_TTL_MS",
                    defaults.deferred.max_ttl_ms,
                )?,
            },
            reply_ttl_ms: env_u64("AIVTUBER_CHAT_REPLY_TTL_MS", defaults.reply_ttl_ms)?,
            deferred_ack: env_bool("AIVTUBER_DEFERRED_ACK", defaults.deferred_ack)?,
            audience: if audience_requested {
                let audience_defaults = aivtuber_reflex::AudienceConfig::default();
                Some(aivtuber_reflex::AudienceConfig {
                    min_cluster_size: env_usize(
                        "AIVTUBER_CHAT_AUDIENCE_MIN_CLUSTER",
                        audience_defaults.min_cluster_size,
                    )?,
                    min_unique_actors: env_usize(
                        "AIVTUBER_CHAT_AUDIENCE_MIN_ACTORS",
                        audience_defaults.min_unique_actors,
                    )?,
                })
            } else {
                None
            },
            crowd: if crowd_requested {
                let crowd_defaults = aivtuber_reflex::CrowdConfig::default();
                Some(aivtuber_reflex::CrowdConfig {
                    window_ms: env_u64("AIVTUBER_CHAT_CROWD_WINDOW_MS", crowd_defaults.window_ms)?,
                    cooldown_ms: env_u64(
                        "AIVTUBER_CHAT_CROWD_COOLDOWN_MS",
                        crowd_defaults.cooldown_ms,
                    )?,
                    max_entries: crowd_defaults.max_entries,
                })
            } else {
                None
            },
        };
        app = app.with_chat_burst(config)?;
        chat_burst_enabled = true;
        if retrieval_score_requested {
            // Reuse the reflex profile's index when there is one; otherwise
            // build the same starter index from the pack. Without a usable
            // index the lane keeps the text heuristic (the documented
            // fallback) rather than refusing startup.
            let index = match chat_score_index.take() {
                Some(index) => Ok(index),
                None => build_starter_semantic_index(&compatibility),
            };
            match index {
                Ok(index) => {
                    let embedder = aivtuber_reflex::ReactionAxisEmbedder::new(index.dimension());
                    let scorer = aivtuber_reflex::RetrievalScorer::new(
                        index,
                        Box::new(embedder),
                        aivtuber_reflex::RetrievalScoreConfig {
                            max_lookups_per_window: env_usize(
                                "AIVTUBER_CHAT_BURST_RETRIEVAL_LOOKUPS",
                                aivtuber_reflex::RetrievalScoreConfig::default()
                                    .max_lookups_per_window,
                            )?,
                            window_ms: config.burst.window_ms,
                        },
                    )?;
                    app = app.with_chat_burst_retrieval_score(scorer)?;
                    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                        eprintln!("chat_burst: retrieval-augmented local score enabled");
                    }
                }
                Err(error) => eprintln!(
                    "chat_burst: retrieval score unavailable ({error}); using the text heuristic"
                ),
            }
        }
        if jev_ranker_requested {
            let adapter = chat_jev_adapter.take().ok_or(
                "AIVTUBER_CHAT_BURST_JEV_RANKER requires AIVTUBER_MODE=reflex or full (a composed Jev adapter)",
            )?;
            let ranker_config = aivtuber_reflex::JevBatchRankerConfig {
                deadline: Duration::from_millis(env_u64(
                    "AIVTUBER_CHAT_BURST_JEV_RANKER_DEADLINE_MS",
                    aivtuber_reflex::JevBatchRankerConfig::default()
                        .deadline
                        .as_millis() as u64,
                )?),
                ..aivtuber_reflex::JevBatchRankerConfig::default()
            };
            let ranker = aivtuber_reflex::JevBatchRanker::new(adapter, ranker_config)?;
            app = app.with_chat_burst_ranker(Box::new(ranker))?;
            if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                eprintln!(
                    "chat_burst: Jev batch ranker enabled deadline_ms={} (reorders only; policy re-applies)",
                    ranker_config.deadline.as_millis()
                );
            }
        }
        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
            eprintln!(
                "chat_burst: enabled window_ms={} enter_count={} top_k={} max_per_actor={} actor_cooldown_ms={} defer_capacity={} defer_per_actor={} reply_ttl_ms={}",
                config.burst.window_ms,
                config.burst.burst_enter_count,
                config.burst.top_k,
                config.burst.max_per_actor,
                config.burst.actor_cooldown_ms,
                config.deferred.capacity,
                config.deferred.per_actor_cap,
                config.reply_ttl_ms
            );
        }
        if let Some(audience) = config.audience
            && aivtuber_adapters::log_verbosity::diagnostics_enabled()
        {
            eprintln!(
                "chat_burst: audience aggregation enabled min_cluster={} min_actors={}",
                audience.min_cluster_size, audience.min_unique_actors
            );
        }
    }
    let mut speech_floor_enabled = false;
    let mut speech_barge_in_enabled = false;
    let mut last_floor_metrics = SpeechFloorMetrics::default();
    let mut last_barge_in_metrics = BargeInMetrics::default();
    let mut last_phrase_metrics = PhraseMetrics::default();
    let mut last_memory_writer_metrics = MemoryWriterMetrics::default();
    let mut last_memory_falsifier_metrics = MemoryFalsifierMetrics::default();
    let mut last_projection_metrics = aivtuber_app::SpeechProjectionMetrics::default();
    let mut last_phrase_response_logged: Option<u64> = None;
    let mut last_conversation_metrics = SpeechConversationMetrics::default();
    // #397: under `aivtuber-setup start` the microphone is opened only when the
    // state file says voice input is on. #400: the trackers, conversation
    // route, floor and barge-in are built either way (voice input can be
    // switched on later from the panel); only the sidecar is started or not.
    // The speech program and state-file variable were read above.
    // #396: the file also remembers the capture device and the duplex choice.
    let speech_remembered = speech_settings
        .as_deref()
        .map(aivtuber_app::speech_input::read_settings)
        .unwrap_or_default();
    let speech_input = aivtuber_app::speech_input::SpeechInput::decide(
        speech_program.is_some(),
        speech_settings.is_some(),
        speech_remembered.enabled,
    );
    if speech_input.announces() {
        eprintln!(
            "{}",
            aivtuber_app::ready_guide::voice_input_text(
                speech_input == aivtuber_app::speech_input::SpeechInput::On,
                thinking_leaves_machine,
                aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                speech_remembered.device.is_some(),
            )
        );
    }
    if let Some(program) = speech_program {
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
                    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                        eprintln!(
                            "speech_floor: enabled take_turn_silence_ms={} max_defer_ms={} max_deferred={}",
                            floor.take_turn_silence_ms, floor.max_defer_ms, floor.max_deferred
                        );
                    }
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
                        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                            eprintln!("speech_barge_in: enabled fade_ms={fade_ms}");
                        }
                    }
                }
                if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                    eprintln!(
                        "speech_conversation: enabled max_finals_per_tick={} max_text_chars={}",
                        conversation.max_finals_per_tick, conversation.max_text_chars
                    );
                }
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
        // Issue #440: the input level for the operation panel meter, 10 Hz at
        // most. An explicit --level-ms (0 turns it off) wins.
        if !sidecar_args.iter().any(|arg| arg == "--level-ms") {
            sidecar_args.push("--level-ms".to_owned());
            sidecar_args.push("100".to_owned());
        }
        let mut config = SpeechSidecarConfig::new(program, sidecar_args);
        config.resync_interval =
            Duration::from_millis(env_u64("AIVTUBER_SPEECH_SIDECAR_RESYNC_MS", 5_000)?);
        let mut input = SpeechInputController::new(
            config,
            started,
            speech_settings.clone(),
            thinking_leaves_machine,
        )
        .with_settings(speech_remembered.clone());
        if speech_input.starts_sidecar() {
            match input.start_initial() {
                Ok(()) => {
                    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                        eprintln!("speech_sidecar: started");
                    }
                }
                Err(error) => {
                    eprintln!("speech_sidecar: could not start: {error}; no speech observations");
                    tracker.record_provider_unavailable();
                }
            }
        }
        app = app
            .with_speech_input(input)
            .with_speech_observations(tracker);
    }
    // #90 S1b: opt-in live screen perception ("perception" local profile).
    // Composed disabled; the only way to turn it on is the authenticated
    // control endpoint (`aivtuberctl perception-on`). No configuration
    // enables it (ADR-0007 D-5).
    let mut last_perception_status: Option<(PerceptionStatus, Option<String>)> = None;
    if perception_opt_in {
        let service = build_perception_service(profile, started)?;
        app = app.with_perception(service);
    }
    // #87 S1: the bounded idle initiative. Composed whenever the generative
    // path is, so the panel switch and `aivtuberctl initiative` exist; it
    // starts on in the local profile and off elsewhere unless
    // AIVTUBER_INITIATIVE says otherwise. A bad idle value refuses startup.
    if generative_composed {
        let idle_secs = env_u64(
            "AIVTUBER_INITIATIVE_IDLE_SECS",
            aivtuber_app::initiative::DEFAULT_IDLE_SECS,
        )?;
        let enabled = env_bool("AIVTUBER_INITIATIVE", profile == CompositionProfile::Local)?;
        let config =
            aivtuber_app::initiative::InitiativeConfig::with_idle_secs(enabled, idle_secs)?;
        eprintln!(
            "initiative: {} (speaks on its own after {} s without chat; switch in the panel or with aivtuberctl initiative)",
            if enabled { "on" } else { "off" },
            idle_secs
        );
        app = app.with_initiative(config);
        // #80 S1: the performer's own recent turns, for recall questions.
        // Off until live-verified; sends only its own cut phrases (never
        // viewer text) to the Thinking model.
        if env_bool("AIVTUBER_RECENT_TOPICS", true)? {
            app = app.with_recent_turns();
            eprintln!("recent_topics: on (chat replies see the performer's last turns)");
        }
        // #87 S2: the operator's topic list (「雑談ネタ」), one line per topic
        // in the file named by AIVTUBER_TOPICS_FILE. Read once, bounded; a
        // missing or refused file is an empty list, and the panel or
        // `aivtuberctl topics set` can still fill it (the same
        // `persona.configure` capability as the character sheet). The always
        // printed line says how many were loaded and, under a remote Thinking
        // endpoint, that the topic of an initiative is sent to the cloud model.
        let topics_path = env_path(aivtuber_app::topics::TOPICS_FILE_VAR);
        let topics_load = topics_path
            .as_deref()
            .map(aivtuber_app::topics::load_topics_file)
            .unwrap_or(aivtuber_app::topics::TopicsLoad::Absent);
        eprintln!(
            "{}{}",
            topics_load.line(),
            if thinking_leaves_machine {
                " (the topic of an initiative is sent to the cloud model)"
            } else {
                ""
            }
        );
        app = app.with_topics(
            aivtuber_app::topics::TopicsEditor::new(topics_path, thinking_leaves_machine),
            &topics_load.list(),
        );
    }
    // #88 S1: the scene context (rank 2 OBS program scene through the
    // operator's table, rank 4 perception). Composed only when the operator
    // names a scene table or perception is composed; otherwise nothing changes.
    let obs_scene_map = match env_path("AIVTUBER_OBS_SCENE_MAP") {
        Some(path) => Some(load_obs_scene_map(&path)?),
        None => None,
    };
    if obs_scene_map.is_some() || perception_opt_in {
        let service = build_scene_context_service(profile, obs_scene_map)?;
        app = app.with_scene_context(service);
    }
    let mut last_speech_stats = SpeechSidecarStats::default();
    let mut last_chat_burst_accounted = 0_u64;
    let mut last_generation_fallbacks = GenerationFallbackCounts::default();
    let mut sequence_seed = 0_u64;
    let mut next_maintenance_ms = 0_u64;
    let mut last_raw_ingress_metrics = RawIngressMetricsSnapshot::default();
    // #449: when an authenticated `app.shutdown` was seen, the loop keeps
    // turning for a short grace so the HTTP / control reply is written, then
    // ends the app. The reply is never cut off by an exit.
    let mut shutdown_exit_at_ms: Option<u64> = None;

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
                // Final per-stream falsifier counters (#321): one line to
                // record for the ADR-0008 Q4 procedure.
                let falsifier = app.memory_falsifier_metrics();
                eprintln!(
                    "memory_falsifier_final: seed_entries_loaded={} requests_with_memory={} seed_used={} writer_used_reproducible_from_seed={} writer_only_used={} requests_with_writer_only={} exports={}",
                    falsifier.seed_entries_loaded,
                    falsifier.requests_with_memory,
                    falsifier.seed_used,
                    falsifier.writer_used_reproducible_from_seed,
                    falsifier.writer_only_used,
                    falsifier.requests_with_writer_only,
                    falsifier.exports
                );
                if let Some(service) = app.perception_mut() {
                    service.shutdown();
                }
                app.shutdown(now_ms);
                app.shutdown_voice();
                app.shutdown_chat_ingress();
                app.shutdown_speech_input();
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
        // #357: pick up a finished model warm-up. Never blocks: the load runs
        // on its own thread.
        app.service_model_switch();
        // #335: panel stop/mute (the same operator path) and chat lines (the
        // same admission as a stdin line), after the operator endpoint drain.
        if let Some(panel) = panel.as_mut() {
            panel.service(&mut app, now_ms, &mut sequence_seed);
        }
        if app.shutdown_requested() {
            let exit_at = *shutdown_exit_at_ms
                .get_or_insert_with(|| now_ms.saturating_add(SHUTDOWN_REPLY_GRACE_MS));
            if now_ms >= exit_at {
                eprintln!("shutdown: stopped from the operator (app.shutdown); exiting");
                // Bounded: if a sidecar or worker will not stop, the process
                // still ends (exit 0, the shutdown was requested) after the
                // deadline. Started only now, after the reply grace.
                thread::spawn(|| {
                    thread::sleep(Duration::from_millis(SHUTDOWN_DEADLINE_MS));
                    eprintln!("shutdown: deadline reached; exiting anyway");
                    std::process::exit(0);
                });
                if let Some(service) = app.perception_mut() {
                    service.shutdown();
                }
                app.shutdown(now_ms);
                app.shutdown_voice();
                app.shutdown_chat_ingress();
                app.shutdown_speech_input();
                break;
            }
        }
        // #360: collect a finished key check and submit a waiting voice
        // preview line. Never blocks: the check runs on its own thread.
        if app.service_voice(now_ms, &mut sequence_seed)
            && let Some(panel) = panel.as_mut()
        {
            panel.publish_next_turn();
        }
        // #361: admit what the panel-started chat sidecars wrote, a few
        // records per platform per turn, through the same admission as stdin.
        app.service_chat_ingress(now_ms, &mut sequence_seed);
        // #90 S1b: after the operator drain, so a disable is already in effect
        // before the next capture is considered. Never blocks: the slow OBS and
        // model work is on the perception worker thread.
        app.service_perception(now_ms);
        // #88: after perception, so the scene sees this tick's observations.
        // Never blocks: the OBS scene read is on its own thread. Only a change
        // of scene class is reported, text-free.
        app.service_scene_context(now_ms);
        if let Some(scene) = app.scene_context_mut() {
            for transition in scene.take_transitions(8) {
                eprintln!(
                    "scene: transition from={} to={} rank={} version={}",
                    transition.from.name(),
                    transition.to.name(),
                    transition.source_rank.name(),
                    transition.version
                );
            }
        }
        if let Some(service) = app.perception() {
            let report = service.report(now_ms);
            let key = (
                report.governor.status,
                report
                    .governor
                    .suspend_reason
                    .map(|reason| reason.name().to_owned()),
            );
            if last_perception_status.as_ref() != Some(&key) {
                eprintln!(
                    "perception: status={} suspend_reason={:?} last_probe_ms={:?} captures_ok={} captures_failed={} captures_late={} yielded_thinking={} yielded_speech={}",
                    report.governor.status.name(),
                    key.1,
                    report.governor.last_probe_ms,
                    report.governor.metrics.captures_ok,
                    report.governor.metrics.captures_failed,
                    report.governor.metrics.captures_late,
                    report.governor.metrics.yielded_thinking,
                    report.governor.metrics.yielded_speech,
                );
                last_perception_status = Some(key);
            }
        }
        // #400: the sidecar is owned by the app (it can be switched from the
        // panel). Feed liveness is noted here too, first, then the confirmed
        // barge-in, so a stalled feed is already stale when the barge-in is
        // judged. Only lengths and counts are logged (issue #81 S2).
        app.service_speech_input(now_ms);
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
                    // Quiet hides the normal accepted outcome only.
                    Ok(outcome)
                        if !aivtuber_app::ready_guide::final_line_visible(
                            aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                            matches!(
                                outcome.admission,
                                aivtuber_runtime::ContentAdmitDecision::Queued
                            ),
                        ) => {}
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
        // Issues #75 S1 / #78 S2: flush a due burst window and dispatch deferred
        // replies once the performer is available. After the operator drain
        // above, so emergency control is never queued behind it.
        if chat_burst_enabled {
            sequence_seed = sequence_seed.saturating_add(32);
            app.service_chat_burst(now_ms, sequence_seed);
            report_chat_burst(&mut app, &mut last_chat_burst_accounted);
        }
        // #87: after chat and speech were served, so arrivals this turn have
        // already restarted the idle clock. Never blocks.
        sequence_seed = sequence_seed.saturating_add(8);
        app.service_initiative(now_ms, sequence_seed);
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
            if let Some(stats) = app.speech_sidecar_stats()
                && (stats.lines_dropped_queue_full != last_speech_stats.lines_dropped_queue_full
                    || stats.lines_dropped_oversize != last_speech_stats.lines_dropped_oversize
                    || stats.lines_rejected != last_speech_stats.lines_rejected)
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
            if let Some(metrics) = app.speech_conversation_metrics()
                && metrics != last_conversation_metrics
            {
                if aivtuber_app::ready_guide::metrics_line_visible(
                    aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    &[
                        metrics.dropped_oversize,
                        metrics.dropped_blank,
                        metrics.dropped_by_runtime,
                        metrics.route_errors,
                    ],
                ) {
                    eprintln!(
                        "speech_conversation_metrics: finals={} admitted={} dropped_oversize={} dropped_blank={} dropped_by_runtime={} route_errors={}",
                        metrics.finals_seen,
                        metrics.admitted,
                        metrics.dropped_oversize,
                        metrics.dropped_blank,
                        metrics.dropped_by_runtime,
                        metrics.route_errors
                    );
                }
                last_conversation_metrics = metrics;
            }
            if let Some(metrics) = app.speech_floor_metrics()
                && (metrics.policy != last_floor_metrics.policy
                    || metrics.deferred != last_floor_metrics.deferred
                    || metrics.dropped_expired != last_floor_metrics.dropped_expired
                    || metrics.dropped_overflow != last_floor_metrics.dropped_overflow
                    || metrics.dropped_by_operator != last_floor_metrics.dropped_by_operator)
            {
                if aivtuber_app::ready_guide::metrics_line_visible(
                    aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    &[
                        metrics.dropped_expired,
                        metrics.dropped_overflow,
                        metrics.dropped_by_operator,
                    ],
                ) {
                    eprintln!(
                        "speech_floor_metrics: decisions={} take_turn={} wait={} hold={} yield_recorded={} fallback_take_turn={} deferred={} released={} dropped_expired={} dropped_overflow={} dropped_by_operator={} queued={}",
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
                        metrics.dropped_by_operator,
                        metrics.queued
                    );
                }
                last_floor_metrics = metrics;
            }
            if let Some(metrics) = app.speech_barge_in_metrics()
                && metrics != last_barge_in_metrics
            {
                if aivtuber_app::ready_guide::metrics_line_visible(
                    aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    &[
                        metrics.audio_stop_unsupported,
                        metrics.audio_stop_errors,
                        metrics.avatar_reset_errors,
                    ],
                ) {
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
                }
                last_barge_in_metrics = metrics;
            }
            let projection_metrics = app.speech_projection_metrics();
            if projection_metrics != last_projection_metrics {
                if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                    eprintln!(
                        "speech_projection: replies={} lexicon_hits={} projection_passthrough_latin={}",
                        projection_metrics.replies,
                        projection_metrics.lexicon_hits,
                        projection_metrics.passthrough_latin
                    );
                }
                last_projection_metrics = projection_metrics;
            }
            if let Some(metrics) = app.memory_writer_metrics()
                && metrics != last_memory_writer_metrics
            {
                if aivtuber_app::ready_guide::metrics_line_visible(
                    aivtuber_adapters::log_verbosity::diagnostics_enabled(),
                    &[
                        metrics.expired,
                        metrics.rejected_viewer_attributed,
                        metrics.rejected_oversize,
                        metrics.rejected_quota,
                        metrics.rejected_duplicate,
                        metrics.rejected_gate,
                        metrics.rejected_store,
                    ],
                ) {
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
                }
                last_memory_writer_metrics = metrics;
            }
            let falsifier = app.memory_falsifier_metrics();
            if falsifier != last_memory_falsifier_metrics {
                if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                    eprintln!(
                        "memory_falsifier_metrics: seed_entries_loaded={} requests_with_memory={} seed_used={} writer_used_reproducible_from_seed={} writer_only_used={} requests_with_writer_only={} exports={}",
                        falsifier.seed_entries_loaded,
                        falsifier.requests_with_memory,
                        falsifier.seed_used,
                        falsifier.writer_used_reproducible_from_seed,
                        falsifier.writer_only_used,
                        falsifier.requests_with_writer_only,
                        falsifier.exports
                    );
                }
                last_memory_falsifier_metrics = falsifier;
            }
            let phrase_metrics = app.phrase_metrics();
            if phrase_metrics != last_phrase_metrics {
                if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
                    eprintln!(
                        "phrase_metrics: responses={} completed={} partial={} phrases_committed={} gate_rejected_before_first={} gate_rejected_after_commit={} stream_cumulative_gate_rejected={} stream_whole_reply_gate_audits={} stream_whole_reply_gate_rejected_after_commit={} scheduler_rejected={} synthesis_failed_after_commit={} cancelled_after_commit={} underruns={} max_gap_ms={} max_synthesis_ms={} late_dropped={}",
                        phrase_metrics.responses,
                        phrase_metrics.completed,
                        phrase_metrics.partial,
                        phrase_metrics.phrases_committed,
                        phrase_metrics.gate_rejected_before_first_commit,
                        phrase_metrics.gate_rejected_after_commit,
                        phrase_metrics.stream_cumulative_gate_rejected,
                        phrase_metrics.stream_whole_reply_gate_audits,
                        phrase_metrics.stream_whole_reply_gate_rejected_after_commit,
                        phrase_metrics.scheduler_rejected,
                        phrase_metrics.synthesis_failed_after_commit,
                        phrase_metrics.cancelled_after_commit,
                        phrase_metrics.underruns,
                        phrase_metrics.max_gap_ms,
                        phrase_metrics.max_synthesis_ms,
                        phrase_metrics.late_phrases_dropped
                    );
                }
                last_phrase_metrics = phrase_metrics;
            }
            for record in app.recent_phrase_responses() {
                if last_phrase_response_logged.is_some_and(|logged| record.response_id <= logged) {
                    continue;
                }
                if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
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
                }
                last_phrase_response_logged = Some(record.response_id);
            }
            report_raw_ingress_metrics(&raw_ingress_metrics, &mut last_raw_ingress_metrics, false);
            next_maintenance_ms = now_ms.saturating_add(1_000);
        }
    }

    Ok(())
}
