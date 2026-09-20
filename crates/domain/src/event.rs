use crate::DomainValidationError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const EVENT_SCHEMA_VERSION: &str = "0.2.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceClass {
    PublicChat,
    Donation,
    Speech,
    Game,
    Stream,
    Timer,
    Operator,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityPlane {
    Content,
    Control,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Untrusted,
    SemiTrusted,
    Trusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "chat.message")]
    ChatMessage,
    #[serde(rename = "chat.donation")]
    ChatDonation,
    #[serde(rename = "speech.input")]
    SpeechInput,
    #[serde(rename = "game.event")]
    GameEvent,
    #[serde(rename = "stream.event")]
    StreamEvent,
    #[serde(rename = "timer.tick")]
    TimerTick,
    #[serde(rename = "operator.command")]
    OperatorCommand,
    #[serde(rename = "system.health")]
    SystemHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "performer.mute")]
    PerformerMute,
    #[serde(rename = "performer.stop")]
    PerformerStop,
    #[serde(rename = "obs.control")]
    ObsControl,
    #[serde(rename = "avatar.control")]
    AvatarControl,
    #[serde(rename = "memory.admin")]
    MemoryAdmin,
    #[serde(rename = "tool.grant")]
    ToolGrant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationMethod {
    OperatorUi,
    OperatorHotkey,
    SignedLocalApi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationContext {
    pub principal: String,
    pub method: AuthorizationMethod,
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: String,
    pub event_id: String,
    pub correlation_id: String,
    pub sequence: u64,
    pub observed_at: String,
    pub source: String,
    pub source_class: SourceClass,
    pub plane: SecurityPlane,
    pub trust_level: TrustLevel,
    pub kind: EventKind,
    pub actor_id: Option<String>,
    pub priority_hint: Option<f64>,
    pub authorization: Option<AuthorizationContext>,
    pub payload: BTreeMap<String, serde_json::Value>,
}

impl EventEnvelope {
    /// Validate schema version, trust classification, and authorization invariants.
    pub fn validate(&self) -> Result<(), DomainValidationError> {
        if self.schema_version != EVENT_SCHEMA_VERSION {
            return Err(DomainValidationError::new(
                "schema_version",
                format!("expected {EVENT_SCHEMA_VERSION}"),
            ));
        }
        require_non_empty("event_id", &self.event_id)?;
        require_non_empty("correlation_id", &self.correlation_id)?;
        require_non_empty("source", &self.source)?;

        if let Some(priority) = self.priority_hint {
            validate_unit_interval("priority_hint", priority)?;
        }

        match self.kind {
            EventKind::ChatMessage => self.validate_unprivileged(
                SourceClass::PublicChat,
                SecurityPlane::Content,
                &[TrustLevel::Untrusted, TrustLevel::SemiTrusted],
            ),
            EventKind::ChatDonation => self.validate_unprivileged(
                SourceClass::Donation,
                SecurityPlane::Content,
                &[TrustLevel::Untrusted, TrustLevel::SemiTrusted],
            ),
            EventKind::SpeechInput => self.validate_unprivileged(
                SourceClass::Speech,
                SecurityPlane::Content,
                &[TrustLevel::Untrusted, TrustLevel::SemiTrusted],
            ),
            EventKind::GameEvent => self.validate_unprivileged(
                SourceClass::Game,
                SecurityPlane::Content,
                &[TrustLevel::SemiTrusted],
            ),
            EventKind::StreamEvent => self.validate_unprivileged(
                SourceClass::Stream,
                SecurityPlane::System,
                &[TrustLevel::SemiTrusted, TrustLevel::Trusted],
            ),
            EventKind::TimerTick => self.validate_unprivileged(
                SourceClass::Timer,
                SecurityPlane::System,
                &[TrustLevel::Trusted],
            ),
            EventKind::SystemHealth => self.validate_unprivileged(
                SourceClass::System,
                SecurityPlane::System,
                &[TrustLevel::Trusted],
            ),
            EventKind::OperatorCommand => self.validate_operator_command(),
        }
    }

    fn validate_unprivileged(
        &self,
        source_class: SourceClass,
        plane: SecurityPlane,
        trust: &[TrustLevel],
    ) -> Result<(), DomainValidationError> {
        if self.source_class != source_class {
            return Err(DomainValidationError::new(
                "source_class",
                format!("invalid source class for {:?}", self.kind),
            ));
        }
        if self.plane != plane {
            return Err(DomainValidationError::new(
                "plane",
                format!("invalid security plane for {:?}", self.kind),
            ));
        }
        if !trust.contains(&self.trust_level) {
            return Err(DomainValidationError::new(
                "trust_level",
                format!("invalid trust level for {:?}", self.kind),
            ));
        }
        if self.authorization.is_some() {
            return Err(DomainValidationError::new(
                "authorization",
                "authorization is forbidden outside operator.command",
            ));
        }
        Ok(())
    }

    fn validate_operator_command(&self) -> Result<(), DomainValidationError> {
        if self.source_class != SourceClass::Operator {
            return Err(DomainValidationError::new(
                "source_class",
                "operator.command requires source_class=operator",
            ));
        }
        if self.plane != SecurityPlane::Control {
            return Err(DomainValidationError::new(
                "plane",
                "operator.command requires plane=control",
            ));
        }
        if self.trust_level != TrustLevel::Trusted {
            return Err(DomainValidationError::new(
                "trust_level",
                "operator.command requires trust_level=trusted",
            ));
        }

        let authorization = self.authorization.as_ref().ok_or_else(|| {
            DomainValidationError::new(
                "authorization",
                "operator.command requires authorization",
            )
        })?;
        require_non_empty("authorization.principal", &authorization.principal)?;
        if authorization.capabilities.is_empty() {
            return Err(DomainValidationError::new(
                "authorization.capabilities",
                "at least one capability is required",
            ));
        }
        Ok(())
    }
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    if value.trim().is_empty() {
        Err(DomainValidationError::new(field, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_unit_interval(
    field: &'static str,
    value: f64,
) -> Result<(), DomainValidationError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            "must be a finite value in 0..=1",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_event() -> EventEnvelope {
        EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: "evt-1".to_owned(),
            correlation_id: "corr-1".to_owned(),
            sequence: 1,
            observed_at: "2026-09-20T10:00:00Z".to_owned(),
            source: "example-chat".to_owned(),
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

    #[test]
    fn valid_chat_round_trips() {
        let event = chat_event();
        event.validate().expect("valid chat event");

        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: EventEnvelope = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.kind, EventKind::ChatMessage);
        decoded.validate().expect("round-trip remains valid");
    }

    #[test]
    fn forged_operator_from_public_chat_is_rejected() {
        let mut event = chat_event();
        event.kind = EventKind::OperatorCommand;
        event.plane = SecurityPlane::Control;
        event.trust_level = TrustLevel::Trusted;
        event.authorization = Some(AuthorizationContext {
            principal: "viewer:test".to_owned(),
            method: AuthorizationMethod::OperatorHotkey,
            capabilities: BTreeSet::from([Capability::PerformerStop]),
        });

        let error = event.validate().expect_err("must reject forged operator");
        assert_eq!(error.field(), "source_class");
    }

    #[test]
    fn operator_command_requires_authorization() {
        let mut event = chat_event();
        event.source = "local-hotkey".to_owned();
        event.source_class = SourceClass::Operator;
        event.plane = SecurityPlane::Control;
        event.trust_level = TrustLevel::Trusted;
        event.kind = EventKind::OperatorCommand;

        let error = event.validate().expect_err("authorization is required");
        assert_eq!(error.field(), "authorization");
    }
}
