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
//!
//! #396: the same file remembers the capture device and the duplex choice
//! (half: the performer ignores speech that starts during its own playback).
//! Choosing a device restarts the sidecar on it; both choices go through the
//! same capability.

use crate::speech_sidecar::{
    SpeechSidecar, SpeechSidecarConfig, SpeechSidecarState, SpeechSidecarStats,
};
use aivtuber_domain::{
    MAX_SPEECH_INPUT_SETTINGS_BYTES, SpeechDuplex, SpeechInputSettings,
    is_valid_speech_device_name, parse_speech_input_document, render_speech_input_document,
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

/// The remembered settings. Every problem (a missing, unreadable, oversized or
/// malformed file) reads as the default: off, system default device, half
/// duplex.
pub fn read_settings(path: &Path) -> SpeechInputSettings {
    let Ok(file) = std::fs::File::open(path) else {
        return SpeechInputSettings::default();
    };
    let mut bytes = Vec::new();
    match file
        .take(MAX_SPEECH_INPUT_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        Ok(_) => parse_speech_input_document(&bytes),
        Err(_) => SpeechInputSettings::default(),
    }
}

/// `true` only when the file is readable, small and exactly an enabled
/// settings document. Every other case, including a missing file, is off.
pub fn read_enabled(path: &Path) -> bool {
    read_settings(path).enabled
}

/// Longest failure reason shown to an operator surface, in characters.
pub const MAX_SPEECH_INPUT_REASON_CHARS: usize = 400;

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
    /// The capture device the running sidecar opened (#396); absent when it
    /// is not running or has not reported one yet. Operator-local data: shown
    /// in the panel only, never sent to a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The remembered choice (absent is the system default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_device: Option<String>,
    /// Capture devices the sidecar reported at its last start, at most 32.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    /// Whether the performer listens while it speaks (#396).
    #[serde(default = "full_duplex")]
    pub duplex: SpeechDuplex,
    /// Speech segments dropped because they started during the performer's
    /// own playback (half duplex).
    #[serde(default)]
    pub dropped_during_playback: u64,
    /// Live microphone input level, 0..=100 (peak frame RMS, dBFS -60..0), and
    /// the highest speech probability in the same interval (#440). Display
    /// data only: absent (no signal) when voice input is not running or the
    /// sidecar sent nothing for a second. Never authority, never audited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_prob: Option<u8>,
}

fn full_duplex() -> SpeechDuplex {
    SpeechDuplex::Full
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
    /// The requested capture device is not acceptable (a bad name, or one the
    /// sidecar did not list). Nothing changed.
    Device(String),
}

impl fmt::Display for SpeechInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(reason) => write!(f, "voice input could not start: {reason}"),
            Self::Device(reason) => write!(f, "capture device refused: {reason}"),
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
    /// What the file remembers (#396). Written back whole on every change, so
    /// a toggle never loses the device or the duplex choice.
    settings: SpeechInputSettings,
    /// Capture devices of the last report; kept after the sidecar ends so the
    /// panel can still offer them (and a failure can name them).
    known_devices: Vec<String>,
    opened_device: Option<String>,
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
            settings: SpeechInputSettings::default(),
            known_devices: Vec::new(),
            opened_device: None,
        }
    }

    /// The remembered settings read at start (the device and the duplex
    /// choice; `enabled` is tracked by the controller itself).
    pub fn with_settings(mut self, settings: SpeechInputSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Whether the performer listens while it speaks. A legacy (not composed)
    /// run has no settings file and keeps today's behaviour: full duplex.
    pub fn duplex(&self) -> SpeechDuplex {
        if self.composed() {
            self.settings.duplex
        } else {
            SpeechDuplex::Full
        }
    }

    /// The remembered capture device (absent is the system default).
    pub fn selected_device(&self) -> Option<&str> {
        self.settings.device.as_deref()
    }

    /// The sidecar configuration for the next spawn: the configured one with
    /// the remembered capture device (#396) replacing any `--input-device`.
    fn spawn_config(&self) -> SpeechSidecarConfig {
        let mut config = self.config.clone();
        if let Some(device) = &self.settings.device {
            let mut args = Vec::with_capacity(config.args.len() + 2);
            let mut skip = false;
            for arg in &config.args {
                if std::mem::take(&mut skip) {
                    continue;
                }
                if arg == "--input-device" {
                    skip = true;
                    continue;
                }
                args.push(arg.clone());
            }
            args.push("--input-device".to_owned());
            args.push(device.clone());
            config.args = args;
        }
        config
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
        let level = if self.state == State::On {
            let now_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.sidecar
                .as_ref()
                .and_then(|sidecar| sidecar.level_at(now_ms))
        } else {
            None
        };
        SpeechInputStatus {
            composed: self.composed(),
            state,
            reason,
            thinking_remote: self.thinking_remote,
            device: if self.state == State::On {
                self.opened_device.clone()
            } else {
                None
            },
            selected_device: self.settings.device.clone(),
            devices: self.known_devices.clone(),
            duplex: self.duplex(),
            dropped_during_playback: 0,
            level: level.map(|sample| sample.rms),
            speech_prob: level.map(|sample| sample.p),
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
        self.settings.enabled = true;
        match SpeechSidecar::spawn(self.spawn_config(), self.started) {
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
        if let Some(report) = sidecar.device_report()
            && (self.opened_device != report.opened || self.known_devices != report.available)
        {
            self.opened_device = report.opened.clone();
            self.known_devices = report.available.clone();
        }
        if self.state == State::On
            && let SpeechSidecarState::Degraded(reason) = sidecar.state()
        {
            // A named device the sidecar could not open: say which devices
            // exist (it reported them before it exited).
            let reason = match (&self.settings.device, sidecar.device_report()) {
                (Some(wanted), Some(report)) if report.opened.is_none() => {
                    device_not_found_reason(wanted, &report.available)
                }
                _ => bounded_reason(reason),
            };
            self.state = State::Failed(reason);
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
        match SpeechSidecar::spawn(self.spawn_config(), self.started) {
            Ok(sidecar) => {
                drop(old);
                self.opened_device = None;
                self.sidecar = Some(sidecar);
                self.state = State::On;
            }
            Err(error) => {
                self.sidecar = old;
                return Err(SpeechInputError::Spawn(spawn_reason(&error)));
            }
        }
        self.settings.enabled = true;
        let saved = self.write_choice();
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
        self.opened_device = None;
        self.settings.enabled = false;
        let saved = self.write_choice();
        announce(false, self.thinking_remote);
        match saved {
            Ok(()) => "voice input is off".to_owned(),
            Err(note) => {
                format!("voice input is off ({note}; it may be on again after a restart)")
            }
        }
    }

    /// Choose the capture device (#396); `None` is the system default. The
    /// name must be acceptable and, when the sidecar has listed its devices,
    /// one of them (a request cannot make up a device). The choice is saved
    /// whether or not the microphone is on; when it is on (or failed) the
    /// sidecar is stopped and a fresh one is started on the new device, so
    /// two never hold a capture device or the STT model at once. A spawn
    /// failure leaves the state failed and is returned; the choice stays
    /// saved. The `Ok` text is a fixed phrase, never the name.
    pub fn set_device(
        &mut self,
        device: Option<String>,
        announce: impl FnOnce(bool, bool),
    ) -> Result<String, SpeechInputError> {
        if let Some(name) = &device {
            if !is_valid_speech_device_name(name) {
                return Err(SpeechInputError::Device(
                    "the name is empty, too long or has control characters".to_owned(),
                ));
            }
            if !self.known_devices.is_empty() && !self.known_devices.contains(name) {
                return Err(SpeechInputError::Device(
                    "not one of the devices the microphone reported".to_owned(),
                ));
            }
        }
        if device == self.settings.device {
            return Ok("capture device unchanged".to_owned());
        }
        self.settings.device = device;
        let running = self.settings.enabled && self.state != State::Off;
        let mut spawned = Ok(());
        if running {
            if let Some(mut old) = self.sidecar.take() {
                old.shutdown();
            }
            self.opened_device = None;
            match SpeechSidecar::spawn(self.spawn_config(), self.started) {
                Ok(sidecar) => {
                    self.sidecar = Some(sidecar);
                    self.state = State::On;
                    announce(true, self.thinking_remote);
                }
                Err(error) => {
                    let reason = spawn_reason(&error);
                    self.state = State::Failed(reason.clone());
                    spawned = Err(SpeechInputError::Spawn(reason));
                }
            }
        }
        let saved = self.write_choice();
        spawned?;
        let what = if self.settings.device.is_some() {
            "capture device changed"
        } else {
            "capture device is the system default"
        };
        Ok(match saved {
            Ok(()) => what.to_owned(),
            Err(note) => format!("{what} ({note}; it will be forgotten after a restart)"),
        })
    }

    /// Choose whether the performer listens while it speaks (#396). Applies at
    /// once (the sidecar is untouched) and is saved.
    pub fn set_duplex(&mut self, duplex: SpeechDuplex) -> String {
        self.settings.duplex = duplex;
        let saved = self.write_choice();
        let what = match duplex {
            SpeechDuplex::Half => "half duplex: speech during playback is ignored",
            SpeechDuplex::Full => "full duplex: speech during playback is heard",
        };
        match saved {
            Ok(()) => what.to_owned(),
            Err(note) => format!("{what} ({note}; it will be forgotten after a restart)"),
        }
    }

    /// Orderly end of the app: stop the child, change nothing else.
    pub fn shutdown(&mut self) {
        if let Some(mut sidecar) = self.sidecar.take() {
            sidecar.shutdown();
        }
    }

    fn write_choice(&self) -> Result<(), String> {
        let Some(path) = self.settings_path.as_deref() else {
            return Ok(());
        };
        crate::voice_control::write_atomic(
            path,
            &render_speech_input_document(&self.settings),
            false,
        )
        .map_err(|error| {
            eprintln!("speech_input: could not write the settings file: {error}");
            format!("the choice could not be saved ({})", error.kind())
        })
    }
}

/// Why a named capture device could not be opened, with the ones that exist
/// (bilingual; the names are cut so the reason stays short).
fn device_not_found_reason(wanted: &str, available: &[String]) -> String {
    let cut = |name: &str| -> String { name.chars().take(40).collect() };
    let mut listed: Vec<String> = available.iter().take(4).map(|n| cut(n)).collect();
    if available.len() > 4 {
        listed.push("…".to_owned());
    }
    let listed = listed.join(", ");
    let wanted = cut(wanted);
    bounded_reason(&format!(
        "入力デバイス「{wanted}」が見つかりません。利用できるデバイス: {listed} / input device \"{wanted}\" not found; available: {listed}"
    ))
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
        let dir =
            crate::test_dir::scratch_for_test_thread(&format!("aivtuber-speech-input-{name}"));
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

    #[test]
    fn the_settings_reader_returns_device_and_duplex_and_defaults_on_any_damage() {
        let path = scratch("settings");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_settings(&path), SpeechInputSettings::default());
        std::fs::write(
            &path,
            "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true,\"device\":\"USB Mic\",\"duplex\":\"full\"}\n",
        )
        .unwrap();
        let settings = read_settings(&path);
        assert!(settings.enabled);
        assert_eq!(settings.device.as_deref(), Some("USB Mic"));
        assert_eq!(settings.duplex, SpeechDuplex::Full);
        std::fs::write(&path, b"{\"nonsense\":1}").unwrap();
        let damaged = read_settings(&path);
        assert!(!damaged.enabled && damaged.device.is_none());
        assert_eq!(damaged.duplex, SpeechDuplex::Half);
        std::fs::remove_file(&path).unwrap();
    }

    fn controller(args: &[&str], path: Option<PathBuf>) -> SpeechInputController {
        SpeechInputController::new(
            SpeechSidecarConfig::new(
                "no-such-sidecar-program",
                args.iter().map(|a| (*a).to_owned()).collect(),
            ),
            Instant::now(),
            path,
            false,
        )
    }

    #[test]
    fn the_remembered_device_replaces_any_configured_input_device() {
        let mut input = controller(&["--language", "ja", "--input-device", "Old", "--x"], None);
        assert_eq!(
            input.spawn_config().args,
            ["--language", "ja", "--input-device", "Old", "--x"],
            "no choice: the configuration is untouched"
        );
        input.settings.device = Some("New Mic".to_owned());
        assert_eq!(
            input.spawn_config().args,
            ["--language", "ja", "--x", "--input-device", "New Mic"]
        );
        // The name is one argument, never split or interpreted.
        input.settings.device = Some("a b --evil".to_owned());
        let args = input.spawn_config().args;
        assert_eq!(&args[args.len() - 2..], ["--input-device", "a b --evil"]);
    }

    #[test]
    fn a_legacy_controller_is_full_duplex_and_a_composed_one_follows_its_file() {
        let legacy = controller(&[], None);
        assert_eq!(legacy.duplex(), SpeechDuplex::Full);
        let composed = controller(&[], Some(scratch("duplex")));
        assert_eq!(composed.duplex(), SpeechDuplex::Half, "the default");
        let composed = composed.with_settings(SpeechInputSettings {
            duplex: SpeechDuplex::Full,
            ..SpeechInputSettings::default()
        });
        assert_eq!(composed.duplex(), SpeechDuplex::Full);
    }

    #[test]
    fn choices_are_saved_whole_so_a_toggle_never_loses_the_others() {
        let path = scratch("whole");
        let _ = std::fs::remove_file(&path);
        let mut input = controller(&[], Some(path.clone())).with_settings(SpeechInputSettings {
            enabled: false,
            device: None,
            duplex: SpeechDuplex::Half,
        });
        // Off: only remembered. An unknown (never listed) name is accepted
        // while nothing is known; a bad one is refused.
        assert!(
            input
                .set_device(Some("USB Mic".to_owned()), |_, _| {})
                .is_ok()
        );
        input.set_duplex(SpeechDuplex::Full);
        let saved = read_settings(&path);
        assert_eq!(saved.device.as_deref(), Some("USB Mic"));
        assert_eq!(saved.duplex, SpeechDuplex::Full);
        assert!(!saved.enabled, "choosing a device does not turn it on");
        for bad in ["", "a\nb"] {
            assert!(matches!(
                input.set_device(Some(bad.to_owned()), |_, _| {}),
                Err(SpeechInputError::Device(_))
            ));
        }
        // The next disable keeps them.
        input.disable(|_, _| {});
        let saved = read_settings(&path);
        assert_eq!(saved.device.as_deref(), Some("USB Mic"));
        assert_eq!(saved.duplex, SpeechDuplex::Full);
        // Back to the system default.
        assert!(input.set_device(None, |_, _| {}).is_ok());
        assert_eq!(read_settings(&path).device, None);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_level_is_display_data_and_absent_from_json_when_unknown() {
        let mut status = controller(&[], None).status();
        assert_eq!((status.level, status.speech_prob), (None, None));
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("level").is_none() && json.get("speech_prob").is_none());
        status.level = Some(42);
        status.speech_prob = Some(97);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            (json["level"].as_u64(), json["speech_prob"].as_u64()),
            (Some(42), Some(97))
        );
        // An older document without them still reads.
        let mut old = json;
        old.as_object_mut().unwrap().remove("level");
        old.as_object_mut().unwrap().remove("speech_prob");
        let back: SpeechInputStatus = serde_json::from_value(old).unwrap();
        assert_eq!(back.level, None);
    }

    #[test]
    fn a_listed_device_is_required_once_the_sidecar_reported_its_devices() {
        let mut input = controller(&[], Some(scratch("listed")));
        input.known_devices = vec!["Mic A".to_owned(), "Mic B".to_owned()];
        assert!(matches!(
            input.set_device(Some("Mic C".to_owned()), |_, _| {}),
            Err(SpeechInputError::Device(_))
        ));
        assert_eq!(input.selected_device(), None, "nothing changed");
        assert!(
            input
                .set_device(Some("Mic B".to_owned()), |_, _| {})
                .is_ok()
        );
        assert_eq!(input.selected_device(), Some("Mic B"));
        // The status shows the names and the choice.
        let status = input.status();
        assert_eq!(status.devices, ["Mic A", "Mic B"]);
        assert_eq!(status.selected_device.as_deref(), Some("Mic B"));
        assert_eq!(status.device, None, "nothing is open");
    }

    #[test]
    fn the_not_found_reason_names_the_devices_in_both_languages_and_is_bounded() {
        let many: Vec<String> = (0..10).map(|i| format!("device number {i}")).collect();
        let reason = device_not_found_reason("Missing Mic", &many);
        assert!(reason.contains("Missing Mic") && reason.contains("device number 0"));
        assert!(reason.contains("見つかりません") && reason.contains("not found"));
        assert!(reason.contains('…'), "more than four are elided");
        assert!(!reason.contains("device number 9"));
        assert!(reason.chars().count() <= MAX_SPEECH_INPUT_REASON_CHARS);
    }
}
