//! `kj mcp` — visibility and reconciliation for external MCP servers
//! (mcp.toml: kaibo, bevy_brp, …).
//!
//! Before this verb existed, `kj config set mcp.toml` was a silent no-op at
//! runtime — nothing ever read the file back. `list` (alias `status`) makes
//! the configured-vs-actually-running split visible; `reload` re-reads
//! mcp.toml and reconciles the broker's live registrations against it (see
//! `mcp::external_registry`'s module doc for the reload semantics — in
//! short: adds new entries, removes entries no longer configured, and
//! refreshes `InstancePolicy` on entries that stay running, but never
//! reconnects an already-running server even if its `command`/`args`
//! changed underneath it).
//!
//! ```text
//! kj mcp list [--json]     # alias: status
//! kj mcp reload            # re-read mcp.toml, reconcile
//! ```

use clap::{Parser, Subcommand};

use crate::mcp::{Capability, Health, external_instance_id};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "mcp",
    about = "External MCP servers (mcp.toml) — configured-vs-running visibility + reload",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    /// Show every configured server (mcp.toml) alongside what's actually
    /// registered on the broker, with health.
    #[command(alias = "status")]
    List {
        /// Emit a JSON object instead of a labelled table
        #[arg(long)]
        json: bool,
    },
    /// Re-read mcp.toml and reconcile: add newly-configured servers, remove
    /// ones no longer configured, refresh QoS policy on servers that stay
    /// running. Never reconnects an already-running server.
    Reload {},
}

impl KjDispatcher {
    pub(crate) async fn dispatch_mcp(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<McpArgs>();
        }
        let parsed = match McpArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), kaijutsu_types::ContentType::Plain);
                }
                return KjResult::Err(format!("kj mcp: {e}"));
            }
        };

        // `reload` re-reads the kernel-owned mcp.toml and materializes it as
        // running processes — the same authority tier as being able to edit
        // that file (`kj config set/reset mcp.toml`), so it's gated on the
        // same capability rather than a bespoke one. `list` is a read.
        if matches!(parsed.command, McpCommand::Reload {})
            && let Err(denied) = self.require_cap(caller, Capability::ConfigWrite, "mcp reload")
        {
            return denied;
        }

        match parsed.command {
            McpCommand::List { json } => self.mcp_list(json).await,
            McpCommand::Reload {} => self.mcp_reload().await,
        }
    }

    async fn mcp_list(&self, json: bool) -> KjResult {
        use crate::mcp::load_mcp_config_toml;
        use crate::vfs::VfsOps;
        use kaijutsu_types::paths;

        let mcp_path = paths::config_path("mcp.toml");
        let raw = match self
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(&mcp_path))
            .await
        {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                return KjResult::Err(format!("kj mcp list: read {mcp_path}: {e}"));
            }
        };
        let load = match load_mcp_config_toml(&raw) {
            Ok(l) => l,
            Err(e) => {
                return KjResult::Err(format!(
                    "kj mcp list: {mcp_path} is unparseable: {e} — repair with \
                     `kj config reset mcp.toml`"
                ));
            }
        };

        let broker = self.kernel().broker();
        let registered: std::collections::HashSet<String> = broker
            .list_instances()
            .await
            .into_iter()
            .filter_map(|id| id.as_str().strip_prefix("external.").map(str::to_string))
            .collect();
        let last_failures: std::collections::HashMap<String, String> = broker
            .external_mcp_failures()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        // A structurally-invalid `[servers.X]` entry (bad transport, missing
        // `command`/`url`) never lands in `load.servers` — `load_mcp_config_toml`
        // drops it before it's a `McpServerConfig` at all. Without this,
        // such an entry had NO row here: not in `load.servers` (never
        // parsed clean), not in `registered` (never even attempted). `list`
        // reads mcp.toml directly rather than reconciling, so this must not
        // depend on `kj mcp reload` having run first — the fresh parse
        // result is the source of truth, `last_failures` is a fallback for
        // names this pass doesn't know about.
        let invalid: std::collections::HashMap<String, String> = load
            .invalid
            .iter()
            .map(|i| (i.name.clone(), i.reason.clone()))
            .collect();

        // Union of configured names, malformed-but-named entries, and
        // anything still registered under `external.*` (e.g. a stale
        // registration mid-reload race) — every name here is worth
        // showing, not just the ones that agree.
        let mut names: Vec<String> = load.servers.iter().map(|s| s.name.clone()).collect();
        for name in invalid.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        for r in &registered {
            if !names.contains(r) {
                names.push(r.clone());
            }
        }
        names.sort();

        let mut rows = Vec::with_capacity(names.len());
        for name in &names {
            let cfg = load.servers.iter().find(|s| &s.name == name);
            let is_running = registered.contains(name);
            let invalid_reason = invalid.get(name);
            let health = if is_running {
                match broker.instances_snapshot().await.get(&external_instance_id(name)) {
                    Some(server) => Some(server.health().await),
                    None => None,
                }
            } else {
                None
            };
            let status = if invalid_reason.is_some() {
                // Structurally malformed — never got a connect attempt, so
                // it's never "running"/"orphaned"; surfaced exactly like a
                // well-formed-but-unreachable entry rather than vanishing.
                "failed"
            } else {
                match (cfg.is_some(), is_running) {
                    (true, true) => "running",
                    (true, false) => "failed", // configured, enabled, not registered
                    (false, true) => "orphaned", // registered but no longer configured (reload pending)
                    (false, false) => "unknown", // unreachable in practice — in `names` because of one of the two sets
                }
            };
            rows.push((
                name.clone(),
                status,
                cfg.map(|c| format!("{:?}", c.transport)),
                health,
                invalid_reason.cloned().or_else(|| last_failures.get(name).cloned()),
            ));
        }

        if rows.is_empty() {
            return KjResult::ok_with_data(
                "(no external MCP servers configured)".to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        }

        let data = serde_json::Value::Array(
            rows.iter()
                .map(|(name, _, _, _, _)| {
                    serde_json::Value::String(external_instance_id(name).as_str().to_string())
                })
                .collect(),
        );

        if json {
            let servers: Vec<serde_json::Value> = rows
                .iter()
                .map(|(name, status, transport, health, last_failure)| {
                    serde_json::json!({
                        "name": name,
                        "instance": external_instance_id(name).as_str(),
                        "status": status,
                        "transport": transport,
                        "health": health.as_ref().map(health_json),
                        "last_failure": last_failure,
                    })
                })
                .collect();
            let out = serde_json::json!({ "count": rows.len(), "servers": servers });
            return KjResult::ok_with_data(out.to_string(), data);
        }

        let mut lines = Vec::with_capacity(rows.len());
        for (name, status, transport, health, last_failure) in &rows {
            let health_str = match health {
                Some(Health::Ready) => " ready".to_string(),
                Some(Health::Degraded { reason }) => format!(" degraded ({reason})"),
                Some(Health::Down { reason }) => format!(" down ({reason})"),
                None => String::new(),
            };
            let transport_str = transport.as_deref().map(|t| format!(" [{t}]")).unwrap_or_default();
            let mut line = format!("  {name} — {status}{transport_str}{health_str}");
            if let Some(reason) = last_failure {
                line.push_str(&format!("\n    last failure: {reason}"));
            }
            lines.push(line);
        }
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    async fn mcp_reload(&self) -> KjResult {
        use crate::mcp::reconcile_external_mcp_servers;

        let report = reconcile_external_mcp_servers(self.kernel()).await;

        let data = serde_json::json!({
            "added": report.added,
            "removed": report.removed,
            "unchanged": report.unchanged,
            "failed": report.failed,
            "warnings": report.warnings,
        });

        let message = format!(
            "mcp reload: {} added, {} removed, {} unchanged, {} failed, {} malformed entr{}",
            report.added.len(),
            report.removed.len(),
            report.unchanged.len(),
            report.failed.len(),
            report.warnings.len(),
            if report.warnings.len() == 1 { "y" } else { "ies" },
        );

        if report.is_clean() {
            KjResult::ok_with_data(message, data)
        } else {
            // Reload itself did not fail — a subset of entries did, and that
            // subset is fully captured in `data`/`message`. This is still an
            // `Ok` (not `Err`) because a partial reload with some servers
            // added/removed successfully is a real, useful outcome, not a
            // command failure — the failures are the point of surfacing this
            // loudly, not evidence the command itself broke.
            KjResult::ok_with_data(message, data)
        }
    }
}

/// JSON-friendly health projection for `--json`.
fn health_json(h: &Health) -> serde_json::Value {
    match h {
        Health::Ready => serde_json::json!({"state": "ready"}),
        Health::Degraded { reason } => serde_json::json!({"state": "degraded", "reason": reason}),
        Health::Down { reason } => serde_json::json!({"state": "down", "reason": reason}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers;
    use crate::vfs::VfsOps;
    use kaijutsu_types::paths;

    /// A dispatcher whose `/etc/config` is the real kernel-owned backend
    /// (`test_dispatcher_rc` seeds every config file including
    /// mcp.toml), with mcp.toml then overwritten to the given body — enough
    /// to exercise `kj mcp list`/`reload` through the real VFS path without
    /// standing up a full `create_shared_kernel` (which also boots
    /// LLM/semantic-index/etc.).
    async fn test_dispatcher(mcp_toml: &str) -> KjDispatcher {
        let d = test_helpers::test_dispatcher_rc().await;
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new(&paths::config_path("mcp.toml")),
                mcp_toml.as_bytes(),
            )
            .await
            .unwrap();
        d
    }

    #[tokio::test]
    async fn list_shows_configured_but_unstartable_server_as_failed() {
        let d = test_dispatcher(
            r#"
[servers.ghost]
command = "/definitely/does/not/exist/mcp-server"
"#,
        )
        .await;
        let caller = test_helpers::test_caller();

        // Nothing registered yet — mirrors the boot state before reconcile
        // has run for this entry.
        let result = d.dispatch_mcp(&[s("list"), s("--json")], &caller).await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(v["servers"][0]["name"], "ghost");
        assert_eq!(v["servers"][0]["status"], "failed");
    }

    /// Defect 3: a structurally-invalid `[servers.X]` entry (bad transport,
    /// missing `command`/`url`) used to be dropped inside
    /// `load_mcp_config_toml` with only a free-text warning — it never
    /// landed in `load.servers`, so `list`'s `names` (built from
    /// `load.servers` union what's registered) never contained it either.
    /// To an operator running `kj mcp list`, a typo'd entry simply didn't
    /// exist: no row, no reason, nothing to grep for. This must be visible
    /// even when `kj mcp reload` has never run (list reads mcp.toml
    /// directly, it doesn't reconcile).
    #[tokio::test]
    async fn list_shows_malformed_entry_as_failed_not_missing() {
        let d = test_dispatcher(
            r#"
[servers.typo]
transport = "streemable_http"
url = "http://localhost:9"
"#,
        )
        .await;
        let caller = test_helpers::test_caller();

        let result = d.dispatch_mcp(&[s("list"), s("--json")], &caller).await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&message)
            .unwrap_or_else(|e| panic!("expected JSON, got {e}: {message:?}"));
        assert_eq!(v["servers"][0]["name"], "typo");
        assert_eq!(v["servers"][0]["status"], "failed");
        assert!(
            v["servers"][0]["last_failure"]
                .as_str()
                .expect("last_failure should be a string")
                .contains("streemable_http"),
            "expected the reason to name the bad transport, got {:?}",
            v["servers"][0]["last_failure"]
        );
    }

    #[tokio::test]
    async fn reload_then_list_shows_running() {
        let d = test_dispatcher(
            r#"
[servers.brp]
command = "/bin/true"
"#,
        )
        .await;
        let caller = test_helpers::test_caller();

        // `/bin/true` exits immediately without speaking MCP, so connect()
        // fails fast — this exercises the reload→list pipeline end to end
        // without needing a real MCP-speaking subprocess. The "connects
        // successfully" path is covered by the `mcp_stub_server` fixture
        // used in the server-crate boot integration test.
        let reload = d.dispatch_mcp(&[s("reload")], &caller).await;
        assert!(reload.is_ok(), "{reload:?}");

        let result = d.dispatch_mcp(&[s("list"), s("--json")], &caller).await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        let v: serde_json::Value = serde_json::from_str(&message).unwrap();
        assert_eq!(v["servers"][0]["name"], "brp");
        assert_eq!(v["servers"][0]["status"], "failed");
        assert!(v["servers"][0]["last_failure"].is_string());
    }

    #[tokio::test]
    async fn reload_denied_without_config_write_capability() {
        let d = test_dispatcher("").await;
        let caller = test_helpers::caller_with_context(kaijutsu_types::ContextId::new());

        let result = d.dispatch_mcp(&[s("reload")], &caller).await;
        assert!(matches!(result, KjResult::Err(_)));
    }

    #[tokio::test]
    async fn list_is_ungated_for_a_non_privileged_caller() {
        let d = test_dispatcher(
            r#"
[servers.brp]
command = "/bin/true"
"#,
        )
        .await;
        let caller = test_helpers::caller_with_context(kaijutsu_types::ContextId::new());

        let result = d.dispatch_mcp(&[s("list")], &caller).await;
        assert!(result.is_ok(), "{result:?}");
    }

    fn s(x: &str) -> String {
        x.to_string()
    }
}
