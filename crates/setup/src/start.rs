//! `aivtuber-setup start`: runs the local profile from the env file written by
//! `aivtuber-setup local` (quickstart steps 3 and 4 in one command).
//!
//! It starts llama-server on loopback, waits for `/health`, then runs
//! `aivtuber-app` with the env file's variables in the foreground (chat is read
//! from this terminal's stdin, exactly as when the app is started by hand), and
//! stops llama-server when the app exits.

use crate::config::{LLAMA_ALIAS, LLAMA_DEFAULT_CONTEXT, parse_env};
use std::collections::BTreeMap;
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

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Returns the app's exit code.
pub fn start(opts: &StartOptions) -> Result<i32, String> {
    let env = load_env(&opts.env_file)?;
    let llm = llm(&env)?;
    let app = PathBuf::from(need(&env, "AIVTUBER_SETUP_APP")?);
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
        println!("starting the app (type chat lines here; Ctrl+C stops it)");
        let status = Command::new(&app)
            .envs(env.iter())
            .status()
            .map_err(|e| format!("cannot start {}: {e}", app.display()))?;
        return Ok(status.code().unwrap_or(1));
    };
    if port_in_use(llama.port) {
        return Err(format!(
            "port {} is already in use (a /health check there could be answered by someone else's server). Stop that program, or change AIVTUBER_SETUP_LLAMA_PORT and the port in AIVTUBER_OPENAI_ENDPOINT in {}",
            llama.port,
            opts.env_file.display()
        ));
    }
    let log_path = opts.env_file.with_file_name("llama-server.log");
    let log = std::fs::File::create(&log_path)
        .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;
    let mut child = Command::new(&llama.program)
        .args(&llama.args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot start llama-server: {e}"))?;
    println!(
        "waiting for llama-server (model load can take a minute; log: {})",
        log_path.display()
    );
    let deadline = Instant::now() + opts.health_timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!(
                "llama-server exited ({status}); see {}",
                log_path.display()
            ));
        }
        if healthy(llama.port) {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err(format!(
                "llama-server was not healthy within {} s; see {}",
                opts.health_timeout.as_secs(),
                log_path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _guard = KillOnDrop(child);
    println!(
        "llama-server ready; starting the app (type chat lines here; Ctrl+C stops everything)"
    );
    let status = Command::new(&app)
        .envs(env.iter())
        .status()
        .map_err(|e| format!("cannot start {}: {e}", app.display()))?;
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

    #[test]
    fn a_missing_key_says_how_to_fix_it() {
        let mut e = env("0");
        e.remove("AIVTUBER_SETUP_LLAMA_GGUF");
        let err = llama_command(&e).err().unwrap();
        assert!(err.contains("AIVTUBER_SETUP_LLAMA_GGUF") && err.contains("aivtuber-setup local"));
    }
}
