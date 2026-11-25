use super::env::*;
use aivtuber_adapters::speech_tts::pins::{SUPERTONIC_3_INT8, SUPERTONIC_MODEL_ALIAS};
use aivtuber_adapters::speech_tts::supervisor::LivenessGatedTts;
use aivtuber_adapters::{
    LocalAvatarHandle, NormalizedHttpTtsAdapter, NormalizedHttpTtsConfig, OpenAiResponsesAdapter,
    OpenAiResponsesConfig, SecretString, StreamLimits, SystemHostResolver, ThinkingAuthentication,
    endpoint_origin, verify_loopback_endpoint,
};
use aivtuber_app::{
    AIVIS_ENDPOINT_VAR, AIVIS_KEY_FILE_VAR, AIVIS_KEY_VAR, AIVIS_TIMEOUT_VAR, AIVIS_VISEMES_VAR,
    AVATAR_DIR_VAR, AVATAR_SETTINGS_VAR, AivisBridge, AvatarController, AvatarEnvironment,
    CHAT_SETTINGS_VAR, ChatIngressController, ChatIngressEnvironment, CompositionProfile,
    DEFAULT_WARMUP_TIMEOUT_MS, GenerativeRuntime, IrodoriBridge, ModelSwitcher, PhraseMode,
    PreviewSlot, PreviewThinking, RawIngressConfig, RuntimeRetentionPolicy, SwitchWarmup,
    ThinkingEgressAllowlist, VOICE_SETTINGS_VAR, VoiceController, VoiceEnvironment, VoicePaths,
    VoicevoxBridge, WarmupMode, YOUTUBE_API_BASE_VAR, YOUTUBE_KEY_FILE_VAR,
    apply_thinking_provider_env, is_acceptable_api_base, is_valid_key, openrouter_preset_active,
    read_voice_settings, remembered_model, start_thinking_warmup, switch_warmup_for,
    warmup_timeout,
};
use aivtuber_asset_store::RuntimeCompatibility;
use aivtuber_domain::ThinkingModelCatalog;
use aivtuber_generative::{
    AffectConfig, GenerativePipeline, PerformanceCompiler, PerformanceCompilerConfig, PhraseConfig,
    SpeechLexicon, StreamPolicy,
};
use aivtuber_reflex::{JevAdapter, JevAdapterConfig, JevApiKey};
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// What the composition root built for the generative path.
pub(crate) struct GenerativeParts {
    pub(crate) runtime: Option<GenerativeRuntime>,
    pub(crate) model_switcher: Option<ModelSwitcher>,
    /// #360: live voice settings.
    pub(crate) voice: Option<VoiceController>,
    /// #364: the allowlisted remote host Thinking traffic goes to under the
    /// hybrid profile; `None` when nothing leaves the machine.
    pub(crate) thinking_egress_host: Option<String>,
    /// #397: the loopback decision for the Thinking endpoint; `true` when no
    /// generative runtime is composed (nothing is sent anywhere).
    pub(crate) thinking_endpoint_is_loopback: bool,
    /// #367: Thinking usage totals and the optional spend cap.
    pub(crate) thinking_meter: Option<aivtuber_app::ThinkingMeter>,
    /// #426: counters of the opt-in System One affect judge.
    pub(crate) affect_judge_stats: Option<aivtuber_generative::AffectJudgeStats>,
    /// #447: the baseline expression the face settles to after a reply; `None`
    /// unless affect expression is enabled.
    pub(crate) face_settle_baseline: Option<(String, f64)>,
}

/// The environment-selected TTS bridges and the directories the voice
/// settings (#360) start their own bridge under.
pub(crate) struct TtsBackends<'a> {
    pub(crate) voicevox: Option<&'a VoicevoxBridge>,
    pub(crate) aivis: Option<&'a AivisBridge>,
    pub(crate) irodori: Option<&'a IrodoriBridge>,
    pub(crate) audio_root: &'a std::path::Path,
    pub(crate) pack_root: &'a std::path::Path,
}

pub(crate) fn build_generative_runtime(
    compatibility: &RuntimeCompatibility,
    retention: RuntimeRetentionPolicy,
    profile: CompositionProfile,
    supervised_tts: Option<Arc<AtomicBool>>,
    persona: Option<&aivtuber_app::persona::PersonaSheet>,
    backends: TtsBackends<'_>,
) -> Result<GenerativeParts, Box<dyn Error>> {
    let TtsBackends {
        voicevox,
        aivis,
        irodori,
        audio_root,
        pack_root,
    } = backends;
    if !env_bool("AIVTUBER_GENERATIVE_ENABLED", false)? {
        return Ok(GenerativeParts {
            runtime: None,
            model_switcher: None,
            voice: None,
            thinking_egress_host: None,
            thinking_endpoint_is_loopback: true,
            thinking_meter: None,
            affect_judge_stats: None,
            face_settle_baseline: None,
        });
    }

    // #367: optional session spend cap; invalid is a startup error.
    let cost_cap = env_optional(aivtuber_app::THINKING_COST_CAP_VAR)
        .map(|raw| aivtuber_app::parse_cost_cap_usd(&raw))
        .transpose()?;
    let thinking_meter = aivtuber_app::ThinkingMeter::new(cost_cap);
    if let Some(cap) = cost_cap {
        eprintln!(
            "thinking cost cap: {cap} micro-USD (Thinking is suspended for the session once the provider-reported cost reaches it)"
        );
    }
    let mut thinking_config = OpenAiResponsesConfig::default();
    // #363: wire selector and the OpenRouter preset. The explicit endpoint and
    // model overrides below still win over the preset.
    apply_thinking_provider_env(&mut thinking_config, &|name| env_optional(name))?;
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
    // The bridge endpoint is loopback either way; the Aivis Cloud egress is
    // inside the bridge, not a provider endpoint the profile check sees.
    let tts_endpoint = match (voicevox, aivis, irodori) {
        (Some(bridge), _, _) => bridge.tts_endpoint().to_owned(),
        (None, Some(bridge), _) => bridge.tts_endpoint().to_owned(),
        (None, None, Some(bridge)) => bridge.tts_endpoint().to_owned(),
        (None, None, None) => required_env("AIVTUBER_TTS_ENDPOINT")?,
    };
    // Per-role egress policy (#364). The local stack's non-Thinking roles are
    // loopback-only; Thinking is loopback-only under `local` and may go to an
    // allowlisted HTTPS host with Bearer auth under `hybrid`.
    let resolver = SystemHostResolver;
    let egress_host_env = env_optional("AIVTUBER_THINKING_EGRESS_HOST");
    if egress_host_env.is_some() && profile != CompositionProfile::Hybrid {
        return Err(
            "AIVTUBER_THINKING_EGRESS_HOST needs AIVTUBER_MODE=hybrid (only that profile may send Thinking off the machine)"
                .into(),
        );
    }
    let egress_allowlist = ThinkingEgressAllowlist::new(
        openrouter_preset_active(&|name| env_optional(name)),
        egress_host_env.as_deref(),
    )?;
    let thinking_endpoint_is_loopback =
        verify_loopback_endpoint(&thinking_config.endpoint, &resolver).is_ok();
    let thinking_egress_host = profile.validate_thinking_endpoint(
        &thinking_config.endpoint,
        thinking_endpoint_is_loopback,
        matches!(thinking_auth, ThinkingAuthentication::Bearer(_)),
        &egress_allowlist,
    )?;
    profile.validate_provider_endpoints(&[(
        "tts",
        verify_loopback_endpoint(&tts_endpoint, &resolver).is_ok(),
    )])?;
    let mut thinking = OpenAiResponsesAdapter::with_authentication(thinking_config, thinking_auth)?;
    // #404: the warm-up carries the character sheet so the first reply does not
    // pay to evaluate it. Set before any clone (the model switcher's warm-up
    // and the pipeline's copy) is made.
    if let Some(sheet) = persona {
        thinking = thinking.with_warmup_context(vec![sheet.thinking_content()]);
    }
    if let Some(name) = env_optional("AIVTUBER_OPENAI_BACKEND_NAME") {
        thinking = thinking.with_backend_name(name);
    }
    // Issue #83 S2: optional provider token streaming into phrase commit.
    let mut phrase_mode = resolve_phrase_mode(profile)?;
    // #83 S3c: on by default for the local profile, only while replies are
    // phrased (AIVTUBER_PHRASE_MODE=buffered there turns it off with it);
    // an explicit AIVTUBER_THINKING_STREAM=on with buffered replies stays an
    // error.
    let stream_default =
        profile.thinking_stream_by_default() && matches!(phrase_mode, PhraseMode::Phrased(_));
    if env_bool("AIVTUBER_THINKING_STREAM", stream_default)? {
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
            // The streamed-reply gate input is bounded by the same cap the
            // adapter enforces on the stream (#83 S3a).
            max_reply_chars: limits.max_reply_chars,
        };
        config.stream = Some(policy);
        config.validate()?;
        if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
            eprintln!(
                "thinking stream: on (idle {} ms, max {} ms, max reply {} chars)",
                limits.idle_timeout.as_millis(),
                policy.max_duration.as_millis(),
                limits.max_reply_chars,
            );
        }
    }
    // Non-secret, replay-relevant identity: where Thinking runs and how it is
    // authenticated. Origin only; no path/query that could carry a token.
    // The wire is appended only when it is not the default, so the existing
    // line is byte-identical for the Responses wire.
    let wire_note = match thinking.wire() {
        aivtuber_adapters::ThinkingWire::Responses => String::new(),
        wire => format!(
            " wire={} data_collection={}",
            wire.label(),
            thinking
                .data_collection()
                .map_or("unset", aivtuber_adapters::DataCollection::label),
        ),
    };
    if aivtuber_app::ready_guide::thinking_line_visible(
        aivtuber_adapters::log_verbosity::diagnostics_enabled(),
        thinking_endpoint_is_loopback,
    ) {
        eprintln!(
            "thinking: endpoint={} auth={} backend={}{wire_note}",
            endpoint_origin(thinking.endpoint()),
            thinking.authentication_label(),
            aivtuber_domain::ThinkingEngine::identity(&thinking).name,
        );
    }

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
    } else if let Some(bridge) = aivis {
        (
            aivtuber_adapters::speech_tts::aivis_cloud::BACKEND_NAME,
            Some(bridge.settings().model_alias()),
            None,
        )
    } else if let Some(bridge) = irodori {
        (
            aivtuber_adapters::speech_tts::irodori::BACKEND_NAME,
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

    let baseline_preset = env_string("AIVTUBER_GENERATED_EXPRESSION_PRESET", "speaking.neutral");
    let compiler = PerformanceCompiler::new(PerformanceCompilerConfig {
        compiler_version: compatibility.compiler_version.clone(),
        avatar_profile: compatibility.avatar_profile.clone(),
        motion_library: compatibility.motion_library.clone(),
        expression_preset: Some(baseline_preset.clone()),
        expression_intensity: 0.4,
    })?;
    // #84 S1: the text-to-affect classifier picks the expression of generated
    // replies from what the loaded pack ships. `AIVTUBER_AFFECT_EXPRESSION=false`
    // restores the fixed baseline expression.
    let affect_expression = env_bool("AIVTUBER_AFFECT_EXPRESSION", true)?;
    let compiler = if affect_expression {
        compiler.with_affect(AffectConfig::new(pack_expression_presets(
            &pack_root.join("descriptors"),
        )))
    } else {
        compiler
    };
    // #426: opt-in System One affect judgement, in parallel with TTS. Default
    // `rules` composes nothing; the S1 classifier is then unchanged.
    let affect_judge_mode = aivtuber_app::AffectJudgeMode::parse(
        env_optional(aivtuber_app::AFFECT_JUDGE_VAR).as_deref(),
    )?;
    affect_judge_mode.validate(
        profile,
        &env_string(
            "AIVTUBER_JEV_ENDPOINT",
            "https://api.typesafe.ai/v1/systemone",
        ),
        affect_expression,
        env_optional("AIVTUBER_JEV_API_KEY").is_some_and(|key| !key.trim().is_empty()),
        phrase_mode.is_phrased(),
    )?;
    let (compiler, affect_judge_stats) = match affect_judge_mode {
        aivtuber_app::AffectJudgeMode::Rules => (compiler, None),
        aivtuber_app::AffectJudgeMode::SystemOne => {
            let hard_cap = Duration::from_millis(env_u64(
                aivtuber_app::AFFECT_JUDGE_DEADLINE_VAR,
                aivtuber_generative::DEFAULT_JUDGE_HARD_CAP.as_millis() as u64,
            )?);
            let adapter = JevAdapter::new(
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
            let runner = aivtuber_generative::AffectJudgeRunner::new(
                Arc::new(aivtuber_app::SystemOneAffectJudge::new(
                    aivtuber_reflex::JevAffectJudge::new(adapter),
                )),
                hard_cap,
            )?;
            // Always printed: this is data leaving the machine.
            eprintln!("{}", aivtuber_app::affect_judge_startup_notice(hard_cap));
            let stats = runner.stats();
            (compiler.with_affect_judge(runner), Some(stats))
        }
    };

    // #360: the voice settings share the adapter's target slot (clones follow a
    // switch) and open the sidecar liveness gate while Aivis is speaking.
    let voice_bypass = Arc::new(AtomicBool::new(false));
    let preview_slot = PreviewSlot::default();
    let voice_controller = build_voice_controller(
        profile,
        voicevox.is_some() || aivis.is_some() || irodori.is_some(),
        &tts,
        &voice_bypass,
        &preview_slot,
        audio_root,
        pack_root,
    )?;
    let tts: Arc<dyn aivtuber_domain::TtsEngine> = match supervised_tts {
        Some(alive) => {
            Arc::new(LivenessGatedTts::new(Arc::new(tts), alive).with_bypass(voice_bypass))
        }
        None => Arc::new(tts),
    };
    let speech_lexicon = build_speech_lexicon()?;
    let warmup_mode = WarmupMode::parse(env_optional("AIVTUBER_THINKING_WARMUP").as_deref())?;
    let warmup_timeout = warmup_timeout(env_u64(
        "AIVTUBER_THINKING_WARMUP_TIMEOUT_MS",
        DEFAULT_WARMUP_TIMEOUT_MS,
    )?)?;
    let warmup_adapter = thinking.clone();
    // #357: a clone shares the adapter's model slot, so a switch made through
    // the selector is seen by the pipeline's copy.
    let model_switcher = build_model_switcher(&warmup_adapter, warmup_timeout)?;
    // The preview override (#360) answers one armed panel line with the fixed
    // sentence; with no voice settings composed the model is used as is.
    // #367: the meter wraps the real engine (the preview line costs nothing and
    // is not counted); it also enforces the optional spend cap.
    let metered: Arc<dyn aivtuber_domain::ThinkingEngine> = Arc::new(
        aivtuber_app::MeteredThinking::new(Arc::new(thinking), thinking_meter.clone()),
    );
    let thinking: Arc<dyn aivtuber_domain::ThinkingEngine> = if voice_controller.is_some() {
        Arc::new(PreviewThinking::new(metered, preview_slot))
    } else {
        metered
    };
    let pipeline = GenerativePipeline::new(thinking, tts, compiler)
        .with_speech_lexicon(Arc::new(speech_lexicon));
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!(
            "generated replies: {}",
            match phrase_mode {
                PhraseMode::Buffered => "buffered (one commit per reply)",
                PhraseMode::Phrased(_) => "phrase mode (one atomic commit per phrase)",
            }
        );
    }
    let runtime =
        GenerativeRuntime::with_execution_config(pipeline, retention.generation_execution_config())
            .map_err(io::Error::other)?
            .with_phrase_mode(phrase_mode)
            .map_err(io::Error::other)?;
    // Last step, so a configuration error above never leaves a warm-up
    // running. Background only; readiness and ingress never wait for it.
    let backend = aivtuber_domain::ThinkingEngine::identity(&warmup_adapter).name;
    let _ = start_thinking_warmup(warmup_mode, &warmup_adapter, &backend, warmup_timeout);
    Ok(GenerativeParts {
        runtime: Some(runtime),
        model_switcher,
        voice: voice_controller,
        thinking_egress_host,
        thinking_endpoint_is_loopback,
        thinking_meter: Some(thinking_meter),
        affect_judge_stats,
        face_settle_baseline: affect_expression.then_some((baseline_preset, 0.4)),
    })
}

/// #360: the live voice settings. `AIVTUBER_VOICE_SETTINGS` names the file
/// that remembers the choice (set by `aivtuber-setup start`, beside the env
/// file); `AIVTUBER_AIVIS_KEY_FILE` the key's own file (default: next to it).
/// `AIVIS_API` still wins over the stored key, and `AIVTUBER_AIVIS_ENDPOINT` /
/// `AIVTUBER_AIVIS_TIMEOUT_MS` / `AIVTUBER_AIVIS_AMPLITUDE_VISEMES` mean what
/// they mean for the environment-selected backend. When the environment already
/// selects a TTS backend (`AIVTUBER_TTS_BACKEND`) the settings stay off: one
/// source of truth for the voice.
pub(crate) fn build_voice_controller(
    profile: CompositionProfile,
    env_selects_backend: bool,
    tts: &NormalizedHttpTtsAdapter,
    bypass: &Arc<AtomicBool>,
    preview: &PreviewSlot,
    audio_root: &std::path::Path,
    pack_root: &std::path::Path,
) -> Result<Option<VoiceController>, Box<dyn Error>> {
    let Some(settings_path) = env_path(VOICE_SETTINGS_VAR) else {
        return Ok(None);
    };
    if !profile.local_stack() {
        return Err(format!(
            "{VOICE_SETTINGS_VAR} is supported with AIVTUBER_MODE=local or hybrid only"
        )
        .into());
    }
    if env_selects_backend {
        eprintln!(
            "voice settings: off ({VOICE_SETTINGS_VAR} is set but AIVTUBER_TTS_BACKEND selects the voice)"
        );
        return Ok(None);
    }
    let key_file = env_path(AIVIS_KEY_FILE_VAR)
        .unwrap_or_else(|| settings_path.with_file_name("local-aivis-key.txt"));
    let env_key = match env_optional(AIVIS_KEY_VAR) {
        Some(key) if is_valid_key(key.trim()) => Some(SecretString::new(key.trim())),
        Some(_) => {
            return Err(format!(
                "{AIVIS_KEY_VAR} is not a usable key (control characters, whitespace or too long); check how it was set"
            )
            .into());
        }
        None => None,
    };
    let endpoint = env_optional(AIVIS_ENDPOINT_VAR)
        .map_or_else(VoiceEnvironment::default_endpoint, |v| v.trim().to_owned());
    aivtuber_adapters::speech_tts::aivis_cloud::validate_origin(&endpoint)
        .map_err(|error| format!("{AIVIS_ENDPOINT_VAR}: {error}"))?;
    let synth_timeout_ms = env_u64(AIVIS_TIMEOUT_VAR, 12_000)?;
    if !(1..=120_000).contains(&synth_timeout_ms) {
        return Err(format!("{AIVIS_TIMEOUT_VAR} must be an integer from 1 to 120000").into());
    }
    let amplitude_visemes = env_bool(
        AIVIS_VISEMES_VAR,
        env_bool("AIVTUBER_TTS_SIDECAR_AMPLITUDE_VISEMES", false)?,
    )?;
    let initial = read_voice_settings(&settings_path).unwrap_or_default();
    Ok(Some(VoiceController::new(
        VoiceEnvironment {
            paths: VoicePaths {
                settings: settings_path,
                key_file,
            },
            env_key,
            endpoint,
            synth_timeout: Duration::from_millis(synth_timeout_ms),
            amplitude_visemes,
            audio_root: audio_root.to_path_buf(),
            pack_root: pack_root.to_path_buf(),
        },
        tts.clone(),
        Arc::clone(bypass),
        preview.clone(),
        initial,
    )))
}

/// #361: chat connections from the panel. `AIVTUBER_CHAT_INGRESS_SETTINGS`
/// names the file that remembers the last channel and video (set by
/// `aivtuber-setup start`, beside the env file); `AIVTUBER_YOUTUBE_KEY_FILE`
/// the YouTube key's own file (default: next to it). `YOUTUBE_API_KEY` wins
/// over the stored key. The sidecars are the `twitch-chat-ingress` and
/// `youtube-chat-ingress` programs next to this binary.
/// `AIVTUBER_YOUTUBE_API_BASE` points the YouTube sidecar at another origin
/// (https, or http on loopback for a fixture server); it is an operator
/// setting, not reachable from the panel.
pub(crate) fn build_chat_ingress(
    raw: RawIngressConfig,
) -> Result<Option<ChatIngressController>, Box<dyn Error>> {
    let Some(settings_path) = env_path(CHAT_SETTINGS_VAR) else {
        return Ok(None);
    };
    let key_file = env_path(YOUTUBE_KEY_FILE_VAR)
        .unwrap_or_else(|| settings_path.with_file_name("local-youtube-key.txt"));
    let env_key = match env_optional("YOUTUBE_API_KEY") {
        Some(key) if is_valid_key(key.trim()) => Some(SecretString::new(key.trim())),
        Some(_) => {
            return Err(
                "YOUTUBE_API_KEY is not a usable key (control characters, whitespace or too long); check how it was set"
                    .to_owned()
                    .into(),
            );
        }
        None => None,
    };
    let api_base = match env_optional(YOUTUBE_API_BASE_VAR) {
        Some(base) if is_acceptable_api_base(base.trim()) => Some(base.trim().to_owned()),
        Some(_) => {
            return Err(format!(
                "{YOUTUBE_API_BASE_VAR} must be an https URL, or an http URL on loopback"
            )
            .into());
        }
        None => None,
    };
    let exe = env::current_exe()?;
    let program_dir = exe
        .parent()
        .ok_or("cannot locate the directory of the app binary")?;
    Ok(Some(ChatIngressController::new(
        ChatIngressEnvironment::new(settings_path, key_file, env_key, program_dir, api_base, raw),
    )))
}

/// #362: the avatar store, when `AIVTUBER_AVATAR_SETTINGS` and
/// `AIVTUBER_AVATAR_DIR` are set (`aivtuber-setup start` sets both beside the
/// env file). The sample is the page directory's `model.vrm`. A store that
/// cannot be opened is a startup error: the operator asked for it.
pub(crate) fn build_avatar_controller(
    handle: LocalAvatarHandle,
    page_dir: Option<&std::path::Path>,
) -> Result<Option<AvatarController>, Box<dyn Error>> {
    let (Some(settings), Some(dir)) = (env_path(AVATAR_SETTINGS_VAR), env_path(AVATAR_DIR_VAR))
    else {
        return Ok(None);
    };
    let sample = page_dir
        .map(|d| d.join("model.vrm"))
        .filter(|path| path.is_file());
    AvatarController::new(
        AvatarEnvironment {
            settings,
            dir,
            sample,
        },
        handle,
    )
    .map(Some)
    .map_err(|error| format!("{AVATAR_DIR_VAR}: {error}").into())
}

/// #357/#365: the Thinking model allowlist (`AIVTUBER_OPENAI_MODEL_CHOICES`)
/// and the file that remembers the choice (`AIVTUBER_OPENAI_MODEL_SETTINGS`).
/// A loopback unauthenticated endpoint (a local llama-server router) is
/// switched and warmed up; a Bearer endpoint can be switched only under the
/// OpenRouter preset, where a switch merely changes the request `model` from
/// the fixed allowlist and makes no call of its own.
pub(crate) fn build_model_switcher(
    adapter: &OpenAiResponsesAdapter,
    timeout: Duration,
) -> Result<Option<ModelSwitcher>, Box<dyn Error>> {
    let Some(choices) = env_optional("AIVTUBER_OPENAI_MODEL_CHOICES") else {
        return Ok(None);
    };
    let warmup = switch_warmup_for(
        adapter.authentication_label(),
        openrouter_preset_active(&|name| env_optional(name)),
    )?;
    let catalog = ThinkingModelCatalog::parse_choices(&choices)
        .map_err(|error| format!("AIVTUBER_OPENAI_MODEL_CHOICES: {error}"))?;
    let settings = env_optional("AIVTUBER_OPENAI_MODEL_SETTINGS").map(PathBuf::from);
    // The local launcher applies the remembered choice before the app starts;
    // with a remote provider there is no launcher, so apply it here. A name
    // outside the allowlist is ignored.
    if warmup == SwitchWarmup::Skip
        && let Some(model) = settings
            .as_deref()
            .and_then(|path| remembered_model(path, &catalog))
    {
        adapter.select_model(&model.alias, Some(&model.version))?;
    }
    Ok(Some(ModelSwitcher::new(
        catalog,
        adapter.clone(),
        settings,
        timeout,
        warmup,
    )?))
}

/// ADR-0004 / issue #222 S1: the reading lexicon applied between the gated
/// display text and TTS. `AIVTUBER_SPEECH_LEXICON` names an operator lexicon
/// file (schema `speech-lexicon.schema.json`) merged over the committed starter
/// lexicon, which `AIVTUBER_SPEECH_LEXICON_DEFAULTS=false` turns off. A file that
/// cannot be read or violates a bound is a startup error naming the entry;
/// speech never silently runs without the lexicon the operator asked for.
pub(crate) fn build_speech_lexicon() -> Result<SpeechLexicon, Box<dyn Error>> {
    let include_builtin = env_bool("AIVTUBER_SPEECH_LEXICON_DEFAULTS", true)?;
    let operator_file = env_path("AIVTUBER_SPEECH_LEXICON");
    let lexicon = SpeechLexicon::load(include_builtin, operator_file.as_deref())
        .map_err(|error| format!("AIVTUBER_SPEECH_LEXICON: {error}"))?;
    // The identity is not secret and is replay-relevant; entries are not logged.
    if aivtuber_adapters::log_verbosity::diagnostics_enabled() {
        eprintln!(
            "speech lexicon: {} entries, projection {}",
            lexicon.len(),
            lexicon.identity().key()
        );
    }
    Ok(lexicon)
}

/// `AIVTUBER_PHRASE_MODE=buffered|phrase|auto` (issue #83 S1). `auto` (the
/// default) follows the composition profile; `AIVTUBER_PHRASE_TTS_TIMEOUT_MS`
/// bounds the synthesis of one phrase (default 12000), not of the whole reply.
pub(crate) fn resolve_phrase_mode(
    profile: CompositionProfile,
) -> Result<PhraseMode, Box<dyn Error>> {
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
