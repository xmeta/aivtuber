use rig_core::tool::{DynamicTool, ManagedToolSink, ManagedToolToken};
use rig_rmcp::{McpClientHandler, rmcp::model::ClientInfo};
use rmcp::{
    ServiceExt,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

struct FixtureServer;

#[tool_router(server_handler)]
impl FixtureServer {
    #[tool(description = "Read one deterministic Rig conformance value")]
    async fn read_fixture(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(CallToolResult::success(vec![ContentBlock::text("rig-ok")]))
    }
}

#[derive(Clone, Default)]
struct RecordingSink {
    tools: Arc<Mutex<HashMap<String, (DynamicTool, ManagedToolToken)>>>,
}

impl RecordingSink {
    fn tool(&self, name: &str) -> Option<DynamicTool> {
        self.tools
            .lock()
            .expect("recording sink lock")
            .get(name)
            .map(|(tool, _)| tool.clone())
    }
}

impl ManagedToolSink for RecordingSink {
    fn add_managed_tools(&self, tools: Vec<DynamicTool>) -> HashMap<String, ManagedToolToken> {
        let mut stored = self.tools.lock().expect("recording sink lock");
        let mut tokens = HashMap::new();
        for tool in tools.into_iter().filter(DynamicTool::is_live) {
            let name = tool.name().to_owned();
            let token = ManagedToolToken::new();
            stored.insert(name.clone(), (tool, token.clone()));
            tokens.insert(name, token);
        }
        tokens
    }

    fn reconcile_managed_tools(
        &self,
        expected: HashMap<String, ManagedToolToken>,
        tools: Vec<DynamicTool>,
    ) -> HashMap<String, ManagedToolToken> {
        let mut stored = self.tools.lock().expect("recording sink lock");
        for (name, expected_token) in expected {
            let owned = stored
                .get(&name)
                .is_some_and(|(_, current_token)| current_token == &expected_token);
            if owned {
                stored.remove(&name);
            }
        }
        drop(stored);
        self.add_managed_tools(tools)
    }
}

#[tokio::test]
async fn rig_rmcp_can_register_and_invoke_a_bounded_mcp_tool() {
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    tokio::spawn(async move {
        let service = FixtureServer
            .serve(server_transport)
            .await
            .expect("fixture server starts");
        service.waiting().await.expect("fixture server completes");
    });

    let sink = RecordingSink::default();
    let service = McpClientHandler::new(ClientInfo::default(), sink.clone())
        .with_timeout(Duration::from_secs(1))
        .with_refresh_timeout(Duration::from_secs(1))
        .connect(client_transport)
        .await
        .expect("Rig MCP client connects");

    let tool = sink
        .tool("read_fixture")
        .expect("Rig registers the discovered MCP tool");
    let output = tool.execute(json!({})).await.expect("Rig invokes MCP tool");
    assert_eq!(output.as_text(), Some("rig-ok"));

    drop(service);
}
