//! `aivtuber-setup start`: runs the local profile from the env file written by
//! `aivtuber-setup local` (quickstart steps 3 and 4 in one command), or the
//! hybrid profile from the one `aivtuber-setup hybrid` wrote (#366), where
//! Thinking is remote and only the app is started.
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
    render_speech_input_settings,
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
    /// `--verbose`: give the app `AIVTUBER_LOG=verbose` (every line printed).
    pub verbose: bool,
    /// `--voice-input on|off` (#397): remember the choice in the state file
    /// before starting. `None` leaves the remembered value (default off).
    pub voice_input: Option<bool>,
    pub health_timeout: Duration,
}

/// Same name as `aivtuber_adapters::log_verbosity::LOG_ENV`. The setup binary
/// deliberately does not link the adapters crate (it is a dev-dependency only,
/// to keep the release setup tool small); a test pins the two together.
const LOG_ENV: &str = "AIVTUBER_LOG";

/// What `start` does about the app's `AIVTUBER_LOG` (#391).
#[derive(Debug, PartialEq, Eq)]
pub struct LogChoice {
    /// A value to put in the child's environment; `None` leaves the user's own
    /// value (process environment or env file) untouched.
    pub set: Option<&'static str>,
    /// Whether the app will end up quiet, so `start` says so.
    pub quiet: bool,
}

/// `--verbose` wins; otherwise an explicit non-empty user value (env file, then
/// process environment) wins; otherwise quiet is the default under `start`.
pub fn log_choice(
    verbose_flag: bool,
    file_env: &BTreeMap<String, String>,
    process_value: Option<&str>,
) -> LogChoice {
    if verbose_flag {
        return LogChoice {
            set: Some("verbose"),
            quiet: false,
        };
    }
    let explicit = file_env
        .get(LOG_ENV)
        .map(String::as_str)
        .or(process_value)
        .filter(|v| !v.trim().is_empty());
    match explicit {
        Some(value) => LogChoice {
            set: None,
            quiet: value.trim().eq_ignore_ascii_case("quiet"),
        },
        None => LogChoice {
            set: Some("quiet"),
            quiet: true,
        },
    }
}

/// Quiet start-up text (#393), Japanese first with English beside it. Fixed
/// strings, no locale detection. With the app's five lines
/// (`aivtuber_app::ready_guide`) the quiet start-up adds at most eight lines.
/// The Japanese text can be garbled when Windows PowerShell 5.1 output is
/// redirected or piped; the English half stays readable.
const QUIET_NOTE: &str = "表示を簡略にしています。すべて表示するには `aivtuber-setup start --verbose` / Terminal output is quiet; run `aivtuber-setup start --verbose` to see everything.";
const READY_STARTING: &str =
    "llama-server の準備ができました。アプリを起動します。 / llama-server ready; starting the app.";
const STARTING_APP: &str = "アプリを起動します。 / Starting the app.";
const TYPE_HERE: &str = "この画面に文字を入力して Enter でも話しかけられます。終了は Ctrl+C。 / Type here and press Enter to talk; Ctrl+C stops everything.";
#[cfg(test)]
const QUIET_LINES: [&str; 4] = [QUIET_NOTE, READY_STARTING, STARTING_APP, TYPE_HERE];

/// Whether `start` prints the llama-server command line, the model list and
/// the app path. A dry run exists to show them, so it always does.
pub fn show_launch_details(quiet: bool, dry_run: bool) -> bool {
    !quiet || dry_run
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
            "cannot read {}: {e}; run `aivtuber-setup local` (or `aivtuber-setup hybrid`) first",
            path.display()
        )
    })?;
    Ok(parse_env(&text).into_iter().collect())
}

/// The env file `start` uses when `--env-file` is not given: the one `local`
/// or `hybrid` wrote beside this program. Both present is ambiguous and is
/// refused rather than guessed, because they start different profiles.
pub fn default_env_file(dir: &Path, exists: &dyn Fn(&Path) -> bool) -> Result<PathBuf, String> {
    let local = dir.join("local-profile.env");
    let hybrid = dir.join("hybrid-profile.env");
    match (exists(&local), exists(&hybrid)) {
        (true, true) => Err(format!(
            "both {} and {} exist; say which profile to start with --env-file",
            local.display(),
            hybrid.display()
        )),
        (false, true) => Ok(hybrid),
        _ => Ok(local),
    }
}

/// What `aivtuber-setup local` recorded about the language model.
#[derive(Debug, PartialEq, Eq)]
pub enum Llm {
    /// Start llama-server with this command.
    Managed(LlamaCommand),
    /// The operator runs the model server at the endpoint in the env file.
    External,
    /// `aivtuber-setup hybrid` (#366): Thinking is OpenRouter; no model server
    /// is started here.
    Remote,
}

/// Decides how the model is served, or refuses with the way out. An env file
/// from before profiles existed has no mode key and is treated as managed.
pub fn llm(env: &BTreeMap<String, String>) -> Result<Llm, String> {
    match env.get("AIVTUBER_SETUP_LLAMA_MODE").map(String::as_str) {
        None | Some("managed") => llama_command(env).map(Llm::Managed),
        Some("external") => Ok(Llm::External),
        Some("remote") => {
            for key in ["AIVTUBER_OPENAI_API_KEY", "AIVTUBER_OPENAI_MODEL"] {
                if env.get(key).is_none_or(|v| v.trim().is_empty()) {
                    return Err(format!(
                        "{key} is missing from the env file; run `aivtuber-setup hybrid --force` to write it again"
                    ));
                }
            }
            Ok(Llm::Remote)
        }
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
/// File beside the env file that remembers the last Twitch channel and YouTube
/// video connected from the panel (#361). Offered on the next start, never
/// connected automatically. It never holds the API key.
pub const CHAT_SETTINGS_FILE: &str = "local-chat.json";
/// The YouTube Data API key entered in the panel, in its own file. Owner-only
/// where the OS allows. The app owns it; `YOUTUBE_API_KEY` in the environment
/// still wins.
pub const YOUTUBE_KEY_FILE: &str = "local-youtube-key.txt";
/// File beside the env file that remembers which VRM avatar model is active
/// (#362): `sample` or the content hash of an uploaded model. Small, replaced
/// atomically; a missing or invalid file means the sample.
pub const AVATAR_SETTINGS_FILE: &str = "local-avatar.json";
/// File beside the env file that remembers whether voice input (the microphone,
/// VAD and speech recognition) is on (#397). `start --voice-input on|off`
/// writes it; the app reads it and opens no capture device unless it says
/// enabled. A missing or invalid file means off. Since #400 the app rewrites it
/// too, when the operator switches voice input in the panel or with
/// `aivtuberctl voice-input` (the same atomic write, the same document).
pub const SPEECH_INPUT_SETTINGS_FILE: &str = "local-speech-input.json";
/// Same name as `aivtuber_app::speech_input::SPEECH_INPUT_SETTINGS_VAR`; a test
/// pins the two together (the app crate is not linked here).
const SPEECH_INPUT_SETTINGS_VAR: &str = "AIVTUBER_SPEECH_INPUT_SETTINGS";
const SPEECH_SIDECAR_VAR: &str = "AIVTUBER_SPEECH_SIDECAR";
/// The character sheet beside the env file (#404): Markdown the streamer edits
/// (`examples/persona/starter.ja.md` in the release archive is the template).
/// The app reads it once at start; missing or empty means no character sheet.
pub const PERSONA_FILE: &str = "local-persona.md";
/// Same name as `aivtuber_app::persona::PERSONA_FILE_VAR`; a test pins the two
/// together by value (the app crate is not linked here).
const PERSONA_FILE_VAR: &str = "AIVTUBER_PERSONA_FILE";
/// The operator memory seed beside the env file (#402): the JSON the streamer
/// edits in the panel or with `aivtuberctl memory-seed`
/// (`schemas/memory-seed.schema.json`). `start` creates it empty when absent so
/// the app can load it and the panel has a file to save to.
pub const MEMORY_SEED_FILE: &str = "local-memory-seed.json";
/// Same name as `aivtuber_app::MEMORY_SEED_FILE_VAR`; a test pins the two
/// together by value (the app crate is not linked here).
const MEMORY_SEED_FILE_VAR: &str = "AIVTUBER_MEMORY_SEED";
/// The empty seed written when the file is absent. It is a fixture the app's
/// own loader test validates, so it cannot drift from the seed contract.
const EMPTY_MEMORY_SEED: &str =
    include_str!("../../../examples/memory-seeds/valid/local-empty.json");
/// Directory beside the env file that holds the models uploaded in the panel
/// (#362), named by content hash, at most a few. The app owns it; no file name
/// or path ever comes from a client.
pub const AVATAR_DIR: &str = "avatars";

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

/// Where the app keeps the chat choice and the YouTube key (#361), layered
/// under the env file's own values like [`app_voice_env`]. The sidecars are
/// started by the app from the directory it runs from, which is this archive's.
pub fn app_chat_env(env_file: &Path) -> Vec<(String, String)> {
    vec![
        (
            "AIVTUBER_CHAT_INGRESS_SETTINGS".to_owned(),
            env_file
                .with_file_name(CHAT_SETTINGS_FILE)
                .display()
                .to_string(),
        ),
        (
            "AIVTUBER_YOUTUBE_KEY_FILE".to_owned(),
            env_file
                .with_file_name(YOUTUBE_KEY_FILE)
                .display()
                .to_string(),
        ),
    ]
}

/// Where the app keeps the avatar choice and the uploaded models (#362),
/// layered under the env file's own values like [`app_voice_env`]. The sample
/// stays where the renderer page directory ships it.
pub fn app_avatar_env(env_file: &Path) -> Vec<(String, String)> {
    vec![
        (
            "AIVTUBER_AVATAR_SETTINGS".to_owned(),
            env_file
                .with_file_name(AVATAR_SETTINGS_FILE)
                .display()
                .to_string(),
        ),
        (
            "AIVTUBER_AVATAR_DIR".to_owned(),
            env_file.with_file_name(AVATAR_DIR).display().to_string(),
        ),
    ]
}

/// Where the app finds the voice-input state file (#397), whenever the profile
/// has a speech sidecar at all: without this variable the app starts a
/// configured sidecar unconditionally, which is how v0.1.6 opened the
/// microphone by default. An explicit value in the env file or the process
/// environment wins; a profile without a sidecar gets nothing.
pub fn app_speech_input_env(
    env_file: &Path,
    file_env: &BTreeMap<String, String>,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let set = |name: &str| {
        file_env.get(name).is_some_and(|v| !v.trim().is_empty())
            || process_env(name).is_some_and(|v| !v.trim().is_empty())
    };
    if !set(SPEECH_SIDECAR_VAR) || set(SPEECH_INPUT_SETTINGS_VAR) {
        return Vec::new();
    }
    vec![(
        SPEECH_INPUT_SETTINGS_VAR.to_owned(),
        env_file
            .with_file_name(SPEECH_INPUT_SETTINGS_FILE)
            .display()
            .to_string(),
    )]
}

/// Where the app finds the character sheet (#404): `local-persona.md` beside the
/// env file, for the local and hybrid profiles (the only ones with a Thinking
/// model that can use it). An explicit value in the env file or the process
/// environment wins. The file need not exist: the app treats a missing file as
/// no character sheet, so this changes nothing until the streamer creates it.
pub fn app_persona_env(
    env_file: &Path,
    file_env: &BTreeMap<String, String>,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let value = |name: &str| {
        file_env
            .get(name)
            .cloned()
            .or_else(|| process_env(name))
            .filter(|v| !v.trim().is_empty())
    };
    let generative = matches!(value("AIVTUBER_MODE").as_deref(), Some("local" | "hybrid"));
    if !generative || value(PERSONA_FILE_VAR).is_some() {
        return Vec::new();
    }
    vec![(
        PERSONA_FILE_VAR.to_owned(),
        env_file.with_file_name(PERSONA_FILE).display().to_string(),
    )]
}

/// Where the app finds the operator memory seed (#402): `local-memory-seed.json`
/// beside the env file, for the local and hybrid profiles (the only ones with a
/// Thinking model that can use it). An explicit value in the env file or the
/// process environment wins and is never created or touched here.
pub fn app_memory_seed_env(
    env_file: &Path,
    file_env: &BTreeMap<String, String>,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let value = |name: &str| {
        file_env
            .get(name)
            .cloned()
            .or_else(|| process_env(name))
            .filter(|v| !v.trim().is_empty())
    };
    let generative = matches!(value("AIVTUBER_MODE").as_deref(), Some("local" | "hybrid"));
    if !generative || value(MEMORY_SEED_FILE_VAR).is_some() {
        return Vec::new();
    }
    vec![(
        MEMORY_SEED_FILE_VAR.to_owned(),
        env_file
            .with_file_name(MEMORY_SEED_FILE)
            .display()
            .to_string(),
    )]
}

/// Creates the empty seed at `path` unless something is already there (an
/// existing file, valid or not, is the streamer's and is never overwritten).
pub fn ensure_memory_seed_file(path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(EMPTY_MEMORY_SEED.as_bytes()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Replaces the state file with the on/off document (temp file in the same
/// directory, then rename, so a crash leaves the old or the new file, never
/// half of one). Same pattern as the app's own settings writers.
pub fn write_speech_input_settings(path: &Path, enabled: bool) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, render_speech_input_settings(enabled))?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// The state file `--voice-input` writes: the explicit value the user gave (env
/// file, then process environment), else the one `start` derived. `None` when
/// the profile has no speech sidecar.
fn speech_input_file(
    env: &BTreeMap<String, String>,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    env.get(SPEECH_INPUT_SETTINGS_VAR)
        .cloned()
        .or_else(|| process_env(SPEECH_INPUT_SETTINGS_VAR))
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
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
    // The child inherits the process environment, so an explicit user value is
    // simply left alone; only the default and `--verbose` are written here.
    let process_log = std::env::var(LOG_ENV).ok();
    let log = log_choice(opts.verbose, &env, process_log.as_deref());
    if let Some(value) = log.set {
        env.insert(LOG_ENV.to_owned(), value.to_owned());
    }
    for (name, value) in app_voice_env(&opts.env_file)
        .into_iter()
        .chain(app_chat_env(&opts.env_file))
        .chain(app_avatar_env(&opts.env_file))
    {
        env.entry(name).or_insert(value);
    }
    let speech_input_vars =
        app_speech_input_env(&opts.env_file, &env, &|name| std::env::var(name).ok());
    env.extend(speech_input_vars);
    let persona_vars = app_persona_env(&opts.env_file, &env, &|name| std::env::var(name).ok());
    env.extend(persona_vars);
    let seed_vars = app_memory_seed_env(&opts.env_file, &env, &|name| std::env::var(name).ok());
    for (_, path) in &seed_vars {
        ensure_memory_seed_file(Path::new(path))
            .map_err(|e| format!("cannot create {path}: {e}"))?;
    }
    env.extend(seed_vars);
    let plan = match &llm {
        Llm::Managed(_) => plan_models(&env, &Manifest::embedded(), &settings, &|p| {
            std::fs::metadata(p).ok().map(|m| m.len())
        })?,
        Llm::External | Llm::Remote => None,
    };
    let mut llm = llm;
    if let (Some(plan), Llm::Managed(llama)) = (&plan, &mut llm) {
        env.extend(app_model_env(plan, &settings));
        if plan.is_router() {
            *llama = router_command(&env, &preset)?;
        }
    }
    let details = show_launch_details(log.quiet, opts.dry_run);
    match &llm {
        Llm::Managed(llama) if details => println!(
            "llama-server: {} {}",
            llama.program.display(),
            llama.args.join(" ")
        ),
        Llm::Managed(_) => {}
        Llm::Remote => println!(
            "language model: OpenRouter, model {} (remote: chat and reply text are sent to openrouter.ai; no local model server is started)",
            env.get("AIVTUBER_OPENAI_MODEL").map_or("?", String::as_str)
        ),
        Llm::External => println!(
            "language model: your own server at {} (not started here)",
            env.get("AIVTUBER_OPENAI_ENDPOINT")
                .map_or("?", String::as_str)
        ),
    }
    if let Some(plan) = plan.as_ref().filter(|_| details) {
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
    if details {
        println!(
            "app:          {} (environment from {})",
            app.display(),
            opts.env_file.display()
        );
    }
    if log.quiet {
        println!("{QUIET_NOTE}");
    }
    // #397: the state file the app was told about (ours, or the user's own
    // value); absent when the profile has no speech sidecar.
    let speech_input_file = speech_input_file(&env, &|name| std::env::var(name).ok());
    if let Some(enabled) = opts.voice_input {
        let word = if enabled { "on" } else { "off" };
        match &speech_input_file {
            Some(path) if opts.dry_run => {
                println!("voice input: would be turned {word} in {}", path.display());
            }
            Some(_) => {}
            None => println!(
                "voice input: --voice-input {word} has no effect; {} configures no speech input (AIVTUBER_SPEECH_SIDECAR)",
                opts.env_file.display()
            ),
        }
    }
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
    if let (Some(enabled), Some(path)) = (opts.voice_input, &speech_input_file) {
        write_speech_input_settings(path, enabled)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let Llm::Managed(llama) = llm else {
        announce_app(
            log.quiet,
            STARTING_APP,
            "starting the app",
            "Ctrl+C stops it",
        );
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
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;
    let mut guard = KillOnDrop(
        Command::new(&llama.program)
            .args(&llama.args)
            .stdin(Stdio::null())
            .stdout(log_file.try_clone().map_err(|e| e.to_string())?)
            .stderr(log_file)
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
    announce_app(
        log.quiet,
        READY_STARTING,
        "llama-server ready; starting the app",
        "Ctrl+C stops everything",
    );
    run_app(&app, &env)
}

/// Quiet: the short Japanese-first lines. Otherwise the line printed before #393.
fn announce_app(quiet: bool, quiet_first: &str, verbose_first: &str, stop: &str) {
    if quiet {
        println!("{quiet_first}");
        println!("{TYPE_HERE}");
    } else {
        println!("{verbose_first} ({CHAT_HINT}; {stop})");
    }
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

    #[test]
    fn the_quiet_text_is_short_and_carries_no_secret() {
        // The app prints five more (aivtuber_app::ready_guide, the fourth being
        // the voice-input line of #397 and the fifth the character sheet line of
        // #404); eight in all.
        // STARTING_APP and READY_STARTING are alternatives, never both.
        assert!(QUIET_LINES.len() - 1 <= 3);
        for line in QUIET_LINES {
            assert!(!line.trim().is_empty() && !line.contains('\n'));
            assert!(!line.contains("token="), "{line}");
        }
        assert!(QUIET_NOTE.contains("aivtuber-setup start --verbose"));
    }

    // The literal documents the app's own tests pin
    // (crates/app/src/speech_input.rs, crates/domain/src/speech_input.rs).
    const SPEECH_ON: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}
";
    const SPEECH_OFF: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}
";

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_app_is_told_where_the_voice_input_state_file_is_when_a_sidecar_is_configured() {
        let env_file = Path::new("/x/local-profile.env");
        let none = |_: &str| None;
        let configured = map(&[("AIVTUBER_SPEECH_SIDECAR", "/x/speech-vad-sidecar")]);
        let got = app_speech_input_env(env_file, &configured, &none);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "AIVTUBER_SPEECH_INPUT_SETTINGS");
        assert!(got[0].1.ends_with(SPEECH_INPUT_SETTINGS_FILE), "{got:?}");
        // The file sits beside the env file, like the other settings.
        assert_eq!(Path::new(&got[0].1).parent(), env_file.parent(), "{got:?}");
        // An env file written by v0.1.6 or earlier has no such variable; the
        // same decision covers it, so the missing state file means off.
        // A profile without a sidecar gets nothing.
        assert!(app_speech_input_env(env_file, &map(&[]), &none).is_empty());
        assert!(
            app_speech_input_env(env_file, &map(&[("AIVTUBER_SPEECH_SIDECAR", " ")]), &none)
                .is_empty()
        );
        // A sidecar named only in the process environment counts too.
        let process = |n: &str| (n == "AIVTUBER_SPEECH_SIDECAR").then(|| "/y/side".to_owned());
        assert_eq!(app_speech_input_env(env_file, &map(&[]), &process).len(), 1);
        // An explicit value (env file or process) wins.
        let explicit = map(&[
            ("AIVTUBER_SPEECH_SIDECAR", "/x/s"),
            ("AIVTUBER_SPEECH_INPUT_SETTINGS", "/mine.json"),
        ]);
        assert!(app_speech_input_env(env_file, &explicit, &none).is_empty());
        let process_explicit =
            |n: &str| (n == "AIVTUBER_SPEECH_INPUT_SETTINGS").then(|| "/p".to_owned());
        assert!(app_speech_input_env(env_file, &configured, &process_explicit).is_empty());
    }

    #[test]
    fn the_app_is_told_where_the_character_sheet_is_for_local_and_hybrid_only() {
        let env_file = Path::new("/x/local-profile.env");
        let none = |_: &str| None;
        for mode in ["local", "hybrid"] {
            let got = app_persona_env(env_file, &map(&[("AIVTUBER_MODE", mode)]), &none);
            assert_eq!(got.len(), 1, "{mode}");
            assert_eq!(got[0].0, "AIVTUBER_PERSONA_FILE");
            assert!(got[0].1.ends_with(PERSONA_FILE), "{got:?}");
            // Beside the env file, like the other settings.
            assert_eq!(Path::new(&got[0].1).parent(), env_file.parent(), "{got:?}");
        }
        // The names are the app's: a test in the app pins the same literals.
        assert_eq!(PERSONA_FILE, "local-persona.md");
        assert_eq!(PERSONA_FILE_VAR, "AIVTUBER_PERSONA_FILE");
        // Other profiles, and an env file with no mode, get nothing.
        assert!(app_persona_env(env_file, &map(&[("AIVTUBER_MODE", "cached")]), &none).is_empty());
        assert!(app_persona_env(env_file, &map(&[]), &none).is_empty());
        // The mode can come from the process environment.
        let process_mode = |n: &str| (n == "AIVTUBER_MODE").then(|| "local".to_owned());
        assert_eq!(app_persona_env(env_file, &map(&[]), &process_mode).len(), 1);
        // An explicit value (env file or process environment) wins.
        let explicit = map(&[
            ("AIVTUBER_MODE", "local"),
            ("AIVTUBER_PERSONA_FILE", "/mine.md"),
        ]);
        assert!(app_persona_env(env_file, &explicit, &none).is_empty());
        let process_explicit = |n: &str| (n == "AIVTUBER_PERSONA_FILE").then(|| "/p.md".to_owned());
        assert!(
            app_persona_env(
                env_file,
                &map(&[("AIVTUBER_MODE", "local")]),
                &process_explicit
            )
            .is_empty()
        );
    }

    #[test]
    fn the_app_is_told_where_the_memory_seed_is_for_local_and_hybrid_only() {
        let env_file = Path::new("/x/local-profile.env");
        let none = |_: &str| None;
        for mode in ["local", "hybrid"] {
            let got = app_memory_seed_env(env_file, &map(&[("AIVTUBER_MODE", mode)]), &none);
            assert_eq!(got.len(), 1, "{mode}");
            assert_eq!(got[0].0, "AIVTUBER_MEMORY_SEED");
            assert!(got[0].1.ends_with(MEMORY_SEED_FILE), "{got:?}");
            assert_eq!(Path::new(&got[0].1).parent(), env_file.parent(), "{got:?}");
        }
        assert_eq!(MEMORY_SEED_FILE, "local-memory-seed.json");
        assert_eq!(MEMORY_SEED_FILE_VAR, "AIVTUBER_MEMORY_SEED");
        assert!(
            app_memory_seed_env(env_file, &map(&[("AIVTUBER_MODE", "cached")]), &none).is_empty()
        );
        assert!(app_memory_seed_env(env_file, &map(&[]), &none).is_empty());
        let process_mode = |n: &str| (n == "AIVTUBER_MODE").then(|| "local".to_owned());
        assert_eq!(
            app_memory_seed_env(env_file, &map(&[]), &process_mode).len(),
            1
        );
        // An explicit value (env file or process environment) wins.
        let explicit = map(&[
            ("AIVTUBER_MODE", "local"),
            ("AIVTUBER_MEMORY_SEED", "/mine.json"),
        ]);
        assert!(app_memory_seed_env(env_file, &explicit, &none).is_empty());
        let process_explicit =
            |n: &str| (n == "AIVTUBER_MEMORY_SEED").then(|| "/p.json".to_owned());
        assert!(
            app_memory_seed_env(
                env_file,
                &map(&[("AIVTUBER_MODE", "local")]),
                &process_explicit
            )
            .is_empty()
        );
    }

    #[test]
    fn the_empty_seed_is_created_only_when_absent_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("aivtuber-seed-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MEMORY_SEED_FILE);
        let _ = std::fs::remove_file(&path);
        ensure_memory_seed_file(&path).unwrap();
        let created = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&created).unwrap();
        assert_eq!(value["schema_version"], "0.1.0");
        assert_eq!(value["entries"], serde_json::json!([]));
        // The streamer's own file, even an invalid one, is left alone.
        std::fs::write(&path, "my own notes").unwrap();
        ensure_memory_seed_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my own notes");
        // A directory that is not there is an error, not a silent skip.
        assert!(ensure_memory_seed_file(&dir.join("missing").join("s.json")).is_err());
    }

    #[test]
    fn the_state_file_to_write_follows_an_explicit_value_wherever_it_was_given() {
        let none = |_: &str| None;
        let process = |n: &str| (n == "AIVTUBER_SPEECH_INPUT_SETTINGS").then(|| "/p/s.json".into());
        let derived = map(&[(
            "AIVTUBER_SPEECH_INPUT_SETTINGS",
            "/x/local-speech-input.json",
        )]);
        assert_eq!(
            speech_input_file(&derived, &none),
            Some(PathBuf::from("/x/local-speech-input.json"))
        );
        // Only the process environment names it: `start` derived nothing, the
        // flag still writes where the app will read.
        assert_eq!(
            speech_input_file(&map(&[]), &process),
            Some(PathBuf::from("/p/s.json"))
        );
        assert_eq!(speech_input_file(&map(&[]), &none), None);
        assert_eq!(
            speech_input_file(&map(&[("AIVTUBER_SPEECH_INPUT_SETTINGS", " ")]), &none),
            None
        );
    }

    #[test]
    fn the_variable_name_is_the_one_the_app_reads() {
        // The app crate is not linked here; its own test pins the same literal.
        assert_eq!(SPEECH_INPUT_SETTINGS_VAR, "AIVTUBER_SPEECH_INPUT_SETTINGS");
    }

    #[test]
    fn the_state_file_writer_replaces_atomically_and_matches_the_pinned_documents() {
        let dir = temp_dir("speech-input");
        let path = dir.join(SPEECH_INPUT_SETTINGS_FILE);
        let _ = std::fs::remove_file(&path);
        write_speech_input_settings(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SPEECH_ON);
        write_speech_input_settings(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SPEECH_OFF);
        write_speech_input_settings(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SPEECH_ON);
        // No temporary file is left behind.
        assert!(
            !dir.join(format!("{SPEECH_INPUT_SETTINGS_FILE}.tmp"))
                .exists()
        );
        assert!((SPEECH_ON.len() as u64) < aivtuber_domain::MAX_SPEECH_INPUT_SETTINGS_BYTES);
        assert!(aivtuber_domain::parse_speech_input_settings(
            SPEECH_ON.as_bytes()
        ));
        assert!(!aivtuber_domain::parse_speech_input_settings(
            SPEECH_OFF.as_bytes()
        ));
        // An unwritable location is an error, not a silent success.
        assert!(
            write_speech_input_settings(&dir.join("no-such-dir").join("x.json"), true).is_err()
        );
    }

    #[test]
    fn launch_details_are_hidden_only_in_a_quiet_real_run() {
        assert!(!show_launch_details(true, false));
        assert!(show_launch_details(true, true));
        assert!(show_launch_details(false, false));
        assert!(show_launch_details(false, true));
    }

    #[test]
    fn log_env_name_matches_the_adapters_helper() {
        assert_eq!(LOG_ENV, aivtuber_adapters::log_verbosity::LOG_ENV);
    }

    #[test]
    fn log_choice_defaults_to_quiet_and_respects_explicit_values() {
        let none = BTreeMap::new();
        let quiet = LogChoice {
            set: Some("quiet"),
            quiet: true,
        };
        assert_eq!(log_choice(false, &none, None), quiet);
        assert_eq!(log_choice(false, &none, Some("  ")), quiet);
        // An explicit user value wins and is left for the child to inherit.
        assert_eq!(
            log_choice(false, &none, Some("verbose")),
            LogChoice {
                set: None,
                quiet: false
            }
        );
        assert_eq!(
            log_choice(false, &none, Some("QUIET")),
            LogChoice {
                set: None,
                quiet: true
            }
        );
        let file: BTreeMap<_, _> = [("AIVTUBER_LOG".to_owned(), "verbose".to_owned())].into();
        assert_eq!(
            log_choice(false, &file, Some("quiet")),
            LogChoice {
                set: None,
                quiet: false
            }
        );
        // --verbose beats everything.
        assert_eq!(
            log_choice(true, &file, Some("quiet")),
            LogChoice {
                set: Some("verbose"),
                quiet: false
            }
        );
    }

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
    fn the_chat_files_sit_beside_the_env_file_and_the_key_has_its_own() {
        let env_file = Path::new("rel").join("local-profile.env");
        let pairs = app_chat_env(&env_file);
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| PathBuf::from(v))
                .unwrap()
        };
        let settings = get("AIVTUBER_CHAT_INGRESS_SETTINGS");
        let key = get("AIVTUBER_YOUTUBE_KEY_FILE");
        assert_eq!(settings.parent(), env_file.parent());
        assert_eq!(key.parent(), env_file.parent());
        assert_eq!(settings.file_name().unwrap(), CHAT_SETTINGS_FILE);
        assert_eq!(key.file_name().unwrap(), YOUTUBE_KEY_FILE);
        assert_ne!(settings, key, "the key has its own file");
        let all = [
            CHAT_SETTINGS_FILE,
            YOUTUBE_KEY_FILE,
            VOICE_SETTINGS_FILE,
            AIVIS_KEY_FILE,
            MODEL_SETTINGS_FILE,
        ];
        let distinct: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len());
    }

    #[test]
    fn the_avatar_files_sit_beside_the_env_file() {
        let env_file = Path::new("rel").join("local-profile.env");
        let pairs = app_avatar_env(&env_file);
        let get = |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| PathBuf::from(v))
                .unwrap()
        };
        let settings = get("AIVTUBER_AVATAR_SETTINGS");
        let dir = get("AIVTUBER_AVATAR_DIR");
        assert_eq!(settings.parent(), env_file.parent());
        assert_eq!(dir.parent(), env_file.parent());
        assert_eq!(settings.file_name().unwrap(), AVATAR_SETTINGS_FILE);
        assert_eq!(dir.file_name().unwrap(), AVATAR_DIR);
        // Not the same name as any other file the launcher keeps there.
        let all = [
            AVATAR_SETTINGS_FILE,
            AVATAR_DIR,
            CHAT_SETTINGS_FILE,
            YOUTUBE_KEY_FILE,
            VOICE_SETTINGS_FILE,
            AIVIS_KEY_FILE,
            MODEL_SETTINGS_FILE,
        ];
        let distinct: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len());
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
