//! The operator's voice choice and the small settings file that remembers it
//! (issue #360).
//!
//! Two voices exist: the local one (the supervised sidecar, nothing leaves the
//! machine) and the Aivis Cloud one (reply text goes to an internet service).
//! This module is the one definition of what a valid choice is, shared by the
//! app (applies it live, writes the file), the panel's HTTP layer and
//! `aivtuberctl`. Everything here is bounded, typed data: a model reference is
//! a UUID, a style an integer, tuning four floats inside the API's bounds and a
//! credit line short plain text. The API key is **not** part of it: it lives in
//! its own file and never in a settings document, a patch, a status or an audit
//! record.
//!
//! This module only parses, validates and renders; reading and writing files is
//! the app's job.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

pub const VOICE_SETTINGS_SCHEMA: &str = "local-voice-settings/1";
/// A settings file larger than this is ignored, not read.
pub const MAX_VOICE_SETTINGS_BYTES: u64 = 4096;
/// A patch (the `voice.apply` argument) larger than this is refused.
pub const MAX_VOICE_PATCH_BYTES: usize = 2048;
pub const MAX_VOICE_CREDIT_CHARS: usize = 200;
pub const MAX_VOICE_MODEL_NAME_CHARS: usize = 80;
/// Highest style id the Aivis Cloud API accepts.
pub const MAX_VOICE_STYLE_ID: u8 = 31;

pub const SPEAKING_RATE_RANGE: (f32, f32) = (0.5, 2.0);
pub const PITCH_RANGE: (f32, f32) = (-1.0, 1.0);
pub const VOLUME_RANGE: (f32, f32) = (0.0, 2.0);
pub const EMOTIONAL_INTENSITY_RANGE: (f32, f32) = (0.0, 2.0);

/// The fixed sentence the preview speaks. There is no free-text preview.
pub const VOICE_PREVIEW_TEXT: &str = "こんにちは、声の試し読みです。この声で配信をお届けしますね。";

/// `8-4-4-4-12` hex, any case.
pub fn is_voice_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceBackend {
    /// The local voice (current default). Nothing leaves the machine.
    Local,
    /// Aivis Cloud: reply text is sent to an internet service.
    Aivis,
}

impl VoiceBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Aivis => "aivis",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceSettingsError {
    PatchTooLarge,
    Malformed,
    ModelUuid,
    SpeakerUuid,
    StyleId,
    Tuning(&'static str),
    Credit(&'static str),
    ModelName,
    /// Aivis needs a model before it can be enabled.
    MissingModel,
    /// Aivis needs the credit line before it can be enabled.
    MissingCredit,
    /// Switching to Aivis sends reply text to an internet service: the patch
    /// must carry the explicit confirmation.
    EgressNotConfirmed,
}

impl fmt::Display for VoiceSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PatchTooLarge => write!(f, "the request exceeds {MAX_VOICE_PATCH_BYTES} bytes"),
            Self::Malformed => f.write_str("not a voice settings request"),
            Self::ModelUuid => f.write_str("the model must be a UUID"),
            Self::SpeakerUuid => f.write_str("the speaker must be a UUID"),
            Self::StyleId => write!(f, "the style id must be 0..={MAX_VOICE_STYLE_ID}"),
            Self::Tuning(name) => write!(f, "{name} is out of range"),
            Self::Credit(reason) => write!(f, "the credit line {reason}"),
            Self::ModelName => write!(
                f,
                "the model name is at most {MAX_VOICE_MODEL_NAME_CHARS} characters without control characters"
            ),
            Self::MissingModel => f.write_str("choose a model before enabling Aivis Cloud"),
            Self::MissingCredit => {
                f.write_str("enter the credit line the model requires before enabling Aivis Cloud")
            }
            Self::EgressNotConfirmed => f.write_str(
                "confirm that reply text is sent to Aivis Cloud (an internet service) to enable it",
            ),
        }
    }
}

impl std::error::Error for VoiceSettingsError {}

/// The four sliders. Defaults are the API's own (rate 1, pitch 0, volume 1,
/// emotional intensity 1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceTuning {
    pub speaking_rate: f32,
    pub pitch: f32,
    pub volume: f32,
    pub emotional_intensity: f32,
}

impl Default for VoiceTuning {
    fn default() -> Self {
        Self {
            speaking_rate: 1.0,
            pitch: 0.0,
            volume: 1.0,
            emotional_intensity: 1.0,
        }
    }
}

fn in_range(value: f32, (low, high): (f32, f32)) -> bool {
    value.is_finite() && (low..=high).contains(&value)
}

impl VoiceTuning {
    pub fn validate(&self) -> Result<(), VoiceSettingsError> {
        for (name, value, range) in [
            ("speaking_rate", self.speaking_rate, SPEAKING_RATE_RANGE),
            ("pitch", self.pitch, PITCH_RANGE),
            ("volume", self.volume, VOLUME_RANGE),
            (
                "emotional_intensity",
                self.emotional_intensity,
                EMOTIONAL_INTENSITY_RANGE,
            ),
        ] {
            if !in_range(value, range) {
                return Err(VoiceSettingsError::Tuning(name));
            }
        }
        Ok(())
    }

    /// Stable text for cache identity and provenance: the shortest form that
    /// round-trips each float.
    pub fn identity(&self) -> String {
        format!(
            "rate={};pitch={};volume={};emotion={}",
            self.speaking_rate, self.pitch, self.volume, self.emotional_intensity
        )
    }
}

/// What is chosen. Never holds a key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSettings {
    pub backend: VoiceBackend,
    #[serde(default)]
    pub model_uuid: Option<String>,
    /// For display after a restart; never used to select anything.
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub speaker_uuid: Option<String>,
    #[serde(default)]
    pub style_id: Option<u8>,
    #[serde(default)]
    pub tuning: VoiceTuning,
    /// The credit line the model's licence asks for. Plain text.
    #[serde(default)]
    pub credit: Option<String>,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            backend: VoiceBackend::Local,
            model_uuid: None,
            model_name: None,
            speaker_uuid: None,
            style_id: None,
            tuning: VoiceTuning::default(),
            credit: None,
        }
    }
}

/// A credit line: non-blank, at most [`MAX_VOICE_CREDIT_CHARS`], no control
/// characters (it is logged and may be copied to an overlay). Trimmed.
pub fn normalize_credit(credit: &str) -> Result<String, VoiceSettingsError> {
    let credit = credit.trim();
    if credit.is_empty() {
        return Err(VoiceSettingsError::Credit("is empty"));
    }
    if credit.chars().count() > MAX_VOICE_CREDIT_CHARS {
        return Err(VoiceSettingsError::Credit("is too long"));
    }
    if credit.chars().any(char::is_control) {
        return Err(VoiceSettingsError::Credit("contains control characters"));
    }
    Ok(credit.to_owned())
}

fn normalize_model_name(name: &str) -> Result<Option<String>, VoiceSettingsError> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_VOICE_MODEL_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(VoiceSettingsError::ModelName);
    }
    Ok(Some(name.to_owned()))
}

impl VoiceSettings {
    /// Shape and bounds only; whether Aivis can be enabled is
    /// [`Self::require_aivis_ready`].
    pub fn validate(&self) -> Result<(), VoiceSettingsError> {
        if self
            .model_uuid
            .as_deref()
            .is_some_and(|m| !is_voice_uuid(m))
        {
            return Err(VoiceSettingsError::ModelUuid);
        }
        if self
            .speaker_uuid
            .as_deref()
            .is_some_and(|s| !is_voice_uuid(s))
        {
            return Err(VoiceSettingsError::SpeakerUuid);
        }
        if self.style_id.is_some_and(|id| id > MAX_VOICE_STYLE_ID) {
            return Err(VoiceSettingsError::StyleId);
        }
        self.tuning.validate()?;
        if let Some(credit) = &self.credit
            && normalize_credit(credit)? != *credit
        {
            return Err(VoiceSettingsError::Credit("is not normalized"));
        }
        if let Some(name) = &self.model_name
            && normalize_model_name(name)?.as_deref() != Some(name.as_str())
        {
            return Err(VoiceSettingsError::ModelName);
        }
        Ok(())
    }

    /// The conditions for the Aivis backend to be on: a model and the credit
    /// line. (The API key is the app's concern.)
    pub fn require_aivis_ready(&self) -> Result<(), VoiceSettingsError> {
        if self.model_uuid.is_none() {
            return Err(VoiceSettingsError::MissingModel);
        }
        if self.credit.is_none() {
            return Err(VoiceSettingsError::MissingCredit);
        }
        Ok(())
    }

    /// The settings after `patch`, validated. Changing the model resets what
    /// belongs to the old one (speaker, style, name, credit line) unless the
    /// patch sets it, so a credit line is never carried to another model.
    /// Enabling Aivis from local needs `patch.confirm_egress`.
    pub fn apply(&self, patch: &VoicePatch) -> Result<Self, VoiceSettingsError> {
        let mut next = self.clone();
        if let Some(model) = &patch.model_uuid
            && *model != self.model_uuid
        {
            next.model_uuid = model.clone();
            next.speaker_uuid = None;
            next.style_id = None;
            next.model_name = None;
            next.credit = None;
        }
        if let Some(name) = &patch.model_name {
            next.model_name = match name {
                Some(name) => normalize_model_name(name)?,
                None => None,
            };
        }
        if let Some(speaker) = &patch.speaker_uuid {
            next.speaker_uuid = speaker.clone();
        }
        if let Some(style) = patch.style_id {
            next.style_id = style;
        }
        if let Some(credit) = &patch.credit {
            next.credit = match credit {
                Some(credit) => Some(normalize_credit(credit)?),
                None => None,
            };
        }
        if let Some(value) = patch.speaking_rate {
            next.tuning.speaking_rate = value;
        }
        if let Some(value) = patch.pitch {
            next.tuning.pitch = value;
        }
        if let Some(value) = patch.volume {
            next.tuning.volume = value;
        }
        if let Some(value) = patch.emotional_intensity {
            next.tuning.emotional_intensity = value;
        }
        if let Some(backend) = patch.backend {
            next.backend = backend;
        }
        next.validate()?;
        if next.backend == VoiceBackend::Aivis {
            next.require_aivis_ready()?;
            if self.backend == VoiceBackend::Local && !patch.confirm_egress {
                return Err(VoiceSettingsError::EgressNotConfirmed);
            }
        }
        Ok(next)
    }

    /// One short, text-free line for audit and logs: no credit line, no model
    /// name, never a key.
    pub fn audit_summary(&self) -> String {
        format!(
            "backend={} model={} speaker={} style={} {}",
            self.backend.as_str(),
            self.model_uuid.as_deref().unwrap_or("-"),
            self.speaker_uuid.as_deref().unwrap_or("-"),
            self.style_id
                .map_or_else(|| "-".to_owned(), |s| s.to_string()),
            self.tuning.identity()
        )
    }
}

fn some_value<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A partial update: a field that is absent is left alone, `null` clears an
/// optional one. This is the whole body of the panel's voice POST and the
/// `voice.apply` argument; it has no key field.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoicePatch {
    #[serde(default)]
    pub backend: Option<VoiceBackend>,
    #[serde(default, deserialize_with = "some_value")]
    pub model_uuid: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_value")]
    pub model_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_value")]
    pub speaker_uuid: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_value")]
    pub style_id: Option<Option<u8>>,
    #[serde(default, deserialize_with = "some_value")]
    pub credit: Option<Option<String>>,
    #[serde(default)]
    pub speaking_rate: Option<f32>,
    #[serde(default)]
    pub pitch: Option<f32>,
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub emotional_intensity: Option<f32>,
    /// The operator confirmed that reply text will be sent to Aivis Cloud.
    #[serde(default)]
    pub confirm_egress: bool,
}

impl VoicePatch {
    pub fn parse(text: &str) -> Result<Self, VoiceSettingsError> {
        if text.len() > MAX_VOICE_PATCH_BYTES {
            return Err(VoiceSettingsError::PatchTooLarge);
        }
        // A derived struct would also accept a JSON array; only an object is a patch.
        if !text.trim_start().starts_with('{') {
            return Err(VoiceSettingsError::Malformed);
        }
        serde_json::from_str(text).map_err(|_| VoiceSettingsError::Malformed)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    schema: String,
    settings: VoiceSettings,
}

/// The settings a document holds, or `None` for anything that is not exactly
/// the documented shape within the bounds.
pub fn parse_voice_settings(bytes: &[u8]) -> Option<VoiceSettings> {
    if bytes.len() as u64 > MAX_VOICE_SETTINGS_BYTES {
        return None;
    }
    let file: SettingsFile = serde_json::from_slice(bytes).ok()?;
    if file.schema != VOICE_SETTINGS_SCHEMA || file.settings.validate().is_err() {
        return None;
    }
    // A file that names Aivis without what Aivis needs is not trusted.
    if file.settings.backend == VoiceBackend::Aivis && file.settings.require_aivis_ready().is_err()
    {
        return None;
    }
    Some(file.settings)
}

pub fn render_voice_settings(settings: &VoiceSettings) -> Result<String, VoiceSettingsError> {
    settings.validate()?;
    let mut text = serde_json::to_string(&SettingsFile {
        schema: VOICE_SETTINGS_SCHEMA.to_owned(),
        settings: settings.clone(),
    })
    .map_err(|_| VoiceSettingsError::Malformed)?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "a59cb814-0083-4369-8542-f51a29e72af7";
    const SPEAKER: &str = "e756b8e4-b606-4e15-99b1-3f9c6a1b2317";

    fn patch(text: &str) -> VoicePatch {
        VoicePatch::parse(text).expect("patch")
    }

    fn aivis_patch() -> VoicePatch {
        patch(&format!(
            r#"{{"backend":"aivis","model_uuid":"{MODEL}","speaker_uuid":"{SPEAKER}","style_id":2,"credit":"AivisSpeech: まお","confirm_egress":true}}"#
        ))
    }

    #[test]
    fn the_defaults_are_the_apis_own_and_the_local_voice() {
        let s = VoiceSettings::default();
        assert_eq!(s.backend, VoiceBackend::Local);
        assert_eq!(
            s.tuning,
            VoiceTuning {
                speaking_rate: 1.0,
                pitch: 0.0,
                volume: 1.0,
                emotional_intensity: 1.0
            }
        );
        s.validate().unwrap();
    }

    #[test]
    fn enabling_aivis_needs_a_model_the_credit_and_the_egress_confirmation() {
        let local = VoiceSettings::default();
        assert_eq!(
            local.apply(&patch(r#"{"backend":"aivis","confirm_egress":true}"#)),
            Err(VoiceSettingsError::MissingModel)
        );
        assert_eq!(
            local.apply(&patch(&format!(
                r#"{{"backend":"aivis","model_uuid":"{MODEL}","confirm_egress":true}}"#
            ))),
            Err(VoiceSettingsError::MissingCredit)
        );
        let mut unconfirmed = aivis_patch();
        unconfirmed.confirm_egress = false;
        assert_eq!(
            local.apply(&unconfirmed),
            Err(VoiceSettingsError::EgressNotConfirmed)
        );
        let on = local.apply(&aivis_patch()).unwrap();
        assert_eq!(on.backend, VoiceBackend::Aivis);
        assert_eq!(on.credit.as_deref(), Some("AivisSpeech: まお"));
        assert_eq!(on.style_id, Some(2));
        // Already on: a style change needs no second confirmation.
        let style = on.apply(&patch(r#"{"style_id":3}"#)).unwrap();
        assert_eq!(style.style_id, Some(3));
        assert_eq!(style.credit, on.credit);
        // Back to local needs nothing and keeps the choice for later.
        let off = on.apply(&patch(r#"{"backend":"local"}"#)).unwrap();
        assert_eq!(off.backend, VoiceBackend::Local);
        assert_eq!(off.model_uuid.as_deref(), Some(MODEL));
    }

    #[test]
    fn another_model_resets_what_belonged_to_the_old_one() {
        let on = VoiceSettings::default().apply(&aivis_patch()).unwrap();
        let other = "11111111-2222-3333-4444-555555555555";
        // Without a credit line the new model cannot stay enabled.
        assert_eq!(
            on.apply(&patch(&format!(r#"{{"model_uuid":"{other}"}}"#))),
            Err(VoiceSettingsError::MissingCredit)
        );
        let next = on
            .apply(&patch(&format!(
                r#"{{"model_uuid":"{other}","credit":"AivisSpeech: x","model_name":"x"}}"#
            )))
            .unwrap();
        assert_eq!(next.speaker_uuid, None);
        assert_eq!(next.style_id, None);
        assert_eq!(next.model_name.as_deref(), Some("x"));
        // The same model again keeps its speaker and style.
        let same = on
            .apply(&patch(&format!(r#"{{"model_uuid":"{MODEL}"}}"#)))
            .unwrap();
        assert_eq!(same, on);
        // null clears an optional field.
        let cleared = on.apply(&patch(r#"{"style_id":null}"#)).unwrap();
        assert_eq!(cleared.style_id, None);
    }

    #[test]
    fn tuning_is_bounded_exactly_at_the_apis_limits() {
        let base = VoiceSettings::default();
        for ok in [
            r#"{"speaking_rate":0.5}"#,
            r#"{"speaking_rate":2.0}"#,
            r#"{"pitch":-1.0}"#,
            r#"{"pitch":1.0}"#,
            r#"{"volume":0.0}"#,
            r#"{"volume":2.0}"#,
            r#"{"emotional_intensity":0}"#,
            r#"{"emotional_intensity":2}"#,
        ] {
            base.apply(&patch(ok))
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            r#"{"speaking_rate":0.49}"#,
            r#"{"speaking_rate":2.01}"#,
            r#"{"pitch":-1.01}"#,
            r#"{"pitch":1.01}"#,
            r#"{"volume":-0.1}"#,
            r#"{"volume":2.1}"#,
            r#"{"emotional_intensity":-1}"#,
            r#"{"emotional_intensity":2.5}"#,
            r#"{"speaking_rate":1e39}"#,
        ] {
            assert!(
                matches!(
                    VoicePatch::parse(bad).and_then(|p| base.apply(&p)),
                    Err(VoiceSettingsError::Tuning(_) | VoiceSettingsError::Malformed)
                ),
                "{bad}"
            );
        }
        assert!(
            VoiceTuning {
                speaking_rate: f32::NAN,
                ..VoiceTuning::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn identifiers_and_text_are_validated() {
        let base = VoiceSettings::default();
        for bad in [
            r#"{"model_uuid":"not-a-uuid"}"#,
            r#"{"model_uuid":"ak_secretkey"}"#,
            r#"{"speaker_uuid":"../x"}"#,
            r#"{"style_id":32}"#,
            r#"{"style_id":-1}"#,
            r#"{"credit":"   "}"#,
            r#"{"credit":"a\nb"}"#,
            r#"{"model_name":"a\u0007b"}"#,
        ] {
            assert!(
                VoicePatch::parse(bad).and_then(|p| base.apply(&p)).is_err(),
                "{bad}"
            );
        }
        let long = format!(r#"{{"credit":"{}"}}"#, "あ".repeat(201));
        assert!(base.apply(&patch(&long)).is_err());
        let long_name = format!(r#"{{"model_name":"{}"}}"#, "a".repeat(81));
        assert!(base.apply(&patch(&long_name)).is_err());
        // A trimmed credit is what is stored.
        let s = base
            .apply(&patch(r#"{"credit":"  AivisSpeech: a  "}"#))
            .unwrap();
        assert_eq!(s.credit.as_deref(), Some("AivisSpeech: a"));
    }

    #[test]
    fn a_patch_has_no_key_and_no_unknown_fields() {
        for bad in [
            r#"{"api_key":"sk-x"}"#,
            r#"{"key":"x"}"#,
            r#"{"backend":"openai"}"#,
            r#"{"unknown":1}"#,
            "",
            "[]",
            "null",
        ] {
            assert!(VoicePatch::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            VoicePatch::parse(&" ".repeat(MAX_VOICE_PATCH_BYTES + 1)),
            Err(VoiceSettingsError::PatchTooLarge)
        );
        assert_eq!(VoicePatch::parse("{}").unwrap(), VoicePatch::default());
    }

    #[test]
    fn the_settings_document_round_trips_and_refuses_everything_else() {
        let on = VoiceSettings::default().apply(&aivis_patch()).unwrap();
        let text = render_voice_settings(&on).unwrap();
        assert_eq!(parse_voice_settings(text.as_bytes()), Some(on.clone()));
        assert!(!text.to_lowercase().contains("key"));
        for bad in [
            &b""[..],
            b"not json",
            br#"{"schema":"other/1","settings":{"backend":"local"}}"#,
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"local"},"x":1}"#,
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"local","api_key":"x"}}"#,
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"local","model_uuid":"C:\\evil"}}"#,
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"local","tuning":{"speaking_rate":9,"pitch":0,"volume":1,"emotional_intensity":1}}}"#,
            // Aivis without a model or a credit is not trusted.
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"aivis"}}"#,
        ] {
            assert_eq!(
                parse_voice_settings(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let huge = format!(
            r#"{{"schema":"local-voice-settings/1","settings":{{"backend":"local","credit":"{}"}}}}"#,
            "x".repeat(5000)
        );
        assert_eq!(parse_voice_settings(huge.as_bytes()), None);
        // A minimal document gets the defaults.
        let minimal = parse_voice_settings(
            br#"{"schema":"local-voice-settings/1","settings":{"backend":"local"}}"#,
        )
        .unwrap();
        assert_eq!(minimal, VoiceSettings::default());
        // The largest legal document fits the size bound.
        let max = VoiceSettings {
            credit: Some("あ".repeat(MAX_VOICE_CREDIT_CHARS)),
            model_name: Some("あ".repeat(MAX_VOICE_MODEL_NAME_CHARS)),
            ..on
        };
        assert!(render_voice_settings(&max).unwrap().len() as u64 <= MAX_VOICE_SETTINGS_BYTES);
    }

    #[test]
    fn the_audit_line_carries_no_free_text() {
        let on = VoiceSettings::default().apply(&aivis_patch()).unwrap();
        let line = on.audit_summary();
        assert!(line.contains(MODEL) && line.contains("rate=1"));
        assert!(!line.contains("まお") && !line.contains("AivisSpeech"));
    }
}
