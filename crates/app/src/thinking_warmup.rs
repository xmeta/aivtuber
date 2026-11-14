//! App-owned Thinking warm-up (issue #333).
//!
//! A cold local llama-server needs far longer for its first prompt than the
//! normal Thinking timeout, so without a warm-up the first chat line always
//! falls back. At startup the app sends one internal completion built by the
//! adapter's own request builder (see `OpenAiResponsesAdapter::warm_up`) on a
//! background thread.
//!
//! Boundaries: the thread never touches the performer, the frame/motor loop,
//! chat ingress or operator control; it holds no queue and no history. The
//! output is discarded inside the adapter (only a token count comes back), so
//! it is never spoken, shown, remembered or treated as authority. A failure or
//! timeout is logged and ignored. Startup does not wait for it.

use aivtuber_adapters::OpenAiResponsesAdapter;
use aivtuber_domain::EngineErrorKind;
use std::thread::JoinHandle;
use std::time::Duration;

pub const DEFAULT_WARMUP_TIMEOUT_MS: u64 = 120_000;
pub const MIN_WARMUP_TIMEOUT_MS: u64 = 1_000;
pub const MAX_WARMUP_TIMEOUT_MS: u64 = 600_000;

/// The backend name that identifies a local llama-server (set by
/// `aivtuber-setup`; see `AIVTUBER_OPENAI_BACKEND_NAME`).
const LLAMA_SERVER_BACKEND: &str = "llama-server";
const UNAUTHENTICATED_LOOPBACK: &str = "unauthenticated-loopback";

/// `AIVTUBER_THINKING_WARMUP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupMode {
    /// On only for the loopback llama-server backend (the local profile).
    Auto,
    On,
    Off,
}

impl WarmupMode {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("auto") => Ok(Self::Auto),
            Some("on" | "true" | "1" | "yes") => Ok(Self::On),
            Some("off" | "false" | "0" | "no") => Ok(Self::Off),
            Some(_) => Err("AIVTUBER_THINKING_WARMUP must be auto, on or off".to_owned()),
        }
    }
}

/// `AIVTUBER_THINKING_WARMUP_TIMEOUT_MS`, bounded.
pub fn warmup_timeout(ms: u64) -> Result<Duration, String> {
    if !(MIN_WARMUP_TIMEOUT_MS..=MAX_WARMUP_TIMEOUT_MS).contains(&ms) {
        return Err(format!(
            "AIVTUBER_THINKING_WARMUP_TIMEOUT_MS must be {MIN_WARMUP_TIMEOUT_MS}..={MAX_WARMUP_TIMEOUT_MS}"
        ));
    }
    Ok(Duration::from_millis(ms))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupDecision {
    Run,
    Skip(&'static str),
}

/// `auth_label` is `ThinkingAuthentication::label`; `unauthenticated-loopback`
/// exists only for an endpoint the adapter verified as loopback, so it also
/// means "no paid or remote endpoint".
pub fn decide(mode: WarmupMode, auth_label: &str, backend_name: &str) -> WarmupDecision {
    match mode {
        WarmupMode::Off => WarmupDecision::Skip("disabled"),
        WarmupMode::On => WarmupDecision::Run,
        WarmupMode::Auto
            if auth_label == UNAUTHENTICATED_LOOPBACK && backend_name == LLAMA_SERVER_BACKEND =>
        {
            WarmupDecision::Run
        }
        WarmupMode::Auto => WarmupDecision::Skip("not_local_llama_server"),
    }
}

fn reason(kind: EngineErrorKind) -> &'static str {
    match kind {
        EngineErrorKind::Timeout => "timeout",
        EngineErrorKind::Unavailable => "unavailable",
        EngineErrorKind::RateLimited => "rate_limited",
        EngineErrorKind::Overloaded => "overloaded",
        EngineErrorKind::QuotaExhausted => "quota_exhausted",
        EngineErrorKind::Authentication | EngineErrorKind::Unauthorized => "authentication",
        EngineErrorKind::InvalidRequest => "invalid_request",
        EngineErrorKind::Backend => "backend",
    }
}

/// Run one warm-up to completion on the calling thread, reporting through
/// `log`. Never panics on provider failure and returns nothing but whether it
/// succeeded; no reply text exists at this layer.
pub fn run(adapter: &OpenAiResponsesAdapter, timeout: Duration, log: &dyn Fn(&str)) -> bool {
    log(&format!(
        "thinking_warmup: started timeout_ms={}",
        timeout.as_millis()
    ));
    match adapter.warm_up(timeout) {
        Ok(report) => {
            let tokens = report
                .prompt_tokens
                .map(|n| format!(" prompt_tokens={n}"))
                .unwrap_or_default();
            log(&format!(
                "thinking_warmup: ok ms={}{tokens}",
                report.elapsed.as_millis()
            ));
            true
        }
        Err(error) => {
            log(&format!(
                "thinking_warmup: failed reason={}",
                reason(error.kind)
            ));
            false
        }
    }
}

/// Apply the decision and, when it is `Run`, start the warm-up on its own
/// thread. A thread that cannot start is a logged skip, not a startup error.
pub fn start(
    mode: WarmupMode,
    adapter: &OpenAiResponsesAdapter,
    backend_name: &str,
    timeout: Duration,
) -> Option<JoinHandle<()>> {
    match decide(mode, adapter.authentication_label(), backend_name) {
        WarmupDecision::Skip(why) => {
            eprintln!("thinking_warmup: skipped reason={why}");
            None
        }
        WarmupDecision::Run => {
            let adapter = adapter.clone();
            let spawned = std::thread::Builder::new()
                .name("thinking-warmup".to_owned())
                .spawn(move || {
                    run(&adapter, timeout, &|line| eprintln!("{line}"));
                });
            match spawned {
                Ok(handle) => Some(handle),
                Err(_) => {
                    eprintln!("thinking_warmup: skipped reason=thread_unavailable");
                    None
                }
            }
        }
    }
}
