//! `aivtuber-setup start`: runs the local profile from the env file written by
//! `aivtuber-setup local` (quickstart steps 3 and 4 in one command).
//!
//! It starts llama-server on loopback, waits for `/health`, then runs
//! `aivtuber-app` with the env file's variables in the foreground (chat is read
//! from this terminal's stdin and forwarded: plain text is wrapped as untrusted
//! public chat, envelope JSON lines pass through), and
//! stops llama-server when the app exits.
//!
//! When optional Thinking models are installed next to the default one
//! (`aivtuber-setup local --with-model`, #357) llama-server runs in router mode
//! from a generated preset file (one section per installed model, one model in
//! VRAM at a time) and the app is told the allowlist and the model to start
//! with. With only the default model nothing about the launch changes.

use crate::config::{LLAMA_ALIAS, LLAMA_DEFAULT_CONTEXT, parse_env};
use crate::manifest::{DEFAULT_PROFILE, Manifest};
use aivtuber_domain::{
    MAX_MODEL_SETTINGS_BYTES, ThinkingModel, ThinkingModelCatalog, parse_model_settings,
};
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct StartOptions {
    pub env_file: PathBuf,
    pub dry_run: bool,
    pub health_timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub struct LlamaCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub port: u16,
}

fn need<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    env.get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{key} is missing from the env file; run `aivtuber-setup local` (add --force to rewrite it)"))
}

pub fn load_env(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "cannot read {}: {e}; run `aivtuber-setup local` first",
            path.display()
        )
    })?;
    Ok(parse_env(&text).into_iter().collect())
}

/// What `aivtuber-setup local` recorded about the language model.
#[derive(Debug, PartialEq, Eq)]
pub enum Llm {
    /// Start llama-server with this command.
    Managed(LlamaCommand),
    /// The operator runs the model server at the endpoint in the env file.
    External,
}

/// Decides how the model is served, or refuses with the way out. An env file
/// from before profiles existed has no mode key and is treated as managed.
pub fn llm(env: &BTreeMap<String, String>) -> Result<Llm, String> {
    match env.get("AIVTUBER_SETUP_LLAMA_MODE").map(String::as_str) {
        None | Some("managed") => llama_command(env).map(Llm::Managed),
        Some("external") => Ok(Llm::External),
        Some("missing") => Err(
            "this env file has no language model: `aivtuber-setup local` was run with --skip llm (or llama), or the files are not on disk. Run it again without --skip (add --force to rewrite the env file), or run it with --llm-endpoint <loopback url> to use a model server you run yourself".to_owned(),
        ),
        Some(other) => Err(format!(
            "AIVTUBER_SETUP_LLAMA_MODE={other:?} is not one of managed, external, missing; run `aivtuber-setup local --force`"
        )),
    }
}

/// The llama-server command the quickstart step 3 documents.
pub fn llama_command(env: &BTreeMap<String, String>) -> Result<LlamaCommand, String> {
    let port: u16 = need(env, "AIVTUBER_SETUP_LLAMA_PORT")?
        .parse()
        .map_err(|_| "AIVTUBER_SETUP_LLAMA_PORT is not a port number".to_owned())?;
    let layers: u32 = need(env, "AIVTUBER_SETUP_LLAMA_GPU_LAYERS")?
        .parse()
        .map_err(|_| "AIVTUBER_SETUP_LLAMA_GPU_LAYERS is not a number".to_owned())?;
    let number = |key: &str| -> Result<Option<u32>, String> {
        env.get(key)
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{key} is not a positive number"))
            })
            .transpose()
    };
    let context = number("AIVTUBER_SETUP_LLAMA_CONTEXT")?.unwrap_or(LLAMA_DEFAULT_CONTEXT);
    let alias = env
        .get("AIVTUBER_SETUP_LLAMA_ALIAS")
        .filter(|v| !v.is_empty())
        .map_or(LLAMA_ALIAS, String::as_str);
    let mut args: Vec<String> = [
        "-m",
        need(env, "AIVTUBER_SETUP_LLAMA_GGUF")?,
        // Loopback only: the app refuses any other endpoint in the local profile.
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--alias",
        alias,
        "-c",
        &context.to_string(),
        // Reasoning models otherwise spend the whole reply budget on hidden
        // thinking and return empty text (#340); harmless for other templates.
        "--reasoning",
        "off",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(mmproj) = env
        .get("AIVTUBER_SETUP_LLAMA_MMPROJ")
        .filter(|v| !v.is_empty())
    {
        args.push("--mmproj".to_owned());
        args.push(mmproj.clone());
    }
    if let Some(parallel) = number("AIVTUBER_SETUP_LLAMA_PARALLEL")? {
        args.push("--parallel".to_owned());
        args.push(parallel.to_string());
    }
    if layers > 0 {
        args.push("-ngl".to_owned());
        args.push(layers.to_string());
    }
    Ok(LlamaCommand {
        program: PathBuf::from(need(env, "AIVTUBER_SETUP_LLAMA_SERVER")?),
        args,
        port,
    })
}

/// File beside the env file that remembers the model chosen in the panel. The
/// app writes it; `start` reads it back (alias only, checked against what is
/// installed).
pub const MODEL_SETTINGS_FILE: &str = "local-model.json";
/// Generated router preset, rewritten at every start.
pub const PRESET_FILE: &str = "llama-models.ini";
/// File beside the env file that remembers the voice chosen in the panel
/// (#360): backend, Aivis model/speaker/style, tuning and the credit line. The
/// app reads and writes it; it never holds the API key.
pub const VOICE_SETTINGS_FILE: &str = "local-voice.json";
/// The Aivis Cloud API key entered in the panel, in its own file so it is never
/// mixed into the settings or the env file. Owner-only where the OS allows. The
/// app owns it; `AIVIS_API` in the environment still wins.
pub const AIVIS_KEY_FILE: &str = "local-aivis-key.txt";

/// One model llama-server can serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedModel {
    pub alias: String,
    pub version: String,
    pub gguf: PathBuf,
    pub context: u32,
}

/// The models `start` serves and the one the app begins with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPlan {
    /// The default model first, then every installed optional model.
    pub models: Vec<ServedModel>,
    pub catalog: ThinkingModelCatalog,
    /// Alias to start on: the saved choice while it is still installed,
    /// otherwise the default.
    pub initial: String,
}

impl ModelPlan {
    /// Router mode is needed only when there is something to switch to.
    pub fn is_router(&self) -> bool {
        self.models.len() > 1
    }
}

/// Decides which models are served. `None` for a profile that has no model
/// switching (perception, or anything with a vision projector). An optional
/// model counts as installed when its file exists with its pinned size
/// (`aivtuber-setup local` verified the digest when it downloaded it).
pub fn plan_models(
    env: &BTreeMap<String, String>,
    manifest: &Manifest,
    settings: &Path,
    size_of: &dyn Fn(&Path) -> Option<u64>,
) -> Result<Option<ModelPlan>, String> {
    let profile = env
        .get("AIVTUBER_SETUP_PROFILE")
        .map_or(DEFAULT_PROFILE, String::as_str);
    if profile != DEFAULT_PROFILE
        || env
            .get("AIVTUBER_SETUP_LLAMA_MMPROJ")
            .is_some_and(|v| !v.is_empty())
    {
        return Ok(None);
    }
    let gguf = PathBuf::from(need(env, "AIVTUBER_SETUP_LLAMA_GGUF")?);
    let default_profile = manifest.profile(DEFAULT_PROFILE)?;
    let alias = env
        .get("AIVTUBER_SETUP_LLAMA_ALIAS")
        .filter(|v| !v.is_empty())
        .map_or(LLAMA_ALIAS, String::as_str);
    let version = env
        .get("AIVTUBER_OPENAI_MODEL_VERSION")
        .filter(|v| !v.is_empty())
        .map_or(default_profile.spec.model_version.as_str(), String::as_str);
    let context = env
        .get("AIVTUBER_SETUP_LLAMA_CONTEXT")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(LLAMA_DEFAULT_CONTEXT);
    let dir = gguf.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut models = vec![ServedModel {
        alias: alias.to_owned(),
        version: version.to_owned(),
        gguf: gguf.clone(),
        context,
    }];
    for (id, optional) in &manifest.optional_models {
        let path = dir.join(&optional.llm.file);
        if optional.alias == alias || size_of(&path) != Some(optional.llm.size) {
            continue;
        }
        if !path
            .to_str()
            .is_some_and(|p| !p.contains(['\n', '\r']) && !p.is_empty())
        {
            return Err(format!("the path of model {id} is not usable"));
        }
        models.push(ServedModel {
            alias: optional.alias.clone(),
            version: optional.model_version.clone(),
            gguf: path,
            context: optional.context,
        });
    }
    let catalog = ThinkingModelCatalog::new(
        models
            .iter()
            .map(|m| {
                ThinkingModel::new(&m.alias, &m.version).map_err(|e| format!("{}: {e}", m.alias))
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(|e| e.to_string())?;
    let initial = read_model_settings(settings, &catalog).unwrap_or_else(|| alias.to_owned());
    Ok(Some(ModelPlan {
        models,
        catalog,
        initial,
    }))
}

/// The remembered choice, if the file exists, is small, well formed and names a
/// model still in `catalog`. Every other case (missing, oversized, garbage,
/// unknown or uninstalled alias) is `None`, so the caller falls back to its
/// default; a bad file never stops a start.
pub fn read_model_settings(path: &Path, catalog: &ThinkingModelCatalog) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_MODEL_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let alias = parse_model_settings(&bytes)?;
    catalog.get(&alias).map(|m| m.alias.clone())
}

/// The router's preset file: one `[alias]` section per model with the same
/// per-model flags the single-model launch passes (context, GPU layers, hidden
/// reasoning off). Keys are the long llama-server flags without the dashes.
/// Paths use forward slashes, which Windows accepts, so no escaping is needed.
pub fn router_preset(models: &[ServedModel], gpu_layers: u32) -> String {
    let mut text = String::from(
        "# Generated by `aivtuber-setup start`; rewritten at every start. Do not edit.\nversion = 1\n",
    );
    for model in models {
        text.push_str(&format!(
            "\n[{}]\nmodel = {}\nctx-size = {}\n",
            model.alias,
            model.gguf.display().to_string().replace('\\', "/"),
            model.context
        ));
        if gpu_layers > 0 {
            text.push_str(&format!("n-gpu-layers = {gpu_layers}\n"));
        }
        // Reasoning models otherwise spend the whole reply budget on hidden
        // thinking and return empty text (#340); harmless for other templates.
        text.push_str("reasoning = off\n");
    }
    text
}

/// The router command: loopback only, one model resident at a time (the
/// previous one is unloaded on a switch, so VRAM holds one).
pub fn router_command(
    env: &BTreeMap<String, String>,
    preset: &Path,
) -> Result<LlamaCommand, String> {
    let port: u16 = need(env, "AIVTUBER_SETUP_LLAMA_PORT")?
        .parse()
        .map_err(|_| "AIVTUBER_SETUP_LLAMA_PORT is not a port number".to_owned())?;
    let args = [
        "--models-preset",
        &preset.display().to_string(),
        "--models-max",
        "1",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    Ok(LlamaCommand {
        program: PathBuf::from(need(env, "AIVTUBER_SETUP_LLAMA_SERVER")?),
        args,
        port,
    })
}

/// What the app needs to offer the switch: the allowlist, the model to start
/// on and where to remember the choice. Layered over the env file's values.
pub fn app_model_env(plan: &ModelPlan, settings: &Path) -> Vec<(String, String)> {
    let initial = plan
        .catalog
        .get(&plan.initial)
        .unwrap_or(&plan.catalog.models()[0]);
    vec![
        ("AIVTUBER_OPENAI_MODEL".to_owned(), initial.alias.clone()),
        (
            "AIVTUBER_OPENAI_MODEL_VERSION".to_owned(),
            initial.version.clone(),
        ),
        (
            "AIVTUBER_OPENAI_MODEL_CHOICES".to_owned(),
            plan.catalog.render_choices(),
        ),
        (
            "AIVTUBER_OPENAI_MODEL_SETTINGS".to_owned(),
            settings.display().to_string(),
        ),
    ]
}

/// Where the app keeps the voice choice and the API key (#360), layered under
/// the env file's own values: a setting spelled out there wins. The files are
/// created by the app on the first change; nothing is written here.
pub fn app_voice_env(env_file: &Path) -> Vec<(String, String)> {
    vec![
        (
            "AIVTUBER_VOICE_SETTINGS".to_owned(),
            env_file
                .with_file_name(VOICE_SETTINGS_FILE)
                .display()
                .to_string(),
        ),
        (
            "AIVTUBER_AIVIS_KEY_FILE".to_owned(),
            env_file
                .with_file_name(AIVIS_KEY_FILE)
                .display()
                .to_string(),
        ),
    ]
}

fn port_in_use(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn healthy(port: u16) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .get(format!("http://127.0.0.1:{port}/health"))
        .call()
        .is_ok_and(|r| r.status().as_u16() == 200)
}

/// `taskkill` by absolute path under `%SystemRoot%`, so a copy in the current
/// directory, next to this program or earlier on PATH is never run instead.
#[cfg(windows)]
fn taskkill_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
    root.join("System32").join("taskkill.exe")
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // The router spawns one llama-server per loaded model. On Windows a
        // killed parent leaves them running, so end the whole tree.
        #[cfg(windows)]
        let _ = Command::new(taskkill_path())
            .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns the app's exit code.
pub fn start(opts: &StartOptions) -> Result<i32, String> {
    let env = load_env(&opts.env_file)?;
    let llm = llm(&env)?;
    let app = PathBuf::from(need(&env, "AIVTUBER_SETUP_APP")?);
    let settings = opts.env_file.with_file_name(MODEL_SETTINGS_FILE);
    let preset = opts.env_file.with_file_name(PRESET_FILE);
    let mut env = env;
    for (name, value) in app_voice_env(&opts.env_file) {
        env.entry(name).or_insert(value);
    }
    let plan = match &llm {
        Llm::Managed(_) => plan_models(&env, &Manifest::embedded(), &settings, &|p| {
            std::fs::metadata(p).ok().map(|m| m.len())
        })?,
        Llm::External => None,
    };
    let mut llm = llm;
    if let (Some(plan), Llm::Managed(llama)) = (&plan, &mut llm) {
        env.extend(app_model_env(plan, &settings));
        if plan.is_router() {
            *llama = router_command(&env, &preset)?;
        }
    }
    match &llm {
        Llm::Managed(llama) => println!(
            "llama-server: {} {}",
            llama.program.display(),
            llama.args.join(" ")
        ),
        Llm::External => println!(
            "language model: your own server at {} (not started here)",
            env.get("AIVTUBER_OPENAI_ENDPOINT")
                .map_or("?", String::as_str)
        ),
    }
    if let Some(plan) = &plan {
        println!(
            "models:       {} (starting on {}){}",
            plan.catalog.render_choices(),
            plan.initial,
            if plan.is_router() {
                format!("; router preset {}", preset.display())
            } else {
                " (only the default is installed; add one with `aivtuber-setup local --with-model qwen3-4b`)".to_owned()
            }
        );
    }
    println!(
        "app:          {} (environment from {})",
        app.display(),
        opts.env_file.display()
    );
    if opts.dry_run {
        println!("dry run: nothing was started");
        return Ok(0);
    }
    let mut needed = vec![("aivtuber-app", &app)];
    if let Llm::Managed(llama) = &llm {
        needed.push(("llama-server", &llama.program));
    }
    for (what, path) in needed {
        if !path.is_file() {
            return Err(format!(
                "{what} not found at {}; run `aivtuber-setup local` again",
                path.display()
            ));
        }
    }
    let Llm::Managed(llama) = llm else {
        println!("starting the app ({CHAT_HINT}; Ctrl+C stops it)");
        return run_app(&app, &env);
    };
    // The pinned llama.cpp build imports the Visual C++ runtime, which is not
    // bundled (#349): say so instead of a bare 0xC0000135 from llama-server.
    crate::vcruntime::require(
        if cfg!(windows) { "win32-x64" } else { "" },
        &crate::manifest::Manifest::embedded()
            .llama
            .windows_runtime_dlls,
        &crate::vcruntime::search_dirs(llama.program.parent()),
        &|p| p.is_file(),
    )?;
    if port_in_use(llama.port) {
        return Err(format!(
            "port {} is already in use (a /health check there could be answered by someone else's server). Stop that program, or change AIVTUBER_SETUP_LLAMA_PORT and the port in AIVTUBER_OPENAI_ENDPOINT in {}",
            llama.port,
            opts.env_file.display()
        ));
    }
    if let Some(plan) = plan.as_ref().filter(|p| p.is_router()) {
        let gpu_layers: u32 = env
            .get("AIVTUBER_SETUP_LLAMA_GPU_LAYERS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        std::fs::write(&preset, router_preset(&plan.models, gpu_layers))
            .map_err(|e| format!("cannot write {}: {e}", preset.display()))?;
    }
    let log_path = opts.env_file.with_file_name("llama-server.log");
    let log = std::fs::File::create(&log_path)
        .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;
    let mut guard = KillOnDrop(
        Command::new(&llama.program)
            .args(&llama.args)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("cannot start llama-server: {e}"))?,
    );
    println!(
        "waiting for llama-server (model load can take a minute; log: {})",
        log_path.display()
    );
    let deadline = Instant::now() + opts.health_timeout;
    loop {
        if let Some(status) = guard.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "llama-server exited ({status}); see {}",
                log_path.display()
            ));
        }
        if healthy(llama.port) {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "llama-server was not healthy within {} s; see {}",
                opts.health_timeout.as_secs(),
                log_path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("llama-server ready; starting the app ({CHAT_HINT}; Ctrl+C stops everything)");
    run_app(&app, &env)
}

const CHAT_HINT: &str = "type chat as plain text, or paste Event Envelope JSON lines";

/// Runs the app in the foreground with its stdin fed from this terminal through
/// the plain-text wrapper (see `chat`). EOF here closes the app's stdin, which
/// is its clean-shutdown signal, exactly as before.
fn run_app(app: &Path, env: &BTreeMap<String, String>) -> Result<i32, String> {
    let mut child = Command::new(app)
        .envs(env.iter())
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", app.display()))?;
    if let Some(mut app_stdin) = child.stdin.take() {
        // Detached on purpose: it blocks on the terminal read and ends with
        // this process once the app has exited.
        std::thread::spawn(move || {
            let mut wrapper = crate::chat::ChatWrapper::new();
            crate::chat::pump(
                std::io::stdin().lock(),
                &mut app_stdin,
                std::io::stderr(),
                &mut wrapper,
            );
        });
    }
    let status = child
        .wait()
        .map_err(|e| format!("waiting for {}: {e}", app.display()))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(layers: &str) -> BTreeMap<String, String> {
        parse_env(&format!(
            "AIVTUBER_SETUP_LLAMA_SERVER=/x/llama-server\nAIVTUBER_SETUP_LLAMA_GGUF=/x/m.gguf\nAIVTUBER_SETUP_LLAMA_PORT=18080\nAIVTUBER_SETUP_LLAMA_GPU_LAYERS={layers}\n"
        ))
        .into_iter()
        .collect()
    }

    #[test]
    fn the_llama_command_is_loopback_with_the_documented_alias() {
        let c = llama_command(&env("99")).unwrap();
        assert_eq!(c.port, 18080);
        let joined = c.args.join(" ");
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains(&format!("--alias {LLAMA_ALIAS}")));
        assert!(joined.contains("--reasoning off"), "{joined}");
        assert!(joined.ends_with("-ngl 99"));
        assert!(
            !llama_command(&env("0"))
                .unwrap()
                .args
                .contains(&"-ngl".to_owned())
        );
    }

    #[test]
    fn a_vision_profile_adds_the_projector_slots_context_and_alias() {
        let mut e = env("99");
        e.insert(
            "AIVTUBER_SETUP_LLAMA_MMPROJ".into(),
            "/x/mmproj.gguf".into(),
        );
        e.insert("AIVTUBER_SETUP_LLAMA_CONTEXT".into(), "8192".into());
        e.insert("AIVTUBER_SETUP_LLAMA_PARALLEL".into(), "2".into());
        e.insert(
            "AIVTUBER_SETUP_LLAMA_ALIAS".into(),
            "qwen3-vl-4b-instruct".into(),
        );
        let joined = llama_command(&e).unwrap().args.join(" ");
        assert!(joined.contains("--mmproj /x/mmproj.gguf"), "{joined}");
        assert!(joined.contains("-c 8192"), "{joined}");
        assert!(joined.contains("--parallel 2"), "{joined}");
        assert!(joined.contains("--alias qwen3-vl-4b-instruct"), "{joined}");
        assert!(joined.contains("--host 127.0.0.1"));
        // The default profile passes neither flag.
        let plain = llama_command(&env("99")).unwrap().args.join(" ");
        assert!(!plain.contains("--mmproj") && !plain.contains("--parallel"));
        assert!(plain.contains("-c 4096"));
        e.insert("AIVTUBER_SETUP_LLAMA_PARALLEL".into(), "0".into());
        assert!(llama_command(&e).is_err());
    }

    #[test]
    fn a_missing_model_is_refused_clearly_and_an_external_one_starts_no_server() {
        let mut e = env("0");
        e.insert("AIVTUBER_SETUP_LLAMA_MODE".into(), "missing".into());
        let err = llm(&e).unwrap_err();
        assert!(
            err.contains("--skip") && err.contains("--llm-endpoint"),
            "{err}"
        );
        e.insert("AIVTUBER_SETUP_LLAMA_MODE".into(), "external".into());
        assert_eq!(llm(&e).unwrap(), Llm::External);
        e.insert("AIVTUBER_SETUP_LLAMA_MODE".into(), "bogus".into());
        assert!(llm(&e).is_err());
        // Env files written before profiles existed have no mode key.
        assert!(matches!(llm(&env("0")).unwrap(), Llm::Managed(_)));
    }

    const QWEN_FILE: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";

    fn qwen_size() -> u64 {
        Manifest::embedded().optional_models["qwen3-4b"].llm.size
    }

    fn plan(
        e: &BTreeMap<String, String>,
        settings: &Path,
        qwen_on_disk: bool,
    ) -> Option<ModelPlan> {
        plan_models(e, &Manifest::embedded(), settings, &|p: &Path| {
            (qwen_on_disk && p.file_name().is_some_and(|n| n == QWEN_FILE)).then(qwen_size)
        })
        .unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aivtuber-start-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_voice_files_sit_beside_the_env_file_and_the_key_is_not_in_the_settings_file() {
        let env_file = Path::new("rel").join("local-profile.env");
        let pairs = app_voice_env(&env_file);
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| PathBuf::from(v))
                .unwrap()
        };
        let settings = get("AIVTUBER_VOICE_SETTINGS");
        let key = get("AIVTUBER_AIVIS_KEY_FILE");
        assert_eq!(settings.parent(), env_file.parent());
        assert_eq!(key.parent(), env_file.parent());
        assert_eq!(settings.file_name().unwrap(), VOICE_SETTINGS_FILE);
        assert_eq!(key.file_name().unwrap(), AIVIS_KEY_FILE);
        assert_ne!(settings, key, "the key has its own file");
        assert_ne!(VOICE_SETTINGS_FILE, MODEL_SETTINGS_FILE);
    }

    #[test]
    fn only_the_default_model_keeps_the_single_model_launch() {
        let settings = temp_dir("single").join(MODEL_SETTINGS_FILE);
        let p = plan(&env("99"), &settings, false).unwrap();
        assert!(!p.is_router());
        assert_eq!(p.initial, LLAMA_ALIAS);
        assert_eq!(p.catalog.models().len(), 1, "the selector shows one model");
        let env_pairs = app_model_env(&p, &settings);
        let get = |k: &str| {
            env_pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("AIVTUBER_OPENAI_MODEL"), Some(LLAMA_ALIAS));
        assert_eq!(
            get("AIVTUBER_OPENAI_MODEL_CHOICES"),
            Some(&*format!("{LLAMA_ALIAS}:UD-Q4_K_XL"))
        );
        assert!(
            get("AIVTUBER_OPENAI_MODEL_SETTINGS")
                .unwrap()
                .ends_with(MODEL_SETTINGS_FILE)
        );
    }

    #[test]
    fn an_installed_optional_model_turns_on_the_router_and_the_saved_choice_is_validated() {
        let dir = temp_dir("router");
        let settings = dir.join(MODEL_SETTINGS_FILE);
        let p = plan(&env("99"), &settings, true).unwrap();
        assert!(p.is_router());
        assert_eq!(
            p.models
                .iter()
                .map(|m| m.alias.as_str())
                .collect::<Vec<_>>(),
            [LLAMA_ALIAS, "qwen3-4b-instruct-2507"]
        );
        assert_eq!(p.models[1].gguf, PathBuf::from("/x").join(QWEN_FILE));
        assert_eq!(p.initial, LLAMA_ALIAS, "no saved choice: the default");
        // A saved choice that is installed is used...
        std::fs::write(
            &settings,
            aivtuber_domain::render_model_settings("qwen3-4b-instruct-2507").unwrap(),
        )
        .unwrap();
        let p = plan(&env("99"), &settings, true).unwrap();
        assert_eq!(p.initial, "qwen3-4b-instruct-2507");
        let pairs = app_model_env(&p, &settings);
        assert!(pairs.contains(&(
            "AIVTUBER_OPENAI_MODEL_VERSION".to_owned(),
            "Q4_K_M".to_owned()
        )));
        // ...but not once the file is gone, and a bad file is ignored.
        assert_eq!(
            plan(&env("99"), &settings, false).unwrap().initial,
            LLAMA_ALIAS
        );
        std::fs::write(&settings, "C:\\evil.gguf").unwrap();
        assert_eq!(
            plan(&env("99"), &settings, true).unwrap().initial,
            LLAMA_ALIAS
        );
        std::fs::write(&settings, "x".repeat(100_000)).unwrap();
        assert_eq!(
            plan(&env("99"), &settings, true).unwrap().initial,
            LLAMA_ALIAS
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_choice_is_read_boundedly_and_only_while_installed() {
        let dir = temp_dir("read");
        let path = dir.join(MODEL_SETTINGS_FILE);
        let both = ThinkingModelCatalog::parse_choices("a:v1,b:v2").unwrap();
        assert_eq!(read_model_settings(&path, &both), None, "missing file");
        let doc = aivtuber_domain::render_model_settings("b").unwrap();
        std::fs::write(&path, &doc).unwrap();
        assert_eq!(read_model_settings(&path, &both).as_deref(), Some("b"));
        let only_a = ThinkingModelCatalog::parse_choices("a:v1").unwrap();
        assert_eq!(read_model_settings(&path, &only_a), None, "uninstalled");
        std::fs::write(&path, "x".repeat(10_000)).unwrap();
        assert_eq!(read_model_settings(&path, &both), None, "oversized");
        std::fs::write(&path, b"\xff\xfe").unwrap();
        assert_eq!(read_model_settings(&path, &both), None, "garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_perception_profile_has_no_model_switch() {
        let mut e = env("99");
        e.insert("AIVTUBER_SETUP_PROFILE".into(), "perception".into());
        assert_eq!(plan(&e, Path::new("/s.json"), true), None);
        let mut e = env("99");
        e.insert("AIVTUBER_SETUP_LLAMA_MMPROJ".into(), "/x/mm.gguf".into());
        assert_eq!(plan(&e, Path::new("/s.json"), true), None);
    }

    #[test]
    fn the_preset_lists_every_model_with_the_single_model_flags() {
        let p = plan(&env("99"), Path::new("/none.json"), true).unwrap();
        let ini = router_preset(&p.models, 99);
        assert!(ini.contains("version = 1"));
        assert!(ini.contains(&format!("[{LLAMA_ALIAS}]\nmodel = /x/m.gguf\nctx-size = 4096\nn-gpu-layers = 99\nreasoning = off\n")), "{ini}");
        assert!(ini.contains("[qwen3-4b-instruct-2507]"), "{ini}");
        assert!(ini.contains(&format!("model = /x/{QWEN_FILE}")), "{ini}");
        // CPU only: no GPU layers key. Windows paths use forward slashes.
        let cpu = router_preset(
            &[ServedModel {
                alias: "a".into(),
                version: "v".into(),
                gguf: PathBuf::from("C:\\llm\\a.gguf"),
                context: 2048,
            }],
            0,
        );
        assert!(!cpu.contains("n-gpu-layers"));
        assert!(cpu.contains("model = C:/llm/a.gguf"), "{cpu}");
    }

    #[test]
    fn the_router_command_is_loopback_with_one_resident_model() {
        let c = router_command(&env("99"), Path::new("/x/llama-models.ini")).unwrap();
        assert_eq!(c.port, 18080);
        let joined = c.args.join(" ");
        assert!(joined.contains("--models-preset"), "{joined}");
        assert!(joined.contains("--models-max 1"), "{joined}");
        assert!(joined.contains("--host 127.0.0.1"), "{joined}");
        assert!(
            !joined.contains(" -m "),
            "router mode takes no single model: {joined}"
        );
    }

    #[test]
    fn a_missing_key_says_how_to_fix_it() {
        let mut e = env("0");
        e.remove("AIVTUBER_SETUP_LLAMA_GGUF");
        let err = llama_command(&e).err().unwrap();
        assert!(err.contains("AIVTUBER_SETUP_LLAMA_GGUF") && err.contains("aivtuber-setup local"));
    }
}
