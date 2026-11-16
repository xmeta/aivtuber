//! `aivtuber-setup`: first-run setup and launcher for the local profile.
//!
//!   aivtuber-setup local [--dry-run] [--offline] [--force] [--llama-flavor vulkan|cpu]
//!                        [--skip llama,llm,vad,stt,tts,avatar,audio]
//!                        [--root DIR] [--assets DIR] [--env-file FILE] [--llama-port N]
//!                        [--profile default|perception] [--llm-endpoint URL]
//!                        [--with-model ID]...
//!                        [--from-target CARGO_TARGET_DIR]
//!   aivtuber-setup hybrid [--model ID]... [--check-key] [--dry-run] [--offline] [--force]
//!                         [--skip steps] [--root DIR] [--assets DIR] [--env-file FILE]
//!                         [--from-target CARGO_TARGET_DIR]
//!                         (the OpenRouter key is read from OPENROUTER_API_KEY or a hidden prompt)
//!   aivtuber-setup start [--env-file FILE] [--dry-run] [--verbose] [--voice-input on|off]
//!
//! Exit codes: 0 success, 1 a step failed (the message says which and how to
//! fix it), 2 bad arguments.

use aivtuber_setup::config::HybridSetup;
use aivtuber_setup::download::DownloadConfig;
use aivtuber_setup::hybrid;
use aivtuber_setup::local::{self, LocalOptions};
use aivtuber_setup::manifest::Manifest;
use aivtuber_setup::prereq;
use aivtuber_setup::start::{self, StartOptions};
use aivtuber_setup::vcruntime;
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
    Local(Box<LocalArgs>),
    /// Like `local`, but Thinking runs on OpenRouter: no llama.cpp, no model download. Writes
    /// hybrid-profile.env with your OpenRouter API key (OPENROUTER_API_KEY or a hidden prompt).
    Hybrid(Box<HybridArgs>),
    /// Start the profile from the env file `local` or `hybrid` wrote (llama-server first for `local`).
    Start(StartArgs),
}

#[derive(clap::Args)]
struct HybridArgs {
    /// OpenRouter model id (vendor/model), repeatable, at most 8; the first is the one the app
    /// starts on. Default: a few inexpensive chat models. The key is never taken from the
    /// command line: set OPENROUTER_API_KEY, or you are asked for it with the input hidden.
    #[arg(long = "model", value_name = "ID", value_delimiter = ',')]
    models: Vec<String>,
    /// Ask OpenRouter whether the key works before anything is written or downloaded.
    #[arg(long)]
    check_key: bool,
    /// The extracted release directory (default: the directory of this program).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Where the voice and speech models are stored (default: <root>/local-assets; no spaces in the path).
    #[arg(long)]
    assets: Option<PathBuf>,
    /// Env file to write (default: <root>/hybrid-profile.env). Never overwritten without --force.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Steps to leave out, comma separated: vad, stt, tts, avatar, audio.
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
    /// Print the plan only: no network, no key asked for, nothing written.
    #[arg(long)]
    dry_run: bool,
    /// Use no network: verify what is on disk and finish the local steps.
    #[arg(long)]
    offline: bool,
    /// Replace an existing env file (it gets new generated secrets and the key you give now).
    #[arg(long)]
    force: bool,
    /// Developers: run from a source checkout; see `local --from-target`.
    #[arg(long)]
    from_target: Option<PathBuf>,
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
    /// Also download an optional Thinking model (repeatable; for example qwen3-4b). `start` then
    /// serves it next to the default one and the operation panel can switch between them.
    /// Nothing optional is downloaded unless named here.
    #[arg(long = "with-model", value_name = "ID", value_delimiter = ',')]
    with_models: Vec<String>,
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
    /// Env file written by `local` or `hybrid` (default: the one beside this program).
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Print what would run and stop.
    #[arg(long)]
    dry_run: bool,
    /// Print every diagnostic line the app writes (by default `start` hides periodic
    /// measurement lines; sets AIVTUBER_LOG=verbose for the app).
    #[arg(long)]
    verbose: bool,
    /// Turn voice input (the microphone, voice-activity detection and speech
    /// recognition) on or off and remember it beside the env file. Without this
    /// flag the remembered choice is used; the default is off. Under the hybrid
    /// profile, turning it on sends transcribed speech to the cloud model. While
    /// the app runs, the operation panel and `aivtuberctl voice-input on|off`
    /// switch it too, with the same remembered choice.
    #[arg(long, value_enum, value_name = "on|off")]
    voice_input: Option<VoiceInput>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum VoiceInput {
    On,
    Off,
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
        with_models: args.with_models,
        llm_endpoint: args.llm_endpoint,
        bin_dir: args.from_target,
        hybrid: None,
        platform: local::host_platform()?,
        download: DownloadConfig::default(),
    };
    // The pinned llama.cpp build imports the Visual C++ runtime, which is not
    // bundled (#349): fail before any download, not after 2 GB.
    if !opts.dry_run && !opts.skip.contains("llama") && opts.llm_endpoint.is_none() {
        let manifest = Manifest::embedded();
        vcruntime::require(
            &opts.platform,
            &manifest.llama.windows_runtime_dlls,
            &vcruntime::search_dirs(None),
            &|p| p.is_file(),
        )?;
    }
    let probe = prereq::check();
    local::run(&opts, &Manifest::embedded(), &probe, &mut std::io::stdout()).map(|_| ())
}

fn run_hybrid(args: HybridArgs) -> Result<(), String> {
    let root = match (args.root, &args.from_target) {
        (Some(r), _) => r,
        (None, Some(_)) => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}; pass --root"))?,
        (None, None) => program_dir()?,
    };
    let env_file = args
        .env_file
        .unwrap_or_else(|| root.join("hybrid-profile.env"));
    let models = hybrid::resolve_models(&args.models)?;
    // Fail before asking for a key that would be thrown away.
    if !args.dry_run && !args.force && env_file.exists() {
        return Err(format!(
            "{} already exists and is never overwritten without --force (--force writes new secrets and the key you give now)",
            env_file.display()
        ));
    }
    let api_key = if args.dry_run {
        None
    } else {
        Some(hybrid::key_from_environment_or_prompt()?)
    };
    let opts = LocalOptions {
        assets: args.assets.unwrap_or_else(|| root.join("local-assets")),
        env_file,
        root,
        flavor: None,
        skip: args.skip.into_iter().collect(),
        dry_run: args.dry_run,
        offline: args.offline,
        force: args.force,
        llama_port: aivtuber_setup::config::DEFAULT_LLAMA_PORT,
        profile: aivtuber_setup::manifest::DEFAULT_PROFILE.to_owned(),
        with_models: Vec::new(),
        llm_endpoint: None,
        bin_dir: args.from_target,
        hybrid: Some(HybridSetup { api_key, models }),
        platform: local::host_platform()?,
        download: DownloadConfig::default(),
    };
    if args.check_key && opts.offline {
        return Err(
            "--check-key needs the network and cannot be combined with --offline".to_owned(),
        );
    }
    let probe = prereq::check();
    hybrid::run(
        &opts,
        args.check_key.then_some(hybrid::KEY_CHECK_URL),
        &Manifest::embedded(),
        &probe,
        &mut std::io::stdout(),
    )
    .map(|_| ())
}

fn run_start(args: StartArgs) -> Result<i32, String> {
    let env_file = match args.env_file {
        Some(p) => p,
        None => start::default_env_file(&program_dir()?, &|p| p.is_file())?,
    };
    start::start(&StartOptions {
        env_file,
        dry_run: args.dry_run,
        verbose: args.verbose,
        voice_input: args.voice_input.map(|v| matches!(v, VoiceInput::On)),
        health_timeout: Duration::from_secs(180),
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Local(args) => run_local(*args).map(|()| 0),
        Command::Hybrid(args) => run_hybrid(*args).map(|()| 0),
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
