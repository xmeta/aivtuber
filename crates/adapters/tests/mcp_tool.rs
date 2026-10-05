use aivtuber_adapters::RmcpToolAdapter;
use aivtuber_domain::{
    AuthorizationMethod, Capability, ControlSecret, EngineErrorKind, LocalControlIngress,
    OperatorCommandInput, ToolAction, ToolAdapter, ToolCommitFence, ToolResultClass,
    ToolVerificationOutcome, ToolVerificationRule, authorize_tool_action, verify_tool_result,
};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolResult, ContentBlock},
    service::RunningService,
    tool, tool_router,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

struct FixtureServer;

#[tool_router(server_handler)]
impl FixtureServer {
    #[tool(description = "Read one deterministic fixture value")]
    async fn read_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "value=42",
        )]))
    }

    #[tool(description = "Return a tool-level failure")]
    async fn fail_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "fixture failed",
        )]))
    }

    #[tool(description = "Return content this adapter cannot normalize")]
    async fn image_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::image(
            "AAAA",
            "image/png",
        )]))
    }

    #[tool(description = "Return an oversized observation")]
    async fn oversized_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "x".repeat(40 * 1024),
        )]))
    }

    #[tool(description = "Sleep long enough to exercise the adapter deadline")]
    async fn delayed_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(CallToolResult::success(vec![ContentBlock::text("late")]))
    }
}

async fn adapter_pair(timeout: Duration) -> (RmcpToolAdapter, RunningService<RoleClient, ()>) {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    tokio::spawn(async move {
        let service = FixtureServer
            .serve(server_transport)
            .await
            .expect("fixture server starts");
        service.waiting().await.expect("fixture server completes");
    });
    let client = ().serve(client_transport).await.expect("client starts");
    let adapter = RmcpToolAdapter::new("fixture", client.peer().clone(), timeout);
    (adapter, client)
}

fn authenticated(capability: Capability) -> aivtuber_domain::AuthenticatedControl {
    let action = match capability {
        Capability::PerformerMute => "performer.mute",
        Capability::PerformerStop => "performer.stop",
        Capability::ObsControl => "obs.control",
        Capability::AvatarControl => "avatar.control",
        Capability::MemoryAdmin => "memory.admin",
        Capability::ToolGrant => "tool.grant",
    };
    let secret = [11_u8; 32];
    let ingress = LocalControlIngress::new(
        "local-tool-test",
        "operator:test",
        AuthorizationMethod::OperatorHotkey,
        BTreeSet::from([capability]),
        ControlSecret::new(secret),
    )
    .expect("trusted ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-tool-test".to_owned(),
                correlation_id: "corr-tool-test".to_owned(),
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

fn authorized(operation: &str) -> aivtuber_domain::AuthorizedToolAction {
    authorize_tool_action(
        &authenticated(Capability::ToolGrant),
        ToolAction {
            tool: "fixture".to_owned(),
            operation: operation.to_owned(),
            arguments: BTreeMap::new(),
        },
    )
    .expect("tool action authorized")
}

#[tokio::test]
async fn discovers_invokes_and_verifies_read_only_tool() {
    let (adapter, _client) = adapter_pair(Duration::from_secs(1)).await;

    let tools = adapter.discover().await.expect("tools discovered");
    let descriptor = tools
        .iter()
        .find(|tool| tool.name() == "read_fixture")
        .expect("fixture tool advertised");
    assert_eq!(
        descriptor.description(),
        Some("Read one deterministic fixture value")
    );
    assert_eq!(descriptor.schema_fingerprint().len(), 64);

    let result = adapter
        .execute(&authorized("read_fixture"))
        .await
        .expect("tool executes");
    assert_eq!(result.class(), ToolResultClass::Success);
    assert_eq!(result.server(), "fixture");
    assert_eq!(result.operation(), "read_fixture");

    let verified = verify_tool_result(
        &result,
        "fixture",
        "read_fixture",
        ToolCommitFence {
            expected_generation: 7,
            observed_generation: 7,
            cancelled: false,
        },
        &ToolVerificationRule::JsonEquals {
            pointer: "/text/0".to_owned(),
            expected: json!("value=42"),
        },
    );
    assert_eq!(verified, ToolVerificationOutcome::Verified);

    let not_proven = verify_tool_result(
        &result,
        "fixture",
        "read_fixture",
        ToolCommitFence {
            expected_generation: 7,
            observed_generation: 7,
            cancelled: false,
        },
        &ToolVerificationRule::JsonEquals {
            pointer: "/text/0".to_owned(),
            expected: json!("different"),
        },
    );
    assert_eq!(not_proven, ToolVerificationOutcome::NotProven);
}

#[test]
fn forged_or_wrong_capability_cannot_authorize_tool_action() {
    let error = authorize_tool_action(
        &authenticated(Capability::AvatarControl),
        ToolAction {
            tool: "fixture".to_owned(),
            operation: "read_fixture".to_owned(),
            arguments: BTreeMap::new(),
        },
    )
    .expect_err("wrong capability must fail");
    assert_eq!(error.kind, EngineErrorKind::Unauthorized);
}

#[tokio::test]
async fn closed_mcp_transport_is_typed_as_unavailable() {
    let (adapter, client) = adapter_pair(Duration::from_secs(1)).await;
    client.cancel().await.expect("client cancellation succeeds");

    let error = adapter
        .execute(&authorized("read_fixture"))
        .await
        .expect_err("closed transport must fail");
    assert_eq!(error.kind, EngineErrorKind::Unavailable);
}

#[tokio::test]
async fn timeout_tool_error_and_malformed_result_are_typed() {
    let (adapter, _client) = adapter_pair(Duration::from_millis(20)).await;
    let timeout = adapter
        .execute(&authorized("delayed_fixture"))
        .await
        .expect_err("deadline must fire");
    assert_eq!(timeout.kind, EngineErrorKind::Timeout);

    let (adapter, _client) = adapter_pair(Duration::from_secs(1)).await;
    let tool_error = adapter
        .execute(&authorized("fail_fixture"))
        .await
        .expect("tool-level error remains an observation");
    assert_eq!(tool_error.class(), ToolResultClass::ToolError);

    let malformed = adapter
        .execute(&authorized("image_fixture"))
        .await
        .expect("unsupported content remains a bounded observation");
    assert_eq!(malformed.class(), ToolResultClass::Malformed);
}

#[tokio::test]
async fn oversized_observation_is_rejected_before_it_enters_domain_state() {
    let (adapter, _client) = adapter_pair(Duration::from_secs(1)).await;
    let error = adapter
        .execute(&authorized("oversized_fixture"))
        .await
        .expect_err("oversized result must fail closed");
    assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
}

#[tokio::test]
async fn cancelled_or_stale_result_cannot_verify_success() {
    let (adapter, _client) = adapter_pair(Duration::from_secs(1)).await;
    let result = adapter
        .execute(&authorized("read_fixture"))
        .await
        .expect("tool executes");
    let rule = ToolVerificationRule::JsonEquals {
        pointer: "/text/0".to_owned(),
        expected: json!("value=42"),
    };

    assert_eq!(
        verify_tool_result(
            &result,
            "fixture",
            "read_fixture",
            ToolCommitFence {
                expected_generation: 8,
                observed_generation: 7,
                cancelled: false,
            },
            &rule,
        ),
        ToolVerificationOutcome::Stale
    );
    assert_eq!(
        verify_tool_result(
            &result,
            "fixture",
            "read_fixture",
            ToolCommitFence {
                expected_generation: 7,
                observed_generation: 7,
                cancelled: true,
            },
            &rule,
        ),
        ToolVerificationOutcome::Cancelled
    );
}
