//! `Broker` — the one tool-call pipeline (§4.2, D-02).
//!
//! Phase 2 responsibilities (in addition to §4.2 call_tool):
//! - Subscribes to per-server `ServerNotification` streams and synthesizes
//!   per-tool `ToolsChanged` diffs on `register`/`unregister` (D-35).
//! - Coalesces Log/PromptsChanged bursts via `NotificationCoalescer` (§5.3,
//!   D-39) and emits a single summary block on window close.
//! - Routes every emitted notification into `BlockKind::Notification` blocks
//!   in contexts whose binding allows the emitting instance.
//!
//! Out of scope for Phase 2: hook evaluation (tables exist but empty),
//! ResourceUpdated → `BlockKind::Resource` (Phase 3), elicitation live
//! handling (§9, D-25), tool search / late injection (Phase 5).

use std::collections::HashMap;
use std::sync::Arc;

use std::collections::HashSet;

use kaijutsu_types::{BlockId, ContextId, NotificationPayload, ResourcePayload};
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::binding::{ContextToolBinding, ResolvedName};
use super::coalescer::{NotificationCoalescer, ObserveOutcome};
use super::context::CallContext;
use super::error::{McpError, McpResult, PolicyError};
use super::hook_table::HookTables;
use super::policy::InstancePolicy;
use super::server_like::{McpServerLike, ServerNotification};
use super::types::{
    InstanceId, KernelCallParams, KernelNotification, KernelReadResource, KernelResourceContents,
    KernelResourceList, KernelTool, KernelToolResult, LogLevel, NotifKind,
};
use crate::block_store::SharedBlockStore;

/// Default notification channel capacity.
const NOTIF_CAPACITY: usize = 256;

/// Flush-timer key. `uri` is `Some(...)` for `NotifKind::ResourceUpdated`
/// (per-URI coalescing, D-40); `None` for Log / PromptsChanged.
type FlushKey = (InstanceId, NotifKind, Option<String>);

pub struct Broker {
    instances: RwLock<HashMap<InstanceId, Arc<dyn McpServerLike>>>,
    bindings: RwLock<HashMap<ContextId, ContextToolBinding>>,
    policies: RwLock<HashMap<InstanceId, InstancePolicy>>,
    semaphores: RwLock<HashMap<InstanceId, Arc<Semaphore>>>,
    hooks: RwLock<HookTables>,
    coalescer: Arc<NotificationCoalescer>,
    notif_tx: broadcast::Sender<KernelNotification>,
    /// Block store used to emit `BlockKind::Notification` blocks. Set via
    /// `set_documents` at kernel bootstrap (D-37). `None` → emission is a
    /// no-op, which keeps `Broker::new()` workable for tests that don't
    /// care about notifications.
    documents: RwLock<Option<SharedBlockStore>>,
    /// Per-instance pump task that subscribes to
    /// `server.notifications()` and fans events out to emission. Aborted
    /// on `unregister`.
    pump_handles: Mutex<HashMap<InstanceId, JoinHandle<()>>>,
    /// Per-(instance, kind) flush timers used to fire a single Coalesced
    /// summary once a window closes (D-39).
    flush_timers: Mutex<HashMap<FlushKey, JoinHandle<()>>>,
    /// Last-seen tool list per instance, used to diff ToolsChanged into
    /// per-tool ToolAdded/ToolRemoved emissions (D-35).
    tool_snapshots: Mutex<HashMap<InstanceId, Vec<KernelTool>>>,
    /// Phase 3 (D-44): live resource subscriptions tied to `ContextToolBinding`.
    /// `clear_binding` / `unregister` walk this table and call
    /// `server.unsubscribe` on each matching entry.
    subscriptions: Mutex<HashMap<ContextId, HashSet<(InstanceId, String)>>>,
    /// Phase 3 (D-43): parent block id for each subscribed resource. Used
    /// when `ResourceUpdated` fires: the re-read emits a child resource
    /// block threaded under the initial read.
    resource_parents: Mutex<HashMap<(ContextId, InstanceId, String), BlockId>>,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker").finish_non_exhaustive()
    }
}

impl Broker {
    pub fn new() -> Self {
        let (notif_tx, _) = broadcast::channel(NOTIF_CAPACITY);
        Self {
            instances: RwLock::new(HashMap::new()),
            bindings: RwLock::new(HashMap::new()),
            policies: RwLock::new(HashMap::new()),
            semaphores: RwLock::new(HashMap::new()),
            hooks: RwLock::new(HookTables::default()),
            coalescer: Arc::new(NotificationCoalescer::default()),
            notif_tx,
            documents: RwLock::new(None),
            pump_handles: Mutex::new(HashMap::new()),
            flush_timers: Mutex::new(HashMap::new()),
            tool_snapshots: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            resource_parents: Mutex::new(HashMap::new()),
        }
    }

    pub fn coalescer(&self) -> &Arc<NotificationCoalescer> {
        &self.coalescer
    }

    pub fn notifications(&self) -> broadcast::Receiver<KernelNotification> {
        self.notif_tx.subscribe()
    }

    /// Wire the block store used to emit `BlockKind::Notification` blocks
    /// (D-37). Call before registering servers at kernel bootstrap; until
    /// this is set, `register`/`unregister` still work but no blocks emit.
    pub async fn set_documents(self: &Arc<Self>, docs: SharedBlockStore) {
        *self.documents.write().await = Some(docs);
    }

    /// Register a server instance under `id`. Replaces any existing instance
    /// with the same id (no implicit merge). Emits a `ToolAdded` notification
    /// block into every bound context for each tool the server advertises
    /// (D-35).
    pub async fn register(
        self: &Arc<Self>,
        server: Arc<dyn McpServerLike>,
        policy: InstancePolicy,
    ) -> McpResult<()> {
        self.register_inner(server, policy, /* emit_tool_added = */ true)
            .await
    }

    /// Like `register` but skips synthetic `ToolAdded` emission (D-38).
    /// Used at kernel bootstrap for the three builtin MCP servers so a
    /// persistent context does not accumulate bootstrap noise on every
    /// restart.
    pub async fn register_silently(
        self: &Arc<Self>,
        server: Arc<dyn McpServerLike>,
        policy: InstancePolicy,
    ) -> McpResult<()> {
        self.register_inner(server, policy, /* emit_tool_added = */ false)
            .await
    }

    async fn register_inner(
        self: &Arc<Self>,
        server: Arc<dyn McpServerLike>,
        policy: InstancePolicy,
        emit_tool_added: bool,
    ) -> McpResult<()> {
        let id = server.instance_id().clone();
        let permits = policy.max_concurrency;

        self.instances.write().await.insert(id.clone(), server.clone());
        self.policies.write().await.insert(id.clone(), policy);
        self.semaphores
            .write()
            .await
            .insert(id.clone(), Arc::new(Semaphore::new(permits)));

        // Snapshot the initial tool list for future diffs (D-35).
        let initial_tools = server
            .list_tools(&CallContext::system())
            .await
            .unwrap_or_default();
        self.tool_snapshots
            .lock()
            .await
            .insert(id.clone(), initial_tools.clone());

        // Emit synthetic ToolAdded blocks for each tool the server exposes,
        // unless this is a silent registration.
        if emit_tool_added {
            for tool in &initial_tools {
                let payload = NotificationPayload {
                    instance: id.as_str().to_string(),
                    kind: kaijutsu_types::NotificationKind::ToolAdded,
                    level: None,
                    tool: Some(tool.name.clone()),
                    count: None,
                    detail: None,
                };
                self.emit_for_bindings(&id, payload).await;
            }
        }

        // Spawn pump subscribed to this server's notification stream. Aborted
        // on unregister (or when the broker is dropped).
        let rx = server.notifications();
        let broker = Arc::clone(self);
        let id_for_pump = id.clone();
        let handle = tokio::spawn(async move {
            pump_loop(broker, id_for_pump, rx).await;
        });
        self.pump_handles.lock().await.insert(id, handle);
        Ok(())
    }

    /// Walk `subscriptions` and call `server.unsubscribe` on every entry
    /// that points at `instance` (Phase 3 M3). Tolerates errors — the server
    /// may already be down. Also drops matching rows from `resource_parents`.
    async fn teardown_subscriptions_for_instance(
        self: &Arc<Self>,
        instance: &InstanceId,
        server: Option<&Arc<dyn McpServerLike>>,
    ) {
        // Collect (context_id, uri) pairs we need to tear down, then drop
        // the lock before awaiting on the server.
        let to_teardown: Vec<(ContextId, String)> = {
            let mut subs = self.subscriptions.lock().await;
            let mut hits = Vec::new();
            for (ctx, set) in subs.iter_mut() {
                let matching: Vec<String> = set
                    .iter()
                    .filter(|(i, _)| i == instance)
                    .map(|(_, uri)| uri.clone())
                    .collect();
                for uri in &matching {
                    hits.push((*ctx, uri.clone()));
                }
                set.retain(|(i, _)| i != instance);
            }
            subs.retain(|_, set| !set.is_empty());
            hits
        };
        // Drop parent-block entries regardless of whether unsubscribe succeeds.
        self.resource_parents
            .lock()
            .await
            .retain(|(_, i, _), _| i != instance);
        if let Some(server) = server {
            for (ctx_id, uri) in to_teardown {
                let sys = CallContext::system_for_context(ctx_id);
                if let Err(e) = server.unsubscribe(&uri, &sys).await {
                    tracing::debug!(
                        context_id = %ctx_id,
                        instance = %instance,
                        uri = %uri,
                        error = ?e,
                        "unsubscribe during unregister failed (best-effort)",
                    );
                }
            }
        }
    }

    pub async fn unregister(self: &Arc<Self>, id: &InstanceId) -> McpResult<()> {
        // Abort the pump first so no new events slip through during teardown.
        if let Some(handle) = self.pump_handles.lock().await.remove(id) {
            handle.abort();
        }
        // Abort any pending flush timers for this instance (D-39, R2).
        let flush_keys: Vec<FlushKey> = self
            .flush_timers
            .lock()
            .await
            .keys()
            .filter(|(i, _, _)| i == id)
            .cloned()
            .collect();
        for key in flush_keys {
            if let Some(h) = self.flush_timers.lock().await.remove(&key) {
                h.abort();
            }
        }

        // Pull the last-seen tool snapshot so we can emit per-tool
        // ToolRemoved events (D-35).
        let removed_tools = self.tool_snapshots.lock().await.remove(id).unwrap_or_default();

        // Remove from `instances` FIRST so any concurrent `subscribe()` that
        // has not yet reached `resolve_instance` fails fast with
        // `InstanceNotFound`. Only then tear down subscriptions using the
        // Arc we took out. Running teardown before the remove left a race
        // window where a concurrent subscribe could: (1) resolve the
        // instance, (2) call `server.subscribe`, and (3) write to
        // `self.subscriptions` AFTER our teardown swept the table, leaving
        // a stale row pointing at a removed instance.
        let server_arc = self.instances.write().await.remove(id);
        self.policies.write().await.remove(id);
        self.semaphores.write().await.remove(id);
        self.teardown_subscriptions_for_instance(id, server_arc.as_ref())
            .await;
        // Defensive second sweep: any `subscribe()` that was already past
        // `resolve_instance` before our remove may have just now finished
        // its `server.subscribe` await and landed its row after the first
        // sweep. Sweep once more to clean those up. Bounded by the
        // in-flight subscribe count at the moment of remove.
        self.teardown_subscriptions_for_instance(id, server_arc.as_ref())
            .await;

        for tool in removed_tools {
            let payload = NotificationPayload {
                instance: id.as_str().to_string(),
                kind: kaijutsu_types::NotificationKind::ToolRemoved,
                level: None,
                tool: Some(tool.name),
                count: None,
                detail: None,
            };
            self.emit_for_bindings(id, payload).await;
        }
        // Bindings keep their stickies; tools-removed error reports at call
        // time (D-06).
        Ok(())
    }

    pub async fn list_instances(&self) -> Vec<InstanceId> {
        self.instances
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }

    /// Clone of the instance registry for callers that want to call
    /// `list_tools` on each server without holding the broker's RwLock.
    pub async fn instances_snapshot(&self) -> HashMap<InstanceId, Arc<dyn McpServerLike>> {
        self.instances.read().await.clone()
    }

    /// Replace a context's binding wholesale. Sticky resolutions on the
    /// incoming binding are preserved as-is; the broker does not recompute.
    pub async fn set_binding(&self, context_id: ContextId, binding: ContextToolBinding) {
        self.bindings.write().await.insert(context_id, binding);
    }

    /// Drop a binding. D-44: walk any live subscriptions for this context
    /// and best-effort unsubscribe on each server. Subscription drops are
    /// not replayed by CRDT — they are a live side effect.
    pub async fn clear_binding(&self, context_id: &ContextId) {
        // Drain the subscription set for this context.
        let pending = self.subscriptions.lock().await.remove(context_id);
        if let Some(set) = pending {
            let system_ctx = CallContext::system_for_context(*context_id);
            for (instance, uri) in set {
                let server = self.instances.read().await.get(&instance).cloned();
                if let Some(server) = server
                    && let Err(e) = server.unsubscribe(&uri, &system_ctx).await
                {
                    tracing::debug!(
                        context_id = %context_id,
                        instance = %instance,
                        uri = %uri,
                        error = ?e,
                        "unsubscribe during clear_binding failed (best-effort)",
                    );
                }
            }
        }
        // Drop any parent-block entries for this context.
        self.resource_parents
            .lock()
            .await
            .retain(|(ctx, _, _), _| ctx != context_id);
        self.bindings.write().await.remove(context_id);
    }

    /// Read a context's binding (cloned to keep lock regions small).
    pub async fn binding(&self, context_id: &ContextId) -> Option<ContextToolBinding> {
        self.bindings.read().await.get(context_id).cloned()
    }

    /// Compute the visible tool list for `context_id` by walking the
    /// binding's `allowed_instances` and applying sticky `Auto` resolution
    /// (D-20). Updates the sticky `name_map` side-effectfully with
    /// freshly-resolved names.
    pub async fn list_visible_tools(
        &self,
        context_id: ContextId,
        ctx: &CallContext,
    ) -> McpResult<Vec<(String, KernelTool)>> {
        // Snapshot binding + servers so we don't hold locks across awaits.
        let binding = {
            let guard = self.bindings.read().await;
            guard.get(&context_id).cloned().unwrap_or_default()
        };
        let servers: Vec<Arc<dyn McpServerLike>> = {
            let guard = self.instances.read().await;
            binding
                .allowed_instances
                .iter()
                .filter_map(|id| guard.get(id).cloned())
                .collect()
        };

        // Gather advertised tools from allowed instances.
        let mut all: Vec<KernelTool> = Vec::new();
        for server in servers {
            let tools = server.list_tools(ctx).await?;
            all.extend(tools);
        }

        // Auto-resolve: unqualified if unique across visible set, else
        // qualified as `instance.tool`.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for kt in &all {
            *counts.entry(kt.name.as_str()).or_insert(0) += 1;
        }
        let mut resolutions: Vec<(ResolvedName, String)> = Vec::new();
        for kt in &all {
            let visible = if counts.get(kt.name.as_str()).copied().unwrap_or(0) > 1 {
                format!("{}.{}", kt.instance.as_str(), kt.name)
            } else {
                kt.name.clone()
            };
            resolutions.push(((kt.instance.clone(), kt.name.clone()), visible));
        }

        // Merge stickily into the binding and write back.
        let mut binding = binding;
        binding.apply_resolutions(resolutions);
        self.bindings
            .write()
            .await
            .insert(context_id, binding.clone());

        // Build the visible-name → KernelTool map.
        let mut out: Vec<(String, KernelTool)> = Vec::new();
        for kt in all {
            let key = (kt.instance.clone(), kt.name.clone());
            if let Some((visible_name, _)) = binding
                .name_map
                .iter()
                .find(|(_, v)| **v == key)
            {
                out.push((visible_name.clone(), kt));
            }
        }
        Ok(out)
    }

    /// The one tool-call pipeline. Phase 1 skips hook evaluation (tables
    /// empty) and has no notification emission.
    #[tracing::instrument(
        name = "broker.call_tool",
        skip(self, ctx, cancel),
        fields(
            instance = %params.instance,
            tool = %params.tool,
            context.id = %ctx.context_id,
            principal.id = %ctx.principal_id,
        )
    )]
    pub async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let server = {
            let guard = self.instances.read().await;
            guard
                .get(&params.instance)
                .cloned()
                .ok_or_else(|| McpError::InstanceNotFound(params.instance.clone()))?
        };

        let policy = self
            .policies
            .read()
            .await
            .get(&params.instance)
            .cloned()
            .unwrap_or_default();

        let sem = self
            .semaphores
            .read()
            .await
            .get(&params.instance)
            .cloned();
        let _permit = match sem {
            Some(sem) => match sem.try_acquire_owned() {
                Ok(p) => Some(p),
                Err(_) => {
                    return Err(McpError::Policy(PolicyError::ConcurrencyCap {
                        instance: params.instance.clone(),
                        max: policy.max_concurrency,
                    }));
                }
            },
            None => None,
        };

        let instance_for_timeout = params.instance.clone();
        let timeout_ms = policy.call_timeout.as_millis() as u64;
        let call_fut = async {
            let span = tracing::info_span!(
                "server.call_tool",
                instance = %params.instance,
                tool = %params.tool,
            );
            let _enter = span.enter();
            server.call_tool(params, ctx, cancel).await
        };

        let result = tokio::time::timeout(policy.call_timeout, call_fut)
            .await
            .map_err(|_| {
                McpError::Policy(PolicyError::Timeout {
                    instance: instance_for_timeout.clone(),
                    timeout_ms,
                })
            })??;

        // Crude result-size check — sum textual content. Structured payloads
        // are JSON; serialized len is the size proxy.
        let size = estimate_result_size(&result);
        if size > policy.max_result_bytes {
            return Err(McpError::Policy(PolicyError::ResultTooLarge {
                instance: instance_for_timeout,
                size,
                max: policy.max_result_bytes,
            }));
        }

        Ok(result)
    }

    // ── Resource dispatch (Phase 3 M3) ─────────────────────────────────

    /// Resolve a live instance by id, returning `InstanceNotFound` otherwise.
    async fn resolve_instance(
        &self,
        instance: &InstanceId,
    ) -> McpResult<Arc<dyn McpServerLike>> {
        self.instances
            .read()
            .await
            .get(instance)
            .cloned()
            .ok_or_else(|| McpError::InstanceNotFound(instance.clone()))
    }

    /// List resources advertised by `instance`.
    pub async fn list_resources(
        &self,
        instance: &InstanceId,
        ctx: &CallContext,
    ) -> McpResult<KernelResourceList> {
        let server = self.resolve_instance(instance).await?;
        server.list_resources(ctx).await
    }

    /// Read a single resource by URI and emit a root `BlockKind::Resource`
    /// block into the calling context (exit #1). Records the new block id in
    /// `resource_parents` so a future `ResourceUpdated` flush can emit child
    /// blocks parented to this one (D-43).
    pub async fn read_resource(
        &self,
        instance: &InstanceId,
        uri: &str,
        ctx: &CallContext,
    ) -> McpResult<KernelReadResource> {
        let server = self.resolve_instance(instance).await?;
        let result = server.read_resource(uri, ctx).await?;

        // Emit a root Resource block for the read. Only the first content
        // chunk drives the block payload; additional chunks are rare (rmcp's
        // `ReadResourceResult.contents` is Vec-shaped). If a server returns
        // zero chunks, synthesize an empty text body so the block still
        // surfaces.
        if let Some(docs) = self.documents.read().await.clone() {
            let payload = resource_payload_from_contents(
                instance,
                uri,
                result.contents.first(),
                None,
            );
            let summary = payload.summary_line();
            match docs.insert_resource_block_as(
                ctx.context_id,
                None,
                &payload,
                summary,
                Some(kaijutsu_types::PrincipalId::system()),
            ) {
                Ok(block_id) => {
                    self.resource_parents.lock().await.insert(
                        (ctx.context_id, instance.clone(), uri.to_string()),
                        block_id,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        context_id = %ctx.context_id,
                        instance = %instance,
                        uri = %uri,
                        error = ?e,
                        "failed to emit root resource block",
                    );
                }
            }
        }
        Ok(result)
    }

    /// Subscribe this context to `uri` updates on `instance`. Idempotent at
    /// the caller layer: the HashSet dedupes repeated calls, but the
    /// underlying server call runs on every invocation (servers that track
    /// their own idempotency will no-op).
    ///
    /// If no root Resource block exists for `(context_id, instance, uri)`
    /// yet (i.e. the caller did not first call `read_resource`), we do a
    /// read here so `handle_resource_flush` has a parent to thread child
    /// blocks under. Without this, subscribe-before-read was a silent trap:
    /// `self.subscriptions` would have the entry, but `resource_parents`
    /// would not, and every update would be filtered out in
    /// `handle_resource_flush`.
    pub async fn subscribe(
        &self,
        instance: &InstanceId,
        uri: &str,
        ctx: &CallContext,
    ) -> McpResult<()> {
        let server = self.resolve_instance(instance).await?;

        let has_parent = {
            self.resource_parents.lock().await.contains_key(&(
                ctx.context_id,
                instance.clone(),
                uri.to_string(),
            ))
        };
        if !has_parent {
            self.read_resource(instance, uri, ctx).await?;
        }

        server.subscribe(uri, ctx).await?;
        self.subscriptions
            .lock()
            .await
            .entry(ctx.context_id)
            .or_default()
            .insert((instance.clone(), uri.to_string()));
        Ok(())
    }

    /// Tear down a previously-created subscription. Idempotent: a second
    /// call with the same (instance, uri) is a no-op on the broker's table
    /// and delegates to the server (which may also no-op).
    pub async fn unsubscribe(
        &self,
        instance: &InstanceId,
        uri: &str,
        ctx: &CallContext,
    ) -> McpResult<()> {
        let server = self.resolve_instance(instance).await?;
        server.unsubscribe(uri, ctx).await?;
        if let Some(set) = self.subscriptions.lock().await.get_mut(&ctx.context_id) {
            set.remove(&(instance.clone(), uri.to_string()));
        }
        self.resource_parents
            .lock()
            .await
            .remove(&(ctx.context_id, instance.clone(), uri.to_string()));
        Ok(())
    }

    /// Accessor for the (empty in Phase 1) hook tables.
    pub fn hooks(&self) -> &RwLock<HookTables> {
        &self.hooks
    }

    /// Emit a notification block into every bound context that allows this
    /// instance. Walks bindings (no reverse index in Phase 2 — simple scale).
    /// No-op when `documents` is unset (broker constructed without bootstrap).
    async fn emit_for_bindings(
        &self,
        instance: &InstanceId,
        payload: NotificationPayload,
    ) {
        let docs = match self.documents.read().await.clone() {
            Some(d) => d,
            None => return,
        };
        let contexts: Vec<ContextId> = {
            let guard = self.bindings.read().await;
            guard
                .iter()
                .filter(|(_, b)| b.is_allowed(instance))
                .map(|(id, _)| *id)
                .collect()
        };
        if contexts.is_empty() {
            return;
        }
        let summary = payload.summary_line();
        for ctx in contexts {
            if let Err(e) = docs.insert_notification_block_as(
                ctx,
                None,
                &payload,
                summary.clone(),
                Some(kaijutsu_types::PrincipalId::system()),
            ) {
                tracing::warn!(
                    context_id = %ctx,
                    instance = %instance,
                    error = ?e,
                    "failed to emit notification block",
                );
            }
        }
    }

    /// Schedule a `flush` timer for `(instance, kind, uri)` if none is
    /// pending. When the timer elapses, it calls `coalescer.flush()` and
    /// emits a single Coalesced summary notification block per bound context.
    /// `uri` is `Some(...)` for `NotifKind::ResourceUpdated` (D-40); `None`
    /// for Log / PromptsChanged.
    async fn schedule_flush(
        self: &Arc<Self>,
        instance: InstanceId,
        kind: NotifKind,
        uri: Option<String>,
    ) {
        let key = (instance.clone(), kind, uri.clone());
        let mut timers = self.flush_timers.lock().await;
        if timers.contains_key(&key) {
            return;
        }
        let window = self.coalescer.policy().window;
        let broker = Arc::clone(self);
        let instance_for_task = instance.clone();
        let uri_for_task = uri.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(window).await;
            if let Some(count) = broker.coalescer.flush(
                &instance_for_task,
                kind,
                uri_for_task.as_deref(),
            ) {
                let payload = NotificationPayload {
                    instance: instance_for_task.as_str().to_string(),
                    kind: kaijutsu_types::NotificationKind::Coalesced,
                    level: None,
                    tool: None,
                    count: Some(count),
                    detail: Some(notif_kind_label(kind).to_string()),
                };
                broker.emit_for_bindings(&instance_for_task, payload).await;
            }
            broker
                .flush_timers
                .lock()
                .await
                .remove(&(instance_for_task, kind, uri_for_task));
        });
        timers.insert(key, handle);
    }

    /// Schedule a window-flush timer for `ResourceUpdated` on `(instance, uri)`
    /// (Phase 3 M3, D-43). When the window elapses, the broker re-reads the
    /// URI once and emits a child `BlockKind::Resource` block per subscribed
    /// context. `Broker::coalescer::flush` is called for bookkeeping (clears
    /// the window); the coalesced count is unused because the re-read result
    /// itself stands in for the "N updates happened" signal.
    async fn schedule_resource_flush(
        self: &Arc<Self>,
        instance: InstanceId,
        uri: String,
    ) {
        let key = (
            instance.clone(),
            NotifKind::ResourceUpdated,
            Some(uri.clone()),
        );
        let mut timers = self.flush_timers.lock().await;
        if timers.contains_key(&key) {
            return;
        }
        let window = self.coalescer.policy().window;
        let broker = Arc::clone(self);
        let instance_for_task = instance.clone();
        let uri_for_task = uri.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(window).await;
            // Clear the coalescer window; the return value (count) is only
            // interesting for telemetry — the re-read itself is what we emit.
            let _ = broker.coalescer.flush(
                &instance_for_task,
                NotifKind::ResourceUpdated,
                Some(uri_for_task.as_str()),
            );
            handle_resource_flush(&broker, &instance_for_task, &uri_for_task).await;
            broker.flush_timers.lock().await.remove(&(
                instance_for_task,
                NotifKind::ResourceUpdated,
                Some(uri_for_task),
            ));
        });
        timers.insert(key, handle);
    }
}

/// Long-running task per registered instance. Receives `ServerNotification`
/// events and dispatches them through the coalescer / diff / emission paths.
async fn pump_loop(
    broker: Arc<Broker>,
    id: InstanceId,
    mut rx: broadcast::Receiver<ServerNotification>,
) {
    loop {
        match rx.recv().await {
            Ok(ServerNotification::ToolsChanged) => {
                handle_tools_changed(&broker, &id).await;
            }
            Ok(ServerNotification::Log { level, message, tool }) => {
                match broker.coalescer.observe(&id, NotifKind::Log, None) {
                    ObserveOutcome::PassThrough => {
                        let payload = NotificationPayload {
                            instance: id.as_str().to_string(),
                            kind: kaijutsu_types::NotificationKind::Log,
                            level: Some(log_level_to_types(level)),
                            tool,
                            count: None,
                            detail: Some(message),
                        };
                        broker.emit_for_bindings(&id, payload).await;
                    }
                    ObserveOutcome::StartWindow => {
                        broker
                            .schedule_flush(id.clone(), NotifKind::Log, None)
                            .await;
                    }
                    ObserveOutcome::Coalesced { .. } => {
                        // Already counted inside the coalescer; timer pending.
                    }
                }
            }
            Ok(ServerNotification::PromptsChanged) => {
                match broker
                    .coalescer
                    .observe(&id, NotifKind::PromptsChanged, None)
                {
                    ObserveOutcome::PassThrough => {
                        let payload = NotificationPayload {
                            instance: id.as_str().to_string(),
                            kind: kaijutsu_types::NotificationKind::PromptsChanged,
                            level: None,
                            tool: None,
                            count: None,
                            detail: None,
                        };
                        broker.emit_for_bindings(&id, payload).await;
                    }
                    ObserveOutcome::StartWindow => {
                        broker
                            .schedule_flush(id.clone(), NotifKind::PromptsChanged, None)
                            .await;
                    }
                    ObserveOutcome::Coalesced { .. } => {}
                }
            }
            Ok(ServerNotification::ResourceUpdated { uri }) => {
                // D-45: default policy sets max_in_window=0 for ResourceUpdated,
                // so the first event always returns StartWindow. PassThrough
                // is unreachable under the default policy; if a custom policy
                // raises the cap we just re-read synchronously the same way.
                match broker.coalescer.observe(
                    &id,
                    NotifKind::ResourceUpdated,
                    Some(uri.as_str()),
                ) {
                    ObserveOutcome::PassThrough => {
                        handle_resource_flush(&broker, &id, &uri).await;
                    }
                    ObserveOutcome::StartWindow => {
                        broker
                            .schedule_resource_flush(id.clone(), uri.clone())
                            .await;
                    }
                    ObserveOutcome::Coalesced { .. } => {
                        // Already counted; the pending flush timer will fire
                        // one re-read per (instance, uri) window.
                    }
                }
            }
            Ok(ServerNotification::Elicitation(_)) => {
                // Reserved per D-25; no live handling yet.
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn handle_tools_changed(broker: &Arc<Broker>, id: &InstanceId) {
    // Fetch the server's current tool list under CallContext::system().
    let server = broker.instances.read().await.get(id).cloned();
    let new_tools = match server {
        Some(s) => s
            .list_tools(&CallContext::system())
            .await
            .unwrap_or_default(),
        None => return,
    };
    let old_tools = {
        let mut snaps = broker.tool_snapshots.lock().await;
        let prev = snaps.get(id).cloned().unwrap_or_default();
        snaps.insert(id.clone(), new_tools.clone());
        prev
    };
    let (added, removed) = diff_tools(&old_tools, &new_tools);
    for name in added {
        let payload = NotificationPayload {
            instance: id.as_str().to_string(),
            kind: kaijutsu_types::NotificationKind::ToolAdded,
            level: None,
            tool: Some(name),
            count: None,
            detail: None,
        };
        broker.emit_for_bindings(id, payload).await;
    }
    for name in removed {
        let payload = NotificationPayload {
            instance: id.as_str().to_string(),
            kind: kaijutsu_types::NotificationKind::ToolRemoved,
            level: None,
            tool: Some(name),
            count: None,
            detail: None,
        };
        broker.emit_for_bindings(id, payload).await;
    }
}

/// D-43 flush body: re-read the URI **once** and fan the fresh payload out
/// to every subscribed context as a child `BlockKind::Resource` block under
/// that context's original root read. On re-read failure, emit a `Log
/// { level: Warn }` notification under each parent instead of a fake
/// Resource block.
///
/// One read for N subscribers, not N — otherwise bursty resources get
/// N-amplified against the external server and subscribers can observe
/// divergent content if the resource updates mid-fanout.
async fn handle_resource_flush(broker: &Arc<Broker>, id: &InstanceId, uri: &str) {
    let server = match broker.instances.read().await.get(id).cloned() {
        Some(s) => s,
        None => return,
    };
    // Snapshot the (context, parent_block) pairs we need to emit into.
    let targets: Vec<(ContextId, BlockId)> = {
        let parents = broker.resource_parents.lock().await;
        let subs = broker.subscriptions.lock().await;
        subs.iter()
            .filter_map(|(ctx_id, set)| {
                if set.contains(&(id.clone(), uri.to_string())) {
                    parents
                        .get(&(*ctx_id, id.clone(), uri.to_string()))
                        .copied()
                        .map(|b| (*ctx_id, b))
                } else {
                    None
                }
            })
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let docs = match broker.documents.read().await.clone() {
        Some(d) => d,
        None => return,
    };

    // Single read — broker-internal attribution. Per-context trace
    // divergence is acceptable here since the flush is not caused by any
    // one subscriber.
    let sys = CallContext::system();
    match server.read_resource(uri, &sys).await {
        Ok(result) => {
            let chunk = result.contents.first();
            for (ctx_id, parent_block) in targets {
                let payload =
                    resource_payload_from_contents(id, uri, chunk, Some(parent_block));
                let summary = payload.summary_line();
                if let Err(e) = docs.insert_resource_block_as(
                    ctx_id,
                    Some(&parent_block),
                    &payload,
                    summary,
                    Some(kaijutsu_types::PrincipalId::system()),
                ) {
                    tracing::warn!(
                        context_id = %ctx_id,
                        instance = %id,
                        uri = %uri,
                        error = ?e,
                        "failed to emit child resource block",
                    );
                }
            }
        }
        Err(e) => {
            // D-43 failure path: one Log notification per subscriber,
            // parented under their own root Resource block.
            let detail = format!("re-read of {} failed: {:?}", uri, e);
            for (ctx_id, parent_block) in targets {
                let payload = NotificationPayload {
                    instance: id.as_str().to_string(),
                    kind: kaijutsu_types::NotificationKind::Log,
                    level: Some(kaijutsu_types::LogLevel::Warn),
                    tool: None,
                    count: None,
                    detail: Some(detail.clone()),
                };
                let summary = payload.summary_line();
                if let Err(ee) = docs.insert_notification_block_as(
                    ctx_id,
                    Some(&parent_block),
                    &payload,
                    summary,
                    Some(kaijutsu_types::PrincipalId::system()),
                ) {
                    tracing::warn!(
                        context_id = %ctx_id,
                        instance = %id,
                        uri = %uri,
                        error = ?ee,
                        "failed to emit fallback Log notification",
                    );
                }
            }
        }
    }
}

/// Build a `ResourcePayload` from the first `KernelResourceContents` chunk
/// of a `read_resource` result. Missing contents → empty-text payload so the
/// block still surfaces.
fn resource_payload_from_contents(
    instance: &InstanceId,
    uri: &str,
    chunk: Option<&KernelResourceContents>,
    parent_resource_block_id: Option<BlockId>,
) -> ResourcePayload {
    match chunk {
        Some(KernelResourceContents::Text { mime_type, text, .. }) => ResourcePayload {
            instance: instance.as_str().to_string(),
            uri: uri.to_string(),
            mime_type: mime_type.clone(),
            size: Some(text.len() as u64),
            text: Some(text.clone()),
            blob_base64: None,
            parent_resource_block_id,
        },
        Some(KernelResourceContents::Blob {
            mime_type, blob_base64, ..
        }) => {
            let bytes = blob_base64.len().saturating_mul(3) / 4;
            ResourcePayload {
                instance: instance.as_str().to_string(),
                uri: uri.to_string(),
                mime_type: mime_type.clone(),
                size: Some(bytes as u64),
                text: None,
                blob_base64: Some(blob_base64.clone()),
                parent_resource_block_id,
            }
        }
        None => ResourcePayload {
            instance: instance.as_str().to_string(),
            uri: uri.to_string(),
            mime_type: None,
            size: Some(0),
            text: Some(String::new()),
            blob_base64: None,
            parent_resource_block_id,
        },
    }
}

fn diff_tools(old: &[KernelTool], new: &[KernelTool]) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let old_names: HashSet<&str> = old.iter().map(|t| t.name.as_str()).collect();
    let new_names: HashSet<&str> = new.iter().map(|t| t.name.as_str()).collect();
    let added = new
        .iter()
        .filter(|t| !old_names.contains(t.name.as_str()))
        .map(|t| t.name.clone())
        .collect();
    let removed = old
        .iter()
        .filter(|t| !new_names.contains(t.name.as_str()))
        .map(|t| t.name.clone())
        .collect();
    (added, removed)
}

fn notif_kind_label(kind: NotifKind) -> &'static str {
    match kind {
        NotifKind::Log => "log",
        NotifKind::PromptsChanged => "prompts_changed",
        NotifKind::ResourceUpdated => "resource_updated",
        NotifKind::ToolsChanged => "tools_changed",
        NotifKind::Elicitation => "elicitation",
    }
}

fn log_level_to_types(l: LogLevel) -> kaijutsu_types::LogLevel {
    match l {
        LogLevel::Trace => kaijutsu_types::LogLevel::Trace,
        LogLevel::Debug => kaijutsu_types::LogLevel::Debug,
        LogLevel::Info => kaijutsu_types::LogLevel::Info,
        LogLevel::Warn => kaijutsu_types::LogLevel::Warn,
        LogLevel::Error => kaijutsu_types::LogLevel::Error,
    }
}

fn estimate_result_size(result: &KernelToolResult) -> usize {
    let mut total = 0usize;
    for c in &result.content {
        match c {
            super::types::ToolContent::Text(s) => total += s.len(),
            super::types::ToolContent::Json(v) => total += v.to_string().len(),
        }
    }
    if let Some(v) = &result.structured {
        total += v.to_string().len();
    }
    total
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::mcp::{
        CallContext, KernelToolResult, McpError, PolicyError, ServerNotification, ToolContent,
    };

    /// Closure-driven `McpServerLike` fake. Tests build an instance with
    /// `MockServer::new(id).with_tool(...).on_call(|p| async { ... })`.
    /// Phase 2 extensions: tools are interior-mutable via `set_tools()` so
    /// a test can swap the advertised list and push ToolsChanged, and the
    /// notification sender is exposed via `sender()` so tests can push
    /// events into the pump directly.
    struct MockServer {
        id: InstanceId,
        tools: std::sync::Mutex<Vec<KernelTool>>,
        on_call: Arc<
            dyn Fn(KernelCallParams) -> BoxFuture<'static, McpResult<KernelToolResult>>
                + Send
                + Sync,
        >,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl MockServer {
        fn new(id: &str) -> Self {
            let (notif_tx, _) = broadcast::channel(64);
            Self {
                id: InstanceId::new(id),
                tools: std::sync::Mutex::new(Vec::new()),
                on_call: Arc::new(|_p| Box::pin(async { Ok(KernelToolResult::text("ok")) })),
                notif_tx,
            }
        }

        fn with_tool(self, name: &str) -> Self {
            self.tools.lock().unwrap().push(KernelTool {
                instance: self.id.clone(),
                name: name.to_string(),
                description: None,
                input_schema: json!({ "type": "object" }),
            });
            self
        }

        fn on_call<F, Fut>(mut self, f: F) -> Self
        where
            F: Fn(KernelCallParams) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = McpResult<KernelToolResult>> + Send + 'static,
        {
            self.on_call = Arc::new(move |p| Box::pin(f(p)));
            self
        }

        /// Replace the advertised tool list (for ToolsChanged diff tests).
        fn set_tools(&self, names: &[&str]) {
            let mut tools = self.tools.lock().unwrap();
            tools.clear();
            for name in names {
                tools.push(KernelTool {
                    instance: self.id.clone(),
                    name: (*name).to_string(),
                    description: None,
                    input_schema: json!({ "type": "object" }),
                });
            }
        }

        fn sender(&self) -> broadcast::Sender<ServerNotification> {
            self.notif_tx.clone()
        }
    }

    #[async_trait]
    impl McpServerLike for MockServer {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }

        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(self.tools.lock().unwrap().clone())
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

    fn params(instance: &str, tool: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(instance),
            tool: tool.to_string(),
            arguments: json!({}),
        }
    }

    // ── Phase 3 M2: trait defaults for resource methods ───────────────

    /// A minimal `McpServerLike` that does NOT override the resource
    /// methods. Used to prove the trait default returns `Unsupported`.
    struct BareServer {
        id: InstanceId,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl BareServer {
        fn new(id: &str) -> Self {
            let (notif_tx, _) = broadcast::channel(8);
            Self {
                id: InstanceId::new(id),
                notif_tx,
            }
        }
    }

    #[async_trait]
    impl McpServerLike for BareServer {
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
    }

    #[tokio::test]
    async fn default_resource_methods_return_unsupported() {
        let s = BareServer::new("bare");
        let ctx = CallContext::test();
        assert!(matches!(
            s.list_resources(&ctx).await,
            Err(McpError::Unsupported)
        ));
        assert!(matches!(
            s.read_resource("file:///x", &ctx).await,
            Err(McpError::Unsupported)
        ));
        assert!(matches!(
            s.subscribe("file:///x", &ctx).await,
            Err(McpError::Unsupported)
        ));
        assert!(matches!(
            s.unsubscribe("file:///x", &ctx).await,
            Err(McpError::Unsupported)
        ));
    }

    #[tokio::test]
    async fn instance_not_found_errors() {
        let broker = Arc::new(Broker::new());
        let err = broker
            .call_tool(
                params("nope", "x"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::InstanceNotFound(ref id) if id.as_str() == "nope"),
            "expected InstanceNotFound(nope), got {err:?}"
        );
    }

    #[tokio::test]
    async fn unregister_then_call_errors() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("ephemeral").with_tool("ping"));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let ctx = CallContext::test();
        broker
            .call_tool(
                params("ephemeral", "ping"),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .expect("first call should succeed");

        broker
            .unregister(&InstanceId::new("ephemeral"))
            .await
            .unwrap();

        let err = broker
            .call_tool(
                params("ephemeral", "ping"),
                &ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::InstanceNotFound(_)),
            "expected InstanceNotFound after unregister, got {err:?}"
        );
    }

    #[tokio::test]
    async fn policy_concurrency_cap_fires() {
        // locks broker.rs try_acquire_owned semantics — over-cap callers fail
        // fast rather than queueing.
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("slow")
                .with_tool("work")
                .on_call(|_p| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(KernelToolResult::text("done"))
                }),
        );
        broker
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_secs(5),
                    max_result_bytes: 1024,
                    max_concurrency: 1,
                },
            )
            .await
            .unwrap();

        let b1 = broker.clone();
        let b2 = broker.clone();
        let first = tokio::spawn(async move {
            b1.call_tool(params("slow", "work"), &CallContext::test(), CancellationToken::new())
                .await
        });
        // Let first grab the permit before racing.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = tokio::spawn(async move {
            b2.call_tool(params("slow", "work"), &CallContext::test(), CancellationToken::new())
                .await
        });

        let (r1, r2) = tokio::join!(first, second);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        // Exactly one Ok and one ConcurrencyCap, regardless of spawn ordering.
        let (ok, err) = match (r1, r2) {
            (Ok(v), Err(e)) => (v, e),
            (Err(e), Ok(v)) => (v, e),
            other => panic!("expected one Ok and one Err, got {other:?}"),
        };
        assert!(!ok.is_error);
        assert!(
            matches!(
                err,
                McpError::Policy(PolicyError::ConcurrencyCap { max: 1, .. })
            ),
            "expected Policy(ConcurrencyCap{{max:1}}), got {err:?}"
        );
    }

    #[tokio::test]
    async fn policy_timeout_fires() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("napper")
                .with_tool("sleep")
                .on_call(|_p| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(KernelToolResult::text("done"))
                }),
        );
        broker
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_millis(50),
                    max_result_bytes: 1024,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();

        let err = broker
            .call_tool(
                params("napper", "sleep"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                McpError::Policy(PolicyError::Timeout { timeout_ms: 50, .. })
            ),
            "expected Policy(Timeout{{timeout_ms:50}}), got {err:?}"
        );
    }

    #[tokio::test]
    async fn policy_result_too_large_fires() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("chatty")
                .with_tool("say")
                .on_call(|_p| async { Ok(KernelToolResult::text("x".repeat(64))) }),
        );
        broker
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_secs(5),
                    max_result_bytes: 32,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();

        let err = broker
            .call_tool(
                params("chatty", "say"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                McpError::Policy(PolicyError::ResultTooLarge {
                    size: 64,
                    max: 32,
                    ..
                })
            ),
            "expected Policy(ResultTooLarge{{size:64,max:32}}), got {err:?}"
        );
    }

    #[tokio::test]
    async fn tool_not_found_propagates_verbatim() {
        // Broker is a passthrough for server-layer errors — it must not
        // remap or swallow ToolNotFound coming out of call_tool.
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("picky").with_tool("real").on_call(|p| async move {
                Err(McpError::ToolNotFound {
                    instance: InstanceId::new("picky"),
                    tool: p.tool,
                })
            }),
        );
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let err = broker
            .call_tool(
                params("picky", "missing"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        match err {
            McpError::ToolNotFound { instance, tool } => {
                assert_eq!(instance.as_str(), "picky");
                assert_eq!(tool, "missing");
            }
            other => panic!("expected ToolNotFound, got {other:?}"),
        }

        // Sanity: a successful call still passes through the same pipeline.
        let ok_server = Arc::new(
            MockServer::new("ok").with_tool("greet").on_call(|_p| async {
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Text("hi".into())],
                    structured: None,
                })
            }),
        );
        broker
            .register(ok_server, InstancePolicy::default())
            .await
            .unwrap();
        let ok = broker
            .call_tool(
                params("ok", "greet"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!ok.is_error);
        assert!(matches!(ok.content.first(), Some(ToolContent::Text(s)) if s == "hi"));
    }

    // ═════════════════════════════════════════════════════════════════════
    // Phase 2 (M3) — notification emission
    // ═════════════════════════════════════════════════════════════════════

    use crate::block_store::{SharedBlockStore, shared_block_store};
    use crate::mcp::binding::ContextToolBinding;
    use crate::mcp::coalescer::{CoalescePolicy, NotificationCoalescer};
    use crate::DocumentKind;
    use kaijutsu_types::{BlockFilter, ContextId, PrincipalId};
    use tokio::time::sleep;

    /// Build a broker wired to a fresh in-memory block store + one bound
    /// context. Returns `(broker, store, context_id)`.
    async fn wired_broker() -> (Arc<Broker>, SharedBlockStore, ContextId) {
        let broker = Arc::new(Broker::new());
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        broker.set_documents(store.clone()).await;
        (broker, store, ctx)
    }

    async fn bind(broker: &Arc<Broker>, ctx: ContextId, instance: &str) {
        let binding = ContextToolBinding::with_instances(vec![InstanceId::new(instance)]);
        broker.set_binding(ctx, binding).await;
    }

    fn notifications_in(store: &SharedBlockStore, ctx: ContextId) -> Vec<NotificationPayload> {
        let filter = BlockFilter {
            kinds: vec![kaijutsu_types::BlockKind::Notification],
            ..Default::default()
        };
        store
            .query_blocks(ctx, &filter)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|b| b.notification.clone())
            .collect()
    }

    /// Exit criterion #1: register → ToolAdded block in bound context.
    #[tokio::test]
    async fn register_emits_tool_added_into_bound_context() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("ping"));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let notifs = notifications_in(&store, ctx);
        assert_eq!(notifs.len(), 1, "expected exactly one notification block");
        assert_eq!(
            notifs[0].kind,
            kaijutsu_types::NotificationKind::ToolAdded
        );
        assert_eq!(notifs[0].tool.as_deref(), Some("ping"));
        assert_eq!(notifs[0].instance, "svc");
    }

    /// D-38: register_silently suppresses synthetic ToolAdded.
    #[tokio::test]
    async fn register_silently_suppresses_synthetic_tool_added() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("ping"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        assert!(
            notifications_in(&store, ctx).is_empty(),
            "silent register should emit zero notification blocks"
        );
    }

    /// Exit criterion #3: unregister emits ToolRemoved + future calls error.
    #[tokio::test]
    async fn unregister_emits_tool_removed_and_future_call_errors() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "ephemeral").await;
        let server = Arc::new(MockServer::new("ephemeral").with_tool("ping"));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        // Confirm the register emitted a ToolAdded.
        assert_eq!(notifications_in(&store, ctx).len(), 1);

        // First call succeeds.
        broker
            .call_tool(
                params("ephemeral", "ping"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("first call should succeed");

        broker
            .unregister(&InstanceId::new("ephemeral"))
            .await
            .unwrap();

        let notifs = notifications_in(&store, ctx);
        assert!(
            notifs.iter().any(|n| n.kind
                == kaijutsu_types::NotificationKind::ToolRemoved
                && n.tool.as_deref() == Some("ping")),
            "expected a ToolRemoved notification for 'ping', got {:?}",
            notifs
        );

        // Subsequent call errors.
        let err = broker
            .call_tool(
                params("ephemeral", "ping"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::InstanceNotFound(_)),
            "expected InstanceNotFound after unregister, got {err:?}"
        );
    }

    /// Exit criterion #4: Log burst produces one Coalesced summary block.
    #[tokio::test]
    async fn log_burst_coalesces_to_one_summary_block() {
        // Custom coalescer with a tight window and small cap so 25 Logs
        // become 20 passthrough + 1 coalesced summary.
        let broker = Arc::new({
            let b = Broker::new();
            // Replace the coalescer with a test-tuned one via a private
            // mut-swap. The public API is `new(CoalescePolicy::default())`;
            // we build the broker, then mutate.
            b
        });
        // Overwrite coalescer with tight policy via Arc-swap-like trick: new
        // broker with desired policy and move documents/bindings onto it.
        // Easier: construct a fresh broker with overridden coalescer.
        // Since Broker::new() doesn't take a policy, add the policy via the
        // coalescer's public `policy()` — no mutation. Instead, drop this
        // broker and build one with a hand-built coalescer.
        drop(broker);

        let broker = Arc::new({
            let (notif_tx, _) = broadcast::channel(NOTIF_CAPACITY);
            Broker {
                instances: RwLock::new(HashMap::new()),
                bindings: RwLock::new(HashMap::new()),
                policies: RwLock::new(HashMap::new()),
                semaphores: RwLock::new(HashMap::new()),
                hooks: RwLock::new(HookTables::default()),
                coalescer: Arc::new(NotificationCoalescer::new(CoalescePolicy {
                    window: Duration::from_millis(100),
                    max_in_window: 20,
                    hard_drop_after: None,
                    per_kind_override: HashMap::new(),
                })),
                notif_tx,
                documents: RwLock::new(None),
                pump_handles: Mutex::new(HashMap::new()),
                flush_timers: Mutex::new(HashMap::new()),
                tool_snapshots: Mutex::new(HashMap::new()),
                subscriptions: Mutex::new(HashMap::new()),
                resource_parents: Mutex::new(HashMap::new()),
            }
        });
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        broker.set_documents(store.clone()).await;

        bind(&broker, ctx, "chatty").await;
        let server = Arc::new(MockServer::new("chatty").with_tool("talk"));
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        // Push 25 Log events; first 20 pass through, 5 get coalesced.
        for i in 0..25 {
            let _ = tx.send(ServerNotification::Log {
                level: LogLevel::Info,
                message: format!("event {i}"),
                tool: None,
            });
        }

        // Wait for pump + flush timer.
        sleep(Duration::from_millis(250)).await;

        let notifs = notifications_in(&store, ctx);
        let logs = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::Log)
            .count();
        let coalesced: Vec<&NotificationPayload> = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::Coalesced)
            .collect();
        assert!(
            logs >= 20,
            "expected at least 20 pass-through Log blocks, got {logs}"
        );
        assert_eq!(
            coalesced.len(),
            1,
            "expected exactly one Coalesced summary, got {}: {:?}",
            coalesced.len(),
            coalesced
        );
        assert_eq!(
            coalesced[0].count,
            Some(5),
            "Coalesced summary should collapse 5 events"
        );
    }

    /// D-35: ToolsChanged is diffed per-tool (ToolAdded for new, ToolRemoved
    /// for gone).
    #[tokio::test]
    async fn tools_changed_emits_per_tool_diff() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(
            MockServer::new("svc")
                .with_tool("a")
                .with_tool("b"),
        );
        let tx = server.sender();
        // register_silently to keep the fixture clean: ignore the bootstrap
        // ToolAdded blocks for 'a' and 'b'.
        broker
            .register_silently(server.clone(), InstancePolicy::default())
            .await
            .unwrap();
        assert!(notifications_in(&store, ctx).is_empty());

        // Swap advertised tools to ["a", "c"].
        server.set_tools(&["a", "c"]);
        let _ = tx.send(ServerNotification::ToolsChanged);

        // Wait for the pump to process.
        for _ in 0..10 {
            sleep(Duration::from_millis(20)).await;
            let n = notifications_in(&store, ctx);
            if n.len() >= 2 {
                break;
            }
        }
        let notifs = notifications_in(&store, ctx);
        assert!(
            notifs.iter().any(|n| n.kind
                == kaijutsu_types::NotificationKind::ToolAdded
                && n.tool.as_deref() == Some("c")),
            "expected ToolAdded for 'c', got {notifs:?}"
        );
        assert!(
            notifs.iter().any(|n| n.kind
                == kaijutsu_types::NotificationKind::ToolRemoved
                && n.tool.as_deref() == Some("b")),
            "expected ToolRemoved for 'b', got {notifs:?}"
        );
        // 'a' should not show up in either direction (unchanged).
        assert!(
            !notifs.iter().any(|n| n.tool.as_deref() == Some("a")),
            "'a' was unchanged; no diff block expected, got {notifs:?}"
        );
    }

    #[tokio::test]
    /// Phase 3 (D-44): ResourceUpdated without a live subscription emits
    /// nothing. The flush body walks `subscriptions`, finds no matching
    /// `(ctx, instance, uri)` entry, and returns silently. Locks the
    /// "subscription is the trigger, not the notification" contract.
    async fn resource_updated_without_subscription_emits_nothing() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        for _ in 0..3 {
            let _ = tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///whatever".into(),
            });
        }
        sleep(Duration::from_millis(700)).await;
        assert!(
            notifications_in(&store, ctx).is_empty(),
            "no Notification blocks — no subscription exists",
        );
        let filter = BlockFilter {
            kinds: vec![kaijutsu_types::BlockKind::Resource],
            ..Default::default()
        };
        assert!(
            store.query_blocks(ctx, &filter).unwrap().is_empty(),
            "no Resource blocks either — no subscription exists",
        );
    }

    /// D-39 / R2: the flush timer is aborted on unregister so no orphan
    /// Coalesced block fires after teardown.
    #[tokio::test]
    async fn flush_timer_aborts_on_unregister() {
        let broker = Arc::new({
            let (notif_tx, _) = broadcast::channel(NOTIF_CAPACITY);
            Broker {
                instances: RwLock::new(HashMap::new()),
                bindings: RwLock::new(HashMap::new()),
                policies: RwLock::new(HashMap::new()),
                semaphores: RwLock::new(HashMap::new()),
                hooks: RwLock::new(HookTables::default()),
                coalescer: Arc::new(NotificationCoalescer::new(CoalescePolicy {
                    window: Duration::from_millis(500),
                    max_in_window: 2,
                    hard_drop_after: None,
                    per_kind_override: HashMap::new(),
                })),
                notif_tx,
                documents: RwLock::new(None),
                pump_handles: Mutex::new(HashMap::new()),
                flush_timers: Mutex::new(HashMap::new()),
                tool_snapshots: Mutex::new(HashMap::new()),
                subscriptions: Mutex::new(HashMap::new()),
                resource_parents: Mutex::new(HashMap::new()),
            }
        });
        let store = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        broker.set_documents(store.clone()).await;
        bind(&broker, ctx, "chatty").await;
        let server = Arc::new(MockServer::new("chatty").with_tool("t"));
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        // Trip the coalescer window: 2 passthrough + 2 coalesced.
        for i in 0..4 {
            let _ = tx.send(ServerNotification::Log {
                level: LogLevel::Info,
                message: format!("m{i}"),
                tool: None,
            });
        }
        // Give the pump time to observe and schedule the flush timer,
        // but less than the 500ms window.
        sleep(Duration::from_millis(50)).await;
        // Unregister immediately — the pending flush timer should be
        // aborted, so no Coalesced block should ever appear.
        broker
            .unregister(&InstanceId::new("chatty"))
            .await
            .unwrap();

        // Wait past the original window.
        sleep(Duration::from_millis(700)).await;
        let notifs = notifications_in(&store, ctx);
        let coalesced = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::Coalesced)
            .count();
        assert_eq!(
            coalesced, 0,
            "unregister should have aborted the flush timer; got {coalesced} orphan Coalesced blocks in {notifs:?}"
        );
    }

    // ── Phase 3 M3: resource dispatch + subscription lifecycle ───────

    use crate::mcp::KernelResource;
    use crate::mcp::KernelResourceContents;
    use crate::mcp::KernelResourceList;
    use crate::mcp::KernelReadResource;
    use std::collections::HashSet as StdHashSet;

    /// Test fake that implements the resource surface on top of `MockServer`.
    /// Tests push resources/contents into interior-mutable fields; the server
    /// returns them on read. `read_fails` makes `read_resource` return an
    /// error to exercise the D-43 failure path.
    struct ResourceMock {
        id: InstanceId,
        resources: std::sync::Mutex<Vec<KernelResource>>,
        contents: std::sync::Mutex<
            std::collections::HashMap<String, KernelResourceContents>,
        >,
        subscribed: std::sync::Mutex<StdHashSet<String>>,
        unsubscribed: std::sync::Mutex<Vec<String>>,
        read_fails: std::sync::Mutex<bool>,
        /// Incremented every time `read_resource` is called. Used by the
        /// N+1-reads regression test to assert flush-side fan-out.
        read_count: std::sync::atomic::AtomicUsize,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl ResourceMock {
        fn new(id: &str) -> Self {
            let (notif_tx, _) = broadcast::channel(64);
            Self {
                id: InstanceId::new(id),
                resources: std::sync::Mutex::new(Vec::new()),
                contents: std::sync::Mutex::new(std::collections::HashMap::new()),
                subscribed: std::sync::Mutex::new(StdHashSet::new()),
                unsubscribed: std::sync::Mutex::new(Vec::new()),
                read_fails: std::sync::Mutex::new(false),
                read_count: std::sync::atomic::AtomicUsize::new(0),
                notif_tx,
            }
        }

        fn read_count(&self) -> usize {
            self.read_count.load(std::sync::atomic::Ordering::SeqCst)
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
            self.contents.lock().unwrap().insert(
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

        fn make_read_fail(&self, v: bool) {
            *self.read_fails.lock().unwrap() = v;
        }

        fn was_subscribed(&self, uri: &str) -> bool {
            self.subscribed.lock().unwrap().contains(uri)
        }

        fn unsubscribed_uris(&self) -> Vec<String> {
            self.unsubscribed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl McpServerLike for ResourceMock {
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
                resources: self.resources.lock().unwrap().clone(),
            })
        }
        async fn read_resource(
            &self,
            uri: &str,
            _ctx: &CallContext,
        ) -> McpResult<KernelReadResource> {
            self.read_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if *self.read_fails.lock().unwrap() {
                return Err(McpError::Protocol("simulated re-read failure".to_string()));
            }
            match self.contents.lock().unwrap().get(uri).cloned() {
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
            self.unsubscribed.lock().unwrap().push(uri.to_string());
            Ok(())
        }
    }

    fn resource_blocks_in(
        store: &SharedBlockStore,
        ctx: ContextId,
    ) -> Vec<kaijutsu_types::BlockSnapshot> {
        let filter = BlockFilter {
            kinds: vec![kaijutsu_types::BlockKind::Resource],
            ..Default::default()
        };
        store.query_blocks(ctx, &filter).unwrap_or_default()
    }

    /// Exit criterion #1: read_resource emits a root BlockKind::Resource block.
    #[tokio::test]
    async fn read_resource_emits_block_in_context() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(ResourceMock::new("res").with_text_resource("file:///a", "hi"));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        let result = broker
            .read_resource(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        assert_eq!(result.contents.len(), 1);

        let blocks = resource_blocks_in(&store, ctx);
        assert_eq!(blocks.len(), 1, "expected exactly one Resource block");
        let payload = blocks[0].resource.as_ref().unwrap();
        assert_eq!(payload.uri, "file:///a");
        assert_eq!(payload.instance, "res");
        assert_eq!(payload.text.as_deref(), Some("hi"));
        assert!(blocks[0].parent_id.is_none(), "root read has no parent");
    }

    /// Exit criterion #2: ResourceUpdated produces a child block under the root.
    #[tokio::test]
    async fn resource_updated_emits_child_block_under_parent() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "initial"),
        );
        let tx = server.sender();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        broker
            .read_resource(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();

        // Fire a single ResourceUpdated; default policy (D-45) opens a window
        // immediately. Wait past the window for the flush timer + re-read.
        tx.send(ServerNotification::ResourceUpdated {
            uri: "file:///a".to_string(),
        })
        .unwrap();
        sleep(Duration::from_millis(700)).await;

        let blocks = resource_blocks_in(&store, ctx);
        assert_eq!(
            blocks.len(),
            2,
            "expected root + one child Resource block; got {blocks:?}"
        );
        let root = blocks.iter().find(|b| b.parent_id.is_none()).unwrap();
        let child = blocks.iter().find(|b| b.parent_id.is_some()).unwrap();
        assert_eq!(child.parent_id, Some(root.id));
        assert_eq!(
            child.resource.as_ref().unwrap().parent_resource_block_id,
            Some(root.id),
        );
    }

    /// Exit criterion #3: a burst collapses to exactly one child block per window (D-45).
    #[tokio::test]
    async fn resource_updated_burst_coalesces_to_one_child() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "initial"),
        );
        let tx = server.sender();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        broker
            .read_resource(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();

        // Fire 25 updates within ~50ms (well under the 500ms default window).
        for _ in 0..25 {
            tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///a".to_string(),
            })
            .unwrap();
        }
        sleep(Duration::from_millis(700)).await;

        let blocks = resource_blocks_in(&store, ctx);
        // 1 root + 1 flush-emitted child = 2 total.
        assert_eq!(
            blocks.len(),
            2,
            "expected root + exactly one child from burst; got {blocks:?}"
        );
    }

    /// D-40: per-URI windows track independently — two URIs produce two children.
    #[tokio::test]
    async fn two_uris_coalesce_independently() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res")
                .with_text_resource("file:///a", "A")
                .with_text_resource("file:///b", "B"),
        );
        let tx = server.sender();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        broker
            .read_resource(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        broker
            .read_resource(&InstanceId::new("res"), "file:///b", &call_ctx)
            .await
            .unwrap();
        broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        broker
            .subscribe(&InstanceId::new("res"), "file:///b", &call_ctx)
            .await
            .unwrap();

        for _ in 0..5 {
            tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///a".to_string(),
            })
            .unwrap();
            tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///b".to_string(),
            })
            .unwrap();
        }
        sleep(Duration::from_millis(700)).await;

        let blocks = resource_blocks_in(&store, ctx);
        // 2 roots + 2 flush-emitted children (one per URI) = 4 total.
        assert_eq!(blocks.len(), 4, "expected 2 roots + 2 children");
        let children: Vec<_> = blocks.iter().filter(|b| b.parent_id.is_some()).collect();
        assert_eq!(children.len(), 2);
        let child_uris: StdHashSet<String> = children
            .iter()
            .map(|b| b.resource.as_ref().unwrap().uri.clone())
            .collect();
        assert!(child_uris.contains("file:///a"));
        assert!(child_uris.contains("file:///b"));
    }

    /// Exit criterion #4: clear_binding unsubscribes every live URI cleanly.
    #[tokio::test]
    async fn clear_binding_unsubscribes_all_uris() {
        let (broker, _store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res")
                .with_text_resource("file:///a", "A")
                .with_text_resource("file:///b", "B")
                .with_text_resource("file:///c", "C"),
        );
        let server_handle = server.clone();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        for uri in ["file:///a", "file:///b", "file:///c"] {
            broker
                .subscribe(&InstanceId::new("res"), uri, &call_ctx)
                .await
                .unwrap();
        }
        assert!(server_handle.was_subscribed("file:///a"));
        assert!(server_handle.was_subscribed("file:///b"));
        assert!(server_handle.was_subscribed("file:///c"));

        broker.clear_binding(&ctx).await;

        assert!(!server_handle.was_subscribed("file:///a"));
        assert!(!server_handle.was_subscribed("file:///b"));
        assert!(!server_handle.was_subscribed("file:///c"));
        let unsubs = server_handle.unsubscribed_uris();
        assert_eq!(unsubs.len(), 3, "each uri unsubscribed once");
    }

    /// R2 / D-44: unregister tears down subscriptions even if clear_binding
    /// was never called.
    #[tokio::test]
    async fn unregister_unsubscribes_bound_contexts() {
        let (broker, _store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///x", "X"),
        );
        let server_handle = server.clone();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        broker
            .subscribe(&InstanceId::new("res"), "file:///x", &call_ctx)
            .await
            .unwrap();
        assert!(server_handle.was_subscribed("file:///x"));

        broker.unregister(&InstanceId::new("res")).await.unwrap();

        assert!(!server_handle.was_subscribed("file:///x"));
        // The subscription table must no longer carry this entry.
        let subs = broker.subscriptions.lock().await;
        assert!(
            subs.get(&ctx).map(|s| s.is_empty()).unwrap_or(true),
            "subscriptions should be empty for ctx after unregister"
        );
    }

    /// D-43 failure path: a failing re-read emits a Log notification under
    /// the parent Resource block, not a fake Resource block.
    #[tokio::test]
    async fn failed_reread_emits_log_not_resource() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "initial"),
        );
        let server_handle = server.clone();
        let tx = server.sender();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        broker
            .read_resource(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();

        // Flip the mock to fail re-reads, then trigger an update.
        server_handle.make_read_fail(true);
        tx.send(ServerNotification::ResourceUpdated {
            uri: "file:///a".to_string(),
        })
        .unwrap();
        sleep(Duration::from_millis(700)).await;

        let resource_blocks = resource_blocks_in(&store, ctx);
        assert_eq!(
            resource_blocks.len(),
            1,
            "only the initial read; no fake Resource child on failure",
        );
        let root = resource_blocks[0].id;

        // The failure must surface as a Log notification under the parent.
        let notifs = notifications_in(&store, ctx);
        let log_children: Vec<_> = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::Log)
            .collect();
        assert_eq!(
            log_children.len(),
            1,
            "expected one Warn Log notification; got {notifs:?}",
        );
        assert_eq!(log_children[0].level, Some(kaijutsu_types::LogLevel::Warn));
        // Confirm the Log block's parent_id is the original Resource root.
        let filter = BlockFilter {
            kinds: vec![kaijutsu_types::BlockKind::Notification],
            ..Default::default()
        };
        let blocks = store.query_blocks(ctx, &filter).unwrap();
        assert!(
            blocks
                .iter()
                .any(|b| b.parent_id == Some(root)
                    && b.notification
                        .as_ref()
                        .map(|p| p.kind == kaijutsu_types::NotificationKind::Log)
                        .unwrap_or(false)),
            "Log notification must be threaded under the Resource root",
        );
    }

    // ── Phase 3 post-review bugfixes ────────────────────────────────────

    /// Bug #1 (unregister subscription-leak race): after `unregister`, the
    /// broker's view of `instances` must not carry the removed id, and a
    /// subsequent `subscribe` for that id must error with `InstanceNotFound`
    /// — not silently record a row pointing at a vanished server. Locks the
    /// invariant that `unregister` removes from `self.instances` BEFORE the
    /// subscription teardown sweep so new subscribe calls fail fast.
    #[tokio::test]
    async fn subscribe_after_unregister_errors_and_leaves_no_row() {
        let (broker, _store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "A"),
        );
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        broker.unregister(&InstanceId::new("res")).await.unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        let err = broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::InstanceNotFound(_)),
            "subscribe after unregister must fail fast, got {err:?}"
        );

        let subs = broker.subscriptions.lock().await;
        let stale: Vec<_> = subs
            .values()
            .flat_map(|set| set.iter().filter(|(i, _)| i.as_str() == "res"))
            .collect();
        assert!(
            stale.is_empty(),
            "failed subscribe must not record a row, found {stale:?}",
        );
    }

    /// Bug #2 (silent-subscribe trap): calling `subscribe` without first
    /// calling `read_resource` used to record the subscription but leave
    /// `resource_parents` empty, so every `ResourceUpdated` was filtered out
    /// in `handle_resource_flush` — the LLM saw "subscribed" success and
    /// zero updates. The fix auto-reads inside `subscribe` to establish a
    /// root parent. This test locks both halves of the fix: the root block
    /// appears on subscribe, and subsequent updates thread under it.
    #[tokio::test]
    async fn subscribe_without_prior_read_delivers_updates() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "res").await;
        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "initial"),
        );
        let tx = server.sender();
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let mut call_ctx = CallContext::test();
        call_ctx.context_id = ctx;
        // Skip read_resource — call subscribe directly.
        broker
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();

        // The auto-read must have emitted a root Resource block.
        let blocks = resource_blocks_in(&store, ctx);
        assert_eq!(
            blocks.len(),
            1,
            "subscribe must auto-emit a root block when no prior read existed",
        );
        assert!(blocks[0].parent_id.is_none());

        // Push an update. Pre-fix this was silently filtered out.
        tx.send(ServerNotification::ResourceUpdated {
            uri: "file:///a".to_string(),
        })
        .unwrap();
        sleep(Duration::from_millis(700)).await;

        let blocks = resource_blocks_in(&store, ctx);
        assert_eq!(
            blocks.len(),
            2,
            "expected root + one child after the update; pre-fix this was 1 (silent drop)",
        );
        let child = blocks
            .iter()
            .find(|b| b.parent_id.is_some())
            .expect("child block must exist under root");
        assert!(child.resource.as_ref().unwrap().text.is_some());
    }

    /// Bug #3 (N+1 reads on flush): multiple contexts subscribed to the
    /// same `(instance, uri)` must share a single re-read on window flush —
    /// not one read per subscriber. The old code looped over targets and
    /// called `server.read_resource` inside the loop. This test registers
    /// two contexts, each subscribed (one each → 2 initial auto-reads),
    /// then pushes one update burst and asserts the post-flush read
    /// counter went up by exactly one (fan-out), not two.
    #[tokio::test]
    async fn flush_reads_once_for_all_subscribers() {
        let broker = Arc::new(Broker::new());
        let store = shared_block_store(PrincipalId::system());
        let ctx_a = ContextId::new();
        let ctx_b = ContextId::new();
        store
            .create_document(ctx_a, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        store
            .create_document(ctx_b, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        broker.set_documents(store.clone()).await;
        for ctx in [ctx_a, ctx_b] {
            broker
                .set_binding(
                    ctx,
                    ContextToolBinding::with_instances(vec![InstanceId::new("res")]),
                )
                .await;
        }

        let server = Arc::new(
            ResourceMock::new("res").with_text_resource("file:///a", "initial"),
        );
        let server_handle = server.clone();
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        for ctx in [ctx_a, ctx_b] {
            let mut call_ctx = CallContext::test();
            call_ctx.context_id = ctx;
            broker
                .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
                .await
                .unwrap();
        }
        let reads_before_flush = server_handle.read_count();
        assert_eq!(
            reads_before_flush, 2,
            "each context's subscribe auto-reads once, expected 2",
        );

        // One burst; the flush path must read the URI once for both
        // subscribers, not once per subscriber.
        for _ in 0..5 {
            tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///a".to_string(),
            })
            .unwrap();
        }
        sleep(Duration::from_millis(700)).await;

        let flushed_reads = server_handle.read_count() - reads_before_flush;
        assert_eq!(
            flushed_reads, 1,
            "flush must do one read for N subscribers; got {flushed_reads}",
        );

        // Both contexts must have received a child block under their own
        // root, from the single read.
        for ctx in [ctx_a, ctx_b] {
            let blocks = resource_blocks_in(&store, ctx);
            assert_eq!(
                blocks.len(),
                2,
                "each subscriber gets root + one child from the fan-out; ctx={ctx:?}",
            );
        }
    }
}
