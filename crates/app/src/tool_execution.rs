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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Authoritative task state sampled after a tool execution returns.
pub struct ToolCommitStateSnapshot {
    pub observed_generation: u64,
    pub cancelled: bool,
}

/// Execute an authorized tool and verify its bounded result against task state
/// re-read after the adapter await completes.
pub async fn execute_verified_tool<F>(
    adapter: &dyn ToolAdapter,
    authority: &AuthenticatedControl,
    action: ToolAction,
    expected_generation: u64,
    post_execution_state: F,
    rule: &ToolVerificationRule,
) -> Result<ToolExecutionEvidence, EngineError>
where
    F: FnOnce() -> ToolCommitStateSnapshot,
{
    let expected_tool = action.tool.clone();
    let expected_operation = action.operation.clone();
    let authorized = authorize_tool_action(authority, action)?;
    let result = adapter.execute(&authorized).await?;
    let current = post_execution_state();
    let fence = ToolCommitFence {
        expected_generation,
        observed_generation: current.observed_generation,
        cancelled: current.cancelled,
    };
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

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

    struct DelayedFixtureAdapter {
        started: Arc<AtomicBool>,
    }

    impl ToolAdapter for DelayedFixtureAdapter {
        fn discover<'a>(&'a self) -> EngineFuture<'a, Vec<ToolDescriptor>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn execute<'a>(
            &'a self,
            action: &'a aivtuber_domain::AuthorizedToolAction,
        ) -> EngineFuture<'a, ToolExecutionResult> {
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                started.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(25)).await;
                ToolExecutionResult::new(
                    action.action().tool.clone(),
                    action.action().tool.clone(),
                    action.action().operation.clone(),
                    ToolResultClass::Success,
                    Some(json!({ "value": 42 })),
                    None,
                    25,
                )
            })
        }
    }

    fn current_state(generation: u64, cancelled: bool) -> ToolCommitStateSnapshot {
        ToolCommitStateSnapshot {
            observed_generation: generation,
            cancelled,
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
            3,
            || current_state(3, false),
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
            3,
            || current_state(3, false),
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
            1,
            || current_state(1, false),
            &ToolVerificationRule::JsonEquals {
                pointer: "/value".to_owned(),
                expected: json!(42),
            },
        )
        .await
        .expect_err("authorization must fail");

        assert_eq!(error.kind, EngineErrorKind::Unauthorized);
    }

    #[tokio::test]
    async fn generation_change_during_execution_is_stale() {
        let started = Arc::new(AtomicBool::new(false));
        let adapter = DelayedFixtureAdapter {
            started: Arc::clone(&started),
        };
        let authoritative_state = Arc::new(Mutex::new(current_state(7, false)));
        let state_for_verification = Arc::clone(&authoritative_state);
        let authority = authenticated(Capability::ToolGrant);
        let rule = ToolVerificationRule::JsonEquals {
            pointer: "/value".to_owned(),
            expected: json!(42),
        };

        let execution = execute_verified_tool(
            &adapter,
            &authority,
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            7,
            move || {
                *state_for_verification
                    .lock()
                    .expect("authoritative state lock")
            },
            &rule,
        );
        let mutation = async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            authoritative_state
                .lock()
                .expect("authoritative state lock")
                .observed_generation = 8;
        };

        let (evidence, ()) = tokio::join!(execution, mutation);
        let evidence = evidence.expect("tool execution remains observable when stale");

        assert_eq!(evidence.verification, ToolVerificationOutcome::Stale);
        assert!(!evidence.is_completed());
    }

    #[tokio::test]
    async fn cancellation_during_execution_is_cancelled() {
        let started = Arc::new(AtomicBool::new(false));
        let adapter = DelayedFixtureAdapter {
            started: Arc::clone(&started),
        };
        let authoritative_state = Arc::new(Mutex::new(current_state(7, false)));
        let state_for_verification = Arc::clone(&authoritative_state);
        let authority = authenticated(Capability::ToolGrant);
        let rule = ToolVerificationRule::JsonEquals {
            pointer: "/value".to_owned(),
            expected: json!(42),
        };

        let execution = execute_verified_tool(
            &adapter,
            &authority,
            ToolAction {
                tool: "fixture".to_owned(),
                operation: "read".to_owned(),
                arguments: BTreeMap::new(),
            },
            7,
            move || {
                *state_for_verification
                    .lock()
                    .expect("authoritative state lock")
            },
            &rule,
        );
        let mutation = async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            authoritative_state
                .lock()
                .expect("authoritative state lock")
                .cancelled = true;
        };

        let (evidence, ()) = tokio::join!(execution, mutation);
        let evidence = evidence.expect("tool execution remains observable when cancelled");

        assert_eq!(evidence.verification, ToolVerificationOutcome::Cancelled);
        assert!(!evidence.is_completed());
    }
}
