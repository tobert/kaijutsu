//! `BuiltinResourcesServer` — virtual MCP server exposing broker-level
//! resource lifecycle as tools (instance `builtin.resources`, Phase 3 / D-41).
//!
//! Four tools delegate to `Broker::{list_resources, read_resource, subscribe,
//! unsubscribe}`:
//! - `list { instance }` — returns a JSON-serialized `KernelResourceList`.
//! - `read { instance, uri }` — triggers `Broker::read_resource` (which
//!   emits the root `BlockKind::Resource` block) and returns a short
//!   confirmation referencing the emitted block id.
//! - `subscribe { instance, uri }` — registers a live subscription tied to
//!   the calling `ContextToolBinding` (D-44).
//! - `unsubscribe { instance, uri }` — symmetric.
//!
//! Holds `Weak<Broker>` to avoid the Arc-cycle (broker owns the
//! instance Arc; the instance refers back via Weak and upgrades on each call).

use std::sync::{Arc, Weak};

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
pub struct ListParams {
    /// MCP instance id to list resources from (e.g. `gpal`, `bevy_brp`).
    pub instance: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UriParams {
    /// MCP instance id that owns the resource.
    pub instance: String,
    /// Resource URI (e.g. `file:///tmp/note.md`).
    pub uri: String,
}

pub struct BuiltinResourcesServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    /// Seat for notifications. The resources server itself never emits;
    /// subscribers get nothing.
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinResourcesServer {
    pub const INSTANCE: &'static str = "builtin.resources";

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
impl McpServerLike for BuiltinResourcesServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let list_schema = schemars::schema_for!(ListParams);
        let uri_schema = schemars::schema_for!(UriParams);
        let list_value = serde_json::to_value(&list_schema).map_err(McpError::InvalidParams)?;
        let uri_value = serde_json::to_value(&uri_schema).map_err(McpError::InvalidParams)?;
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "list".to_string(),
                description: Some(
                    "List resources advertised by the given MCP instance.".to_string(),
                ),
                input_schema: list_value,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "read".to_string(),
                description: Some(
                    "Read a resource and emit a BlockKind::Resource block into the \
                     calling context."
                        .to_string(),
                ),
                input_schema: uri_value.clone(),
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "subscribe".to_string(),
                description: Some(
                    "Subscribe the calling context to update notifications for the \
                     given resource URI. Subscription dies with the binding."
                        .to_string(),
                ),
                input_schema: uri_value.clone(),
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "unsubscribe".to_string(),
                description: Some(
                    "Remove a previously-created subscription for the given URI."
                        .to_string(),
                ),
                input_schema: uri_value,
            },
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let broker = self.broker()?;
        match params.tool.as_str() {
            "list" => {
                let p: ListParams =
                    serde_json::from_value(params.arguments.clone())
                        .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance);
                let list = broker.list_resources(&instance, ctx).await?;
                let json =
                    serde_json::json!({
                        "resources": list.resources.iter().map(|r| serde_json::json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mime_type": r.mime_type,
                            "size": r.size,
                        })).collect::<Vec<_>>(),
                    });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![super::super::types::ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "read" => {
                let p: UriParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance.clone());
                let _result = broker.read_resource(&instance, &p.uri, ctx).await?;
                Ok(KernelToolResult::text(format!(
                    "read {} from {}",
                    p.uri, p.instance
                )))
            }
            "subscribe" => {
                let p: UriParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance.clone());
                broker.subscribe(&instance, &p.uri, ctx).await?;
                Ok(KernelToolResult::text(format!(
                    "subscribed to {} on {}",
                    p.uri, p.instance
                )))
            }
            "unsubscribe" => {
                let p: UriParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance.clone());
                broker.unsubscribe(&instance, &p.uri, ctx).await?;
                Ok(KernelToolResult::text(format!(
                    "unsubscribed from {} on {}",
                    p.uri, p.instance
                )))
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use async_trait::async_trait;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::super::super::binding::ContextToolBinding;
    use super::super::super::broker::Broker;
    use super::super::super::context::CallContext;
    use super::super::super::error::{McpError, McpResult};
    use super::super::super::policy::InstancePolicy;
    use super::super::super::server_like::{McpServerLike, ServerNotification};
    use super::super::super::types::{
        InstanceId, KernelCallParams, KernelReadResource, KernelResource, KernelResourceContents,
        KernelResourceList, KernelTool, KernelToolResult,
    };
    use super::*;

    /// Minimal resource-providing fake for round-trip tests. Separate from
    /// broker.rs::tests::ResourceMock so each test module stays self-contained.
    struct BuiltinMock {
        id: InstanceId,
        subscribed: std::sync::Mutex<HashSet<String>>,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl BuiltinMock {
        fn new() -> Self {
            let (notif_tx, _) = broadcast::channel(16);
            Self {
                id: InstanceId::new("target"),
                subscribed: std::sync::Mutex::new(HashSet::new()),
                notif_tx,
            }
        }
    }

    #[async_trait]
    impl McpServerLike for BuiltinMock {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }
        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _params: KernelCallParams,
            _ctx: &CallContext,
            _cancel: CancellationToken,
        ) -> McpResult<KernelToolResult> {
            Ok(KernelToolResult::default())
        }
        fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
            self.notif_tx.subscribe()
        }
        async fn list_resources(&self, _ctx: &CallContext) -> McpResult<KernelResourceList> {
            Ok(KernelResourceList {
                resources: vec![KernelResource {
                    instance: self.id.clone(),
                    uri: "file:///hello".into(),
                    name: "hello".into(),
                    description: None,
                    mime_type: Some("text/plain".into()),
                    size: Some(5),
                }],
            })
        }
        async fn read_resource(
            &self,
            uri: &str,
            _ctx: &CallContext,
        ) -> McpResult<KernelReadResource> {
            Ok(KernelReadResource {
                contents: vec![KernelResourceContents::Text {
                    uri: uri.to_string(),
                    mime_type: Some("text/plain".into()),
                    text: "hello".into(),
                }],
            })
        }
        async fn subscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
            self.subscribed.lock().unwrap().insert(uri.to_string());
            Ok(())
        }
        async fn unsubscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
            self.subscribed.lock().unwrap().remove(uri);
            Ok(())
        }
    }

    /// D-41: subscribe via builtin.resources MCP tool reaches the target
    /// server through the broker. Locks the admin-tool round-trip.
    #[tokio::test]
    async fn builtin_resources_server_subscribe_roundtrip() {
        let broker = Arc::new(Broker::new());
        let resources_server = Arc::new(BuiltinResourcesServer::new(Arc::downgrade(&broker)));
        let target = Arc::new(BuiltinMock::new());

        broker
            .register(target.clone(), InstancePolicy::default())
            .await
            .unwrap();
        broker
            .register(resources_server, InstancePolicy::default())
            .await
            .unwrap();

        // Bind a context to the builtin server so subscribe lands on a real
        // ContextId.
        let ctx_id = kaijutsu_types::ContextId::new();
        let binding = ContextToolBinding::with_instances(vec![
            InstanceId::new(BuiltinResourcesServer::INSTANCE),
            InstanceId::new("target"),
        ]);
        broker.set_binding(ctx_id, binding).await;

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx_id;

        let result = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BuiltinResourcesServer::INSTANCE),
                    tool: "subscribe".to_string(),
                    arguments: serde_json::json!({
                        "instance": "target",
                        "uri": "file:///hello",
                    }),
                },
                &call_ctx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(
            target.subscribed.lock().unwrap().contains("file:///hello"),
            "subscribe via builtin.resources should reach the target server",
        );

        // Broker-side table is private; verify indirectly via clear_binding,
        // which must call unsubscribe on every tracked entry (D-44). If the
        // broker did not record the subscribe, clear_binding would be a no-op
        // on the target's `subscribed` set.
        broker.clear_binding(&ctx_id).await;
        assert!(
            !target.subscribed.lock().unwrap().contains("file:///hello"),
            "clear_binding should have unsubscribed via the broker table",
        );
    }

    #[tokio::test]
    async fn builtin_resources_server_unknown_tool_errors() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinResourcesServer::new(Arc::downgrade(&broker)));
        broker
            .register(server.clone(), InstancePolicy::default())
            .await
            .unwrap();

        let err = broker
            .call_tool(
                KernelCallParams {
                    instance: InstanceId::new(BuiltinResourcesServer::INSTANCE),
                    tool: "does_not_exist".to_string(),
                    arguments: serde_json::json!({}),
                },
                &CallContext::test(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolNotFound { .. }));
    }
}
