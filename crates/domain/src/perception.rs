//! Provider-neutral perception observations (issue #90, ADR-0007, slice S1a).
//!
//! A perception observation is what a vision model *thinks* one operator-chosen
//! screen source currently shows: a closed scene class with closed-vocabulary
//! labels, or one short caption. It is untrusted content and never authority.
//! The wire form has no source class, plane, trust, priority or authorization
//! field on purpose: the receiving ingress assigns plane and trust (always
//! content / untrusted), and unknown fields are rejected so a payload cannot
//! self-escalate. There is no image, pixel, hash, embedding, bounding box or
//! OCR transcript here.
//!
//! Normative wire form: `schemas/perception-observation.schema.json`. Ingress,
//! expiry, source hierarchy and bounded snapshots live in
//! `aivtuber_runtime::PerceptionTracker`.

use crate::context::PrivacyClass;
use crate::speech::{
    MAX_MODEL_ID_LEN, MAX_OBSERVATION_TIME_MS, MAX_PROVIDER_TOKEN_LEN, deserialize_present,
    validate_charset, validate_provider_token,
};
use crate::{DomainValidationError, ENGINE_SHA256_HEX_LEN};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

pub const PERCEPTION_OBSERVATION_SCHEMA_VERSION: &str = "0.1.0";
/// Upper bound on observations in one batch.
pub const MAX_PERCEPTION_OBSERVATIONS_PER_BATCH: usize = 8;
/// Upper bound on one serialized batch, enforced by the ingress before parsing.
pub const MAX_PERCEPTION_BATCH_BYTES: usize = 16 * 1024;
/// Longest caption, in UTF-8 bytes. Limits both the injection surface and the
/// amount of screen content that can be exfiltrated through a caption.
pub const MAX_PERCEPTION_CAPTION_BYTES: usize = 240;
/// Most labels one scene observation may carry.
pub const MAX_PERCEPTION_LABELS: usize = 8;
/// Longest label token, in bytes.
pub const MAX_PERCEPTION_LABEL_BYTES: usize = 32;
/// Most entries in an operator profile's label vocabulary.
pub const MAX_PERCEPTION_LABEL_VOCABULARY: usize = 64;
/// Longest source alias, observation id and similar label, in bytes.
pub const MAX_PERCEPTION_ALIAS_BYTES: usize = 64;
/// Longest `expires_at_ms - observed_at_ms` a screen observation may declare
/// (ADR-0007 D-Q12). A description of a game that has moved on is worse than
/// none.
pub const MAX_SCREEN_OBSERVATION_VALIDITY_MS: u64 = 30_000;
/// Largest downscaled frame edge an observation may report.
pub const MAX_PERCEPTION_FRAME_DIMENSION: u32 = 4_096;
/// Most producer-side secret redactions an observation may report.
pub const MAX_PERCEPTION_SECRET_HITS: u32 = 1_000;
/// The Thinking context item built from a caption (delimiters and age
/// included) is at most this many bytes (ADR-0007 D-Q7).
pub const MAX_SCREEN_OBSERVATION_THINKING_BYTES: usize = 512;

/// Provisional label vocabulary (ADR-0007 D-Q11). The scene context (#88) owns
/// the final vocabulary; an operator profile may replace this set. A label
/// outside the configured vocabulary is dropped and counted at ingress.
pub const PROVISIONAL_LABEL_VOCABULARY: [&str; 12] = [
    "boss_fight",
    "combat",
    "dialogue",
    "exploration",
    "inventory",
    "map",
    "multiplayer_lobby",
    "puzzle",
    "racing",
    "settings",
    "title_screen",
    "victory",
];

/// Provisional closed scene vocabulary (ADR-0007 D-Q11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerceptionSceneClass {
    Gameplay,
    Menu,
    Loading,
    Cutscene,
    Result,
    Desktop,
    /// The producer could not tell. Never overrides another source.
    Unknown,
}

impl PerceptionSceneClass {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gameplay => "gameplay",
            Self::Menu => "menu",
            Self::Loading => "loading",
            Self::Cutscene => "cutscene",
            Self::Result => "result",
            Self::Desktop => "desktop",
            Self::Unknown => "unknown",
        }
    }
}

/// Closed observation kinds. Turn-cue kinds are reserved, not specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerceptionKind {
    ScreenScene,
    ScreenCaption,
}

impl PerceptionKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ScreenScene => "screen_scene",
            Self::ScreenCaption => "screen_caption",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionSource {
    /// Operator-configured label, never a window title or process name.
    pub alias: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionFrame {
    pub capture_seq: u64,
    pub captured_at_ms: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionProducer {
    pub model_id: String,
    /// Lower-case hex SHA-256 over the producer configuration.
    pub config_digest: String,
    pub prompt_version: String,
}

/// Counters only; the removed text is never kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionRedaction {
    pub policy_version: String,
    pub secret_hits: u32,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionScenePayload {
    pub scene_class: PerceptionSceneClass,
    pub labels: Vec<String>,
}

/// One short free-text caption: untrusted text about a screen.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionCaptionPayload {
    pub caption: String,
}

// The caption is derived from private screen content: keep it out of logs and
// panic messages.
impl fmt::Debug for PerceptionCaptionPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerceptionCaptionPayload")
            .field("caption_bytes", &self.caption.len())
            .finish()
    }
}

/// Payload shape, fixed by [`PerceptionKind`]. Each variant rejects unknown
/// fields, so a payload carrying both shapes matches neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PerceptionPayload {
    Scene(PerceptionScenePayload),
    Caption(PerceptionCaptionPayload),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionObservation {
    pub kind: PerceptionKind,
    pub observation_id: String,
    pub source: PerceptionSource,
    pub frame: PerceptionFrame,
    pub observed_at_ms: u64,
    /// Required: an observation that never expires would describe a screen
    /// that no longer exists.
    pub expires_at_ms: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub confidence: Option<f64>,
    /// Defaults to false. A consumer must not read an uncalibrated confidence
    /// as a probability.
    #[serde(default)]
    pub calibrated: bool,
    pub producer: PerceptionProducer,
    pub privacy: PrivacyClass,
    pub redaction: PerceptionRedaction,
    pub payload: PerceptionPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionObservationBatch {
    pub schema_version: String,
    pub provider_id: String,
    pub provider_version: String,
    pub observations: Vec<PerceptionObservation>,
}

impl PerceptionObservationBatch {
    /// Mirrors `perception-observation.schema.json`; serde enforces shape and
    /// unknown fields, this enforces ranges, lengths, charsets and the
    /// cross-field rules JSON Schema cannot express.
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != PERCEPTION_OBSERVATION_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {PERCEPTION_OBSERVATION_SCHEMA_VERSION}"),
            ));
        }
        validate_provider_token("provider_id", &self.provider_id)?;
        validate_version_token("provider_version", &self.provider_version)?;
        if self.observations.is_empty()
            || self.observations.len() > MAX_PERCEPTION_OBSERVATIONS_PER_BATCH
        {
            return Err(DomainValidationError::new(
                "observations",
                format!("must contain 1..={MAX_PERCEPTION_OBSERVATIONS_PER_BATCH} observations"),
            ));
        }
        let mut ids = BTreeSet::new();
        for observation in &self.observations {
            validate_observation(observation)?;
            if !ids.insert(observation.observation_id.as_str()) {
                return Err(DomainValidationError::new(
                    "observation_id",
                    "must be unique within a batch",
                ));
            }
        }
        Ok(())
    }

    /// Parse and validate one batch from bytes. The caller bounds the byte
    /// length first (`MAX_PERCEPTION_BATCH_BYTES`).
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, DomainValidationError> {
        let batch: Self = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("batch", error.to_string()))?;
        batch.validate()?;
        Ok(batch)
    }
}

fn validate_version_token(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    validate_charset(field, value, MAX_PROVIDER_TOKEN_LEN, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-')
    })
}

fn validate_label(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    validate_charset(field, value, MAX_PERCEPTION_ALIAS_BYTES, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
    })
}

/// `^[a-z][a-z0-9_]*$`, at most 32 bytes: one closed-vocabulary token.
pub fn validate_perception_label_token(
    field: &'static str,
    value: &str,
) -> Result<(), DomainValidationError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_PERCEPTION_LABEL_BYTES {
        return Err(DomainValidationError::new(
            field,
            format!("length must be 1..={MAX_PERCEPTION_LABEL_BYTES}"),
        ));
    }
    if !bytes[0].is_ascii_lowercase()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    {
        return Err(DomainValidationError::new(
            field,
            "must be a lower-case token",
        ));
    }
    Ok(())
}

/// Characters a caption may not contain: controls, bidi controls, zero-width
/// and separator characters, and the compatibility forms (full-width and
/// half-width forms, the ideographic space) that NFKC would rewrite. The
/// schema pattern lists the same ranges.
fn caption_char_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{3000}'
                | '\u{FEFF}'
                | '\u{FF00}'..='\u{FFFF}'
        )
}

fn validate_caption(caption: &str) -> Result<(), DomainValidationError> {
    if caption.is_empty() || caption.len() > MAX_PERCEPTION_CAPTION_BYTES {
        return Err(DomainValidationError::new(
            "caption",
            format!("must contain 1..={MAX_PERCEPTION_CAPTION_BYTES} UTF-8 bytes"),
        ));
    }
    if caption.chars().any(caption_char_forbidden) {
        return Err(DomainValidationError::new(
            "caption",
            "must not contain control, bidi, zero-width or NFKC-unstable characters",
        ));
    }
    Ok(())
}

fn validate_observation(observation: &PerceptionObservation) -> Result<(), DomainValidationError> {
    validate_label("observation_id", &observation.observation_id)?;
    validate_label("source.alias", &observation.source.alias)?;
    let frame = &observation.frame;
    if frame.capture_seq > MAX_OBSERVATION_TIME_MS || frame.captured_at_ms > MAX_OBSERVATION_TIME_MS
    {
        return Err(DomainValidationError::new(
            "frame",
            "exceeds the JSON-safe integer range",
        ));
    }
    for (field, edge) in [("frame.width", frame.width), ("frame.height", frame.height)] {
        if !(1..=MAX_PERCEPTION_FRAME_DIMENSION).contains(&edge) {
            return Err(DomainValidationError::new(
                field,
                format!("must be in 1..={MAX_PERCEPTION_FRAME_DIMENSION}"),
            ));
        }
    }
    if observation.observed_at_ms > MAX_OBSERVATION_TIME_MS {
        return Err(DomainValidationError::new(
            "observed_at_ms",
            "exceeds the JSON-safe integer range",
        ));
    }
    if frame.captured_at_ms > observation.observed_at_ms {
        return Err(DomainValidationError::new(
            "frame.captured_at_ms",
            "must not be after observed_at_ms",
        ));
    }
    if observation.expires_at_ms <= observation.observed_at_ms
        || observation.expires_at_ms - observation.observed_at_ms
            > MAX_SCREEN_OBSERVATION_VALIDITY_MS
    {
        return Err(DomainValidationError::new(
            "expires_at_ms",
            format!(
                "must be after observed_at_ms and within {MAX_SCREEN_OBSERVATION_VALIDITY_MS} ms"
            ),
        ));
    }
    if let Some(confidence) = observation.confidence
        && !(confidence.is_finite() && (0.0..=1.0).contains(&confidence))
    {
        return Err(DomainValidationError::new("confidence", "must be in 0..=1"));
    }
    let producer = &observation.producer;
    validate_charset(
        "producer.model_id",
        &producer.model_id,
        MAX_MODEL_ID_LEN,
        |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'/' | b'-'),
    )?;
    let digest = &producer.config_digest;
    if digest.len() != ENGINE_SHA256_HEX_LEN
        || !digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(DomainValidationError::new(
            "producer.config_digest",
            "must be 64 lower-case hex characters",
        ));
    }
    validate_version_token("producer.prompt_version", &producer.prompt_version)?;
    validate_version_token(
        "redaction.policy_version",
        &observation.redaction.policy_version,
    )?;
    if observation.redaction.secret_hits > MAX_PERCEPTION_SECRET_HITS {
        return Err(DomainValidationError::new(
            "redaction.secret_hits",
            format!("must be at most {MAX_PERCEPTION_SECRET_HITS}"),
        ));
    }
    match (observation.kind, &observation.payload) {
        (PerceptionKind::ScreenScene, PerceptionPayload::Scene(scene)) => {
            if scene.labels.len() > MAX_PERCEPTION_LABELS {
                return Err(DomainValidationError::new(
                    "labels",
                    format!("must contain at most {MAX_PERCEPTION_LABELS} labels"),
                ));
            }
            let mut unique = BTreeSet::new();
            for label in &scene.labels {
                validate_perception_label_token("labels", label)?;
                if !unique.insert(label.as_str()) {
                    return Err(DomainValidationError::new("labels", "items must be unique"));
                }
            }
        }
        (PerceptionKind::ScreenCaption, PerceptionPayload::Caption(caption)) => {
            validate_caption(&caption.caption)?;
        }
        _ => {
            return Err(DomainValidationError::new(
                "payload",
                "payload shape does not match kind",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "0".repeat(64)
    }

    fn observation(kind: PerceptionKind, payload: PerceptionPayload) -> PerceptionObservation {
        PerceptionObservation {
            kind,
            observation_id: "obs-1".to_owned(),
            source: PerceptionSource {
                alias: "game-main".to_owned(),
            },
            frame: PerceptionFrame {
                capture_seq: 1,
                captured_at_ms: 1_000,
                width: 768,
                height: 432,
            },
            observed_at_ms: 1_200,
            expires_at_ms: 11_200,
            confidence: None,
            calibrated: false,
            producer: PerceptionProducer {
                model_id: "qwen3-vl:4b".to_owned(),
                config_digest: digest(),
                prompt_version: "p1".to_owned(),
            },
            privacy: PrivacyClass::Private,
            redaction: PerceptionRedaction {
                policy_version: "r1".to_owned(),
                secret_hits: 0,
                truncated: false,
            },
            payload,
        }
    }

    fn caption_batch(text: &str) -> PerceptionObservationBatch {
        PerceptionObservationBatch {
            schema_version: PERCEPTION_OBSERVATION_SCHEMA_VERSION.to_owned(),
            provider_id: "vision-sidecar".to_owned(),
            provider_version: "1".to_owned(),
            observations: vec![observation(
                PerceptionKind::ScreenCaption,
                PerceptionPayload::Caption(PerceptionCaptionPayload {
                    caption: text.to_owned(),
                }),
            )],
        }
    }

    #[test]
    fn a_well_formed_batch_validates_and_round_trips() {
        let batch = caption_batch("ボス戦の途中です");
        batch.validate().unwrap();
        let bytes = serde_json::to_vec(&batch).unwrap();
        assert_eq!(
            PerceptionObservationBatch::from_json_bytes(&bytes).unwrap(),
            batch
        );
    }

    #[test]
    fn caption_is_bounded_in_bytes_and_rejects_hostile_characters() {
        assert!(caption_batch(&"a".repeat(240)).validate().is_ok());
        assert!(caption_batch(&"a".repeat(241)).validate().is_err());
        // 80 three-byte characters are exactly 240 bytes; 81 are over.
        assert!(caption_batch(&"あ".repeat(80)).validate().is_ok());
        assert!(caption_batch(&"あ".repeat(81)).validate().is_err());
        for bad in [
            "a\u{0}b",
            "a\nb",
            "a\tb",
            "a\u{202E}b",
            "a\u{200B}b",
            "a\u{2066}b",
            "ＡＢＣ",
            "ｶﾀｶﾅ",
            "a\u{3000}b",
            "a\u{FEFF}b",
            "a\u{85}b",
        ] {
            assert!(caption_batch(bad).validate().is_err(), "{bad:?}");
        }
        assert!(caption_batch("").validate().is_err());
    }

    #[test]
    fn expiry_is_required_after_observation_and_within_the_cap() {
        let mut batch = caption_batch("x");
        batch.observations[0].expires_at_ms = batch.observations[0].observed_at_ms;
        assert!(batch.validate().is_err());
        batch.observations[0].expires_at_ms =
            batch.observations[0].observed_at_ms + MAX_SCREEN_OBSERVATION_VALIDITY_MS;
        assert!(batch.validate().is_ok());
        batch.observations[0].expires_at_ms += 1;
        assert!(batch.validate().is_err());
        let json = serde_json::to_string(&caption_batch("x")).unwrap();
        let without = json.replace(",\"expires_at_ms\":11200", "");
        assert_ne!(json, without);
        assert!(PerceptionObservationBatch::from_json_bytes(without.as_bytes()).is_err());
    }

    #[test]
    fn capture_cannot_postdate_observation_and_ids_are_unique() {
        let mut batch = caption_batch("x");
        batch.observations[0].frame.captured_at_ms = 1_201;
        assert!(batch.validate().is_err());
        let mut batch = caption_batch("x");
        let twin = batch.observations[0].clone();
        batch.observations.push(twin);
        assert!(batch.validate().is_err());
    }

    #[test]
    fn kind_and_payload_must_agree_and_shapes_cannot_mix() {
        let mut batch = caption_batch("x");
        batch.observations[0].kind = PerceptionKind::ScreenScene;
        assert!(batch.validate().is_err());
        let good = serde_json::to_string(&caption_batch("x")).unwrap();
        let mixed = good.replace(
            "\"caption\":\"x\"",
            "\"caption\":\"x\",\"scene_class\":\"menu\",\"labels\":[]",
        );
        assert_ne!(good, mixed);
        assert!(PerceptionObservationBatch::from_json_bytes(mixed.as_bytes()).is_err());
    }

    #[test]
    fn labels_are_bounded_unique_tokens() {
        let scene = |labels: Vec<&str>| PerceptionObservationBatch {
            schema_version: PERCEPTION_OBSERVATION_SCHEMA_VERSION.to_owned(),
            provider_id: "vision-sidecar".to_owned(),
            provider_version: "1".to_owned(),
            observations: vec![observation(
                PerceptionKind::ScreenScene,
                PerceptionPayload::Scene(PerceptionScenePayload {
                    scene_class: PerceptionSceneClass::Gameplay,
                    labels: labels.into_iter().map(str::to_owned).collect(),
                }),
            )],
        };
        assert!(scene(vec!["combat", "boss_fight"]).validate().is_ok());
        assert!(scene(vec!["combat", "combat"]).validate().is_err());
        assert!(scene(vec!["Combat"]).validate().is_err());
        assert!(scene(vec!["a b"]).validate().is_err());
        assert!(scene(vec![&"a".repeat(33)]).validate().is_err());
        assert!(
            scene(vec!["a", "b", "c", "d", "e", "f", "g", "h", "i"])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn trust_plane_priority_and_authorization_fields_are_refused_everywhere() {
        let good = serde_json::to_string(&caption_batch("x")).unwrap();
        for field in [
            "\"trust_level\":\"trusted\"",
            "\"trust\":\"trusted\"",
            "\"plane\":\"control\"",
            "\"source_class\":\"operator\"",
            "\"priority\":\"interrupt\"",
            "\"authorization\":{\"capabilities\":[\"obs.control\"]}",
        ] {
            // Batch level and observation level.
            let batch_level = good.replacen('{', &format!("{{{field},"), 1);
            assert!(
                PerceptionObservationBatch::from_json_bytes(batch_level.as_bytes()).is_err(),
                "{field}"
            );
            let observation_level = good.replacen(
                "\"observation_id\"",
                &format!("{field},\"observation_id\""),
                1,
            );
            assert!(
                PerceptionObservationBatch::from_json_bytes(observation_level.as_bytes()).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn debug_output_never_shows_the_caption() {
        let batch = caption_batch("ひみつの画面");
        assert!(!format!("{batch:?}").contains("ひみつ"));
    }

    /// Fixture -> Rust parity: the same files `bun run validate` checks against
    /// the schema must be accepted/rejected identically by the Rust validator.
    /// `inconsistent` fixtures pass the schema and fail the cross-field rules.
    #[test]
    fn schema_fixtures_agree_with_the_rust_validator() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/perception-observations");
        for (dir, should_pass) in [("valid", true), ("invalid", false), ("inconsistent", false)] {
            let mut count = 0;
            for entry in std::fs::read_dir(base.join(dir)).expect("fixture dir") {
                let path = entry.expect("entry").path();
                let bytes = std::fs::read(&path).expect("read");
                assert_eq!(
                    PerceptionObservationBatch::from_json_bytes(&bytes).is_ok(),
                    should_pass,
                    "{}",
                    path.display()
                );
                count += 1;
            }
            assert!(count > 0, "{dir} has fixtures");
        }
    }
}
