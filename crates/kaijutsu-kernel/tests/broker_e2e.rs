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
async fn tool_search_returns_scored_matches() {
    // M3-D2: tool_search runs against the calling context's visible
    // tools and returns highest-scoring matches in JSON.
    let fx = setup().await;

    // Seed the binding so every builtin (including builtin.tool_search)
    // is visible.
    let _ = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;

    let exec = fx
        .kernel
        .dispatch_tool_via_broker(
            "tool_search",
            &serde_json::json!({"query": "block"}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("dispatch");
    assert!(exec.success, "tool_search should succeed: {}", exec.stderr);

    let payload: serde_json::Value =
        serde_json::from_str(&exec.stdout).expect("structured output is JSON");
    let matches = payload
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches array");
    assert!(!matches.is_empty(), "expected at least one match for 'block'");
    // First entry has the highest score (block_* tools have 'block' in
    // the name, so name-match scoring should put them on top).
    let first = matches.first().unwrap();
    assert!(
        first
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .contains("block"),
        "top match should contain 'block' in name, got: {first:?}"
    );
}

#[tokio::test]
async fn policy_show_and_set_round_trip() {
    // M3-D5: builtin.policy exposes get/set for InstancePolicy. Show
    // returns the current policy; set mutates call_timeout_ms /
    // max_result_bytes in place; subsequent show reflects the change.
    let fx = setup().await;
    // Seed binding so builtin.policy is visible.
    let _ = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;

    // Show: builtin.kernel_info should have the default policy.
    let show = fx
        .kernel
        .dispatch_tool_via_broker(
            "policy_show",
            &serde_json::json!({"instance": "builtin.kernel_info"}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("show");
    assert!(show.success);
    let payload: serde_json::Value = serde_json::from_str(&show.stdout).expect("json");
    let timeout = payload["call_timeout_ms"].as_u64().expect("timeout_ms");
    assert!(timeout > 0, "default timeout should be non-zero");

    // Set a new timeout and verify it round-trips.
    let new_timeout: u64 = 7777;
    let set = fx
        .kernel
        .dispatch_tool_via_broker(
            "policy_set",
            &serde_json::json!({
                "instance": "builtin.kernel_info",
                "call_timeout_ms": new_timeout
            })
            .to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("set");
    assert!(set.success, "policy_set failed: {}", set.stderr);
    let updated: serde_json::Value = serde_json::from_str(&set.stdout).expect("json");
    assert_eq!(updated["call_timeout_ms"].as_u64(), Some(new_timeout));
    assert_eq!(updated["updated"].as_bool(), Some(true));

    // Subsequent show reflects the change.
    let show2 = fx
        .kernel
        .dispatch_tool_via_broker(
            "policy_show",
            &serde_json::json!({"instance": "builtin.kernel_info"}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("show2");
    let payload2: serde_json::Value = serde_json::from_str(&show2.stdout).expect("json");
    assert_eq!(payload2["call_timeout_ms"].as_u64(), Some(new_timeout));
}

#[tokio::test]
async fn personas_list_define_apply_round_trip() {
    // M3-D1: builtin.personas exposes list/define/apply. Defining a
    // persona, applying it, then listing the calling context's tools
    // should reflect the new binding.
    let fx = setup().await;
    // Seed binding so builtin.personas is visible.
    let _ = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;

    // Default seeded archetypes are visible from list.
    let list = fx
        .kernel
        .dispatch_tool_via_broker("personas_list", "{}", &fx.exec_ctx)
        .await
        .expect("list");
    let payload: serde_json::Value = serde_json::from_str(&list.stdout).expect("json");
    let names: Vec<&str> = payload["personas"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    for expected in ["coder", "explorer", "planner"] {
        assert!(
            names.contains(&expected),
            "default persona {expected} missing, got: {names:?}"
        );
    }

    // Define a custom persona.
    let define = fx
        .kernel
        .dispatch_tool_via_broker(
            "personas_define",
            &serde_json::json!({
                "name": "minimal",
                "instances": ["builtin.kernel_info"],
                "description": "Test persona — kernel info only."
            })
            .to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("define");
    assert!(define.success, "define failed: {}", define.stderr);

    // Apply it. Subsequent visible-tool list should be a strict subset.
    let apply = fx
        .kernel
        .dispatch_tool_via_broker(
            "personas_apply",
            &serde_json::json!({"name": "minimal"}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("apply");
    assert!(apply.success, "apply failed: {}", apply.stderr);

    let visible_after = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;
    let names_after: Vec<&str> = visible_after.iter().map(|(n, _, _)| n.as_str()).collect();
    // kernel_info's `whoami` is the persona's own contribution. Block
    // tools (block_create etc.) must drop. `personas_apply` is auto-
    // injected so the model never paints itself into a corner.
    assert!(
        names_after.contains(&"whoami"),
        "expected whoami, got {names_after:?}"
    );
    assert!(
        !names_after.contains(&"block_create"),
        "minimal persona should drop block tools, got {names_after:?}"
    );
    assert!(
        names_after.contains(&"personas_apply"),
        "personas_apply must stay callable post-apply, got {names_after:?}"
    );
    assert!(
        names_after.contains(&"tool_search"),
        "tool_search must stay callable post-apply, got {names_after:?}"
    );
}

#[tokio::test]
async fn seeded_personas_keep_personas_callable() {
    // Regression for the "personas_apply is a one-way trapdoor" bug:
    // each shipped seed must produce a binding from which personas_apply
    // and tool_search are still callable. Earlier the seeds had empty
    // instances, which zeroed out the entire tool surface — including
    // personas_apply itself.
    for persona in ["planner", "coder", "explorer"] {
        let fx = setup().await;
        // Seed the binding so builtin.personas is visible to start.
        let _ = fx
            .kernel
            .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
            .await;

        let apply = fx
            .kernel
            .dispatch_tool_via_broker(
                "personas_apply",
                &serde_json::json!({ "name": persona }).to_string(),
                &fx.exec_ctx,
            )
            .await
            .unwrap_or_else(|e| panic!("apply {persona} dispatch failed: {e:?}"));
        assert!(
            apply.success,
            "apply {persona} reported failure: {}",
            apply.stderr
        );

        // Visible tools after apply must still include personas_apply and
        // tool_search regardless of the persona's literal instance list.
        let visible = fx
            .kernel
            .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
            .await;
        let names: Vec<&str> = visible.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(
            names.contains(&"personas_apply"),
            "{persona}: personas_apply missing post-apply, got {names:?}"
        );
        assert!(
            names.contains(&"tool_search"),
            "{persona}: tool_search missing post-apply, got {names:?}"
        );
    }
}

#[tokio::test]
async fn personas_apply_rejects_empty_instances() {
    // A user-defined persona with no instances must error on apply
    // rather than installing a binding of just the auto-injected guards
    // — that would silently produce a meaningless tool surface.
    let fx = setup().await;
    let _ = fx
        .kernel
        .list_tool_defs_via_broker(fx.ctx_id, fx.exec_ctx.principal_id)
        .await;

    let define = fx
        .kernel
        .dispatch_tool_via_broker(
            "personas_define",
            &serde_json::json!({"name": "blank", "instances": []}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect("define");
    assert!(define.success, "define failed: {}", define.stderr);

    let err = fx
        .kernel
        .dispatch_tool_via_broker(
            "personas_apply",
            &serde_json::json!({"name": "blank"}).to_string(),
            &fx.exec_ctx,
        )
        .await
        .expect_err("apply must reject empty-instances persona");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no instances") || msg.contains("personas_define"),
        "expected error to mention empty instances / personas_define, got: {msg}"
    );
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

// ============================================================================
// Phase 5 M5 — end-to-end binding management scenarios
// ============================================================================

use kaijutsu_kernel::mcp::servers::bindings_builtin::KERNEL_TOOLS_URI;

/// Richer setup that exposes the `KernelDb` handle so tests can tear down
/// a kernel and stand up a second one against the same DB. Sets
/// `broker.set_db()` so `ContextToolBinding` mutations persist.
async fn setup_with_db() -> (Fixture, Arc<parking_lot::Mutex<KernelDb>>) {
    use kaijutsu_kernel::kernel_db::ContextRow;
    use kaijutsu_types::{ConsentMode, ContextState};

    let tmp = tempfile::tempdir().unwrap();
    let kernel = Arc::new(Kernel::new("phase5-m5", Some(tmp.path())).await);

    let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
    let creator = PrincipalId::system();
    let kernel_id = KernelId::new();
    let ws_id = {
        let g = db.lock();
        g.get_or_create_default_workspace(kernel_id, creator).unwrap()
    };
    let store: SharedBlockStore =
        shared_block_store_with_db(db.clone(), kernel_id, ws_id, creator);

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
        // Phase 5: the `context_bindings` table FKs to `contexts.context_id`,
        // so we need a matching context row for persistence tests.
        g.insert_context(&ContextRow {
            context_id: ctx_id,
            kernel_id,
            label: None,
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            created_at: now_millis() as i64,
            created_by: creator,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
        })
        .unwrap();
    }
    store.create_document(ctx_id, DocumentKind::Code, None).unwrap();

    let file_cache = Arc::new(FileDocumentCache::new(store.clone(), kernel.vfs().clone()));
    kernel
        .register_builtin_mcp_servers(store.clone(), file_cache, None)
        .await
        .expect("register_builtin_mcp_servers");

    // Wire the DB into the broker so bind/unbind/set_binding persist.
    kernel.broker().set_db(db.clone()).await;

    let exec_ctx = ExecContext::new(creator, ctx_id, "/", SessionId::new(), kernel_id);
    let fx = Fixture {
        kernel,
        ctx_id,
        exec_ctx,
        store,
        _tmp: tmp,
    };
    (fx, db)
}

/// Phase 5 exit criterion #3: a context curated to a specific instance
/// survives a kernel restart via `KernelDb`. After bootstrapping kernel A
/// with a binding to `builtin.file` only, dropping it, and standing up
/// kernel B against the same DB, the calling context sees the curated
/// binding rather than re-defaulting to "bind all registered."
#[tokio::test]
async fn binding_persists_across_kernel_restart() {
    let (fx, db) = setup_with_db().await;
    let ctx_id = fx.ctx_id;

    // Curate: only builtin.file is bound.
    fx.kernel
        .broker()
        .set_binding(
            ctx_id,
            ContextToolBinding::with_instances(vec![InstanceId::new("builtin.file")]),
        )
        .await;

    // Sanity: live broker sees the curated binding (one instance).
    let loaded_live = fx.kernel.broker().binding(&ctx_id).await.unwrap();
    assert_eq!(loaded_live.allowed_instances.len(), 1);
    assert_eq!(loaded_live.allowed_instances[0].as_str(), "builtin.file");

    drop(fx);

    // Stand up a second kernel pointing at the same DB. No call to
    // `set_binding` here — it must hydrate from `KernelDb` on first touch.
    let tmp2 = tempfile::tempdir().unwrap();
    let kernel2 = Arc::new(Kernel::new("phase5-m5-restart", Some(tmp2.path())).await);
    let creator2 = PrincipalId::system();
    let kernel_id2 = KernelId::new();
    let ws_id2 = {
        let g = db.lock();
        g.get_or_create_default_workspace(kernel_id2, creator2).unwrap()
    };
    let store2: SharedBlockStore =
        shared_block_store_with_db(db.clone(), kernel_id2, ws_id2, creator2);
    let file_cache2 = Arc::new(FileDocumentCache::new(store2.clone(), kernel2.vfs().clone()));
    kernel2
        .register_builtin_mcp_servers(store2.clone(), file_cache2, None)
        .await
        .unwrap();
    kernel2.broker().set_db(db.clone()).await;

    let loaded_after_restart = kernel2.broker().binding(&ctx_id).await;
    let loaded = loaded_after_restart.expect("binding should hydrate from DB");
    assert_eq!(
        loaded.allowed_instances.len(),
        1,
        "restart should NOT fall back to bind-all; DB row exists",
    );
    assert_eq!(loaded.allowed_instances[0].as_str(), "builtin.file");
}

/// Phase 5 exit criterion #7: a `ListTools Deny` hook hides a tool from
/// the per-context visible list AND blocks call_tool for that context,
/// while the `kj://kernel/tools` resource stays honest about what's
/// installed. The three-way invariant is the D-56 contract.
#[tokio::test]
async fn list_tools_deny_hides_and_blocks_but_keeps_discovery_honest() {
    use kaijutsu_kernel::mcp::{GlobPattern, HookAction, HookEntry, HookId};

    let (fx, _db) = setup_with_db().await;
    let ctx_id = fx.ctx_id;

    // Bind builtin.file so file_* tools are visible in the context.
    fx.kernel
        .broker()
        .bind(ctx_id, InstanceId::new("builtin.file"))
        .await;

    // Register a ListTools Deny for write.
    fx.kernel
        .broker()
        .hooks()
        .write()
        .await
        .list_tools
        .entries
        .push(HookEntry {
            id: HookId("no-writes".into()),
            match_instance: Some(GlobPattern("builtin.file".into())),
            match_tool: Some(GlobPattern("write".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Deny("read-only context".into()),
            priority: 0,
        });

    // (a) The per-context list must NOT include `write`.
    let call_ctx = {
        let mut c = CallContext::test();
        c.context_id = ctx_id;
        c
    };
    let visible: Vec<String> = fx
        .kernel
        .broker()
        .list_visible_tools(ctx_id, &call_ctx)
        .await
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        !visible.iter().any(|n| n.contains("write")),
        "write must be filtered out of visible set; saw {visible:?}",
    );

    // (b) The binding's `name_map` must have no entry for write,
    // so dispatch via resolved name surfaces ToolNotFound.
    let binding = fx.kernel.broker().binding(&ctx_id).await.unwrap();
    assert!(
        binding.resolve("write").is_none(),
        "ListTools-Denied tool must not land in name_map",
    );

    // (c) `kj://kernel/tools` stays honest about what's installed. The
    // payload is instance-grouped; find builtin.file and verify
    // write still appears in its tools list. Use a system context
    // to read the resource so we're not filtering against any binding.
    let sys_ctx = CallContext::system();
    let server = fx
        .kernel
        .broker()
        .instances_snapshot()
        .await
        .get(&InstanceId::new("builtin.bindings"))
        .cloned()
        .expect("builtin.bindings registered by bootstrap");
    let read = server
        .read_resource(KERNEL_TOOLS_URI, &sys_ctx)
        .await
        .unwrap();
    let content = match &read.contents[0] {
        KernelResourceContents::Text { text, .. } => text.clone(),
        _ => panic!("expected text content"),
    };
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    let file_entry = v["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"].as_str() == Some("builtin.file"))
        .expect("builtin.file must appear in kernel-wide discovery");
    let tool_names: Vec<&str> = file_entry["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        tool_names.contains(&"write"),
        "kj://kernel/tools must stay honest about write being installed; \
         saw {tool_names:?}",
    );
}

/// Phase 5 exit criterion #4: reading `kj://kernel/tools` through the
/// kernel's builtin bindings server returns every registered instance,
/// and the calling-context `bound` flag reflects the live binding.
#[tokio::test]
async fn kernel_tools_resource_end_to_end() {
    let (fx, _db) = setup_with_db().await;
    let ctx_id = fx.ctx_id;

    // Bind one builtin so `bound: true` appears for that entry.
    fx.kernel
        .broker()
        .bind(ctx_id, InstanceId::new("builtin.block"))
        .await;

    let call_ctx = {
        let mut c = CallContext::test();
        c.context_id = ctx_id;
        c
    };
    let server = fx
        .kernel
        .broker()
        .instances_snapshot()
        .await
        .get(&InstanceId::new("builtin.bindings"))
        .cloned()
        .unwrap();
    let read = server.read_resource(KERNEL_TOOLS_URI, &call_ctx).await.unwrap();
    let content = match &read.contents[0] {
        KernelResourceContents::Text { text, .. } => text.clone(),
        _ => panic!("expected text content"),
    };
    let v: serde_json::Value = serde_json::from_str(&content).unwrap();
    let instances = v["instances"].as_array().unwrap();

    // Every builtin bootstrap registers should appear.
    let ids: Vec<&str> = instances
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    for expected in [
        "builtin.block",
        "builtin.file",
        "builtin.kernel_info",
        "builtin.resources",
        "builtin.hooks",
        "builtin.bindings",
    ] {
        assert!(
            ids.contains(&expected),
            "missing {expected} in kernel tools listing; saw {ids:?}",
        );
    }

    // `bound` must be true for builtin.block (we just bound it) and
    // false for at least one other instance the calling context did not
    // bind explicitly.
    let block_entry = instances
        .iter()
        .find(|x| x["id"].as_str() == Some("builtin.block"))
        .unwrap();
    assert_eq!(
        block_entry["bound"].as_bool(),
        Some(true),
        "builtin.block should be bound to the calling context",
    );

    let file_entry = instances
        .iter()
        .find(|x| x["id"].as_str() == Some("builtin.file"))
        .unwrap();
    assert_eq!(
        file_entry["bound"].as_bool(),
        Some(false),
        "builtin.file should NOT be bound",
    );
}

/// Hook persistence end-to-end. Proves the full path:
///   admin `hook_add` → `broker.persist_hook_insert` → SQLite
///   → drop kernel → new kernel `set_db` → `hydrate_hooks_from_db`
///   → `evaluate_phase` → `Denied { by_hook }`.
///
/// Uses a real admin call (not direct `HookTables` poke) on the first
/// kernel so the entire M3 persist path is exercised; uses a real tool
/// call on `builtin.block` on the second kernel so the entire M2
/// hydrate path is exercised. No code-level shortcuts through the
/// in-memory table.
#[tokio::test]
async fn hooks_persist_across_kernel_restart() {
    let (fx, db) = setup_with_db().await;
    let sys = CallContext::system();

    // --- Kernel A: install a PreCall Deny on builtin.block via admin. ---
    let add = fx
        .kernel
        .broker()
        .call_tool(
            KernelCallParams {
                instance: InstanceId::new("builtin.hooks"),
                tool: "hook_add".into(),
                arguments: serde_json::json!({
                    "phase": "pre_call",
                    "match_instance": "builtin.block",
                    "hook_id": "no-block-tools",
                    "action": { "type": "deny", "reason": "persisted across restart" },
                }),
            },
            &sys,
            CancellationToken::new(),
        )
        .await
        .expect("hook_add admin call must succeed in kernel A");
    assert!(!add.is_error);

    // Kernel A: the hook is live — a builtin.block call is Denied.
    let err_a = fx
        .kernel
        .broker()
        .call_tool(
            KernelCallParams {
                instance: InstanceId::new("builtin.block"),
                tool: "block_list".into(),
                arguments: serde_json::json!({}),
            },
            &sys,
            CancellationToken::new(),
        )
        .await
        .expect_err("PreCall Deny should block the call");
    assert!(
        matches!(&err_a, McpError::Denied { by_hook } if by_hook.0 == "no-block-tools"),
        "kernel A should Deny with by_hook=no-block-tools, got {err_a:?}",
    );

    // Sanity: the DB has the persisted row before we tear down.
    let rows_pre = db.lock().load_all_hooks().unwrap();
    assert_eq!(rows_pre.len(), 1);
    assert_eq!(rows_pre[0].hook_id, "no-block-tools");
    assert_eq!(rows_pre[0].action_kind, "deny");

    // --- Drop kernel A. Hooks are in-memory; only the DB survives. ---
    drop(fx);

    // --- Kernel B: fresh kernel, same DB. ---
    let tmp2 = tempfile::tempdir().unwrap();
    let kernel2 = Arc::new(Kernel::new("hook-persist-restart", Some(tmp2.path())).await);
    let creator2 = PrincipalId::system();
    let kernel_id2 = KernelId::new();
    let ws_id2 = {
        let g = db.lock();
        g.get_or_create_default_workspace(kernel_id2, creator2).unwrap()
    };
    let store2: SharedBlockStore =
        shared_block_store_with_db(db.clone(), kernel_id2, ws_id2, creator2);
    let file_cache2 = Arc::new(FileDocumentCache::new(store2.clone(), kernel2.vfs().clone()));
    kernel2
        .register_builtin_mcp_servers(store2.clone(), file_cache2, None)
        .await
        .unwrap();
    // set_db is the hydrate trigger. Before this call the HookTables
    // are empty; after, the persisted Deny is back in place.
    kernel2.broker().set_db(db.clone()).await;

    // Kernel B: same Deny fires on the same instance. No new hook_add
    // was issued — if this passes, the hook came back from the DB.
    let err_b = kernel2
        .broker()
        .call_tool(
            KernelCallParams {
                instance: InstanceId::new("builtin.block"),
                tool: "block_list".into(),
                arguments: serde_json::json!({}),
            },
            &sys,
            CancellationToken::new(),
        )
        .await
        .expect_err("hook should have survived restart");
    assert!(
        matches!(&err_b, McpError::Denied { by_hook } if by_hook.0 == "no-block-tools"),
        "kernel B should Deny with by_hook=no-block-tools after hydrate, got {err_b:?}",
    );

    // Belt-and-braces: the pre_call table on kernel B has exactly the
    // persisted entry (not a drift of the live in-memory count vs the
    // DB) — if a future regression double-loaded we'd see 2 here.
    let hooks = kernel2.broker().hooks().read().await;
    assert_eq!(hooks.pre_call.entries.len(), 1);
    assert_eq!(hooks.pre_call.entries[0].id.0, "no-block-tools");
}
