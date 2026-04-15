//! `KernelInfoServer` — virtual MCP server exposing context/kernel identity
//! tools. Phase 1 tool set: `whoami`.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::drift::DriftRouter;
use crate::file_tools::WhoamiEngine;


use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};
use super::adapter::{from_exec_result, to_exec_context};

/// `whoami` takes no parameters. Empty struct derives an object schema with
/// no properties — matches the legacy hand-written schema.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhoamiParams {}

pub struct KernelInfoServer {
    instance_id: InstanceId,
    whoami: WhoamiEngine,
    /// Seat for notifications. Phase 1 never emits onto this channel;
    /// subscribers will get nothing.
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl KernelInfoServer {
    pub const INSTANCE: &'static str = "builtin.kernel_info";

    pub fn new(drift: Arc<RwLock<DriftRouter>>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            whoami: WhoamiEngine::new(drift),
            notif_tx,
        }
    }
}

#[async_trait]
impl McpServerLike for KernelInfoServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let schema = schemars::schema_for!(WhoamiParams);
        Ok(vec![KernelTool {
            instance: self.instance_id.clone(),
            name: "whoami".to_string(),
            description: Some(self.whoami.description().to_string()),
            input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != "whoami" {
            return Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: params.tool,
            });
        }

        // Validate params shape even though it's empty — catches accidental
        // extras via `deny_unknown_fields`.
        let _: WhoamiParams =
            serde_json::from_value(params.arguments.clone()).map_err(McpError::InvalidParams)?;

        let tool_ctx = to_exec_context(ctx);
        let exec = self
            .whoami
            .execute("", &tool_ctx)
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;
        Ok(from_exec_result(exec))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::shared_drift_router;
    use crate::mcp::{Broker, InstancePolicy, KernelCallParams, ToolContent};

    #[tokio::test]
    async fn whoami_reaches_broker() {
        let drift = shared_drift_router();
        let server = Arc::new(KernelInfoServer::new(drift));
        let broker = Broker::new();
        broker
            .register(server.clone(), InstancePolicy::default())
            .await
            .unwrap();

        let ctx = CallContext::test();
        let result = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(KernelInfoServer::INSTANCE),
                    tool: "whoami".to_string(),
                    arguments: serde_json::json!({}),
                },
                &ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        match result.content.first().unwrap() {
            ToolContent::Text(s) => assert!(s.contains(&ctx.context_id.to_hex())),
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn whoami_rejects_unknown_tool() {
        let drift = shared_drift_router();
        let server = Arc::new(KernelInfoServer::new(drift));
        let broker = Broker::new();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx = CallContext::test();
        let err = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(KernelInfoServer::INSTANCE),
                    tool: "does_not_exist".to_string(),
                    arguments: serde_json::json!({}),
                },
                &ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, McpError::ToolNotFound { .. }));
    }
}
