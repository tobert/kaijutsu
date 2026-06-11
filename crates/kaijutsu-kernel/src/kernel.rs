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
    SharedBlockFlowBus, SharedTurnFlowBus, shared_block_flow_bus, shared_turn_flow_bus,
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
    /// own a beat (composer, audio). A context is **armed** by inserting it here;
    /// a context with no entry (every coder) has no timeline and costs nothing.
    /// The beat scheduler in `kaijutsu-server` pumps these; the turn-completion
    /// handler schedules cells onto them. Sharded by `ContextId` like
    /// `nonce_stores`, each behind a sync mutex (see [`SharedTimeline`]).
    timelines: dashmap::DashMap<kaijutsu_types::ContextId, crate::hyoushigi::SharedTimeline>,
    /// Ingress to the beat scheduler. Installed by the server at startup (the
    /// scheduler lives there, since it needs the block store too). Kernel-side rc
    /// code arms/disarms composer contexts by sending here; absent in embedded /
    /// test setups with no scheduler, where sends are simply no-ops.
    beat_ingress: OnceLock<tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatCommand>>,
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
        }
    }

    /// Create a kernel rooted at a throwaway, per-call temp directory.
    ///
    /// For tests and short-lived tooling that need a real on-disk `data_dir`
    /// but must never touch the user's XDG store or share CAS state with any
    /// other kernel. Each call mints a unique `kj-eph-<id>/` under the system
    /// temp dir, isolating every kernel from every other. The directory is
    /// leaked for the process lifetime — there is no live handle to drop it
    /// out from under the kernel.
    pub async fn new_ephemeral(name: impl Into<String>) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("kj-eph-{}", kaijutsu_types::KernelId::new().to_hex()));
        std::fs::create_dir_all(&dir).expect("create ephemeral kernel data dir");
        Self::new(name, &dir).await
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
        // for roles that must not write or shell out (the `explorer`). Same
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
        tx: tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatCommand>,
    ) -> bool {
        self.beat_ingress.set(tx).is_ok()
    }

    /// Send a command to the beat scheduler, if one is installed. Returns whether
    /// it was delivered — `false` when no scheduler is wired (embedded/test) or
    /// the scheduler has shut down. Callers decide whether that's fatal; arming a
    /// composer with no scheduler simply means it never beats (no silent
    /// corruption, just no beat).
    pub fn send_beat_command(&self, cmd: crate::hyoushigi::BeatCommand) -> bool {
        match self.beat_ingress.get() {
            Some(tx) => tx.send(cmd).is_ok(),
            None => false,
        }
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
        const PEER_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

        let sender = {
            let registry = self.peers.read().await;
            registry
                .get_invoke_sender(nick)
                .ok_or_else(|| PeerError::NotFound(nick.to_string()))?
        };
        // RwLock released before the async send

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = InvokeRequest {
            action: action.to_string(),
            params,
            reply: reply_tx,
        };

        sender
            .send(request)
            .await
            .map_err(|_| PeerError::Disconnected(format!("{}: channel closed", nick)))?;

        let response = tokio::time::timeout(PEER_INVOKE_TIMEOUT, reply_rx)
            .await
            .map_err(|_| {
                PeerError::Timeout(format!(
                    "{}: no reply after {}s",
                    nick,
                    PEER_INVOKE_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|_| PeerError::Disconnected(format!("{}: handler dropped reply", nick)))?;

        response.result.map_err(PeerError::InvocationFailed)
    }

    /// Detach a peer from this kernel.
    pub async fn detach_peer(&self, nick: &str) -> Option<PeerInfo> {
        self.peers.write().await.detach(nick)
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
