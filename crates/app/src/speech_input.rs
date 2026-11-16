//! #397: whether the speech sidecar (capture device, VAD, STT) is started.
//!
//! `aivtuber-setup start` names a small state file in
//! `AIVTUBER_SPEECH_INPUT_SETTINGS`; the sidecar then runs only when the file
//! says enabled. Without the variable (the app run directly, existing
//! end-to-end setups) nothing changes: a configured sidecar is started. The
//! file is untrusted local input: every problem reads as off.

use aivtuber_domain::{MAX_SPEECH_INPUT_SETTINGS_BYTES, parse_speech_input_settings};
use std::io::Read;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
