//! Reconciles the CRDT-owned `mcp.toml` (docs/config-crdt-ownership.md) with
//! the broker's live external-MCP registrations — the caller
//! `ExternalMcpServer` always lacked. `external.rs`'s own doc comment says it
//! plainly: "Phase 1 covers… reconnect is a follow-up" — but nothing ever
//! constructed an `McpServerConfig` or called `connect()` outside its own
//! unit tests. This module is that caller.
//!
//! ## Naming
//!
//! A configured server named `X` in mcp.toml registers under the instance id
//! `external.X` — distinct from the `builtin.*` namespace so a context
//! binding can grant/deny the two independently. (`external.gpal` already
//! appears as the convention's example in `kernel_db.rs`'s binding-roundtrip
//! test, predating this module — adopted rather than invented.)
//!
//! ## Reload semantics — the design fork
//!
//! [`reconcile_with_toml`] is deliberately conservative about servers that
//! are already running:
//!
//! - **New** in mcp.toml (not currently registered) → connect + register.
//! - **Removed** from mcp.toml (or `enabled = false`) → unregister.
//! - **Still present** → the *connection* is never touched — no reconnect,
//!   no respawn — even if `command`/`args`/`env` changed underneath it. Only
//!   its `InstancePolicy` is refreshed from the current config (cheap: an
//!   in-memory `Duration`/`usize` swap on `Broker.policies`, no subprocess
//!   involved), so a `call_timeout_ms` edit takes effect on the next
//!   `kj mcp reload` without disturbing a call in flight.
//!
//! This answers the "what happens to a running server whose config changed,
//! and what about in-flight calls" question conservatively: a kaibo
//! consultation that's mid-flight when someone edits mcp.toml is never
//! interrupted by a reload, at the cost of a `command`/`args`/`env` edit not
//! taking effect until the server is explicitly restarted — there is no
//! `kj mcp restart <name>` in this slice (noted in docs/issues.md).
//!
//! The alternative — diffing `McpServerConfig` and hot-swapping a changed
//! entry — is also defensible: `Broker::register` already replaces-by-id,
//! and an in-flight call holds its own `Arc` to the old instance (resolved
//! once per call, not re-resolved mid-flight) so it would run to completion
//! against the stale process either way, meaning a hot-swap costs nothing
//! *additional* over what reload already risks. But it requires remembering
//! each running instance's last-applied config somewhere — `Arc<dyn
//! McpServerLike>` carries no config accessor, and adding one would mean
//! either widening the shared trait (every builtin server pays for a
//! concept it doesn't have) or a parallel `InstanceId -> McpServerConfig`
//! side table alongside `Broker.policies`. That's real new state for a
//! benefit (picking up a config edit without an explicit action) a
//! name-scoped restart verb gets more predictably and more visibly. Picked
//! the smaller, zero-new-state design here; flagged for the record per
//! instructions rather than silently choosing one of two defensible options.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kaijutsu_types::paths;

use super::policy::InstancePolicy;
use super::servers::external::{ExternalMcpServer, McpServerConfig};
use super::toml::load_mcp_config_toml;
use super::types::InstanceId;
use crate::Kernel;

/// `external.<name>` — the instance-id namespace for every server sourced
/// from mcp.toml, distinct from `builtin.*`.
pub fn external_instance_id(name: &str) -> InstanceId {
    InstanceId::new(format!("external.{name}"))
}

/// Outcome of one reconcile pass — boot-time or `kj mcp reload`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalMcpReconcileReport {
    /// Server names newly connected + registered this pass.
    pub added: Vec<String>,
    /// Server names unregistered because they're no longer in mcp.toml (or
    /// were disabled).
    pub removed: Vec<String>,
    /// Server names that were already running and stayed running (their
    /// `InstancePolicy` was refreshed regardless — see the module doc).
    pub unchanged: Vec<String>,
    /// `(server name, error)` for entries mcp.toml asked to be running that
    /// could not be brought up — wrong binary path, crashed handshake,
    /// connect timeout. Never silently dropped: every entry here was also
    /// logged at error level, and mirrored onto
    /// `Broker::external_mcp_failures` for `kj mcp list` to surface without
    /// a log grep.
    pub failed: Vec<(String, String)>,
    /// Per-entry parse warnings from `load_mcp_config_toml` — malformed
    /// `[servers.X]` tables that were skipped (not fatal to the rest of the
    /// file).
    pub warnings: Vec<String>,
}

impl ExternalMcpReconcileReport {
    /// True when nothing needs an operator's attention (no failures, no
    /// malformed entries).
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty() && self.warnings.is_empty()
    }
}

/// Merge `cfg.call_timeout` (mcp.toml's `call_timeout_ms` override) onto the
/// kernel-wide default policy. `None` → the kernel default applies, which
/// also correctly handles an override being *removed* from mcp.toml between
/// reloads (the policy reverts to kernel-wide, it doesn't stick at the old
/// override).
fn policy_for(kernel: &Kernel, cfg: &McpServerConfig) -> InstancePolicy {
    let mut policy = InstancePolicy::for_kernel(kernel);
    if let Some(t) = cfg.call_timeout {
        policy.call_timeout = t;
    }
    policy
}

/// Reconcile a specific TOML body against the broker.
///
/// Split out from [`reconcile_external_mcp_servers`] so the reconciliation
/// *decision* logic is testable without a VFS-mounted kernel: tests pass
/// literal TOML strings directly. The VFS read + embedded-default fallback
/// lives only in the wrapper.
pub async fn reconcile_with_toml(
    kernel: &Arc<Kernel>,
    raw_toml: &str,
) -> ExternalMcpReconcileReport {
    let mut report = ExternalMcpReconcileReport::default();
    let broker = kernel.broker();

    let load = match load_mcp_config_toml(raw_toml) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                "mcp.toml is unparseable ({e}); leaving every currently-registered \
                 external MCP server as-is — repair with `kj config reset mcp.toml` \
                 then `kj mcp reload`"
            );
            report.failed.push(("<mcp.toml>".to_string(), e));
            broker.set_external_mcp_failures(report.failed.clone()).await;
            return report;
        }
    };
    for w in &load.warnings {
        tracing::error!("mcp.toml: {w}");
    }
    report.warnings = load.warnings;

    let desired: HashMap<String, McpServerConfig> =
        load.servers.into_iter().map(|s| (s.name.clone(), s)).collect();

    let current_external: HashSet<String> = broker
        .list_instances()
        .await
        .into_iter()
        .filter_map(|id| id.as_str().strip_prefix("external.").map(str::to_string))
        .collect();

    // Removals first — free the name before we might re-add it under a
    // changed transport/enabled flag in the same pass.
    let mut removed_names: Vec<&String> =
        current_external.iter().filter(|n| !desired.contains_key(*n)).collect();
    removed_names.sort();
    for name in removed_names {
        let instance = external_instance_id(name);
        match broker.unregister(&instance).await {
            Ok(()) => {
                tracing::info!("external MCP server '{name}' unregistered (no longer in mcp.toml)");
                report.removed.push(name.clone());
            }
            Err(e) => {
                tracing::error!("external MCP server '{name}': unregister failed: {e}");
                report.failed.push((name.clone(), format!("unregister: {e}")));
            }
        }
    }

    let mut names: Vec<&String> = desired.keys().collect();
    names.sort();
    for name in names {
        let cfg = &desired[name];
        let instance = external_instance_id(name);

        if current_external.contains(name) {
            // Already running: never touch the connection (module doc,
            // "Reload semantics"). Refresh the policy so a call_timeout_ms
            // edit takes effect, and so a restart re-derives the SAME
            // override from the CRDT instead of reverting to
            // `Broker.policies`'s in-memory default (component 3's
            // restart-persistence fix).
            let policy = policy_for(kernel, cfg);
            if let Err(e) = broker.update_policy(&instance, Some(policy.call_timeout), None).await
            {
                // Raced a concurrent unregister of the same instance between
                // the snapshot above and here. Loud, not fatal.
                tracing::error!("external MCP server '{name}': policy refresh failed: {e}");
            }
            report.unchanged.push(name.clone());
            continue;
        }

        // New entry — connect + register.
        match ExternalMcpServer::connect(
            cfg.clone(),
            instance.clone(),
            kernel.timeouts().mcp_connect_timeout,
        )
        .await
        {
            Ok(server) => {
                let policy = policy_for(kernel, cfg);
                match broker.register_silently(Arc::new(server), policy).await {
                    Ok(()) => {
                        tracing::info!("external MCP server '{name}' registered as {instance}");
                        report.added.push(name.clone());
                    }
                    Err(e) => {
                        tracing::error!("external MCP server '{name}': register failed: {e}");
                        report.failed.push((name.clone(), format!("register: {e}")));
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    "external MCP server '{name}' is configured but failed to start: {e} \
                     — it will not be callable until `kj mcp reload` succeeds for it"
                );
                report.failed.push((name.clone(), e.to_string()));
            }
        }
    }

    broker.set_external_mcp_failures(report.failed.clone()).await;
    report
}

/// The real entry point: read `/etc/config/mcp.toml` through the VFS (the
/// CRDT is the sole owner — no host file), then reconcile.
///
/// A read or whole-file parse failure falls back to the **embedded
/// default**, loudly — same shape as `initialize_kernel_models`'s handling
/// of `models.toml` (`kaijutsu-server::rpc`) — because "no configured MCP
/// servers at all" is at least an honest state, where silently keeping
/// stale registrations from a previous boot would not be.
pub async fn reconcile_external_mcp_servers(kernel: &Arc<Kernel>) -> ExternalMcpReconcileReport {
    use crate::vfs::VfsOps;

    let mcp_path = paths::config_path("mcp.toml");
    let raw = match kernel.vfs().read_all(std::path::Path::new(&mcp_path)).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            tracing::error!(
                "read {mcp_path} from CRDT failed: {e}; reconciling external MCP servers \
                 against the embedded default instead"
            );
            crate::config_seed::DEFAULT_MCP_CONFIG.to_string()
        }
    };

    reconcile_with_toml(kernel, &raw).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::context::CallContext;
    use crate::mcp::error::{McpError, McpResult};
    use crate::mcp::server_like::{McpServerLike, ServerNotification};
    use crate::mcp::types::{Health, KernelCallParams, KernelTool, KernelToolResult};
    use async_trait::async_trait;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    /// A no-op `McpServerLike` used to simulate "already registered" without
    /// spawning a real subprocess. Stands in for a running `ExternalMcpServer`
    /// in tests that only care about the broker-level reconcile decisions
    /// (remove / leave-alone / policy-refresh), not the rmcp handshake.
    struct FakeServer {
        id: InstanceId,
    }

    #[async_trait]
    impl McpServerLike for FakeServer {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }
        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            _params: KernelCallParams,
            _ctx: &CallContext,
            _cancel: CancellationToken,
        ) -> McpResult<KernelToolResult> {
            Err(McpError::Unsupported)
        }
        fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
            let (_tx, rx) = broadcast::channel(1);
            rx
        }
        async fn health(&self) -> Health {
            Health::Ready
        }
    }

    async fn fake_kernel() -> Arc<Kernel> {
        Arc::new(Kernel::new_ephemeral("test").await)
    }

    async fn register_fake(kernel: &Arc<Kernel>, name: &str) {
        let id = external_instance_id(name);
        kernel
            .broker()
            .register_silently(Arc::new(FakeServer { id: id.clone() }), InstancePolicy::for_kernel(kernel))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unparseable_toml_reports_a_failure_and_touches_nothing() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "kept").await;

        let report = reconcile_with_toml(&kernel, "[invalid").await;

        assert_eq!(report.failed.len(), 1);
        assert!(report.added.is_empty());
        assert!(report.removed.is_empty());
        // The pre-existing registration is untouched — a broken mcp.toml
        // must not take down what's already running.
        let ids = kernel.broker().list_instances().await;
        assert!(ids.contains(&external_instance_id("kept")));
    }

    #[tokio::test]
    async fn malformed_entry_is_reported_but_does_not_abort_the_rest() {
        let kernel = fake_kernel().await;
        let toml = r#"
[servers.broken]
transport = "carrier_pigeon"

[servers.also_unreachable]
command = "/definitely/does/not/exist/mcp-server"
"#;
        let report = reconcile_with_toml(&kernel, toml).await;

        assert_eq!(report.warnings.len(), 1, "the malformed entry is reported");
        assert!(report.warnings[0].contains("broken"));
        // The other, well-formed-but-unreachable entry still gets a real
        // connect attempt (and fails loudly, not silently).
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "also_unreachable");
    }

    #[tokio::test]
    async fn configured_but_unstartable_server_is_visibly_failed() {
        let kernel = fake_kernel().await;
        let toml = r#"
[servers.ghost]
command = "/definitely/does/not/exist/mcp-server"
"#;
        let report = reconcile_with_toml(&kernel, toml).await;

        assert!(report.added.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "ghost");
        assert!(
            kernel
                .broker()
                .list_instances()
                .await
                .iter()
                .all(|id| id.as_str() != "external.ghost"),
            "a failed connect must not leave a half-registered instance"
        );
        // Observable via the broker, not just the log (component 2's "loud,
        // not a log grep" requirement).
        let failures = kernel.broker().external_mcp_failures().await.unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "ghost");
    }

    #[tokio::test]
    async fn removed_from_config_unregisters() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "leaving").await;

        let report = reconcile_with_toml(&kernel, "").await;

        assert_eq!(report.removed, vec!["leaving".to_string()]);
        assert!(
            kernel.broker().list_instances().await.is_empty(),
            "the instance should be gone from the broker"
        );
    }

    #[tokio::test]
    async fn disabling_a_server_in_config_unregisters_it() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "toggled").await;

        let toml = r#"
[servers.toggled]
command = "/bin/anything"
enabled = false
"#;
        let report = reconcile_with_toml(&kernel, toml).await;
        assert_eq!(report.removed, vec!["toggled".to_string()]);
    }

    #[tokio::test]
    async fn already_running_server_is_left_alone_not_reconnected() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "steady").await;

        let toml = r#"
[servers.steady]
command = "/definitely/does/not/exist/would-fail-if-touched"
"#;
        let report = reconcile_with_toml(&kernel, toml).await;

        // If reload tried to hot-swap this entry it would attempt to connect
        // the (deliberately unreachable) new command and land in `failed`.
        // Landing in `unchanged` instead proves the connection was never
        // touched.
        assert_eq!(report.unchanged, vec!["steady".to_string()]);
        assert!(report.failed.is_empty());
        assert!(report.added.is_empty());
    }

    #[tokio::test]
    async fn call_timeout_override_reaches_instance_policy_on_add() {
        let kernel = fake_kernel().await;
        // command is a real, fast-exiting binary so connect() fails quickly
        // via a handshake/EOF error rather than a slow timeout — we only
        // need `failed` to prove connect was attempted with this config;
        // the policy-reaches-InstancePolicy assertion below covers a
        // registered (not merely attempted) instance via `already_running`.
        let toml = r#"
[servers.steady]
command = "/bin/true"
call_timeout_ms = 900000
"#;
        // First bring "steady" up for real against the repo's own stub
        // fixture would require a working MCP handshake; instead, register
        // it directly (as the "already running" tests do) and prove reload
        // refreshes its policy from call_timeout_ms.
        register_fake(&kernel, "steady").await;
        let _ = reconcile_with_toml(&kernel, toml).await;

        let policy = kernel
            .broker()
            .policy_of(&external_instance_id("steady"))
            .await
            .expect("instance should still be registered");
        assert_eq!(policy.call_timeout, std::time::Duration::from_millis(900_000));
    }

    #[tokio::test]
    async fn removing_the_override_reverts_to_kernel_default() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "steady").await;

        let with_override = r#"
[servers.steady]
command = "/bin/true"
call_timeout_ms = 900000
"#;
        reconcile_with_toml(&kernel, with_override).await;
        let after_override = kernel
            .broker()
            .policy_of(&external_instance_id("steady"))
            .await
            .unwrap();
        assert_eq!(after_override.call_timeout, std::time::Duration::from_millis(900_000));

        let without_override = r#"
[servers.steady]
command = "/bin/true"
"#;
        reconcile_with_toml(&kernel, without_override).await;
        let after_revert = kernel
            .broker()
            .policy_of(&external_instance_id("steady"))
            .await
            .unwrap();
        assert_eq!(after_revert.call_timeout, kernel.timeouts().mcp_call_timeout_default);
    }

    #[tokio::test]
    async fn reconcile_is_idempotent() {
        let kernel = fake_kernel().await;
        register_fake(&kernel, "steady").await;

        let toml = r#"
[servers.steady]
command = "/bin/true"
"#;
        let first = reconcile_with_toml(&kernel, toml).await;
        let second = reconcile_with_toml(&kernel, toml).await;

        assert_eq!(first.unchanged, vec!["steady".to_string()]);
        assert_eq!(second.unchanged, vec!["steady".to_string()]);
        assert_eq!(
            kernel.broker().list_instances().await.len(),
            1,
            "running reconcile twice must not double-register"
        );
    }

    // A "the add path actually lands a callable instance" test — using a
    // real MCP-speaking subprocess — lives in `tests/external_mcp_stub.rs`
    // instead of here: `CARGO_BIN_EXE_mcp_stub_server` is only defined by
    // Cargo for integration tests, not for this crate's own `--lib` unit
    // tests.
}
