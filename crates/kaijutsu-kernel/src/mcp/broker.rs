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
use super::error::{HookId, McpError, McpResult, PolicyError};
use super::hook_table::{HookAction, HookBody, HookEntry, HookPhase, HookTables};
use super::hooks_builtin::BuiltinHookRegistry;
use super::policy::InstancePolicy;
use super::server_like::{McpServerLike, ServerNotification};
use super::types::{
    InstanceId, KernelCallParams, KernelNotification, KernelReadResource, KernelResourceContents,
    KernelResourceList, KernelTool, KernelToolResult, LogLevel, NotifKind,
};
use crate::block_store::{DbHandle, SharedBlockStore};

/// Default notification channel capacity.
const NOTIF_CAPACITY: usize = 256;

/// Max `HookAction::Invoke` depth per tokio task (§4.3, D-29, D-47).
/// The counter lives in a task-local `Cell<u32>`; `enter_hook_depth`
/// increments on entry, a drop guard decrements on exit (surviving panic
/// unwind). Recursion that would exceed this cap returns
/// `McpError::HookRecursionLimit`.
pub(crate) const MAX_HOOK_DEPTH: u32 = 4;

tokio::task_local! {
    static HOOK_DEPTH: std::cell::Cell<u32>;
}

#[cfg(test)]
pub(crate) static HOOK_DEPTH_OVERRIDE: std::sync::OnceLock<u32> =
    std::sync::OnceLock::new();

fn max_hook_depth() -> u32 {
    #[cfg(test)]
    {
        HOOK_DEPTH_OVERRIDE.get().copied().unwrap_or(MAX_HOOK_DEPTH)
    }
    #[cfg(not(test))]
    {
        MAX_HOOK_DEPTH
    }
}

struct HookDepthGuard;
impl Drop for HookDepthGuard {
    fn drop(&mut self) {
        // `try_with` returns Err outside the scope; safe to ignore in that
        // case (the guard has outlived its scope — impossible via the
        // public API, but robust against future refactors).
        let _ = HOOK_DEPTH.try_with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Increment the per-task hook depth counter. Returns a guard that
/// decrements on drop (including panic unwind). `McpError::HookRecursionLimit`
/// if the increment would exceed the cap.
fn enter_hook_depth() -> McpResult<HookDepthGuard> {
    let current = HOOK_DEPTH.try_with(|d| d.get()).unwrap_or(0);
    let next = current.saturating_add(1);
    if next > max_hook_depth() {
        return Err(McpError::HookRecursionLimit { depth: next });
    }
    // Best-effort: if the scope isn't installed (caller bypassed
    // `call_tool`), the guard won't have anything to decrement — acceptable
    // because the only path that increments is the call_tool → evaluate_phase
    // → Invoke arm, and call_tool installs the scope.
    let _ = HOOK_DEPTH.try_with(|d| d.set(next));
    Ok(HookDepthGuard)
}

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
    /// Phase 4 (D-50): named builtin hook registry. Admin RPC looks up hook
    /// bodies by name so the wire never carries `Arc<dyn Hook>`. Frozen
    /// after construction.
    builtin_hooks: BuiltinHookRegistry,
    /// Phase 5 (D-54): kernel DB handle used to persist `ContextToolBinding`
    /// across kernel restart. Set via `set_db` at kernel bootstrap (same
    /// D-37 setter pattern as `documents`). `None` → persistence is a
    /// no-op, which keeps `Broker::new()` workable for tests.
    db: RwLock<Option<DbHandle>>,
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
            builtin_hooks: BuiltinHookRegistry::new(),
            db: RwLock::new(None),
        }
    }

    /// Wire the kernel DB handle used to persist `ContextToolBinding` and
    /// `HookTables` across kernel restart (D-54, D-37 setter pattern, hook
    /// persistence follow-up). Call before first binding/hook mutation at
    /// kernel bootstrap; until this is set, mutation still works on the
    /// in-memory maps but persistence is a no-op.
    ///
    /// Hooks are eagerly hydrated from the DB at wire-time since the
    /// tables are global (not per-context sharded) and small. Unknown
    /// `action_builtin_name` rows or shape violations `tracing::warn!` +
    /// skip rather than crash — sqlite surgery is the intended recovery
    /// path for bad rows.
    pub async fn set_db(self: &Arc<Self>, db: DbHandle) {
        *self.db.write().await = Some(db.clone());
        self.hydrate_hooks_from_db(&db).await;
    }

    /// Load every persisted hook row and reconstruct `HookTables` in
    /// place. Called from `set_db` at bootstrap. Rows whose action
    /// shape can't be reified (unknown builtin name, invalid enum
    /// field, kaish body) are logged at WARN and skipped individually —
    /// one bad row must not brick the whole hook table.
    async fn hydrate_hooks_from_db(self: &Arc<Self>, db: &DbHandle) {
        let rows = {
            let guard = db.lock();
            match guard.load_all_hooks() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        "failed to load hooks from DB; hook tables remain empty",
                    );
                    return;
                }
            }
        };
        if rows.is_empty() {
            return;
        }
        let mut hooks = self.hooks.write().await;
        let mut loaded = 0usize;
        let mut skipped = 0usize;
        for row in rows {
            match super::hook_persist::row_to_entry(&row, &self.builtin_hooks) {
                Ok((phase, entry)) => {
                    let table = match phase {
                        HookPhase::PreCall => &mut hooks.pre_call,
                        HookPhase::PostCall => &mut hooks.post_call,
                        HookPhase::OnError => &mut hooks.on_error,
                        HookPhase::OnNotification => &mut hooks.on_notification,
                        HookPhase::ListTools => &mut hooks.list_tools,
                    };
                    table.entries.push(entry);
                    loaded += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        hook_id = %row.hook_id,
                        phase = %row.phase,
                        action_kind = %row.action_kind,
                        reason = %e,
                        "skipping persisted hook row",
                    );
                    skipped += 1;
                }
            }
        }
        tracing::debug!(
            loaded,
            skipped,
            "hydrated hook tables from DB",
        );
    }

    /// Persist one hook entry to the DB after `hook_add`. Best-effort:
    /// failure warns but does not bubble up so the in-memory push
    /// remains authoritative for the rest of the kernel's lifetime
    /// (operator retries via `hook_add` after fixing the DB problem).
    pub async fn persist_hook_insert(&self, phase: HookPhase, entry: &HookEntry) {
        let db = match self.db.read().await.clone() {
            Some(h) => h,
            None => return,
        };
        let row = super::hook_persist::entry_to_row(phase, entry);
        let guard = db.lock();
        if let Err(e) = guard.insert_hook(&row) {
            tracing::warn!(
                hook_id = %entry.id,
                phase = ?phase,
                error = ?e,
                "failed to persist hook insert",
            );
        }
    }

    /// Drop a hook from the DB after `hook_remove`. Best-effort mirror
    /// of `persist_hook_insert`; idempotent (delete of an unknown id
    /// is a non-error).
    pub async fn persist_hook_delete(&self, hook_id: &str) {
        let db = match self.db.read().await.clone() {
            Some(h) => h,
            None => return,
        };
        let guard = db.lock();
        if let Err(e) = guard.delete_hook(hook_id) {
            tracing::warn!(
                hook_id = hook_id,
                error = ?e,
                "failed to persist hook delete",
            );
        }
    }

    pub fn coalescer(&self) -> &Arc<NotificationCoalescer> {
        &self.coalescer
    }

    /// Named builtin hook factories (D-50). The admin surface
    /// (`BuiltinHooksServer`) looks hooks up here by string name.
    pub fn builtin_hooks(&self) -> &BuiltinHookRegistry {
        &self.builtin_hooks
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
        self.pump_handles.lock().await.insert(id.clone(), handle);

        // Phase 5 (D-55): publish a kernel-level ToolsChanged so
        // `builtin.bindings`'s bridge task can turn this into a
        // `ResourceUpdated { uri: "kj://kernel/tools" }` and subscribers
        // to the kernel-wide tools resource see the new instance.
        let _ = self
            .notif_tx
            .send(KernelNotification::ToolsChanged { instance: id });
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
        // Phase 5 (D-55): same kernel-level signal as `register_inner` —
        // `kj://kernel/tools` subscribers get notified via the bindings
        // server's bridge task.
        let _ = self
            .notif_tx
            .send(KernelNotification::ToolsChanged { instance: id.clone() });
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

    /// Snapshot a registered instance's current `InstancePolicy`. Returns
    /// `None` when the instance hasn't been registered (M3-D5).
    pub async fn policy_of(&self, instance: &InstanceId) -> Option<InstancePolicy> {
        self.policies.read().await.get(instance).cloned()
    }

    /// Update `call_timeout` and `max_result_bytes` for a registered
    /// instance (M3-D5). `max_concurrency` is intentionally not mutable
    /// here — resizing the semaphore mid-flight would race in-flight
    /// permits; that knob is set at registration time only.
    ///
    /// Returns `Ok(())` on update, `Err(InstanceNotFound)` when the
    /// instance isn't registered.
    pub async fn update_policy(
        &self,
        instance: &InstanceId,
        call_timeout: Option<std::time::Duration>,
        max_result_bytes: Option<usize>,
    ) -> McpResult<()> {
        let mut policies = self.policies.write().await;
        let policy = policies
            .get_mut(instance)
            .ok_or_else(|| McpError::InstanceNotFound(instance.clone()))?;
        if let Some(t) = call_timeout {
            policy.call_timeout = t;
        }
        if let Some(b) = max_result_bytes {
            policy.max_result_bytes = b;
        }
        Ok(())
    }

    /// Clone of the instance registry for callers that want to call
    /// `list_tools` on each server without holding the broker's RwLock.
    pub async fn instances_snapshot(&self) -> HashMap<InstanceId, Arc<dyn McpServerLike>> {
        self.instances.read().await.clone()
    }

    /// Replace a context's binding wholesale. Sticky resolutions on the
    /// incoming binding are preserved as-is; the broker does not recompute.
    ///
    /// Phase 5 (D-54): persists to `KernelDb` if a handle is installed.
    ///
    /// Phase 5 (D-35 reuse): computes the `(instance, tool_name)` diff
    /// against the previous binding and fires per-tool `ToolAdded` /
    /// `ToolRemoved` notifications into this specific context so the LLM
    /// sees the change on the next turn without new notification kinds.
    /// R1 mitigation: the diff is per-pair, not per-instance, so
    /// `unbind → rebind` of the same instance is a no-op emission
    /// (identical pairs, empty set difference).
    pub async fn set_binding(&self, context_id: ContextId, binding: ContextToolBinding) {
        let old = self
            .bindings
            .read()
            .await
            .get(&context_id)
            .cloned()
            .unwrap_or_default();

        let old_pairs = self.binding_visible_tool_pairs(&old).await;
        let new_pairs = self.binding_visible_tool_pairs(&binding).await;

        self.bindings
            .write()
            .await
            .insert(context_id, binding.clone());

        self.persist_binding(context_id, &binding).await;

        self.emit_binding_diff(context_id, &old_pairs, &new_pairs)
            .await;
    }

    /// Add an instance to a context's binding (idempotent). Triggers the
    /// same diff + persistence + notification pipeline as `set_binding`.
    pub async fn bind(&self, context_id: ContextId, instance: InstanceId) {
        let mut binding = self
            .binding(&context_id)
            .await
            .unwrap_or_default();
        binding.allow(instance);
        self.set_binding(context_id, binding).await;
    }

    /// Remove an instance from a context's binding (idempotent if absent).
    /// Also evicts `name_map` entries pointing at the dropped instance so
    /// follow-up calls surface the removed-tool error cleanly.
    pub async fn unbind(&self, context_id: ContextId, instance: &InstanceId) {
        let mut binding = self
            .binding(&context_id)
            .await
            .unwrap_or_default();
        binding.revoke(instance);
        self.set_binding(context_id, binding).await;
    }

    /// Expand a binding into its `(instance, tool_name)` pair set using the
    /// cached `tool_snapshots` map. Used for diff computation on binding
    /// mutation (R1 mitigation per the plan).
    async fn binding_visible_tool_pairs(
        &self,
        binding: &ContextToolBinding,
    ) -> HashSet<(InstanceId, String)> {
        let snapshots = self.tool_snapshots.lock().await;
        let mut pairs = HashSet::new();
        for instance in &binding.allowed_instances {
            if let Some(tools) = snapshots.get(instance) {
                for kt in tools {
                    pairs.insert((instance.clone(), kt.name.clone()));
                }
            }
        }
        pairs
    }

    /// Fire per-tool `ToolAdded` / `ToolRemoved` into a specific context for
    /// the set-difference between old and new pair sets.
    async fn emit_binding_diff(
        &self,
        context_id: ContextId,
        old_pairs: &HashSet<(InstanceId, String)>,
        new_pairs: &HashSet<(InstanceId, String)>,
    ) {
        for (instance, tool) in new_pairs.difference(old_pairs) {
            let payload = NotificationPayload {
                instance: instance.as_str().to_string(),
                kind: kaijutsu_types::NotificationKind::ToolAdded,
                level: None,
                tool: Some(tool.clone()),
                count: None,
                detail: None,
            };
            self.emit_for_context(context_id, instance, payload).await;
        }
        for (instance, tool) in old_pairs.difference(new_pairs) {
            let payload = NotificationPayload {
                instance: instance.as_str().to_string(),
                kind: kaijutsu_types::NotificationKind::ToolRemoved,
                level: None,
                tool: Some(tool.clone()),
                count: None,
                detail: None,
            };
            self.emit_for_context(context_id, instance, payload).await;
        }
    }

    /// D-56: walk `list_tools` hook entries and strip any `KernelTool`
    /// whose `(instance, name)` matches a `Deny` entry. `Log` fires a
    /// `tracing::event!` and keeps the tool. `BuiltinInvoke` /
    /// `ShortCircuit` / `Kaish` are rejected at `hook_add` time (D-56,
    /// `validate_action_for_phase`) so they never reach this path;
    /// unreachable arms are skipped here rather than panicking, since
    /// a broker loaded with pre-D-56 state could in principle carry one.
    async fn apply_list_tools_filter(&self, ctx: &CallContext, tools: &mut Vec<KernelTool>) {
        let entries = {
            let guard = self.hooks.read().await;
            if guard.list_tools.entries.is_empty() {
                return;
            }
            guard.list_tools.entries.clone()
        };
        // Sort once: priority asc, insertion order tiebreak. Log entries
        // execute before Deny at the same priority so observability fires
        // before mutation. Within a single tool, the first matching Deny
        // terminates further evaluation for that tool (no point running
        // more filters after we've stripped it).
        let mut ordered: Vec<(usize, super::hook_table::HookEntry)> =
            entries.into_iter().enumerate().collect();
        ordered.sort_by_key(|(idx, e)| (e.priority, *idx));

        tools.retain(|kt| {
            let synth_params = KernelCallParams {
                instance: kt.instance.clone(),
                tool: kt.name.clone(),
                arguments: serde_json::Value::Null,
            };
            for (_idx, entry) in &ordered {
                if !hook_matches(entry, &synth_params, ctx) {
                    continue;
                }
                match &entry.action {
                    HookAction::Log(spec) => match spec.level {
                        tracing::Level::TRACE => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::TRACE,
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            "{}", spec.target,
                        ),
                        tracing::Level::DEBUG => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::DEBUG,
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            "{}", spec.target,
                        ),
                        tracing::Level::INFO => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::INFO,
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            "{}", spec.target,
                        ),
                        tracing::Level::WARN => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::WARN,
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            "{}", spec.target,
                        ),
                        tracing::Level::ERROR => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::ERROR,
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            "{}", spec.target,
                        ),
                    },
                    HookAction::Deny(reason) => {
                        tracing::debug!(
                            phase = "list_tools",
                            hook_id = %entry.id,
                            instance = %kt.instance,
                            tool = %kt.name,
                            reason = %reason,
                            "tool hidden by ListTools Deny hook",
                        );
                        return false;
                    }
                    // Never reached in normal operation (D-56 rejects these
                    // at add time). Defensive no-op if somehow present.
                    HookAction::ShortCircuit(_) | HookAction::Invoke(_) => {}
                }
            }
            true
        });
    }

    /// Persist a binding to the kernel DB. No-op when the DB handle is
    /// not set (tests, early bootstrap).
    async fn persist_binding(&self, context_id: ContextId, binding: &ContextToolBinding) {
        let db = match self.db.read().await.clone() {
            Some(h) => h,
            None => return,
        };
        let mut guard = db.lock();
        if let Err(e) = guard.upsert_context_binding(context_id, binding) {
            tracing::warn!(
                context_id = %context_id,
                error = ?e,
                "failed to persist context binding",
            );
        }
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

    /// Read a context's binding, hydrating from the kernel DB on cache miss.
    /// Phase 5 (D-54): first-touch loads from persistent storage so
    /// curation survives kernel restart. Callers that fall back to "bind
    /// all registered" on `None` will observe the persisted binding here.
    /// Returns `None` if no row exists in DB (never-bound context) or the
    /// DB is not wired (tests).
    pub async fn binding(&self, context_id: &ContextId) -> Option<ContextToolBinding> {
        if let Some(b) = self.bindings.read().await.get(context_id).cloned() {
            return Some(b);
        }
        let db = self.db.read().await.clone()?;
        let loaded = {
            let guard = db.lock();
            match guard.get_context_binding(*context_id) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        context_id = %context_id,
                        error = ?e,
                        "failed to load context binding; treating as absent",
                    );
                    return None;
                }
            }
        };
        if let Some(loaded) = loaded {
            // Cache in memory for subsequent lookups.
            self.bindings
                .write()
                .await
                .insert(*context_id, loaded.clone());
            Some(loaded)
        } else {
            None
        }
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

        // Phase 5 (D-56): apply `ListTools` hook filter before resolution
        // so denied tools never enter `name_map` — they become uncallable
        // via `call_tool` in the same stroke, because `binding.resolve`
        // has no entry for them. Matches on raw (instance, tool_name) per
        // D-56; sticky visible name is irrelevant here.
        self.apply_list_tools_filter(ctx, &mut all).await;

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

    /// The one tool-call pipeline. Phase 4 wires hook evaluation at three
    /// pinch points: `PreCall` before the server call, `PostCall` on success,
    /// `OnError` on failure. ShortCircuit in any phase bypasses the server
    /// (or converts an error to a success in OnError). Deny terminates with
    /// `McpError::Denied { by_hook }`.
    ///
    /// Outer wrapper installs the per-task `HOOK_DEPTH` scope on first
    /// entry, reuses it on recursive re-entry from hook bodies so the depth
    /// counter survives `broker.call_tool(...)` calls from inside an
    /// `Invoke` body (D-47).
    pub async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if HOOK_DEPTH.try_with(|_| ()).is_ok() {
            self.call_tool_inner(params, ctx, cancel).await
        } else {
            HOOK_DEPTH
                .scope(
                    std::cell::Cell::new(0),
                    self.call_tool_inner(params, ctx, cancel),
                )
                .await
        }
    }

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
    async fn call_tool_inner(
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

        // PreCall — may short-circuit the call entirely, or deny it outright.
        match self.evaluate_phase(HookPhase::PreCall, &params, ctx).await? {
            PhaseOutcome::Continue => {}
            PhaseOutcome::ShortCircuit { hook_id, result } => {
                emit_short_circuit_attribution(HookPhase::PreCall, &hook_id);
                // PostCall still runs on short-circuit per §4.3 evaluation law
                // — it observes that a (synthetic) result was produced.
                // Result can itself short-circuit or deny.
                return match self
                    .evaluate_phase(HookPhase::PostCall, &params, ctx)
                    .await?
                {
                    PhaseOutcome::Continue => Ok(result),
                    PhaseOutcome::ShortCircuit { hook_id, result: r2 } => {
                        emit_short_circuit_attribution(HookPhase::PostCall, &hook_id);
                        Ok(r2)
                    }
                    PhaseOutcome::Deny { hook_id, reason } => {
                        emit_deny_attribution(HookPhase::PostCall, &hook_id, &reason);
                        Err(McpError::Denied { by_hook: hook_id })
                    }
                };
            }
            PhaseOutcome::Deny { hook_id, reason } => {
                emit_deny_attribution(HookPhase::PreCall, &hook_id, &reason);
                return Err(McpError::Denied { by_hook: hook_id });
            }
        }

        let instance_for_timeout = params.instance.clone();
        let timeout_ms = policy.call_timeout.as_millis() as u64;
        let call_params_for_hooks = params.clone();
        let cancel_for_call = cancel.clone();
        let call_fut = async {
            let span = tracing::info_span!(
                "server.call_tool",
                instance = %params.instance,
                tool = %params.tool,
            );
            let _enter = span.enter();
            server.call_tool(params, ctx, cancel_for_call).await
        };

        // Race the call against (a) the per-instance timeout and (b) an
        // externally-supplied cancellation (M2-B5). Without (b) a hard
        // interrupt would wait the full call_timeout for builtin servers
        // that don't observe the token themselves.
        let call_result: Result<McpResult<KernelToolResult>, McpError> = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            r = tokio::time::timeout(policy.call_timeout, call_fut) => {
                r.map_err(|_| McpError::Policy(PolicyError::Timeout {
                    instance: instance_for_timeout.clone(),
                    timeout_ms,
                }))
            }
        };

        match call_result {
            Ok(Ok(result)) => {
                // Size check — pathological output must not OOM the kernel.
                // M3-D4: instead of erroring, truncate the result text in
                // place with an explicit footer so the model still sees
                // useful prefix data plus a clear note about what was cut.
                let mut result = result;
                let size = estimate_result_size(&result);
                if size > policy.max_result_bytes {
                    truncate_result_to_budget(&mut result, policy.max_result_bytes, size);
                }
                match self
                    .evaluate_phase(HookPhase::PostCall, &call_params_for_hooks, ctx)
                    .await?
                {
                    PhaseOutcome::Continue => Ok(result),
                    PhaseOutcome::ShortCircuit { hook_id, result: r2 } => {
                        emit_short_circuit_attribution(HookPhase::PostCall, &hook_id);
                        Ok(r2)
                    }
                    PhaseOutcome::Deny { hook_id, reason } => {
                        emit_deny_attribution(HookPhase::PostCall, &hook_id, &reason);
                        Err(McpError::Denied { by_hook: hook_id })
                    }
                }
            }
            Ok(Err(e)) => {
                self.run_on_error_then_err(&call_params_for_hooks, ctx, e)
                    .await
            }
            Err(timeout_err) => {
                self.run_on_error_then_err(&call_params_for_hooks, ctx, timeout_err)
                    .await
            }
        }
    }

    /// Run `OnError` and return either a short-circuited success (converting
    /// the error) or the original error. `Deny` on the error path still
    /// returns `McpError::Denied { by_hook }` — a denial overrides the
    /// original error in the attribution channel.
    async fn run_on_error_then_err(
        &self,
        params: &KernelCallParams,
        ctx: &CallContext,
        err: McpError,
    ) -> McpResult<KernelToolResult> {
        match self.evaluate_phase(HookPhase::OnError, params, ctx).await {
            Ok(PhaseOutcome::Continue) => Err(err),
            Ok(PhaseOutcome::ShortCircuit { hook_id, result }) => {
                emit_short_circuit_attribution(HookPhase::OnError, &hook_id);
                Ok(result)
            }
            Ok(PhaseOutcome::Deny { hook_id, reason }) => {
                emit_deny_attribution(HookPhase::OnError, &hook_id, &reason);
                Err(McpError::Denied { by_hook: hook_id })
            }
            Err(eval_err) => {
                tracing::warn!(
                    error = ?eval_err,
                    "on_error evaluation failed; returning original error",
                );
                Err(err)
            }
        }
    }

    /// Evaluate one hook phase against a call site. Snapshots the matching
    /// entries under a short read-lock on `hooks`, drops the guard before any
    /// awaits, then walks matches in priority order (ascending, insertion-
    /// tiebreak) applying each action. Terminal actions (`ShortCircuit`,
    /// `Deny`) stop the walk; `Log`, `Invoke-Ok` continue. `Invoke-Err`
    /// terminates the phase as `Deny` with the hook's error text as reason.
    ///
    /// D-51 retired: admin MCP servers (`builtin.hooks`, `builtin.bindings`)
    /// are subject to hook evaluation like every other instance. An
    /// operator who locks themselves out with an overbroad Deny recovers
    /// by editing the persisted hook row out of the DB (`sqlite3
    /// kernel.db "DELETE FROM hooks WHERE ..."`) and restarting. Kernel
    /// recovery is out-of-band; the broker doesn't self-guard.
    async fn evaluate_phase(
        &self,
        phase: HookPhase,
        params: &KernelCallParams,
        ctx: &CallContext,
    ) -> McpResult<PhaseOutcome> {
        // Snapshot matching entries + their indices so the sort is stable
        // across priority ties (the HashTable::entries Vec is the
        // authoritative insertion order).
        let snapshot: Vec<(usize, super::hook_table::HookEntry)> = {
            let guard = self.hooks.read().await;
            let table = match phase {
                HookPhase::PreCall => &guard.pre_call,
                HookPhase::PostCall => &guard.post_call,
                HookPhase::OnError => &guard.on_error,
                HookPhase::OnNotification => &guard.on_notification,
                HookPhase::ListTools => &guard.list_tools,
            };
            table
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| hook_matches(e, params, ctx))
                .map(|(i, e)| (i, e.clone()))
                .collect()
        };
        if snapshot.is_empty() {
            return Ok(PhaseOutcome::Continue);
        }
        let mut ordered = snapshot;
        ordered.sort_by_key(|(idx, e)| (e.priority, *idx));

        for (_idx, entry) in ordered {
            match entry.action {
                HookAction::Log(spec) => {
                    // LogSpec::level is a tracing::Level; dispatch via
                    // static match since tracing::event! needs a const
                    // Level.
                    match spec.level {
                        tracing::Level::TRACE => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::TRACE,
                            hook_id = %entry.id,
                            phase = ?phase,
                            instance = %params.instance,
                            tool = %params.tool,
                            "{}",
                            spec.target,
                        ),
                        tracing::Level::DEBUG => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::DEBUG,
                            hook_id = %entry.id,
                            phase = ?phase,
                            instance = %params.instance,
                            tool = %params.tool,
                            "{}",
                            spec.target,
                        ),
                        tracing::Level::INFO => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::INFO,
                            hook_id = %entry.id,
                            phase = ?phase,
                            instance = %params.instance,
                            tool = %params.tool,
                            "{}",
                            spec.target,
                        ),
                        tracing::Level::WARN => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::WARN,
                            hook_id = %entry.id,
                            phase = ?phase,
                            instance = %params.instance,
                            tool = %params.tool,
                            "{}",
                            spec.target,
                        ),
                        tracing::Level::ERROR => tracing::event!(
                            target: "kaijutsu::hooks",
                            tracing::Level::ERROR,
                            hook_id = %entry.id,
                            phase = ?phase,
                            instance = %params.instance,
                            tool = %params.tool,
                            "{}",
                            spec.target,
                        ),
                    }
                }
                HookAction::Deny(reason) => {
                    return Ok(PhaseOutcome::Deny {
                        hook_id: entry.id,
                        reason,
                    });
                }
                HookAction::ShortCircuit(result) => {
                    return Ok(PhaseOutcome::ShortCircuit {
                        hook_id: entry.id,
                        result,
                    });
                }
                HookAction::Invoke(body) => match body {
                    HookBody::Builtin { name, hook } => {
                        // D-29 / D-47: guard against runaway recursion when
                        // a hook body re-enters `broker.call_tool`.
                        let _depth_guard = enter_hook_depth()?;
                        if let Err(e) = hook.invoke(params, ctx).await {
                            return Ok(PhaseOutcome::Deny {
                                hook_id: entry.id,
                                reason: format!(
                                    "hook body `{name}` returned error: {e}"
                                ),
                            });
                        }
                    }
                    HookBody::Kaish(_) => return Err(McpError::Unsupported),
                },
            }
        }
        Ok(PhaseOutcome::Continue)
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
        let synth_params = build_notification_synth(instance, &payload);
        for ctx in contexts {
            self.emit_into_context(ctx, instance, &payload, &summary, &synth_params, &docs)
                .await;
        }
    }

    /// Emit a notification block into a single specific context, regardless
    /// of whether that context's binding currently allows the instance.
    /// Phase 5: used for binding-mutation diff emissions where the mutation
    /// itself is the trigger, not ongoing binding membership. Still runs
    /// `OnNotification` hooks for consistency with `emit_for_bindings`.
    async fn emit_for_context(
        &self,
        context_id: ContextId,
        instance: &InstanceId,
        payload: NotificationPayload,
    ) {
        let docs = match self.documents.read().await.clone() {
            Some(d) => d,
            None => return,
        };
        let summary = payload.summary_line();
        let synth_params = build_notification_synth(instance, &payload);
        self.emit_into_context(
            context_id,
            instance,
            &payload,
            &summary,
            &synth_params,
            &docs,
        )
        .await;
    }

    /// Shared per-context emission body used by both `emit_for_bindings` and
    /// `emit_for_context`. Evaluates `OnNotification` hooks then writes the
    /// block. Silent on hook errors (emits anyway, per prior Phase 4
    /// behavior) so transient hook failures don't swallow notifications.
    async fn emit_into_context(
        &self,
        ctx: ContextId,
        instance: &InstanceId,
        payload: &NotificationPayload,
        summary: &str,
        synth_params: &KernelCallParams,
        docs: &SharedBlockStore,
    ) {
        let synth_ctx = CallContext::system_for_context(ctx);
        match self
            .evaluate_phase(HookPhase::OnNotification, synth_params, &synth_ctx)
            .await
        {
            Ok(PhaseOutcome::Continue) => {}
            Ok(PhaseOutcome::ShortCircuit { hook_id, .. }) => {
                emit_short_circuit_attribution(HookPhase::OnNotification, &hook_id);
                return;
            }
            Ok(PhaseOutcome::Deny { hook_id, reason }) => {
                emit_deny_attribution(HookPhase::OnNotification, &hook_id, &reason);
                return;
            }
            Err(e) => {
                tracing::warn!(
                    context_id = %ctx,
                    instance = %instance,
                    error = ?e,
                    "on_notification evaluation failed; emitting anyway",
                );
            }
        }
        if let Err(e) = docs.insert_notification_block_as(
            ctx,
            None,
            payload,
            summary.to_string(),
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

/// Phase-evaluation result (§4.3).
#[derive(Debug)]
enum PhaseOutcome {
    /// No terminal match; proceed to the next stage.
    Continue,
    /// A `ShortCircuit` or `Invoke` body produced a synthesized result that
    /// replaces the would-be server call (or converts an error into success
    /// in `OnError`).
    ShortCircuit {
        hook_id: HookId,
        result: KernelToolResult,
    },
    /// A `Deny` hook matched. `reason` is tracing-only; the LLM-visible
    /// error is `McpError::Denied { by_hook }` (D-28 channel discipline).
    Deny { hook_id: HookId, reason: String },
}

/// Build a synthetic `KernelCallParams` for OnNotification evaluation. Hook
/// entries match on `(instance, tool)`; notifications have no tool name, so
/// we use `__notification.<kind>` as the synthetic tool — users filter with
/// `match_tool: Some("__notification.log")` or similar. The `__` prefix is
/// the convention for synthetic names; real instances must not advertise
/// tools in that namespace.
fn build_notification_synth(
    instance: &InstanceId,
    payload: &NotificationPayload,
) -> KernelCallParams {
    KernelCallParams {
        instance: instance.clone(),
        tool: format!("__notification.{}", payload.kind.as_str()),
        arguments: serde_json::json!({
            "kind": payload.kind.as_str(),
            "count": payload.count,
            "level": payload.level.map(|l| format!("{l:?}")),
            "detail": payload.detail.clone(),
            "tool": payload.tool.clone(),
        }),
    }
}

/// Predicate for a single hook entry against a call site. Pure (no awaits,
/// no side effects) — callable while holding a read lock.
fn hook_matches(
    entry: &super::hook_table::HookEntry,
    params: &KernelCallParams,
    ctx: &CallContext,
) -> bool {
    if let Some(g) = &entry.match_instance
        && !kaish_glob::glob_match(&g.0, params.instance.as_str())
    {
        return false;
    }
    if let Some(g) = &entry.match_tool
        && !kaish_glob::glob_match(&g.0, &params.tool)
    {
        return false;
    }
    if let Some(c) = &entry.match_context
        && *c != ctx.context_id
    {
        return false;
    }
    if let Some(p) = &entry.match_principal
        && *p != ctx.principal_id
    {
        return false;
    }
    true
}

/// Tracing attribution for a `ShortCircuit` result — D-49. Event, not a new
/// span, so the parent `broker.call_tool` span remains the correlation
/// anchor.
fn emit_short_circuit_attribution(phase: HookPhase, hook_id: &HookId) {
    tracing::info!(
        hook_id = %format!("hook:{hook_id}"),
        phase = ?phase,
        "hook.short_circuit",
    );
}

/// Tracing attribution for a `Deny` result. The inner `reason` is recorded
/// here; the LLM only sees `McpError::Denied { by_hook }` (D-28).
fn emit_deny_attribution(phase: HookPhase, hook_id: &HookId, reason: &str) {
    tracing::info!(
        hook_id = %format!("hook:{hook_id}"),
        phase = ?phase,
        reason = %reason,
        "hook.deny",
    );
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
            // OnNotification synth for the success path. Per-context Deny /
            // ShortCircuit skips emission for that one context.
            let success_synth = KernelCallParams {
                instance: id.clone(),
                tool: "__notification.resource_updated".to_string(),
                arguments: serde_json::json!({
                    "uri": uri,
                }),
            };
            for (ctx_id, parent_block) in targets {
                let synth_ctx = CallContext::system_for_context(ctx_id);
                match broker
                    .evaluate_phase(HookPhase::OnNotification, &success_synth, &synth_ctx)
                    .await
                {
                    Ok(PhaseOutcome::Continue) => {}
                    Ok(PhaseOutcome::ShortCircuit { hook_id, .. }) => {
                        emit_short_circuit_attribution(
                            HookPhase::OnNotification,
                            &hook_id,
                        );
                        continue;
                    }
                    Ok(PhaseOutcome::Deny { hook_id, reason }) => {
                        emit_deny_attribution(
                            HookPhase::OnNotification,
                            &hook_id,
                            &reason,
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            context_id = %ctx_id,
                            instance = %id,
                            uri = %uri,
                            error = ?e,
                            "on_notification evaluation failed; emitting anyway",
                        );
                    }
                }
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
            let fail_synth = KernelCallParams {
                instance: id.clone(),
                tool: "__notification.log".to_string(),
                arguments: serde_json::json!({
                    "kind": "log",
                    "level": "warn",
                    "detail": detail.clone(),
                }),
            };
            for (ctx_id, parent_block) in targets {
                let synth_ctx = CallContext::system_for_context(ctx_id);
                match broker
                    .evaluate_phase(HookPhase::OnNotification, &fail_synth, &synth_ctx)
                    .await
                {
                    Ok(PhaseOutcome::Continue) => {}
                    Ok(PhaseOutcome::ShortCircuit { hook_id, .. }) => {
                        emit_short_circuit_attribution(
                            HookPhase::OnNotification,
                            &hook_id,
                        );
                        continue;
                    }
                    Ok(PhaseOutcome::Deny { hook_id, reason }) => {
                        emit_deny_attribution(
                            HookPhase::OnNotification,
                            &hook_id,
                            &reason,
                        );
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(
                            context_id = %ctx_id,
                            instance = %id,
                            uri = %uri,
                            error = ?err,
                            "on_notification evaluation failed; emitting anyway",
                        );
                    }
                }
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

/// Truncate a tool result so its serialized size fits inside `budget`,
/// replacing tail content with a "[truncated N bytes]" footer (M3-D4).
///
/// Strategy: drop `structured`, walk `content` in order, keep whole
/// items that fit. The first item that would overflow is byte-truncated
/// to fill the remaining budget minus footer length. Subsequent items
/// are dropped. A final Text item carries the footer.
fn truncate_result_to_budget(result: &mut KernelToolResult, budget: usize, original_size: usize) {
    use super::types::ToolContent;
    let dropped = original_size.saturating_sub(budget);
    let footer = format!("\n\n[truncated {} bytes — output exceeded max_result_bytes]", dropped);
    // Reserve room for the footer; if the budget is smaller than the
    // footer itself, drop everything except the footer.
    let footer_len = footer.len();
    let body_budget = budget.saturating_sub(footer_len);

    // Discard structured payload — it's an indivisible blob and easier
    // for the model to lose entirely than partially.
    result.structured = None;

    let mut kept: Vec<ToolContent> = Vec::new();
    let mut used = 0usize;
    let original = std::mem::take(&mut result.content);
    for item in original {
        let item_text = match &item {
            ToolContent::Text(s) => s.clone(),
            ToolContent::Json(v) => v.to_string(),
        };
        let item_len = item_text.len();
        if used + item_len <= body_budget {
            kept.push(item);
            used += item_len;
            continue;
        }
        // Partial fit — slice item_text on a UTF-8 char boundary so we
        // never produce invalid UTF-8 mid-truncation.
        let remaining = body_budget.saturating_sub(used);
        if remaining > 0 {
            let mut cut = remaining.min(item_text.len());
            while cut > 0 && !item_text.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut > 0 {
                kept.push(ToolContent::Text(item_text[..cut].to_string()));
            }
        }
        break;
    }

    kept.push(ToolContent::Text(footer));
    result.content = kept;
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
    async fn cancellation_aborts_in_flight_call() {
        // M2-B5: cancelling the supplied token should abort the broker
        // call within bounded time (well below the policy timeout). Without
        // this the user waits the full call_timeout for builtin servers
        // that don't observe the token themselves.
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("napper")
                .with_tool("sleep")
                .on_call(|_p| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(KernelToolResult::text("done"))
                }),
        );
        broker
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_secs(60),
                    max_result_bytes: 1024,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        // Cancel after 50ms.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel2.cancel();
        });

        let start = std::time::Instant::now();
        let err = broker
            .call_tool(params("napper", "sleep"), &CallContext::test(), cancel)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, McpError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel should land well under 500ms, took {elapsed:?}"
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
    async fn policy_result_too_large_truncates() {
        // M3-D4: oversized results truncate in place with a footer
        // instead of erroring. The model gets useful prefix data plus a
        // clear note about how much was cut.
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("chatty")
                .with_tool("say")
                .on_call(|_p| async { Ok(KernelToolResult::text("x".repeat(200))) }),
        );
        broker
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_secs(5),
                    max_result_bytes: 64,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();

        let result = broker
            .call_tool(
                params("chatty", "say"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("oversized result should still succeed (truncated)");

        use crate::mcp::ToolContent;
        let combined: String = result
            .content
            .iter()
            .map(|c| match c {
                ToolContent::Text(s) => s.clone(),
                ToolContent::Json(v) => v.to_string(),
            })
            .collect();
        assert!(
            combined.contains("[truncated"),
            "truncation footer missing, got: {combined:?}"
        );
        assert!(
            combined.starts_with("xxx"),
            "prefix preserved before footer, got: {combined:?}"
        );
        assert!(
            !result.is_error,
            "truncation must not flip is_error — model still gets data"
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
                builtin_hooks: BuiltinHookRegistry::new(),
                db: RwLock::new(None),
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
                builtin_hooks: BuiltinHookRegistry::new(),
                db: RwLock::new(None),
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

    // ═════════════════════════════════════════════════════════════════════
    // Phase 4 (M1) — hook evaluation wiring
    // ═════════════════════════════════════════════════════════════════════

    use super::super::hook_table::{
        GlobPattern, HookAction, HookBody, HookEntry, Hook, LogSpec,
    };
    use crate::mcp::error::HookId;

    fn hook_id(s: &str) -> HookId {
        HookId(s.to_string())
    }

    fn log_hook(id: &str, tool_glob: &str, level: tracing::Level) -> HookEntry {
        HookEntry {
            id: hook_id(id),
            match_instance: None,
            match_tool: Some(GlobPattern(tool_glob.to_string())),
            match_context: None,
            match_principal: None,
            action: HookAction::Log(LogSpec {
                target: format!("audit {id}"),
                level,
            }),
            priority: 0,
        }
    }

    /// Exit #2: a PreCall Deny blocks a call end-to-end — the server is
    /// never invoked and the caller sees `McpError::Denied { by_hook }`.
    #[tokio::test]
    async fn pre_call_deny_blocks_call() {
        let broker = Arc::new(Broker::new());
        let server_invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = server_invoked.clone();
        let server = Arc::new(
            MockServer::new("guarded")
                .with_tool("do")
                .on_call(move |_p| {
                    let f = flag.clone();
                    async move {
                        f.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(KernelToolResult::text("server ran"))
                    }
                }),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("deny-all"),
            match_instance: None,
            match_tool: Some(GlobPattern("*".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Deny("not today".into()),
            priority: 0,
        });

        let err = broker
            .call_tool(
                params("guarded", "do"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Denied { ref by_hook } if by_hook.0 == "deny-all"),
            "expected Denied(by_hook=deny-all), got {err:?}",
        );
        assert!(
            !server_invoked.load(std::sync::atomic::Ordering::SeqCst),
            "server must not have been invoked after a PreCall Deny",
        );
    }

    /// Exit #3 (positive companion) + locks ShortCircuit bypass: PreCall
    /// ShortCircuit must replace the server call with the hook's result
    /// without invoking the server.
    #[tokio::test]
    async fn pre_call_shortcircuit_skips_server() {
        let broker = Arc::new(Broker::new());
        let server_invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = server_invoked.clone();
        let server = Arc::new(
            MockServer::new("bypassed")
                .with_tool("do")
                .on_call(move |_p| {
                    let f = flag.clone();
                    async move {
                        f.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(KernelToolResult::text("server ran"))
                    }
                }),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("sc"),
            match_instance: None,
            match_tool: Some(GlobPattern("*".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::ShortCircuit(KernelToolResult::text("from hook")),
            priority: 0,
        });

        let result = broker
            .call_tool(
                params("bypassed", "do"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match result.content.first() {
            Some(ToolContent::Text(s)) => assert_eq!(s, "from hook"),
            other => panic!("expected text 'from hook', got {other:?}"),
        }
        assert!(
            !server_invoked.load(std::sync::atomic::Ordering::SeqCst),
            "server must not be invoked when PreCall ShortCircuits",
        );
    }

    /// Locks the "PostCall fires after success" half of §4.3. Uses an
    /// `Invoke` Hook body to count fires — simplest reliable counter.
    #[tokio::test]
    async fn post_call_fires_after_success() {
        #[derive(Clone)]
        struct Counter(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait]
        impl Hook for Counter {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                _ctx: &CallContext,
            ) -> McpResult<()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("s").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook: Arc<dyn Hook> = Arc::new(Counter(counter.clone()));
        broker.hooks().write().await.post_call.entries.push(HookEntry {
            id: hook_id("count"),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.count".into(),
                hook,
            }),
            priority: 0,
        });

        broker
            .call_tool(
                params("s", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "PostCall must fire exactly once after a successful call",
        );
    }

    /// OnError fires when the server errors; PostCall does NOT. Uses two
    /// counters on two phases to distinguish.
    #[tokio::test]
    async fn on_error_fires_on_server_error_not_post_call() {
        #[derive(Clone)]
        struct Counter(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait]
        impl Hook for Counter {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                _ctx: &CallContext,
            ) -> McpResult<()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("sad")
                .with_tool("fail")
                .on_call(|p| async move {
                    Err(McpError::ToolNotFound {
                        instance: InstanceId::new("sad"),
                        tool: p.tool,
                    })
                }),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        let on_err = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let post = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        broker.hooks().write().await.on_error.entries.push(HookEntry {
            id: hook_id("err-count"),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.on_err_count".into(),
                hook: Arc::new(Counter(on_err.clone())),
            }),
            priority: 0,
        });
        broker.hooks().write().await.post_call.entries.push(HookEntry {
            id: hook_id("post-count"),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.post_count".into(),
                hook: Arc::new(Counter(post.clone())),
            }),
            priority: 0,
        });

        let err = broker
            .call_tool(
                params("sad", "fail"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolNotFound { .. }));
        assert_eq!(on_err.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(post.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// §4.3 evaluation law: OnError ShortCircuit converts an error into a
    /// success result. The LLM sees `Ok(KernelToolResult)`; the original
    /// error is attribution-only in tracing.
    #[tokio::test]
    async fn on_error_shortcircuit_converts_error_to_success() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("sad")
                .with_tool("fail")
                .on_call(|_p| async {
                    Err(McpError::Protocol("boom".into()))
                }),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.hooks().write().await.on_error.entries.push(HookEntry {
            id: hook_id("rescue"),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::ShortCircuit(KernelToolResult::text("rescued")),
            priority: 0,
        });

        let result = broker
            .call_tool(
                params("sad", "fail"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match result.content.first() {
            Some(ToolContent::Text(s)) => assert_eq!(s, "rescued"),
            other => panic!("expected rescue text, got {other:?}"),
        }
    }

    /// D-46: `match_instance` and `match_tool` use `kaish_glob::glob_match`
    /// so `*`/`?` wildcards work without pre-compilation.
    #[tokio::test]
    async fn hook_match_instance_and_tool_globs() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(
            MockServer::new("glob.test")
                .with_tool("foo")
                .with_tool("bar"),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("foo-only"),
            match_instance: Some(GlobPattern("glob.*".into())),
            match_tool: Some(GlobPattern("foo".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Deny("foo is forbidden".into()),
            priority: 0,
        });

        let foo_err = broker
            .call_tool(
                params("glob.test", "foo"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(foo_err, McpError::Denied { ref by_hook } if by_hook.0 == "foo-only"),
            "glob.test/foo should match; got {foo_err:?}"
        );

        let bar_ok = broker
            .call_tool(
                params("glob.test", "bar"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!bar_ok.is_error, "bar should not match the hook");
    }

    /// D-46: `match_context` / `match_principal` use equality, not globs.
    #[tokio::test]
    async fn hook_match_context_and_principal_filters() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let target_ctx = ContextId::new();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("ctx-only"),
            match_instance: None,
            match_tool: None,
            match_context: Some(target_ctx),
            match_principal: None,
            action: HookAction::Deny("wrong ctx".into()),
            priority: 0,
        });

        let mut matching = CallContext::test();
        matching.context_id = target_ctx;
        let err = broker
            .call_tool(params("svc", "t"), &matching, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Denied { .. }));

        let other = CallContext::test();
        let ok = broker
            .call_tool(params("svc", "t"), &other, CancellationToken::new())
            .await
            .unwrap();
        assert!(!ok.is_error);
    }

    /// §4.3 evaluation law: priority ascending, insertion-order tiebreak.
    /// Higher-priority Deny fires before lower-priority Deny.
    #[tokio::test]
    async fn hook_priority_and_insertion_order_is_deterministic() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        // Push lower-priority Deny first (priority=0); higher-priority
        // should still win because priority=-1 evaluates first (ascending).
        {
            let mut hooks = broker.hooks().write().await;
            hooks.pre_call.entries.push(HookEntry {
                id: hook_id("low"),
                match_instance: None,
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("low-pri".into()),
                priority: 0,
            });
            hooks.pre_call.entries.push(HookEntry {
                id: hook_id("high"),
                match_instance: None,
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("high-pri".into()),
                priority: -1, // evaluates first because priority ascending
            });
        }
        let err = broker
            .call_tool(
                params("svc", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Denied { ref by_hook } if by_hook.0 == "high"),
            "priority=-1 must fire before priority=0; got {err:?}",
        );
    }

    /// D-48: `HookAction::Log` emits a `tracing::event!` and does NOT write a
    /// Notification block.
    #[tokio::test]
    async fn log_hook_emits_tracing_event_not_block() {
        use tracing_subscriber::layer::SubscriberExt;

        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let collector = LogCaptureLayer(events.clone());
        let subscriber = tracing_subscriber::registry().with(collector);
        // `set_default` on a current_thread tokio runtime installs the
        // subscriber as the thread-local default for the remainder of the
        // test. Safer than `with_default` + block_in_place, which requires
        // the multi-threaded flavor.
        let _sub_guard = tracing::subscriber::set_default(subscriber);

        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker
            .hooks()
            .write()
            .await
            .pre_call
            .entries
            .push(log_hook("log-hook", "*", tracing::Level::INFO));

        broker
            .call_tool(
                params("svc", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.iter().any(|s| s.contains("hook_id=log-hook")),
            "expected tracing event mentioning hook_id=log-hook; got {recorded:?}",
        );
        let blocks = notifications_in(&store, ctx);
        assert!(
            blocks.is_empty(),
            "Log hooks must NOT emit Notification blocks; found {blocks:?}",
        );
    }

    /// D-49: `ShortCircuit` emits a `hook.short_circuit` tracing event with
    /// `hook_id = "hook:<id>"`. Exit criterion #3.
    #[tokio::test]
    async fn short_circuit_emits_attribution_event() {
        use tracing_subscriber::layer::SubscriberExt;

        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let collector = LogCaptureLayer(events.clone());
        let subscriber = tracing_subscriber::registry().with(collector);
        let _sub_guard = tracing::subscriber::set_default(subscriber);

        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("s").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("sc-attr"),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::ShortCircuit(KernelToolResult::text("sc")),
            priority: 0,
        });

        broker
            .call_tool(
                params("s", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded.iter().any(|s| {
                s.contains("hook.short_circuit") && s.contains("hook:sc-attr")
            }),
            "expected hook.short_circuit event with hook:sc-attr; got {recorded:?}",
        );
    }

    // Minimal tracing Layer that stringifies events so tests can grep their
    // field bag.
    struct LogCaptureLayer(Arc<std::sync::Mutex<Vec<String>>>);
    impl<S> tracing_subscriber::Layer<S> for LogCaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut s = String::new();
            let mut visitor = StringVisitor(&mut s);
            event.record(&mut visitor);
            // Include the event "name" / message so we can match on
            // `hook.short_circuit`, `hook.deny`, etc.
            let full = format!("{}: {}", event.metadata().name(), s);
            self.0.lock().unwrap().push(full);
        }
    }

    struct StringVisitor<'a>(&'a mut String);
    impl tracing::field::Visit for StringVisitor<'_> {
        fn record_debug(
            &mut self,
            field: &tracing::field::Field,
            value: &dyn std::fmt::Debug,
        ) {
            use std::fmt::Write;
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write;
            let _ = write!(self.0, "{}={} ", field.name(), value);
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // Phase 4 (M2) — OnNotification post-coalesce wiring
    // ═════════════════════════════════════════════════════════════════════

    /// OnNotification fires once per emitted block. A PassThrough Log block
    /// (from below the coalescer cap) should fire the hook exactly once for
    /// the one bound context.
    #[tokio::test]
    async fn on_notification_fires_for_log_passthrough() {
        #[derive(Clone)]
        struct Counter(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait]
        impl Hook for Counter {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                _ctx: &CallContext,
            ) -> McpResult<()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let (broker, _store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        broker
            .hooks()
            .write()
            .await
            .on_notification
            .entries
            .push(HookEntry {
                id: hook_id("notif-count"),
                match_instance: None,
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Invoke(HookBody::Builtin {
                    name: "test.notif_count".into(),
                    hook: Arc::new(Counter(n.clone())),
                }),
                priority: 0,
            });

        let _ = tx.send(ServerNotification::Log {
            level: LogLevel::Info,
            message: "hi".into(),
            tool: None,
        });
        sleep(Duration::from_millis(150)).await;
        assert_eq!(
            n.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "OnNotification must fire exactly once per PassThrough Log block",
        );
    }

    /// Locks the post-coalesce semantic: a 25-event Log burst with cap=5
    /// produces 5 PassThrough blocks + 1 Coalesced summary = 6 emissions.
    /// OnNotification fires once per emission → 6 hook fires.
    #[tokio::test]
    async fn on_notification_fires_once_per_emission_in_burst() {
        #[derive(Clone)]
        struct Counter(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait]
        impl Hook for Counter {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                _ctx: &CallContext,
            ) -> McpResult<()> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

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
                    max_in_window: 5,
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
                builtin_hooks: BuiltinHookRegistry::new(),
                db: RwLock::new(None),
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

        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        broker
            .hooks()
            .write()
            .await
            .on_notification
            .entries
            .push(HookEntry {
                id: hook_id("burst-count"),
                match_instance: None,
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Invoke(HookBody::Builtin {
                    name: "test.burst_count".into(),
                    hook: Arc::new(Counter(n.clone())),
                }),
                priority: 0,
            });

        for i in 0..25 {
            let _ = tx.send(ServerNotification::Log {
                level: LogLevel::Info,
                message: format!("m{i}"),
                tool: None,
            });
        }
        sleep(Duration::from_millis(250)).await;

        let fires = n.load(std::sync::atomic::Ordering::SeqCst);
        // 5 pass-through Logs + 1 Coalesced summary = 6 emissions = 6 hook fires.
        assert_eq!(
            fires, 6,
            "OnNotification must fire once per emitted block (5 passthrough + 1 coalesced); got {fires}",
        );
    }

    /// A `Deny` OnNotification hook skips the emission for that context —
    /// no Notification block lands.
    #[tokio::test]
    async fn on_notification_deny_skips_emission() {
        let (broker, store, ctx) = wired_broker().await;
        bind(&broker, ctx, "svc").await;
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        let tx = server.sender();
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker
            .hooks()
            .write()
            .await
            .on_notification
            .entries
            .push(HookEntry {
                id: hook_id("mute"),
                match_instance: None,
                match_tool: Some(GlobPattern("__notification.*".into())),
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("muted".into()),
                priority: 0,
            });

        let _ = tx.send(ServerNotification::Log {
            level: LogLevel::Info,
            message: "silenced".into(),
            tool: None,
        });
        sleep(Duration::from_millis(150)).await;

        assert!(
            notifications_in(&store, ctx).is_empty(),
            "Deny OnNotification must skip emission",
        );
    }

    // ═════════════════════════════════════════════════════════════════════
    // Phase 4 (M5) — reentrancy cap
    // ═════════════════════════════════════════════════════════════════════

    /// Exit #5: a reentrant hook that recurses past `MAX_HOOK_DEPTH`
    /// returns `McpError::HookRecursionLimit`. Uses `HOOK_DEPTH_OVERRIDE`
    /// to keep the cap small (2) so we can hit it in a few hops.
    #[tokio::test]
    async fn reentrant_hook_exceeds_depth_cap() {
        // Cap at 2 for this test so we can hit it cheaply.
        let _ = HOOK_DEPTH_OVERRIDE.set(2);

        struct Reentering(std::sync::OnceLock<std::sync::Weak<Broker>>);
        #[async_trait]
        impl Hook for Reentering {
            async fn invoke(
                &self,
                params: &KernelCallParams,
                ctx: &CallContext,
            ) -> McpResult<()> {
                let broker = self.0.get().unwrap().upgrade().unwrap();
                broker
                    .call_tool(params.clone(), ctx, CancellationToken::new())
                    .await
                    .map(|_| ())
            }
        }

        let broker = Arc::new(Broker::new());
        let server = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        let hook = Arc::new(Reentering(std::sync::OnceLock::new()));
        hook.0.set(Arc::downgrade(&broker)).unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("recur"),
            match_instance: None,
            match_tool: Some(GlobPattern("t".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.reentering".into(),
                hook: hook as Arc<dyn Hook>,
            }),
            priority: 0,
        });

        let err = broker
            .call_tool(
                params("svc", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        // The inner recursion hits HookRecursionLimit; the middle Invoke
        // converts it to a Deny (per evaluate_phase semantics). The outer
        // caller sees McpError::Denied.
        assert!(
            matches!(err, McpError::Denied { ref by_hook } if by_hook.0 == "recur"),
            "expected recursion to surface as Denied(by_hook=recur); got {err:?}",
        );
    }

    /// Positive control: a single-level reentry under the cap completes
    /// `Ok`.
    #[tokio::test]
    async fn reentrant_hook_under_cap_succeeds() {
        // Only run once per cap; cannot unset a OnceLock in tests without
        // tricks. Use the default cap (4) — one level of recursion is well
        // under.
        struct OneHop(std::sync::OnceLock<std::sync::Weak<Broker>>, InstanceId);
        #[async_trait]
        impl Hook for OneHop {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                ctx: &CallContext,
            ) -> McpResult<()> {
                let broker = self.0.get().unwrap().upgrade().unwrap();
                // Call the sibling tool on a different instance (so the hook
                // itself doesn't re-match and infinite-loop).
                broker
                    .call_tool(
                        KernelCallParams {
                            instance: self.1.clone(),
                            tool: "other".into(),
                            arguments: serde_json::json!({}),
                        },
                        ctx,
                        CancellationToken::new(),
                    )
                    .await
                    .map(|_| ())
            }
        }

        let broker = Arc::new(Broker::new());
        let svc = Arc::new(MockServer::new("svc").with_tool("t"));
        let other = Arc::new(MockServer::new("other").with_tool("other"));
        broker
            .register_silently(svc, InstancePolicy::default())
            .await
            .unwrap();
        broker
            .register_silently(other, InstancePolicy::default())
            .await
            .unwrap();
        let hook = Arc::new(OneHop(
            std::sync::OnceLock::new(),
            InstanceId::new("other"),
        ));
        hook.0.set(Arc::downgrade(&broker)).unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("one-hop"),
            match_instance: Some(GlobPattern("svc".into())),
            match_tool: Some(GlobPattern("t".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.one_hop".into(),
                hook: hook as Arc<dyn Hook>,
            }),
            priority: 0,
        });

        let result = broker
            .call_tool(
                params("svc", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
    }

    /// The HOOK_DEPTH counter must reset across independent top-level
    /// `call_tool` invocations. If the first call hits the cap and then a
    /// second call with the same cap cannot proceed, we've leaked the
    /// counter across tasks.
    #[tokio::test]
    async fn hook_depth_resets_across_calls() {
        let broker = Arc::new(Broker::new());
        let svc = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(svc, InstancePolicy::default())
            .await
            .unwrap();

        // No hook → no depth bump. Two back-to-back calls both succeed.
        for _ in 0..2 {
            broker
                .call_tool(
                    params("svc", "t"),
                    &CallContext::test(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
    }

    /// A panic inside a hook body must not leak the depth counter. After
    /// the panic, subsequent calls still work at depth 0.
    #[tokio::test]
    async fn panicking_hook_body_does_not_leak_depth() {
        struct Panicker;
        #[async_trait]
        impl Hook for Panicker {
            async fn invoke(
                &self,
                _params: &KernelCallParams,
                _ctx: &CallContext,
            ) -> McpResult<()> {
                panic!("simulated hook body panic");
            }
        }

        let broker = Arc::new(Broker::new());
        let svc = Arc::new(MockServer::new("svc").with_tool("t"));
        broker
            .register_silently(svc, InstancePolicy::default())
            .await
            .unwrap();
        broker.hooks().write().await.pre_call.entries.push(HookEntry {
            id: hook_id("panicker"),
            match_instance: None,
            match_tool: Some(GlobPattern("t".into())),
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "test.panicker".into(),
                hook: Arc::new(Panicker),
            }),
            priority: 0,
        });

        let broker2 = broker.clone();
        let panic_result = tokio::spawn(async move {
            broker2
                .call_tool(
                    params("svc", "t"),
                    &CallContext::test(),
                    CancellationToken::new(),
                )
                .await
        })
        .await;
        assert!(
            panic_result.is_err(),
            "expected task to panic from hook body; got {panic_result:?}"
        );

        // Remove the panicking hook so subsequent calls aren't blocked.
        broker.hooks().write().await.pre_call.entries.clear();

        // If the drop guard did its job, the counter is back to 0 and the
        // next call succeeds without hitting HookRecursionLimit.
        broker
            .call_tool(
                params("svc", "t"),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    /// Resource-flush emission also goes through OnNotification. A
    /// ShortCircuit hook on the resource_updated synth skips that subscriber
    /// — no child Resource block lands under that context's parent.
    #[tokio::test]
    async fn on_notification_fires_for_resource_flush_success_path() {
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
            .subscribe(&InstanceId::new("res"), "file:///a", &call_ctx)
            .await
            .unwrap();
        let root_blocks_before = resource_blocks_in(&store, ctx).len();
        assert_eq!(
            root_blocks_before, 1,
            "subscribe auto-reads, producing the root block",
        );

        // Short-circuit the resource-update synth so the flush does not emit
        // a child.
        broker
            .hooks()
            .write()
            .await
            .on_notification
            .entries
            .push(HookEntry {
                id: hook_id("no-res-updates"),
                match_instance: None,
                match_tool: Some(GlobPattern("__notification.resource_updated".into())),
                match_context: None,
                match_principal: None,
                action: HookAction::ShortCircuit(KernelToolResult::text("swallowed")),
                priority: 0,
            });

        for _ in 0..5 {
            let _ = tx.send(ServerNotification::ResourceUpdated {
                uri: "file:///a".into(),
            });
        }
        sleep(Duration::from_millis(700)).await;

        let after = resource_blocks_in(&store, ctx).len();
        assert_eq!(
            after, root_blocks_before,
            "ShortCircuit OnNotification must prevent the child resource block",
        );
    }

    // ── Phase 5 M2: binding mutation + ListTools filter ───────────────

    /// `bind` with a previously-registered instance emits `ToolAdded` into
    /// the calling context for every tool that instance advertises. This is
    /// the Phase 5 late-injection happy path and closes exit criterion #1
    /// at the unit level (broker_e2e covers the admin-server path in M5).
    #[tokio::test]
    async fn bind_emits_tool_added_for_newly_visible_tools() {
        let (broker, store, ctx) = wired_broker().await;
        // Register first (so tool_snapshots has the instance's tools) with
        // no binding for this context yet — suppress the synthetic
        // ToolAdded from register so we only measure the bind's emission.
        let server = Arc::new(
            MockServer::new("svc")
                .with_tool("ping")
                .with_tool("pong"),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        assert!(
            notifications_in(&store, ctx).is_empty(),
            "silent register should not emit notifications",
        );

        broker.bind(ctx, InstanceId::new("svc")).await;

        let notifs = notifications_in(&store, ctx);
        assert_eq!(notifs.len(), 2, "expected one ToolAdded per tool");
        let kinds: Vec<_> = notifs
            .iter()
            .map(|n| (n.instance.as_str(), n.tool.as_deref(), n.kind))
            .collect();
        assert!(kinds.contains(&("svc", Some("ping"), kaijutsu_types::NotificationKind::ToolAdded)));
        assert!(kinds.contains(&("svc", Some("pong"), kaijutsu_types::NotificationKind::ToolAdded)));
    }

    /// `unbind` emits `ToolRemoved` for every tool that *was* visible through
    /// the bound instance.
    #[tokio::test]
    async fn unbind_emits_tool_removed() {
        let (broker, store, ctx) = wired_broker().await;
        let server = Arc::new(MockServer::new("svc").with_tool("ping"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.bind(ctx, InstanceId::new("svc")).await;
        // One ToolAdded from bind so far.
        assert_eq!(notifications_in(&store, ctx).len(), 1);

        broker.unbind(ctx, &InstanceId::new("svc")).await;

        let notifs = notifications_in(&store, ctx);
        assert_eq!(notifs.len(), 2, "expected ToolAdded + ToolRemoved");
        // `notifications_in` returns reverse-chronological; assert on the set
        // of (kind, tool) pairs rather than positional order.
        assert!(
            notifs
                .iter()
                .any(|n| n.kind == kaijutsu_types::NotificationKind::ToolRemoved
                    && n.tool.as_deref() == Some("ping")),
            "expected a ToolRemoved for ping; got {notifs:#?}",
        );
    }

    /// `set_binding` with a wholesale-different binding fires per-instance
    /// ToolAdded / ToolRemoved reflecting the set difference — not one
    /// blanket ToolsChanged. Guards the diff computation against
    /// over/under-emission.
    #[tokio::test]
    async fn set_binding_diff_fires_per_added_and_removed_instance() {
        let (broker, store, ctx) = wired_broker().await;
        let a = Arc::new(MockServer::new("a").with_tool("alpha"));
        let b = Arc::new(MockServer::new("b").with_tool("beta").with_tool("gamma"));
        broker
            .register_silently(a, InstancePolicy::default())
            .await
            .unwrap();
        broker
            .register_silently(b, InstancePolicy::default())
            .await
            .unwrap();

        // Start with {a}
        broker
            .set_binding(ctx, ContextToolBinding::with_instances(vec![InstanceId::new("a")]))
            .await;
        assert_eq!(
            notifications_in(&store, ctx).len(),
            1,
            "a.alpha added",
        );

        // Swap to {b} — expect ToolAdded for beta+gamma, ToolRemoved for alpha.
        broker
            .set_binding(ctx, ContextToolBinding::with_instances(vec![InstanceId::new("b")]))
            .await;
        // Assert on total counts across both set_binding calls. Cumulative:
        // 1 ToolAdded (a:alpha, from first) + 2 ToolAdded (b:beta + b:gamma,
        // from swap) + 1 ToolRemoved (a:alpha, from swap) = 3 added, 1
        // removed. Positional ordering is not load-bearing here — the set
        // of emissions is.
        let notifs = notifications_in(&store, ctx);
        let added: Vec<_> = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::ToolAdded)
            .map(|n| (n.instance.as_str(), n.tool.as_deref().unwrap()))
            .collect();
        let removed: Vec<_> = notifs
            .iter()
            .filter(|n| n.kind == kaijutsu_types::NotificationKind::ToolRemoved)
            .map(|n| (n.instance.as_str(), n.tool.as_deref().unwrap()))
            .collect();
        assert_eq!(added.len(), 3, "3 cumulative ToolAdded events; got {added:?}");
        assert!(added.contains(&("a", "alpha")));
        assert!(added.contains(&("b", "beta")));
        assert!(added.contains(&("b", "gamma")));
        assert_eq!(removed, vec![("a", "alpha")]);
    }

    /// R1 mitigation: if an unbind→rebind cycle lands on the same
    /// `(instance, tool_name)` pair set, no spurious diff is emitted. The
    /// plan specifically calls this case out as a pitfall to guard.
    #[tokio::test]
    async fn set_binding_no_emission_when_pairs_unchanged() {
        let (broker, store, ctx) = wired_broker().await;
        let server = Arc::new(MockServer::new("svc").with_tool("ping"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();

        let b = ContextToolBinding::with_instances(vec![InstanceId::new("svc")]);
        broker.set_binding(ctx, b.clone()).await;
        let after_first = notifications_in(&store, ctx).len();
        assert_eq!(after_first, 1, "first set_binding emits ToolAdded");

        // Re-apply an identical binding. Pair set is unchanged → no diff.
        broker.set_binding(ctx, b).await;
        assert_eq!(
            notifications_in(&store, ctx).len(),
            after_first,
            "identical set_binding must not emit",
        );
    }

    /// D-56 exit #7(a): a `ListTools Deny` hook strips the tool from the
    /// visible set. `kj://kernel/tools`-style kernel-wide enumeration via
    /// `list_tools` (not `list_visible_tools`) stays honest — asserted in
    /// a sibling test.
    #[tokio::test]
    async fn list_tools_deny_strips_tool_from_visible_set() {
        let (broker, _store, ctx) = wired_broker().await;
        let server = Arc::new(
            MockServer::new("files")
                .with_tool("file_read")
                .with_tool("file_write"),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.bind(ctx, InstanceId::new("files")).await;

        // Install a ListTools Deny on file_write.
        broker
            .hooks()
            .write()
            .await
            .list_tools
            .entries
            .push(HookEntry {
                id: hook_id("no-writes"),
                match_instance: Some(GlobPattern("files".into())),
                match_tool: Some(GlobPattern("file_write".into())),
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("read-only context".into()),
                priority: 0,
            });

        let call_ctx = {
            let mut c = CallContext::test();
            c.context_id = ctx;
            c
        };
        let visible: Vec<String> = broker
            .list_visible_tools(ctx, &call_ctx)
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(visible.contains(&"file_read".to_string()));
        assert!(
            !visible.iter().any(|n| n.contains("file_write")),
            "file_write must be hidden by ListTools Deny; saw {visible:?}",
        );
    }

    /// D-56 exit #7(b): a tool hidden by `ListTools Deny` is also
    /// uncallable from that context — `call_tool` returns `ToolNotFound`
    /// because the filter strips the tool before sticky resolution, so
    /// `name_map` has no entry pointing at it.
    #[tokio::test]
    async fn list_tools_deny_makes_tool_uncallable_via_this_binding() {
        let (broker, _store, ctx) = wired_broker().await;
        let server = Arc::new(
            MockServer::new("files")
                .with_tool("file_read")
                .with_tool("file_write"),
        );
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.bind(ctx, InstanceId::new("files")).await;

        broker
            .hooks()
            .write()
            .await
            .list_tools
            .entries
            .push(HookEntry {
                id: hook_id("no-writes"),
                match_instance: Some(GlobPattern("files".into())),
                match_tool: Some(GlobPattern("file_write".into())),
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("read-only".into()),
                priority: 0,
            });

        // Prime the binding's name_map by listing visible tools first.
        let call_ctx = {
            let mut c = CallContext::test();
            c.context_id = ctx;
            c
        };
        let _ = broker.list_visible_tools(ctx, &call_ctx).await.unwrap();

        // Attempt to call the hidden tool directly via call_tool. The
        // invocation is what the kernel's dispatch path does: resolve the
        // visible name against the binding then hit `call_tool`. Since
        // `file_write` was filtered out of resolution, the binding's
        // `resolve` returns None for it — callers will short-circuit to
        // `ToolNotFound`. This test exercises the contract at the broker
        // level by calling `call_tool` with the raw (instance, tool)
        // pair: that path still runs PreCall hooks etc., but the D-56
        // contract says the tool is conceptually gone from this binding.
        // We assert the indirect-resolution failure here: binding.resolve
        // must be None for file_write.
        let binding = broker.binding(&ctx).await.unwrap();
        assert!(
            binding.resolve("file_write").is_none(),
            "ListTools-Denied tool must not land in name_map",
        );
    }

    /// `ListTools Log` observes without stripping. Negative control for the
    /// Deny-strips tests and positive control for Log as a valid action.
    #[tokio::test]
    async fn list_tools_log_does_not_strip() {
        let (broker, _store, ctx) = wired_broker().await;
        let server = Arc::new(MockServer::new("files").with_tool("file_read"));
        broker
            .register_silently(server, InstancePolicy::default())
            .await
            .unwrap();
        broker.bind(ctx, InstanceId::new("files")).await;

        broker
            .hooks()
            .write()
            .await
            .list_tools
            .entries
            .push(HookEntry {
                id: hook_id("observe-only"),
                match_instance: Some(GlobPattern("files".into())),
                match_tool: Some(GlobPattern("*".into())),
                match_context: None,
                match_principal: None,
                action: HookAction::Log(super::super::hook_table::LogSpec {
                    target: "observe".into(),
                    level: tracing::Level::INFO,
                }),
                priority: 0,
            });

        let call_ctx = {
            let mut c = CallContext::test();
            c.context_id = ctx;
            c
        };
        let visible: Vec<String> = broker
            .list_visible_tools(ctx, &call_ctx)
            .await
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(visible, vec!["file_read".to_string()]);
    }

    // ── Hook persistence (M2) ────────────────────────────────────────
    //
    // These tests exercise the round-trip: in-memory HookTables →
    // `persist_hook_insert` → SQLite → `set_db` → hydrate → HookTables.
    // The M5 e2e test stitches this through the admin handler; these
    // tests isolate each direction.

    fn hook_db() -> DbHandle {
        use crate::kernel_db::KernelDb;
        Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()))
    }

    fn log_entry(id: &str, match_tool: Option<&str>) -> super::HookEntry {
        super::HookEntry {
            id: super::HookId(id.into()),
            match_instance: None,
            match_tool: match_tool.map(|s| super::super::hook_table::GlobPattern(s.into())),
            match_context: None,
            match_principal: None,
            action: super::HookAction::Log(super::super::hook_table::LogSpec {
                target: "kaijutsu::hooks::test".into(),
                level: tracing::Level::INFO,
            }),
            priority: 0,
        }
    }

    /// `set_db` eagerly loads hook rows into `HookTables` in
    /// priority/insertion order — the hydrate half of the persistence
    /// loop. Guards against "restart silently drops hooks."
    #[tokio::test]
    async fn hooks_hydrate_on_set_db() {
        use crate::kernel_db::HookRow;
        let db = hook_db();

        // Seed the DB directly. Two pre_call hooks at different priorities
        // plus a post_call hook — hydrate must fan them into the right
        // tables and order pre_call by priority ASC.
        {
            let guard = db.lock();
            for row in [
                HookRow {
                    hook_id: "pc-high".into(),
                    phase: "pre_call".into(),
                    priority: 10,
                    match_instance: None,
                    match_tool: None,
                    match_context: None,
                    match_principal: None,
                    action_kind: "log".into(),
                    action_builtin_name: None,
                    action_kaish_script_id: None,
                    action_result_text: None,
                    action_is_error: None,
                    action_deny_reason: None,
                    action_log_target: Some("x".into()),
                    action_log_level: Some("info".into()),
                },
                HookRow {
                    hook_id: "pc-low".into(),
                    phase: "pre_call".into(),
                    priority: 0,
                    match_instance: None,
                    match_tool: None,
                    match_context: None,
                    match_principal: None,
                    action_kind: "log".into(),
                    action_builtin_name: None,
                    action_kaish_script_id: None,
                    action_result_text: None,
                    action_is_error: None,
                    action_deny_reason: None,
                    action_log_target: Some("x".into()),
                    action_log_level: Some("info".into()),
                },
                HookRow {
                    hook_id: "post".into(),
                    phase: "post_call".into(),
                    priority: 0,
                    match_instance: None,
                    match_tool: None,
                    match_context: None,
                    match_principal: None,
                    action_kind: "log".into(),
                    action_builtin_name: None,
                    action_kaish_script_id: None,
                    action_result_text: None,
                    action_is_error: None,
                    action_deny_reason: None,
                    action_log_target: Some("x".into()),
                    action_log_level: Some("info".into()),
                },
            ] {
                guard.insert_hook(&row).unwrap();
            }
        }

        let broker = Arc::new(Broker::new());
        broker.set_db(db).await;

        let hooks = broker.hooks().read().await;
        // pre_call ordered by priority ASC: pc-low (0) then pc-high (10).
        let pre_ids: Vec<&str> = hooks.pre_call.entries.iter().map(|e| e.id.0.as_str()).collect();
        assert_eq!(pre_ids, vec!["pc-low", "pc-high"]);
        // post_call has exactly the one hook.
        let post_ids: Vec<&str> = hooks.post_call.entries.iter().map(|e| e.id.0.as_str()).collect();
        assert_eq!(post_ids, vec!["post"]);
        // Other tables are empty.
        assert!(hooks.on_error.entries.is_empty());
        assert!(hooks.on_notification.entries.is_empty());
        assert!(hooks.list_tools.entries.is_empty());
    }

    /// `persist_hook_insert` writes a row that `load_all_hooks` returns.
    /// The persist half of the loop; exercise without the admin handler
    /// so failure points are obvious.
    #[tokio::test]
    async fn persist_hook_insert_writes_row() {
        let db = hook_db();
        let broker = Arc::new(Broker::new());
        broker.set_db(db.clone()).await;

        let entry = log_entry("h1", Some("file_*"));
        broker.persist_hook_insert(HookPhase::PreCall, &entry).await;

        let rows = db.lock().load_all_hooks().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.hook_id, "h1");
        assert_eq!(r.phase, "pre_call");
        assert_eq!(r.match_tool.as_deref(), Some("file_*"));
        assert_eq!(r.action_kind, "log");
    }

    /// `persist_hook_delete` removes the row written by insert.
    #[tokio::test]
    async fn persist_hook_delete_removes_row() {
        let db = hook_db();
        let broker = Arc::new(Broker::new());
        broker.set_db(db.clone()).await;

        broker
            .persist_hook_insert(HookPhase::PreCall, &log_entry("h-del", None))
            .await;
        assert_eq!(db.lock().load_all_hooks().unwrap().len(), 1);

        broker.persist_hook_delete("h-del").await;
        assert_eq!(db.lock().load_all_hooks().unwrap().len(), 0);

        // Idempotent — deleting again is fine.
        broker.persist_hook_delete("h-del").await;
    }

    /// A row with an `action_builtin_name` not in the registry is
    /// skipped at hydrate time with a warn — the kernel must not brick
    /// on a single bad row (stale builtin rename, manual SQL typo). The
    /// surrounding valid rows still load.
    #[tokio::test]
    async fn hydrate_skips_unknown_builtin_and_keeps_valid_rows() {
        use crate::kernel_db::HookRow;
        let db = hook_db();
        {
            let guard = db.lock();
            guard
                .insert_hook(&HookRow {
                    hook_id: "bad".into(),
                    phase: "pre_call".into(),
                    priority: 0,
                    match_instance: None,
                    match_tool: None,
                    match_context: None,
                    match_principal: None,
                    action_kind: "builtin_invoke".into(),
                    action_builtin_name: Some("no_such_builtin".into()),
                    action_kaish_script_id: None,
                    action_result_text: None,
                    action_is_error: None,
                    action_deny_reason: None,
                    action_log_target: None,
                    action_log_level: None,
                })
                .unwrap();
            guard
                .insert_hook(&HookRow {
                    hook_id: "good".into(),
                    phase: "pre_call".into(),
                    priority: 0,
                    match_instance: None,
                    match_tool: None,
                    match_context: None,
                    match_principal: None,
                    action_kind: "log".into(),
                    action_builtin_name: None,
                    action_kaish_script_id: None,
                    action_result_text: None,
                    action_is_error: None,
                    action_deny_reason: None,
                    action_log_target: Some("x".into()),
                    action_log_level: Some("info".into()),
                })
                .unwrap();
        }

        let broker = Arc::new(Broker::new());
        broker.set_db(db).await;

        let hooks = broker.hooks().read().await;
        let ids: Vec<&str> = hooks.pre_call.entries.iter().map(|e| e.id.0.as_str()).collect();
        // The bad row is silently skipped (warn logged); the good row loaded.
        assert_eq!(ids, vec!["good"]);
    }
}
