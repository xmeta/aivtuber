use crate::{DomainValidationError, EventEnvelope, EventKind, SourceClass, TrustLevel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const REFLEX_REQUEST_SCHEMA_VERSION: &str = "0.1.0";
pub const REFLEX_CONTEXT_SCHEMA_VERSION: &str = "0.1.0";
pub const THINKING_REQUEST_SCHEMA_VERSION: &str = "0.1.0";

pub const MAX_SNAPSHOT_STRING_BYTES: usize = 256;
pub const MAX_RECENT_ITEMS: usize = 8;
pub const MAX_RETRIEVAL_CANDIDATES: usize = 8;
pub const MAX_THINKING_CONTEXT_ITEMS: usize = 12;
pub const MAX_THINKING_TEXT_BYTES: usize = 4 * 1024;
pub const MAX_THINKING_CONTEXT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformerSnapshot {
    pub currently_speaking: bool,
    pub current_asset_id: Option<String>,
    pub current_interruptible: bool,
    pub mood_valence: Option<f64>,
    pub arousal: Option<f64>,
}

impl PerformerSnapshot {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        validate_optional_label(
            "performer.current_asset_id",
            self.current_asset_id.as_deref(),
        )?;
        if let Some(value) = self.mood_valence {
            validate_range("performer.mood_valence", value, -1.0, 1.0)?;
        }
        if let Some(value) = self.arousal {
            validate_range("performer.arousal", value, 0.0, 1.0)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSnapshot {
    pub mode: Option<String>,
    pub topic: Option<String>,
}

impl StreamSnapshot {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        validate_optional_label("stream.mode", self.mode.as_deref())?;
        validate_optional_label("stream.topic", self.topic.as_deref())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentInteractionSnapshot {
    pub reaction_families: Vec<String>,
    pub asset_ids: Vec<String>,
    pub seconds_since_direct_reply: Option<f64>,
}

impl RecentInteractionSnapshot {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        validate_bounded_labels(
            "recent.reaction_families",
            &self.reaction_families,
            MAX_RECENT_ITEMS,
        )?;
        validate_bounded_labels("recent.asset_ids", &self.asset_ids, MAX_RECENT_ITEMS)?;
        if let Some(value) = self.seconds_since_direct_reply
            && (!value.is_finite() || value < 0.0)
        {
            return Err(DomainValidationError::new(
                "recent.seconds_since_direct_reply",
                "must be finite and non-negative",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalCandidateContext {
    pub asset_id: String,
    pub rank: u32,
    pub similarity: f64,
}

impl RetrievalCandidateContext {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        validate_label("retrieval.candidates.asset_id", &self.asset_id)?;
        if self.rank == 0 {
            return Err(DomainValidationError::new(
                "retrieval.candidates.rank",
                "must be greater than zero",
            ));
        }
        validate_range(
            "retrieval.candidates.similarity",
            self.similarity,
            -1.0,
            1.0,
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalSnapshot {
    pub candidates: Vec<RetrievalCandidateContext>,
}

impl RetrievalSnapshot {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.candidates.len() > MAX_RETRIEVAL_CANDIDATES {
            return Err(DomainValidationError::new(
                "retrieval.candidates",
                format!("must contain at most {MAX_RETRIEVAL_CANDIDATES} candidates"),
            ));
        }

        let mut ids = BTreeSet::new();
        for (index, candidate) in self.candidates.iter().enumerate() {
            candidate.validate()?;
            if !ids.insert(candidate.asset_id.as_str()) {
                return Err(DomainValidationError::new(
                    "retrieval.candidates",
                    "asset ids must be unique",
                ));
            }
            let expected_rank = (index + 1) as u32;
            if candidate.rank != expected_rank {
                return Err(DomainValidationError::new(
                    "retrieval.candidates.rank",
                    format!("must be contiguous and ordered from 1; expected {expected_rank}"),
                ));
            }
        }
        Ok(())
    }

    pub fn candidate_asset_ids(&self) -> impl Iterator<Item = &str> {
        self.candidates
            .iter()
            .map(|candidate| candidate.asset_id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflexContext {
    pub schema_version: String,
    pub performer: PerformerSnapshot,
    pub stream: StreamSnapshot,
    pub recent: RecentInteractionSnapshot,
    pub now_ms: Option<u64>,
}

impl Default for ReflexContext {
    fn default() -> Self {
        Self {
            schema_version: REFLEX_CONTEXT_SCHEMA_VERSION.to_owned(),
            performer: PerformerSnapshot::default(),
            stream: StreamSnapshot::default(),
            recent: RecentInteractionSnapshot::default(),
            now_ms: None,
        }
    }
}

impl ReflexContext {
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != REFLEX_CONTEXT_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "context.schema_version",
                format!("expected {REFLEX_CONTEXT_SCHEMA_VERSION}"),
            ));
        }
        self.performer.validate()?;
        self.stream.validate()?;
        self.recent.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflexRequest {
    pub schema_version: String,
    pub event: EventEnvelope,
    pub context: ReflexContext,
    pub retrieval: RetrievalSnapshot,
}

impl ReflexRequest {
    pub fn new(event: EventEnvelope, context: ReflexContext) -> Self {
        Self {
            schema_version: REFLEX_REQUEST_SCHEMA_VERSION.to_owned(),
            event,
            context,
            retrieval: RetrievalSnapshot::default(),
        }
    }

    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != REFLEX_REQUEST_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "reflex_request.schema_version",
                format!("expected {REFLEX_REQUEST_SCHEMA_VERSION}"),
            ));
        }
        self.event.validate()?;
        self.context.validate()?;
        self.retrieval.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    Public,
    Pseudonymous,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingContextSource {
    CurrentEvent,
    RecentInteraction,
    Memory,
    StreamState,
    OperatorCurated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingContent {
    pub source: ThinkingContextSource,
    pub event_id: Option<String>,
    pub event_kind: Option<EventKind>,
    pub source_class: Option<SourceClass>,
    pub trust_level: TrustLevel,
    pub privacy: PrivacyClass,
    pub text: String,
}

impl ThinkingContent {
    pub fn from_event(
        event: &EventEnvelope,
        text: impl Into<String>,
        privacy: PrivacyClass,
    ) -> Self {
        Self {
            source: ThinkingContextSource::CurrentEvent,
            event_id: Some(event.event_id.clone()),
            event_kind: Some(event.kind),
            source_class: Some(event.source_class),
            trust_level: event.trust_level,
            privacy,
            text: text.into(),
        }
    }

    pub fn validate(&self, field: &'static str) -> Result<(), DomainValidationError> {
        if self.text.is_empty() || self.text.len() > MAX_THINKING_TEXT_BYTES {
            return Err(DomainValidationError::new(
                field,
                format!("text must contain 1..={MAX_THINKING_TEXT_BYTES} UTF-8 bytes"),
            ));
        }
        validate_optional_label(field, self.event_id.as_deref())?;

        if self.source == ThinkingContextSource::CurrentEvent
            && (self.event_id.is_none() || self.event_kind.is_none() || self.source_class.is_none())
        {
            return Err(DomainValidationError::new(
                field,
                "current_event content requires event_id, event_kind, and source_class",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingRequest {
    pub schema_version: String,
    pub input: ThinkingContent,
    pub context: ReflexContext,
    pub retrieval: RetrievalSnapshot,
    pub curated_context: Vec<ThinkingContent>,
}

impl ThinkingRequest {
    pub fn from_event(
        event: &EventEnvelope,
        text: impl Into<String>,
        privacy: PrivacyClass,
        context: ReflexContext,
        retrieval: RetrievalSnapshot,
        curated_context: Vec<ThinkingContent>,
    ) -> Self {
        Self {
            schema_version: THINKING_REQUEST_SCHEMA_VERSION.to_owned(),
            input: ThinkingContent::from_event(event, text, privacy),
            context,
            retrieval,
            curated_context,
        }
    }

    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != THINKING_REQUEST_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "thinking_request.schema_version",
                format!("expected {THINKING_REQUEST_SCHEMA_VERSION}"),
            ));
        }
        if self.input.source != ThinkingContextSource::CurrentEvent {
            return Err(DomainValidationError::new(
                "thinking_request.input.source",
                "must be current_event",
            ));
        }
        self.input.validate("thinking_request.input")?;
        self.context.validate()?;
        self.retrieval.validate()?;

        if self.curated_context.len() > MAX_THINKING_CONTEXT_ITEMS {
            return Err(DomainValidationError::new(
                "thinking_request.curated_context",
                format!("must contain at most {MAX_THINKING_CONTEXT_ITEMS} items"),
            ));
        }

        let mut total_bytes = self.input.text.len();
        for item in &self.curated_context {
            item.validate("thinking_request.curated_context")?;
            if item.source == ThinkingContextSource::CurrentEvent {
                return Err(DomainValidationError::new(
                    "thinking_request.curated_context",
                    "current_event belongs only in input",
                ));
            }
            total_bytes = total_bytes.saturating_add(item.text.len());
        }
        if total_bytes > MAX_THINKING_CONTEXT_BYTES {
            return Err(DomainValidationError::new(
                "thinking_request",
                format!(
                    "curated text must contain at most {MAX_THINKING_CONTEXT_BYTES} UTF-8 bytes"
                ),
            ));
        }
        Ok(())
    }
}

fn validate_bounded_labels(
    field: &'static str,
    values: &[String],
    max_items: usize,
) -> Result<(), DomainValidationError> {
    if values.len() > max_items {
        return Err(DomainValidationError::new(
            field,
            format!("must contain at most {max_items} items"),
        ));
    }
    let mut unique = BTreeSet::new();
    for value in values {
        validate_label(field, value)?;
        if !unique.insert(value.as_str()) {
            return Err(DomainValidationError::new(field, "items must be unique"));
        }
    }
    Ok(())
}

fn validate_optional_label(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), DomainValidationError> {
    if let Some(value) = value {
        validate_label(field, value)?;
    }
    Ok(())
}

fn validate_label(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    if value.is_empty() || value.len() > MAX_SNAPSHOT_STRING_BYTES {
        return Err(DomainValidationError::new(
            field,
            format!("must contain 1..={MAX_SNAPSHOT_STRING_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(())
}

fn validate_range(
    field: &'static str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), DomainValidationError> {
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            format!("must be finite and in {min}..={max}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EVENT_SCHEMA_VERSION, SecurityPlane};
    use std::collections::BTreeMap;

    fn chat_event() -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: "evt-context".to_owned(),
            correlation_id: "corr-context".to_owned(),
            sequence: 1,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            source: "test-chat".to_owned(),
            source_class: SourceClass::PublicChat,
            plane: SecurityPlane::Content,
            trust_level: TrustLevel::Untrusted,
            kind: EventKind::ChatMessage,
            actor_id: Some("viewer:test".to_owned()),
            priority_hint: None,
            authorization: None,
            payload: BTreeMap::from([(
                "text".to_owned(),
                serde_json::Value::String("hello".to_owned()),
            )]),
        }
    }

    fn candidate(rank: u32, asset_id: &str) -> RetrievalCandidateContext {
        RetrievalCandidateContext {
            asset_id: asset_id.to_owned(),
            rank,
            similarity: 0.9 - f64::from(rank.saturating_sub(1)) * 0.01,
        }
    }

    fn thinking_request() -> ThinkingRequest {
        ThinkingRequest::from_event(
            &chat_event(),
            "hello",
            PrivacyClass::Pseudonymous,
            ReflexContext::default(),
            RetrievalSnapshot {
                candidates: vec![candidate(1, "asset.a")],
            },
            vec![ThinkingContent {
                source: ThinkingContextSource::RecentInteraction,
                event_id: Some("evt-previous".to_owned()),
                event_kind: Some(EventKind::ChatMessage),
                source_class: Some(SourceClass::PublicChat),
                trust_level: TrustLevel::Untrusted,
                privacy: PrivacyClass::Pseudonymous,
                text: "previous public interaction summary".to_owned(),
            }],
        )
    }

    #[test]
    fn reflex_request_round_trip_records_exact_schema_versions_without_free_form_state() {
        let request = ReflexRequest::new(chat_event(), ReflexContext::default());
        request.validate().expect("valid reflex request");

        let encoded = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(encoded["schema_version"], REFLEX_REQUEST_SCHEMA_VERSION);
        assert_eq!(
            encoded["context"]["schema_version"],
            REFLEX_CONTEXT_SCHEMA_VERSION
        );
        assert!(encoded.get("state").is_none());
        assert!(encoded.get("candidate_asset_ids").is_none());
        assert!(encoded.get("transcript").is_none());

        let decoded: ReflexRequest = serde_json::from_value(encoded).expect("deserialize request");
        decoded.validate().expect("round trip remains valid");
        assert_eq!(decoded, request);
    }

    #[test]
    fn snapshot_strings_and_recent_lists_are_bounded() {
        let too_long = "x".repeat(MAX_SNAPSHOT_STRING_BYTES + 1);
        let performer = PerformerSnapshot {
            current_asset_id: Some(too_long),
            ..PerformerSnapshot::default()
        };
        assert_eq!(
            performer.validate().expect_err("asset id bound").field(),
            "performer.current_asset_id"
        );

        let recent = RecentInteractionSnapshot {
            reaction_families: (0..=MAX_RECENT_ITEMS)
                .map(|index| format!("reaction-{index}"))
                .collect(),
            ..RecentInteractionSnapshot::default()
        };
        assert_eq!(
            recent.validate().expect_err("recent bound").field(),
            "recent.reaction_families"
        );
    }

    #[test]
    fn retrieval_context_is_bounded_unique_and_rank_ordered() {
        let too_many = RetrievalSnapshot {
            candidates: (1..=(MAX_RETRIEVAL_CANDIDATES + 1))
                .map(|rank| candidate(rank as u32, &format!("asset.{rank}")))
                .collect(),
        };
        assert_eq!(
            too_many.validate().expect_err("candidate bound").field(),
            "retrieval.candidates"
        );

        let duplicate = RetrievalSnapshot {
            candidates: vec![candidate(1, "asset.a"), candidate(2, "asset.a")],
        };
        assert_eq!(
            duplicate.validate().expect_err("duplicate asset").field(),
            "retrieval.candidates"
        );

        let skipped_rank = RetrievalSnapshot {
            candidates: vec![candidate(1, "asset.a"), candidate(3, "asset.b")],
        };
        assert_eq!(
            skipped_rank.validate().expect_err("rank order").field(),
            "retrieval.candidates.rank"
        );
    }

    #[test]
    fn optional_snapshot_fields_round_trip_as_none() {
        let context = ReflexContext::default();
        let encoded = serde_json::to_string(&context).expect("serialize");
        let decoded: ReflexContext = serde_json::from_str(&encoded).expect("deserialize");

        decoded.validate().expect("default context");
        assert_eq!(decoded.performer.current_asset_id, None);
        assert_eq!(decoded.performer.mood_valence, None);
        assert_eq!(decoded.stream.mode, None);
        assert_eq!(decoded.recent.seconds_since_direct_reply, None);
        assert_eq!(decoded.now_ms, None);
    }

    #[test]
    fn thinking_request_preserves_untrusted_source_and_explicit_privacy() {
        let request = thinking_request();
        request.validate().expect("valid thinking request");

        assert_eq!(request.input.trust_level, TrustLevel::Untrusted);
        assert_eq!(request.input.privacy, PrivacyClass::Pseudonymous);
        assert_eq!(request.input.source, ThinkingContextSource::CurrentEvent);
        assert!(
            request
                .curated_context
                .iter()
                .all(|item| item.source != ThinkingContextSource::CurrentEvent)
        );

        let encoded = serde_json::to_value(&request).expect("serialize");
        assert_eq!(encoded["schema_version"], THINKING_REQUEST_SCHEMA_VERSION);
        assert_eq!(encoded["input"]["trust_level"], "untrusted");
        assert_eq!(encoded["input"]["privacy"], "pseudonymous");
        assert!(encoded.get("history").is_none());
        assert!(encoded.get("transcript").is_none());
    }

    #[test]
    fn thinking_text_and_curated_context_are_bounded() {
        let mut request = thinking_request();
        request.input.text = "x".repeat(MAX_THINKING_TEXT_BYTES + 1);
        assert_eq!(
            request.validate().expect_err("input text bound").field(),
            "thinking_request.input"
        );

        let mut request = thinking_request();
        request.curated_context = (0..=MAX_THINKING_CONTEXT_ITEMS)
            .map(|index| ThinkingContent {
                source: ThinkingContextSource::Memory,
                event_id: None,
                event_kind: None,
                source_class: None,
                trust_level: TrustLevel::SemiTrusted,
                privacy: PrivacyClass::Private,
                text: format!("memory-{index}"),
            })
            .collect();
        assert_eq!(
            request.validate().expect_err("curated item bound").field(),
            "thinking_request.curated_context"
        );

        let mut request = thinking_request();
        request.input.text = "i".repeat(MAX_THINKING_TEXT_BYTES);
        request.curated_context = (0..2)
            .map(|_| ThinkingContent {
                source: ThinkingContextSource::Memory,
                event_id: None,
                event_kind: None,
                source_class: None,
                trust_level: TrustLevel::SemiTrusted,
                privacy: PrivacyClass::Private,
                text: "m".repeat(MAX_THINKING_TEXT_BYTES / 2),
            })
            .collect();
        request.validate().expect("exact aggregate bound");
        request.curated_context[1].text.push('x');
        assert_eq!(
            request
                .validate()
                .expect_err("aggregate byte bound")
                .field(),
            "thinking_request"
        );
    }

    #[test]
    fn thinking_request_rejects_unversioned_or_misplaced_current_event_context() {
        let mut request = thinking_request();
        request.schema_version = "9.9.9".to_owned();
        assert_eq!(
            request.validate().expect_err("version").field(),
            "thinking_request.schema_version"
        );

        let mut request = thinking_request();
        request.curated_context.push(ThinkingContent::from_event(
            &chat_event(),
            "duplicate current event",
            PrivacyClass::Public,
        ));
        assert_eq!(
            request
                .validate()
                .expect_err("current event must not be duplicated")
                .field(),
            "thinking_request.curated_context"
        );
    }
}
