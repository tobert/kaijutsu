//! The Kernel: core primitive of kaijutsu.
//!
//! A kernel owns:
//! - A VFS (MountTable)
//! - State (variables, history, checkpoints)
//! - Tools (execution engines)
//! - LLM providers (for model access)
//! - Control plane (consent mode)

use async_trait::async_trait;
use kaijutsu_types::PrincipalId;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

use kaijutsu_cas::FileStore;

use crate::peers::{InvokeRequest, PeerConfig, PeerError, PeerInfo, PeerRegistry};
use crate::control::ConsentMode;
use crate::drift::{SharedDriftRouter, shared_drift_router};
use crate::execution::{ExecContext, ExecResult};
use crate::flows::{
    SharedBlockFlowBus, SharedEditorFlowBus, SharedTurnFlowBus, shared_block_flow_bus,
    shared_editor_flow_bus, shared_turn_flow_bus,
};
use crate::llm::{LlmRegistry, Provider};
use crate::mcp::Broker;
use crate::state::KernelState;
use crate::vfs::{DirEntry, FileAttr, MountTable, SetAttr, StatFs, VfsOps, VfsResult};

/// The Kernel: fundamental primitive of kaijutsu.
///
/// Everything is a kernel. A kernel:
/// - Owns `/` in its VFS
/// - Can mount worktrees, repos, other kernels
/// - Has a consent mode (collaborative vs autonomous)
/// - Can checkpoint, fork, and thread
pub struct Kernel {
    /// Stable kernel identity — set at construction from the KernelDb
    /// singleton row, immutable thereafter. Used by the wire layer for
    /// `ping`/`bind_kernel` so clients can detect kernel changes.
    id: kaijutsu_types::KernelId,
    /// VFS mount table.
    vfs: Arc<MountTable>,
    /// Kernel state (behind RwLock for interior mutability).
    state: RwLock<KernelState>,
    /// LLM provider registry (behind RwLock for interior mutability).
    llm: RwLock<LlmRegistry>,
    /// Peer registry (behind RwLock for interior mutability).
    peers: RwLock<PeerRegistry>,
    /// Consent mode (collaborative vs autonomous).
    consent_mode: RwLock<ConsentMode>,
    /// FlowBus for block events.
    block_flows: SharedBlockFlowBus,
    /// FlowBus for autonomous turn requests (headless drive). Kernel-side
    /// callers publish here; the server drains it and runs the LLM turn.
    turn_flows: SharedTurnFlowBus,
    /// DriftRouter for cross-context communication.
    drift: SharedDriftRouter,
    /// Content-addressed store for binary blobs (images, etc.).
    cas: Arc<FileStore>,
    /// Image generation backend registry.
    image_backends: RwLock<crate::image::ImageBackendRegistry>,
    /// MCP-centric tool broker (Phase 1; sits alongside the old `tools`
    /// registry until M4 swaps call sites).
    broker: Arc<Broker>,
    /// Kernel-wide timeout policy: kaish-script bounds, LLM streaming,
    /// MCP connect/handshake. Per-instance MCP `call_timeout` overrides live
    /// on `InstancePolicy`.
    timeouts: kaijutsu_types::TimeoutPolicy,
    /// Shared CRDT file-document cache. Both the MCP `builtin.file` tools and
    /// the kaish `MountBackend` resolve through this one instance so a single
    /// real file maps to a single CRDT document regardless of surface. Set
    /// explicitly by the server at startup; lazily initialized from a block
    /// store otherwise (tests, embedded callers).
    file_cache: OnceLock<Arc<crate::file_tools::FileDocumentCache>>,
    /// Per-context latch confirmation nonce stores. kaish is materialized fresh
    /// per MCP `execute`, but a latch nonce issued by one command must be
    /// confirmable by the next. Keying these stores by `ContextId` here — on
    /// the long-lived kernel rather than the ephemeral `EmbeddedKaish` — gives
    /// the nonce the same durable, per-context lifetime that shell vars and cwd
    /// already have. Without it, every `--confirm` lands in a fresh empty store
    /// and reports "invalid nonce".
    nonce_stores: dashmap::DashMap<kaijutsu_types::ContextId, kaish_kernel::nonce::NonceStore>,
    /// Per-context hyoushigi timelines — the live open future for contexts that
    /// own a beat (musician, audio). A context is **armed** by inserting it here;
    /// a context with no entry (every coder) has no timeline and costs nothing.
    /// The beat scheduler in `kaijutsu-server` pumps these; the turn-completion
    /// handler schedules cells onto them. Sharded by `ContextId` like
    /// `nonce_stores`, each behind a sync mutex (see [`SharedTimeline`]).
    timelines: dashmap::DashMap<kaijutsu_types::ContextId, crate::hyoushigi::SharedTimeline>,
    /// Ingress to the beat scheduler. Installed by the server at startup (the
    /// scheduler lives there, since it needs the block store too). Kernel-side rc
    /// code arms/disarms musician contexts by sending here; absent in embedded /
    /// test setups with no scheduler, where sends are simply no-ops.
    beat_ingress: OnceLock<tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatRequest>>,
    /// RAII guard for a `new_ephemeral()` data dir: removes the throwaway dir
    /// when the kernel drops, so repeated test runs don't accumulate `kj-eph-*`
    /// dirs (each holding a full CAS + DB) in `/tmp`. `None` for kernels rooted
    /// at a caller-provided `data_dir` (production, embedded). `Arc` keeps the dir
    /// alive until the last clone of this guard drops.
    temp_cleanup: Option<std::sync::Arc<TempDirGuard>>,
    /// Kernel key–value store — the persistent, synced `env` (see
    /// `docs/kernel-kv.md`). Late-wired by the server once the `KernelDb` handle
    /// exists (`init_kv`), since the store journals to the oplog. Absent in
    /// embedded/test kernels that never call `init_kv`; `kv()` returns `None`
    /// there and callers degrade rather than panic.
    kv: OnceLock<Arc<crate::kv::Kv>>,
    /// Open in-app editor sessions (`vi`/`kj editor`). The registry is
    /// kernel-owned so any peer can drive it and the app renders it. Behind a
    /// sync mutex because every editor op is synchronous — modalkit's `!Send`
    /// `EditorCore` never crosses an await (see [`crate::editor::SendSessions`]).
    editor_sessions: parking_lot::Mutex<crate::editor::SendSessions>,
    /// FlowBus for editor-session state changes — the push channel the app
    /// renders from. `editor_keys`/`editor_save` publish `StateChanged`,
    /// `editor_quit` publishes `Closed`; the server's `subscribe_editor` bridge
    /// serializes these onto the `EditorEvents` capnp callback.
    editor_flows: SharedEditorFlowBus,
}

/// Removes its directory on drop. A tiny owned guard so `new_ephemeral()` test
/// kernels self-clean their throwaway data dir instead of leaking it for the
/// process lifetime (the `/tmp` inode accumulation that bites repeated local
/// test runs).
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // Best-effort: a failed cleanup must never panic a dropping kernel.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("vfs", &self.vfs)
            .field("state", &"<locked>")
            .field("tools", &"<locked>")
            .field("llm", &"<locked>")
            .field("consent_mode", &"<locked>")
            .field("drift", &"<shared>")
            .finish()
    }
}

/// Default capacity for the block flow bus.
const DEFAULT_FLOW_CAPACITY: usize = 1024;

impl Kernel {
    /// Resolve the CAS base path from a data_dir.
    fn cas_for_data_dir(data_dir: &Path) -> Arc<FileStore> {
        Arc::new(FileStore::at_path(data_dir.join("cas")))
    }

    /// Create a new kernel with the given name.
    ///
    /// `data_dir` is the kernel's on-disk data directory; the frontend owns
    /// resolving it (XDG, config flag, etc.) — the kernel never defaults it,
    /// so a process can't accidentally write into the user's real store. CAS
    /// lives at `{data_dir}/cas/` and creates directories lazily on first write.
    pub async fn new(name: impl Into<String>, data_dir: &Path) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());

        Self {
            id: kaijutsu_types::KernelId::new(),
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            peers: RwLock::new(PeerRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows: shared_block_flow_bus(DEFAULT_FLOW_CAPACITY),
            turn_flows: shared_turn_flow_bus(DEFAULT_FLOW_CAPACITY),
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new({
                let b = Broker::new();
                b.engage_unbound_deny();
                b
            }),
            timeouts: kaijutsu_types::TimeoutPolicy::default(),
            file_cache: OnceLock::new(),
            nonce_stores: dashmap::DashMap::new(),
            timelines: dashmap::DashMap::new(),
            beat_ingress: OnceLock::new(),
            temp_cleanup: None,
            kv: OnceLock::new(),
            editor_sessions: parking_lot::Mutex::new(crate::editor::SendSessions(
                crate::editor::EditorSessions::new(),
            )),
            editor_flows: shared_editor_flow_bus(DEFAULT_FLOW_CAPACITY),
        }
    }

    /// Create a kernel rooted at a throwaway, per-call temp directory.
    ///
    /// For tests and short-lived tooling that need a real on-disk `data_dir`
    /// but must never touch the user's XDG store or share CAS state with any
    /// other kernel. Each call mints a unique `kj-eph-<id>/` under the system
    /// temp dir, isolating every kernel from every other. The dir is removed
    /// when the kernel drops (a `TempDirGuard` on `temp_cleanup`), so repeated
    /// test runs don't accumulate `kj-eph-*` dirs in `/tmp`.
    pub async fn new_ephemeral(name: impl Into<String>) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("kj-eph-{}", kaijutsu_types::KernelId::new().to_hex()));
        std::fs::create_dir_all(&dir).expect("create ephemeral kernel data dir");
        let mut kernel = Self::new(name, &dir).await;
        kernel.temp_cleanup = Some(std::sync::Arc::new(TempDirGuard(dir)));
        kernel
    }

    /// Create a new kernel with a shared FlowBus.
    ///
    /// Use this when you need to share the flow bus with other components
    /// (like BlockStore) before creating the kernel.
    pub async fn with_flows(
        id: kaijutsu_types::KernelId,
        name: impl Into<String>,
        block_flows: SharedBlockFlowBus,
        data_dir: &Path,
    ) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());

        Self {
            id,
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            peers: RwLock::new(PeerRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows,
            turn_flows: shared_turn_flow_bus(DEFAULT_FLOW_CAPACITY),
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new({
                let b = Broker::new();
                b.engage_unbound_deny();
                b
            }),
            timeouts: kaijutsu_types::TimeoutPolicy::default(),
            file_cache: OnceLock::new(),
            nonce_stores: dashmap::DashMap::new(),
            timelines: dashmap::DashMap::new(),
            beat_ingress: OnceLock::new(),
            temp_cleanup: None,
            kv: OnceLock::new(),
            editor_sessions: parking_lot::Mutex::new(crate::editor::SendSessions(
                crate::editor::EditorSessions::new(),
            )),
            editor_flows: shared_editor_flow_bus(DEFAULT_FLOW_CAPACITY),
        }
    }

    /// Stable kernel identity.
    pub fn id(&self) -> kaijutsu_types::KernelId {
        self.id
    }

    /// Get the MCP tool broker (Phase 1).
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// Kernel-wide timeout policy. Read-only today; future revisions will
    /// load this from the config CRDT and expose RPC mutation via the kj CLI.
    pub fn timeouts(&self) -> &kaijutsu_types::TimeoutPolicy {
        &self.timeouts
    }

    /// Builder-style override for the kernel-wide timeout policy. **Must be
    /// called pre-`Arc::new`** — consumes `self` so the type system rejects
    /// post-wrap mutation (production code holds `Arc<Kernel>` and can't get
    /// `&mut`, so a setter method would be unreachable in practice and
    /// misleading to future maintainers).
    ///
    /// Used today by `KjDispatcher::test_dispatcher_with_timeouts`; once the
    /// config CRDT lands, the load path will use the same construction
    /// shape.
    pub fn with_timeouts(mut self, policy: kaijutsu_types::TimeoutPolicy) -> Self {
        self.timeouts = policy;
        self
    }

    /// Builder-style attach of a throwaway-dir cleanup guard (test support).
    /// The given dir is removed when the kernel drops — use it to root a kernel
    /// (and any sibling test scaffolding, e.g. a mounted `/etc/rc` tree) under
    /// one temp dir that self-cleans, instead of leaking it for the process
    /// lifetime. Must be called pre-`Arc::new` (consumes `self`, like
    /// `with_timeouts`). `new_ephemeral` sets this for you; this is for tests
    /// that root a kernel via `new`/`with_flows` at their own temp dir.
    pub fn with_temp_cleanup(mut self, dir: std::path::PathBuf) -> Self {
        self.temp_cleanup = Some(std::sync::Arc::new(TempDirGuard(dir)));
        self
    }

    /// Dispatch a tool call through the broker using the internal
    /// `ExecContext` call-site shape.
    ///
    /// This is the shim kaijutsu-server / kaijutsu-mcp call from the legacy
    /// dispatch sites; it resolves the tool through the context's
    /// `ContextToolBinding`, executes via the broker, and flattens the
    /// `KernelToolResult` back into an `ExecResult` so the surrounding
    /// agentic-loop error handling keeps working without further rewriting.
    ///
    /// Resolves `tool_name` through the context's `ContextToolBinding`,
    /// auto-populating the binding on first call with all registered
    /// instances.
    pub async fn dispatch_tool_via_broker(
        &self,
        tool_name: &str,
        params_json: &str,
        tool_ctx: &ExecContext,
    ) -> Result<ExecResult, crate::mcp::McpError> {
        use tokio_util::sync::CancellationToken;
        // Default path: no propagated cancellation. Callers that need it
        // (LLM streaming) call `dispatch_tool_via_broker_with_cancel`.
        self.dispatch_tool_via_broker_with_cancel(
            tool_name,
            params_json,
            tool_ctx,
            CancellationToken::new(),
        )
        .await
    }

    /// Same as `dispatch_tool_via_broker` but threads an externally-managed
    /// `CancellationToken` into the broker call (M2-B5). Cancelling the token
    /// causes the in-flight broker call to abort within a bounded time.
    pub async fn dispatch_tool_via_broker_with_cancel(
        &self,
        tool_name: &str,
        params_json: &str,
        tool_ctx: &ExecContext,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecResult, crate::mcp::McpError> {
        use crate::mcp::{
            CallContext, InstanceId, KernelCallParams, McpError, ToolContent, TraceContext,
        };

        // Deny-by-default: use whatever binding the context has (assigned by
        // its rc `create`/`fork` lifecycle). No first-touch permissive seeding
        // — an unbound context grants nothing. The resolver still needs the
        // sticky `name_map` populated, so kick `list_visible_tools` to refresh
        // it against the current binding.
        let broker = self.broker.clone();
        let seed_ctx = CallContext::new(
            tool_ctx.principal_id,
            tool_ctx.context_id,
            tool_ctx.session_id,
            tool_ctx.kernel_id,
        );
        let _ = broker
            .list_visible_tools(tool_ctx.context_id, &seed_ctx)
            .await?;
        let binding = broker
            .binding(&tool_ctx.context_id)
            .await
            .unwrap_or_default();

        let (instance, tool) = binding.resolve(tool_name).cloned().ok_or_else(|| {
            McpError::ToolNotFound {
                instance: InstanceId::new(""),
                tool: tool_name.to_string(),
            }
        })?;

        let arguments: serde_json::Value = if params_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(params_json).map_err(McpError::InvalidParams)?
        };

        let call_ctx = CallContext::new(
            tool_ctx.principal_id,
            tool_ctx.context_id,
            tool_ctx.session_id,
            tool_ctx.kernel_id,
        )
        .with_cwd(tool_ctx.cwd.clone())
        .with_trace(TraceContext::from_current_span());

        let result = broker
            .call_tool(
                KernelCallParams {
                    instance,
                    tool,
                    arguments,
                },
                &call_ctx,
                cancel,
            )
            .await?;

        // Flatten KernelToolResult → ExecResult. Preserve the is_error →
        // success=false convention so the existing llm_stream result arm
        // keeps working without modification.
        let mut text = String::new();
        for c in &result.content {
            match c {
                ToolContent::Text(s) => text.push_str(s),
                ToolContent::Json(v) => text.push_str(&v.to_string()),
            }
        }
        if let Some(s) = &result.structured
            && text.is_empty()
        {
            text = serde_json::to_string_pretty(s).unwrap_or_default();
        }
        if result.is_error {
            Ok(ExecResult::failure(1, text))
        } else {
            Ok(ExecResult::success(text))
        }
    }

    /// Enumerate every tool currently registered on the broker, without
    /// binding filtering. Returns `(tool_name, instance, schema,
    /// description)` quadruples. Used by admin/introspection paths (kaish
    /// CLI, capnp `get_tool_schemas`) that want the global surface.
    pub async fn list_all_registered_tools(
        &self,
    ) -> Vec<(String, crate::mcp::InstanceId, serde_json::Value, Option<String>)> {
        use crate::mcp::CallContext;
        let broker = self.broker.clone();
        let ctx = CallContext::new(
            PrincipalId::system(),
            kaijutsu_types::ContextId::new(),
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        let mut out = Vec::new();
        for instance in broker.list_instances().await {
            // Snapshot the server Arc to avoid holding the registry lock
            // across the list_tools await.
            let server = {
                let instances_guard = broker.instances_snapshot().await;
                instances_guard.get(&instance).cloned()
            };
            if let Some(server) = server
                && let Ok(tools) = server.list_tools(&ctx).await
            {
                for kt in tools {
                    out.push((
                        kt.name.clone(),
                        kt.instance.clone(),
                        kt.input_schema,
                        kt.description,
                    ));
                }
            }
        }
        out
    }

    /// List tool definitions visible to a context via the broker.
    /// Auto-populates the binding on first call. Returns `(name, schema,
    /// description)` triples suitable for LLM tool-definition construction.
    pub async fn list_tool_defs_via_broker(
        &self,
        context_id: kaijutsu_types::ContextId,
        principal_id: PrincipalId,
    ) -> Vec<(String, serde_json::Value, Option<String>)> {
        use crate::mcp::CallContext;

        // Deny-by-default: list whatever the context's binding (assigned by its
        // rc lifecycle) allows. No first-touch permissive seeding.
        let broker = self.broker.clone();
        let ctx = CallContext::new(
            principal_id,
            context_id,
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        match broker.list_visible_tools(context_id, &ctx).await {
            Ok(visible) => visible
                .into_iter()
                .map(|(visible_name, kt)| (visible_name, kt.input_schema, kt.description))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Register the Phase 1 builtin virtual MCP servers
    /// (`BlockToolsServer`, `FileToolsServer`, `KernelInfoServer`) on the
    /// broker.
    ///
    /// Callers pass the `SharedBlockStore` + `FileDocumentCache` they already
    /// have (the kernel does not own a `BlockStore`). Safe to call multiple
    /// times — subsequent calls replace the previous registrations.
    ///
    /// Registered under: `builtin.block`, `builtin.file`, `builtin.kernel_info`.
    pub async fn register_builtin_mcp_servers(
        &self,
        documents: crate::block_store::SharedBlockStore,
        file_cache: Arc<crate::file_tools::FileDocumentCache>,
        workspace_guard: Option<crate::file_tools::WorkspaceGuard>,
        kernel_db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
    ) -> crate::mcp::McpResult<()> {
        use crate::mcp::servers::{
            BlockToolsServer, BuiltinBindingsServer, BuiltinHooksServer, BuiltinResourcesServer,
            FileToolsServer, KernelInfoServer,
        };
        use crate::mcp::servers::bindings_builtin::KERNEL_TOOLS_URI;
        use crate::mcp::{InstancePolicy, KernelNotification};
        use crate::mcp::server_like::ServerNotification;

        // Wire the block store into the broker so Phase 2 notification
        // emission can reach bound contexts (D-37). Done before registering
        // so the initial tool snapshots are captured but `register_silently`
        // suppresses the bootstrap ToolAdded noise (D-38).
        self.broker.set_documents(documents.clone()).await;

        // Wire the kernel DB so `ContextToolBinding`s persist (and survive
        // restart) and so binding reads (e.g. fork inheritance via
        // `get_context_binding`) see what `set_binding` wrote.
        self.broker.set_db(kernel_db.clone()).await;

        self.broker
            .register_silently(
                Arc::new(BlockToolsServer::new(documents, self.cas.clone())),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(FileToolsServer::new(
                    file_cache,
                    self.vfs.clone(),
                    workspace_guard,
                )),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(KernelInfoServer::new(self.drift.clone(), kernel_db.clone())),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 3 (D-41): builtin.resources admin server. Weak<Broker> avoids
        // the Arc cycle (broker owns the instance arc, instance refers back).
        self.broker
            .register_silently(
                Arc::new(BuiltinResourcesServer::new(Arc::downgrade(&self.broker))),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 4: builtin.hooks admin server. Same Weak<Broker> pattern.
        // Exposes hook_add / hook_remove / hook_list / hook_inspect so
        // every admin path (LLM / kaish / kj CLI) speaks MCP (D-14).
        self.broker
            .register_silently(
                Arc::new(BuiltinHooksServer::new(Arc::downgrade(&self.broker))),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 5 (D-55): builtin.bindings admin server + kj://kernel/tools
        // resource. The bridge task subscribes to kernel-level ToolsChanged
        // events (fired by `register_inner`/`unregister`) and forwards them
        // to the bindings server's notif channel as `ResourceUpdated`, so
        // subscribers to `kj://kernel/tools` see a child Resource block via
        // the Phase 3 coalescer pipeline. Subscribe BEFORE the bindings
        // server registers so no ToolsChanged event from its own
        // registration is lost; the bridge task is spawned immediately so
        // the receiver drains the broadcast channel as events arrive.
        let bindings_server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&self.broker)));
        let bridge_rx = self.broker.notifications();
        let bridge_tx = bindings_server.resource_update_sender();
        tokio::spawn(async move {
            let mut rx = bridge_rx;
            loop {
                match rx.recv().await {
                    Ok(KernelNotification::ToolsChanged { .. }) => {
                        // Drop-on-no-subscribers is fine: if nobody has read
                        // kj://kernel/tools, there's no resource parent to
                        // thread a child under.
                        let _ = bridge_tx.send(ServerNotification::ResourceUpdated {
                            uri: KERNEL_TOOLS_URI.to_string(),
                        });
                    }
                    Ok(_) => {} // other KernelNotification variants irrelevant
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
        self.broker
            .register_silently(bindings_server, InstancePolicy::for_kernel(self))
            .await?;

        // M3-D2: builtin.tool_search — keyword search across the calling
        // context's visible tools. Holds Weak<Broker> to avoid cycles.
        let tool_search_server = Arc::new(
            crate::mcp::servers::BuiltinToolSearchServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(tool_search_server, InstancePolicy::for_kernel(self))
            .await?;

        // M3-D5: builtin.policy — get/set per-instance InstancePolicy.
        let policy_server = Arc::new(
            crate::mcp::servers::BuiltinPolicyServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(policy_server, InstancePolicy::for_kernel(self))
            .await?;

        // builtin.shell — the in-kernel projection of the `shell` facade as a
        // broker tool, so the native LLM agent gets a shell (the RPC seam alone
        // never reached its tool roster). Gated by `facade:shell` via the
        // binding's facade projection (FACADE_PROJECTED_INSTANCES), NOT a
        // separate instance grant — one capability covers both surfaces. Holds
        // Weak<Broker> to reach the kj dispatcher (wired post-bootstrap by
        // `set_kj_dispatcher`) and materialize a per-context kaish on demand.
        let shell_server = Arc::new(
            crate::mcp::servers::ShellServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(shell_server, InstancePolicy::for_kernel(self))
            .await?;

        // builtin.shell_readonly — the read-only twin (`read_only_shell` tool)
        // for roles that must not write or shell out (the `toolie`). Same
        // facade-projection mechanism (FACADE_PROJECTED_INSTANCES), gated by
        // `facade:shell_readonly`. The constraint rides in the tool *name* so
        // the model never attempts a write it can't perform. A read-only role
        // never grants `facade:shell`, so it gets this shell or the writable
        // one, not both (broad `*`/`facade:*` roles may see both — a harmless
        // strict subset).
        let read_only_shell_server = Arc::new(
            crate::mcp::servers::ShellServer::new_read_only(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(read_only_shell_server, InstancePolicy::for_kernel(self))
            .await?;

        Ok(())
    }

    /// Get the block flows bus.
    pub fn block_flows(&self) -> &SharedBlockFlowBus {
        &self.block_flows
    }

    /// Get the editor flows bus — the editor-state push channel. The server's
    /// `subscribe_editor` bridge subscribes here and forwards to a client.
    pub fn editor_flows(&self) -> &SharedEditorFlowBus {
        &self.editor_flows
    }

    /// Get the turn flows bus (autonomous turn requests).
    pub fn turn_flows(&self) -> &SharedTurnFlowBus {
        &self.turn_flows
    }

    /// Get the drift router.
    pub fn drift(&self) -> &SharedDriftRouter {
        &self.drift
    }

    /// Get the content-addressed store.
    pub fn cas(&self) -> &Arc<FileStore> {
        &self.cas
    }

    /// Get the image backend registry.
    pub fn image_backends(&self) -> &RwLock<crate::image::ImageBackendRegistry> {
        &self.image_backends
    }

    // ========================================================================
    // Hyoushigi timelines (the beat substrate)
    // ========================================================================

    /// Arm a context with a hyoushigi timeline driven by `clock`, registering
    /// the production resolvers onto it and seeding its playhead to `seed`.
    /// Idempotent: a context already armed keeps its live timeline (we never
    /// clobber an open future), so re-arming is a no-op that returns the existing
    /// handle.
    ///
    /// `seed` positions the playhead so musical time stays globally monotone per
    /// context across restarts/rotations (design §4). It is applied **inside**
    /// `or_insert_with` — only on the freshly-constructed (virgin-by-construction)
    /// timeline — so an idempotent re-arm of a LIVE timeline never re-seeds or
    /// rewinds the playhead, preserving the "never clobber an open future"
    /// contract above. `seed_playhead` is virgin-only and `.expect()`s here
    /// because the timeline is brand new; a non-virgin seed would be a kernel bug
    /// (crash over corruption), not a recoverable condition.
    ///
    /// Arming is the *only* thing that gives a context a timeline — a coder is
    /// never armed, so it has no entry and the beat scheduler never wakes for
    /// it ("paused is no heap entry").
    pub fn arm_timeline(
        &self,
        context_id: kaijutsu_types::ContextId,
        clock: kaijutsu_hyoushigi::TickClock,
        seed: kaijutsu_types::Tick,
    ) -> crate::hyoushigi::SharedTimeline {
        self.timelines
            .entry(context_id)
            .or_insert_with(|| {
                let mut tl = kaijutsu_hyoushigi::Timeline::new(clock);
                crate::hyoushigi::register_resolvers(&mut tl, self.cas.clone());
                // Virgin by construction: this closure only fires when the entry
                // is absent, so the seed always lands on a fresh timeline. A
                // re-arm hits the existing entry and skips this block entirely —
                // that is exactly how re-arm avoids re-seeding a live playhead.
                tl.seed_playhead(seed)
                    .expect("freshly-constructed timeline must be virgin for seed_playhead");
                Arc::new(parking_lot::Mutex::new(tl))
            })
            .clone()
    }

    /// Look up a context's timeline, if it is armed. Returns `None` for every
    /// un-armed context (lookup never arms).
    pub fn timeline(
        &self,
        context_id: kaijutsu_types::ContextId,
    ) -> Option<crate::hyoushigi::SharedTimeline> {
        self.timelines.get(&context_id).map(|e| e.value().clone())
    }

    /// Disarm a context: drop its timeline and its open future. Used when a
    /// context is archived. The beat scheduler skips a context with no entry.
    pub fn disarm_timeline(&self, context_id: kaijutsu_types::ContextId) {
        self.timelines.remove(&context_id);
    }

    /// Install the beat-scheduler ingress. Called once by the server at startup.
    /// Returns whether it was set (false if already installed).
    pub fn set_beat_ingress(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatRequest>,
    ) -> bool {
        self.beat_ingress.set(tx).is_ok()
    }

    /// Send a fire-and-forget command to the beat scheduler, if one is installed.
    /// Returns whether it was delivered — `false` when no scheduler is wired
    /// (embedded/test) or the scheduler has shut down. Callers decide whether
    /// that's fatal; arming a musician with no scheduler simply means it never
    /// beats (no silent corruption, just no beat). Use [`send_beat_request`] when
    /// you need to know whether the scheduler actually applied the command.
    ///
    /// [`send_beat_request`]: Self::send_beat_request
    pub fn send_beat_command(&self, cmd: crate::hyoushigi::BeatCommand) -> bool {
        match self.beat_ingress.get() {
            Some(tx) => tx.send(cmd.into()).is_ok(),
            None => false,
        }
    }

    /// Send a command and get a receiver for the scheduler's [`BeatAck`] — the
    /// truthful outcome (`Ok` applied, `Err(reason)` no-op, e.g. not armed). The
    /// scheduler owns the armed map, so this is how `kj transport` reports what
    /// really happened instead of blindly claiming success. `None` when no
    /// scheduler is wired or it has shut down (same meaning as `send_beat_command`
    /// returning `false`).
    ///
    /// [`BeatAck`]: crate::hyoushigi::BeatAck
    pub fn send_beat_request(
        &self,
        cmd: crate::hyoushigi::BeatCommand,
    ) -> Option<tokio::sync::oneshot::Receiver<crate::hyoushigi::BeatAck>> {
        let tx = self.beat_ingress.get()?;
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        let request = crate::hyoushigi::BeatRequest { command: cmd, reply: Some(reply) };
        match tx.send(request) {
            Ok(()) => Some(reply_rx),
            Err(_) => None, // scheduler dropped its receiver
        }
    }

    // ========================================================================
    // Key–value store
    // ========================================================================

    /// Initialize the kernel KV store against `db`. Called once by the server at
    /// startup, where the `KernelDb` handle lives. Builds a persistent [`Kv`]
    /// (rebuilding live state from the oplog) and installs it. Returns whether it
    /// was newly set (`false` if already installed). Fail-loud: a DB error here
    /// surfaces rather than leaving the kernel KV-less.
    pub fn init_kv(
        &self,
        db: crate::block_store::DbHandle,
    ) -> Result<bool, crate::kv::KvError> {
        if self.kv.get().is_some() {
            return Ok(false);
        }
        let kv = crate::kv::Kv::persistent(db, PrincipalId::system())?;
        Ok(self.kv.set(Arc::new(kv)).is_ok())
    }

    /// The kernel KV store, or `None` in an embedded/test kernel that never
    /// called [`init_kv`](Self::init_kv). Callers degrade rather than panic.
    pub fn kv(&self) -> Option<&Arc<crate::kv::Kv>> {
        self.kv.get()
    }

    // ========================================================================
    // Identity
    // ========================================================================

    /// Get the legacy KernelState UUID. Distinct from `Self::id()`, which is
    /// the stable wire-level `KernelId`. KernelState is kept around for
    /// checkpoint/fork bookkeeping; once that surface is retired this can
    /// collapse onto the singleton id.
    pub async fn state_id(&self) -> Uuid {
        self.state.read().await.id
    }

    /// Get the kernel name.
    pub async fn name(&self) -> String {
        self.state.read().await.name.clone()
    }

    /// Set the kernel name.
    pub async fn set_name(&self, name: impl Into<String>) {
        self.state.write().await.name = name.into();
    }

    // ========================================================================
    // VFS
    // ========================================================================

    /// Get the VFS mount table.
    pub fn vfs(&self) -> &Arc<MountTable> {
        &self.vfs
    }

    /// Install the shared CRDT file-document cache. Called once by the server
    /// at startup with the same instance handed to the MCP `builtin.file`
    /// tools, so the kaish `MountBackend` and the tools share one cache.
    /// Returns whether it was set (false if already initialized).
    pub fn set_file_cache(
        &self,
        cache: Arc<crate::file_tools::FileDocumentCache>,
    ) -> bool {
        self.file_cache.set(cache).is_ok()
    }

    /// Get the shared CRDT file-document cache, lazily building one from
    /// `blocks` + the kernel VFS if the server never installed one (tests,
    /// embedded callers). The lazy instance is backed by the same block store
    /// and mount table, so it stays coherent with any other instance over the
    /// shared CRDT documents.
    pub fn file_cache(
        &self,
        blocks: &crate::block_store::SharedBlockStore,
    ) -> Arc<crate::file_tools::FileDocumentCache> {
        self.file_cache
            .get_or_init(|| {
                Arc::new(crate::file_tools::FileDocumentCache::new(
                    blocks.clone(),
                    self.vfs.clone(),
                ))
            })
            .clone()
    }

    // ── Editor sessions ───────────────────────────────────────────────────

    /// Open an in-app editor on `path`, binding to the CRDT block that owns its
    /// text (config/rc → the ConfigCrdtFs block; ordinary file → its file-doc).
    /// Returns the session handle + initial state; fails loud if the path names
    /// no editable document.
    pub async fn editor_open(
        &self,
        path: &str,
        blocks: &crate::block_store::SharedBlockStore,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        self.editor_open_as(path, blocks, None).await
    }

    /// Open an editor recording the [`EditorOpener`](crate::editor::EditorOpener)
    /// on the session, so `fg` can re-foreground it for that principal and
    /// `:r !cmd` can shell out in the opener's context. The signaled front doors
    /// (`vi`/`edit`, `kj editor`, `kj rc edit`) pass the caller here.
    pub async fn editor_open_as(
        &self,
        path: &str,
        blocks: &crate::block_store::SharedBlockStore,
        opener: Option<crate::editor::EditorOpener>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        let file_cache = self.file_cache(blocks);
        // Resolve (the only async step) BEFORE taking the sync mutex, so the
        // `!Send` `EditorCore` never coexists with an await. The mount table is
        // the authority on what owns the path (config-doc backend vs. file).
        let target =
            crate::editor::resolve_editor_target(path, blocks, &file_cache, self.vfs()).await?;
        self.editor_sessions.lock().0.open(path, target, blocks, opener)
    }

    /// Feed keys to an open session, mirroring the edits onto the CRDT block.
    /// Publishes the new state on the editor push channel so every renderer of
    /// this session updates. A `ZZ`/`ZQ` in the batch saves/discards and closes
    /// the session (modalkit disambiguates it from an inserted `ZZ`), publishing
    /// `Closed` instead — so a key forwarder never needs to detect quit itself.
    pub async fn editor_keys(
        &self,
        id: crate::editor::EditorSessionId,
        keys: &str,
        blocks: &crate::block_store::SharedBlockStore,
    ) -> Result<crate::editor::EditorState, String> {
        // Capture the path and feed the keys under one lock — a ZZ/ZQ in the
        // batch drops the session, so the path must be read first. A `:r` read
        // intent is taken here too (only when the session is still open), then
        // fulfilled below: the async fetch happens *outside* the lock, so the
        // `!Send` `EditorCore` never crosses an await (the `SendSessions`
        // invariant); only the fetched `String` does.
        let (path, outcome, io, io_cursor, io_opener) = {
            let mut sessions = self.editor_sessions.lock();
            let path = sessions.0.session_path(id);
            let outcome = sessions.0.keys(id, keys, blocks)?;
            let io = if matches!(outcome, crate::editor::KeysOutcome::Updated(_)) {
                sessions.0.take_io(id)
            } else {
                None
            };
            // Capture the cursor NOW (at `:r` submit), so a keystroke that moves
            // it while the fetch awaits can't make the read land at the wrong
            // place (the "wandering cursor" race) — insert happens at this offset.
            let io_cursor = io.as_ref().and_then(|_| sessions.0.session_cursor(id));
            // The opener context, captured at submit too — `:r !cmd` shells out
            // in it (the caller's context/capabilities, not the edited block's).
            let io_opener = io.as_ref().and_then(|_| sessions.0.session_opener(id));
            (path, outcome, io, io_cursor, io_opener)
        };

        // Fulfill a `:r` read: fetch the content, then splice it at the cursor
        // captured above (not the live cursor, which may have moved).
        if let Some(io) = io {
            let content = self.fetch_editor_io(io, io_opener, blocks).await?;
            let at = io_cursor.unwrap_or(0);
            let state = {
                let mut sessions = self.editor_sessions.lock();
                sessions.0.insert_text(id, &content, at, blocks)?
            };
            // The block changed; drop the file-cache shadow of the *edited* path.
            if let Some(path) = path.as_deref() {
                self.invalidate_config_file_cache(path);
            }
            self.publish_editor_state(id, &state);
            return Ok(state);
        }

        // The mirror (and any ZZ/ZQ rollback) wrote the block; drop the file
        // cache's now-stale shadow so a kaish `cat` re-reads fresh.
        if let Some(path) = path.as_deref() {
            self.invalidate_config_file_cache(path);
        }
        match outcome {
            crate::editor::KeysOutcome::Updated(state) => {
                self.publish_editor_state(id, &state);
                Ok(state)
            }
            crate::editor::KeysOutcome::Closed(state) => {
                self.editor_flows.publish(crate::flows::EditorFlow::Closed {
                    session_id: id.as_u64(),
                });
                Ok(state)
            }
        }
    }

    /// Fetch the content for a `:r` read intent. `:r <file>` reads through the
    /// shared `FileDocumentCache` (the same source the editor and file tools
    /// use). `:r !cmd` materializes a per-context kaish in the *opener's*
    /// `(principal, context_id, session_id)` — the same `materialize_context_kaish`
    /// the model shell and rc lifecycle use — and splices the command's stdout.
    /// Running in the opener's context means the command sees their cwd and
    /// capability allow-set, not the edited block's context. Fails loud (never a
    /// silent empty splice) when there's no opener, no dispatcher, or the command
    /// fails.
    async fn fetch_editor_io(
        &self,
        io: kaijutsu_editor::EditorIo,
        opener: Option<crate::editor::EditorOpener>,
        blocks: &crate::block_store::SharedBlockStore,
    ) -> Result<String, String> {
        match io {
            kaijutsu_editor::EditorIo::ReadFile(path) => {
                self.file_cache(blocks).read_content(&path).await
            }
            kaijutsu_editor::EditorIo::ReadShell(cmd) => {
                // No opener (a headless driver / wire open) → no context to run
                // in. Fail loud pointing at the interactive shell, as before.
                let opener = opener.ok_or_else(|| {
                    format!(
                        "editor: ':r !{cmd}' needs an opener context — open via \
                         vi/edit, or use Ctrl+Z to the shell"
                    )
                })?;
                let dispatcher = self.broker.kj_dispatcher().await.ok_or_else(|| {
                    "editor: ':r !cmd' unavailable — kj dispatcher not wired".to_string()
                })?;
                let kaish = dispatcher
                    .materialize_context_kaish(
                        "editor-read",
                        opener.principal,
                        opener.context_id,
                        opener.session_id,
                        dispatcher.semantic_index(),
                        dispatcher.block_source(),
                    )
                    .await
                    .map_err(|e| format!("editor: ':r !{cmd}' materialize shell: {e}"))?;
                let result = kaish
                    .execute_with_options(&cmd, kaish_kernel::ExecuteOptions::default())
                    .await
                    .map_err(|e| format!("editor: ':r !{cmd}' failed: {e}"))?;
                if result.code != 0 {
                    return Err(format!(
                        "editor: ':r !{cmd}' exited {}: {}",
                        result.code,
                        result.err.trim()
                    ));
                }
                Ok(result.text_out().into_owned())
            }
        }
    }

    /// Current state of an open session.
    pub fn editor_state(
        &self,
        id: crate::editor::EditorSessionId,
    ) -> Result<crate::editor::EditorState, String> {
        self.editor_sessions.lock().0.state(id)
    }

    /// `ZZ` — checkpoint the session's buffer as saved. Publishes the now-clean
    /// state (dirty flips false) so renderers reflect the save.
    pub fn editor_save(
        &self,
        id: crate::editor::EditorSessionId,
    ) -> Result<crate::editor::EditorState, String> {
        let state = self.editor_sessions.lock().0.save(id)?;
        self.publish_editor_state(id, &state);
        Ok(state)
    }

    /// `ZQ` — roll the block back to the session's checkpoint and close it.
    /// Publishes `Closed` so renderers drop the session.
    pub fn editor_quit(
        &self,
        id: crate::editor::EditorSessionId,
        blocks: &crate::block_store::SharedBlockStore,
    ) -> Result<(), String> {
        let path = {
            let mut sessions = self.editor_sessions.lock();
            let path = sessions.0.session_path(id);
            sessions.0.quit(id, blocks)?;
            path
        };
        // The rollback wrote the block; drop the file cache's stale shadow.
        if let Some(path) = path.as_deref() {
            self.invalidate_config_file_cache(path);
        }
        self.editor_flows.publish(crate::flows::EditorFlow::Closed {
            session_id: id.as_u64(),
        });
        Ok(())
    }

    /// Drop the shared [`FileDocumentCache`] shadow for an editor-written
    /// **config** path, so a kaish `cat` / file tool re-reads the just-edited
    /// `ConfigCrdtFs` block instead of a stale copy. Config paths get a separate
    /// `file_context_id` shadow document; a direct editor block write (keyed by
    /// `config_context_id`) leaves it stale, and the symlink-lstat mtime can't
    /// self-heal it. Regular-file editors bind the cache's own block, so they
    /// need no invalidation (a `None`/non-config path is a no-op).
    /// Invalidate the shared [`FileDocumentCache`] shadow for a **config** path
    /// after a write that touched the `ConfigCrdtFs` block **directly** (the vi
    /// editor's block mirror, `kj rc edit/reset/add/rm`, `kj config set/reset`).
    ///
    /// Config paths get a separate `file_context_id` shadow doc that backs the
    /// kaish `cat`/file-tool read path; a direct config-block write leaves it
    /// stale (and the symlink-lstat mtime can't self-heal it). Every such writer
    /// calls this so the next read reloads. A no-op for non-config paths and
    /// before the cache is installed (tests/embedded). Uses the *installed*
    /// cache, so callers don't need a block-store handle.
    pub fn invalidate_config_file_cache(&self, path: &str) {
        if crate::editor::config_owned(path)
            && let Some(cache) = self.file_cache.get()
            && let Err(e) = cache.invalidate_document(path)
        {
            // The cache shadow is now inconsistent with the written config block;
            // a later kaish `cat` could serve stale text. Loud, not swallowed.
            tracing::warn!("failed to invalidate file cache for {path}: {e}");
        }
    }

    /// Publish a session's current state on the editor push channel.
    fn publish_editor_state(
        &self,
        id: crate::editor::EditorSessionId,
        state: &crate::editor::EditorState,
    ) {
        self.editor_flows
            .publish(crate::flows::EditorFlow::StateChanged {
                session_id: id.as_u64(),
                state: state.clone(),
            });
    }

    /// Reconcile open editor sessions after a block's text changed underneath
    /// them (a sibling session, MCP edit, or streaming turn wrote it), and push
    /// the new state for every session that actually moved. Driven by the
    /// server's editor-reconciler task off the block flow; a no-op when nothing
    /// is bound to this block (the common case). A session's *own* mirror write
    /// is skipped (its buffer already matches), so this never echoes a
    /// self-edit. This is the remote-merge half of the push channel — the reason
    /// the editor channel is push, not poll (docs/vi.md step 1b).
    pub fn editor_reconcile_block(
        &self,
        context_id: kaijutsu_types::ContextId,
        block_id: kaijutsu_crdt::BlockId,
        blocks: &crate::block_store::SharedBlockStore,
    ) {
        let changed = self
            .editor_sessions
            .lock()
            .0
            .reconcile_block(context_id, block_id, blocks);
        for (id, state) in &changed {
            self.publish_editor_state(*id, state);
        }
    }

    /// Get the latch-nonce store for a context, creating it on first use.
    ///
    /// The returned `NonceStore` is `Arc`-backed and `Clone`; clones share the
    /// same nonce table. Each `EmbeddedKaish` materialized for `context_id`
    /// injects this clone into its kaish config so a nonce issued by one
    /// command survives to be confirmed by the next, even though the shell
    /// itself is rebuilt per `execute`.
    pub fn nonce_store_for(
        &self,
        context_id: kaijutsu_types::ContextId,
    ) -> kaish_kernel::nonce::NonceStore {
        self.nonce_stores
            .entry(context_id)
            .or_insert_with(kaish_kernel::nonce::NonceStore::new)
            .clone()
    }

    /// Mount a filesystem at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: impl VfsOps + 'static,
    ) -> bool {
        self.vfs.mount(path, fs).await
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount_arc(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: Arc<dyn VfsOps>,
    ) -> bool {
        self.vfs.mount_arc(path, fs).await
    }

    /// Unmount a filesystem.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        self.vfs.unmount(path).await
    }

    /// Freeze the mount table — no more mount/unmount after this.
    pub fn freeze_mounts(&self) {
        self.vfs.freeze();
    }

    /// List all mounts.
    pub async fn list_mounts(&self) -> Vec<crate::vfs::MountInfo> {
        self.vfs.list_mounts().await
    }

    // ========================================================================
    // State
    // ========================================================================

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<String> {
        self.state.read().await.get_var(name).map(|s| s.to_string())
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: impl Into<String>, value: impl Into<String>) {
        self.state.write().await.set_var(name, value);
    }

    /// Unset a variable.
    pub async fn unset_var(&self, name: &str) -> Option<String> {
        self.state.write().await.unset_var(name)
    }

    /// Add a command to history.
    pub async fn add_history(&self, command: impl Into<String>) -> u64 {
        self.state.write().await.add_history(command)
    }

    /// Add a command with result to history.
    pub async fn add_history_with_result(
        &self,
        command: impl Into<String>,
        output: impl Into<String>,
        exit_code: i32,
    ) -> u64 {
        self.state
            .write()
            .await
            .add_history_with_result(command, output, exit_code)
    }

    /// Get recent history.
    pub async fn recent_history(&self, limit: usize) -> Vec<crate::state::HistoryEntry> {
        self.state.read().await.recent_history(limit).to_vec()
    }

    /// Create a checkpoint.
    pub async fn checkpoint(&self, name: impl Into<String>) -> Uuid {
        self.state.write().await.checkpoint(name)
    }

    /// Restore to a checkpoint.
    pub async fn restore_checkpoint(&self, id: Uuid) -> bool {
        self.state.write().await.restore_checkpoint(id)
    }

    // ========================================================================
    // LLM Providers
    // ========================================================================

    /// Register an LLM provider.
    pub async fn register_llm(&self, name: impl Into<String>, provider: Arc<Provider>) {
        self.llm.write().await.register(name, provider);
    }

    /// Set the default LLM provider.
    pub async fn set_default_llm(&self, name: &str) -> bool {
        self.llm.write().await.set_default(name)
    }

    /// Get the LLM registry (for direct access).
    pub fn llm(&self) -> &RwLock<LlmRegistry> {
        &self.llm
    }

    /// List registered LLM providers.
    pub async fn list_llm_providers(&self) -> Vec<String> {
        self.llm
            .read()
            .await
            .list()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ========================================================================
    // Consent Mode
    // ========================================================================

    /// Get the current consent mode.
    pub async fn consent_mode(&self) -> ConsentMode {
        *self.consent_mode.read().await
    }

    /// Set the consent mode.
    pub async fn set_consent_mode(&self, mode: ConsentMode) {
        *self.consent_mode.write().await = mode;
    }

    // ========================================================================
    // Peers (drift navigation transport)
    // ========================================================================

    /// Attach a peer to this kernel.
    ///
    /// The optional `invoke_sender` enables kernel → peer invocation.
    pub async fn attach_peer(
        &self,
        config: PeerConfig,
        invoke_sender: Option<tokio::sync::mpsc::Sender<InvokeRequest>>,
    ) -> Result<PeerInfo, PeerError> {
        self.peers.write().await.attach(config, invoke_sender)
    }

    /// Invoke a peer by nick.
    ///
    /// Dispatches the request to the peer's registered channel and awaits
    /// the response. The kernel-side timeout (30s) is a safety net — the
    /// client-side timeout (15s) should fire first, producing a clean
    /// `Disconnected` rather than `Timeout`.
    pub async fn invoke_peer(
        &self,
        nick: &str,
        action: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, PeerError> {
        let sender = {
            let registry = self.peers.read().await;
            registry
                .get_invoke_sender(nick)
                .ok_or_else(|| PeerError::NotFound(nick.to_string()))?
        };
        // RwLock released before the async send
        let result = Self::send_invoke(&sender, action, params, nick).await;
        if matches!(result, Err(PeerError::Disconnected(_))) {
            // The bridge task is gone — its self-detach on conn_cancel should
            // have removed it, but reap as a backstop so a dead window can't
            // linger in the registry (and out of fan-out).
            self.peers.write().await.reap_closed();
        }
        result
    }

    /// Send one invoke request to an already-resolved peer channel and await the
    /// reply. Shared by [`invoke_peer`](Self::invoke_peer) (single nick target)
    /// and [`signal_open_editor`](Self::signal_open_editor) (principal fan-out).
    /// `label` is only for error context.
    async fn send_invoke(
        sender: &tokio::sync::mpsc::Sender<InvokeRequest>,
        action: &str,
        params: Vec<u8>,
        label: &str,
    ) -> Result<Vec<u8>, PeerError> {
        const PEER_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = InvokeRequest {
            action: action.to_string(),
            params,
            reply: reply_tx,
        };

        if sender.send(request).await.is_err() {
            return Err(PeerError::Disconnected(format!("{label}: channel closed")));
        }

        let response = tokio::time::timeout(PEER_INVOKE_TIMEOUT, reply_rx)
            .await
            .map_err(|_| {
                PeerError::Timeout(format!(
                    "{label}: no reply after {}s",
                    PEER_INVOKE_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|_| PeerError::Disconnected(format!("{label}: handler dropped reply")))?;

        response.result.map_err(PeerError::InvocationFailed)
    }

    /// Signal app renderers to open on `session`/`path` — the `open_editor` peer
    /// nudge that pops a `Screen::Editor`. **Submitter-aware:** fans out to the
    /// submitter principal's app windows (the server-stamped principal — the
    /// app-id addressing infra), falling back to the well-known
    /// [`APP_PEER_NICK`](crate::editor::APP_PEER_NICK) when that principal owns
    /// no window (e.g. a model running `vi` headless).
    ///
    /// Best-effort: the editor session is already open, so a missing or
    /// unreachable renderer is **logged, never fatal** — a headless driver
    /// (`kj editor keys …`) needs no app. Observable (a warn line), not silent.
    ///
    /// Exact-window targeting (by the submitter's `instance`) is a follow-up:
    /// the app's `instance` is not yet threaded onto the execute path
    /// (`ConnectionState`→`ExecContext`), so principal fan-out is the current
    /// precision. See `docs/vi.md`.
    pub async fn signal_open_editor(
        &self,
        session: crate::editor::EditorSessionId,
        path: &str,
        state: &crate::editor::EditorState,
        submitter: Option<kaijutsu_types::PrincipalId>,
    ) {
        // Carry the initial state in the signal so the renderer has text to draw
        // the instant it lands — no fetch, no race against the first push. Reuses
        // the shared `EditorState::to_json` shape (`{session,text,cursor,mode,dirty}`)
        // plus the path; subsequent `editor.state_changed` pushes carry updates.
        let mut params_json = state.to_json(session);
        params_json["path"] = serde_json::Value::String(path.to_string());
        let params = match serde_json::to_vec(&params_json) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("open_editor: failed to encode signal params: {e}");
                return;
            }
        };

        // Target the submitter's app windows; fall back to the well-known nick.
        let targets = {
            let reg = self.peers.read().await;
            let by_principal = submitter
                .map(|p| reg.senders_by_principal(p))
                .unwrap_or_default();
            if by_principal.is_empty() {
                reg.get_invoke_sender(crate::editor::APP_PEER_NICK)
                    .into_iter()
                    .collect()
            } else {
                by_principal
            }
        };

        if targets.is_empty() {
            tracing::warn!(
                "open_editor: no app peer to signal for session {} (headless?) — \
                 the session is open; drive it with `kj editor keys {0} …`",
                session.as_u64()
            );
            return;
        }

        for sender in &targets {
            if let Err(e) = Self::send_invoke(sender, "open_editor", params.clone(), "open_editor").await
            {
                tracing::warn!("open_editor: signal to an app window failed (non-fatal): {e}");
            }
        }
    }

    /// [`editor_open`](Self::editor_open) **plus** the `open_editor` peer signal
    /// to the submitter's app windows. The ergonomic front doors (`vi`/`edit`,
    /// `kj editor open`, `kj rc edit`) use this so a human's `vi foo` pops a
    /// renderer; the wire `editorOpen` handler and tests call the plain
    /// `editor_open` (they are the renderer / a driver and need no nudge). One
    /// signal site, threaded the submitter principal from each door's caller.
    pub async fn editor_open_signaled(
        &self,
        path: &str,
        blocks: &crate::block_store::SharedBlockStore,
        opener: Option<crate::editor::EditorOpener>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        // Record the full opener so `fg` and `:r !cmd` can find the caller's
        // session + context later; the open_editor signal fans only to their
        // app windows, so it needs just the principal.
        let submitter = opener.map(|o| o.principal);
        let (id, state) = self.editor_open_as(path, blocks, opener).await?;
        self.signal_open_editor(id, path, &state, submitter).await;
        Ok((id, state))
    }

    /// `fg` — re-foreground the submitter's most-recently-opened editor session
    /// (job-control resume after a Ctrl+Z suspend). Re-fires the existing
    /// `open_editor` signal with the session's *current* state, so the app pops
    /// back to `Screen::Editor` via the same landing handler. Fails loud with
    /// "no editor session" when the principal has nothing suspended (so the shell
    /// reports it like bash's `fg: no current job`).
    pub async fn resume_editor(
        &self,
        submitter: Option<kaijutsu_types::PrincipalId>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        let (id, path, state) = {
            let mut sessions = self.editor_sessions.lock();
            // Prefer the caller's own most-recent session — now that the opener
            // is captured at construction on every materialized-shell front door,
            // this is the normal path. The most-recent-of-any fallback remains a
            // shared-trust safety net for a caller with no recorded session (a
            // headless / context-less open) — single-user "the editor" is
            // unambiguous; precise multi-user targeting is a later refinement.
            let found = submitter
                .and_then(|p| sessions.0.latest_session_for(p))
                .or_else(|| sessions.0.latest_session_any());
            let (id, path) = found.ok_or_else(|| "fg: no editor session".to_string())?;
            let state = sessions.0.state(id)?;
            (id, path, state)
        };
        self.signal_open_editor(id, &path, &state, submitter).await;
        Ok((id, state))
    }

    /// Detach a peer from this kernel.
    pub async fn detach_peer(&self, nick: &str) -> Option<PeerInfo> {
        self.peers.write().await.detach(nick)
    }

    /// Detach a peer by key only if `sender` is still its registered channel —
    /// the bridge task's self-detach, safe against a re-attach having replaced
    /// the entry. Returns whether it removed anything.
    pub async fn detach_peer_if_sender(
        &self,
        key: &str,
        sender: &tokio::sync::mpsc::Sender<InvokeRequest>,
    ) -> bool {
        self.peers.write().await.detach_if_sender(key, sender)
    }

    /// Get information about an attached peer.
    pub async fn get_peer(&self, nick: &str) -> Option<PeerInfo> {
        self.peers.read().await.get(nick).cloned()
    }

    /// List all attached peers.
    pub async fn list_peers(&self) -> Vec<PeerInfo> {
        self.peers
            .read()
            .await
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get the peer registry (for direct access).
    pub fn peers(&self) -> &RwLock<PeerRegistry> {
        &self.peers
    }

    /// Count of attached peers.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.count()
    }
}

// Delegate VfsOps to the mount table
#[async_trait]
impl VfsOps for Kernel {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        self.vfs.getattr(path).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        self.vfs.readdir(path).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        self.vfs.read(path, offset, size).await
    }

    async fn readlink(&self, path: &Path) -> VfsResult<std::path::PathBuf> {
        self.vfs.readlink(path).await
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        self.vfs.write(path, offset, data).await
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.create(path, mode).await
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.mkdir(path, mode).await
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.vfs.unlink(path).await
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.vfs.rmdir(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.vfs.rename(from, to).await
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.vfs.truncate(path, size).await
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.vfs.setattr(path, attr).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.vfs.symlink(path, target).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.vfs.link(oldpath, newpath).await
    }

    fn read_only(&self) -> bool {
        self.vfs.read_only()
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        self.vfs.statfs().await
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<std::path::PathBuf>> {
        self.vfs.real_path(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_kernel_creation() {
        let kernel = Kernel::new_ephemeral("test").await;
        assert_eq!(kernel.name().await, "test");
    }

    /// Drive the kernel-owned editor surface end to end: open an rc block, type,
    /// observe state, roll back. Proves the methods + the `!Send` registry
    /// integration work through the shared kernel.
    #[tokio::test]
    async fn editor_session_roundtrip_through_kernel() {
        use crate::block_store::shared_block_store_with_db;
        use crate::kernel_db::KernelDb;
        use crate::runtime::config_crdt_fs::ConfigCrdtFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_crdt::PrincipalId;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;

        // Seed an rc script through its owning ConfigCrdtFs backend.
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);
        ConfigCrdtFs::new(blocks.clone(), "/etc/rc")
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        // Mount it so the resolver's mount-table query routes the path to the
        // config backend (same blocks, so it finds the seeded block).
        kernel
            .mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        let path = "/etc/rc/coder/create/S00.kai";

        // Open → type → state reflects, all through the kernel surface.
        let (id, st) = kernel.editor_open(path, &blocks).await.unwrap();
        assert_eq!(st.text, "hello");
        let st = kernel.editor_keys(id, "iX<Esc>", &blocks).await.unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(st.dirty);
        assert_eq!(kernel.editor_state(id).unwrap().text, "Xhello");

        // ZQ rolls the block back and closes the session.
        kernel.editor_quit(id, &blocks).unwrap();
        let err = kernel.editor_keys(id, "x", &blocks).await.unwrap_err();
        assert!(err.contains("no such session"), "got: {err}");
    }

    #[tokio::test]
    async fn editor_edit_invalidates_the_file_cache_shadow() {
        // A config path gets a *shadow* copy in the FileDocumentCache (keyed by
        // file_context_id) separate from the ConfigCrdtFs block the editor writes
        // (config_context_id). A direct editor block write would leave that shadow
        // stale — so a kaish `cat` after an in-app edit would serve old bytes.
        // Kernel::editor_keys must invalidate the shadow so the next read reloads.
        use crate::block_store::{shared_block_store_with_db, SharedBlockStore};
        use crate::file_tools::FileDocumentCache;
        use crate::kernel_db::KernelDb;
        use crate::runtime::config_crdt_fs::ConfigCrdtFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_crdt::{BlockId, ContextId, PrincipalId};
        use std::path::Path;

        fn block_content(blocks: &SharedBlockStore, ctx: ContextId, block: &BlockId) -> String {
            blocks
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .find(|s| s.id == *block)
                .expect("shadow block present")
                .content
        }

        let kernel = Kernel::new_ephemeral("test").await;
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);

        // Mount the rc backend on the kernel VFS (so the cache reads through it),
        // then seed a config script over the same store.
        kernel
            .mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        ConfigCrdtFs::new(blocks.clone(), "/etc/rc")
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        let path = "/etc/rc/coder/create/S00.kai";

        // One shared file cache over the same store + kernel VFS — the editor's
        // invalidation and our reads must hit the same instance.
        let cache = Arc::new(FileDocumentCache::new(blocks.clone(), kernel.vfs().clone()));
        assert!(kernel.set_file_cache(cache.clone()), "cache installs");

        // Populate the shadow from the source.
        let (sctx, sblock) = cache.get_or_load(path).await.unwrap();
        assert_eq!(
            block_content(&blocks, sctx, &sblock),
            "hello",
            "shadow loads the source content"
        );

        // Edit the config block through the editor (insert X at the front).
        let (id, _) = kernel.editor_open(path, &blocks).await.unwrap();
        kernel.editor_keys(id, "iX<Esc>", &blocks).await.unwrap();

        // The next read must reflect the edit — proving the shadow was dropped and
        // reloaded, not re-served stale. (With a plain cache-entry invalidate the
        // surviving shadow doc would re-serve "hello" and this fails.)
        let (sctx2, sblock2) = cache.get_or_load(path).await.unwrap();
        assert_eq!(
            block_content(&blocks, sctx2, &sblock2),
            "Xhello",
            "kaish read sees the editor's edit after invalidation"
        );
    }

    #[tokio::test]
    async fn editor_colon_r_reads_a_file_into_the_buffer() {
        // `:r <file>` slurps a file's contents at the cursor — the async fetch
        // (read_content via the FileDocumentCache) happens inside Kernel::editor_keys
        // *outside* the session lock; the result mirrors onto the editor's block.
        use crate::block_store::shared_block_store_with_db;
        use crate::file_tools::FileDocumentCache;
        use crate::kernel_db::KernelDb;
        use crate::runtime::config_crdt_fs::ConfigCrdtFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_crdt::PrincipalId;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);

        kernel
            .mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        let rc = ConfigCrdtFs::new(blocks.clone(), "/etc/rc");
        // The block we'll edit, and a separate file we'll read into it.
        rc.write_all(Path::new("coder/create/S00.kai"), b"AB")
            .await
            .unwrap();
        rc.write_all(Path::new("coder/create/snippet.kai"), b"INSERTED")
            .await
            .unwrap();
        let edit_path = "/etc/rc/coder/create/S00.kai";
        let read_path = "/etc/rc/coder/create/snippet.kai";

        let cache = Arc::new(FileDocumentCache::new(blocks.clone(), kernel.vfs().clone()));
        assert!(kernel.set_file_cache(cache.clone()));

        let (id, st) = kernel.editor_open(edit_path, &blocks).await.unwrap();
        assert_eq!(st.text, "AB");

        // Move the cursor one char right (between A and B), then `:r` the file.
        kernel.editor_keys(id, "l", &blocks).await.unwrap();
        let after = kernel
            .editor_keys(id, &format!(":r {read_path}<CR>"), &blocks)
            .await
            .unwrap();
        assert_eq!(after.text, "AINSERTEDB", "file content spliced at the cursor");
        assert!(after.dirty, ":r dirties the buffer");
    }

    #[tokio::test]
    async fn resume_editor_finds_the_opener_session_or_fails_loud() {
        // `fg`: with nothing suspended → fail loud; after a signaled open for a
        // principal → resume_editor returns that session's current state.
        use crate::block_store::shared_block_store_with_db;
        use crate::file_tools::FileDocumentCache;
        use crate::kernel_db::KernelDb;
        use crate::runtime::config_crdt_fs::ConfigCrdtFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_crdt::{ContextId, PrincipalId};
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);
        kernel
            .mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        ConfigCrdtFs::new(blocks.clone(), "/etc/rc")
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        let cache = Arc::new(FileDocumentCache::new(blocks.clone(), kernel.vfs().clone()));
        kernel.set_file_cache(cache);
        let path = "/etc/rc/coder/create/S00.kai";

        // Nothing open at all → fail loud (no session to foreground).
        let me = PrincipalId::system();
        assert!(
            kernel.resume_editor(Some(me)).await.is_err(),
            "fg with nothing open fails loud"
        );
        assert!(kernel.resume_editor(None).await.is_err(), "...regardless of principal");

        // Open as `me` (records the opener), then resume finds it by principal.
        let me_opener = crate::editor::EditorOpener {
            principal: me,
            context_id: ContextId::new(),
            session_id: kaijutsu_types::SessionId::new(),
        };
        let (id, _) = kernel
            .editor_open_as(path, &blocks, Some(me_opener))
            .await
            .unwrap();
        let (resumed_id, st) = kernel.resume_editor(Some(me)).await.unwrap();
        assert_eq!(resumed_id, id, "fg foregrounds the principal's session");
        assert_eq!(st.text, "hello");

        // Shared-trust fallback: even a caller with no recorded session (or no
        // principal at all — e.g. an opener-less open via the external MCP path)
        // resumes the most-recent editor.
        let (fallback_id, _) = kernel.resume_editor(None).await.unwrap();
        assert_eq!(fallback_id, id, "fg falls back to the most-recent editor");
    }

    #[tokio::test]
    async fn editor_colon_r_shell_runs_in_the_opener_context() {
        // `:r !cmd` materializes a kaish in the *opener's* context and splices the
        // command's stdout at the cursor. Wire the dispatcher into the broker (the
        // production shape — `set_self_arc` + `broker().set_kj_dispatcher`) so
        // `fetch_editor_io` can reach it.
        use crate::kj::test_helpers::{
            install_rc_script_file, register_context, test_dispatcher_crdt_rc,
        };
        use kaijutsu_crdt::PrincipalId;
        use kaijutsu_types::SessionId;

        let d = Arc::new(test_dispatcher_crdt_rc().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        let kernel = d.kernel();
        let blocks = d.block_store();

        let path = "/etc/rc/vitest/create/S00-foo.kai";
        install_rc_script_file(&d, path, "hello").await;

        // A real registered context for `:r !cmd` to run in.
        let principal = PrincipalId::system();
        let context_id = register_context(&d, Some("vi-r"), None, principal);
        let opener = crate::editor::EditorOpener {
            principal,
            context_id,
            session_id: SessionId::new(),
        };

        let (id, _) = kernel
            .editor_open_as(path, blocks, Some(opener))
            .await
            .unwrap();
        // `:r !echo hi` splices the command's stdout at the cursor (buffer top).
        let state = kernel
            .editor_keys(id, ":r !echo hi<CR>", blocks)
            .await
            .unwrap();
        assert!(
            state.text.contains("hi"),
            "':r !echo' must splice command stdout: {:?}",
            state.text
        );
    }

    #[tokio::test]
    async fn editor_colon_r_shell_without_opener_fails_loud() {
        // A headless open (no opener) has no context to shell out in — `:r !cmd`
        // must fail loud pointing at the interactive shell, never silently no-op.
        use crate::block_store::shared_block_store_with_db;
        use crate::file_tools::FileDocumentCache;
        use crate::kernel_db::KernelDb;
        use crate::runtime::config_crdt_fs::ConfigCrdtFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_crdt::PrincipalId;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws, creator);
        kernel
            .mount("/etc/rc", ConfigCrdtFs::new(blocks.clone(), "/etc/rc"))
            .await;
        ConfigCrdtFs::new(blocks.clone(), "/etc/rc")
            .write_all(Path::new("coder/create/S00.kai"), b"hi")
            .await
            .unwrap();
        let cache = Arc::new(FileDocumentCache::new(blocks.clone(), kernel.vfs().clone()));
        kernel.set_file_cache(cache);

        // `editor_open` records no opener.
        let (id, _) = kernel
            .editor_open("/etc/rc/coder/create/S00.kai", &blocks)
            .await
            .unwrap();
        let err = kernel
            .editor_keys(id, ":r !date<CR>", &blocks)
            .await
            .unwrap_err();
        assert!(err.contains("needs an opener context"), "got: {err}");
    }

    #[tokio::test]
    async fn test_variables() {
        let kernel = Kernel::new_ephemeral("test").await;

        kernel.set_var("FOO", "bar").await;
        assert_eq!(kernel.get_var("FOO").await, Some("bar".to_string()));

        kernel.unset_var("FOO").await;
        assert_eq!(kernel.get_var("FOO").await, None);
    }

    #[tokio::test]
    async fn test_history() {
        let kernel = Kernel::new_ephemeral("test").await;

        kernel.add_history("echo hello").await;
        kernel.add_history("ls -la").await;

        let history = kernel.recent_history(10).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].command, "echo hello");
    }

    #[tokio::test]
    async fn test_llm_provider() {
        let kernel = Kernel::new_ephemeral("test").await;

        // Register a provider (uses fake key, won't actually call API)
        let provider = Arc::new(Provider::Claude(crate::llm::claude::Client::new(
            "fake-key",
        )));
        kernel.register_llm("anthropic", provider).await;
        kernel.set_default_llm("anthropic").await;

        // Check provider is listed
        let providers = kernel.list_llm_providers().await;
        assert_eq!(providers, vec!["anthropic"]);
    }

    #[tokio::test]
    async fn test_llm_no_provider() {
        let kernel = Kernel::new_ephemeral("test").await;

        // Should fail gracefully without provider
        let result = kernel.llm().read().await.prompt("Hello").await;
        assert!(result.is_err());
    }
}
