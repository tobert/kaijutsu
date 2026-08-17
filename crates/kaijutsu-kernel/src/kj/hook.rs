//! `kj hook` — administer the broker's hook tables without ever entering
//! hook evaluation (`Broker::evaluate_phase`) to do it.
//!
//! Slice 1 of `docs/gate-and-shell-split.md` (Amy's ruling, 2026-08-17,
//! "Hook self-lockout recovery"). A `PreCall Deny("*")` hook denies
//! `builtin.hooks`' own `hook_list`/`hook_remove` MCP tools — proven by
//! `hooks_admin_is_subject_to_hooks`
//! (`mcp/servers/hooks_builtin.rs`) — and because hooks rehydrate from
//! `KernelDb` at `Broker::set_db`, that lockout **survives a restart**. The
//! only recovery path before this verb existed was hand-editing kernel
//! SQLite, which the project's own standing rule forbids
//! (`feedback_no_direct_kernel_db_access`).
//!
//! This is not a carve-out inside the hook mechanism (a permanent "except
//! this one caller" hole a hook could never legitimately close). It is a
//! **sibling path that never enters the mechanism it exists to route
//! around** — the same shape `kj mcp`/`kj policy` already use for other
//! broker-wide admin surfaces (see `require_cap`'s own doc: "`kj` is a
//! kaish builtin that bypasses the broker `call_tool` / facade gates
//! entirely ... this is the third enforcement surface alongside the broker
//! and the facade gate"). `list`/`show`/`remove`/`add` here talk to
//! `Broker::persist_hook_insert`/`persist_hook_delete` and the in-memory
//! `HookTables` directly — never `Broker::call_tool`, never
//! `evaluate_phase`. A `PreCall Deny("*")` hook can only ever match calls
//! that pass through `evaluate_phase`; this verb structurally never does,
//! so there is nothing to carve out.
//!
//! ```text
//! kj hook list [--phase <phase>]        # every persisted hook, all phases or one
//! kj hook show <id>                     # full detail of one entry
//! kj hook remove <id>                   # idempotent; unknown id is not an error
//! kj hook add <phase> <match-json> <action-json>
//! ```
//!
//! `add`/`remove` are gated on `Capability::ConfigWrite` — the same
//! authority tier `kj mcp reload` uses for materializing `mcp.toml` as
//! running processes, reused here per the design doc's own note that this
//! is "a Slice 1 implementation question, not a design one." `list`/`show`
//! are reads, ungated, matching `kj mcp list`/`kj policy show`. None of
//! this authorization is `evaluate_phase` — it is `kj`'s own loadout check
//! against the caller's context binding (or the rc lifecycle's privileged
//! bypass), independent of the broker hook tables this verb exists to
//! recover.
//!
//! `<match-json>` mirrors `HookAddParams`' match/priority/hook_id fields
//! (`mcp/servers/hooks_builtin.rs`) minus `phase`/`action`, which are
//! separate positionals here. `<action-json>` is the same tagged
//! `HookActionWire` shape `builtin.hooks`' own `hook_add` accepts —
//! "symmetry with builtin.hooks' own add," per the design doc.

use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde::Deserialize;

use crate::mcp::broker::Broker;
use crate::mcp::hook_persist::{parse_phase, phase_to_str};
use crate::mcp::servers::hooks_builtin::HookActionWire;
use crate::mcp::{
    AskSpec, Capability, GlobPattern, HookAction, HookBody, HookEntry, HookId, HookTable,
    HookTables, LogSpec, McpHookPhase, ToolContent,
};
use kaijutsu_types::{ContextId, PrincipalId};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "hook",
    about = "Administer the broker's hook tables directly — never routes through hook evaluation",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct HookArgs {
    #[command(subcommand)]
    command: HookCommand,
}

#[derive(Subcommand, Debug)]
enum HookCommand {
    /// List every persisted hook (optionally filtered to one phase).
    List {
        /// pre_call | post_call | on_error | on_notification | list_tools
        #[arg(long)]
        phase: Option<String>,
    },
    /// Show the full detail of one hook entry.
    Show {
        /// Hook id
        hook_id: String,
    },
    /// Remove a hook by id. Idempotent — an unknown id is not an error.
    Remove {
        /// Hook id
        hook_id: String,
    },
    /// Register a new hook entry.
    Add {
        /// pre_call | post_call | on_error | on_notification | list_tools
        phase: String,
        /// JSON object: match_instance/match_tool/match_context/match_principal/priority/hook_id (all optional)
        match_json: String,
        /// JSON object: the tagged HookActionWire shape (same as builtin.hooks' hook_add)
        action_json: String,
    },
}

/// The `<match-json>` payload for `kj hook add` — every field `HookAddParams`
/// carries besides `phase`/`action`, which are separate positionals on this
/// verb. All-optional, `deny_unknown_fields` so a typo'd key fails loud
/// rather than silently matching nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookAddMatch {
    match_instance: Option<String>,
    match_tool: Option<String>,
    match_context: Option<String>,
    match_principal: Option<String>,
    priority: Option<i32>,
    hook_id: Option<String>,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_hook(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<HookArgs>();
        }
        let parsed = match HookArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), kaijutsu_types::ContentType::Plain);
                }
                return KjResult::Err(format!("kj hook: {e}"));
            }
        };

        // `add`/`remove` mutate broker-wide hook tables — the same authority
        // tier as `kj mcp reload` (materializing mcp.toml as running
        // processes) and `kj config set` (kernel-owned config files), so
        // they're gated on the same capability rather than a bespoke one.
        // `list`/`show` are reads. This is `kj`'s OWN loadout check
        // (`require_cap`) — a third enforcement surface, independent of and
        // never routing through the broker's `evaluate_phase`, which is the
        // entire point of this verb existing.
        if matches!(parsed.command, HookCommand::Add { .. } | HookCommand::Remove { .. })
            && let Err(denied) = self.require_cap(caller, Capability::ConfigWrite, "hook")
        {
            return denied;
        }

        match parsed.command {
            HookCommand::List { phase } => self.hook_list(phase.as_deref()).await,
            HookCommand::Show { hook_id } => self.hook_show(&hook_id).await,
            HookCommand::Remove { hook_id } => self.hook_remove(&hook_id).await,
            HookCommand::Add {
                phase,
                match_json,
                action_json,
            } => self.hook_add(&phase, &match_json, &action_json).await,
        }
    }

    async fn hook_list(&self, phase_filter: Option<&str>) -> KjResult {
        let filter = match phase_filter {
            Some(s) => match parse_phase(s) {
                Some(p) => Some(p),
                None => return KjResult::Err(format!("kj hook list: unknown phase '{s}'")),
            },
            None => None,
        };

        let hooks = self.kernel().broker().hooks().read().await;
        let mut rows: Vec<(McpHookPhase, &HookEntry)> = Vec::new();
        for (phase, table) in phase_tables(&hooks) {
            if let Some(f) = filter {
                if f != phase {
                    continue;
                }
            }
            for entry in &table.entries {
                rows.push((phase, entry));
            }
        }

        if rows.is_empty() {
            return KjResult::ok_with_data(
                "(no hooks registered)".to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        }

        let data = serde_json::Value::Array(
            rows.iter()
                .map(|(_, e)| serde_json::Value::String(e.id.0.clone()))
                .collect(),
        );
        let lines: Vec<String> = rows
            .iter()
            .map(|(phase, e)| {
                let j = entry_json(*phase, e, false);
                let action_kind = j["action"]["type"].as_str().unwrap_or("?");
                let inst = j["match_instance"].as_str().unwrap_or("*");
                let tool = j["match_tool"].as_str().unwrap_or("*");
                format!(
                    "  {} [{}] prio={} match={inst}:{tool} action={action_kind}",
                    e.id.0,
                    phase_to_str(*phase),
                    e.priority,
                )
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    async fn hook_show(&self, hook_id: &str) -> KjResult {
        let hooks = self.kernel().broker().hooks().read().await;
        for (phase, table) in phase_tables(&hooks) {
            if let Some(entry) = table.entries.iter().find(|e| e.id.0 == hook_id) {
                let json = entry_json(phase, entry, true);
                let msg = serde_json::to_string_pretty(&json)
                    .unwrap_or_else(|_| format!("hook {hook_id}"));
                return KjResult::ok_with_data(msg, json);
            }
        }
        KjResult::Err(format!("kj hook show: no hook with id '{hook_id}'"))
    }

    async fn hook_remove(&self, hook_id: &str) -> KjResult {
        let broker = self.kernel().broker();
        // Durable delete FIRST — same contract as the MCP `hook_remove` tool:
        // surface a failed write as an error rather than diverging the
        // in-memory mirror from what a restart would rehydrate.
        if let Err(e) = broker.persist_hook_delete(hook_id).await {
            return KjResult::Err(format!(
                "kj hook remove: failed to persist delete of {hook_id}: {e}"
            ));
        }
        let removed = {
            let mut hooks = broker.hooks().write().await;
            let drop_id = |table: &mut HookTable| -> bool {
                let before = table.entries.len();
                table.entries.retain(|e| e.id.0 != hook_id);
                table.entries.len() != before
            };
            let mut any = false;
            any |= drop_id(&mut hooks.pre_call);
            any |= drop_id(&mut hooks.post_call);
            any |= drop_id(&mut hooks.on_error);
            any |= drop_id(&mut hooks.on_notification);
            any |= drop_id(&mut hooks.list_tools);
            any
        };
        let data = serde_json::json!({ "hook_id": hook_id, "removed": removed });
        let msg = if removed {
            format!("hook '{hook_id}' removed")
        } else {
            format!("hook '{hook_id}' not found (nothing to remove)")
        };
        KjResult::ok_with_data(msg, data)
    }

    async fn hook_add(&self, phase_str: &str, match_json: &str, action_json: &str) -> KjResult {
        let phase = match parse_phase(phase_str) {
            Some(p) => p,
            None => return KjResult::Err(format!("kj hook add: unknown phase '{phase_str}'")),
        };
        let m: HookAddMatch = match serde_json::from_str(match_json) {
            Ok(m) => m,
            Err(e) => return KjResult::Err(format!("kj hook add: invalid match JSON: {e}")),
        };
        let action_wire: HookActionWire = match serde_json::from_str(action_json) {
            Ok(a) => a,
            Err(e) => return KjResult::Err(format!("kj hook add: invalid action JSON: {e}")),
        };
        if let Err(e) = validate_action_for_phase(phase, &action_wire) {
            return KjResult::Err(format!("kj hook add: {e}"));
        }

        let match_context = match m.match_context.as_deref().map(ContextId::parse).transpose() {
            Ok(c) => c,
            Err(e) => {
                return KjResult::Err(format!("kj hook add: invalid match_context: {e}"));
            }
        };
        let match_principal = match m
            .match_principal
            .as_deref()
            .map(PrincipalId::parse)
            .transpose()
        {
            Ok(p) => p,
            Err(e) => {
                return KjResult::Err(format!("kj hook add: invalid match_principal: {e}"));
            }
        };

        let broker = self.kernel().broker();
        let (action, kaish_script_id) = match build_hook_action(broker, action_wire).await {
            Ok(a) => a,
            Err(e) => return KjResult::Err(format!("kj hook add: {e}")),
        };
        let id = m.hook_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let entry = HookEntry {
            id: HookId(id.clone()),
            match_instance: m.match_instance.map(GlobPattern),
            match_tool: m.match_tool.map(GlobPattern),
            match_context,
            match_principal,
            kaish_script_id,
            action,
            priority: m.priority.unwrap_or(0),
        };
        // Durable store FIRST — same contract as the MCP `hook_add` tool.
        if let Err(e) = broker.persist_hook_insert(phase, &entry).await {
            return KjResult::Err(format!("kj hook add: failed to persist hook {id}: {e}"));
        }
        {
            let mut hooks = broker.hooks().write().await;
            phase_table_mut(&mut hooks, phase).entries.push(entry);
        }
        let data = serde_json::json!({ "hook_id": id });
        KjResult::ok_with_data(format!("hook '{id}' added on phase {phase_str}"), data)
    }
}

fn phase_tables(hooks: &HookTables) -> [(McpHookPhase, &HookTable); 5] {
    [
        (McpHookPhase::PreCall, &hooks.pre_call),
        (McpHookPhase::PostCall, &hooks.post_call),
        (McpHookPhase::OnError, &hooks.on_error),
        (McpHookPhase::OnNotification, &hooks.on_notification),
        (McpHookPhase::ListTools, &hooks.list_tools),
    ]
}

fn phase_table_mut(hooks: &mut HookTables, phase: McpHookPhase) -> &mut HookTable {
    match phase {
        McpHookPhase::PreCall => &mut hooks.pre_call,
        McpHookPhase::PostCall => &mut hooks.post_call,
        McpHookPhase::OnError => &mut hooks.on_error,
        McpHookPhase::OnNotification => &mut hooks.on_notification,
        McpHookPhase::ListTools => &mut hooks.list_tools,
    }
}

/// Mirrors `mcp/servers/hooks_builtin.rs::validate_action_for_phase` (D-56):
/// `ListTools` is a list-filter phase, so `ShortCircuit`/`Invoke`/`Ask` have
/// no coherent semantics there and are rejected at add time. Duplicated
/// rather than imported — `hooks_builtin.rs`'s copy is module-private and
/// outside this slice's file territory; the rule itself is small and is
/// pinned by the module doc in `mcp/hook_table.rs`, not by this function.
fn validate_action_for_phase(phase: McpHookPhase, action: &HookActionWire) -> Result<(), String> {
    if phase == McpHookPhase::ListTools {
        match action {
            HookActionWire::BuiltinInvoke { .. }
            | HookActionWire::ShortCircuit { .. }
            | HookActionWire::Kaish { .. }
            | HookActionWire::KaishScript { .. }
            | HookActionWire::Ask { .. } => {
                return Err(
                    "list_tools is a list-filter phase; only 'deny' and 'log' actions have \
                     coherent semantics there"
                        .to_string(),
                );
            }
            HookActionWire::Deny { .. } | HookActionWire::Log { .. } => {}
        }
    }
    Ok(())
}

/// Resolve a wire `HookActionWire` into a live `(HookAction,
/// kaish_script_id)` pair. Mirrors
/// `mcp/servers/hooks_builtin.rs::build_hook_action` against the same
/// `Broker` primitives (`builtin_hooks()`, `get_hook_script_body()`) —
/// duplicated rather than imported for the same file-territory reason as
/// `validate_action_for_phase` above.
async fn build_hook_action(
    broker: &Arc<Broker>,
    action: HookActionWire,
) -> Result<(HookAction, Option<String>), String> {
    Ok(match action {
        HookActionWire::BuiltinInvoke { name } => {
            let hook = broker
                .builtin_hooks()
                .build(&name)
                .ok_or_else(|| format!("unknown builtin hook '{name}'"))?;
            (HookAction::Invoke(HookBody::Builtin { name, hook }), None)
        }
        HookActionWire::Kaish { body } => (HookAction::Invoke(HookBody::Kaish(body)), None),
        HookActionWire::KaishScript { script_id } => {
            let body = broker.get_hook_script_body(&script_id).await.ok_or_else(|| {
                format!(
                    "hook script '{script_id}' not found; create it with hook_script_add first"
                )
            })?;
            (HookAction::Invoke(HookBody::Kaish(body)), Some(script_id))
        }
        HookActionWire::ShortCircuit { result_text, is_error } => (
            HookAction::ShortCircuit(crate::mcp::KernelToolResult {
                is_error: is_error.unwrap_or(false),
                content: vec![ToolContent::Text(result_text)],
                structured: None,
            }),
            None,
        ),
        HookActionWire::Deny { reason } => (HookAction::Deny(reason), None),
        HookActionWire::Log { target, level } => {
            let level = parse_tracing_level(&level)?;
            (
                HookAction::Log(LogSpec {
                    target: target.unwrap_or_else(|| "kaijutsu::hooks".to_string()),
                    level,
                }),
                None,
            )
        }
        HookActionWire::Ask { description } => (HookAction::Ask(AskSpec { description }), None),
    })
}

fn parse_tracing_level(s: &str) -> Result<tracing::Level, String> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(tracing::Level::TRACE),
        "debug" => Ok(tracing::Level::DEBUG),
        "info" => Ok(tracing::Level::INFO),
        "warn" | "warning" => Ok(tracing::Level::WARN),
        "error" => Ok(tracing::Level::ERROR),
        other => Err(format!("unknown tracing level: {other:?}")),
    }
}

fn level_to_str(level: tracing::Level) -> &'static str {
    match level {
        tracing::Level::TRACE => "trace",
        tracing::Level::DEBUG => "debug",
        tracing::Level::INFO => "info",
        tracing::Level::WARN => "warn",
        tracing::Level::ERROR => "error",
    }
}

/// JSON summary of one entry. `full=false` (list rows) redacts large
/// bodies/reasons; `full=true` (`kj hook show`) returns everything.
/// Mirrors `mcp/servers/hooks_builtin.rs::entry_summary_json` — see the
/// file-territory note on `validate_action_for_phase`.
fn entry_json(phase: McpHookPhase, entry: &HookEntry, full: bool) -> serde_json::Value {
    let action_json = match &entry.action {
        HookAction::Invoke(HookBody::Builtin { name, .. }) => {
            serde_json::json!({ "type": "builtin_invoke", "name": name })
        }
        HookAction::Invoke(HookBody::Kaish(body)) => {
            let preview: String = if full {
                body.clone()
            } else {
                body.chars().take(64).collect()
            };
            serde_json::json!({ "type": "kaish", "body": preview })
        }
        HookAction::ShortCircuit(r) if full => serde_json::json!({
            "type": "short_circuit",
            "is_error": r.is_error,
            "result_text": r.content.iter().find_map(|c| match c {
                ToolContent::Text(s) => Some(s.clone()),
                _ => None,
            }),
        }),
        HookAction::ShortCircuit(r) => serde_json::json!({
            "type": "short_circuit",
            "is_error": r.is_error,
        }),
        HookAction::Deny(reason) if full => serde_json::json!({
            "type": "deny",
            "reason": reason,
        }),
        HookAction::Deny(_) => serde_json::json!({ "type": "deny" }),
        HookAction::Log(spec) => serde_json::json!({
            "type": "log",
            "target": spec.target,
            "level": level_to_str(spec.level),
        }),
        HookAction::Ask(spec) => serde_json::json!({
            "type": "ask",
            "description": spec.description,
        }),
    };
    serde_json::json!({
        "hook_id": entry.id.0,
        "phase": phase_to_str(phase),
        "priority": entry.priority,
        "match_instance": entry.match_instance.as_ref().map(|g| g.0.clone()),
        "match_tool": entry.match_tool.as_ref().map(|g| g.0.clone()),
        "match_context": entry.match_context.map(|c| c.to_string()),
        "match_principal": entry.match_principal.map(|p| p.to_string()),
        "kaish_script_id": entry.kaish_script_id,
        "action": action_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers;
    use crate::mcp::context::CallContext;
    use crate::mcp::error::McpError;
    use crate::mcp::types::KernelCallParams;
    use crate::mcp::InstanceId;
    use tokio_util::sync::CancellationToken;

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// Wire a dispatcher whose broker carries the real `builtin.hooks`
    /// admin server (block/file/hooks/…) so a direct `broker.call_tool`
    /// against `hook_list`/`hook_remove` actually runs hook evaluation —
    /// the shape `hooks_admin_is_subject_to_hooks` exercises against a bare
    /// `Broker`, wired here on top of `KjDispatcher` so `kj hook` and the
    /// MCP path share the same broker/hook-table state.
    async fn dispatcher_with_hooks_server() -> Arc<crate::kj::KjDispatcher> {
        let d = Arc::new(test_helpers::test_dispatcher().await);
        let store = d.block_store().clone();
        let file_cache = d.kernel().file_cache(&store);
        d.kernel()
            .register_builtin_mcp_servers(store, file_cache, None, d.kernel_db().clone())
            .await
            .expect("register builtin mcp servers");
        d
    }

    fn call_params(tool: &str, args: serde_json::Value) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new("builtin.hooks"),
            tool: tool.to_string(),
            arguments: args,
        }
    }

    /// The point of Slice 1. Install a `PreCall Deny("*")` directly on the
    /// broker's `HookTables` (the same self-lockout shape
    /// `hooks_admin_is_subject_to_hooks` proves is real and durable),
    /// confirm the MCP path (`builtin.hooks`' own `hook_list`) is denied,
    /// then confirm `kj hook remove` recovers it — in the same process, no
    /// restart, and never touching `Broker::call_tool`/`evaluate_phase` to
    /// do it. Both this test and `hooks_admin_is_subject_to_hooks` must
    /// hold: the MCP path being locked out is correct fail-closed
    /// behavior, not a bug this slice papers over.
    #[tokio::test]
    async fn hook_remove_recovers_from_self_lockout() {
        let d = dispatcher_with_hooks_server().await;
        let caller = test_helpers::test_caller(); // privileged: bypasses require_cap,
        // mirroring the rc lifecycle's trusted control plane — the same
        // bypass `require_cap`'s doc says a recovery path may rely on.

        // A real kernel (`Kernel::new`) engages `enforce_unbound_deny`, so
        // `broker.call_tool` refuses an unbound `CallContext` with
        // `CapabilityDenied` before it ever reaches hook evaluation — that's
        // a different gate than the one this test is about. Bind a context
        // to `builtin.hooks` so the call clears the capability check and the
        // Deny("*") hook is what actually stops it.
        let mcp_ctx = kaijutsu_types::ContextId::new();
        d.kernel()
            .broker()
            .set_binding(
                mcp_ctx,
                crate::mcp::ContextToolBinding::with_instances(vec![InstanceId::new(
                    "builtin.hooks",
                )]),
            )
            .await;
        let call_ctx = CallContext::new(
            kaijutsu_types::PrincipalId::new(),
            mcp_ctx,
            kaijutsu_types::SessionId::new(),
            d.kernel_id(),
        );

        {
            let mut hooks = d.kernel().broker().hooks().write().await;
            hooks.pre_call.entries.push(HookEntry {
                id: HookId("lockout".into()),
                match_instance: Some(GlobPattern("*".into())),
                match_tool: None,
                match_context: None,
                match_principal: None,
                kaish_script_id: None,
                action: HookAction::Deny("locked out".into()),
                priority: 0,
            });
        }

        // Confirm the MCP path is locked out, mirroring
        // `hooks_admin_is_subject_to_hooks`.
        let err = d
            .kernel()
            .broker()
            .call_tool(
                call_params("hook_list", serde_json::json!({})),
                &call_ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, McpError::Denied { by_hook } if by_hook.0 == "lockout"),
            "expected the MCP path to be locked out before recovery, got {err:?}",
        );

        // `kj hook remove` never goes through `evaluate_phase` — it should
        // succeed despite the very Deny("*") that just blocked the MCP path.
        let result = d
            .dispatch_hook(&[s("remove"), s("lockout")], &caller)
            .await;
        assert!(result.is_ok(), "kj hook remove failed: {result:?}");

        // The MCP path is no longer locked out.
        let ok = d
            .kernel()
            .broker()
            .call_tool(
                call_params("hook_list", serde_json::json!({})),
                &call_ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(ok.is_ok(), "MCP path should be recovered, got {ok:?}");
    }

    #[tokio::test]
    async fn add_list_show_remove_round_trip() {
        let d = test_helpers::test_dispatcher().await;
        let caller = test_helpers::test_caller();

        let add = d
            .dispatch_hook(
                &[
                    s("add"),
                    s("pre_call"),
                    s(r#"{"match_tool":"*","hook_id":"my-log"}"#),
                    s(r#"{"type":"log","level":"info","target":"kaijutsu::hooks::audit"}"#),
                ],
                &caller,
            )
            .await;
        let KjResult::Ok { data, .. } = &add else {
            panic!("expected Ok, got {add:?}");
        };
        assert_eq!(data.as_ref().unwrap()["hook_id"], "my-log");

        let list = d.dispatch_hook(&[s("list")], &caller).await;
        let KjResult::Ok { data, .. } = &list else {
            panic!("expected Ok, got {list:?}");
        };
        assert_eq!(
            data.as_ref().unwrap(),
            &serde_json::json!(["my-log"]),
            "list data must be an array of full hook ids"
        );

        let show = d.dispatch_hook(&[s("show"), s("my-log")], &caller).await;
        let KjResult::Ok { data, .. } = &show else {
            panic!("expected Ok, got {show:?}");
        };
        let obj = data.as_ref().unwrap();
        assert_eq!(obj["hook_id"], "my-log");
        assert_eq!(obj["action"]["type"], "log");
        assert_eq!(obj["action"]["level"], "info");

        let remove = d.dispatch_hook(&[s("remove"), s("my-log")], &caller).await;
        let KjResult::Ok { data, .. } = &remove else {
            panic!("expected Ok, got {remove:?}");
        };
        assert_eq!(data.as_ref().unwrap()["removed"], true);

        // Idempotent second remove.
        let again = d.dispatch_hook(&[s("remove"), s("my-log")], &caller).await;
        let KjResult::Ok { data, .. } = &again else {
            panic!("expected Ok (idempotent), got {again:?}");
        };
        assert_eq!(data.as_ref().unwrap()["removed"], false);
    }

    #[tokio::test]
    async fn add_rejects_shortcircuit_for_list_tools() {
        let d = test_helpers::test_dispatcher().await;
        let caller = test_helpers::test_caller();

        let result = d
            .dispatch_hook(
                &[
                    s("add"),
                    s("list_tools"),
                    s("{}"),
                    s(r#"{"type":"short_circuit","result_text":"nope"}"#),
                ],
                &caller,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "{result:?}");
    }

    #[tokio::test]
    async fn add_and_remove_denied_without_config_write_capability() {
        let d = test_helpers::test_dispatcher().await;
        let caller = test_helpers::caller_with_context(kaijutsu_types::ContextId::new());

        let add = d
            .dispatch_hook(
                &[
                    s("add"),
                    s("pre_call"),
                    s("{}"),
                    s(r#"{"type":"log","level":"info"}"#),
                ],
                &caller,
            )
            .await;
        assert!(matches!(add, KjResult::Err(_)), "{add:?}");

        let remove = d.dispatch_hook(&[s("remove"), s("whatever")], &caller).await;
        assert!(matches!(remove, KjResult::Err(_)), "{remove:?}");
    }

    #[tokio::test]
    async fn list_and_show_are_ungated_for_a_non_privileged_caller() {
        let d = test_helpers::test_dispatcher().await;
        let caller = test_helpers::caller_with_context(kaijutsu_types::ContextId::new());

        let list = d.dispatch_hook(&[s("list")], &caller).await;
        assert!(list.is_ok(), "{list:?}");

        let show = d.dispatch_hook(&[s("show"), s("no-such-id")], &caller).await;
        // Ungated, but the id doesn't exist — Err is the right shape, just
        // not the capability-denial flavor of Err. Distinguish by message.
        assert!(
            matches!(&show, KjResult::Err(m) if m.contains("no hook with id")),
            "{show:?}"
        );
    }
}
