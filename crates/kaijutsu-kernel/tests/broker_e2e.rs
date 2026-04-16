//! Kernel-level end-to-end tests for the MCP broker dispatch path.
//!
//! These exercise `Kernel::register_builtin_mcp_servers` +
//! `Kernel::dispatch_tool_via_broker` — the shape production call sites
//! (`llm_stream.rs`, `kaish_backend.rs`) use. Per-server unit tests in
//! `mcp/servers/*.rs` cover individual tools; this file locks the
//! kernel-level glue that ties them together.
//!
//! No subprocess external MCPs here — those need live binaries and are
//! smoke-tested on the GPU server (Phase 1 exit criterion #5).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use kaijutsu_kernel::block_store::{DocumentKind, SharedBlockStore, shared_block_store_with_db};
use kaijutsu_kernel::execution::ExecContext;
use kaijutsu_kernel::file_tools::FileDocumentCache;
use kaijutsu_kernel::kernel_db::{DocumentRow, KernelDb};
use kaijutsu_kernel::mcp::{
    CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams, KernelTool,
    KernelToolResult, McpError, McpResult, McpServerLike, ServerNotification,
};
use kaijutsu_kernel::Kernel;
use kaijutsu_types::{now_millis, ContextId, KernelId, PrincipalId, SessionId};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Fixture {
    kernel: Arc<Kernel>,
    ctx_id: ContextId,
    exec_ctx: ExecContext,
    _tmp: tempfile::TempDir,
}

async fn setup() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let kernel = Arc::new(Kernel::new("broker-e2e", Some(tmp.path())).await);

    let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
    let creator = PrincipalId::system();
    let kernel_id = KernelId::new();
    let ws_id = {
        let g = db.lock();
        g.get_or_create_default_workspace(kernel_id, creator).unwrap()
    };
    let store: SharedBlockStore = shared_block_store_with_db(db.clone(), kernel_id, ws_id, creator);

    let ctx_id = ContextId::new();
    {
        let g = db.lock();
        g.insert_document(&DocumentRow {
            document_id: ctx_id,
            kernel_id,
            workspace_id: ws_id,
            doc_kind: DocumentKind::Code,
            language: None,
            path: None,
            created_at: now_millis() as i64,
            created_by: creator,
        })
        .unwrap();
    }
    store.create_document(ctx_id, DocumentKind::Code, None).unwrap();

    let file_cache = Arc::new(FileDocumentCache::new(store.clone(), kernel.vfs().clone()));
    kernel
        .register_builtin_mcp_servers(store, file_cache, None)
        .await
        .expect("register_builtin_mcp_servers");

    let exec_ctx = ExecContext::new(
        creator,
        ctx_id,
        "/",
        SessionId::new(),
        kernel_id,
    );

    Fixture {
        kernel,
        ctx_id,
        exec_ctx,
        _tmp: tmp,
    }
}

#[tokio::test]
async fn builtin_block_roundtrip() {
    let fx = setup().await;

    let create = fx
        .kernel
        .dispatch_tool_via_broker(
            "block_create",
            r#"{"role":"user","kind":"text","content":"roundtrip"}"#,
            &fx.exec_ctx,
        )
        .await
        .expect("block_create dispatch");
    assert!(create.success, "block_create failed: {}", create.stderr);

    let created: serde_json::Value =
        serde_json::from_str(&create.stdout).expect("block_create stdout is JSON");
    let block_id = created
        .get("block_id")
        .and_then(|v| v.as_str())
        .expect("block_create returned block_id");

    let read_params = serde_json::json!({
        "block_id": block_id,
        "line_numbers": false,
    })
    .to_string();
    let read = fx
        .kernel
        .dispatch_tool_via_broker("block_read", &read_params, &fx.exec_ctx)
        .await
        .expect("block_read dispatch");
    assert!(read.success, "block_read failed: {}", read.stderr);
    assert!(
        read.stdout.contains("roundtrip"),
        "block_read output missing original content: {}",
        read.stdout
    );
}

#[tokio::test]
async fn list_visible_tools_surfaces_expected_names() {
    let fx = setup().await;

    // Auto-populate the binding by routing any tool through the kernel
    // shim — it seeds the binding with every registered instance.
    let defs = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;
    let names: Vec<&str> = defs.iter().map(|(n, _, _)| n.as_str()).collect();

    // Sampling, not the full 13 (per-server tests cover exhaustive lists).
    for expected in ["block_create", "block_read", "whoami"] {
        assert!(
            names.contains(&expected),
            "expected `{expected}` in visible tools, got {names:?}"
        );
    }
}

#[tokio::test]
async fn unknown_tool_name_surfaces_tool_not_found() {
    let fx = setup().await;

    let err = fx
        .kernel
        .dispatch_tool_via_broker("does_not_exist", "{}", &fx.exec_ctx)
        .await
        .expect_err("unknown tool must error");

    assert!(
        matches!(err, McpError::ToolNotFound { ref tool, .. } if tool == "does_not_exist"),
        "expected ToolNotFound(tool=does_not_exist), got {err:?}"
    );
}

#[tokio::test]
async fn is_error_result_maps_to_exec_failure() {
    // D-28 happy-path: a tool that *completes successfully* but returns
    // `KernelToolResult { is_error: true }` must surface at the call site as
    // `ExecResult::failure(1, <text>)`. This locks the is_error→LLM-boundary
    // conversion in `Kernel::dispatch_tool_via_broker`.
    let fx = setup().await;

    let mock = Arc::new(LocalMock::new("test.mock").on_call(|_p| async {
        Ok(KernelToolResult {
            is_error: true,
            content: vec![kaijutsu_kernel::mcp::ToolContent::Text("boom".into())],
            structured: None,
        })
    }));
    fx.kernel
        .broker()
        .register(mock, InstancePolicy::default())
        .await
        .unwrap();

    // Rebuild the binding so it picks up the newly-registered instance, then
    // kick list_visible_tools so the sticky name_map gets populated (dispatch
    // resolves via name_map, not allowed_instances alone).
    let mut binding = ContextToolBinding::new();
    for inst in fx.kernel.broker().list_instances().await {
        binding.allow(inst);
    }
    fx.kernel.broker().set_binding(fx.ctx_id, binding).await;
    let seed_ctx = CallContext::new(
        fx.exec_ctx.principal_id,
        fx.ctx_id,
        fx.exec_ctx.session_id,
        fx.exec_ctx.kernel_id,
    );
    fx.kernel
        .broker()
        .list_visible_tools(fx.ctx_id, &seed_ctx)
        .await
        .unwrap();

    let result = fx
        .kernel
        .dispatch_tool_via_broker("fail", "{}", &fx.exec_ctx)
        .await
        .expect("broker call itself should succeed");

    assert!(!result.success, "expected failure, got success");
    assert_eq!(result.exit_code, 1, "expected exit_code 1");
    assert_eq!(result.stderr, "boom", "is_error text should land in stderr");
    assert!(result.stdout.is_empty(), "stdout should be empty on failure");
}

// ---------------------------------------------------------------------------
// Inline fake — this integration test binary cannot reach the MockServer
// defined in `broker.rs` under `#[cfg(test)]`. Keep it minimal.
// ---------------------------------------------------------------------------

struct LocalMock {
    id: InstanceId,
    on_call: Arc<
        dyn Fn(KernelCallParams) -> BoxFuture<'static, McpResult<KernelToolResult>> + Send + Sync,
    >,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl LocalMock {
    fn new(id: &str) -> Self {
        let (notif_tx, _) = broadcast::channel(4);
        Self {
            id: InstanceId::new(id),
            on_call: Arc::new(|_p| Box::pin(async { Ok(KernelToolResult::text("ok")) })),
            notif_tx,
        }
    }

    fn on_call<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(KernelCallParams) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = McpResult<KernelToolResult>> + Send + 'static,
    {
        self.on_call = Arc::new(move |p| Box::pin(f(p)));
        self
    }
}

#[async_trait]
impl McpServerLike for LocalMock {
    fn instance_id(&self) -> &InstanceId {
        &self.id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        Ok(vec![KernelTool {
            instance: self.id.clone(),
            name: "fail".to_string(),
            description: None,
            input_schema: serde_json::json!({ "type": "object" }),
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        _ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        (self.on_call)(params).await
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

// Silence unused-import / Duration warnings if future edits remove the
// timeout-style test.
#[allow(dead_code)]
fn _touch() -> Duration {
    Duration::from_secs(0)
}
