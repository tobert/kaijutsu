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

use kaijutsu_types::{ContextId, NotificationPayload};
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
    InstanceId, KernelCallParams, KernelNotification, KernelTool, KernelToolResult, LogLevel,
    NotifKind,
};
use crate::block_store::SharedBlockStore;

/// Default notification channel capacity.
const NOTIF_CAPACITY: usize = 256;

type FlushKey = (InstanceId, NotifKind);

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
            .filter(|(i, _)| i == id)
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

        self.instances.write().await.remove(id);
        self.policies.write().await.remove(id);
        self.semaphores.write().await.remove(id);

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

    pub async fn clear_binding(&self, context_id: &ContextId) {
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

    /// Schedule a `flush` timer for `(instance, kind)` if none is pending.
    /// When the timer elapses, it calls `coalescer.flush()` and emits a
    /// single Coalesced summary notification block per bound context.
    async fn schedule_flush(
        self: &Arc<Self>,
        instance: InstanceId,
        kind: NotifKind,
    ) {
        let key = (instance.clone(), kind);
        let mut timers = self.flush_timers.lock().await;
        if timers.contains_key(&key) {
            return;
        }
        let window = self.coalescer.policy().window;
        let broker = Arc::clone(self);
        let instance_for_task = instance.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(window).await;
            if let Some(count) = broker.coalescer.flush(&instance_for_task, kind) {
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
            broker.flush_timers.lock().await.remove(&(instance_for_task, kind));
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
                match broker.coalescer.observe(&id, NotifKind::Log) {
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
                        broker.schedule_flush(id.clone(), NotifKind::Log).await;
                    }
                    ObserveOutcome::Coalesced { .. } => {
                        // Already counted inside the coalescer; timer pending.
                    }
                }
            }
            Ok(ServerNotification::PromptsChanged) => {
                match broker.coalescer.observe(&id, NotifKind::PromptsChanged) {
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
                            .schedule_flush(id.clone(), NotifKind::PromptsChanged)
                            .await;
                    }
                    ObserveOutcome::Coalesced { .. } => {}
                }
            }
            Ok(ServerNotification::ResourceUpdated { .. }) => {
                // Phase 3: routed into BlockKind::Resource; dropped in Phase 2.
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
                })),
                notif_tx,
                documents: RwLock::new(None),
                pump_handles: Mutex::new(HashMap::new()),
                flush_timers: Mutex::new(HashMap::new()),
                tool_snapshots: Mutex::new(HashMap::new()),
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

    /// ResourceUpdated is dropped in Phase 2 (Phase 3 routes it to
    /// `BlockKind::Resource`).
    #[tokio::test]
    async fn resource_updated_is_dropped_in_phase_2() {
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
        sleep(Duration::from_millis(50)).await;
        assert!(
            notifications_in(&store, ctx).is_empty(),
            "ResourceUpdated should be silently dropped in Phase 2"
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
                })),
                notif_tx,
                documents: RwLock::new(None),
                pump_handles: Mutex::new(HashMap::new()),
                flush_timers: Mutex::new(HashMap::new()),
                tool_snapshots: Mutex::new(HashMap::new()),
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
}
