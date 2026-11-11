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

#[derive(Subcommand, Clone, PartialEq, Eq)]
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

    let request = OperatorRequest {
        action,
        secret: secret.to_owned(),
        request_id: None,
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
}
