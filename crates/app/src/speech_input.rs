//! #397: whether the speech sidecar (capture device, VAD, STT) is started.
//!
//! `aivtuber-setup start` names a small state file in
//! `AIVTUBER_SPEECH_INPUT_SETTINGS`; the sidecar then runs only when the file
//! says enabled. Without the variable (the app run directly, existing
//! end-to-end setups) nothing changes: a configured sidecar is started. The
//! file is untrusted local input: every problem reads as off.
//!
//! #400: with the variable set ("composed") the operator can also turn voice
//! input on and off while the app runs. [`SpeechInputController`] owns the
//! validated sidecar configuration and the running sidecar, if any; only the
//! sidecar (capture device, VAD, STT worker) is started and stopped. The
//! trackers, conversation route, floor and barge-in are built at start in both
//! states. Enabling and disabling are reachable only through the
//! `speech_input.control` capability (`ProductionApp::configure_speech_input`).

use crate::speech_sidecar::{
    SpeechSidecar, SpeechSidecarConfig, SpeechSidecarState, SpeechSidecarStats,
};
use aivtuber_domain::{
    MAX_SPEECH_INPUT_SETTINGS_BYTES, parse_speech_input_settings, render_speech_input_settings,
};
use aivtuber_runtime::{SpeechObservationTracker, SpeechTranscriptTracker};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const SPEECH_INPUT_SETTINGS_VAR: &str = "AIVTUBER_SPEECH_INPUT_SETTINGS";

/// What start-up does about voice input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechInput {
    /// No `AIVTUBER_SPEECH_SIDECAR`: there is no voice input to decide about.
    NotConfigured,
    /// Configured and the state-file variable is unset: start it, as before.
    Legacy,
    /// Configured and the state file says enabled.
    On,
    /// Configured but the state file is missing, invalid or says disabled.
    Off,
}

impl SpeechInput {
    /// Pure decision. `settings_var_set` is whether the variable names a file;
    /// `file_enabled` is the file's parsed answer (ignored when it is unset).
    pub fn decide(sidecar_configured: bool, settings_var_set: bool, file_enabled: bool) -> Self {
        match (sidecar_configured, settings_var_set, file_enabled) {
            (false, _, _) => Self::NotConfigured,
            (true, false, _) => Self::Legacy,
            (true, true, true) => Self::On,
            (true, true, false) => Self::Off,
        }
    }

    pub fn starts_sidecar(self) -> bool {
        matches!(self, Self::Legacy | Self::On)
    }

    /// Whether start-up prints the on/off line (not in the legacy case, which
    /// must stay as it was).
    pub fn announces(self) -> bool {
        matches!(self, Self::On | Self::Off)
    }
}

/// `true` only when the file is readable, small and exactly an enabled
/// settings document. Every other case, including a missing file, is off.
pub fn read_enabled(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(MAX_SPEECH_INPUT_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && parse_speech_input_settings(&bytes)
}

/// Longest failure reason shown to an operator surface, in characters.
pub const MAX_SPEECH_INPUT_REASON_CHARS: usize = 200;

/// What the operator surfaces show about voice input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechInputStateName {
    Off,
    On,
    Failed,
}

/// The `voice_input` object of the status snapshot (#400).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechInputStatus {
    /// Whether the launcher composed live switching (the settings file is
    /// named). When false the sidecar, if any, was started as before and
    /// cannot be switched; the panel hides the toggle.
    pub composed: bool,
    pub state: SpeechInputStateName,
    /// Why it failed (bounded, control characters removed); only for
    /// `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether the Thinking endpoint leaves this machine, so transcribed
    /// speech would be sent to a cloud model (the loopback decision of the
    /// `thinking:` line).
    pub thinking_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Off,
    On,
    Failed(String),
}

/// A typed refusal from [`SpeechInputController::enable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechInputError {
    /// The speech sidecar did not start (the program is missing, not
    /// executable, or its configuration is out of range). The state is
    /// unchanged and nothing was written.
    Spawn(String),
}

impl fmt::Display for SpeechInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(reason) => write!(f, "voice input could not start: {reason}"),
        }
    }
}

/// Removes control characters and caps the length (reasons come from child
/// exit statuses and OS errors and are shown in the panel).
fn bounded_reason(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_SPEECH_INPUT_REASON_CHARS)
        .collect()
}

/// Owns the speech sidecar and its on/off state (#400). Everything here runs
/// on the composition thread; nothing blocks beyond a child kill and wait.
pub struct SpeechInputController {
    config: SpeechSidecarConfig,
    started: Instant,
    sidecar: Option<SpeechSidecar>,
    state: State,
    /// `Some` only when composed: the state file `aivtuber-setup start` named.
    settings_path: Option<PathBuf>,
    thinking_remote: bool,
}

impl fmt::Debug for SpeechInputController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeechInputController")
            .field("composed", &self.composed())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl SpeechInputController {
    /// `started` is the instant the runtime's `now_ms` counts from;
    /// `settings_path` is `None` for the legacy (not switchable) case.
    pub fn new(
        config: SpeechSidecarConfig,
        started: Instant,
        settings_path: Option<PathBuf>,
        thinking_remote: bool,
    ) -> Self {
        Self {
            config,
            started,
            sidecar: None,
            state: State::Off,
            settings_path,
            thinking_remote,
        }
    }

    pub fn composed(&self) -> bool {
        self.settings_path.is_some()
    }

    pub fn is_on(&self) -> bool {
        self.state == State::On
    }

    pub fn status(&self) -> SpeechInputStatus {
        let (state, reason) = match &self.state {
            State::Off => (SpeechInputStateName::Off, None),
            State::On => (SpeechInputStateName::On, None),
            State::Failed(reason) => (SpeechInputStateName::Failed, Some(reason.clone())),
        };
        SpeechInputStatus {
            composed: self.composed(),
            state,
            reason,
            thinking_remote: self.thinking_remote,
        }
    }

    pub fn stats(&self) -> Option<SpeechSidecarStats> {
        self.sidecar.as_ref().map(SpeechSidecar::stats)
    }

    pub fn feed_heartbeat_ms(&self) -> Option<u64> {
        self.sidecar
            .as_ref()
            .and_then(SpeechSidecar::feed_heartbeat_ms)
    }

    /// Start at app start (the state file said enabled, or legacy). A failure
    /// is the `Failed` state; the file is not touched.
    pub fn start_initial(&mut self) -> Result<(), SpeechInputError> {
        match SpeechSidecar::spawn(self.config.clone(), self.started) {
            Ok(sidecar) => {
                self.sidecar = Some(sidecar);
                self.state = State::On;
                Ok(())
            }
            Err(error) => {
                let reason = spawn_reason(&error);
                self.state = State::Failed(reason.clone());
                Err(SpeechInputError::Spawn(reason))
            }
        }
    }

    /// Poll the sidecar into the trackers (never blocks); a sidecar that
    /// degraded makes the state `Failed` (the file is not rewritten).
    pub fn poll(
        &mut self,
        observations: &mut SpeechObservationTracker,
        transcripts: Option<&mut SpeechTranscriptTracker>,
        now_ms: u64,
    ) {
        let Some(sidecar) = self.sidecar.as_mut() else {
            return;
        };
        sidecar.poll_with_transcripts(observations, transcripts, now_ms, |_, _| {}, |_, _| {});
        if self.state == State::On
            && let SpeechSidecarState::Degraded(reason) = sidecar.state()
        {
            self.state = State::Failed(bounded_reason(reason));
        }
    }

    /// Turn voice input on: spawn a fresh sidecar, and only then write the
    /// state file as enabled. Already on is a no-op. A spawn failure is a
    /// typed error, the state stays as it was and the file is not written.
    /// The `Ok` text says if the choice could not be saved. `announce(on,
    /// thinking_remote)` prints the always-visible voice-input line.
    pub fn enable(
        &mut self,
        announce: impl FnOnce(bool, bool),
    ) -> Result<String, SpeechInputError> {
        if self.state == State::On {
            return Ok("voice input is already on".to_owned());
        }
        // A degraded old instance is gone for good; the new one starts fresh.
        let old = self.sidecar.take();
        match SpeechSidecar::spawn(self.config.clone(), self.started) {
            Ok(sidecar) => {
                drop(old);
                self.sidecar = Some(sidecar);
                self.state = State::On;
            }
            Err(error) => {
                self.sidecar = old;
                return Err(SpeechInputError::Spawn(spawn_reason(&error)));
            }
        }
        let saved = self.write_choice(true);
        announce(true, self.thinking_remote);
        Ok(match saved {
            Ok(()) => "voice input is on".to_owned(),
            Err(note) => format!("voice input is on ({note}; it will be off after a restart)"),
        })
    }

    /// Turn voice input off. Always succeeds: the sidecar (and with it the
    /// STT worker) is stopped and waited for, then the state file is written
    /// as disabled. The `Ok` text says if the choice could not be saved.
    pub fn disable(&mut self, announce: impl FnOnce(bool, bool)) -> String {
        if let Some(mut sidecar) = self.sidecar.take() {
            sidecar.shutdown();
        }
        self.state = State::Off;
        let saved = self.write_choice(false);
        announce(false, self.thinking_remote);
        match saved {
            Ok(()) => "voice input is off".to_owned(),
            Err(note) => {
                format!("voice input is off ({note}; it may be on again after a restart)")
            }
        }
    }

    /// Orderly end of the app: stop the child, change nothing else.
    pub fn shutdown(&mut self) {
        if let Some(mut sidecar) = self.sidecar.take() {
            sidecar.shutdown();
        }
    }

    fn write_choice(&self, enabled: bool) -> Result<(), String> {
        let Some(path) = self.settings_path.as_deref() else {
            return Ok(());
        };
        crate::voice_control::write_atomic(path, &render_speech_input_settings(enabled), false)
            .map_err(|error| {
                eprintln!("speech_input: could not write the settings file: {error}");
                format!("the choice could not be saved ({})", error.kind())
            })
    }
}

/// An OS-level description of why the spawn failed, without the path.
fn spawn_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::InvalidInput => bounded_reason(&error.to_string()),
        kind => bounded_reason(&kind.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The same literals the launcher's tests pin (crates/setup/src/start.rs).
    const ON: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}\n";
    const OFF: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}\n";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aivtuber-speech-input-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("local-speech-input.json")
    }

    #[test]
    fn the_variable_name_is_the_one_the_launcher_sets() {
        // The launcher (crates/setup/src/start.rs) pins the same literal.
        assert_eq!(SPEECH_INPUT_SETTINGS_VAR, "AIVTUBER_SPEECH_INPUT_SETTINGS");
    }

    #[test]
    fn the_decision_table() {
        use SpeechInput::*;
        for var in [false, true] {
            for enabled in [false, true] {
                assert_eq!(SpeechInput::decide(false, var, enabled), NotConfigured);
            }
        }
        // Variable unset: legacy, whatever a stray file would say.
        assert_eq!(SpeechInput::decide(true, false, false), Legacy);
        assert_eq!(SpeechInput::decide(true, false, true), Legacy);
        assert_eq!(SpeechInput::decide(true, true, true), On);
        assert_eq!(SpeechInput::decide(true, true, false), Off);
        assert!(Legacy.starts_sidecar() && On.starts_sidecar());
        assert!(!Off.starts_sidecar() && !NotConfigured.starts_sidecar());
        assert!(On.announces() && Off.announces());
        assert!(!Legacy.announces() && !NotConfigured.announces());
    }

    #[test]
    fn the_file_reader_is_off_for_everything_but_an_enabled_document() {
        let path = scratch("reader");
        let _ = std::fs::remove_file(&path);
        assert!(!read_enabled(&path), "missing file");
        std::fs::write(&path, ON).unwrap();
        assert!(read_enabled(&path));
        std::fs::write(&path, OFF).unwrap();
        assert!(!read_enabled(&path));
        std::fs::write(&path, b"{\"enabled\":true}").unwrap();
        assert!(!read_enabled(&path), "wrong shape");
        std::fs::write(&path, b"\xff\xfe\x00").unwrap();
        assert!(!read_enabled(&path), "not UTF-8");
        let padded = format!("{ON}{}", " ".repeat(600));
        std::fs::write(&path, padded).unwrap();
        assert!(!read_enabled(&path), "oversized");
        std::fs::remove_file(&path).unwrap();
        // A directory where the file should be is unreadable, hence off.
        std::fs::create_dir(&path).unwrap();
        assert!(!read_enabled(&path), "unreadable");
        std::fs::remove_dir(&path).unwrap();
    }
}
