//! Provider-neutral speech lifecycle record (issue #222 S2, ADR-0004 D5).
//!
//! A [`SpeechLifecycleEvent`] says what the scheduler/audio execution path
//! knows about one spoken unit (one reply, or one phrase of a phrased reply):
//! it was prepared, committed, started, passed a declared safe point, finished,
//! or cancelled. It is audit/replay data, never authority: it carries no
//! source class, trust or capability, and no free text (only digests and
//! identities, so the causal-trace "no free-text fields" rule holds).
//!
//! The scheduler owns the truth (`aivtuber_scheduler::SpeechLifecycleLog` is
//! the only producer and the only writer of `seq`); a TTS provider reporting
//! "synthesis done" is `Prepared`, never `Finished`. `basis` says how a fact
//! was observed, and a consumer must not treat `logical_timeline` as audible
//! (ADR-0004 D-6).
//!
//! Normative wire form: `schemas/speech-lifecycle.schema.json`.

use crate::DomainValidationError;
use crate::speech::deserialize_present;
use serde::{Deserialize, Serialize};

pub const SPEECH_LIFECYCLE_SCHEMA_VERSION: &str = "0.1.0";
/// Longest `event_id` / `asset_id` / projection key, in characters. The
/// producer clips an over-long event id at a character boundary: the field is
/// an audit label, and `generation` plus `seq` identify the unit exactly.
pub const MAX_LIFECYCLE_LABEL_CHARS: usize = 256;
/// Largest phrase index a lifecycle record may carry.
pub const MAX_LIFECYCLE_PHRASE: u32 = 4095;
/// Largest JSON-safe integer; keeps the wire form exact in every consumer.
pub const MAX_LIFECYCLE_TIME_MS: u64 = 9_007_199_254_740_991;

/// How a fact was observed (ADR-0004 D-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechTimeBasis {
    /// Derived from the scheduler's logical clock only. Not evidence that
    /// anything was audible.
    LogicalTimeline,
    /// The output adapter accepted the command (a backend-return proxy, not
    /// first-audible time).
    OutputAccepted,
    /// The output adapter reported that playback drained.
    OutputDrained,
}

/// Why a speech unit was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechCancelReason {
    /// Authenticated operator stop.
    OperatorStop,
    /// Operator mute latched, or the command was dropped because muted.
    Muted,
    /// Higher-priority work took the timeline at a declared safe point.
    Preempted,
    /// Queued work displaced by higher-priority work.
    Superseded,
    /// Confirmed barge-in by the user.
    BargeIn,
    /// The scheduler refused the plan (cooldown, priority, broken phrase
    /// continuation).
    SchedulerRejected,
    /// The command was dropped as late or no longer live.
    StaleOrLate,
    /// The generation attempt was cancelled.
    GenerationCancelled,
    /// The output adapter failed.
    OutputFailure,
    /// The runtime is shutting down.
    Shutdown,
    /// The output gate rejected the text.
    GateRejected,
}

/// How far a cancelled unit had got. Computed by the log, never supplied by a
/// reporter, so a cancellation cannot claim a state the unit never reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechReached {
    /// Prepared but never committed to the scheduler.
    BeforeCommit,
    /// Committed (reserved) but not yet started.
    Committed,
    /// Started; `spoken_ms` says how much of it ran.
    Started,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpeechPhase {
    Prepared,
    Committed,
    Started,
    /// A declared safe interrupt point inside a unit was passed.
    Boundary {
        offset_ms: u64,
    },
    Finished,
    Cancelled {
        reason: SpeechCancelReason,
        reached: SpeechReached,
        /// Present exactly when `reached` is `started`: logical milliseconds
        /// between the start and the cancellation.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_present"
        )]
        spoken_ms: Option<u64>,
    },
}

impl SpeechPhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Cancelled { .. })
    }
}

/// Identity of one spoken unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRef {
    /// The scheduler item's event id (phrases after the first derive
    /// `{root}#p{seq}`). Clipped to [`MAX_LIFECYCLE_LABEL_CHARS`].
    pub event_id: String,
    /// The scheduler attempt id; absent until the unit is committed. Distinct
    /// from a generation attempt of the generative pipeline (ADR-0004 D-7):
    /// the phrased reply's response id is not carried here.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub generation: Option<u64>,
    /// Zero-based phrase position. A reply that is not phrased is phrase 0.
    pub phrase: u32,
    /// Whether this is the last phrase of the planned reply.
    pub final_phrase: bool,
    /// The asset whose audio was prepared.
    pub asset_id: String,
    /// SHA-256 (lowercase hex) of the text sent to TTS (`speech_text` when it
    /// differs from the display text, else the display text). Never the text.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub speech_text_digest: Option<String>,
    /// The projection identity (`compatibility.speech_projection`) recorded in
    /// the asset, when it carries one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub projection: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechLifecycleEvent {
    pub schema_version: String,
    /// Per-process monotonic sequence assigned by the log. Ordering is by
    /// `seq`; a gap tells a slow consumer that events were dropped.
    pub seq: u64,
    /// Scheduler logical time of the fact. Not wall-clock time; not globally
    /// monotonic across units (a unit's start may be stamped earlier than a
    /// later-sequenced event of another unit).
    pub at_ms: u64,
    pub basis: SpeechTimeBasis,
    pub speech: SpeechRef,
    pub phase: SpeechPhase,
}

impl SpeechLifecycleEvent {
    /// Mirrors `speech-lifecycle.schema.json`; serde enforces shape and
    /// unknown fields, this enforces ranges, charsets and cross-field rules.
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != SPEECH_LIFECYCLE_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {SPEECH_LIFECYCLE_SCHEMA_VERSION}"),
            ));
        }
        if self.seq > MAX_LIFECYCLE_TIME_MS {
            return Err(DomainValidationError::new(
                "seq",
                "exceeds the JSON-safe integer range",
            ));
        }
        validate_time("at_ms", self.at_ms)?;
        validate_label("speech.event_id", &self.speech.event_id)?;
        validate_label("speech.asset_id", &self.speech.asset_id)?;
        if let Some(generation) = self.speech.generation
            && generation > MAX_LIFECYCLE_TIME_MS
        {
            return Err(DomainValidationError::new(
                "speech.generation",
                "exceeds the JSON-safe integer range",
            ));
        }
        if self.speech.phrase > MAX_LIFECYCLE_PHRASE {
            return Err(DomainValidationError::new(
                "speech.phrase",
                format!("must be at most {MAX_LIFECYCLE_PHRASE}"),
            ));
        }
        if let Some(digest) = self.speech.speech_text_digest.as_deref()
            && !is_lower_hex_sha256(digest)
        {
            return Err(DomainValidationError::new(
                "speech.speech_text_digest",
                "must be 64 lowercase hex characters",
            ));
        }
        if let Some(projection) = self.speech.projection.as_deref() {
            validate_label("speech.projection", projection)?;
        }

        // Cross-field rules (the schema states the same rules with if/then).
        let committed = !matches!(
            self.phase,
            SpeechPhase::Prepared
                | SpeechPhase::Cancelled {
                    reached: SpeechReached::BeforeCommit,
                    ..
                }
        );
        if committed != self.speech.generation.is_some() {
            return Err(DomainValidationError::new(
                "speech.generation",
                "must be present exactly when the unit has been committed",
            ));
        }
        match self.phase {
            SpeechPhase::Boundary { offset_ms } => validate_time("phase.offset_ms", offset_ms)?,
            SpeechPhase::Cancelled {
                reached, spoken_ms, ..
            } => {
                if (reached == SpeechReached::Started) != spoken_ms.is_some() {
                    return Err(DomainValidationError::new(
                        "phase.spoken_ms",
                        "must be present exactly when reached is started",
                    ));
                }
                if let Some(spoken) = spoken_ms {
                    validate_time("phase.spoken_ms", spoken)?;
                }
            }
            _ => {}
        }
        if self.basis != SpeechTimeBasis::LogicalTimeline
            && !matches!(
                self.phase,
                SpeechPhase::Finished | SpeechPhase::Cancelled { .. }
            )
        {
            return Err(DomainValidationError::new(
                "basis",
                "an output-observed basis is only valid on finished and cancelled",
            ));
        }
        Ok(())
    }

    /// Parse and validate one record from bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, DomainValidationError> {
        let event: Self = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("event", error.to_string()))?;
        event.validate()?;
        Ok(event)
    }
}

fn validate_time(field: &'static str, value: u64) -> Result<(), DomainValidationError> {
    if value > MAX_LIFECYCLE_TIME_MS {
        Err(DomainValidationError::new(
            field,
            "exceeds the JSON-safe integer range",
        ))
    } else {
        Ok(())
    }
}

/// 1..=256 characters, no control characters.
fn validate_label(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    let count = value.chars().count();
    if count == 0 || count > MAX_LIFECYCLE_LABEL_CHARS || value.chars().any(char::is_control) {
        return Err(DomainValidationError::new(
            field,
            format!(
                "must be 1..={MAX_LIFECYCLE_LABEL_CHARS} characters without control characters"
            ),
        ));
    }
    Ok(())
}

/// Lowercase-hex SHA-256 shape check (the digest itself is computed by the
/// producer; the domain crate has no hashing dependency).
pub fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Clip a label to [`MAX_LIFECYCLE_LABEL_CHARS`] at a character boundary and
/// replace control characters, so any scheduler event id yields a record that
/// validates.
pub fn clip_lifecycle_label(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .take(MAX_LIFECYCLE_LABEL_CHARS)
        .collect();
    if cleaned.is_empty() {
        "_".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn examples(dir: &str) -> Vec<(String, String)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/speech-lifecycle")
            .join(dir);
        let mut files: Vec<_> = fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("{}: {e}", root.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|path| {
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read_to_string(&path).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn valid_fixtures_parse_and_round_trip() {
        let fixtures = examples("valid");
        assert!(
            fixtures.len() >= 8,
            "one fixture per phase and reason class"
        );
        for (name, text) in fixtures {
            let event = SpeechLifecycleEvent::from_json_bytes(text.as_bytes())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let again: SpeechLifecycleEvent =
                serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
            assert_eq!(event, again, "{name}");
        }
    }

    #[test]
    fn invalid_fixtures_are_rejected_by_serde_or_validation() {
        let fixtures = examples("invalid");
        assert!(fixtures.len() >= 12);
        for (name, text) in fixtures {
            assert!(
                SpeechLifecycleEvent::from_json_bytes(text.as_bytes()).is_err(),
                "{name} must be rejected"
            );
        }
    }

    fn base(phase: SpeechPhase, generation: Option<u64>) -> SpeechLifecycleEvent {
        SpeechLifecycleEvent {
            schema_version: SPEECH_LIFECYCLE_SCHEMA_VERSION.to_owned(),
            seq: 1,
            at_ms: 10,
            basis: SpeechTimeBasis::LogicalTimeline,
            speech: SpeechRef {
                event_id: "evt-1".to_owned(),
                generation,
                phrase: 0,
                final_phrase: true,
                asset_id: "asset-1".to_owned(),
                speech_text_digest: None,
                projection: None,
            },
            phase,
        }
    }

    #[test]
    fn generation_presence_follows_commit() {
        assert!(base(SpeechPhase::Prepared, None).validate().is_ok());
        assert!(base(SpeechPhase::Prepared, Some(1)).validate().is_err());
        for phase in [
            SpeechPhase::Committed,
            SpeechPhase::Started,
            SpeechPhase::Finished,
            SpeechPhase::Boundary { offset_ms: 5 },
        ] {
            assert!(base(phase, Some(1)).validate().is_ok(), "{phase:?}");
            assert!(base(phase, None).validate().is_err(), "{phase:?}");
        }
    }

    #[test]
    fn cancelled_spoken_ms_follows_reached() {
        let cancelled = |reached, spoken_ms, generation| {
            base(
                SpeechPhase::Cancelled {
                    reason: SpeechCancelReason::OperatorStop,
                    reached,
                    spoken_ms,
                },
                generation,
            )
        };
        assert!(
            cancelled(SpeechReached::BeforeCommit, None, None)
                .validate()
                .is_ok()
        );
        assert!(
            cancelled(SpeechReached::BeforeCommit, None, Some(1))
                .validate()
                .is_err()
        );
        assert!(
            cancelled(SpeechReached::Committed, None, Some(1))
                .validate()
                .is_ok()
        );
        assert!(
            cancelled(SpeechReached::Started, Some(40), Some(1))
                .validate()
                .is_ok()
        );
        assert!(
            cancelled(SpeechReached::Started, None, Some(1))
                .validate()
                .is_err()
        );
        assert!(
            cancelled(SpeechReached::Committed, Some(0), Some(1))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn output_basis_is_only_for_terminal_events() {
        let mut event = base(SpeechPhase::Finished, Some(1));
        event.basis = SpeechTimeBasis::OutputDrained;
        assert!(event.validate().is_ok());
        let mut event = base(SpeechPhase::Started, Some(1));
        event.basis = SpeechTimeBasis::OutputAccepted;
        assert!(event.validate().is_err());
    }

    #[test]
    fn labels_digest_and_ranges_are_bounded() {
        let mut event = base(SpeechPhase::Prepared, None);
        event.speech.event_id = "x".repeat(MAX_LIFECYCLE_LABEL_CHARS + 1);
        assert!(event.validate().is_err());
        event.speech.event_id = "line\nbreak".to_owned();
        assert!(event.validate().is_err());
        event.speech.event_id = "ok".to_owned();
        event.speech.speech_text_digest = Some("ABC".to_owned());
        assert!(event.validate().is_err());
        event.speech.speech_text_digest = Some("a".repeat(64));
        assert!(event.validate().is_ok());
        event.at_ms = MAX_LIFECYCLE_TIME_MS + 1;
        assert!(event.validate().is_err());
        assert_eq!(
            clip_lifecycle_label(&"あ".repeat(300)).chars().count(),
            MAX_LIFECYCLE_LABEL_CHARS
        );
        assert!(
            base(SpeechPhase::Prepared, None)
                .speech
                .event_id
                .chars()
                .count()
                > 0
        );
        assert_eq!(clip_lifecycle_label("a\nb"), "a_b");
    }
}
