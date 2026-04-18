//! `BuiltinHooksServer` — admin MCP surface for hook management (D-14,
//! Phase 4 / §4.3).
//!
//! Four tools delegate to `Broker`'s hook tables:
//! - `hook_add { phase, match_*?, priority?, action, hook_id? }` — push an
//!   entry onto the relevant `HookTable`. Returns the assigned id.
//! - `hook_remove { hook_id }` — walk every phase table and drop entries
//!   with the given id. Returns `{ removed: bool }`.
//! - `hook_list { phase? }` — redacted summary per entry. For `Invoke`
//!   bodies the builtin name is exposed; for `ShortCircuit` / `Deny` /
//!   `Log` only the action kind + safe detail is shown.
//! - `hook_inspect { hook_id }` — full payload of one entry.
//!
//! Holds `Weak<Broker>` to avoid the Arc cycle (broker owns the instance
//! Arc; the instance refers back via Weak and upgrades on each call).
//! Registered silently at kernel bootstrap (D-38).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{HookId, McpError, McpResult};
use super::super::hook_table::{
    GlobPattern, HookAction, HookBody, HookEntry, HookPhase, HookTable, LogSpec,
};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{
    InstanceId, KernelCallParams, KernelTool, KernelToolResult, ToolContent,
};
use kaijutsu_types::{ContextId, PrincipalId};

/// Wire representation of a hook action. The broker never serializes
/// `Arc<dyn Hook>`; `BuiltinInvoke` carries a registry name (D-50) and the
/// server resolves via `Broker::builtin_hooks()`.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookActionWire {
    /// Resolve a named builtin hook body from the broker registry.
    BuiltinInvoke { name: String },
    /// Reserved. `hook_add` rejects with `McpError::Unsupported` (D-50).
    Kaish { script_id: String },
    /// Return a synthetic result in lieu of calling the server.
    ShortCircuit {
        result_text: String,
        is_error: Option<bool>,
    },
    /// Terminate the phase with `McpError::Denied { by_hook }`. The
    /// `reason` is observable in tracing only.
    Deny { reason: String },
    /// Observability-only: emit a `tracing::event!` at the given level.
    /// Does NOT write a block (D-48).
    Log {
        target: Option<String>,
        level: String,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookAddParams {
    /// `pre_call` | `post_call` | `on_error` | `on_notification`.
    pub phase: String,
    pub match_instance: Option<String>,
    pub match_tool: Option<String>,
    pub match_context: Option<String>,
    pub match_principal: Option<String>,
    pub priority: Option<i32>,
    pub action: HookActionWire,
    /// Caller-supplied id; else a UUID v4 is generated.
    pub hook_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookRemoveParams {
    pub hook_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookListParams {
    /// Filter to a single phase; omit for all four.
    pub phase: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookInspectParams {
    pub hook_id: String,
}

pub struct BuiltinHooksServer {
    instance_id: InstanceId,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl BuiltinHooksServer {
    pub const INSTANCE: &'static str = "builtin.hooks";

    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            broker,
            notif_tx,
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }
}

fn parse_phase(s: &str) -> McpResult<HookPhase> {
    match s {
        "pre_call" => Ok(HookPhase::PreCall),
        "post_call" => Ok(HookPhase::PostCall),
        "on_error" => Ok(HookPhase::OnError),
        "on_notification" => Ok(HookPhase::OnNotification),
        "list_tools" => Ok(HookPhase::ListTools),
        other => Err(McpError::Protocol(format!(
            "unknown hook phase: {other:?}"
        ))),
    }
}

fn phase_to_str(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::PreCall => "pre_call",
        HookPhase::PostCall => "post_call",
        HookPhase::OnError => "on_error",
        HookPhase::OnNotification => "on_notification",
        HookPhase::ListTools => "list_tools",
    }
}

/// Reject action shapes that have no coherent semantics in the given phase.
///
/// D-56: `ListTools` is a list-filter phase. `ShortCircuit` (what would it
/// return in place of a list?) and `Invoke` (bodies can't rewrite lists)
/// are rejected at add time so the caller sees the failure immediately
/// rather than at first list-tools evaluation. `Kaish` is rejected
/// unconditionally by `build_hook_action` anyway; called out here for
/// completeness.
fn validate_action_for_phase(phase: HookPhase, action: &HookActionWire) -> McpResult<()> {
    if phase == HookPhase::ListTools {
        match action {
            HookActionWire::BuiltinInvoke { .. }
            | HookActionWire::ShortCircuit { .. }
            | HookActionWire::Kaish { .. } => return Err(McpError::Unsupported),
            HookActionWire::Deny { .. } | HookActionWire::Log { .. } => {}
        }
    }
    Ok(())
}

fn parse_tracing_level(s: &str) -> McpResult<tracing::Level> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Ok(tracing::Level::TRACE),
        "debug" => Ok(tracing::Level::DEBUG),
        "info" => Ok(tracing::Level::INFO),
        "warn" | "warning" => Ok(tracing::Level::WARN),
        "error" => Ok(tracing::Level::ERROR),
        other => Err(McpError::Protocol(format!(
            "unknown tracing level: {other:?}"
        ))),
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

/// Parse an admin-wire `HookActionWire` into a `HookAction`. Uses the
/// supplied broker for builtin-name resolution.
fn build_hook_action(
    broker: &Arc<Broker>,
    action: HookActionWire,
    instance: &InstanceId,
) -> McpResult<HookAction> {
    Ok(match action {
        HookActionWire::BuiltinInvoke { name } => {
            let hook = broker
                .builtin_hooks()
                .build(&name)
                .ok_or_else(|| McpError::ToolNotFound {
                    instance: instance.clone(),
                    tool: format!("builtin:{name}"),
                })?;
            HookAction::Invoke(HookBody::Builtin { name, hook })
        }
        HookActionWire::Kaish { .. } => {
            // D-50: Kaish bodies deferred. Reject at add time so the caller
            // sees the failure immediately, not at first match.
            return Err(McpError::Unsupported);
        }
        HookActionWire::ShortCircuit {
            result_text,
            is_error,
        } => HookAction::ShortCircuit(KernelToolResult {
            is_error: is_error.unwrap_or(false),
            content: vec![ToolContent::Text(result_text)],
            structured: None,
        }),
        HookActionWire::Deny { reason } => HookAction::Deny(reason),
        HookActionWire::Log { target, level } => {
            let level = parse_tracing_level(&level)?;
            HookAction::Log(LogSpec {
                target: target.unwrap_or_else(|| "kaijutsu::hooks".to_string()),
                level,
            })
        }
    })
}

/// JSON summary of one entry (list output; body detail for inspect).
fn entry_summary_json(phase: HookPhase, entry: &HookEntry, full: bool) -> serde_json::Value {
    let action_json = match &entry.action {
        HookAction::Invoke(HookBody::Builtin { name, .. }) => {
            serde_json::json!({ "type": "builtin_invoke", "name": name })
        }
        HookAction::Invoke(HookBody::Kaish(script)) => {
            serde_json::json!({ "type": "kaish", "script_id": script.id })
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
    };
    serde_json::json!({
        "hook_id": entry.id.0,
        "phase": phase_to_str(phase),
        "priority": entry.priority,
        "match_instance": entry.match_instance.as_ref().map(|g| g.0.clone()),
        "match_tool": entry.match_tool.as_ref().map(|g| g.0.clone()),
        "match_context": entry.match_context.map(|c| c.to_string()),
        "match_principal": entry.match_principal.map(|p| p.to_string()),
        "action": action_json,
    })
}

#[async_trait]
impl McpServerLike for BuiltinHooksServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let add_schema = schemars::schema_for!(HookAddParams);
        let remove_schema = schemars::schema_for!(HookRemoveParams);
        let list_schema = schemars::schema_for!(HookListParams);
        let inspect_schema = schemars::schema_for!(HookInspectParams);
        let add_val = serde_json::to_value(&add_schema).map_err(McpError::InvalidParams)?;
        let remove_val =
            serde_json::to_value(&remove_schema).map_err(McpError::InvalidParams)?;
        let list_val = serde_json::to_value(&list_schema).map_err(McpError::InvalidParams)?;
        let inspect_val =
            serde_json::to_value(&inspect_schema).map_err(McpError::InvalidParams)?;
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_add".to_string(),
                description: Some(
                    "Register a hook entry on the named phase table (pre_call / \
                     post_call / on_error / on_notification)."
                        .to_string(),
                ),
                input_schema: add_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_remove".to_string(),
                description: Some(
                    "Remove a hook entry by id across every phase table."
                        .to_string(),
                ),
                input_schema: remove_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_list".to_string(),
                description: Some(
                    "List hook entries with redacted bodies. Filters by \
                     phase when supplied."
                        .to_string(),
                ),
                input_schema: list_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_inspect".to_string(),
                description: Some(
                    "Return the full payload for one hook entry."
                        .to_string(),
                ),
                input_schema: inspect_val,
            },
        ])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        _ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        let broker = self.broker()?;
        match params.tool.as_str() {
            "hook_add" => {
                let p: HookAddParams =
                    serde_json::from_value(params.arguments.clone())
                        .map_err(McpError::InvalidParams)?;
                let phase = parse_phase(&p.phase)?;
                validate_action_for_phase(phase, &p.action)?;
                let match_context = p
                    .match_context
                    .as_deref()
                    .map(|s| {
                        ContextId::parse(s).map_err(|e| {
                            McpError::Protocol(format!(
                                "invalid match_context {s:?}: {e}"
                            ))
                        })
                    })
                    .transpose()?;
                let match_principal = p
                    .match_principal
                    .as_deref()
                    .map(|s| {
                        PrincipalId::parse(s).map_err(|e| {
                            McpError::Protocol(format!(
                                "invalid match_principal {s:?}: {e}"
                            ))
                        })
                    })
                    .transpose()?;
                let action = build_hook_action(&broker, p.action, &self.instance_id)?;
                let id = p
                    .hook_id
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let entry = HookEntry {
                    id: HookId(id.clone()),
                    match_instance: p.match_instance.map(GlobPattern),
                    match_tool: p.match_tool.map(GlobPattern),
                    match_context,
                    match_principal,
                    action,
                    priority: p.priority.unwrap_or(0),
                };
                {
                    let mut hooks = broker.hooks().write().await;
                    let table = phase_table_mut(&mut hooks, phase);
                    table.entries.push(entry.clone());
                }
                // Best-effort persist to the kernel DB. No-op when the
                // broker has no DB handle wired (tests, early bootstrap);
                // failure warns but does not bubble — the in-memory
                // push is authoritative for the rest of this kernel's
                // lifetime.
                broker.persist_hook_insert(phase, &entry).await;
                let json = serde_json::json!({ "hook_id": id });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_remove" => {
                let p: HookRemoveParams =
                    serde_json::from_value(params.arguments.clone())
                        .map_err(McpError::InvalidParams)?;
                let removed = {
                    let mut hooks = broker.hooks().write().await;
                    let mut any_removed = false;
                    let drop_id = |table: &mut HookTable, id: &str| -> bool {
                        let before = table.entries.len();
                        table.entries.retain(|e| e.id.0 != id);
                        table.entries.len() != before
                    };
                    any_removed |= drop_id(&mut hooks.pre_call, &p.hook_id);
                    any_removed |= drop_id(&mut hooks.post_call, &p.hook_id);
                    any_removed |= drop_id(&mut hooks.on_error, &p.hook_id);
                    any_removed |= drop_id(&mut hooks.on_notification, &p.hook_id);
                    any_removed |= drop_id(&mut hooks.list_tools, &p.hook_id);
                    any_removed
                };
                // Mirror the delete into the DB regardless of whether
                // an in-memory entry existed: callers may retry
                // `hook_remove` after a partial failure, and the DB is
                // the authoritative store for post-restart state.
                broker.persist_hook_delete(&p.hook_id).await;
                let json = serde_json::json!({ "removed": removed });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_list" => {
                let p: HookListParams =
                    serde_json::from_value(params.arguments.clone())
                        .map_err(McpError::InvalidParams)?;
                let filter_phase = p.phase.as_deref().map(parse_phase).transpose()?;
                let hooks = broker.hooks().read().await;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for (phase, table) in [
                    (HookPhase::PreCall, &hooks.pre_call),
                    (HookPhase::PostCall, &hooks.post_call),
                    (HookPhase::OnError, &hooks.on_error),
                    (HookPhase::OnNotification, &hooks.on_notification),
                    (HookPhase::ListTools, &hooks.list_tools),
                ] {
                    if filter_phase.is_some() && filter_phase != Some(phase) {
                        continue;
                    }
                    for entry in &table.entries {
                        out.push(entry_summary_json(phase, entry, false));
                    }
                }
                let json = serde_json::json!({ "hooks": out });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_inspect" => {
                let p: HookInspectParams =
                    serde_json::from_value(params.arguments.clone())
                        .map_err(McpError::InvalidParams)?;
                let hooks = broker.hooks().read().await;
                let mut found: Option<serde_json::Value> = None;
                for (phase, table) in [
                    (HookPhase::PreCall, &hooks.pre_call),
                    (HookPhase::PostCall, &hooks.post_call),
                    (HookPhase::OnError, &hooks.on_error),
                    (HookPhase::OnNotification, &hooks.on_notification),
                    (HookPhase::ListTools, &hooks.list_tools),
                ] {
                    if let Some(entry) = table.entries.iter().find(|e| e.id.0 == p.hook_id)
                    {
                        found = Some(entry_summary_json(phase, entry, true));
                        break;
                    }
                }
                let json = found.ok_or_else(|| McpError::ToolNotFound {
                    instance: self.instance_id.clone(),
                    tool: format!("hook:{}", p.hook_id),
                })?;
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            other => Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: other.to_string(),
            }),
        }
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

fn phase_table_mut(
    hooks: &mut super::super::hook_table::HookTables,
    phase: HookPhase,
) -> &mut HookTable {
    match phase {
        HookPhase::PreCall => &mut hooks.pre_call,
        HookPhase::PostCall => &mut hooks.post_call,
        HookPhase::OnError => &mut hooks.on_error,
        HookPhase::OnNotification => &mut hooks.on_notification,
        HookPhase::ListTools => &mut hooks.list_tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::policy::InstancePolicy;

    fn call_params(tool: &str, args: serde_json::Value) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(BuiltinHooksServer::INSTANCE),
            tool: tool.to_string(),
            arguments: args,
        }
    }

    /// Exit #4: `hook_add` → `hook_list` → `hook_inspect` → `hook_remove`
    /// round-trip using a Log builtin hook.
    #[tokio::test]
    async fn admin_round_trip_with_builtin_log_hook() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        // hook_add with a Log action.
        let add = broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "match_tool": "*",
                        "hook_id": "my-log",
                        "action": {
                            "type": "log",
                            "level": "info",
                            "target": "kaijutsu::hooks::audit",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!add.is_error);
        assert_eq!(
            add.structured
                .as_ref()
                .and_then(|v| v.get("hook_id"))
                .and_then(|v| v.as_str()),
            Some("my-log"),
        );

        // hook_list with phase filter.
        let list = broker
            .call_tool(
                call_params("hook_list", serde_json::json!({"phase": "pre_call"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let hooks_arr = list
            .structured
            .as_ref()
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap();
        assert_eq!(hooks_arr.len(), 1);
        assert_eq!(
            hooks_arr[0].get("hook_id").and_then(|v| v.as_str()),
            Some("my-log"),
        );

        // hook_inspect returns action detail (level).
        let inspect = broker
            .call_tool(
                call_params("hook_inspect", serde_json::json!({"hook_id": "my-log"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let level = inspect
            .structured
            .as_ref()
            .and_then(|v| v.get("action"))
            .and_then(|a| a.get("level"))
            .and_then(|l| l.as_str())
            .unwrap();
        assert_eq!(level, "info");

        // hook_remove.
        let remove = broker
            .call_tool(
                call_params("hook_remove", serde_json::json!({"hook_id": "my-log"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            remove
                .structured
                .as_ref()
                .and_then(|v| v.get("removed"))
                .and_then(|b| b.as_bool()),
            Some(true),
        );
        // Second remove: idempotent; returns removed=false.
        let again = broker
            .call_tool(
                call_params("hook_remove", serde_json::json!({"hook_id": "my-log"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            again
                .structured
                .as_ref()
                .and_then(|v| v.get("removed"))
                .and_then(|b| b.as_bool()),
            Some(false),
        );
    }

    /// D-50: `BuiltinInvoke` with an unknown name returns `ToolNotFound`.
    #[tokio::test]
    async fn hook_add_unknown_builtin_rejects() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let err = broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "action": {
                            "type": "builtin_invoke",
                            "name": "no_such_hook",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::ToolNotFound { ref tool, .. } if tool.contains("no_such_hook")),
            "expected ToolNotFound(no_such_hook), got {err:?}"
        );
    }

    /// D-56: the `list_tools` phase only admits `Deny` and `Log` actions.
    /// `ShortCircuit` and `BuiltinInvoke` have no coherent list-filter
    /// semantics; reject at add time rather than surprising the caller on
    /// first list-tools evaluation.
    #[tokio::test]
    async fn hook_add_list_tools_rejects_invoke_and_shortcircuit() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        for action in [
            serde_json::json!({ "type": "builtin_invoke", "name": "tracing_audit" }),
            serde_json::json!({ "type": "short_circuit", "result_text": "nope" }),
        ] {
            let err = broker
                .call_tool(
                    call_params(
                        "hook_add",
                        serde_json::json!({
                            "phase": "list_tools",
                            "action": action,
                        }),
                    ),
                    &CallContext::test(),
                    CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, McpError::Unsupported),
                "list_tools should reject action; got {err:?}",
            );
        }
    }

    /// D-56: `Deny` and `Log` are the admitted actions for `list_tools`.
    /// Positive control for the rejection test above — without this we
    /// can't distinguish "rejection works" from "list_tools phase broken."
    #[tokio::test]
    async fn hook_add_list_tools_accepts_deny_and_log() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        for action in [
            serde_json::json!({ "type": "deny", "reason": "no writes" }),
            serde_json::json!({ "type": "log", "level": "info" }),
        ] {
            broker
                .call_tool(
                    call_params(
                        "hook_add",
                        serde_json::json!({
                            "phase": "list_tools",
                            "action": action,
                        }),
                    ),
                    &CallContext::test(),
                    CancellationToken::new(),
                )
                .await
                .expect("list_tools + Deny/Log must be accepted");
        }

        // Confirm the entries landed in the list_tools table specifically.
        let hooks = broker.hooks().read().await;
        assert_eq!(hooks.list_tools.entries.len(), 2);
        assert!(hooks.pre_call.entries.is_empty());
    }

    /// D-50: `Kaish` bodies are rejected at add time with `Unsupported`.
    #[tokio::test]
    async fn hook_add_kaish_rejects() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let err = broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "action": {
                            "type": "kaish",
                            "script_id": "some-script",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Unsupported),
            "expected Unsupported, got {err:?}"
        );
    }

    /// `hook_list` with a phase filter returns only that phase.
    #[tokio::test]
    async fn hook_list_filters_by_phase() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        for (phase, id) in [
            ("pre_call", "pre-1"),
            ("post_call", "post-1"),
            ("on_error", "err-1"),
            ("on_notification", "notif-1"),
        ] {
            broker
                .call_tool(
                    call_params(
                        "hook_add",
                        serde_json::json!({
                            "phase": phase,
                            // Narrow match so these Deny hooks don't intercept
                            // the admin server's own calls (admin goes
                            // through broker.call_tool too).
                            "match_instance": "not-a-real-instance",
                            "hook_id": id,
                            "action": { "type": "deny", "reason": "x" },
                        }),
                    ),
                    &CallContext::test(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let list = broker
            .call_tool(
                call_params("hook_list", serde_json::json!({"phase": "on_error"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let arr = list
            .structured
            .as_ref()
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("hook_id").and_then(|v| v.as_str()), Some("err-1"));

        let all = broker
            .call_tool(
                call_params("hook_list", serde_json::json!({})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let all_arr = all
            .structured
            .as_ref()
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap();
        assert_eq!(all_arr.len(), 4);
    }

    /// `hook_inspect` returns action detail that `hook_list` redacts.
    #[tokio::test]
    async fn hook_inspect_returns_body_detail() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();
        // Add a ShortCircuit hook. `hook_list` redacts `result_text`;
        // `hook_inspect` must return it. Narrow to a fictional instance
        // so the hook doesn't intercept admin calls below.
        broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "match_instance": "not-a-real-instance",
                        "hook_id": "sc",
                        "action": {
                            "type": "short_circuit",
                            "result_text": "from hook",
                            "is_error": false,
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // list: no result_text.
        let list = broker
            .call_tool(
                call_params("hook_list", serde_json::json!({})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let list_action = list
            .structured
            .as_ref()
            .and_then(|v| v.get("hooks"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|e| e.get("action"))
            .cloned()
            .unwrap();
        assert!(
            list_action.get("result_text").is_none(),
            "hook_list must redact result_text; got {list_action:?}"
        );

        // inspect: has result_text.
        let inspect = broker
            .call_tool(
                call_params("hook_inspect", serde_json::json!({"hook_id": "sc"})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let rt = inspect
            .structured
            .as_ref()
            .and_then(|v| v.get("action"))
            .and_then(|a| a.get("result_text"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(rt, "from hook");
    }

    /// D-51 retired: `builtin.hooks` is subject to hook evaluation like
    /// every other instance. Symmetric to
    /// `bindings_server_subject_to_hooks` (Phase 5). Recovery from a
    /// self-inflicted lockout is out-of-band (edit the persisted row,
    /// restart) — the kernel does not self-guard.
    #[tokio::test]
    async fn hooks_admin_is_subject_to_hooks() {
        use super::super::super::hook_table::{GlobPattern, HookAction, HookEntry};
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        // Install a PreCall Deny(*) directly on the broker's HookTables,
        // bypassing the admin surface. This simulates the user-locked-out
        // state before retirement would have been recoverable only via
        // the carve-out. After retirement, `hook_list` on `builtin.hooks`
        // must return `McpError::Denied`.
        {
            let mut hooks = broker.hooks().write().await;
            hooks.pre_call.entries.push(HookEntry {
                id: HookId("lockout".into()),
                match_instance: Some(GlobPattern("*".into())),
                match_tool: None,
                match_context: None,
                match_principal: None,
                action: HookAction::Deny("locked out".into()),
                priority: 0,
            });
            drop(hooks);
        }

        let err = broker
            .call_tool(
                call_params("hook_list", serde_json::json!({})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, McpError::Denied { by_hook } if by_hook.0 == "lockout"),
            "expected Denied(lockout) after D-51 retirement, got {err:?}",
        );
    }

    /// `hook_remove` on an unknown id returns `{ removed: false }` — no
    /// error, idempotent cleanup.
    #[tokio::test]
    async fn hook_remove_missing_is_not_an_error() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let result = broker
            .call_tool(
                call_params(
                    "hook_remove",
                    serde_json::json!({"hook_id": "does-not-exist"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result
                .structured
                .as_ref()
                .and_then(|v| v.get("removed"))
                .and_then(|b| b.as_bool()),
            Some(false),
        );
    }
}
