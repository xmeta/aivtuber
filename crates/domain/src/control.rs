use crate::{
    AuthorizationContext, AuthorizationMethod, Capability, EVENT_SCHEMA_VERSION, EventEnvelope,
    EventKind, SecurityPlane, SourceClass, TrustLevel,
};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

pub const CONTROL_SECRET_LEN: usize = 32;

/// Opaque local-session secret. Debug output is always redacted.
pub struct ControlSecret([u8; CONTROL_SECRET_LEN]);

impl ControlSecret {
    pub fn new(bytes: [u8; CONTROL_SECRET_LEN]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for ControlSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlSecret([REDACTED])")
    }
}

impl Drop for ControlSecret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorCommandInput {
    pub event_id: String,
    pub correlation_id: String,
    pub sequence: u64,
    pub observed_at: String,
    pub action: String,
    pub payload: BTreeMap<String, serde_json::Value>,
}

/// Non-serializable proof of authenticated local control authority.
///
/// Fields are private and there is no public constructor. The only production
/// minting path is LocalControlIngress::authenticate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedControl {
    principal: String,
    method: AuthorizationMethod,
    capabilities: BTreeSet<Capability>,
}

impl AuthenticatedControl {
    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn method(&self) -> AuthorizationMethod {
        self.method
    }
    pub fn capabilities(&self) -> &BTreeSet<Capability> {
        &self.capabilities
    }

    pub fn has_capability(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Authenticated command plus its non-serializable authority token.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthenticatedControlCommand {
    event: EventEnvelope,
    authority: AuthenticatedControl,
}

impl AuthenticatedControlCommand {
    pub fn event(&self) -> &EventEnvelope {
        &self.event
    }

    pub fn authority(&self) -> &AuthenticatedControl {
        &self.authority
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlIngressError {
    InvalidConfiguration(&'static str),
    AuthenticationFailed,
    ActionNotAllowlisted,
    CapabilityNotGranted(Capability),
    InvalidCommand(String),
}

impl fmt::Display for ControlIngressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(field) => {
                write!(f, "invalid local control configuration: {field}")
            }
            Self::AuthenticationFailed => f.write_str("local control authentication failed"),
            Self::ActionNotAllowlisted => f.write_str("control action is not allowlisted"),
            Self::CapabilityNotGranted(capability) => {
                write!(f, "local control capability is not granted: {capability:?}")
            }
            Self::InvalidCommand(message) => write!(f, "invalid control command: {message}"),
        }
    }
}

impl Error for ControlIngressError {}

/// Trusted local ingress that verifies an opaque session secret before
/// creating control authority. Serialized authorization metadata alone cannot
/// produce AuthenticatedControl.
pub struct LocalControlIngress {
    source: String,
    principal: String,
    method: AuthorizationMethod,
    capabilities: BTreeSet<Capability>,
    secret: ControlSecret,
}

impl fmt::Debug for LocalControlIngress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalControlIngress")
            .field("source", &self.source)
            .field("principal", &self.principal)
            .field("method", &self.method)
            .field("capabilities", &self.capabilities)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl LocalControlIngress {
    pub fn new(
        source: impl Into<String>,
        principal: impl Into<String>,
        method: AuthorizationMethod,
        capabilities: BTreeSet<Capability>,
        secret: ControlSecret,
    ) -> Result<Self, ControlIngressError> {
        let source = source.into();
        let principal = principal.into();
        if source.trim().is_empty() {
            return Err(ControlIngressError::InvalidConfiguration("source"));
        }
        if principal.trim().is_empty() {
            return Err(ControlIngressError::InvalidConfiguration("principal"));
        }
        if capabilities.is_empty() {
            return Err(ControlIngressError::InvalidConfiguration("capabilities"));
        }

        Ok(Self {
            source,
            principal,
            method,
            capabilities,
            secret,
        })
    }

    pub fn authenticate(
        &self,
        mut input: OperatorCommandInput,
        presented_secret: &[u8; CONTROL_SECRET_LEN],
    ) -> Result<AuthenticatedControlCommand, ControlIngressError> {
        if !constant_time_eq(&self.secret.0, presented_secret) {
            return Err(ControlIngressError::AuthenticationFailed);
        }

        let required = capability_for_action(&input.action)
            .ok_or(ControlIngressError::ActionNotAllowlisted)?;
        if !self.capabilities.contains(&required) {
            return Err(ControlIngressError::CapabilityNotGranted(required));
        }

        input
            .payload
            .insert("action".to_owned(), serde_json::Value::String(input.action));
        let scoped_capabilities = BTreeSet::from([required]);
        let event = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION.to_owned(),
            event_id: input.event_id,
            correlation_id: input.correlation_id,
            sequence: input.sequence,
            observed_at: input.observed_at,
            source: self.source.clone(),
            source_class: SourceClass::Operator,
            plane: SecurityPlane::Control,
            trust_level: TrustLevel::Trusted,
            kind: EventKind::OperatorCommand,
            actor_id: None,
            priority_hint: None,
            authorization: Some(AuthorizationContext {
                principal: self.principal.clone(),
                method: self.method,
                capabilities: scoped_capabilities.clone(),
            }),
            payload: input.payload,
        };
        event
            .validate()
            .map_err(|error| ControlIngressError::InvalidCommand(error.to_string()))?;

        Ok(AuthenticatedControlCommand {
            event,
            authority: AuthenticatedControl {
                principal: self.principal.clone(),
                method: self.method,
                capabilities: scoped_capabilities,
            },
        })
    }
}

fn capability_for_action(action: &str) -> Option<Capability> {
    match action {
        "stop" | "performer.stop" => Some(Capability::PerformerStop),
        "mute" | "performer.mute" => Some(Capability::PerformerMute),
        "obs.control" => Some(Capability::ObsControl),
        "avatar.control" => Some(Capability::AvatarControl),
        "memory.admin" => Some(Capability::MemoryAdmin),
        "tool.grant" => Some(Capability::ToolGrant),
        _ => None,
    }
}

fn constant_time_eq(
    expected: &[u8; CONTROL_SECRET_LEN],
    presented: &[u8; CONTROL_SECRET_LEN],
) -> bool {
    let mut difference = 0_u8;
    for (left, right) in expected.iter().zip(presented.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingress(capability: Capability, secret: [u8; CONTROL_SECRET_LEN]) -> LocalControlIngress {
        LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([capability]),
            ControlSecret::new(secret),
        )
        .expect("valid ingress")
    }

    fn input(action: &str) -> OperatorCommandInput {
        OperatorCommandInput {
            event_id: "evt-control".to_owned(),
            correlation_id: "corr-control".to_owned(),
            sequence: 1,
            observed_at: "2026-09-24T00:00:00Z".to_owned(),
            action: action.to_owned(),
            payload: BTreeMap::new(),
        }
    }

    #[test]
    fn wrong_secret_cannot_mint_authenticated_control() {
        let expected = [7_u8; CONTROL_SECRET_LEN];
        let presented = [8_u8; CONTROL_SECRET_LEN];
        let ingress = ingress(Capability::PerformerStop, expected);

        let error = ingress
            .authenticate(input("performer.stop"), &presented)
            .expect_err("wrong secret must fail");

        assert_eq!(error, ControlIngressError::AuthenticationFailed);
        assert!(!error.to_string().contains("070707"));
        assert!(!format!("{ingress:?}").contains("7, 7"));
    }

    #[test]
    fn successful_ingress_mints_only_scoped_authenticated_capability() {
        let secret = [9_u8; CONTROL_SECRET_LEN];
        let ingress = LocalControlIngress::new(
            "local-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([Capability::PerformerStop, Capability::ObsControl]),
            ControlSecret::new(secret),
        )
        .expect("valid ingress");

        let command = ingress
            .authenticate(input("performer.stop"), &secret)
            .expect("authenticated");

        assert!(
            command
                .authority()
                .has_capability(Capability::PerformerStop)
        );
        assert!(!command.authority().has_capability(Capability::ObsControl));
        assert_eq!(command.event().kind, EventKind::OperatorCommand);
        assert_eq!(
            command
                .event()
                .authorization
                .as_ref()
                .expect("audit claim")
                .capabilities,
            BTreeSet::from([Capability::PerformerStop])
        );
    }

    #[test]
    fn unknown_actions_are_rejected_before_authority_is_minted() {
        let secret = [3_u8; CONTROL_SECRET_LEN];
        let ingress = ingress(Capability::PerformerStop, secret);

        let error = ingress
            .authenticate(input("shell.exec"), &secret)
            .expect_err("unknown action must fail");

        assert_eq!(error, ControlIngressError::ActionNotAllowlisted);
    }

    #[test]
    fn control_secret_debug_never_exposes_bytes() {
        let secret = ControlSecret::new([0xab; CONTROL_SECRET_LEN]);
        let debug = format!("{secret:?}");

        assert_eq!(debug, "ControlSecret([REDACTED])");
        assert!(!debug.contains("171"));
        assert!(!debug.contains("ab"));
    }
}
