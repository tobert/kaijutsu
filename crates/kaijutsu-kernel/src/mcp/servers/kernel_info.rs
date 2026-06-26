//! `KernelInfoServer` — virtual MCP server exposing context/kernel identity
//! tools. Phase 1 tool set: `whoami`.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::drift::DriftRouter;
use crate::kernel_db::KernelDb;
use crate::kj::format::hex32;

use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult};

/// `whoami` takes no parameters. Empty struct derives an object schema with
/// no properties — matches the legacy hand-written schema.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhoamiParams {}

pub struct KernelInfoServer {
    instance_id: InstanceId,
    drift_router: Arc<RwLock<DriftRouter>>,
    kernel_db: Arc<Mutex<KernelDb>>,
    /// Seat for notifications. Phase 1 never emits onto this channel;
    /// subscribers will get nothing.
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl KernelInfoServer {
    pub const INSTANCE: &'static str = "builtin.kernel_info";

    pub fn new(drift: Arc<RwLock<DriftRouter>>, kernel_db: Arc<Mutex<KernelDb>>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            drift_router: drift,
            kernel_db,
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
            description: Some("Show current context identity: ID, label, model, type, trace, parent".to_string()),
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

        let router = self.drift_router.read();
        let handle = router.get(ctx.context_id);

        let row = self
            .kernel_db
            .lock()
            .get_context(ctx.context_id)
            .map_err(|e| McpError::Protocol(format!("whoami: failed to read context row: {e}")))?;

        if handle.is_some() && row.is_none() {
            let id = ctx.context_id.to_hex();
            tracing::error!(
                context_id = %id,
                "whoami: drift handle exists with no context row — invariant violated"
            );
            return Err(McpError::Protocol(format!(
                "whoami: context {id} has a drift handle but no persisted row (state corruption)"
            )));
        }

        let context_type = row.map(|row| row.context_type);

        let info = serde_json::json!({
            "context_id": ctx.context_id.to_hex(),
            "context_id_short": ctx.context_id.short(),
            "label": handle.and_then(|h| h.label.as_deref()),
            "model": handle.and_then(|h| h.model.as_deref()),
            "provider": handle.and_then(|h| h.provider.as_deref()),
            "context_type": context_type,
            "trace_id": handle.map(|h| hex32(h.trace_id)),
            "forked_from": handle.and_then(|h| h.forked_from.map(|p| p.short())),
        });

        Ok(KernelToolResult::text(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        ))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::shared_drift_router;
    use crate::kernel_db::{ContextRow, DocumentRow};
    use crate::mcp::{Broker, InstancePolicy, ToolContent};
    use kaijutsu_types::{ConsentMode, ContextId, ContextState, DocKind, PrincipalId};

    fn ctx_row(id: ContextId, context_type: &str) -> ContextRow {
        ContextRow {
            context_id: id,
            label: Some("ctx".to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            context_type: context_type.to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
        }
    }

    /// In-memory DB seeded with a persisted context row of the given type, so
    /// whoami's DB-backed `context_type` is always populated (never null).
    fn db_with_context(id: ContextId, context_type: &str) -> Arc<Mutex<KernelDb>> {
        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        {
            let g = db.lock();
            let creator = PrincipalId::new();
            let ws_id = g.get_or_create_default_workspace(creator).unwrap();
            g.insert_document(&DocumentRow {
                document_id: id,
                workspace_id: ws_id,
                doc_kind: DocKind::Conversation,
                language: None,
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: creator,
            })
            .unwrap();
            g.insert_context(&ctx_row(id, context_type)).unwrap();
        }
        db
    }

    #[tokio::test]
    async fn whoami_reaches_broker() {
        let ctx = CallContext::test();
        let drift = shared_drift_router();
        let db = db_with_context(ctx.context_id, "coder");
        let server = Arc::new(KernelInfoServer::new(drift, db));
        let broker = Arc::new(Broker::new());
        broker
            .register(server.clone(), InstancePolicy::default())
            .await
            .unwrap();

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
            ToolContent::Text(s) => {
                assert!(s.contains(&ctx.context_id.to_hex()));
                // DB-backed context_type flows through the broker path.
                assert!(s.contains("\"context_type\": \"coder\""));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn whoami_surfaces_context_type_and_trace_id() {
        let ctx = CallContext::test();
        let drift = shared_drift_router();
        drift
            .write()
            .register(ctx.context_id, Some("ctx"), None, PrincipalId::new())
            .unwrap();

        let db = db_with_context(ctx.context_id, "coder");
        let server = Arc::new(KernelInfoServer::new(drift, db));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

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
            ToolContent::Text(s) => {
                let json: serde_json::Value = serde_json::from_str(s).unwrap();
                assert_eq!(json["context_type"], "coder");
                let trace = json["trace_id"].as_str().expect("trace_id present");
                assert_eq!(trace.len(), 32);
                assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn whoami_context_type_is_independent_of_handle_label() {
        let ctx = CallContext::test();
        let drift = shared_drift_router();
        drift
            .write()
            .register(ctx.context_id, None, None, PrincipalId::new())
            .unwrap();

        let db = db_with_context(ctx.context_id, "planner");
        let server = Arc::new(KernelInfoServer::new(drift, db));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

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
            ToolContent::Text(s) => {
                let json: serde_json::Value = serde_json::from_str(s).unwrap();
                assert_eq!(json["context_type"], "planner");
                assert!(json["label"].is_null());
                assert!(json["trace_id"].is_string());
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn whoami_errors_when_handle_has_no_row() {
        let ctx = CallContext::test();
        let drift = shared_drift_router();
        drift
            .write()
            .register(ctx.context_id, Some("ghost"), None, PrincipalId::new())
            .unwrap();

        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        let server = Arc::new(KernelInfoServer::new(drift, db));
        let broker = Arc::new(Broker::new());
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let err = broker
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
            .expect_err("handle without row must be fatal");

        assert!(err.to_string().contains("no persisted row"));
    }

    #[tokio::test]
    async fn whoami_rejects_unknown_tool() {
        let drift = shared_drift_router();
        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        let server = Arc::new(KernelInfoServer::new(drift, db));
        let broker = Arc::new(Broker::new());
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
