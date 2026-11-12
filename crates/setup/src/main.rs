//! `aivtuber-setup`: first-run setup and launcher for the local profile.
//!
//!   aivtuber-setup local [--dry-run] [--offline] [--force] [--llama-flavor vulkan|cpu]
//!                        [--skip llama,llm,vad,stt,tts,avatar,audio]
//!                        [--root DIR] [--assets DIR] [--env-file FILE] [--llama-port N]
//!                        [--profile default|perception] [--llm-endpoint URL]
//!                        [--from-target CARGO_TARGET_DIR]
//!   aivtuber-setup start [--env-file FILE] [--dry-run]
//!
//! Exit codes: 0 success, 1 a step failed (the message says which and how to
//! fix it), 2 bad arguments.

use aivtuber_setup::download::DownloadConfig;
use aivtuber_setup::local::{self, LocalOptions};
use aivtuber_setup::manifest::Manifest;
use aivtuber_setup::prereq;
use aivtuber_setup::start::{self, StartOptions};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "aivtuber-setup",
    version,
    about = "First-run setup for the local profile"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch and verify the assets, synthesize the starter audio, write the env file.
    Local(LocalArgs),
    /// Start llama-server and the app from the env file `local` wrote.
    Start(StartArgs),
}

#[derive(clap::Args)]
struct LocalArgs {
    /// The extracted release directory (default: the directory of this program).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Where models and llama.cpp are stored (default: <root>/local-assets; no spaces in the path).
    #[arg(long)]
    assets: Option<PathBuf>,
    /// Env file to write (default: <root>/local-profile.env). Never overwritten without --force.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// llama.cpp build: vulkan (GPU) or cpu. Default: vulkan when a Vulkan loader is found.
    #[arg(long, value_parser = ["vulkan", "cpu"])]
    llama_flavor: Option<String>,
    /// Steps to leave out, comma separated: llama, llm, vad, stt, tts, avatar, audio.
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
    /// Print the plan only: no network, nothing written.
    #[arg(long)]
    dry_run: bool,
    /// Use no network: verify what is on disk and finish the local steps.
    #[arg(long)]
    offline: bool,
    /// Replace an existing env file (it gets new generated secrets).
    #[arg(long)]
    force: bool,
    /// Model profile: default (Gemma 4 E4B, text) or perception (Qwen3-VL-4B with its vision
    /// projector, two slots; needs at least about 12 GB of VRAM).
    #[arg(long, default_value = aivtuber_setup::manifest::DEFAULT_PROFILE)]
    profile: String,
    /// Use a model server you run yourself (an http loopback URL) instead of the pinned one;
    /// nothing is fetched for llama.cpp or the model, and `start` runs only the app.
    #[arg(long)]
    llm_endpoint: Option<String>,
    /// Developers: run from a source checkout. The executables come from this cargo target
    /// directory (for example target/release); --root is the checkout (default: the current
    /// directory) and must hold the built avatar page and the starter pack.
    #[arg(long)]
    from_target: Option<PathBuf>,
    /// Loopback port for llama-server.
    #[arg(long, default_value_t = aivtuber_setup::config::DEFAULT_LLAMA_PORT)]
    llama_port: u16,
}

#[derive(clap::Args)]
struct StartArgs {
    /// Env file written by `local` (default: local-profile.env beside this program).
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Print what would run and stop.
    #[arg(long)]
    dry_run: bool,
}

fn program_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .ok_or_else(|| "cannot find the directory of this program; pass --root".to_owned())
}

fn run_local(args: LocalArgs) -> Result<(), String> {
    let root = match (args.root, &args.from_target) {
        (Some(r), _) => r,
        (None, Some(_)) => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}; pass --root"))?,
        (None, None) => program_dir()?,
    };
    let opts = LocalOptions {
        assets: args.assets.unwrap_or_else(|| root.join("local-assets")),
        env_file: args
            .env_file
            .unwrap_or_else(|| root.join("local-profile.env")),
        root,
        flavor: args.llama_flavor,
        skip: args.skip.into_iter().collect(),
        dry_run: args.dry_run,
        offline: args.offline,
        force: args.force,
        llama_port: args.llama_port,
        profile: args.profile,
        llm_endpoint: args.llm_endpoint,
        bin_dir: args.from_target,
        platform: local::host_platform()?,
        download: DownloadConfig::default(),
    };
    let probe = prereq::check();
    local::run(&opts, &Manifest::embedded(), &probe, &mut std::io::stdout()).map(|_| ())
}

fn run_start(args: StartArgs) -> Result<i32, String> {
    let env_file = match args.env_file {
        Some(p) => p,
        None => program_dir()?.join("local-profile.env"),
    };
    start::start(&StartOptions {
        env_file,
        dry_run: args.dry_run,
        health_timeout: Duration::from_secs(180),
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Local(args) => run_local(args).map(|()| 0),
        Command::Start(args) => run_start(args),
    };
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(message) => {
            eprintln!("aivtuber-setup: {message}");
            ExitCode::from(1)
        }
    }
}
