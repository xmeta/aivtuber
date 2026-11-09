//! Provider-neutral speech observations (issue #92, slice S1).
//!
//! These are *observations*, not decisions and not authority. They describe
//! what a VAD or turn-end provider saw about one pseudonymous speaker. The wire
//! form has no source class, plane, trust, priority or authorization field on
//! purpose: the receiving ingress assigns those, and a microphone speaker is
//! always untrusted content-plane input (ADR-0005 S-7). There is no audio,
//! PCM or transcript text here.
//!
//! Normative wire form: `schemas/speech-observation.schema.json`. Floor
//! decisions (`FloorAction`) belong to #81; this module only exposes the
//! per-speaker activity state and the deterministic silence fallback #81
//! consumes.

use crate::DomainValidationError;
use serde::{Deserialize, Deserializer, Serialize};

pub const SPEECH_OBSERVATION_SCHEMA_VERSION: &str = "0.1.0";
/// Upper bound on observations in one batch.
pub const MAX_SPEECH_OBSERVATIONS_PER_BATCH: usize = 32;
/// Upper bound on one serialized batch, enforced by the ingress before parsing.
pub const MAX_SPEECH_OBSERVATION_BATCH_BYTES: usize = 16 * 1024;
pub const MAX_SPEAKER_ID_LEN: usize = 64;
pub const MAX_PROVIDER_TOKEN_LEN: usize = 64;
pub const MAX_MODEL_ID_LEN: usize = 128;
pub const MAX_LANGUAGE_LEN: usize = 35;
/// Largest JSON-safe integer; keeps the wire form exact in every consumer.
pub const MAX_OBSERVATION_TIME_MS: u64 = 9_007_199_254_740_991;
/// Longest `expires_at_ms - observed_at_ms` an observation may declare.
pub const MAX_OBSERVATION_VALIDITY_MS: u64 = 60_000;
pub const MAX_PREDICTION_HORIZON_MS: u64 = 10_000;

/// Raw speech-activity edge. No debounce or minimum-speech filter sits behind
/// it (ADR-0005): a 32 ms fragment is still an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechEdge {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechActivityObservation {
    pub speaker_id: String,
    pub edge: SpeechEdge,
    pub observed_at_ms: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub expires_at_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub confidence: Option<f64>,
}

/// A provider *score* for turn end, never a probability unless `calibrated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnEndScoreObservation {
    pub speaker_id: String,
    pub observed_at_ms: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub expires_at_ms: Option<u64>,
    pub provider: String,
    pub model_id: String,
    /// Defaults to false. A consumer must not read an uncalibrated score as a
    /// probability (EXP-0001 11.1).
    #[serde(default)]
    pub calibrated: bool,
    pub score: f64,
    /// Present only when the provider predicts ahead; absent for a nowcast.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub prediction_horizon_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpeechObservation {
    SpeechActivity(SpeechActivityObservation),
    TurnEndScore(TurnEndScoreObservation),
}

impl SpeechObservation {
    pub fn speaker_id(&self) -> &str {
        match self {
            Self::SpeechActivity(o) => &o.speaker_id,
            Self::TurnEndScore(o) => &o.speaker_id,
        }
    }

    pub fn observed_at_ms(&self) -> u64 {
        match self {
            Self::SpeechActivity(o) => o.observed_at_ms,
            Self::TurnEndScore(o) => o.observed_at_ms,
        }
    }

    pub fn expires_at_ms(&self) -> Option<u64> {
        match self {
            Self::SpeechActivity(o) => o.expires_at_ms,
            Self::TurnEndScore(o) => o.expires_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechObservationBatch {
    pub schema_version: String,
    pub provider_id: String,
    pub provider_version: String,
    /// Declared by configuration, never inferred.
    pub language: String,
    pub observations: Vec<SpeechObservation>,
}

impl SpeechObservationBatch {
    /// Mirrors `speech-observation.schema.json`; serde enforces shape and
    /// unknown fields, this enforces ranges, lengths and charsets.
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != SPEECH_OBSERVATION_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {SPEECH_OBSERVATION_SCHEMA_VERSION}"),
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
        if self.observations.is_empty()
            || self.observations.len() > MAX_SPEECH_OBSERVATIONS_PER_BATCH
        {
            return Err(DomainValidationError::new(
                "observations",
                format!("must contain 1..={MAX_SPEECH_OBSERVATIONS_PER_BATCH} observations"),
            ));
        }
        for observation in &self.observations {
            validate_observation(observation)?;
        }
        Ok(())
    }

    /// Parse and validate one batch from bytes. The caller bounds the byte
    /// length first (`MAX_SPEECH_OBSERVATION_BATCH_BYTES`).
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, DomainValidationError> {
        let batch: Self = serde_json::from_slice(bytes)
            .map_err(|error| DomainValidationError::new("batch", error.to_string()))?;
        batch.validate()?;
        Ok(batch)
    }
}

fn validate_observation(observation: &SpeechObservation) -> Result<(), DomainValidationError> {
    let (speaker_id, observed_at_ms, expires_at_ms) = (
        observation.speaker_id(),
        observation.observed_at_ms(),
        observation.expires_at_ms(),
    );
    validate_charset("speaker_id", speaker_id, MAX_SPEAKER_ID_LEN, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
    })?;
    if observed_at_ms > MAX_OBSERVATION_TIME_MS {
        return Err(DomainValidationError::new(
            "observed_at_ms",
            "exceeds the JSON-safe integer range",
        ));
    }
    if let Some(expires) = expires_at_ms
        && (expires <= observed_at_ms || expires - observed_at_ms > MAX_OBSERVATION_VALIDITY_MS)
    {
        return Err(DomainValidationError::new(
            "expires_at_ms",
            format!("must be after observed_at_ms and within {MAX_OBSERVATION_VALIDITY_MS} ms"),
        ));
    }
    match observation {
        SpeechObservation::SpeechActivity(activity) => {
            if let Some(confidence) = activity.confidence {
                validate_unit("confidence", confidence)?;
            }
        }
        SpeechObservation::TurnEndScore(turn_end) => {
            validate_provider_token("provider", &turn_end.provider)?;
            validate_charset("model_id", &turn_end.model_id, MAX_MODEL_ID_LEN, |b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'+' | b'/' | b'-')
            })?;
            validate_unit("score", turn_end.score)?;
            if let Some(horizon) = turn_end.prediction_horizon_ms
                && !(1..=MAX_PREDICTION_HORIZON_MS).contains(&horizon)
            {
                return Err(DomainValidationError::new(
                    "prediction_horizon_ms",
                    format!("must be in 1..={MAX_PREDICTION_HORIZON_MS}"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_unit(field: &'static str, value: f64) -> Result<(), DomainValidationError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DomainValidationError::new(field, "must be in 0..=1"))
    }
}

/// ASCII token: first byte lowercase alphanumeric, rest `[a-z0-9._-]`.
fn validate_provider_token(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    let lower = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    validate_charset(field, value, MAX_PROVIDER_TOKEN_LEN, |b| {
        lower(b) || matches!(b, b'.' | b'_' | b'-')
    })?;
    if value.as_bytes().first().is_some_and(|b| lower(*b)) {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            "must start with a lowercase letter or digit",
        ))
    }
}

/// Length in 1..=max, ASCII only, every byte allowed, first byte alphanumeric.
fn validate_charset(
    field: &'static str,
    value: &str,
    max: usize,
    allowed: impl Fn(u8) -> bool,
) -> Result<(), DomainValidationError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > max {
        return Err(DomainValidationError::new(
            field,
            format!("length must be 1..={max}"),
        ));
    }
    if !bytes[0].is_ascii_alphanumeric() || !bytes.iter().copied().all(allowed) {
        return Err(DomainValidationError::new(
            field,
            "contains characters outside the allowed set",
        ));
    }
    Ok(())
}

/// `^[a-z]{2,3}(-[A-Za-z0-9]{2,8}){0,3}$`, at most 35 bytes.
fn validate_language(value: &str) -> Result<(), DomainValidationError> {
    let invalid = || DomainValidationError::new("language", "must be a BCP 47-style language tag");
    if value.len() > MAX_LANGUAGE_LEN {
        return Err(invalid());
    }
    let mut parts = value.split('-');
    let primary = parts.next().ok_or_else(invalid)?;
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(invalid());
    }
    let mut subtags = 0;
    for part in parts {
        subtags += 1;
        if subtags > 3
            || !(2..=8).contains(&part.len())
            || !part.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Reject an explicit JSON `null` for an optional field, as the schema does.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Whether a speaker is currently speaking, by the last accepted raw edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerActivity {
    Active,
    Inactive,
}

/// Bounded per-speaker activity state: the current activity and the time of
/// the edge that established it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakerActivityState {
    pub activity: SpeakerActivity,
    pub last_edge_ms: u64,
}

/// Configuration of the deterministic silence fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackTurnConfig {
    /// Silence after the last `end` edge before the speaker is turn-end
    /// eligible. About 800 ms (EXP-0001 section 7: 4 percent false turn-takes
    /// on synthetic holds, p50 lag about 870 ms). Measured on synthetic pauses;
    /// to be re-measured on real dialogue before it is trusted.
    pub silence_ms: u64,
    /// A speaker still `Active` this long after the last edge is reported as
    /// `ActivityStale`: a lost `end` edge must not pin the floor forever.
    pub max_active_ms: u64,
}

impl Default for FallbackTurnConfig {
    fn default() -> Self {
        Self {
            silence_ms: 800,
            max_active_ms: 60_000,
        }
    }
}

impl FallbackTurnConfig {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if !(1..=10_000).contains(&self.silence_ms) {
            return Err(DomainValidationError::new(
                "silence_ms",
                "must be in 1..=10000",
            ));
        }
        if !(1_000..=600_000).contains(&self.max_active_ms) {
            return Err(DomainValidationError::new(
                "max_active_ms",
                "must be in 1000..=600000",
            ));
        }
        Ok(())
    }
}

/// What the silence heuristic says about one speaker right now. This is an
/// input to #81, not a floor decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum FallbackTurnSignal {
    /// Nothing observed for this speaker.
    NoSpeech,
    SpeakerActive,
    /// The last `active` edge is older than `max_active_ms`: a lost `end`
    /// edge or a stalled feed; do not treat the speaker as speaking.
    ActivityStale,
    /// Silent, but not yet for `silence_ms`.
    Waiting {
        silence_ms: u64,
    },
    /// Silent for at least `silence_ms`.
    TurnEndEligible {
        silence_ms: u64,
    },
}

impl FallbackTurnSignal {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::NoSpeech => "no_speech",
            Self::SpeakerActive => "speaker_active",
            Self::ActivityStale => "activity_stale",
            Self::Waiting { .. } => "waiting",
            Self::TurnEndEligible { .. } => "turn_end_eligible",
        }
    }
}

/// Pure deterministic silence heuristic: `(state, now) -> signal`. Used when no
/// turn-end provider is present or the language is unsupported. It reads only
/// its arguments, so replay needs no clock, microphone or model.
pub fn fallback_turn_signal(
    state: Option<&SpeakerActivityState>,
    now_ms: u64,
    config: &FallbackTurnConfig,
) -> FallbackTurnSignal {
    let Some(state) = state else {
        return FallbackTurnSignal::NoSpeech;
    };
    let elapsed = now_ms.saturating_sub(state.last_edge_ms);
    match state.activity {
        SpeakerActivity::Active if elapsed > config.max_active_ms => {
            FallbackTurnSignal::ActivityStale
        }
        SpeakerActivity::Active => FallbackTurnSignal::SpeakerActive,
        SpeakerActivity::Inactive if elapsed >= config.silence_ms => {
            FallbackTurnSignal::TurnEndEligible {
                silence_ms: elapsed,
            }
        }
        SpeakerActivity::Inactive => FallbackTurnSignal::Waiting {
            silence_ms: elapsed,
        },
    }
}

/// Why the deterministic fallback, not a provider turn-end score, applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndFallbackReason {
    /// No turn-end score has been observed for the speaker.
    NoTurnEndProvider,
    /// The declared language is not in the turn-end providers' supported set.
    UnsupportedLanguage,
    /// The last score is stale or predates the current silence.
    TurnEndStale,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(json: &str) -> Result<SpeechObservationBatch, DomainValidationError> {
        SpeechObservationBatch::from_json_bytes(json.as_bytes())
    }

    const HEAD: &str = r#""schema_version":"0.1.0","provider_id":"silero-vad","provider_version":"6.2.3","language":"ja""#;

    fn one(observation: &str) -> String {
        format!(r#"{{{HEAD},"observations":[{observation}]}}"#)
    }

    #[test]
    fn minimal_activity_and_score_parse_with_defaults() {
        let parsed = batch(&one(
            r#"{"kind":"turn_end_score","speaker_id":"s1","observed_at_ms":5,"provider":"p","model_id":"m","score":0.5}"#,
        ))
        .expect("valid");
        let SpeechObservation::TurnEndScore(score) = &parsed.observations[0] else {
            panic!("expected score");
        };
        assert!(!score.calibrated, "calibrated defaults to false");
        assert_eq!(score.prediction_horizon_ms, None);
        let value = serde_json::to_value(&parsed).expect("serialize");
        assert!(
            value["observations"][0]
                .get("prediction_horizon_ms")
                .is_none()
        );
    }

    #[test]
    fn rejects_every_boundary_violation() {
        let cases = [
            // unknown field, e.g. a self-asserted trust level
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"trust_level":"trusted"}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"priority_hint":1.0}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"transcript":"hi"}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"confidence":null}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"middle","observed_at_ms":1}"#,
            r#"{"kind":"speech_activity","speaker_id":"","edge":"start","observed_at_ms":1}"#,
            r#"{"kind":"speech_activity","speaker_id":"has space","edge":"start","observed_at_ms":1}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"confidence":1.5}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"expires_at_ms":1}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1,"expires_at_ms":60002}"#,
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":9007199254740992}"#,
            r#"{"kind":"turn_end_score","speaker_id":"s1","observed_at_ms":1,"provider":"p","model_id":"m","score":1.01}"#,
            r#"{"kind":"turn_end_score","speaker_id":"s1","observed_at_ms":1,"provider":"p","model_id":"m","score":0.5,"prediction_horizon_ms":0}"#,
            r#"{"kind":"turn_end_score","speaker_id":"s1","observed_at_ms":1,"provider":"P","model_id":"m","score":0.5}"#,
            r#"{"kind":"turn_end_score","speaker_id":"s1","observed_at_ms":1,"provider":"p","model_id":"m","score":0.5,"calibrated":"yes"}"#,
            r#"{"kind":"unknown","speaker_id":"s1","observed_at_ms":1}"#,
        ];
        for case in cases {
            assert!(batch(&one(case)).is_err(), "must reject: {case}");
        }
    }

    #[test]
    fn rejects_bad_batch_head_and_size() {
        let obs =
            r#"{"kind":"speech_activity","speaker_id":"s1","edge":"start","observed_at_ms":1}"#;
        for head in [
            r#""schema_version":"0.2.0","provider_id":"a","provider_version":"1","language":"ja""#,
            r#""schema_version":"0.1.0","provider_id":"A","provider_version":"1","language":"ja""#,
            r#""schema_version":"0.1.0","provider_id":"a","provider_version":"","language":"ja""#,
            r#""schema_version":"0.1.0","provider_id":"a","provider_version":"1","language":"JA""#,
            r#""schema_version":"0.1.0","provider_id":"a","provider_version":"1","language":"ja-a""#,
        ] {
            assert!(batch(&format!(r#"{{{head},"observations":[{obs}]}}"#)).is_err());
        }
        assert!(batch(&format!(r#"{{{HEAD},"observations":[]}}"#)).is_err());
        let many = vec![obs; MAX_SPEECH_OBSERVATIONS_PER_BATCH + 1].join(",");
        assert!(batch(&format!(r#"{{{HEAD},"observations":[{many}]}}"#)).is_err());
        let max = vec![obs; MAX_SPEECH_OBSERVATIONS_PER_BATCH].join(",");
        assert!(batch(&format!(r#"{{{HEAD},"observations":[{max}]}}"#)).is_ok());
        assert!(batch(&format!(r#"{{{HEAD},"observations":[{obs}],"x":1}}"#)).is_err());
        for lang in ["ja", "en-US", "zh-Hant-TW"] {
            assert!(validate_language(lang).is_ok(), "{lang}");
        }
    }

    /// Fixture -> Rust parity: the same files `bun run validate` checks against
    /// the schema must be accepted/rejected identically by the Rust validator.
    #[test]
    fn schema_fixtures_agree_with_the_rust_validator() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/speech-observations");
        for (dir, should_pass) in [("valid", true), ("invalid", false), ("inconsistent", false)] {
            let mut count = 0;
            for entry in std::fs::read_dir(base.join(dir)).expect("fixture dir") {
                let path = entry.expect("entry").path();
                let bytes = std::fs::read(&path).expect("read");
                assert_eq!(
                    SpeechObservationBatch::from_json_bytes(&bytes).is_ok(),
                    should_pass,
                    "{}",
                    path.display()
                );
                count += 1;
            }
            assert!(count > 0, "{dir} has fixtures");
        }
    }

    #[test]
    fn fallback_signal_is_a_pure_function_of_state_and_now() {
        let config = FallbackTurnConfig::default();
        assert_eq!(fallback_turn_signal(None, 0, &config).name(), "no_speech");
        let active = SpeakerActivityState {
            activity: SpeakerActivity::Active,
            last_edge_ms: 1_000,
        };
        assert_eq!(
            fallback_turn_signal(Some(&active), 1_000_000 - 1, &config),
            FallbackTurnSignal::ActivityStale
        );
        assert_eq!(
            fallback_turn_signal(Some(&active), 1_500, &config),
            FallbackTurnSignal::SpeakerActive
        );
        let ended = SpeakerActivityState {
            activity: SpeakerActivity::Inactive,
            last_edge_ms: 1_000,
        };
        assert_eq!(
            fallback_turn_signal(Some(&ended), 1_799, &config),
            FallbackTurnSignal::Waiting { silence_ms: 799 }
        );
        assert_eq!(
            fallback_turn_signal(Some(&ended), 1_800, &config),
            FallbackTurnSignal::TurnEndEligible { silence_ms: 800 }
        );
        // `now` before the edge (clock skew) never underflows.
        assert_eq!(
            fallback_turn_signal(Some(&ended), 10, &config),
            FallbackTurnSignal::Waiting { silence_ms: 0 }
        );
        assert!(config.validate().is_ok());
        assert!(
            FallbackTurnConfig {
                silence_ms: 0,
                ..config
            }
            .validate()
            .is_err()
        );
    }
}
