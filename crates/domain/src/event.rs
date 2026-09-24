use crate::DomainValidationError;
use serde::{Deserialize, Deserializer, Serialize, de};
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

/// Serializable authorization claim carried for validation/replay/audit.
///
/// This value is data, not proof of authenticated ingress. Privileged runtime
/// actions require a non-serializable `AuthenticatedControl` minted by the
/// trusted local ingress path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationContext {
    pub principal: String,
    pub method: AuthorizationMethod,
    #[serde(deserialize_with = "deserialize_unique_capabilities")]
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_authorization"
    )]
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
        validate_date_time("observed_at", &self.observed_at)?;
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
            DomainValidationError::new("authorization", "operator.command requires authorization")
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

fn deserialize_unique_capabilities<'de, D>(
    deserializer: D,
) -> Result<BTreeSet<Capability>, D::Error>
where
    D: Deserializer<'de>,
{
    let capabilities = Vec::<Capability>::deserialize(deserializer)?;
    let mut unique = BTreeSet::new();
    for capability in capabilities {
        if !unique.insert(capability) {
            return Err(de::Error::custom(
                "authorization capabilities must be unique",
            ));
        }
    }
    Ok(unique)
}

fn deserialize_present_authorization<'de, D>(
    deserializer: D,
) -> Result<Option<AuthorizationContext>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<AuthorizationContext>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("authorization must be an object when present"))
}

fn validate_date_time(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    if is_json_schema_date_time(value) {
        Ok(())
    } else {
        Err(DomainValidationError::new(
            field,
            "must be an RFC 3339 date-time",
        ))
    }
}

fn is_json_schema_date_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }

    let Some(year) = parse_digits(bytes, 0, 4) else {
        return false;
    };
    let Some(month) = parse_digits(bytes, 5, 2) else {
        return false;
    };
    let Some(day) = parse_digits(bytes, 8, 2) else {
        return false;
    };
    let Some(hour) = parse_digits(bytes, 11, 2) else {
        return false;
    };
    let Some(minute) = parse_digits(bytes, 14, 2) else {
        return false;
    };
    let Some(second) = parse_digits(bytes, 17, 2) else {
        return false;
    };

    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
        || (second == 60 && (hour != 23 || minute != 59))
    {
        return false;
    }

    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == start {
            return false;
        }
    }

    match bytes.get(index) {
        Some(b'Z' | b'z') => index + 1 == bytes.len(),
        Some(b'+' | b'-') => {
            if index + 6 != bytes.len() || bytes.get(index + 3) != Some(&b':') {
                return false;
            }
            let Some(offset_hour) = parse_digits(bytes, index + 1, 2) else {
                return false;
            };
            let Some(offset_minute) = parse_digits(bytes, index + 4, 2) else {
                return false;
            };
            offset_hour <= 23 && offset_minute <= 59
        }
        _ => false,
    }
}

fn parse_digits(bytes: &[u8], start: usize, len: usize) -> Option<u32> {
    let slice = bytes.get(start..start + len)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    slice.iter().try_fold(0_u32, |value, digit| {
        value
            .checked_mul(10)?
            .checked_add(u32::from(digit.saturating_sub(b'0')))
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), DomainValidationError> {
    if value.is_empty() {
        Err(DomainValidationError::new(field, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_unit_interval(field: &'static str, value: f64) -> Result<(), DomainValidationError> {
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
    use std::fs;
    use std::path::{Path, PathBuf};

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

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

    fn assert_valid_round_trip(event: EventEnvelope) {
        event
            .validate()
            .expect("representative event must be valid");

        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: EventEnvelope = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded, event);
        decoded.validate().expect("round-trip remains valid");
    }

    #[test]
    fn representative_chat_event_round_trips() {
        assert_valid_round_trip(chat_event());
    }

    #[test]
    fn representative_donation_event_round_trips() {
        let mut event = chat_event();
        event.event_id = "evt-donation-1".to_owned();
        event.source = "example-donation".to_owned();
        event.source_class = SourceClass::Donation;
        event.trust_level = TrustLevel::SemiTrusted;
        event.kind = EventKind::ChatDonation;
        event.payload = BTreeMap::from([
            (
                "amount".to_owned(),
                serde_json::Value::Number(serde_json::Number::from(500)),
            ),
            (
                "currency".to_owned(),
                serde_json::Value::String("JPY".to_owned()),
            ),
            (
                "message".to_owned(),
                serde_json::Value::String("応援しています".to_owned()),
            ),
        ]);

        assert_valid_round_trip(event);
    }

    #[test]
    fn representative_game_event_round_trips() {
        let mut event = chat_event();
        event.event_id = "evt-game-1".to_owned();
        event.source = "example-game".to_owned();
        event.source_class = SourceClass::Game;
        event.trust_level = TrustLevel::SemiTrusted;
        event.kind = EventKind::GameEvent;
        event.actor_id = None;
        event.priority_hint = Some(0.8);
        event.payload = BTreeMap::from([
            (
                "event".to_owned(),
                serde_json::Value::String("boss.spawn".to_owned()),
            ),
            (
                "boss".to_owned(),
                serde_json::Value::String("example-boss".to_owned()),
            ),
        ]);

        assert_valid_round_trip(event);
    }

    #[test]
    fn representative_timer_event_round_trips() {
        let mut event = chat_event();
        event.event_id = "evt-timer-1".to_owned();
        event.source = "runtime-timer".to_owned();
        event.source_class = SourceClass::Timer;
        event.plane = SecurityPlane::System;
        event.trust_level = TrustLevel::Trusted;
        event.kind = EventKind::TimerTick;
        event.actor_id = None;
        event.payload = BTreeMap::from([
            (
                "timer_id".to_owned(),
                serde_json::Value::String("heartbeat".to_owned()),
            ),
            (
                "elapsed_ms".to_owned(),
                serde_json::Value::Number(serde_json::Number::from(1_000)),
            ),
        ]);

        assert_valid_round_trip(event);
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
    fn repository_chat_fixture_matches_domain_contract() {
        let json = include_str!("../../../examples/events/chat-message.json");
        let event: EventEnvelope = serde_json::from_str(json).expect("deserialize chat fixture");
        event.validate().expect("chat fixture must remain valid");
    }

    #[test]
    fn repository_operator_fixture_matches_domain_contract() {
        let json = include_str!("../../../examples/events/operator-stop.json");
        let event: EventEnvelope =
            serde_json::from_str(json).expect("deserialize operator fixture");
        event
            .validate()
            .expect("operator fixture must remain valid");
    }

    #[test]
    fn repository_forged_operator_fixture_is_rejected() {
        let json = include_str!("../../../examples/events/invalid/forged-operator-command.json");
        let event: EventEnvelope =
            serde_json::from_str(json).expect("deserialize negative fixture");
        let error = event.validate().expect_err("forged operator must fail");
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

    #[test]
    fn every_repository_event_kind_fixture_deserializes_and_validates() {
        let files = [
            "chat-message.json",
            "chat-donation.json",
            "speech-input.json",
            "game-event.json",
            "stream-event.json",
            "timer-tick.json",
            "operator-stop.json",
            "system-health.json",
        ];
        let mut kinds = BTreeSet::new();

        for file in files {
            let path = repository_root().join("examples/events").join(file);
            let json = fs::read_to_string(&path).expect("read event fixture");
            let event: EventEnvelope =
                serde_json::from_str(&json).expect("schema-valid fixture must deserialize");
            event.validate().expect("fixture must pass Rust validation");
            kinds.insert(format!("{:?}", event.kind));
        }

        assert_eq!(kinds.len(), 8);
    }

    #[test]
    fn repository_negative_event_fixtures_are_rejected_by_rust() {
        let root = repository_root().join("examples/events/invalid");
        let mut files = fs::read_dir(root)
            .expect("negative fixture directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        files.sort();

        assert!(!files.is_empty());
        for path in files {
            let json = fs::read_to_string(&path).expect("read negative fixture");
            let rejected = match serde_json::from_str::<EventEnvelope>(&json) {
                Ok(event) => event.validate().is_err(),
                Err(_) => true,
            };
            assert!(
                rejected,
                "negative event fixture unexpectedly accepted: {}",
                path.display()
            );
        }
    }

    #[test]
    fn json_schema_date_time_examples_match_expected_acceptance() {
        for value in [
            "2026-09-24T10:00:00Z",
            "2026-09-24t10:00:00z",
            "2026-09-24 10:00:00+09:30",
            "2026-09-24T10:00:00.125Z",
            "2016-12-31T23:59:60Z",
        ] {
            assert!(is_json_schema_date_time(value), "{value}");
        }
        for value in [
            "2026-02-29T00:00:00Z",
            "2026-09-24T10:00:00",
            "2026-09-24T24:00:00Z",
            "2026-09-24T23:58:60Z",
            "2026-09-24T10:00:00+24:00",
        ] {
            assert!(!is_json_schema_date_time(value), "{value}");
        }
    }
}
