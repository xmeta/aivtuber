//! The small settings file that remembers whether voice input (the microphone,
//! voice-activity detection and speech recognition) is turned on (#397).
//!
//! `aivtuber-setup start --voice-input on|off` writes it beside the env file;
//! the app reads it before it opens any capture device. The file is local,
//! operator-owned data, treated as untrusted input like its siblings: bounded,
//! strictly shaped, and read as "off" for everything except a well-formed
//! `enabled: true`, so a damaged file can never turn the microphone on. This
//! module only parses and renders the document; reading and writing the file
//! is done by the app and the launcher, not by the domain.

use serde::{Deserialize, Serialize};

/// A settings file larger than this is ignored, not read.
pub const MAX_SPEECH_INPUT_SETTINGS_BYTES: u64 = 512;
pub const SPEECH_INPUT_SETTINGS_SCHEMA: &str = "local-speech-input-settings/1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    enabled: bool,
}

/// `true` only for exactly the documented shape with `enabled` true. Oversized,
/// non-UTF-8, malformed, wrong-typed, unknown-field and wrong-schema documents
/// are all `false` (off).
pub fn parse_speech_input_settings(bytes: &[u8]) -> bool {
    if bytes.len() as u64 > MAX_SPEECH_INPUT_SETTINGS_BYTES {
        return false;
    }
    serde_json::from_slice::<SettingsFile>(bytes)
        .is_ok_and(|file| file.schema == SPEECH_INPUT_SETTINGS_SCHEMA && file.enabled)
}

pub fn render_speech_input_settings(enabled: bool) -> String {
    let mut text = serde_json::to_string(&SettingsFile {
        schema: SPEECH_INPUT_SETTINGS_SCHEMA.to_owned(),
        enabled,
    })
    .expect("two plain fields always serialize");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal documents both sides (launcher writes, app reads) agree on.
    pub const ON: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true}\n";
    pub const OFF: &str = "{\"schema\":\"local-speech-input-settings/1\",\"enabled\":false}\n";

    #[test]
    fn the_documented_shape_round_trips_and_is_pinned() {
        assert_eq!(render_speech_input_settings(true), ON);
        assert_eq!(render_speech_input_settings(false), OFF);
        assert!(parse_speech_input_settings(ON.as_bytes()));
        assert!(!parse_speech_input_settings(OFF.as_bytes()));
        // Whitespace and a missing trailing newline are fine.
        assert!(parse_speech_input_settings(
            br#"{ "schema": "local-speech-input-settings/1", "enabled": true }"#
        ));
        assert!((ON.len() as u64) < MAX_SPEECH_INPUT_SETTINGS_BYTES);
    }

    #[test]
    fn everything_but_a_well_formed_enabled_true_is_off() {
        let oversized = format!(
            "{{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true,\"pad\":\"{}\"}}",
            "x".repeat(600)
        );
        let cases: [&[u8]; 17] = [
            b"",
            b"   ",
            b"true",
            b"null",
            b"{}",
            b"{not json",
            b"\xff\xfe{\"enabled\":true}",
            br#"{"enabled":true}"#,
            br#"{"schema":"local-speech-input-settings/2","enabled":true}"#,
            br#"{"schema":"local-voice-settings/1","enabled":true}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":"true"}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":1}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":null}"#,
            br#"{"schema":"local-speech-input-settings/1"}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":true,"device":"x"}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":true}{"x":1}"#,
            oversized.as_bytes(),
        ];
        for case in cases {
            assert!(
                !parse_speech_input_settings(case),
                "{:?}",
                String::from_utf8_lossy(case)
            );
        }
    }
}
