//! Provider-neutral speech transcripts (issue #92, slice S3b).
//!
//! A transcript is *content* a microphone speaker said, as heard by a speech
//! recognizer: untrusted, possibly wrong, and for a partial, deliberately
//! incomplete. The wire form has no source class, plane, trust, priority or
//! authorization field on purpose: the receiving ingress assigns them
//! (ADR-0005 S-7), and the recognizer's output is data, never an instruction.
//! There is no audio, PCM or word timing here.
//!
//! Normative wire form: `schemas/speech-transcript.schema.json`. Consumption
//! rules (a partial may only hold the floor, only a final becomes
//! conversation input, a revision supersedes and never appends) are enforced
//! by `aivtuber_runtime::SpeechTranscriptTracker`.

use crate::DomainValidationError;
use crate::speech::{
    MAX_MODEL_ID_LEN, MAX_OBSERVATION_TIME_MS, MAX_PROVIDER_TOKEN_LEN, MAX_SPEAKER_ID_LEN,
    validate_charset, validate_language, validate_provider_token,
};
use serde::{Deserialize, Serialize};

pub const SPEECH_TRANSCRIPT_SCHEMA_VERSION: &str = "0.1.0";
/// Upper bound on transcripts (revisions) in one batch.
pub const MAX_TRANSCRIPTS_PER_BATCH: usize = 4;
/// Upper bound on one serialized batch, enforced by the ingress before parsing.
/// Four 1000-character Japanese texts are about 12 KiB in UTF-8.
pub const MAX_SPEECH_TRANSCRIPT_BATCH_BYTES: usize = 16 * 1024;
/// Longest transcript text, in Unicode scalar values.
pub const MAX_TRANSCRIPT_TEXT_CHARS: usize = 1_000;
/// Highest revision number of one segment (about 60 are reached in 30 s).
pub const MAX_TRANSCRIPT_REVISION: u32 = 4_095;
/// Longest audio span one transcript may claim to cover.
pub const MAX_TRANSCRIPT_COVERED_AUDIO_MS: u64 = 60_000;
/// Hex length of the engine digest (SHA-256).
pub const ENGINE_SHA256_HEX_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    /// Incomplete and revisable. May only feed observation-style floor policy.
    Partial,
    /// The recognizer's last word on the segment.
    Final,
}

/// Which model produced the text, so replay and audit can tell engines apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEngine {
    pub model_id: String,
    /// Lower-case hex SHA-256 identifying the exact model files.
    pub sha256: String,
}

/// One revision of one segment's text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscript {
    pub speaker_id: String,
    pub segment_id: u64,
    pub revision: u32,
    pub kind: TranscriptKind,
    /// Engine output, unmodified. Untrusted content.
    pub text: String,
    /// Audio, from the segment start (pre-roll included), this text covers.
    pub covered_audio_ms: u64,
    /// Runtime-timeline time of the end of the covered audio.
    pub observed_at_ms: u64,
    /// False on every partial, true on every final.
    pub complete: bool,
}

// The text is what a person said: keep it out of logs and panic messages.
impl std::fmt::Debug for SpeechTranscript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeechTranscript")
            .field("speaker_id", &self.speaker_id)
            .field("segment_id", &self.segment_id)
            .field("revision", &self.revision)
            .field("kind", &self.kind)
            .field("text_chars", &self.text.chars().count())
            .field("covered_audio_ms", &self.covered_audio_ms)
            .field("observed_at_ms", &self.observed_at_ms)
            .field("complete", &self.complete)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscriptBatch {
    pub schema_version: String,
    pub provider_id: String,
    pub provider_version: String,
    /// Declared by configuration, never inferred.
    pub language: String,
    pub engine: TranscriptEngine,
    pub transcripts: Vec<SpeechTranscript>,
}

impl SpeechTranscriptBatch {
    /// Mirrors `speech-transcript.schema.json`; serde enforces shape and
    /// unknown fields, this enforces ranges, lengths and charsets.
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != SPEECH_TRANSCRIPT_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {SPEECH_TRANSCRIPT_SCHEMA_VERSION}"),
            ));
        }
        validate_provider_token("provider_id", &self.provider_id)?;
        validate_charset(
            "provider_version",
            &self.provider_version,
            MAX_PROVIDER_TOKEN_LEN,
            |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'),
        )?;
        validate_language(&self.language)?;
        validate_charset(
            "engine.model_id",
            &self.engine.model_id,
            MAX_MODEL_ID_LEN,
            |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'/' | b'-'),
        )?;
        let sha = &self.engine.sha256;
        if sha.len() != ENGINE_SHA256_HEX_LEN
            || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(DomainValidationError::new(
                "engine.sha256",
                "must be 64 lower-case hex characters",
            ));
        }
        if self.transcripts.is_empty() || self.transcripts.len() > MAX_TRANSCRIPTS_PER_BATCH {
            return Err(DomainValidationError::new(
                "transcripts",
                format!("must contain 1..={MAX_TRANSCRIPTS_PER_BATCH} transcripts"),
            ));
        }
        self.transcripts.iter().try_for_each(validate_transcript)
    }

    /// Parse and validate one batch from bytes. The caller bounds the byte
    /// length first (`MAX_SPEECH_TRANSCRIPT_BATCH_BYTES`).
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, DomainValidationError> {
        let batch: Self = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("batch", error.to_string()))?;
        batch.validate()?;
        Ok(batch)
    }
}

fn validate_transcript(transcript: &SpeechTranscript) -> Result<(), DomainValidationError> {
    validate_charset(
        "speaker_id",
        &transcript.speaker_id,
        MAX_SPEAKER_ID_LEN,
        |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'),
    )?;
    if transcript.segment_id > MAX_OBSERVATION_TIME_MS {
        return Err(DomainValidationError::new(
            "segment_id",
            "exceeds the JSON-safe integer range",
        ));
    }
    if transcript.revision > MAX_TRANSCRIPT_REVISION {
        return Err(DomainValidationError::new(
            "revision",
            format!("must be at most {MAX_TRANSCRIPT_REVISION}"),
        ));
    }
    if transcript.covered_audio_ms > MAX_TRANSCRIPT_COVERED_AUDIO_MS {
        return Err(DomainValidationError::new(
            "covered_audio_ms",
            format!("must be at most {MAX_TRANSCRIPT_COVERED_AUDIO_MS}"),
        ));
    }
    if transcript.observed_at_ms > MAX_OBSERVATION_TIME_MS {
        return Err(DomainValidationError::new(
            "observed_at_ms",
            "exceeds the JSON-safe integer range",
        ));
    }
    // `chars()` stops at the first violation: a hostile megabyte string is
    // rejected after 1001 characters, not scanned whole.
    if transcript
        .text
        .chars()
        .take(MAX_TRANSCRIPT_TEXT_CHARS + 1)
        .count()
        > MAX_TRANSCRIPT_TEXT_CHARS
    {
        return Err(DomainValidationError::new(
            "text",
            format!("must be at most {MAX_TRANSCRIPT_TEXT_CHARS} characters"),
        ));
    }
    if transcript.text.chars().any(char::is_control) {
        return Err(DomainValidationError::new(
            "text",
            "must not contain control characters",
        ));
    }
    match transcript.kind {
        TranscriptKind::Partial => {
            if transcript.complete {
                return Err(DomainValidationError::new(
                    "complete",
                    "a partial must be incomplete",
                ));
            }
            if transcript.text.is_empty() {
                return Err(DomainValidationError::new(
                    "text",
                    "a partial must have at least one character",
                ));
            }
        }
        TranscriptKind::Final => {
            if !transcript.complete {
                return Err(DomainValidationError::new(
                    "complete",
                    "a final must be complete",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(kind: TranscriptKind, text: &str) -> SpeechTranscriptBatch {
        SpeechTranscriptBatch {
            schema_version: SPEECH_TRANSCRIPT_SCHEMA_VERSION.to_owned(),
            provider_id: "speech-stt-worker".to_owned(),
            provider_version: "1".to_owned(),
            language: "ja".to_owned(),
            engine: TranscriptEngine {
                model_id: "m:1".to_owned(),
                sha256: "0".repeat(64),
            },
            transcripts: vec![SpeechTranscript {
                speaker_id: "s1".to_owned(),
                segment_id: 1,
                revision: 0,
                kind,
                text: text.to_owned(),
                covered_audio_ms: 500,
                observed_at_ms: 500,
                complete: kind == TranscriptKind::Final,
            }],
        }
    }

    #[test]
    fn a_well_formed_batch_validates_and_round_trips() {
        let b = batch(TranscriptKind::Final, "こんにちは");
        b.validate().unwrap();
        let bytes = serde_json::to_vec(&b).unwrap();
        assert_eq!(SpeechTranscriptBatch::from_json_bytes(&bytes).unwrap(), b);
    }

    #[test]
    fn text_bounds_count_characters_and_reject_controls() {
        assert!(
            batch(TranscriptKind::Final, &"あ".repeat(1_000))
                .validate()
                .is_ok()
        );
        assert!(
            batch(TranscriptKind::Final, &"あ".repeat(1_001))
                .validate()
                .is_err()
        );
        assert!(batch(TranscriptKind::Final, "a\u{7f}").validate().is_err());
        assert!(batch(TranscriptKind::Final, "a\u{85}").validate().is_err());
        assert!(batch(TranscriptKind::Final, "a\tb").validate().is_err());
    }

    #[test]
    fn partial_and_final_completeness_are_enforced() {
        assert!(batch(TranscriptKind::Partial, "").validate().is_err());
        assert!(batch(TranscriptKind::Final, "").validate().is_ok());
        let mut b = batch(TranscriptKind::Partial, "a");
        b.transcripts[0].complete = true;
        assert!(b.validate().is_err());
        let mut b = batch(TranscriptKind::Final, "a");
        b.transcripts[0].complete = false;
        assert!(b.validate().is_err());
    }

    #[test]
    fn unknown_trust_plane_or_priority_fields_are_refused() {
        let good = serde_json::to_string(&batch(TranscriptKind::Final, "a")).unwrap();
        for field in [
            "\"trust_level\":\"trusted\"",
            "\"plane\":\"control\"",
            "\"priority\":\"interrupt\"",
        ] {
            let hostile = good.replacen('{', &format!("{{{field},"), 1);
            assert!(
                SpeechTranscriptBatch::from_json_bytes(hostile.as_bytes()).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn debug_output_never_shows_the_text() {
        let b = batch(TranscriptKind::Final, "ひみつの発言");
        assert!(!format!("{b:?}").contains("ひみつ"));
    }

    #[test]
    fn engine_digest_must_be_lower_case_hex_of_the_right_length() {
        let mut b = batch(TranscriptKind::Final, "a");
        b.engine.sha256 = "A".repeat(64);
        assert!(b.validate().is_err());
        b.engine.sha256 = "a".repeat(63);
        assert!(b.validate().is_err());
    }

    /// Fixture -> Rust parity: the same files `bun run validate` checks against
    /// the schema must be accepted/rejected identically by the Rust validator.
    #[test]
    fn schema_fixtures_agree_with_the_rust_validator() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/speech-transcripts");
        for (dir, should_pass) in [("valid", true), ("invalid", false)] {
            let mut count = 0;
            for entry in std::fs::read_dir(base.join(dir)).expect("fixture dir") {
                let path = entry.expect("entry").path();
                let bytes = std::fs::read(&path).expect("read");
                assert_eq!(
                    SpeechTranscriptBatch::from_json_bytes(&bytes).is_ok(),
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
