//! `aivtuberctl` — local operator control client (issue #56).
//!
//! Sends one line-delimited JSON request over the daemon's local operator
//! control endpoint (Windows named pipe / Unix domain socket) and prints the
//! response. The secret is supplied via `AIVTUBER_CONTROL_SECRET` (the same
//! value the daemon uses) or `--secret` on the command line.
//!
//! Unmute is intentionally not implemented: mute is a latching emergency
//! state and resuming output requires restarting the daemon. See
//! `docs/production-runtime.adoc` for the security model.

use aivtuber_app::{OperatorRequest, OperatorResponse};
use clap::{Parser, Subcommand};
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

#[derive(Subcommand)]
enum Command {
    /// Adapter/runtime health snapshot (no secret material).
    Status,
    /// Stop and cancel current plus queued performer work.
    Stop,
    /// Latch the operator mute state (unmute is restart-only).
    Mute,
}

fn action_name(command: &Command) -> &'static str {
    match command {
        Command::Status => "status",
        Command::Stop => "stop",
        Command::Mute => "mute",
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
            print_response(&response);
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
    } else {
        println!("{}", response.detail);
    }
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
        use interprocess::local_socket::{GenericFilePath, ToFsPath, traits::tokio::Stream as _};
        let name = std::path::PathBuf::from(endpoint).to_fs_name::<GenericFilePath>()?;
        interprocess::local_socket::tokio::Stream::connect(name).await
    }
}
