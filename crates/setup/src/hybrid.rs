//! `aivtuber-setup hybrid` (issue #366): the `local` first-run flow with the
//! Thinking role on OpenRouter instead of a local llama-server (ADR-0009,
//! docs/security-threat-model.adoc section 10.1).
//!
//! What is new here is only what the key and the model list need:
//!
//! * the API key comes from `OPENROUTER_API_KEY` or a hidden terminal prompt,
//!   never from argv, and is held as a redacted [`ApiKey`];
//! * `--model` ids are validated and bounded before anything is written;
//! * `--check-key` asks OpenRouter whether the key works before any file is
//!   created, and prints only the reason when it does not.
//!
//! The rest (TTS voice, starter audio, renderer, generated secrets, env file
//! rules) is the `local` flow, see `local::run`.

use crate::config::ApiKey;
use crate::local::{self, LocalOptions, Summary};
use crate::manifest::Manifest;
use crate::prereq;
use std::io::{IsTerminal, Write};
use std::time::Duration;

/// The variable the key is read from. The app itself reads
/// `AIVTUBER_OPENAI_API_KEY`; a separate name keeps the shell variable of a
/// user who already has an OpenRouter key out of the app's environment.
pub const KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Preflight endpoint: reports the key's validity and limits, costs nothing.
pub const KEY_CHECK_URL: &str = "https://openrouter.ai/api/v1/key";

/// At most this many `--model` ids.
pub const MAX_MODELS: usize = 8;

/// Models used when no `--model` is given: inexpensive, non-reasoning chat
/// models that answer short in-character replies. Checked against
/// `GET https://openrouter.ai/api/v1/models` on 2026-11-16: each id was listed,
/// text output, no expiration date, prompt/completion USD per million tokens
/// 0.10/0.40, 0.10/0.30 and 0.10/0.30. Prices and availability move; the
/// quickstart tells the operator to check openrouter.ai/models. Never put a
/// `:free` model here: free tiers are rate limited and may train on prompts.
pub const DEFAULT_MODELS: [&str; 3] = [
    "google/gemini-2.5-flash-lite",
    "qwen/qwen3-30b-a3b-instruct-2507",
    "mistralai/mistral-small-3.2-24b-instruct",
];

const MAX_MODEL_ID_LEN: usize = 96;

/// One OpenRouter model id: `vendor/model`, optionally with a `:variant`
/// suffix. Strict on purpose, because the value goes into the env file and a
/// request body: lowercase letters, digits and `.-_` around exactly one `/`.
pub fn validate_model_id(id: &str) -> Result<(), String> {
    let bad = |why: &str| format!("--model {id:?}: {why}");
    if id.is_empty() || id.len() > MAX_MODEL_ID_LEN {
        return Err(bad(&format!(
            "must be 1 to {MAX_MODEL_ID_LEN} characters long"
        )));
    }
    let (body, variant) = match id.split_once(':') {
        Some((body, variant)) => (body, Some(variant)),
        None => (id, None),
    };
    let part_ok = |s: &str| {
        // The app's model catalog (alias rule of #365) takes lowercase only,
        // each segment starting with a letter or digit.
        s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && s.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
            })
    };
    let mut parts = body.split('/');
    let well_formed = matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(vendor), Some(model), None) if part_ok(vendor) && part_ok(model)
    ) && variant.is_none_or(part_ok);
    if !well_formed {
        return Err(bad(
            "an OpenRouter model id looks like vendor/model (for example google/gemini-2.5-flash-lite), optionally followed by :variant",
        ));
    }
    Ok(())
}

/// The models to configure: the defaults when none were named, otherwise the
/// named ones (de-duplicated, order kept; the first is the model the app
/// starts on). Refuses more than [`MAX_MODELS`] and any invalid id.
pub fn resolve_models(named: &[String]) -> Result<Vec<String>, String> {
    if named.is_empty() {
        return Ok(DEFAULT_MODELS.iter().map(|m| (*m).to_owned()).collect());
    }
    let mut models: Vec<String> = Vec::new();
    for id in named {
        validate_model_id(id)?;
        if !models.contains(id) {
            models.push(id.clone());
        }
    }
    if models.len() > MAX_MODELS {
        return Err(format!(
            "at most {MAX_MODELS} --model values are accepted (got {})",
            models.len()
        ));
    }
    Ok(models)
}

/// Where the key came from, decided without touching the terminal so the
/// choice is testable: the environment variable, else a prompt when a person
/// is at the terminal, else guidance.
pub fn resolve_api_key(
    env_value: Option<String>,
    interactive: bool,
    prompt: impl FnOnce() -> Result<String, String>,
) -> Result<ApiKey, String> {
    if let Some(value) = env_value.filter(|v| !v.trim().is_empty()) {
        return ApiKey::parse(&value).map_err(|e| format!("{KEY_ENV}: {e}"));
    }
    if !interactive {
        return Err(format!(
            "no OpenRouter API key: set {KEY_ENV} in the environment (the key is never accepted as a command-line argument), or run this command from a terminal to be asked for it. Create a key at https://openrouter.ai/settings/keys"
        ));
    }
    ApiKey::parse(&prompt()?)
}

/// The hidden prompt on the real terminal.
pub fn key_from_environment_or_prompt() -> Result<ApiKey, String> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    resolve_api_key(std::env::var(KEY_ENV).ok(), interactive, prompt_hidden)
}

/// Reads one line from the terminal without echo. No crate and no `unsafe`
/// (the repository forbids it): the system's own tool switches the echo off,
/// `stty` on Unix and PowerShell's `Read-Host -AsSecureString` on Windows,
/// both by absolute path so a look-alike earlier on PATH is never run. The key
/// never appears in any argument; it travels over the terminal and, on
/// Windows, a pipe back from PowerShell.
#[cfg(unix)]
fn prompt_hidden() -> Result<String, String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let stty = |arg: &str| {
        Command::new("/bin/stty")
            .arg(arg)
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    eprint!("OpenRouter API key (input is hidden): ");
    let _ = std::io::stderr().flush();
    if !stty("-echo") {
        eprintln!();
        return Err(format!(
            "cannot turn terminal echo off to read the key; set {KEY_ENV} instead"
        ));
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    stty("echo");
    eprintln!();
    read.map_err(|e| format!("cannot read the key: {e}"))?;
    Ok(line)
}

#[cfg(windows)]
fn prompt_hidden() -> Result<String, String> {
    use std::process::{Command, Stdio};
    let root = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || std::path::PathBuf::from("C:\\Windows"),
            std::path::PathBuf::from,
        );
    let powershell = root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    // The script holds no secret; the key is typed at the prompt it shows.
    let script = "$s = Read-Host -AsSecureString 'OpenRouter API key (input is hidden)'; \
                  $b = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s); \
                  [Console]::Out.Write([Runtime.InteropServices.Marshal]::PtrToStringBSTR($b))";
    let output = Command::new(powershell)
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| {
            format!("cannot run PowerShell to read the key ({e}); set {KEY_ENV} instead")
        })?;
    if !output.status.success() {
        return Err(format!(
            "reading the key was cancelled or failed; set {KEY_ENV} instead"
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "the key is not valid text".to_owned())
}

#[cfg(not(any(unix, windows)))]
fn prompt_hidden() -> Result<String, String> {
    Err(format!("no hidden prompt on this platform; set {KEY_ENV}"))
}

/// Asks OpenRouter whether the key is accepted. On failure the error is a
/// reason only: never the key and never the response body.
pub fn check_key(url: &str, key: &ApiKey) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "the key-check URL is not valid".to_owned())?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if parsed.scheme() != "https" && !loopback {
        return Err("the key check needs an https URL".to_owned());
    }
    let mut builder = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        // A redirect must not carry the Authorization header anywhere.
        .max_redirects(0)
        .http_status_as_error(false);
    if loopback {
        builder = builder.proxy(None);
    }
    let agent: ureq::Agent = builder.build().into();
    let response = agent
        .get(parsed.as_str())
        .header("Authorization", format!("Bearer {}", key.expose()))
        .header("Accept", "application/json")
        .call()
        .map_err(|_| {
            "could not reach OpenRouter to check the key (network error); check the connection, or leave out --check-key".to_owned()
        })?;
    match response.status().as_u16() {
        200 => Ok(()),
        401 | 403 => Err(
            "OpenRouter rejected the key (HTTP 401): check that it was copied whole and has not been revoked or disabled"
                .to_owned(),
        ),
        402 => Err(
            "OpenRouter reports no credit on this account (HTTP 402): add credit at https://openrouter.ai/settings/credits, then run this command again"
                .to_owned(),
        ),
        429 => Err("OpenRouter is rate limiting this key (HTTP 429): try again shortly".to_owned()),
        status => Err(format!(
            "OpenRouter answered the key check with HTTP {status}"
        )),
    }
}

/// `check_key_url` is `Some` for `--check-key`; tests pass a loopback URL.
/// The check runs before the first file or download, so a failure leaves the
/// machine untouched. A dry run uses no network and so no check.
pub fn run<W: Write>(
    opts: &LocalOptions,
    check_key_url: Option<&str>,
    manifest: &Manifest,
    probe: &prereq::Report,
    out: &mut W,
) -> Result<Summary, String> {
    let key = opts.hybrid.as_ref().and_then(|h| h.api_key.as_ref());
    if let (Some(url), Some(key), false) = (check_key_url, key, opts.dry_run) {
        check_key(url, key)?;
        let _ = writeln!(out, "OpenRouter accepted the key.");
    }
    local::run(opts, manifest, probe, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_models_are_valid_distinct_and_never_free() {
        assert!((2..=3).contains(&DEFAULT_MODELS.len()));
        for id in DEFAULT_MODELS {
            validate_model_id(id).unwrap();
            assert!(!id.ends_with(":free"), "{id}");
        }
        let mut seen = DEFAULT_MODELS.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), DEFAULT_MODELS.len());
    }

    #[test]
    fn model_ids_are_vendor_slash_model_with_an_optional_variant() {
        for ok in [
            "google/gemini-2.5-flash-lite",
            "qwen/qwen3-30b-a3b-instruct-2507",
            "perplexity/pplx-decider-v1.3:free",
            "openai/gpt-4o-mini:nitro",
            "meta-llama/llama-3.1-8b-instruct",
        ] {
            validate_model_id(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "",
            "gpt-4o",
            "/model",
            "vendor/",
            "a/b/c",
            "vendor/model:",
            "vendor/model:a:b",
            "vendor/mo del",
            "Vendor/model",
            "vendor/-model",
            "vendor/model\nAIVTUBER_X=1",
            "vendor/model\r",
            "ven dor/model",
            "vendor/model;rm",
            "vendor/mod\u{e9}l",
            &format!("a/{}", "b".repeat(MAX_MODEL_ID_LEN)),
        ] {
            assert!(validate_model_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_model_list_is_bounded_deduplicated_and_defaults_when_empty() {
        let defaults = resolve_models(&[]).unwrap();
        assert_eq!(defaults, DEFAULT_MODELS.to_vec());

        let named: Vec<String> = ["a/one", "b/two", "a/one"].map(String::from).to_vec();
        assert_eq!(resolve_models(&named).unwrap(), vec!["a/one", "b/two"]);

        let eight: Vec<String> = (0..MAX_MODELS).map(|i| format!("v/m{i}")).collect();
        assert_eq!(resolve_models(&eight).unwrap().len(), MAX_MODELS);
        let nine: Vec<String> = (0..=MAX_MODELS).map(|i| format!("v/m{i}")).collect();
        let err = resolve_models(&nine).unwrap_err();
        assert!(err.contains("at most 8"), "{err}");

        let err = resolve_models(&["not-an-id".to_owned()]).unwrap_err();
        assert!(err.contains("vendor/model"), "{err}");
    }

    #[test]
    fn the_key_comes_from_the_environment_first_then_the_prompt_then_guidance() {
        let never = || -> Result<String, String> { panic!("must not prompt") };
        let key = resolve_api_key(Some("sk-or-v1-abcdef123456\n".into()), true, never).unwrap();
        assert_eq!(key.expose(), "sk-or-v1-abcdef123456");

        // An empty variable falls through to the prompt.
        let key = resolve_api_key(Some("  ".into()), true, || {
            Ok("sk-or-v1-typed-key-1\n".into())
        })
        .unwrap();
        assert_eq!(key.expose(), "sk-or-v1-typed-key-1");

        // No terminal and no variable: guidance that names the variable and not argv.
        let err = resolve_api_key(None, false, never).unwrap_err();
        assert!(
            err.contains(KEY_ENV) && err.contains("never accepted"),
            "{err}"
        );

        // A bad value is refused without echoing it.
        let bad = "sk-or-v1-secret\nAIVTUBER_MODE=local";
        let err = resolve_api_key(Some(bad.into()), true, never).unwrap_err();
        assert!(!err.contains("secret"), "{err}");
    }
}
