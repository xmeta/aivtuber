use aivtuber_domain::{
    AuthenticatedControl, EngineError, ToolAction, ToolAdapter, ToolCommitFence,
    ToolExecutionResult, ToolResultClass, ToolVerificationOutcome, ToolVerificationRule,
    authorize_tool_action, verify_tool_result,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolExecutionEvidence {
    pub result: ToolExecutionResult,
    pub verification: ToolVerificationOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionAuditRecord {
    pub server: String,
    pub tool: String,
    pub operation: String,
    pub result_class: ToolResultClass,
    pub schema_fingerprint: Option<String>,
    pub latency_ms: u64,
    pub verification: ToolVerificationOutcome,
}

impl ToolExecutionEvidence {
    pub fn is_completed(&self) -> bool {
        self.verification.is_verified()
    }

    pub fn audit_record(&self) -> ToolExecutionAuditRecord {
        ToolExecutionAuditRecord {
            server: self.result.server().to_owned(),
            tool: self.result.tool().to_owned(),
            operation: self.result.operation().to_owned(),
            result_class: self.result.class(),
            schema_fingerprint: self.result.schema_fingerprint().map(str::to_owned),
            latency_ms: self.result.latency_ms(),
            verification: self.verification,
        }
    }
}

pub async fn execute_verified_tool(
    adapter: &dyn ToolAdapter,
    authority: &AuthenticatedControl,
    action: ToolAction,
    fence: ToolCommitFence,
    rule: &ToolVerificationRule,
) -> Result<ToolExecutionEvidence, EngineError> {
    let expected_tool = action.tool.clone();
    let expected_operation = action.operation.clone();
    let authorized = authorize_tool_action(authority, action)?;
    let result = adapter.execute(&authorized).await?;
    let verification =
        verify_tool_result(&result, &expected_tool, &expected_operation, fence, rule);
    Ok(ToolExecutionEvidence {
        result,
        verification,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_domain::{
        AuthorizationMethod, Capability, ControlSecret, EngineErrorKind, EngineFuture,
        LocalControlIngress, OperatorCommandInput, ToolDescriptor, ToolResultClass,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    struct FixtureAdapter;

    impl ToolAdapter for FixtureAdapter {
        fn discover<'a>(&'a self) -> EngineFuture<'a, Vec<ToolDescriptor>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn execute<'a>(
            &'a self,
            action: &'a aivtuber_domain::AuthorizedToolAction,
        ) -> EngineFuture<'a, ToolExecutionResult> {
            Box::pin(async move {
                ToolExecutionResult::new(
                    action.action().tool.clone(),
                    action.action().tool.clone(),
                    action.action().operation.clone(),
                    ToolResultClass::Success,
                    Some(json!({ "value": 42 })),
                    None,
                    1,
                )
            })
        }
    }

    fn authenticated(capability: Capability) -> AuthenticatedControl {
        let action = match capability {
            Capability::PerformerMute => "performer.mute",
            Capability::PerformerStop => "performer.stop",
            Capability::ObsControl => "obs.control",
            Capability::AvatarControl => "avatar.control",
            Capability::MemoryAdmin => "memory.admin",
            Capability::ToolGrant => "tool.grant",
        };
        let secret = [13_u8; 32];
        let ingress = LocalControlIngress::new(
            "tool-execution-test",
            "operator:test",
            AuthorizationMethod::OperatorHotkey,
            BTreeSet::from([capability]),
            ControlSecret::new(secret),
        )
        .expect("trusted ingress");
        ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-tool".to_owned(),
                    correlation_id: "corr-tool".to_owned(),
                    sequence: 1,
                    observed_at: "2026-10-05T00:00:00Z".to_owned(),
                    action: action.to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone()
    }

    #[tokio::test]
    async fn authorized_tool_result_requires_postcondition_verification() {
        let evidence = execute_verified_tool(
            &FixtureAdapter,
            &authenticated(Capability::ToolGrant),
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            ToolCommitFence {
                expected_generation: 3,
                observed_generation: 3,
                cancelled: false,
            },
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(42),
            },
        )
        .await
        .expect("execution succeeds");

        assert_eq!(evidence.verification, ToolVerificationOutcome::Verified);
        assert!(evidence.is_completed());
        let audit = evidence.audit_record();
        assert_eq!(audit.tool, "fixture");
        assert_eq!(audit.operation, "read");
        assert_eq!(audit.result_class, ToolResultClass::Success);
        assert_eq!(audit.verification, ToolVerificationOutcome::Verified);
    }

    #[tokio::test]
    async fn verification_failure_is_not_reported_as_completed() {
        let evidence = execute_verified_tool(
            &FixtureAdapter,
            &authenticated(Capability::ToolGrant),
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            ToolCommitFence {
                expected_generation: 3,
                observed_generation: 3,
                cancelled: false,
            },
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(7),
            },
        )
        .await
        .expect("tool execution remains observable when verification fails");

        assert_eq!(evidence.verification, ToolVerificationOutcome::NotProven);
        assert!(!evidence.is_completed());
    }

    #[tokio::test]
    async fn missing_tool_capability_fails_before_adapter_execution() {
        let error = execute_verified_tool(
            &FixtureAdapter,
            &authenticated(Capability::AvatarControl),
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            ToolCommitFence {
                expected_generation: 1,
                observed_generation: 1,
                cancelled: false,
            },
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(42),
            },
        )
        .await
        .expect_err("authorization must fail");

        assert_eq!(error.kind, EngineErrorKind::Unauthorized);
    }
}
