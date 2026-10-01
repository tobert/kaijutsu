//! MCP servers a connected client declares for one context.
//!
//! ```text
//! declare(ctx, owner, [fixture])   start `fixture`, grant it to ctx
//! declare(ctx, owner, [fixture])   same spec: keep the running process
//! declare(ctx, owner, [])          withdraw: stop it once no owner remains
//! release_owner(owner)             the owner's connection closed
//! ```
//!
//! A declared server registers on the broker as
//! `context.<context id hex>.<name>` and the declaring context gets an
//! explicit instance grant. `*` never covers a context-scoped instance
//! (`binding::is_context_scoped`), so no other context sees its tools, and a
//! fork does not inherit the grant.
//!
//! An owner is the connection that declared the server. A declaration
//! replaces that owner's whole set for the context: a name it no longer
//! lists loses this owner. A server stops when its last owner withdraws it
//! or disconnects. Several owners may declare the same name for the same
//! context; the newest spec wins and the process is restarted only when the
//! spec changed.
//!
//! See `docs/acp.md`, "Client-declared MCP servers".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kaijutsu_types::{ContextId, SessionId};

use super::binding::{CONTEXT_SCOPED_PREFIX, Capability};
use super::policy::InstancePolicy;
use super::server_like::McpServerLike;
use super::servers::external::{ExternalMcpServer, McpServerConfig, McpTransport};
use super::types::InstanceId;
use crate::Kernel;

/// One stdio MCP server, as a client declared it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMcpServerSpec {
    pub name: String,
    /// Executable path, or a bare name found on the kernel's `PATH`.
    pub command: String,
    pub args: Vec<String>,
    /// Added to the kernel's own environment, in order.
    pub env: Vec<(String, String)>,
}

/// Why a declaration was refused. Nothing changed when any of these return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextMcpError {
    /// The declaration itself is wrong: a duplicate, empty, or reserved name.
    #[error("{0}")]
    Invalid(String),
    /// A server could not be started or did not complete the MCP handshake.
    #[error("MCP server '{name}' failed to start: {reason}")]
    StartFailed { name: String, reason: String },
    /// The broker refused a registration or grant.
    #[error("MCP server '{name}': {reason}")]
    Broker { name: String, reason: String },
}

impl ContextMcpError {
    /// True for a declaration the client must change, as opposed to a
    /// server that failed at runtime.
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

/// The instance id a server named `name` gets when declared for `context`.
pub fn context_instance_id(context: ContextId, name: &str) -> InstanceId {
    InstanceId::new(format!("{CONTEXT_SCOPED_PREFIX}{}.{name}", context.to_hex()))
}

struct Entry {
    spec: ContextMcpServerSpec,
    owners: HashSet<SessionId>,
}

#[derive(Default)]
struct State {
    entries: HashMap<(ContextId, String), Entry>,
    /// Owners whose connection closed. A declaration still in flight when
    /// its connection closed must not record a server for a gone owner.
    closed: HashSet<SessionId>,
}

/// The runtime every context MCP server runs on. A server's transport tasks
/// live on the runtime that connected it, and a kaijutsu-server connection
/// runs on its own runtime that is dropped when the connection closes, so
/// declarations run here instead.
struct ServerRuntime(Option<tokio::runtime::Runtime>);

impl Drop for ServerRuntime {
    fn drop(&mut self) {
        // A kernel may be dropped inside async code, where a blocking
        // runtime shutdown panics.
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// The live client-declared servers, keyed by context and server name.
#[derive(Default)]
pub struct ContextMcpServers {
    // Held across connect and broker calls so declarations apply one at a
    // time; a declaration is a request-boundary event.
    state: tokio::sync::Mutex<State>,
    runtime: std::sync::OnceLock<ServerRuntime>,
}

impl ContextMcpServers {
    pub fn new() -> Self {
        Self::default()
    }

    fn handle(&self) -> tokio::runtime::Handle {
        let rt = self.runtime.get_or_init(|| {
            ServerRuntime(Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("kj-context-mcp")
                    .enable_all()
                    .build()
                    .expect("build the context MCP server runtime"),
            ))
        });
        rt.0.as_ref().expect("the runtime lives until the kernel drops").handle().clone()
    }

    /// The running servers for `context`, as `(name, instance id)`, sorted
    /// by name.
    pub async fn list(&self, context: ContextId) -> Vec<(String, InstanceId)> {
        let state = self.state.lock().await;
        let mut out: Vec<(String, InstanceId)> = state
            .entries
            .keys()
            .filter(|(ctx, _)| *ctx == context)
            .map(|(ctx, name)| (name.clone(), context_instance_id(*ctx, name)))
            .collect();
        out.sort();
        out
    }

    /// Every running server, as `(context, name, instance id)`, sorted.
    pub async fn list_all(&self) -> Vec<(ContextId, String, InstanceId)> {
        let state = self.state.lock().await;
        let mut out: Vec<(ContextId, String, InstanceId)> = state
            .entries
            .keys()
            .map(|(ctx, name)| (*ctx, name.clone(), context_instance_id(*ctx, name)))
            .collect();
        out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        out
    }

    /// Replace `owner`'s declared servers for `context` with `specs`, and
    /// return the instance ids of `specs` in order.
    ///
    /// Runs on the context MCP runtime and completes even if the caller
    /// stops waiting. Every new or changed server starts before anything
    /// else changes; if one fails to start, the ones started for this call
    /// stop and the previous state stands. A later broker failure leaves
    /// each started server recorded under `owner`, so releasing the owner
    /// stops it.
    pub async fn declare(
        &self,
        kernel: &Arc<Kernel>,
        context: ContextId,
        owner: SessionId,
        specs: Vec<ContextMcpServerSpec>,
    ) -> Result<Vec<InstanceId>, ContextMcpError> {
        let kernel = kernel.clone();
        self.handle()
            .spawn(async move { kernel.context_mcp().declare_here(&kernel, context, owner, specs).await })
            .await
            .unwrap_or_else(|e| {
                Err(ContextMcpError::Broker { name: String::new(), reason: format!("declaration task failed: {e}") })
            })
    }

    async fn declare_here(
        &self,
        kernel: &Arc<Kernel>,
        context: ContextId,
        owner: SessionId,
        specs: Vec<ContextMcpServerSpec>,
    ) -> Result<Vec<InstanceId>, ContextMcpError> {
        validate(kernel, &specs).await?;
        let broker = kernel.broker();
        let mut state = self.state.lock().await;
        if state.closed.contains(&owner) {
            return Err(ContextMcpError::Invalid(
                "the connection declaring these MCP servers has closed".to_string(),
            ));
        }
        let entries = &mut state.entries;

        // Start what is new or changed, before touching any state.
        let mut started: Vec<(ContextMcpServerSpec, ExternalMcpServer)> = Vec::new();
        for spec in &specs {
            let unchanged = entries
                .get(&(context, spec.name.clone()))
                .is_some_and(|e| e.spec == *spec);
            if unchanged {
                continue;
            }
            match connect(kernel, context, spec).await {
                Ok(server) => started.push((spec.clone(), server)),
                Err(reason) => {
                    for (_, server) in started {
                        let _ = server.shutdown().await;
                    }
                    return Err(ContextMcpError::StartFailed { name: spec.name.clone(), reason });
                }
            }
        }

        // Withdraw this owner from names it no longer declares.
        let declared: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let dropped: Vec<String> = entries
            .iter()
            .filter(|((ctx, name), e)| {
                *ctx == context && e.owners.contains(&owner) && !declared.contains(name.as_str())
            })
            .map(|((_, name), _)| name.clone())
            .collect();
        for name in dropped {
            release(entries, kernel, context, &name, owner).await;
        }

        // Record each started server under its owner before registering it,
        // so a failure from here on leaves nothing the owner cannot release.
        // A changed server is replaced in place; `register` tells the
        // context its tools are back.
        for (spec, server) in started {
            let instance = context_instance_id(context, &spec.name);
            let key = (context, spec.name.clone());
            let mut owners = match entries.remove(&key) {
                Some(old) => {
                    if let Err(e) = broker.unregister(&instance).await {
                        tracing::error!(%instance, error = %e, "replacing a context MCP server: unregister failed");
                    }
                    old.owners
                }
                None => HashSet::new(),
            };
            owners.insert(owner);
            let name = spec.name.clone();
            let command = spec.command.clone();
            entries.insert(key, Entry { spec, owners });
            broker
                .register(Arc::new(server), InstancePolicy::for_kernel(kernel))
                .await
                .map_err(|e| ContextMcpError::Broker { name: name.clone(), reason: e.to_string() })?;
            tracing::info!(context = %context.short(), %instance, %command, "context MCP server started");
        }

        // Grant each server, restoring a grant a loadout rewrite removed
        // while it ran.
        let binding = broker
            .binding_checked(&context)
            .await
            .map_err(|e| ContextMcpError::Broker { name: String::new(), reason: e.to_string() })?;
        let mut instances = Vec::with_capacity(specs.len());
        for spec in &specs {
            let entry = entries
                .get_mut(&(context, spec.name.clone()))
                .expect("every declared server is running after the start pass");
            entry.owners.insert(owner);
            let instance = context_instance_id(context, &spec.name);
            if !binding.allows(&Capability::Instance(instance.clone())) {
                broker
                    .bind(context, instance.clone())
                    .await
                    .map_err(|e| ContextMcpError::Broker { name: spec.name.clone(), reason: e.to_string() })?;
            }
            instances.push(instance);
        }
        Ok(instances)
    }

    /// Record a server as declared without starting a process.
    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, context: ContextId, name: &str, owner: SessionId) {
        let spec = ContextMcpServerSpec {
            name: name.to_string(),
            command: "/test/only".to_string(),
            args: Vec::new(),
            env: Vec::new(),
        };
        self.state
            .lock()
            .await
            .entries
            .insert((context, name.to_string()), Entry { spec, owners: HashSet::from([owner]) });
    }

    /// Withdraw `owner` from every server it declared, on every context, and
    /// refuse any later declaration from it. For a connection that closed.
    pub async fn release_owner(&self, kernel: &Arc<Kernel>, owner: SessionId) {
        let kernel = kernel.clone();
        let task = self
            .handle()
            .spawn(async move { kernel.context_mcp().release_owner_here(&kernel, owner).await });
        if let Err(e) = task.await {
            tracing::error!(owner = %owner.short(), error = %e, "releasing context MCP servers failed");
        }
    }

    /// [`Self::release_owner`] without waiting, for a synchronous `Drop`.
    pub fn release_owner_detached(kernel: Arc<Kernel>, owner: SessionId) {
        let handle = kernel.context_mcp().handle();
        handle.spawn(async move { kernel.context_mcp().release_owner_here(&kernel, owner).await });
    }

    async fn release_owner_here(&self, kernel: &Arc<Kernel>, owner: SessionId) {
        let mut state = self.state.lock().await;
        state.closed.insert(owner);
        let held: Vec<(ContextId, String)> = state
            .entries
            .iter()
            .filter(|(_, e)| e.owners.contains(&owner))
            .map(|(key, _)| key.clone())
            .collect();
        for (context, name) in held {
            release(&mut state.entries, kernel, context, &name, owner).await;
        }
    }
}

/// Remove `owner` from one server, stopping it if no owner remains.
async fn release(
    entries: &mut HashMap<(ContextId, String), Entry>,
    kernel: &Arc<Kernel>,
    context: ContextId,
    name: &str,
    owner: SessionId,
) {
    let key = (context, name.to_string());
    let Some(entry) = entries.get_mut(&key) else { return };
    entry.owners.remove(&owner);
    if !entry.owners.is_empty() {
        return;
    }
    entries.remove(&key);
    let instance = context_instance_id(context, name);
    let broker = kernel.broker();
    if let Err(e) = broker.unregister(&instance).await {
        tracing::error!(%instance, error = %e, "context MCP server: unregister failed");
    }
    if let Err(e) = broker.unbind(context, &instance).await {
        tracing::error!(%instance, error = %e, "context MCP server: revoking the grant failed");
    }
    tracing::info!(context = %context.short(), %instance, "context MCP server stopped");
}

/// Refuse a declaration the client must change, before anything starts.
async fn validate(kernel: &Arc<Kernel>, specs: &[ContextMcpServerSpec]) -> Result<(), ContextMcpError> {
    let mut seen = HashSet::new();
    let kernel_wide: HashSet<String> = kernel
        .broker()
        .list_instances()
        .await
        .into_iter()
        .filter_map(|id| id.as_str().strip_prefix("external.").map(str::to_string))
        .collect();
    for spec in specs {
        let name = spec.name.as_str();
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(ContextMcpError::Invalid(format!(
                "MCP server name {name:?} is not usable: it must be non-empty, \
                 without surrounding whitespace or control characters"
            )));
        }
        if !seen.insert(name) {
            return Err(ContextMcpError::Invalid(format!(
                "MCP server name '{name}' is declared twice; each server needs its own name"
            )));
        }
        if kernel_wide.contains(name) {
            return Err(ContextMcpError::Invalid(format!(
                "MCP server name '{name}' is already a kernel-wide server from mcp.toml; \
                 declare it under another name"
            )));
        }
        if spec.command.is_empty() {
            return Err(ContextMcpError::Invalid(format!("MCP server '{name}' has an empty command")));
        }
    }
    Ok(())
}

async fn connect(
    kernel: &Arc<Kernel>,
    context: ContextId,
    spec: &ContextMcpServerSpec,
) -> Result<ExternalMcpServer, String> {
    let config = McpServerConfig {
        name: spec.name.clone(),
        command: spec.command.clone(),
        args: spec.args.clone(),
        env: spec.env.iter().cloned().collect(),
        cwd: None,
        transport: McpTransport::Stdio,
        url: None,
        headers: HashMap::new(),
        call_timeout: None,
    };
    ExternalMcpServer::connect(
        config,
        context_instance_id(context, &spec.name),
        kernel.timeouts().mcp_connect_timeout,
    )
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, command: &str) -> ContextMcpServerSpec {
        ContextMcpServerSpec {
            name: name.to_string(),
            command: command.to_string(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    #[test]
    fn instance_ids_are_context_scoped() {
        let ctx = ContextId::new();
        let id = context_instance_id(ctx, "fixture");
        assert!(super::super::binding::is_context_scoped(&id));
        assert!(id.as_str().ends_with(".fixture"));
        assert!(id.as_str().contains(&ctx.to_hex()));
    }

    #[tokio::test]
    async fn duplicate_names_are_invalid_and_start_nothing() {
        let kernel = Arc::new(Kernel::new_ephemeral("test").await);
        let servers = ContextMcpServers::new();
        let err = servers
            .declare(
                &kernel,
                ContextId::new(),
                SessionId::new(),
                vec![spec("twin", "/definitely/not/here"), spec("twin", "/definitely/not/here")],
            )
            .await
            .unwrap_err();
        assert!(err.is_invalid(), "{err}");
        assert!(err.to_string().contains("twin"), "{err}");
    }

    #[tokio::test]
    async fn an_unstartable_server_names_itself_and_registers_nothing() {
        let kernel = Arc::new(Kernel::new_ephemeral("test").await);
        let servers = ContextMcpServers::new();
        let ctx = ContextId::new();
        let err = servers
            .declare(&kernel, ctx, SessionId::new(), vec![spec("ghost", "/definitely/not/here")])
            .await
            .unwrap_err();
        assert!(!err.is_invalid(), "{err}");
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(servers.list(ctx).await.is_empty());
        assert!(
            kernel.broker().list_instances().await.iter().all(|i| !super::super::binding::is_context_scoped(i))
        );
    }
}
