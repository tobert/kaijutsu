//! `BuiltinBindingsServer` — admin MCP surface for per-context tool binding
//! management (D-53, D-55, Phase 5 / §4.2).
//!
//! Three tools delegate to `Broker::{bind, unbind, binding}`:
//! - `bind    { instance }` — add `instance` to the calling context's
//!   binding. Triggers the per-tool diff pump (D-35) so `tool_added`
//!   notifications surface on the next LLM turn.
//! - `unbind  { instance }` — symmetric; fires `tool_removed`.
//! - `show    { }` — return the calling context's current
//!   `allowed_instances` + `name_map`.
//!
//! One resource, owned by this server directly (not delegated):
//! - `kj://kernel/tools` — kernel-wide instance-grouped listing of every
//!   registered instance and its tools, with a per-calling-context
//!   `bound: bool` flag (D-55). Subscribable — the bindings server's
//!   `notif_tx` is bridged in kernel bootstrap to
//!   `Broker::notifications()`, so kernel-level `ToolsChanged` (fired by
//!   `register` / `unregister`) becomes a `ResourceUpdated` that flows
//!   through the Phase 3 pump and emits a child Resource block under the
//!   original read.
//!
//! No hook-evaluation carve-out (D-53). A user who locks themselves out of
//! `bind`/`unbind` with an overbroad `PreCall Deny(*)` recovers by
//! restarting the kernel (hooks are in-memory) — same escape hatch as
//! every other admin instance.
//!
//! Holds `Weak<Broker>` to avoid the Arc-cycle.

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
use super::super::types::{
    InstanceId, KernelCallParams, KernelReadResource, KernelResource, KernelResourceContents,
    KernelResourceList, KernelTool, KernelToolResult, ToolContent,
};

/// Canonical resource URI for the kernel-wide instance listing (D-55).
/// `kj://kernel/*` is reserved for binding-agnostic kernel-wide views;
/// `kj://context/*` is reserved for per-calling-context filtered views
/// (follow-up).
pub const KERNEL_TOOLS_URI: &str = "kj://kernel/tools";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstanceParams {
    /// MCP instance id to add to / remove from the calling context's
    /// binding (e.g. `builtin.file`, `gpal`, `bevy_brp`).
    pub instance: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowParams {}

pub struct BuiltinBindingsServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinBindingsServer {
    pub const INSTANCE: &'static str = "builtin.bindings";

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

    /// Sender handle for the kernel bootstrap to bridge broker-level
    /// `KernelNotification::ToolsChanged` into
    /// `ServerNotification::ResourceUpdated { uri: KERNEL_TOOLS_URI }`.
    /// See `Kernel::spawn_kernel_tools_resource_pump` at the bootstrap
    /// site; this method is the one hand-off point for the bridging task.
    pub fn resource_update_sender(&self) -> broadcast::Sender<ServerNotification> {
        self.notif_tx.clone()
    }

    /// Build the JSON payload for a `kj://kernel/tools` read. Honest about
    /// the kernel-wide installed set per D-55 — does **not** apply
    /// `ListTools` hook filtering (that's a binding-scoped concern;
    /// discovery is binding-agnostic).
    async fn build_kernel_tools_payload(
        broker: &Arc<Broker>,
        ctx: &CallContext,
    ) -> McpResult<serde_json::Value> {
        let bound: std::collections::HashSet<InstanceId> = broker
            .binding(&ctx.context_id)
            .await
            .map(|b| b.allowed_instances.into_iter().collect())
            .unwrap_or_default();

        let instance_ids = broker.list_instances().await;
        let instances_snapshot = broker.instances_snapshot().await;

        let sys_ctx = CallContext::system_for_context(ctx.context_id);
        let mut instances_json: Vec<serde_json::Value> = Vec::new();
        for id in instance_ids {
            let server = match instances_snapshot.get(&id) {
                Some(s) => s.clone(),
                None => continue,
            };
            let tools = server.list_tools(&sys_ctx).await.unwrap_or_default();
            let tools_json: Vec<serde_json::Value> = tools
                .into_iter()
                .map(|kt| {
                    serde_json::json!({
                        "name": kt.name,
                        "description": kt.description,
                    })
                })
                .collect();
            instances_json.push(serde_json::json!({
                "id": id.as_str(),
                "bound": bound.contains(&id),
                "tools": tools_json,
            }));
        }

        Ok(serde_json::json!({ "instances": instances_json }))
    }
}

#[async_trait]
impl McpServerLike for BuiltinBindingsServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let instance_schema = schemars::schema_for!(InstanceParams);
        let show_schema = schemars::schema_for!(ShowParams);
        let instance_value =
            serde_json::to_value(&instance_schema).map_err(McpError::InvalidParams)?;
        let show_value = serde_json::to_value(&show_schema).map_err(McpError::InvalidParams)?;
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "bind".to_string(),
                description: Some(
                    "Add a registered MCP instance to the calling context's tool \
                     binding. Fires tool_added notifications for every tool the \
                     instance exposes."
                        .to_string(),
                ),
                input_schema: instance_value.clone(),
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "unbind".to_string(),
                description: Some(
                    "Remove an MCP instance from the calling context's tool binding. \
                     Fires tool_removed notifications for every tool that was \
                     visible through that instance."
                        .to_string(),
                ),
                input_schema: instance_value,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "show".to_string(),
                description: Some(
                    "Return the calling context's current binding — allowed \
                     instances in order plus the sticky visible-name map."
                        .to_string(),
                ),
                input_schema: show_value,
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
            "bind" => {
                let p: InstanceParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance.clone());
                broker.bind(ctx.context_id, instance).await;
                let json = serde_json::json!({ "instance": p.instance });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "unbind" => {
                let p: InstanceParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let instance = InstanceId::new(p.instance.clone());
                broker.unbind(ctx.context_id, &instance).await;
                let json = serde_json::json!({ "instance": p.instance });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "show" => {
                let _: ShowParams = serde_json::from_value(params.arguments.clone())
                    .map_err(McpError::InvalidParams)?;
                let binding = broker.binding(&ctx.context_id).await.unwrap_or_default();
                let allowed: Vec<&str> = binding
                    .allowed_instances
                    .iter()
                    .map(|i| i.as_str())
                    .collect();
                let names: serde_json::Value = binding
                    .name_map
                    .iter()
                    .map(|(visible, (inst, tool))| {
                        (
                            visible.clone(),
                            serde_json::json!({ "instance": inst.as_str(), "tool": tool }),
                        )
                    })
                    .collect::<serde_json::Map<_, _>>()
                    .into();
                let json = serde_json::json!({
                    "allowed_instances": allowed,
                    "name_map": names,
                });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            other => Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: other.to_string(),
            }),
        }
    }

    async fn list_resources(&self, _ctx: &CallContext) -> McpResult<KernelResourceList> {
        Ok(KernelResourceList {
            resources: vec![KernelResource {
                instance: self.instance_id.clone(),
                uri: KERNEL_TOOLS_URI.to_string(),
                name: "kernel tools".to_string(),
                description: Some(
                    "Kernel-wide instance and tool listing. `bound` indicates \
                     visibility to the calling context."
                        .to_string(),
                ),
                mime_type: Some("application/json".to_string()),
                size: None,
            }],
        })
    }

    async fn read_resource(
        &self,
        uri: &str,
        ctx: &CallContext,
    ) -> McpResult<KernelReadResource> {
        if uri != KERNEL_TOOLS_URI {
            return Err(McpError::Protocol(format!(
                "{} does not host resource {uri:?}",
                self.instance_id
            )));
        }
        let broker = self.broker()?;
        let json = Self::build_kernel_tools_payload(&broker, ctx).await?;
        let text = serde_json::to_string_pretty(&json).map_err(McpError::InvalidParams)?;
        Ok(KernelReadResource {
            contents: vec![KernelResourceContents::Text {
                uri: uri.to_string(),
                mime_type: Some("application/json".to_string()),
                text,
            }],
        })
    }

    /// Accepts subscription to `KERNEL_TOOLS_URI`. The broker records the
    /// subscription in its own table; updates flow via `notif_tx` which
    /// the kernel bootstrap bridges from `Broker::notifications()`
    /// (`KernelNotification::ToolsChanged`) to
    /// `ServerNotification::ResourceUpdated`.
    async fn subscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
        if uri != KERNEL_TOOLS_URI {
            return Err(McpError::Protocol(format!(
                "{} does not host resource {uri:?}",
                self.instance_id
            )));
        }
        Ok(())
    }

    async fn unsubscribe(&self, uri: &str, _ctx: &CallContext) -> McpResult<()> {
        if uri != KERNEL_TOOLS_URI {
            return Err(McpError::Protocol(format!(
                "{} does not host resource {uri:?}",
                self.instance_id
            )));
        }
        Ok(())
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::super::broker::Broker;
    use super::super::super::error::McpError;
    use super::super::super::hook_table::{GlobPattern, HookAction, HookEntry};
    use super::super::super::policy::InstancePolicy;
    use super::super::super::types::KernelCallParams;
    use async_trait::async_trait;
    use kaijutsu_types::{ContextId, PrincipalId};
    use tokio::sync::broadcast;

    /// Minimal target mock that advertises a couple of tools and nothing
    /// else. Used to populate the broker's instance set for
    /// `kj://kernel/tools` read tests.
    struct ToolsMock {
        id: InstanceId,
        tools: Vec<KernelTool>,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl ToolsMock {
        fn new(id: &str, tool_names: &[&str]) -> Self {
            let (notif_tx, _) = broadcast::channel(8);
            let id = InstanceId::new(id);
            let tools = tool_names
                .iter()
                .map(|name| KernelTool {
                    instance: id.clone(),
                    name: (*name).to_string(),
                    description: None,
                    input_schema: serde_json::json!({ "type": "object" }),
                })
                .collect();
            Self {
                id,
                tools,
                notif_tx,
            }
        }
    }

    #[async_trait]
    impl McpServerLike for ToolsMock {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }
        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(self.tools.clone())
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
    }

    fn call_ctx_for(ctx_id: ContextId) -> CallContext {
        let mut c = CallContext::test();
        c.context_id = ctx_id;
        c
    }

    fn call_params(tool: &str, args: serde_json::Value) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(BuiltinBindingsServer::INSTANCE),
            tool: tool.to_string(),
            arguments: args,
        }
    }

    /// `bind` via the admin tool mutates the context binding and the
    /// subsequent `show` reflects it. Closes exit criterion #1 at the
    /// admin-server level (broker_e2e picks up the full LLM-visible
    /// notification path in M5).
    #[tokio::test]
    async fn bindings_admin_bind_roundtrip() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(
                Arc::new(ToolsMock::new("target", &["alpha", "beta"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx_id = ContextId::new();
        let call_ctx = call_ctx_for(ctx_id);

        // Initially empty.
        let show0 = broker
            .call_tool(
                call_params("show", serde_json::json!({})),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let s0 = show0.structured.unwrap();
        assert_eq!(
            s0["allowed_instances"].as_array().unwrap().len(),
            0,
            "fresh context must show no binding",
        );

        // Bind "target".
        let result = broker
            .call_tool(
                call_params("bind", serde_json::json!({ "instance": "target" })),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);

        // show reflects it.
        let show1 = broker
            .call_tool(
                call_params("show", serde_json::json!({})),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let s1 = show1.structured.unwrap();
        let allowed: Vec<String> = s1["allowed_instances"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(allowed, vec!["target".to_string()]);
    }

    /// `unbind` admin tool removes the instance from the binding.
    #[tokio::test]
    async fn bindings_admin_unbind_roundtrip() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(
                Arc::new(ToolsMock::new("target", &["alpha"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx_id = ContextId::new();
        broker.bind(ctx_id, InstanceId::new("target")).await;
        let call_ctx = call_ctx_for(ctx_id);

        broker
            .call_tool(
                call_params("unbind", serde_json::json!({ "instance": "target" })),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let show = broker
            .call_tool(
                call_params("show", serde_json::json!({})),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let allowed: &Vec<serde_json::Value> =
            show.structured.as_ref().unwrap()["allowed_instances"]
                .as_array()
                .unwrap();
        assert!(allowed.is_empty(), "unbind must leave no instances");
    }

    /// `show` on a never-bound context returns empty rather than erroring.
    /// The kernel defaults to "bind all registered" elsewhere — but the
    /// admin tool itself must honestly report "no binding set."
    #[tokio::test]
    async fn bindings_admin_show_without_binding_returns_empty() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx_id = ContextId::new();
        let show = broker
            .call_tool(
                call_params("show", serde_json::json!({})),
                &call_ctx_for(ctx_id),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let s = show.structured.unwrap();
        assert!(s["allowed_instances"].as_array().unwrap().is_empty());
        assert!(
            s["name_map"].as_object().unwrap().is_empty(),
            "name_map is empty when no binding is set"
        );
    }

    /// D-55 exit #4: `kj://kernel/tools` returns every registered instance
    /// with per-tool detail and a `bound` flag relative to the calling
    /// context.
    #[tokio::test]
    async fn kernel_tools_resource_read_returns_all_instances() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(
                Arc::new(ToolsMock::new("a", &["alpha"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(
                Arc::new(ToolsMock::new("b", &["beta", "gamma"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(server.clone(), InstancePolicy::default())
            .await
            .unwrap();

        let ctx_id = ContextId::new();
        let call_ctx = call_ctx_for(ctx_id);

        let read = server
            .read_resource(KERNEL_TOOLS_URI, &call_ctx)
            .await
            .unwrap();
        let content = match &read.contents[0] {
            KernelResourceContents::Text { text, .. } => text.clone(),
            _ => panic!("expected text content"),
        };
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instances: &Vec<serde_json::Value> = v["instances"].as_array().unwrap();
        let ids: Vec<&str> = instances
            .iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect();
        // Both "a", "b", and the bindings server itself should appear.
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&BuiltinBindingsServer::INSTANCE));

        // Verify per-instance tool listing.
        let b_entry = instances
            .iter()
            .find(|x| x["id"].as_str() == Some("b"))
            .unwrap();
        let b_tools: Vec<&str> = b_entry["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(b_tools.contains(&"beta"));
        assert!(b_tools.contains(&"gamma"));
    }

    /// D-55: the `bound` flag in the resource payload reflects the calling
    /// context's current binding. A never-bound context sees false; after
    /// binding, the same context sees true.
    #[tokio::test]
    async fn kernel_tools_resource_bound_flag_reflects_context() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(
                Arc::new(ToolsMock::new("target", &["alpha"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(server.clone(), InstancePolicy::default())
            .await
            .unwrap();

        let ctx_id = ContextId::new();
        let call_ctx = call_ctx_for(ctx_id);

        let read_before = server
            .read_resource(KERNEL_TOOLS_URI, &call_ctx)
            .await
            .unwrap();
        let bound_before = {
            let content = match &read_before.contents[0] {
                KernelResourceContents::Text { text, .. } => text.clone(),
                _ => panic!(),
            };
            let v: serde_json::Value = serde_json::from_str(&content).unwrap();
            v["instances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"].as_str() == Some("target"))
                .unwrap()["bound"]
                .as_bool()
                .unwrap()
        };
        assert!(!bound_before, "unbound context should see bound=false");

        broker.bind(ctx_id, InstanceId::new("target")).await;

        let read_after = server
            .read_resource(KERNEL_TOOLS_URI, &call_ctx)
            .await
            .unwrap();
        let bound_after = {
            let content = match &read_after.contents[0] {
                KernelResourceContents::Text { text, .. } => text.clone(),
                _ => panic!(),
            };
            let v: serde_json::Value = serde_json::from_str(&content).unwrap();
            v["instances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"].as_str() == Some("target"))
                .unwrap()["bound"]
                .as_bool()
                .unwrap()
        };
        assert!(bound_after, "after bind, context should see bound=true");
    }

    /// D-53: `builtin.bindings` is subject to hook evaluation — no
    /// carve-out like `builtin.hooks` (D-51). A `PreCall Deny` on the
    /// bindings server blocks the call. The escape hatch is kernel
    /// restart, not an immunity policy.
    #[tokio::test]
    async fn bindings_server_subject_to_hooks() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        broker
            .register_silently(
                Arc::new(ToolsMock::new("target", &["alpha"])),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        // Register a PreCall Deny targeting builtin.bindings.
        broker
            .hooks()
            .write()
            .await
            .pre_call
            .entries
            .push(HookEntry {
                id: super::super::super::error::HookId("no-bindings".to_string()),
                match_instance: Some(GlobPattern(
                    BuiltinBindingsServer::INSTANCE.to_string(),
                )),
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("read-only session".to_string()),
                priority: 0,
            });

        let ctx_id = ContextId::new();
        let err = broker
            .call_tool(
                call_params("bind", serde_json::json!({ "instance": "target" })),
                &call_ctx_for(ctx_id),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Denied { .. }),
            "expected Denied, got {err:?}",
        );

        // The binding must NOT have been mutated.
        let binding = broker.binding(&ctx_id).await.unwrap_or_default();
        assert!(binding.allowed_instances.is_empty());
    }

    /// `read_resource` rejects unknown URIs rather than silently returning
    /// empty content — a regression guard against "any URI reads OK."
    #[tokio::test]
    async fn read_resource_rejects_unknown_uri() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&broker)));
        let _ = PrincipalId::new();  // silence unused import on some platforms
        let err = server
            .read_resource("kj://kernel/unknown", &CallContext::test())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }
}
