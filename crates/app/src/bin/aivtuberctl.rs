//! `aivtuberctl` — local operator control client (issues #56 and #68).
//!
//! Sends one line-delimited JSON request over the daemon's local operator
//! control endpoint (Windows named pipe / Unix domain socket) and prints the
//! response. The secret is supplied via `AIVTUBER_CONTROL_SECRET` (the same
//! value the daemon uses) or `--secret` on the command line.
//!
//! Unmute is intentionally not implemented: mute is a latching emergency
//! state and resuming output requires restarting the daemon. The `diagnose`
//! subcommand captures a bounded, redacted support bundle over the same
//! status path (secret verification only, no control authority); see
//! `docs/production-runtime.adoc` for the security model.
//!
//! `memory-export` (issue #321) asks the daemon for its memory writer's live
//! entries in seed-file format and writes them to the path the operator names.
//! The daemon never writes a file; this client refuses to overwrite one, so an
//! export can never replace a hand-curated seed file.
//!
//! `ingress` (issue #361) connects or disconnects Twitch or YouTube chat, like
//! the operation panel. The YouTube API key is deliberately not a subcommand
//! (it would travel in a command line and a request log): it is entered in the
//! panel, or set as `YOUTUBE_API_KEY`.
//!
//! `avatar` (issue #362) shows and changes the VRM avatar, like the operation
//! panel: `status`, `select`, `delete`, and `confirm` / `discard` for a model
//! uploaded in the panel. The upload itself is deliberately not a subcommand:
//! the file is chosen and its licence read in the panel, which is where the
//! operator confirms it.
//!
//! `voice` (issue #360) changes the voice at runtime, like the operation panel:
//! the local voice or Aivis Cloud, the Aivis style and the tuning, and a
//! preview. The API key is deliberately not a subcommand: it would travel in a
//! command line and a request log, so it is entered in the panel (or set as
//! `AIVIS_API`).

use aivtuber_app::{MemorySeed, OperatorRequest, OperatorResponse};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

#[derive(Parser)]
#[command(
    name = "aivtuberctl",
    about = "Local operator control client for the aivtuber daemon",
    version
)]
struct Cli {
    /// Override the operator control endpoint name (default
    /// AIVTUBER_CONTROL_ENDPOINT or "aivtuber-operator-control").
    #[arg(long, env = "AIVTUBER_CONTROL_ENDPOINT")]
    endpoint: Option<String>,

    /// Operator session secret. Prefer the AIVTUBER_CONTROL_SECRET
    /// environment variable so the secret stays out of shell history.
    #[arg(long, env = "AIVTUBER_CONTROL_SECRET", hide_env_values = true)]
    secret: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Clone, PartialEq)]
enum Command {
    /// Adapter/runtime health snapshot (no secret material).
    Status,
    /// Stop and cancel current plus queued performer work.
    Stop,
    /// Latch the operator mute state (unmute is restart-only).
    Mute,
    /// Capture a bounded, redacted diagnostic/support bundle (issue #68).
    /// Uses the status path (secret verification only, no control authority).
    Diagnose,
    /// Enable opt-in live screen perception (issue #90). Needs the daemon to
    /// run with AIVTUBER_PERCEPTION=true; starts with a latency probe.
    PerceptionOn,
    /// Disable live screen perception. Takes effect before the next capture.
    PerceptionOff,
    /// Export the memory writer's live entries in seed-file format (issue
    /// #321). Needs the daemon to run with AIVTUBER_MEMORY_WRITER=true. Writes
    /// a new file at --out and refuses to overwrite an existing one; review and
    /// promote it by hand.
    MemoryExport {
        /// Destination file. Must not exist.
        #[arg(long)]
        out: PathBuf,
    },
    /// Switch the local Thinking model (issue #357), for example
    /// `aivtuberctl model qwen3-4b-instruct-2507`. The alias must be one the
    /// launcher listed (`aivtuberctl status` prints them); the operation panel
    /// sends the same request.
    Model {
        /// A model alias from the daemon's allowlist.
        alias: String,
    },
    /// Change the voice (issue #360); needs the daemon to run with the voice
    /// settings (the local profile started by `aivtuber-setup start`).
    Voice {
        #[command(subcommand)]
        action: VoiceCommand,
    },
    /// Connect or disconnect stream chat (issue #361); needs the daemon to run
    /// with chat connections (the local profile started by `aivtuber-setup
    /// start`). Chat is read-only and arrives as untrusted public chat.
    Ingress {
        #[command(subcommand)]
        action: IngressCommand,
    },
    /// Show or change the VRM avatar (issue #362); needs the daemon to run
    /// with the avatar store (the local profile started by `aivtuber-setup
    /// start`). A model is uploaded in the operation panel, where its licence
    /// is shown.
    Avatar {
        #[command(subcommand)]
        action: AvatarCommand,
    },
}

#[derive(Subcommand, Clone, PartialEq, Eq)]
enum AvatarCommand {
    /// The active model, the stored ones and the pending candidate, with
    /// their licence summaries (the `avatar` lines of `aivtuberctl status`).
    Status,
    /// Switch to `sample` or to a stored model (its hash, or at least 8
    /// characters of it). The avatar page switches without a restart.
    Select {
        /// `sample`, or a model hash (prefix of at least 8 hex characters).
        id: String,
    },
    /// Delete a stored model that is not the active one.
    Delete {
        /// A model hash (prefix of at least 8 hex characters).
        id: String,
    },
    /// Confirm the model uploaded in the panel after reading its licence: it
    /// is stored and becomes the active model.
    Confirm {
        /// The candidate's hash (prefix of at least 8 hex characters).
        id: String,
    },
    /// Drop the model uploaded in the panel without using it.
    Discard,
}

/// The `avatar.*` argument for an avatar subcommand: the model id as typed;
/// the daemon checks its shape and resolves it against its own store.
fn avatar_argument(action: &AvatarCommand) -> Option<String> {
    match action {
        AvatarCommand::Select { id }
        | AvatarCommand::Delete { id }
        | AvatarCommand::Confirm { id } => Some(id.clone()),
        AvatarCommand::Status | AvatarCommand::Discard => None,
    }
}

#[derive(Subcommand, Clone, PartialEq, Eq)]
enum IngressCommand {
    /// Start reading a chat. The daemon validates the target.
    Connect {
        /// `twitch` or `youtube`.
        #[arg(value_parser = ["twitch", "youtube"])]
        platform: String,
        /// A Twitch channel name, or a YouTube live video id or link.
        target: String,
    },
    /// Stop reading a chat and end its sidecar.
    Disconnect {
        /// `twitch` or `youtube`.
        #[arg(value_parser = ["twitch", "youtube"])]
        platform: String,
    },
    /// Remove the stored YouTube API key (and disconnect YouTube if no other
    /// key is set).
    KeyRemove,
}

/// The `ingress.*` argument for an ingress subcommand, as JSON text.
fn ingress_argument(action: &IngressCommand) -> Option<String> {
    match action {
        IngressCommand::Connect { platform, target } => {
            Some(serde_json::json!({ "platform": platform, "target": target }).to_string())
        }
        IngressCommand::Disconnect { platform } => {
            Some(serde_json::json!({ "platform": platform }).to_string())
        }
        IngressCommand::KeyRemove => None,
    }
}

#[derive(Subcommand, Clone, PartialEq)]
enum VoiceCommand {
    /// Back to the local voice. Stops all egress.
    Local,
    /// Switch to Aivis Cloud with the model, credit line and API key already
    /// set (in the panel). Reply text is sent to Aivis Cloud, so this needs
    /// --confirm-egress.
    Aivis {
        /// Confirm that reply text is sent to Aivis Cloud (an internet service).
        #[arg(long)]
        confirm_egress: bool,
    },
    /// Change what Aivis Cloud speaks with. Only the given fields change.
    Set {
        /// AivisHub model UUID (resets speaker, style and credit line unless given).
        #[arg(long)]
        model: Option<String>,
        /// Display name of the model (informational).
        #[arg(long)]
        model_name: Option<String>,
        /// Speaker UUID of a multi-speaker model.
        #[arg(long)]
        speaker: Option<String>,
        /// Style id 0..=31.
        #[arg(long)]
        style: Option<u8>,
        /// The credit line the model requires.
        #[arg(long)]
        credit: Option<String>,
        /// Speaking rate 0.5..=2.0.
        #[arg(long)]
        speaking_rate: Option<f32>,
        /// Pitch -1.0..=1.0.
        #[arg(long, allow_negative_numbers = true)]
        pitch: Option<f32>,
        /// Volume 0.0..=2.0.
        #[arg(long)]
        volume: Option<f32>,
        /// Emotional intensity 0.0..=2.0.
        #[arg(long)]
        emotional_intensity: Option<f32>,
    },
    /// Speak the fixed preview sentence with the current voice.
    Preview,
    /// Remove the stored API key (and leave Aivis if no other key is set).
    KeyRemove,
    /// Ask Aivis Cloud whether the key works; read the result with `status`.
    KeyCheck,
}

/// The `voice.apply` patch for a voice subcommand, as JSON text.
fn voice_patch(action: &VoiceCommand) -> Option<String> {
    let patch = match action {
        VoiceCommand::Local => serde_json::json!({ "backend": "local" }),
        VoiceCommand::Aivis { confirm_egress } => {
            serde_json::json!({ "backend": "aivis", "confirm_egress": confirm_egress })
        }
        VoiceCommand::Set {
            model,
            model_name,
            speaker,
            style,
            credit,
            speaking_rate,
            pitch,
            volume,
            emotional_intensity,
        } => {
            let mut patch = serde_json::Map::new();
            for (name, value) in [
                ("model_uuid", model.as_ref().map(|v| serde_json::json!(v))),
                (
                    "model_name",
                    model_name.as_ref().map(|v| serde_json::json!(v)),
                ),
                (
                    "speaker_uuid",
                    speaker.as_ref().map(|v| serde_json::json!(v)),
                ),
                ("style_id", style.map(|v| serde_json::json!(v))),
                ("credit", credit.as_ref().map(|v| serde_json::json!(v))),
                ("speaking_rate", speaking_rate.map(|v| serde_json::json!(v))),
                ("pitch", pitch.map(|v| serde_json::json!(v))),
                ("volume", volume.map(|v| serde_json::json!(v))),
                (
                    "emotional_intensity",
                    emotional_intensity.map(|v| serde_json::json!(v)),
                ),
            ] {
                if let Some(value) = value {
                    patch.insert(name.to_owned(), value);
                }
            }
            serde_json::Value::Object(patch)
        }
        VoiceCommand::Preview | VoiceCommand::KeyRemove | VoiceCommand::KeyCheck => return None,
    };
    Some(patch.to_string())
}

/// The avatar lines of `status`. Model names and licence fields come from the
/// file and are untrusted text: they are printed with `{:?}` so nothing in
/// them can act as terminal control.
fn print_avatar_status(avatar: &aivtuber_app::AvatarStatus) {
    println!(
        "avatar active={} page_shows={} max_models={} credit={:?} last_error={:?} save_error={:?}",
        avatar.active,
        avatar.renderer_model.as_deref().unwrap_or("-"),
        avatar.max_models,
        avatar.credit,
        avatar.last_error,
        avatar.save_error,
    );
    let mut rows: Vec<(&str, &aivtuber_app::AvatarModelInfo)> = Vec::new();
    if let Some(sample) = &avatar.sample {
        rows.push(("sample", sample));
    }
    rows.extend(avatar.models.iter().map(|m| ("stored", m)));
    if let Some(pending) = &avatar.pending {
        rows.push(("pending", pending));
    }
    for (kind, model) in rows {
        println!(
            "avatar {kind} id={} size={} name={:?} warnings={:?}",
            model.id, model.size, model.name, model.warnings,
        );
        if let Some(meta) = &model.meta {
            println!(
                "avatar   {kind} licence: spec={:?} authors={:?} avatar_permission={:?} commercial_usage={:?} license_name={:?} credit_notation={:?} modification={:?} redistribution={:?} license_url={:?} contact={:?} copyright={:?}",
                meta.spec,
                meta.authors,
                meta.avatar_permission,
                meta.commercial_usage,
                meta.license_name,
                meta.credit_notation,
                meta.modification,
                meta.allow_redistribution,
                meta.license_url,
                meta.contact,
                meta.copyright,
            );
        }
    }
}

fn action_name(command: &Command) -> &'static str {
    match command {
        Command::Status => "status",
        Command::Stop => "stop",
        Command::Mute => "mute",
        Command::Diagnose => "diagnose",
        Command::PerceptionOn => "perception.enable",
        Command::PerceptionOff => "perception.disable",
        Command::MemoryExport { .. } => "memory.export",
        Command::Model { .. } => "model.select",
        Command::Avatar { action } => match action {
            AvatarCommand::Status => "status",
            AvatarCommand::Select { .. } => "avatar.select",
            AvatarCommand::Delete { .. } => "avatar.delete",
            AvatarCommand::Confirm { .. } => "avatar.confirm",
            AvatarCommand::Discard => "avatar.discard",
        },
        Command::Ingress { action } => match action {
            IngressCommand::Connect { .. } => "ingress.connect",
            IngressCommand::Disconnect { .. } => "ingress.disconnect",
            IngressCommand::KeyRemove => "ingress.key.remove",
        },
        Command::Voice { action } => match action {
            VoiceCommand::Preview => "voice.preview",
            VoiceCommand::KeyRemove => "voice.key.remove",
            VoiceCommand::KeyCheck => "voice.key.check",
            _ => "voice.apply",
        },
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(secret) = cli.secret.as_deref() else {
        eprintln!("aivtuberctl: no operator secret. Set AIVTUBER_CONTROL_SECRET or pass --secret.");
        return ExitCode::from(2);
    };
    if secret.len() != 32 {
        eprintln!(
            "aivtuberctl: AIVTUBER_CONTROL_SECRET must contain exactly 32 UTF-8 bytes (got {}).",
            secret.len()
        );
        return ExitCode::from(2);
    }

    let endpoint = cli
        .endpoint
        .as_deref()
        .unwrap_or("aivtuber-operator-control")
        .to_owned();
    let action = action_name(&cli.command).to_owned();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("aivtuberctl: failed to start runtime: {error}");
            return ExitCode::from(1);
        }
    };

    let argument = match &cli.command {
        Command::Model { alias } => Some(alias.clone()),
        Command::Voice { action } => voice_patch(action),
        Command::Ingress { action } => ingress_argument(action),
        Command::Avatar { action } => avatar_argument(action),
        _ => None,
    };
    let request = OperatorRequest {
        action,
        secret: secret.to_owned(),
        request_id: None,
        argument,
    };

    match runtime.block_on(send_request(&endpoint, &request)) {
        Ok(response) => {
            if let Command::MemoryExport { out } = &cli.command
                && response.ok
            {
                return match save_memory_export(&response, out) {
                    Ok(summary) => {
                        println!("{summary}");
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("aivtuberctl: {error}");
                        ExitCode::from(1)
                    }
                };
            }
            if cli.command == Command::Diagnose && response.ok {
                print_diagnose(&response);
            } else {
                print_response(&response);
            }
            if response.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("aivtuberctl: {error}");
            eprintln!(
                "aivtuberctl: is the daemon running with AIVTUBER_CONTROL_ENDPOINT={endpoint}?"
            );
            ExitCode::from(1)
        }
    }
}

/// Validate the daemon's export against the seed contract and write it to a
/// new file. The check repeats the loader's own rules, so a file this writes is
/// one the daemon will load, and a damaged response writes nothing.
fn save_memory_export(response: &OperatorResponse, out: &Path) -> Result<String, String> {
    let export = response
        .memory_export
        .as_ref()
        .ok_or("the daemon sent no memory export; is it a newer build?")?;
    let seed = MemorySeed::parse(&export.seed_json)
        .map_err(|error| format!("the exported seed is not valid: {error}"))?;
    if seed.content_hash() != export.content_hash {
        return Err("the exported seed does not match its content hash".to_owned());
    }
    write_new_file(out, export.seed_json.as_bytes())?;
    Ok(format!(
        "wrote {} entries (skipped {}) to {} hash={}; review it, then promote it by hand",
        export.entries,
        export.skipped,
        out.display(),
        export.content_hash
    ))
}

/// Create `path` and write `bytes`; an existing file is an error, never
/// replaced.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "{} already exists; choose a new path (an export never overwrites a file)",
                    path.display()
                )
            } else {
                format!("cannot create {}: {error}", path.display())
            }
        })?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn print_response(response: &OperatorResponse) {
    if let Some(status) = &response.status {
        println!(
            "muted={} audio_error={:?} avatar_error={:?} stream_error={:?} content_queue={} scheduler_items={} generation_active={}",
            status.muted,
            status.audio_error,
            status.avatar_error,
            status.stream_error,
            status.content_queue,
            status.scheduler_items,
            status.generation_active,
        );
        if let Some(perception) = &status.perception {
            println!(
                "perception status={} suspend_reason={:?} availability={} last_probe_ms={:?} captures_ok={} captures_failed={} captures_late={} yielded_thinking={} yielded_speech={} observations_accepted={}",
                perception.status,
                perception.suspend_reason,
                perception.availability,
                perception.last_probe_ms,
                perception.captures_ok,
                perception.captures_failed,
                perception.captures_late,
                perception.yielded_thinking,
                perception.yielded_speech,
                perception.observations_accepted,
            );
        }
        if let Some(voice) = &status.voice {
            println!(
                "voice backend={} model={} speaker={} style={} rate={} pitch={} volume={} emotion={} key={}{} check={} last_error={:?}",
                voice.backend,
                voice.model_uuid.as_deref().unwrap_or("-"),
                voice.speaker_uuid.as_deref().unwrap_or("-"),
                voice
                    .style_id
                    .map_or_else(|| "-".to_owned(), |s| s.to_string()),
                voice.tuning.speaking_rate,
                voice.tuning.pitch,
                voice.tuning.volume,
                voice.tuning.emotional_intensity,
                if voice.key.set { "set" } else { "not-set" },
                voice
                    .key
                    .source
                    .as_deref()
                    .map_or_else(String::new, |s| format!("({s})")),
                voice.key.check,
                voice.last_error,
            );
        }
        if let Some(avatar) = &status.avatar_model {
            print_avatar_status(avatar);
        }
        if let Some(chat) = &status.chat_ingress {
            for (name, platform) in [("twitch", &chat.twitch), ("youtube", &chat.youtube)] {
                println!(
                    "chat_ingress {name} state={:?} target={} accepted={} dropped={} (queue_full={} refused={}) per_minute={} restarts={} sidecar={} last_error={:?}",
                    platform.state,
                    platform.target.as_deref().unwrap_or("-"),
                    platform.messages_accepted,
                    platform.messages_dropped,
                    platform.dropped_queue_full,
                    platform.dropped_refused,
                    platform.messages_per_minute,
                    platform.restarts,
                    if platform.sidecar_found {
                        "found"
                    } else {
                        "missing"
                    },
                    platform.last_error,
                );
            }
            println!(
                "chat_ingress youtube_key={}{}",
                if chat.youtube_key.set {
                    "set"
                } else {
                    "not-set"
                },
                chat.youtube_key
                    .source
                    .as_deref()
                    .map_or_else(String::new, |s| format!("({s})")),
            );
        }
        if let Some(model) = &status.thinking_model {
            println!(
                "thinking_model active={} version={} state={} target={:?} available={}",
                model.active,
                model.active_version,
                model.state,
                model.target,
                model
                    .available
                    .iter()
                    .map(|m| m.alias.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    } else {
        println!("{}", response.detail);
    }
}

/// Render the #68 support bundle from the daemon's diagnose response. The
/// bundle sections ride the response's detail JSON when the daemon supports
/// #68; the client prints it verbatim as the shareable artifact.
fn print_diagnose(response: &OperatorResponse) {
    println!("{}", response.detail);
}

async fn send_request(
    endpoint: &str,
    request: &OperatorRequest,
) -> Result<OperatorResponse, String> {
    let stream = timeout(Duration::from_secs(5), connect(endpoint))
        .await
        .map_err(|_| "timed out connecting to the operator control endpoint".to_owned())?
        .map_err(|error| format!("connect failed: {error}"))?;

    let mut stream = stream;
    let mut line = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .await
        .map_err(|error| format!("send failed: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flush failed: {error}"))?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    timeout(
        Duration::from_secs(10),
        reader.read_line(&mut response_line),
    )
    .await
    .map_err(|_| "timed out waiting for the daemon response".to_owned())?
    .map_err(|error| format!("read failed: {error}"))?;

    serde_json::from_str(response_line.trim()).map_err(|error| format!("invalid response: {error}"))
}

async fn connect(endpoint: &str) -> std::io::Result<interprocess::local_socket::tokio::Stream> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName, traits::tokio::Stream as _};
        let name = endpoint.to_ns_name::<GenericNamespaced>()?;
        interprocess::local_socket::tokio::Stream::connect(name).await
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName, traits::tokio::Stream as _};
        let name = std::path::PathBuf::from(endpoint).to_fs_name::<GenericFilePath>()?;
        interprocess::local_socket::tokio::Stream::connect(name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_app::OperatorMemoryExport;

    fn response(seed_json: &str, hash: &str) -> OperatorResponse {
        OperatorResponse {
            request_id: "r".to_owned(),
            ok: true,
            detail: String::new(),
            status: None,
            memory_export: Some(OperatorMemoryExport {
                seed_json: seed_json.to_owned(),
                entries: 0,
                skipped: 0,
                content_hash: hash.to_owned(),
            }),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aivtuberctl-{}-{name}", std::process::id()))
    }

    #[test]
    fn export_is_written_to_a_new_file_and_never_overwrites() {
        let seed = MemorySeed::from_entries("memory-export", "export-1", Vec::new()).unwrap();
        let json = seed.to_json();
        let path = temp_path("export.json");
        let _ = std::fs::remove_file(&path);

        save_memory_export(&response(&json, &seed.content_hash()), &path).expect("first write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), json);

        let error = save_memory_export(&response(&json, &seed.content_hash()), &path)
            .expect_err("second write must refuse");
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), json);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_or_mismatched_export_writes_nothing() {
        let seed = MemorySeed::from_entries("memory-export", "export-1", Vec::new()).unwrap();
        let path = temp_path("bad-export.json");
        let _ = std::fs::remove_file(&path);
        assert!(save_memory_export(&response("{}", "x"), &path).is_err());
        assert!(save_memory_export(&response(&seed.to_json(), "wrong"), &path).is_err());
        assert!(!path.exists());
    }

    fn parse(args: &[&str]) -> Command {
        let mut full = vec![
            "aivtuberctl",
            "--secret",
            "01234567890123456789012345678901",
        ];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).expect("arguments").command
    }

    #[test]
    fn voice_subcommands_build_patches_the_daemon_accepts() {
        let patch = |args: &[&str]| {
            let command = parse(args);
            let Command::Voice { action } = &command else {
                panic!("not a voice command");
            };
            (action_name(&command), voice_patch(action))
        };
        let (action, text) = patch(&["voice", "local"]);
        assert_eq!(action, "voice.apply");
        assert_eq!(
            aivtuber_domain::VoicePatch::parse(&text.unwrap())
                .unwrap()
                .backend,
            Some(aivtuber_domain::VoiceBackend::Local)
        );
        let (_, text) = patch(&["voice", "aivis", "--confirm-egress"]);
        let parsed = aivtuber_domain::VoicePatch::parse(&text.unwrap()).unwrap();
        assert!(parsed.confirm_egress);
        let (_, text) = patch(&["voice", "aivis"]);
        assert!(
            !aivtuber_domain::VoicePatch::parse(&text.unwrap())
                .unwrap()
                .confirm_egress
        );
        let (_, text) = patch(&[
            "voice",
            "set",
            "--style",
            "3",
            "--speaking-rate",
            "1.25",
            "--pitch",
            "-0.5",
            "--volume",
            "0.8",
            "--emotional-intensity",
            "1.5",
            "--credit",
            "AivisSpeech: x",
        ]);
        let parsed = aivtuber_domain::VoicePatch::parse(&text.unwrap()).unwrap();
        assert_eq!(parsed.style_id, Some(Some(3)));
        assert_eq!(parsed.pitch, Some(-0.5));
        assert_eq!(parsed.speaking_rate, Some(1.25));
        assert_eq!(parsed.credit, Some(Some("AivisSpeech: x".to_owned())));
        // Only what was given is sent.
        let (_, text) = patch(&["voice", "set", "--volume", "1.1"]);
        let parsed = aivtuber_domain::VoicePatch::parse(&text.unwrap()).unwrap();
        assert_eq!(
            (parsed.volume, parsed.style_id, parsed.pitch),
            (Some(1.1), None, None)
        );
        for (args, action) in [
            (&["voice", "preview"][..], "voice.preview"),
            (&["voice", "key-remove"][..], "voice.key.remove"),
            (&["voice", "key-check"][..], "voice.key.check"),
        ] {
            let (name, text) = patch(args);
            assert_eq!((name, text), (action, None));
        }
    }

    #[test]
    fn ingress_subcommands_build_the_arguments_the_daemon_parses() {
        let (name, argument) = {
            let command = parse(&["ingress", "connect", "twitch", "Some_Channel"]);
            let Command::Ingress { action } = &command else {
                panic!("not an ingress command");
            };
            (action_name(&command), ingress_argument(action))
        };
        assert_eq!(name, "ingress.connect");
        assert_eq!(
            aivtuber_app::parse_connect_argument(argument.as_deref()),
            Ok((
                aivtuber_app::ChatPlatform::Twitch,
                "Some_Channel".to_owned()
            ))
        );
        let command = parse(&["ingress", "disconnect", "youtube"]);
        let Command::Ingress { action } = &command else {
            panic!("not an ingress command");
        };
        assert_eq!(action_name(&command), "ingress.disconnect");
        assert_eq!(
            aivtuber_app::parse_disconnect_argument(ingress_argument(action).as_deref()),
            Ok(aivtuber_app::ChatPlatform::Youtube)
        );
        let command = parse(&["ingress", "key-remove"]);
        let Command::Ingress { action } = &command else {
            panic!("not an ingress command");
        };
        assert_eq!(action_name(&command), "ingress.key.remove");
        assert_eq!(ingress_argument(action), None);
        // Another platform, a missing target, or a key on the command line.
        for args in [
            &["ingress", "connect", "kick", "abc"][..],
            &["ingress", "connect", "twitch"][..],
            &["ingress", "key", "AIza-x"][..],
            &["ingress", "key-set", "AIza-x"][..],
        ] {
            let mut full = vec![
                "aivtuberctl",
                "--secret",
                "01234567890123456789012345678901",
            ];
            full.extend_from_slice(args);
            assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
        }
    }

    #[test]
    fn avatar_subcommands_send_the_id_as_typed_and_have_no_upload() {
        let hash = "ab".repeat(32);
        for (args, name, argument) in [
            (
                vec!["avatar", "select", "sample"],
                "avatar.select",
                Some("sample"),
            ),
            (
                vec!["avatar", "select", &hash[..12]],
                "avatar.select",
                Some(&hash[..12]),
            ),
            (
                vec!["avatar", "delete", &hash],
                "avatar.delete",
                Some(hash.as_str()),
            ),
            (
                vec!["avatar", "confirm", &hash],
                "avatar.confirm",
                Some(hash.as_str()),
            ),
            (vec!["avatar", "discard"], "avatar.discard", None),
            (vec!["avatar", "status"], "status", None),
        ] {
            let command = parse(&args);
            let Command::Avatar { action } = &command else {
                panic!("not an avatar command");
            };
            assert_eq!(action_name(&command), name);
            assert_eq!(avatar_argument(action).as_deref(), argument);
        }
        // The upload is panel-only: no subcommand takes a file or a path.
        for args in [
            &["avatar", "upload", "model.vrm"][..],
            &["avatar", "select"][..],
            &["avatar", "add", "model.vrm"][..],
        ] {
            let mut full = vec![
                "aivtuberctl",
                "--secret",
                "01234567890123456789012345678901",
            ];
            full.extend_from_slice(args);
            assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
        }
    }

    #[test]
    fn there_is_no_subcommand_that_takes_the_key() {
        // The key would travel in a command line and a request log.
        for args in [
            &["voice", "key", "sk-x"][..],
            &["voice", "set", "--api-key", "sk-x"][..],
            &["voice", "key-set", "sk-x"][..],
        ] {
            let mut full = vec![
                "aivtuberctl",
                "--secret",
                "01234567890123456789012345678901",
            ];
            full.extend_from_slice(args);
            assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
        }
    }
}
