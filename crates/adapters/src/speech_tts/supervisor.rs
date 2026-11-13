//! Supervisor of the `local-tts-sidecar` child, owned by the app (#220 S2b).
//!
//! Same shape as the STT worker supervisor, adapted to a server: the app spawns
//! the sidecar as a separate process so that a crash, an out-of-memory kill or
//! a hang in a 100 M-parameter model cannot take the deterministic performer
//! with it. The sidecar announces readiness on stdout once its model is loaded
//! and the socket is listening; until then (and after any failure) TTS requests
//! simply fail with `Unavailable` and the generative pipeline degrades to the
//! typed non-verbal fallback, so the app never waits on this child after
//! start-up.
//!
//! Policy, as for STT: the first failure is final and there is no automatic
//! restart (a restart is an operator decision); the child gets no `AIVTUBER_*`
//! environment; the engine libraries beside the program are checked against
//! their SHA-256 pins *before* the program is spawned; and the child exits by
//! itself when the app goes away (its stdin, held open here, reaches EOF).

use crate::speech_stt::pins::verify_engine_libs;
use aivtuber_domain::{
    EngineError, EngineErrorKind, EngineFuture, SpeechArtifact, SpeechRequest, TtsBackendIdentity,
    TtsEngine,
};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_LINE_BYTES: u64 = 16 * 1024;
const MAX_ARGS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TtsReady {
    pub listen: SocketAddr,
    pub model_id: String,
    pub model_version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyLine {
    tts: ReadyBody,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ReadyBody {
    Ready(TtsReady),
}

/// The one line the sidecar prints on stdout when it is ready:
/// `{"tts":{"state":"ready","listen":"127.0.0.1:8770","model_id":"...","model_version":"..."}}`.
pub fn encode_ready_line(ready: &TtsReady) -> String {
    let mut line = serde_json::to_string(&ReadyLine {
        tts: ReadyBody::Ready(ready.clone()),
    })
    .expect("ready line is serializable");
    line.push('\n');
    line
}

pub fn parse_ready_line(line: &str) -> Option<TtsReady> {
    match serde_json::from_str::<ReadyLine>(line.trim()).ok()?.tts {
        ReadyBody::Ready(ready) => Some(ready),
    }
}

#[derive(Debug, Clone)]
pub struct TtsSidecarConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// How long the model may take to load and the socket to open. A cold
    /// first load measured 6.6 s, a warm one 0.9 s (EXP-0003 section 7).
    pub ready_timeout: Duration,
    /// Check the engine library pins beside the program before spawning it.
    /// On in production; a test double has no such libraries.
    pub verify_engine_libs: bool,
}

impl TtsSidecarConfig {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            ready_timeout: Duration::from_secs(60),
            verify_engine_libs: true,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.program.as_os_str().is_empty() {
            return Err("tts sidecar program is empty".to_owned());
        }
        if self.args.len() > MAX_ARGS {
            return Err(format!("tts sidecar takes at most {MAX_ARGS} arguments"));
        }
        if self.ready_timeout.is_zero() || self.ready_timeout > Duration::from_secs(600) {
            return Err("tts sidecar ready timeout must be 1 ms to 600 s".to_owned());
        }
        Ok(())
    }
}

/// A running, ready sidecar. Dropping it ends the child.
pub struct TtsSidecar {
    child: Child,
    // Held open: its EOF is how the child learns the app is gone.
    _stdin: ChildStdin,
    ready: TtsReady,
    output_open: Arc<AtomicBool>,
}

impl TtsSidecar {
    /// Spawn the sidecar and wait for its ready line. Every failure is a
    /// message for the operator; the caller degrades and carries on.
    pub fn spawn(config: &TtsSidecarConfig) -> Result<Self, String> {
        config.validate()?;
        // The libraries are loaded from the directory of the executable that
        // runs, never from PATH or LD_LIBRARY_PATH (see build.rs), so the
        // verified directory and the executed file must be one and the same.
        let mut program = config.program.clone();
        if config.verify_engine_libs {
            let resolved = std::fs::canonicalize(&config.program).map_err(|error| {
                format!(
                    "tts sidecar {} (give a path, not a name looked up on PATH): {error}",
                    config.program.display()
                )
            })?;
            let dir = resolved
                .parent()
                .ok_or_else(|| "tts sidecar has no parent directory".to_owned())?;
            verify_engine_libs(dir).map_err(|error| format!("tts engine libraries: {error}"))?;
            program = resolved;
        }
        let mut command = Command::new(&program);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // The sidecar has no use for the app's credentials or configuration.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AIVTUBER_") {
                command.env_remove(key);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("tts sidecar {}: {error}", config.program.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let output_open = Arc::new(AtomicBool::new(true));
        let (sender, lines) = mpsc::channel::<TtsReady>();
        {
            let open = output_open.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.by_ref().take(MAX_LINE_BYTES).read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Some(ready) = parse_ready_line(&line) {
                                let _ = sender.send(ready);
                            }
                        }
                    }
                }
                open.store(false, Ordering::Release);
            });
        }

        let started = Instant::now();
        let mut sidecar = Self {
            child,
            _stdin: stdin,
            ready: TtsReady {
                listen: ([127, 0, 0, 1], 0).into(),
                model_id: String::new(),
                model_version: String::new(),
            },
            output_open,
        };
        loop {
            match lines.recv_timeout(Duration::from_millis(50)) {
                Ok(ready) => {
                    sidecar.ready = ready;
                    return Ok(sidecar);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(sidecar.failure("exited before it was ready"));
                }
            }
            if let Ok(Some(status)) = sidecar.child.try_wait() {
                return Err(format!("tts sidecar exited before it was ready ({status})"));
            }
            if started.elapsed() >= config.ready_timeout {
                return Err(sidecar.failure("was not ready in time"));
            }
        }
    }

    fn failure(&mut self, what: &str) -> String {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        format!("tts sidecar {what} ({status:?})")
    }

    pub fn ready(&self) -> &TtsReady {
        &self.ready
    }

    /// Whether the child is still running. The app polls this to report a
    /// mid-run failure once; requests degrade on their own regardless.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// False once the child closed its stdout (it has exited or is exiting).
    pub fn output_open(&self) -> bool {
        self.output_open.load(Ordering::Acquire)
    }

    /// Shared flag behind [`Self::output_open`], for [`LivenessGatedTts`].
    pub fn liveness(&self) -> Arc<AtomicBool> {
        self.output_open.clone()
    }
}

/// A [`TtsEngine`] that answers `Unavailable` at once while the supervised
/// sidecar is known to be gone, and otherwise delegates unchanged. A dead
/// loopback port is not always refused quickly (a connect to a closed port
/// takes about two seconds on Windows), and the generative worker would pay
/// that on every reply; this turns it into an immediate typed fallback.
pub struct LivenessGatedTts {
    inner: Arc<dyn TtsEngine>,
    alive: Arc<AtomicBool>,
    /// While set the gate is open whatever the sidecar is doing: the operator
    /// pointed the voice at another backend (Aivis Cloud, #360), so the
    /// sidecar's state is irrelevant.
    bypass: Arc<AtomicBool>,
}

impl LivenessGatedTts {
    pub fn new(inner: Arc<dyn TtsEngine>, alive: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            alive,
            bypass: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Use `bypass` as the flag that opens the gate.
    pub fn with_bypass(mut self, bypass: Arc<AtomicBool>) -> Self {
        self.bypass = bypass;
        self
    }

    fn is_gone(&self) -> bool {
        !self.bypass.load(Ordering::Acquire) && !self.alive.load(Ordering::Acquire)
    }

    fn gone() -> EngineError {
        EngineError::new(
            EngineErrorKind::Unavailable,
            "the supervised TTS sidecar is not running",
        )
    }
}

impl TtsEngine for LivenessGatedTts {
    fn synthesize<'a>(&'a self, request: &'a SpeechRequest) -> EngineFuture<'a, SpeechArtifact> {
        if self.is_gone() {
            return Box::pin(async { Err(Self::gone()) });
        }
        self.inner.synthesize(request)
    }

    fn synthesize_with_timeout<'a>(
        &'a self,
        request: &'a SpeechRequest,
        timeout: Duration,
    ) -> EngineFuture<'a, SpeechArtifact> {
        if self.is_gone() {
            return Box::pin(async { Err(Self::gone()) });
        }
        self.inner.synthesize_with_timeout(request, timeout)
    }

    fn identity(&self) -> TtsBackendIdentity {
        self.inner.identity()
    }
}

impl Drop for TtsSidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ready_line_round_trips_and_nothing_else_parses() {
        let ready = TtsReady {
            listen: "127.0.0.1:8770".parse().unwrap(),
            model_id: "supertonic-3-int8".to_owned(),
            model_version: "ab12".to_owned(),
        };
        let line = encode_ready_line(&ready);
        assert!(line.ends_with('\n'));
        assert_eq!(parse_ready_line(&line), Some(ready));
        for bad in [
            r#"{"tts":{"state":"loading"}}"#,
            r#"{"tts":{"state":"ready","listen":"not an address","model_id":"a","model_version":"b"}}"#,
            r#"{"tts":{"state":"ready","listen":"127.0.0.1:1","model_id":"a","model_version":"b","x":1}}"#,
            r#"{"stt":{"state":"ready"}}"#,
            "not json",
            "",
        ] {
            assert_eq!(parse_ready_line(bad), None, "{bad}");
        }
    }

    #[test]
    fn config_bounds() {
        assert!(TtsSidecarConfig::new("x", vec![]).validate().is_ok());
        assert!(TtsSidecarConfig::new("", vec![]).validate().is_err());
        assert!(
            TtsSidecarConfig::new("x", vec!["a".into(); MAX_ARGS + 1])
                .validate()
                .is_err()
        );
        let mut config = TtsSidecarConfig::new("x", vec![]);
        config.ready_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_unpinned_or_missing_program_is_refused_before_it_is_spawned() {
        // A program beside no engine libraries: the pin check fails, and the
        // file is never executed.
        let dir = std::env::temp_dir().join(format!("aivtuber-tts-sup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("local-tts-sidecar");
        std::fs::write(&program, b"not executable").unwrap();
        let error = TtsSidecar::spawn(&TtsSidecarConfig::new(&program, vec![]))
            .err()
            .expect("no pinned libraries beside the program");
        assert!(error.contains("engine libraries"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
        let mut missing = TtsSidecarConfig::new("definitely-not-a-program", vec![]);
        missing.verify_engine_libs = false;
        assert!(TtsSidecar::spawn(&missing).is_err());
    }
}
