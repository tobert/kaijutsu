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

/// The live client-declared servers, keyed by context and server name.
#[derive(Default)]
pub struct ContextMcpServers {
    // Held across connect and broker calls so declarations for any context
    // apply one at a time; a declaration is a request-boundary event.
    entries: tokio::sync::Mutex<HashMap<(ContextId, String), Entry>>,
}

impl ContextMcpServers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The running servers for `context`, as `(name, instance id)`, sorted
    /// by name.
    pub async fn list(&self, context: ContextId) -> Vec<(String, InstanceId)> {
        let entries = self.entries.lock().await;
        let mut out: Vec<(String, InstanceId)> = entries
            .keys()
            .filter(|(ctx, _)| *ctx == context)
            .map(|(ctx, name)| (name.clone(), context_instance_id(*ctx, name)))
            .collect();
        out.sort();
        out
    }

    /// Replace `owner`'s declared servers for `context` with `specs`, and
    /// return the instance ids of `specs` in order.
    ///
    /// Every new or changed server is started before anything else changes.
    /// If one fails, the ones started for this call are stopped and the
    /// previous state stands.
    pub async fn declare(
        &self,
        kernel: &Arc<Kernel>,
        context: ContextId,
        owner: SessionId,
        specs: Vec<ContextMcpServerSpec>,
    ) -> Result<Vec<InstanceId>, ContextMcpError> {
        validate(kernel, &specs).await?;
        let broker = kernel.broker();
        let mut entries = self.entries.lock().await;

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
                        use super::server_like::McpServerLike;
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
            release(&mut entries, kernel, context, &name, owner).await;
        }

        // Register what started, replacing a changed server in place.
        for (spec, server) in started {
            let instance = context_instance_id(context, &spec.name);
            let key = (context, spec.name.clone());
            if entries.contains_key(&key) {
                if let Err(e) = broker.unregister(&instance).await {
                    tracing::error!(%instance, error = %e, "replacing a context MCP server: unregister failed");
                }
            }
            broker
                .register_silently(Arc::new(server), InstancePolicy::for_kernel(kernel))
                .await
                .map_err(|e| ContextMcpError::Broker { name: spec.name.clone(), reason: e.to_string() })?;
            tracing::info!(context = %context.short(), %instance, command = %spec.command, "context MCP server started");
            let owners = entries.remove(&key).map(|e| e.owners).unwrap_or_default();
            entries.insert(key, Entry { spec, owners });
        }

        // Restore a grant a loadout rewrite removed while the server ran.
        let binding = broker.binding(&context).await.unwrap_or_default();
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
        self.entries
            .lock()
            .await
            .insert((context, name.to_string()), Entry { spec, owners: HashSet::from([owner]) });
    }

    /// Every running server, as `(context, name, instance id)`, sorted.
    pub async fn list_all(&self) -> Vec<(ContextId, String, InstanceId)> {
        let entries = self.entries.lock().await;
        let mut out: Vec<(ContextId, String, InstanceId)> = entries
            .keys()
            .map(|(ctx, name)| (*ctx, name.clone(), context_instance_id(*ctx, name)))
            .collect();
        out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        out
    }

    /// Withdraw `owner` from every server it declared, on every context.
    /// Called when the owner's connection closes.
    pub async fn release_owner(&self, kernel: &Arc<Kernel>, owner: SessionId) {
        let mut entries = self.entries.lock().await;
        let held: Vec<(ContextId, String)> = entries
            .iter()
            .filter(|(_, e)| e.owners.contains(&owner))
            .map(|(key, _)| key.clone())
            .collect();
        for (context, name) in held {
            release(&mut entries, kernel, context, &name, owner).await;
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
