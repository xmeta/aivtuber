use aivtuber_domain::{
    AuthorizedToolAction, EngineError, EngineErrorKind, EngineFuture, MAX_TOOL_DISCOVERY_ITEMS,
    ToolAdapter, ToolDescriptor, ToolExecutionResult, ToolResultClass,
};
use rmcp::{model::CallToolRequestParams, service::ServerSink};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const DEFAULT_MCP_MAX_IN_FLIGHT: usize = 1;

#[derive(Clone)]
pub struct RmcpToolAdapter {
    server_name: String,
    server: ServerSink,
    timeout: Duration,
    in_flight: Arc<Semaphore>,
}

impl RmcpToolAdapter {
    pub fn new(server_name: impl Into<String>, server: ServerSink, timeout: Duration) -> Self {
        Self {
            server_name: server_name.into(),
            server,
            timeout,
            in_flight: Arc::new(Semaphore::new(DEFAULT_MCP_MAX_IN_FLIGHT)),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

impl ToolAdapter for RmcpToolAdapter {
    fn discover<'a>(&'a self) -> EngineFuture<'a, Vec<ToolDescriptor>> {
        Box::pin(async move {
            let tools = tokio::time::timeout(self.timeout, self.server.list_all_tools())
                .await
                .map_err(|_| {
                    EngineError::new(EngineErrorKind::Timeout, "MCP tool discovery timed out")
                })?
                .map_err(|error| {
                    EngineError::new(
                        EngineErrorKind::Unavailable,
                        format!("MCP tool discovery failed: {error}"),
                    )
                })?;

            normalize_tools(tools)
        })
    }

    fn execute<'a>(
        &'a self,
        action: &'a AuthorizedToolAction,
    ) -> EngineFuture<'a, ToolExecutionResult> {
        Box::pin(async move {
            let requested = action.action();
            if requested.tool != self.server_name {
                return Err(EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    format!(
                        "tool action targeted MCP server {:?}, adapter owns {:?}",
                        requested.tool, self.server_name
                    ),
                ));
            }

            let arguments: Map<String, Value> = requested.arguments.clone().into_iter().collect();
            let params =
                CallToolRequestParams::new(requested.operation.clone()).with_arguments(arguments);
            let started = Instant::now();
            let result = tokio::time::timeout(self.timeout, async {
                let _permit = self.in_flight.acquire().await.map_err(|_| {
                    EngineError::new(
                        EngineErrorKind::Unavailable,
                        "MCP execution concurrency gate is unavailable",
                    )
                })?;
                self.server.call_tool(params).await.map_err(|error| {
                    EngineError::new(
                        EngineErrorKind::Unavailable,
                        format!("MCP tool invocation failed: {error}"),
                    )
                })
            })
            .await
            .map_err(|_| {
                EngineError::new(EngineErrorKind::Timeout, "MCP tool invocation timed out")
            })??;
            let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

            let (class, content) = normalize_result(&result);
            ToolExecutionResult::new(
                self.server_name.clone(),
                requested.tool.clone(),
                requested.operation.clone(),
                class,
                content,
                None,
                latency_ms,
            )
        })
    }
}

fn normalize_tools(tools: Vec<rmcp::model::Tool>) -> Result<Vec<ToolDescriptor>, EngineError> {
    if tools.len() > MAX_TOOL_DISCOVERY_ITEMS {
        return Err(EngineError::new(
            EngineErrorKind::InvalidRequest,
            "MCP discovery exceeds the bounded tool-count limit",
        ));
    }

    tools
        .into_iter()
        .map(|tool| {
            let schema = serde_json::to_value(&tool.input_schema).map_err(|error| {
                EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    format!("MCP tool schema could not be normalized: {error}"),
                )
            })?;
            let schema_bytes = serde_json::to_vec(&schema).map_err(|error| {
                EngineError::new(
                    EngineErrorKind::InvalidRequest,
                    format!("MCP tool schema could not be fingerprinted: {error}"),
                )
            })?;
            let digest = Sha256::digest(schema_bytes);
            let fingerprint = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            ToolDescriptor::new(
                tool.name.to_string(),
                tool.description.map(|value| value.to_string()),
                schema,
                fingerprint,
            )
        })
        .collect()
}

fn normalize_result(result: &rmcp::model::CallToolResult) -> (ToolResultClass, Option<Value>) {
    let class = if result.is_error.unwrap_or(false) {
        ToolResultClass::ToolError
    } else {
        ToolResultClass::Success
    };

    if let Some(value) = &result.structured_content {
        return (class, Some(value.clone()));
    }

    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>();

    if !text.is_empty() {
        return (class, Some(json!({ "text": text })));
    }

    if result.content.is_empty() {
        (class, None)
    } else {
        (ToolResultClass::Malformed, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use std::sync::Arc;

    #[test]
    fn discovery_rejects_more_than_the_bounded_tool_count() {
        let schema = Arc::new(Map::new());
        let tools = (0..=MAX_TOOL_DISCOVERY_ITEMS)
            .map(|index| rmcp::model::Tool::new(format!("tool-{index}"), "fixture", schema.clone()))
            .collect();

        let error = normalize_tools(tools).expect_err("oversized discovery must fail closed");
        assert_eq!(error.kind, EngineErrorKind::InvalidRequest);
    }
}
