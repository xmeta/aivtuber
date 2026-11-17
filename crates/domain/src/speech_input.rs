//! The small settings file that remembers whether voice input (the microphone,
//! voice-activity detection and speech recognition) is turned on (#397), which
//! capture device it uses and whether the performer listens while it speaks
//! (#396).
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
/// Longest capture-device name, in bytes (#396). The sidecar's device report
/// is bounded by the same limit.
pub const MAX_SPEECH_DEVICE_NAME_BYTES: usize = 128;
/// Most capture-device names one sidecar report carries (#396).
pub const MAX_SPEECH_DEVICES: usize = 32;

/// Whether the performer listens while its own audio plays (#396). `Half` is
/// the default: with speakers the performer must not hear itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechDuplex {
    /// A speech segment that starts while the performer's audio plays (or in
    /// the short tail after it) is dropped, and voice barge-in is off.
    #[default]
    Half,
    /// Speech during playback is heard and barge-in is on when configured.
    /// For headset users.
    Full,
}

impl SpeechDuplex {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Half => "half",
            Self::Full => "full",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "half" => Some(Self::Half),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// What the file remembers about voice input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpeechInputSettings {
    pub enabled: bool,
    /// Capture device name; `None` is the system default.
    pub device: Option<String>,
    pub duplex: SpeechDuplex,
}

/// A capture-device name the app accepts: 1 to 128 bytes, no control
/// characters (it becomes one argument of the sidecar and is shown in the
/// panel; it is never sent to a model).
pub fn is_valid_speech_device_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SPEECH_DEVICE_NAME_BYTES
        && !name.chars().any(char::is_control)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    #[serde(default, skip_serializing_if = "is_half")]
    duplex: SpeechDuplex,
}

fn is_half(duplex: &SpeechDuplex) -> bool {
    *duplex == SpeechDuplex::Half
}

/// The remembered settings. Oversized, non-UTF-8, malformed, wrong-typed,
/// unknown-field, wrong-schema and invalid-device documents all read as the
/// default: off, system default device, half duplex.
pub fn parse_speech_input_document(bytes: &[u8]) -> SpeechInputSettings {
    if bytes.len() as u64 > MAX_SPEECH_INPUT_SETTINGS_BYTES {
        return SpeechInputSettings::default();
    }
    match serde_json::from_slice::<SettingsFile>(bytes) {
        Ok(file)
            if file.schema == SPEECH_INPUT_SETTINGS_SCHEMA
                && file
                    .device
                    .as_deref()
                    .is_none_or(is_valid_speech_device_name) =>
        {
            SpeechInputSettings {
                enabled: file.enabled,
                device: file.device,
                duplex: file.duplex,
            }
        }
        _ => SpeechInputSettings::default(),
    }
}

/// `true` only for exactly the documented shape with `enabled` true.
pub fn parse_speech_input_settings(bytes: &[u8]) -> bool {
    parse_speech_input_document(bytes).enabled
}

/// The document for `settings`. An invalid device name is not written.
pub fn render_speech_input_document(settings: &SpeechInputSettings) -> String {
    let mut text = serde_json::to_string(&SettingsFile {
        schema: SPEECH_INPUT_SETTINGS_SCHEMA.to_owned(),
        enabled: settings.enabled,
        device: settings
            .device
            .clone()
            .filter(|name| is_valid_speech_device_name(name)),
        duplex: settings.duplex,
    })
    .expect("plain fields always serialize");
    text.push('\n');
    text
}

pub fn render_speech_input_settings(enabled: bool) -> String {
    render_speech_input_document(&SpeechInputSettings {
        enabled,
        ..SpeechInputSettings::default()
    })
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
        let cases: [&[u8]; 20] = [
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
            br#"{"schema":"local-speech-input-settings/1","enabled":true,"device":""}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":true,"device":"a\nb"}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":true,"duplex":"both"}"#,
            br#"{"schema":"local-speech-input-settings/1","enabled":true,"extra":1}"#,
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

    #[test]
    fn device_and_duplex_round_trip_and_the_default_document_is_unchanged() {
        let settings = SpeechInputSettings {
            enabled: true,
            device: Some("Line (YAMAHA AG06/AG03)".to_owned()),
            duplex: SpeechDuplex::Full,
        };
        let text = render_speech_input_document(&settings);
        assert_eq!(parse_speech_input_document(text.as_bytes()), settings);
        // Absent fields: system default device, half duplex.
        let parsed = parse_speech_input_document(ON.as_bytes());
        assert_eq!(parsed.device, None);
        assert_eq!(parsed.duplex, SpeechDuplex::Half);
        // The default document stays byte-identical to what #397 wrote.
        assert_eq!(
            render_speech_input_document(&SpeechInputSettings {
                enabled: true,
                ..SpeechInputSettings::default()
            }),
            ON
        );
        // The longest name made only of escaped characters still fits the file
        // bound, so a valid choice can always be written.
        let worst = SpeechInputSettings {
            enabled: true,
            device: Some("\"".repeat(MAX_SPEECH_DEVICE_NAME_BYTES)),
            duplex: SpeechDuplex::Full,
        };
        let text = render_speech_input_document(&worst);
        assert!(
            text.len() as u64 <= MAX_SPEECH_INPUT_SETTINGS_BYTES,
            "{}",
            text.len()
        );
        assert_eq!(parse_speech_input_document(text.as_bytes()), worst);
    }

    #[test]
    fn a_bad_device_reads_as_the_default_settings() {
        assert!(!is_valid_speech_device_name(""));
        assert!(!is_valid_speech_device_name("a\u{0}b"));
        let long = "x".repeat(MAX_SPEECH_DEVICE_NAME_BYTES + 1);
        assert!(!is_valid_speech_device_name(&long));
        assert!(is_valid_speech_device_name("マイク (USB Audio)"));
        let doc = format!(
            "{{\"schema\":\"local-speech-input-settings/1\",\"enabled\":true,\"device\":\"{long}\"}}"
        );
        assert_eq!(
            parse_speech_input_document(doc.as_bytes()),
            SpeechInputSettings::default()
        );
    }
}
