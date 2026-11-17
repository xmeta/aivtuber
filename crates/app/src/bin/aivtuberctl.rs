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
//! `memory-seed` (issue #402) shows or replaces the operator memory seed while
//! the daemon runs, like the operation panel: `memory-seed show` prints the seed
//! document, `memory-seed set --file <path>` validates a seed file here against
//! the seed contract, then sends it compact. The control socket carries at most
//! 8 KiB per record, so a larger seed is refused here with a pointer to the
//! panel (which has no such limit). The daemon validates again and writes
//! `AIVTUBER_MEMORY_SEED` atomically when one is configured; no claim is ever
//! logged or audited.
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
//! `voice-input` (issue #400) turns the microphone (voice input) on or off
//! while the daemon runs, like the operation panel; needs the daemon to run
//! with voice input composed (`aivtuber-setup start`). Transcribed speech is
//! admitted as untrusted speech either way.
//!
//! `persona` (issue #401) shows, replaces or clears the operator's character
//! sheet while the daemon runs, like the operation panel: `persona show` prints
//! the text, `persona set --file <path>` sends it (the whole text, checked as
//! UTF-8 and bounded here before anything is sent), `persona clear` removes it.
//! The daemon validates it again and writes `AIVTUBER_PERSONA_FILE` atomically
//! when one is configured; the text is never logged or audited.
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
    /// Show or replace the operator memory seed (issue #402); needs the daemon
    /// to run with live seed editing (the local and hybrid profiles started by
    /// `aivtuber-setup start`). The entries relevant to a reply go to the model
    /// with it; with a cloud model they leave this machine.
    MemorySeed {
        #[command(subcommand)]
        action: MemorySeedCommand,
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
    /// Voice input (the microphone), issue #400 and #396; needs the daemon to
    /// run with voice input composed (the local profile started by
    /// `aivtuber-setup start`). `on` / `off` start or stop the microphone and
    /// the speech recognizer at once; `devices` lists the capture devices the
    /// microphone reported and the one it opened; `device NAME` (or
    /// `device --default`) chooses one and restarts the microphone on it;
    /// `duplex half|full` chooses whether the performer listens while it
    /// speaks (`half`, the default, ignores speech that starts during its
    /// playback and turns voice barge-in off; `full` is for a headset). All
    /// choices are remembered for the next start.
    VoiceInput {
        #[command(subcommand)]
        action: VoiceInputCommand,
    },
    /// Show, replace or clear the character sheet (issue #401); needs the
    /// daemon to run with the generative path (the local and hybrid profiles
    /// started by `aivtuber-setup start`). The text goes to the model with
    /// every reply; with a cloud model it leaves this machine.
    Persona {
        #[command(subcommand)]
        action: PersonaCommand,
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
enum MemorySeedCommand {
    /// Print the seed document the daemon holds (seed-file format).
    Show,
    /// Replace the seed with a seed file (`schemas/memory-seed.schema.json`;
    /// at most 128 entries of 240 bytes, and small enough for one control
    /// record: use the operation panel for a larger one).
    Set {
        /// The seed file to read.
        #[arg(long)]
        file: PathBuf,
    },
}

/// The request line the control socket accepts (`MAX_RECORD_BYTES` of the
/// daemon), less room for the secret and the other fields.
const SEED_ARGUMENT_BUDGET: usize = 7_000;

/// The `memory.seed.set` argument for a memory-seed subcommand: the validated
/// seed as compact JSON. The file is read bounded and parsed with the daemon's
/// own parser before anything is sent; `Show` sends no argument.
fn memory_seed_argument(action: &MemorySeedCommand) -> Result<Option<String>, String> {
    match action {
        MemorySeedCommand::Show => Ok(None),
        MemorySeedCommand::Set { file } => {
            let seed = MemorySeed::load(file)
                .map_err(|error| format!("cannot use {}: {error}", file.display()))?;
            let compact = seed.to_compact_json();
            // JSON inside a JSON string: quotes and backslashes are escaped
            // again, so measure the escaped form.
            let escaped = serde_json::to_string(&compact).map_or(usize::MAX, |s| s.len());
            if escaped > SEED_ARGUMENT_BUDGET {
                return Err(format!(
                    "{} is too large for the control socket ({escaped} of {SEED_ARGUMENT_BUDGET} bytes after encoding); edit it in the operation panel instead",
                    file.display()
                ));
            }
            Ok(Some(compact))
        }
    }
}

#[derive(Subcommand, Clone, PartialEq, Eq)]
enum VoiceInputCommand {
    /// Turn the microphone on.
    On,
    /// Turn the microphone off.
    Off,
    /// List the capture devices and the opened one (the `voice_input` lines of
    /// `status`). The list exists once the microphone has been on.
    Devices,
    /// Choose the capture device by its name as listed by `devices`.
    Device {
        /// The device name, exactly as listed.
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        name: Option<String>,
        /// Use the system default capture device.
        #[arg(long)]
        default: bool,
    },
    /// Half (default) or full duplex.
    Duplex {
        /// `half` or `full`.
        #[arg(value_parser = ["half", "full"])]
        mode: String,
    },
}

#[derive(Subcommand, Clone, PartialEq, Eq)]
enum PersonaCommand {
    /// Print the character sheet the daemon holds (nothing if there is none).
    Show,
    /// Replace the character sheet with the text of a UTF-8 file (at most
    /// 3000 bytes after line endings are normalised).
    Set {
        /// The file to read, for example local-persona.md.
        #[arg(long)]
        file: PathBuf,
    },
    /// Remove the character sheet; replies use the default voice.
    Clear,
}

/// The `persona.set` argument for a persona subcommand: the whole text. The
/// file is read bounded and must be UTF-8 before anything is sent; the daemon
/// validates it again. `Clear` sends the empty text, `Show` no argument.
fn persona_argument(action: &PersonaCommand) -> Result<Option<String>, String> {
    use std::io::Read as _;
    match action {
        PersonaCommand::Show => Ok(None),
        PersonaCommand::Clear => Ok(Some(String::new())),
        PersonaCommand::Set { file } => {
            let limit = aivtuber_domain::MAX_PERSONA_FILE_BYTES;
            let mut bytes = Vec::new();
            std::fs::File::open(file)
                .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
            if bytes.len() as u64 > limit {
                return Err(format!(
                    "{} is larger than {limit} bytes; the character sheet is at most 3000 bytes",
                    file.display()
                ));
            }
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| format!("{} is not valid UTF-8", file.display()))
        }
    }
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

/// The argument of a voice-input subcommand: the device name (empty is the
/// system default) or `half` / `full`.
fn voice_input_argument(action: &VoiceInputCommand) -> Option<String> {
    match action {
        VoiceInputCommand::Device { name, .. } => Some(name.clone().unwrap_or_default()),
        VoiceInputCommand::Duplex { mode } => Some(mode.clone()),
        VoiceInputCommand::On | VoiceInputCommand::Off | VoiceInputCommand::Devices => None,
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
        Command::MemorySeed { action } => match action {
            MemorySeedCommand::Show => "memory.seed.get",
            MemorySeedCommand::Set { .. } => "memory.seed.set",
        },
        Command::Persona { action } => match action {
            PersonaCommand::Show => "persona.get",
            PersonaCommand::Set { .. } | PersonaCommand::Clear => "persona.set",
        },
        Command::VoiceInput { action } => match action {
            VoiceInputCommand::On => "speech_input.enable",
            VoiceInputCommand::Off => "speech_input.disable",
            VoiceInputCommand::Devices => "status",
            VoiceInputCommand::Device { .. } => "speech_input.device",
            VoiceInputCommand::Duplex { .. } => "speech_input.duplex",
        },
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
        Command::MemorySeed { action } => match memory_seed_argument(action) {
            Ok(argument) => argument,
            Err(error) => {
                eprintln!("aivtuberctl: {error}");
                return ExitCode::from(2);
            }
        },
        Command::Persona { action } => match persona_argument(action) {
            Ok(argument) => argument,
            Err(error) => {
                eprintln!("aivtuberctl: {error}");
                return ExitCode::from(2);
            }
        },
        Command::Model { alias } => Some(alias.clone()),
        Command::VoiceInput { action } => voice_input_argument(action),
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
            if let Command::MemorySeed {
                action: MemorySeedCommand::Show,
            } = &cli.command
                && response.ok
            {
                // The identity goes to stderr so stdout is exactly the seed.
                eprintln!("{}", response.detail);
                print!(
                    "{}",
                    response
                        .memory_export
                        .as_ref()
                        .map_or("", |export| export.seed_json.as_str())
                );
                return ExitCode::SUCCESS;
            }
            if let Command::Persona {
                action: PersonaCommand::Show,
            } = &cli.command
                && response.ok
            {
                // The identity goes to stderr so stdout is exactly the sheet.
                eprintln!("{}", response.detail);
                print!("{}", response.persona.as_deref().unwrap_or(""));
                return ExitCode::SUCCESS;
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
        if let Some(voice_input) = &status.voice_input {
            println!(
                "voice_input composed={} state={:?} thinking_remote={} reason={:?} duplex={:?} dropped_during_playback={} device={:?} selected_device={:?}",
                voice_input.composed,
                voice_input.state,
                voice_input.thinking_remote,
                voice_input.reason,
                voice_input.duplex,
                voice_input.dropped_during_playback,
                voice_input.device,
                voice_input.selected_device,
            );
            for name in &voice_input.devices {
                println!("voice_input_device {name:?}");
            }
        }
        if let Some(persona) = &status.persona {
            // Identity only (#404): never the character sheet's text.
            println!(
                "persona present={} bytes={} sha256={}",
                persona.present,
                persona.bytes,
                persona.sha256.as_deref().unwrap_or("-"),
            );
        }
        if let Some(editor) = &status.persona_editor {
            println!(
                "persona_editor persisted={} thinking_remote={}",
                editor.persisted, editor.thinking_remote
            );
        }
        if let Some(editor) = &status.memory_seed_editor {
            println!(
                "memory_seed_editor persisted={} thinking_remote={} entries={} content_hash={}",
                editor.persisted, editor.thinking_remote, editor.entries, editor.content_hash
            );
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
            persona: None,
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
    fn memory_seed_subcommands_map_to_the_two_actions_and_validate_the_file_before_sending() {
        let dir = std::env::temp_dir().join(format!("aivtuberctl-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path.to_string_lossy().into_owned()
        };
        let command = parse(&["memory-seed", "show"]);
        assert_eq!(action_name(&command), "memory.seed.get");
        let Command::MemorySeed { action } = &command else {
            panic!("not a memory-seed command");
        };
        assert_eq!(memory_seed_argument(action), Ok(None));

        let seed = MemorySeed::from_entries(
            "s",
            "1",
            vec![aivtuber_app::SeedEntry {
                kind: aivtuber_app::SeedKind::PersonaFact,
                claim: "ミコトは猫が好き".to_owned(),
            }],
        )
        .unwrap();
        let good = write("good.json", &seed.to_json());
        let command = parse(&["memory-seed", "set", "--file", &good]);
        assert_eq!(action_name(&command), "memory.seed.set");
        let Command::MemorySeed { action } = &command else {
            panic!("not a memory-seed command");
        };
        let argument = memory_seed_argument(action).unwrap().unwrap();
        assert_eq!(MemorySeed::parse(&argument).unwrap(), seed);
        assert!(!argument.contains('\n'), "compact: one line");

        // Invalid, unknown-field, missing and over-the-record files send nothing.
        let invalid = write("invalid.json", "{\"schema_version\":\"0.1.0\"}");
        let missing = dir.join("missing.json").to_string_lossy().into_owned();
        let many = MemorySeed::from_entries(
            "s",
            "1",
            (0..40)
                .map(|i| aivtuber_app::SeedEntry {
                    kind: aivtuber_app::SeedKind::RunningJoke,
                    claim: format!("{i:03} {}", "x".repeat(200)),
                })
                .collect(),
        )
        .unwrap();
        let big = write("big.json", &many.to_json());
        for file in [&invalid, &missing, &big] {
            let command = parse(&["memory-seed", "set", "--file", file]);
            let Command::MemorySeed { action } = &command else {
                panic!("not a memory-seed command");
            };
            assert!(memory_seed_argument(action).is_err(), "{file}");
        }
        for args in [
            &["memory-seed"][..],
            &["memory-seed", "set"][..],
            &["memory-seed", "set", "--text", "x"][..],
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
    fn persona_subcommands_map_to_the_two_actions_and_bound_the_file_before_sending() {
        let dir = std::env::temp_dir().join(format!("aivtuberctl-persona-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_string_lossy().into_owned()
        };
        let command = parse(&["persona", "show"]);
        assert_eq!(action_name(&command), "persona.get");
        let Command::Persona { action } = &command else {
            panic!("not a persona command");
        };
        assert_eq!(persona_argument(action), Ok(None));

        let command = parse(&["persona", "clear"]);
        assert_eq!(action_name(&command), "persona.set");
        let Command::Persona { action } = &command else {
            panic!("not a persona command");
        };
        assert_eq!(persona_argument(action), Ok(Some(String::new())));

        let good = write("good.md", "名前: ミコト\n".as_bytes());
        let command = parse(&["persona", "set", "--file", &good]);
        assert_eq!(action_name(&command), "persona.set");
        let Command::Persona { action } = &command else {
            panic!("not a persona command");
        };
        assert_eq!(
            persona_argument(action),
            Ok(Some("名前: ミコト\n".to_owned()))
        );

        let bad = write("bad.md", b"\xFF\xFE");
        let big = write("big.md", &vec![b'a'; 100_000]);
        let missing = dir.join("missing.md").to_string_lossy().into_owned();
        for file in [&bad, &big, &missing] {
            let command = parse(&["persona", "set", "--file", file]);
            let Command::Persona { action } = &command else {
                panic!("not a persona command");
            };
            assert!(persona_argument(action).is_err(), "{file}");
        }
        for args in [
            &["persona"][..],
            &["persona", "set"][..],
            &["persona", "set", "--text", "x"][..],
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
    fn voice_input_subcommand_maps_to_the_actions_and_their_arguments() {
        for (state, action) in [
            ("on", "speech_input.enable"),
            ("off", "speech_input.disable"),
            ("devices", "status"),
        ] {
            let command = parse(&["voice-input", state]);
            assert_eq!(action_name(&command), action);
            let Command::VoiceInput { action } = &command else {
                panic!("not voice-input")
            };
            assert_eq!(voice_input_argument(action), None, "{state}");
        }
        let command = parse(&["voice-input", "device", "Line (YAMAHA AG06/AG03)"]);
        assert_eq!(action_name(&command), "speech_input.device");
        let Command::VoiceInput { action } = &command else {
            panic!("not voice-input")
        };
        assert_eq!(
            voice_input_argument(action).as_deref(),
            Some("Line (YAMAHA AG06/AG03)")
        );
        let command = parse(&["voice-input", "device", "--default"]);
        let Command::VoiceInput { action } = &command else {
            panic!("not voice-input")
        };
        assert_eq!(voice_input_argument(action).as_deref(), Some(""));
        for mode in ["half", "full"] {
            let command = parse(&["voice-input", "duplex", mode]);
            assert_eq!(action_name(&command), "speech_input.duplex");
            let Command::VoiceInput { action } = &command else {
                panic!("not voice-input")
            };
            assert_eq!(voice_input_argument(action).as_deref(), Some(mode));
        }
        for args in [
            &["voice-input"][..],
            &["voice-input", "maybe"][..],
            &["voice-input", "on", "off"][..],
            &["voice-input", "device"][..],
            &["voice-input", "device", "--default", "Mic"][..],
            &["voice-input", "duplex", "both"][..],
            &["voice-input", "duplex"][..],
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
