//! `Broker` — the one tool-call pipeline (§4.2, D-02).
//!
//! Phase 1 responsibilities:
//! - Registry of `InstanceId -> Arc<dyn McpServerLike>`.
//! - Per-context `ContextToolBinding` with sticky `Auto` name resolution
//!   (D-20).
//! - `call_tool` pipeline: binding lookup → instance lookup → policy wrap
//!   (timeout / concurrency permit / result-size cap) → server invocation.
//! - Tracing spans around `broker.call_tool` and `server.call_tool` (D-23).
//!
//! Out of scope for Phase 1: hook evaluation (tables exist but empty),
//! notification emission (coalescer injected but no subscribers), resource
//! and prompt aggregation (servers support unsupported-default).

use std::collections::HashMap;
use std::sync::Arc;

use kaijutsu_types::ContextId;
use tokio::sync::{RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

use super::binding::{ContextToolBinding, ResolvedName};
use super::coalescer::NotificationCoalescer;
use super::context::CallContext;
use super::error::{McpError, McpResult, PolicyError};
use super::hook_table::HookTables;
use super::policy::InstancePolicy;
use super::server_like::McpServerLike;
use super::types::{InstanceId, KernelCallParams, KernelNotification, KernelTool, KernelToolResult};

/// Default notification channel capacity. Phase 1 nobody subscribes.
const NOTIF_CAPACITY: usize = 256;

pub struct Broker {
    instances: RwLock<HashMap<InstanceId, Arc<dyn McpServerLike>>>,
    bindings: RwLock<HashMap<ContextId, ContextToolBinding>>,
    policies: RwLock<HashMap<InstanceId, InstancePolicy>>,
    semaphores: RwLock<HashMap<InstanceId, Arc<Semaphore>>>,
    hooks: RwLock<HookTables>,
    coalescer: Arc<NotificationCoalescer>,
    notif_tx: broadcast::Sender<KernelNotification>,
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
        }
    }

    pub fn coalescer(&self) -> &Arc<NotificationCoalescer> {
        &self.coalescer
    }

    pub fn notifications(&self) -> broadcast::Receiver<KernelNotification> {
        self.notif_tx.subscribe()
    }

    /// Register a server instance under `id`. Replaces any existing instance
    /// with the same id (no implicit merge).
    pub async fn register(
        &self,
        server: Arc<dyn McpServerLike>,
        policy: InstancePolicy,
    ) -> McpResult<()> {
        let id = server.instance_id().clone();
        let permits = policy.max_concurrency;

        self.instances.write().await.insert(id.clone(), server);
        self.policies.write().await.insert(id.clone(), policy);
        self.semaphores
            .write()
            .await
            .insert(id, Arc::new(Semaphore::new(permits)));
        Ok(())
    }

    pub async fn unregister(&self, id: &InstanceId) -> McpResult<()> {
        self.instances.write().await.remove(id);
        self.policies.write().await.remove(id);
        self.semaphores.write().await.remove(id);
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
