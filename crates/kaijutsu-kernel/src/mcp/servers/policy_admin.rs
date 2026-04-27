//! `BuiltinPolicyServer` — admin MCP surface for per-instance
//! `InstancePolicy` introspection and runtime tuning (M3-D5).
//!
//! Two tools, both delegate to the broker's policy state:
//! - `policy_show { instance }` — return the current `InstancePolicy`
//!   (call_timeout_ms, max_result_bytes, max_concurrency).
//! - `policy_set { instance, call_timeout_ms?, max_result_bytes? }` —
//!   mutate one or both in place. `max_concurrency` is registration-only
//!   because resizing the semaphore mid-flight would race in-flight
//!   permits.
//!
//! Per-principal budgets and fair queuing are deferred follow-ups; this
//! lands the get/set surface so operators can tune without restarting.
//!
//! Holds `Weak<Broker>` to avoid the Arc cycle.

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyShowParams {
    /// MCP instance id (e.g. `builtin.file`, `gpal`).
    pub instance: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicySetParams {
    pub instance: String,
    /// New per-call timeout in milliseconds. Omit to keep current.
    #[serde(default)]
    pub call_timeout_ms: Option<u64>,
    /// New max result bytes (truncation threshold). Omit to keep current.
    #[serde(default)]
    pub max_result_bytes: Option<u64>,
}

pub struct BuiltinPolicyServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinPolicyServer {
    pub const INSTANCE: &'static str = "builtin.policy";

    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            broker,
            notif_tx,
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }
}

#[async_trait]
impl McpServerLike for BuiltinPolicyServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let show_schema = schemars::schema_for!(PolicyShowParams);
        let set_schema = schemars::schema_for!(PolicySetParams);
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "policy_show".to_string(),
                description: Some(
                    "Return the current InstancePolicy (call_timeout_ms, \
                     max_result_bytes, max_concurrency) for a registered \
                     MCP instance."
                        .to_string(),
                ),
                input_schema: serde_json::to_value(show_schema)
                    .map_err(McpError::InvalidParams)?,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "policy_set".to_string(),
                description: Some(
                    "Update call_timeout_ms and/or max_result_bytes for a \
                     registered MCP instance. max_concurrency is set at \
                     registration time only and cannot be changed live."
                        .to_string(),
                ),
                input_schema: serde_json::to_value(set_schema)
                    .map_err(McpError::InvalidParams)?,
            },
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        _ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let broker = self.broker()?;
        match params.tool.as_str() {
            "policy_show" => {
                let parsed: PolicyShowParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let id = InstanceId::new(parsed.instance);
                let policy = broker
                    .policy_of(&id)
                    .await
                    .ok_or_else(|| McpError::InstanceNotFound(id.clone()))?;
                let payload = serde_json::json!({
                    "instance": id.as_str(),
                    "call_timeout_ms": policy.call_timeout.as_millis() as u64,
                    "max_result_bytes": policy.max_result_bytes,
                    "max_concurrency": policy.max_concurrency,
                });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![],
                    structured: Some(payload),
                })
            }
            "policy_set" => {
                let parsed: PolicySetParams =
                    serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;
                let id = InstanceId::new(parsed.instance);
                let timeout = parsed.call_timeout_ms.map(Duration::from_millis);
                let bytes = parsed.max_result_bytes.map(|b| b as usize);
                broker.update_policy(&id, timeout, bytes).await?;
                let policy = broker
                    .policy_of(&id)
                    .await
                    .ok_or_else(|| McpError::InstanceNotFound(id.clone()))?;
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![],
                    structured: Some(serde_json::json!({
                        "instance": id.as_str(),
                        "call_timeout_ms": policy.call_timeout.as_millis() as u64,
                        "max_result_bytes": policy.max_result_bytes,
                        "max_concurrency": policy.max_concurrency,
                        "updated": true,
                    })),
                })
            }
            other => Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: other.to_string(),
            }),
        }
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}
