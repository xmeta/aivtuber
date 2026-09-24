use aivtuber_adapters::{
    ObsWebSocketAdapter, ObsWebSocketConfig, ProcessAudioConfig, ProcessAudioPlayer, SecretString,
    VTubeStudioAdapter, VTubeStudioConfig,
};
use aivtuber_app::{
    AdapterHealth, AvatarOutput, IntentRoutePlanner, NoopAvatarOutput, NoopStreamOutput,
    ObsStreamOutput, ProductionApp, StreamOutput, VtsAvatarOutput, VtsPlaybackConfig,
};
use aivtuber_asset_store::{AssetStore, RuntimeCompatibility};
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, LocalControlIngress, OperatorCommandInput,
};
use aivtuber_runtime::{
    CachedPerformer, CachedPlaybackConfig, LocalVisemeStore, SecurityRuntime, SecurityRuntimeConfig,
};
use aivtuber_scheduler::{Scheduler, SchedulerConfig};
use aivtuber_telemetry::SecretRedactor;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::sync::mpsc;
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

    let scheduler_config = SchedulerConfig::default();
    let performer = CachedPerformer::new(
        AssetStore::new(descriptors, compatibility),
        Scheduler::new(scheduler_config),
        CachedPlaybackConfig::default(),
    );
    let security = SecurityRuntime::new(
        SecurityRuntimeConfig::default(),
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

    let mut app = ProductionApp::new(
        security,
        performer,
        visemes,
        IntentRoutePlanner,
        Box::new(audio),
        avatar,
        stream,
        max_lateness_ms,
    );
    let preload = app.startup()?;
    eprintln!(
        "aivtuber-app ready: indexed={} usable={} preloaded={}",
        preload.indexed.indexed, preload.indexed.usable, preload.preloaded
    );
    report_degraded(app.health());

    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if sender.send(line.into_bytes()).is_err() {
                break;
            }
        }
    });

    let started = Instant::now();
    let mut sequence_seed = 0_u64;
    let mut next_maintenance_ms = 0_u64;

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
                let now_ms = elapsed_ms(started);
                app.shutdown(now_ms);
                break;
            }
        }

        let now_ms = elapsed_ms(started);
        app.tick(now_ms);
        if now_ms >= next_maintenance_ms {
            app.maintain_adapters();
            next_maintenance_ms = now_ms.saturating_add(1_000);
        }
    }

    Ok(())
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
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
