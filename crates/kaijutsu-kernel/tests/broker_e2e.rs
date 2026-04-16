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
use kaijutsu_kernel::llm::hydrate_from_blocks;
use kaijutsu_kernel::mcp::{
    CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
    KernelReadResource, KernelResource, KernelResourceContents, KernelResourceList, KernelTool,
    KernelToolResult, McpError, McpResult, McpServerLike, ServerNotification,
};
use kaijutsu_kernel::Kernel;
use kaijutsu_types::{
    now_millis, BlockFilter, BlockKind, ContextId, KernelId, NotificationKind, PrincipalId,
    SessionId,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Fixture {
    kernel: Arc<Kernel>,
    ctx_id: ContextId,
    exec_ctx: ExecContext,
    store: SharedBlockStore,
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
        .register_builtin_mcp_servers(store.clone(), file_cache, None)
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
        store,
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
    /// Resources advertised by `list_resources` (Phase 3).
    resources: std::sync::Mutex<Vec<KernelResource>>,
    /// URI → contents map returned by `read_resource`.
    resource_contents: std::sync::Mutex<std::collections::HashMap<String, KernelResourceContents>>,
    /// URIs this mock considers currently subscribed.
    subscribed: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl LocalMock {
    fn new(id: &str) -> Self {
        let (notif_tx, _) = broadcast::channel(64);
        Self {
            id: InstanceId::new(id),
            on_call: Arc::new(|_p| Box::pin(async { Ok(KernelToolResult::text("ok")) })),
            notif_tx,
            resources: std::sync::Mutex::new(Vec::new()),
            resource_contents: std::sync::Mutex::new(std::collections::HashMap::new()),
            subscribed: std::sync::Mutex::new(std::collections::HashSet::new()),
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

    fn with_text_resource(self, uri: &str, text: &str) -> Self {
        self.resources.lock().unwrap().push(KernelResource {
            instance: self.id.clone(),
            uri: uri.to_string(),
            name: uri.to_string(),
            description: None,
            mime_type: Some("text/plain".to_string()),
            size: Some(text.len() as u64),
        });
        self.resource_contents.lock().unwrap().insert(
            uri.to_string(),
            KernelResourceContents::Text {
                uri: uri.to_string(),
                mime_type: Some("text/plain".to_string()),
                text: text.to_string(),
            },
        );
        self
    }

    fn sender(&self) -> broadcast::Sender<ServerNotification> {
        self.notif_tx.clone()
    }

    fn is_subscribed(&self, uri: &str) -> bool {
        self.subscribed.lock().unwrap().contains(uri)
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

    async fn list_resources(&self, _ctx: &CallContext) -> McpResult<KernelResourceList> {
        Ok(KernelResourceList {
            resources: self.resources.lock().unwrap().clone(),
        })
    }

    async fn read_resource(
        &self,
        uri: &str,
        _ctx: &CallContext,
    ) -> McpResult<KernelReadResource> {
        match self.resource_contents.lock().unwrap().get(uri).cloned() {
            Some(c) => Ok(KernelReadResource { contents: vec![c] }),
            None => Err(McpError::Protocol(format!("no such uri: {uri}"))),
        }
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

/// Phase 2 (M4) — exit criterion #2: a tool registered at runtime appears
/// in `list_tool_defs_via_broker` on the next call, without any caching
/// between calls. `build_tool_definitions` (in `llm_stream.rs`) calls this
/// fresh on every LLM spawn, so this test locks the "no stale list" path.
#[tokio::test]
async fn late_registration_visible_next_turn() {
    let fx = setup().await;

    // Turn 1: enumerate tools. Auto-populates the context binding with the
    // three builtins. `block_create` is one of the builtin surfaces.
    let defs_before = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;
    let names_before: Vec<&str> = defs_before.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        names_before.contains(&"block_create"),
        "expected builtin 'block_create' before late registration, got {names_before:?}",
    );
    assert!(
        !names_before.contains(&"fail"),
        "LocalMock should not be visible before its registration, got {names_before:?}",
    );

    // Register a new MCP instance at runtime. Its tool is named "fail"
    // (LocalMock::list_tools).
    let mock = Arc::new(LocalMock::new("test.late"));
    fx.kernel
        .broker()
        .register(mock, InstancePolicy::default())
        .await
        .expect("runtime register should succeed");

    // Clear the binding so the next call auto-populates with the full
    // instance list — simulates the admin-hatch path that reconciles
    // bindings when a new MCP is registered.
    fx.kernel.broker().clear_binding(&fx.ctx_id).await;

    // Turn 2: `list_tool_defs_via_broker` must NOT return a cached list —
    // it queries the broker fresh, sees the newly-registered instance, and
    // surfaces its tools. This is what the LLM sees on its next turn.
    let defs_after = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;
    let names_after: Vec<&str> = defs_after.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(
        names_after.contains(&"fail"),
        "expected late-registered 'fail' tool in next-turn list, got {names_after:?}",
    );
    assert!(
        names_after.contains(&"block_create"),
        "builtin tools must still be present after late registration, got {names_after:?}",
    );
}

/// Phase 2 end-to-end: `ServerNotification` → Notification block in context
/// → LLM hydrator sees the XML envelope.
///
/// This is the only test that stitches the full chain together without
/// needing the Bevy app:
///
///   register(MCP instance)
///     └─► broker.emit_for_bindings
///           └─► SharedBlockStore::insert_notification_block_as
///                 └─► BlockKind::Notification row in the CRDT
///                       └─► store.query_blocks(ctx, kind=Notification)
///                             └─► hydrate_from_blocks
///                                   └─► `<notification …>` in a user message
///
/// Unit tests at each layer cover the layer in isolation; this one locks
/// the composition so that a filter change in the hydrator, a mis-wired
/// binding in the broker, or a schema miss in the CRDT cannot silently
/// break the LLM-visible story (D-34) while every unit test still passes.
///
/// Effective because it uses only production APIs — no mock hydrator, no
/// mock broker, no backdoor into CRDT internals. If this test passes, the
/// kernel's side of the Phase 2 story is real; only the app rendering
/// remains to verify (M5).
#[tokio::test]
async fn server_notification_reaches_llm_hydrator() {
    let fx = setup().await;

    // Pre-bind the context to the instance we're about to register. The
    // broker walks existing bindings at emit time; a context with no
    // binding would skip the notification silently, which is correct
    // behavior — so the test has to bind first for the emission to land.
    let mock_id = InstanceId::new("test.hydrator");
    let binding = ContextToolBinding::with_instances(vec![mock_id.clone()]);
    fx.kernel.broker().set_binding(fx.ctx_id, binding).await;

    // Runtime-register an MCP instance → broker emits a `ToolAdded`
    // notification block into every bound context (exit criterion #1).
    let mock = Arc::new(LocalMock::new(mock_id.as_str()));
    fx.kernel
        .broker()
        .register(mock, InstancePolicy::default())
        .await
        .expect("runtime register should succeed");

    // Fetch notification blocks via the production query path the app
    // uses. `LocalMock::list_tools` advertises one tool named "fail".
    let notif_blocks = fx
        .store
        .query_blocks(
            fx.ctx_id,
            &BlockFilter {
                kinds: vec![BlockKind::Notification],
                ..Default::default()
            },
        )
        .expect("query_blocks");
    assert_eq!(
        notif_blocks.len(),
        1,
        "expected exactly one ToolAdded notification block, got {:?}",
        notif_blocks
    );
    let payload = notif_blocks[0]
        .notification
        .as_ref()
        .expect("notification payload must survive CRDT storage");
    assert_eq!(payload.kind, NotificationKind::ToolAdded);
    assert_eq!(payload.instance, "test.hydrator");
    assert_eq!(payload.tool.as_deref(), Some("fail"));

    // Feed the full block stream through the LLM hydrator — the same
    // function `build_tool_definitions` feeds on every LLM spawn. The
    // notification must surface as a user message with the XML envelope.
    let all_blocks = fx
        .store
        .query_blocks(fx.ctx_id, &BlockFilter::default())
        .expect("query_blocks all");
    let msgs = hydrate_from_blocks(&all_blocks);
    let envelope_msg = msgs.iter().find(|m| {
        m.as_text()
            .map(|t| t.contains("<notification ") && t.contains("</notification>"))
            .unwrap_or(false)
    });
    let text = envelope_msg
        .expect("at least one LLM message must carry the notification envelope")
        .as_text()
        .expect("envelope message is text");
    assert!(text.contains("instance=\"test.hydrator\""));
    assert!(text.contains("kind=\"tool_added\""));
    assert!(text.contains("tool=\"fail\""));
}

/// Phase 3 M4 end-to-end: `Broker::read_resource` + `Broker::subscribe`
/// + pump `ResourceUpdated` burst → coalesced child block under the root
/// resource block → LLM hydrator sees the `<resource>` XML envelope →
/// `clear_binding` unsubscribes cleanly.
///
/// Mirrors the Phase 2 `server_notification_reaches_llm_hydrator` pattern
/// but exercises every Phase 3 decision in one test:
///
///   read_resource → root BlockKind::Resource (exit #1, D-41 read path)
///     └─► subscribe + push 25 ResourceUpdated events
///           └─► coalescer (D-45: max_in_window=0) + per-URI key (D-40)
///                 └─► schedule_resource_flush fires exactly once
///                       └─► re-read + insert_resource_block_as with parent
///                             └─► child BlockKind::Resource (exit #2, #3, D-43)
///     └─► clear_binding
///           └─► broker walks subscriptions, calls server.unsubscribe (exit #4, D-44)
///     └─► hydrate_from_blocks
///           └─► user message with `<resource instance="…" uri="…">` envelope (D-34)
#[tokio::test]
async fn resource_updated_threads_child_block_and_llm_sees_it() {
    let fx = setup().await;

    let mock_id = InstanceId::new("test.resource");
    let binding = ContextToolBinding::with_instances(vec![mock_id.clone()]);
    fx.kernel.broker().set_binding(fx.ctx_id, binding).await;

    let mock = Arc::new(LocalMock::new(mock_id.as_str()).with_text_resource(
        "file:///note.md",
        "initial body",
    ));
    let tx = mock.sender();
    let mock_handle = mock.clone();
    fx.kernel
        .broker()
        .register_silently(mock, InstancePolicy::default())
        .await
        .expect("register should succeed");

    let mut call_ctx = CallContext::new(
        fx.exec_ctx.principal_id,
        fx.ctx_id,
        SessionId::new(),
        KernelId::new(),
    );
    call_ctx.cwd = Some("/".into());

    // Exit #1: initial read emits a root Resource block in the context.
    let read_result = fx
        .kernel
        .broker()
        .read_resource(&mock_id, "file:///note.md", &call_ctx)
        .await
        .expect("read_resource");
    assert_eq!(read_result.contents.len(), 1);

    let root_blocks = fx
        .store
        .query_blocks(
            fx.ctx_id,
            &BlockFilter {
                kinds: vec![BlockKind::Resource],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(root_blocks.len(), 1, "expected one root resource block");
    let root_id = root_blocks[0].id;
    assert!(root_blocks[0].parent_id.is_none());

    // Subscribe, then push a burst of updates. D-45 routes every event into
    // the flush window; D-40 keeps the URI window isolated.
    fx.kernel
        .broker()
        .subscribe(&mock_id, "file:///note.md", &call_ctx)
        .await
        .expect("subscribe");
    assert!(mock_handle.is_subscribed("file:///note.md"));

    for _ in 0..25 {
        tx.send(ServerNotification::ResourceUpdated {
            uri: "file:///note.md".to_string(),
        })
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Exits #2 + #3: the burst must produce exactly one additional child
    // resource block parented to the root.
    let all_resource_blocks = fx
        .store
        .query_blocks(
            fx.ctx_id,
            &BlockFilter {
                kinds: vec![BlockKind::Resource],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        all_resource_blocks.len(),
        2,
        "expected root + exactly one coalesced child; got {all_resource_blocks:?}",
    );
    let child = all_resource_blocks
        .iter()
        .find(|b| b.parent_id == Some(root_id))
        .expect("child block must exist under root");
    assert_eq!(
        child.resource.as_ref().unwrap().parent_resource_block_id,
        Some(root_id),
    );

    // D-34: the LLM hydrator must surface both the root and child as
    // `<resource>` envelope user messages.
    let all_blocks = fx
        .store
        .query_blocks(fx.ctx_id, &BlockFilter::default())
        .unwrap();
    let msgs = hydrate_from_blocks(&all_blocks);
    let resource_envelopes: Vec<&str> = msgs
        .iter()
        .filter_map(|m| m.as_text())
        .filter(|t| t.contains("<resource ") && t.contains("</resource>"))
        .collect();
    assert!(
        resource_envelopes.len() >= 2,
        "expected at least one envelope per Resource block; got {resource_envelopes:?}",
    );
    let joined = resource_envelopes.join("\n");
    assert!(joined.contains("instance=\"test.resource\""));
    assert!(joined.contains("uri=\"file:///note.md\""));

    // Exit #4: clear_binding must unsubscribe cleanly on the server side.
    fx.kernel.broker().clear_binding(&fx.ctx_id).await;
    assert!(
        !mock_handle.is_subscribed("file:///note.md"),
        "clear_binding must tear down live subscriptions",
    );
}

// Silence unused-import / Duration warnings if future edits remove the
// timeout-style test.
#[allow(dead_code)]
fn _touch() -> Duration {
    Duration::from_secs(0)
}
