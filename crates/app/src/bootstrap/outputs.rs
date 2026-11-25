use super::env::*;
use super::policy::{fixed_hex_key, runtime_avatar_authority};
use super::reporting::elapsed_ms;
use aivtuber_adaptation::{
    ActorPseudonymizer, AdaptationEngine, PromotionPolicy, WorkingMemory, WorkingMemoryConfig,
};
use aivtuber_adapters::{
    AdapterClock, CaptionLanguage, LocalAvatarConfig, LocalAvatarHandle, LocalAvatarServer,
    ObsScreenSource, ObsScreenshotRequest, ObsWebSocketAdapter, ObsWebSocketConfig,
    ScreenDescriber, ScreenDescriberConfig, ScreenImageFormat, ScreenObservationProducer,
    ScreenProducerConfig, SecretString, SystemHostResolver, VTubeStudioAdapter, VTubeStudioConfig,
    verify_loopback_endpoint,
};
use aivtuber_app::{
    AdaptationRuntime, AvatarOutput, CompositionProfile, LocalAvatarOutput, NoopAvatarOutput,
    NoopStreamOutput, ObsProgramSceneSource, ObsStreamOutput, PanelService, PerceptionService,
    PerceptionServiceConfig, ProducerWorker, RuntimeRetentionPolicy, SceneContextService,
    StreamOutput, VtsAvatarOutput, VtsPlaybackConfig, panel_settings,
};
use aivtuber_domain::{LocalControlIngress, MAX_OBS_SCENE_MAP_BYTES, ObsSceneMap};
use aivtuber_runtime::PerceptionGovernorConfig;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) fn build_adaptation_runtime(
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
pub(crate) fn build_avatar_output(
    launch_url: &mut Option<String>,
    server_handle: &mut Option<(LocalAvatarHandle, Option<PathBuf>)>,
) -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
    match env_optional("AIVTUBER_AVATAR_OUTPUT").as_deref() {
        None => build_vts_avatar_output(env_bool("AIVTUBER_VTS_ENABLED", false)?),
        Some("vts") => build_vts_avatar_output(true),
        Some("none") => Ok(Box::new(NoopAvatarOutput)),
        Some("local") => build_local_avatar_output(launch_url, server_handle),
        Some(other) => {
            Err(format!("AIVTUBER_AVATAR_OUTPUT must be local, vts or none (got {other:?})").into())
        }
    }
}

pub(crate) fn build_local_avatar_output(
    launch_url: &mut Option<String>,
    server_handle: &mut Option<(LocalAvatarHandle, Option<PathBuf>)>,
) -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
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
    let page_dir = config.page_dir.clone();
    let server = LocalAvatarServer::start(config)?;
    // Kept for the avatar choice (#362): it tells the server which model to
    // serve; the handle is not authority for anything else.
    *server_handle = Some((server.handle(), page_dir));
    let output = LocalAvatarOutput::new(server, runtime_avatar_authority()?);
    // Deliberate operator console output: the launch URL carries the
    // per-launch secret and is the only way to hand it to OBS. Do not capture
    // stderr into shared logs.
    eprintln!(
        "local avatar renderer URL (contains a per-launch secret, do not share or log): {}",
        output.launch_url()
    );
    // Kept for the local panel's avatar link (#335); it only ever reaches the
    // page through an authenticated panel call.
    *launch_url = Some(output.launch_url());
    Ok(Box::new(output))
}

pub(crate) fn build_vts_avatar_output(
    enabled: bool,
) -> Result<Box<dyn AvatarOutput>, Box<dyn Error>> {
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

pub(crate) fn build_stream_output() -> Result<Box<dyn StreamOutput>, Box<dyn Error>> {
    if !env_bool("AIVTUBER_OBS_ENABLED", false)? {
        return Ok(Box::new(NoopStreamOutput));
    }

    let mut config = ObsWebSocketConfig::default();
    config.endpoint = env_string("AIVTUBER_OBS_ENDPOINT", &config.endpoint);
    config.password = env_optional("AIVTUBER_OBS_PASSWORD").map(SecretString::new);
    let adapter = ObsWebSocketAdapter::new(config)?;
    Ok(Box::new(ObsStreamOutput::new(adapter)))
}

/// Monotonic clock on the main loop's `started` base, shared with the
/// perception worker so observation stamps and the loop's `now_ms` agree.
pub(crate) struct LoopClock(pub(crate) Instant);

impl AdapterClock for LoopClock {
    fn now_ms(&self) -> u64 {
        elapsed_ms(self.0)
    }
}

pub(crate) fn loopback_ws(endpoint: &str) -> bool {
    verify_loopback_endpoint(
        &endpoint.replacen("ws://", "http://", 1),
        &SystemHostResolver,
    )
    .is_ok()
}

/// Load the operator's OBS scene table (#88). Bounded before parsing; an
/// invalid file stops startup rather than running with a different mapping.
pub(crate) fn load_obs_scene_map(path: &std::path::Path) -> Result<ObsSceneMap, Box<dyn Error>> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("AIVTUBER_OBS_SCENE_MAP: cannot read the file: {error}"))?;
    if metadata.len() > MAX_OBS_SCENE_MAP_BYTES as u64 {
        return Err(format!(
            "AIVTUBER_OBS_SCENE_MAP: the file exceeds {MAX_OBS_SCENE_MAP_BYTES} bytes"
        )
        .into());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("AIVTUBER_OBS_SCENE_MAP: cannot read the file: {error}"))?;
    let map = ObsSceneMap::from_json_bytes(&bytes)
        .map_err(|error| format!("AIVTUBER_OBS_SCENE_MAP: {error}"))?;
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!(
            "scene_context: obs scene table loaded scenes={}",
            map.scenes.len()
        );
    }
    Ok(map)
}

/// #88 S1: build the scene context service. With a scene table it also reads
/// the OBS program scene (read-only, its own loopback-checked session without
/// an event stream, polled off the main loop).
pub(crate) fn build_scene_context_service(
    profile: CompositionProfile,
    obs_scene_map: Option<ObsSceneMap>,
) -> Result<SceneContextService, Box<dyn Error>> {
    let mut config = aivtuber_runtime::SceneContextConfig::default();
    config.obs_max_age_ms = env_u64("AIVTUBER_OBS_SCENE_MAX_AGE_MS", config.obs_max_age_ms)?;
    let Some(map) = obs_scene_map.filter(|map| !map.scenes.is_empty()) else {
        return Ok(SceneContextService::new(config)?);
    };
    config.obs_map = map;
    let mut obs_config = ObsWebSocketConfig::default();
    obs_config.endpoint = env_string("AIVTUBER_OBS_ENDPOINT", &obs_config.endpoint);
    obs_config.password = env_optional("AIVTUBER_OBS_PASSWORD").map(SecretString::new);
    // This session only reads the program scene; no event stream.
    obs_config.event_subscriptions = Some(0);
    profile
        .validate_provider_endpoints(&[("scene context OBS", loopback_ws(&obs_config.endpoint))])?;
    let poll_ms = env_u64("AIVTUBER_OBS_SCENE_POLL_MS", 2_000)?;
    if !(250..=30_000).contains(&poll_ms) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AIVTUBER_OBS_SCENE_POLL_MS must be in 250..=30000",
        )
        .into());
    }
    let service = SceneContextService::with_obs_source(
        config,
        Box::new(ObsProgramSceneSource::new(ObsWebSocketAdapter::new(
            obs_config,
        )?)),
        Duration::from_millis(poll_ms),
    )?;
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!("scene_context: composed obs_poll_ms={poll_ms}");
    }
    Ok(service)
}

/// #90 S1b: build the opt-in perception service (never enabled here).
///
/// Needs the local profile (loopback-only), a loopback OBS websocket and a
/// loopback multimodal model server; anything else is a startup error. The
/// vision model needs about 12 GB of VRAM next to the local stack
/// (docs/local-profile-quickstart.adoc, EXP-0004c); the runtime latency probe
/// turns a spill into a visible `suspended` status rather than a stall.
pub(crate) fn build_perception_service(
    profile: CompositionProfile,
    started: Instant,
) -> Result<PerceptionService, Box<dyn Error>> {
    if !profile.local_stack() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AIVTUBER_PERCEPTION requires AIVTUBER_MODE=local or hybrid (loopback-only model and OBS endpoints)",
        )
        .into());
    }
    let alias = env_string("AIVTUBER_PERCEPTION_ALIAS", "game-main");
    let obs_source = env_optional("AIVTUBER_PERCEPTION_OBS_SOURCE").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "AIVTUBER_PERCEPTION_OBS_SOURCE (the one OBS source to capture) is required with AIVTUBER_PERCEPTION",
        )
    })?;
    let vlm_endpoint = env_optional("AIVTUBER_PERCEPTION_VLM_ENDPOINT").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "AIVTUBER_PERCEPTION_VLM_ENDPOINT (loopback chat-completions URL of the multimodal server) is required with AIVTUBER_PERCEPTION",
        )
    })?;
    let mut obs_config = ObsWebSocketConfig::default();
    obs_config.endpoint = env_string("AIVTUBER_OBS_ENDPOINT", &obs_config.endpoint);
    obs_config.password = env_optional("AIVTUBER_OBS_PASSWORD").map(SecretString::new);
    // This session only issues requests; no event stream.
    obs_config.event_subscriptions = Some(0);
    profile.validate_provider_endpoints(&[
        ("perception OBS", loopback_ws(&obs_config.endpoint)),
        (
            "perception model",
            verify_loopback_endpoint(&vlm_endpoint, &SystemHostResolver).is_ok(),
        ),
    ])?;

    let format = ScreenImageFormat::parse(&env_string("AIVTUBER_PERCEPTION_IMAGE_FORMAT", "png"))
        .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "AIVTUBER_PERCEPTION_IMAGE_FORMAT must be png or jpeg",
        )
    })?;
    let language = CaptionLanguage::parse(&env_string("AIVTUBER_PERCEPTION_LANGUAGE", "ja"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "AIVTUBER_PERCEPTION_LANGUAGE must be ja or en",
            )
        })?;
    let width = narrow_u32(
        "AIVTUBER_PERCEPTION_IMAGE_WIDTH",
        env_u64("AIVTUBER_PERCEPTION_IMAGE_WIDTH", 768)?,
    )?;
    let height = narrow_u32(
        "AIVTUBER_PERCEPTION_IMAGE_HEIGHT",
        env_u64("AIVTUBER_PERCEPTION_IMAGE_HEIGHT", 432)?,
    )?;

    let defaults = PerceptionGovernorConfig::default();
    let governor = PerceptionGovernorConfig {
        cadence_ms: env_u64("AIVTUBER_PERCEPTION_CADENCE_MS", defaults.cadence_ms)?,
        gate_settle_ms: env_u64(
            "AIVTUBER_PERCEPTION_GATE_SETTLE_MS",
            defaults.gate_settle_ms,
        )?,
        caption_deadline_ms: env_u64(
            "AIVTUBER_PERCEPTION_CAPTION_DEADLINE_MS",
            defaults.caption_deadline_ms,
        )?,
        probe_interval_ms: env_u64(
            "AIVTUBER_PERCEPTION_PROBE_INTERVAL_MS",
            defaults.probe_interval_ms,
        )?,
        probe_ceiling_ms: env_u64(
            "AIVTUBER_PERCEPTION_PROBE_CEILING_MS",
            defaults.probe_ceiling_ms,
        )?,
        ..defaults
    };
    governor.validate()?;
    let tracker_defaults = aivtuber_runtime::PerceptionConfig::default();
    let describer = ScreenDescriber::new(ScreenDescriberConfig {
        endpoint: vlm_endpoint,
        model_id: env_string("AIVTUBER_PERCEPTION_MODEL", "qwen3-vl-4b-instruct-q4_k_m"),
        request_timeout: Duration::from_millis(governor.caption_deadline_ms.saturating_mul(2)),
        max_tokens: 96,
        caption_language: language,
        label_vocabulary: tracker_defaults.label_vocabulary.clone(),
        image_width: width,
        image_height: height,
        image_format: format,
    })?;
    let clock: Arc<dyn AdapterClock> = Arc::new(LoopClock(started));
    // Filled only by the operator's authenticated enable; never from config.
    let grant: aivtuber_adapters::CaptureGrant = Arc::new(Mutex::new(None));
    let source = ObsScreenSource::new(
        ObsWebSocketAdapter::new(obs_config)?,
        grant.clone(),
        ObsScreenshotRequest {
            source_name: obs_source,
            format,
            width,
            height,
            compression_quality: -1,
        },
    );
    let producer = ScreenObservationProducer::new(
        Box::new(source),
        describer,
        ScreenProducerConfig {
            source_alias: alias.clone(),
            validity_ms: env_u64("AIVTUBER_PERCEPTION_VALIDITY_MS", 20_000)?,
            provider_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        clock,
    )?;
    let known_secrets: Vec<String> = [
        "AIVTUBER_OBS_PASSWORD",
        "AIVTUBER_CONTROL_SECRET",
        "AIVTUBER_JEV_API_KEY",
        "AIVTUBER_THINKING_API_KEY",
    ]
    .iter()
    .filter_map(|name| env_optional(name))
    .collect();
    let service = PerceptionService::start(
        PerceptionServiceConfig {
            alias: alias.clone(),
            governor,
            label_vocabulary: None,
        },
        Box::new(ProducerWorker::new(producer)),
        aivtuber_telemetry::SecretRedactor::new(known_secrets),
        grant,
    )?;
    eprintln!(
        "perception: composed (off until enabled) alias={alias} cadence_ms={} probe_ceiling_ms={} requires ~12 GB VRAM for the local stack",
        governor.cadence_ms, governor.probe_ceiling_ms
    );
    Ok(service)
}

/// #335: start the local operation panel when enabled (`AIVTUBER_PANEL`; on by
/// default in the local profile). A panel that is only on by default degrades
/// to a log line when it cannot start; an explicit request is a startup error.
pub(crate) fn start_panel(
    profile: CompositionProfile,
    ingress: &Arc<LocalControlIngress>,
    control_secret: &[u8; 32],
    avatar_launch_url: Option<String>,
    voice_search_endpoint: Option<String>,
    avatar_staging_dir: Option<PathBuf>,
) -> Result<Option<PanelService>, Box<dyn Error>> {
    let Some(settings) = panel_settings(profile.local_stack(), &|name| env_optional(name))? else {
        return Ok(None);
    };
    match PanelService::start(
        &settings,
        Arc::clone(ingress),
        control_secret,
        avatar_launch_url,
        voice_search_endpoint,
        avatar_staging_dir,
    ) {
        Ok(service) => {
            // Deliberate operator console output, like the avatar URL: it
            // carries the per-launch panel secret in the fragment. Do not
            // capture stderr into shared logs.
            eprintln!(
                "local operation panel URL (contains a per-launch secret, do not share or log): {}",
                service.launch_url()
            );
            Ok(Some(service))
        }
        Err(error) if settings.explicit => Err(format!("AIVTUBER_PANEL: {error}").into()),
        Err(error) => {
            eprintln!(
                "panel: not started ({error}); set AIVTUBER_PANEL_PORT or AIVTUBER_PANEL=false"
            );
            Ok(None)
        }
    }
}
