//! Hook and stored-script administration through the MCP broker.
//!
//! Hook tools delegate to `Broker`'s tables:
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
//! Registered silently at kernel bootstrap.

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
use super::super::params::decode_params;
use super::super::hook_table::{
    AskSpec, GlobPattern, HookAction, HookBody, HookEntry, McpHookPhase, HookTable, LogSpec,
};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{
    InstanceId, KernelCallParams, KernelTool, KernelToolResult, ToolContent,
};
use kaijutsu_types::{ContextId, PrincipalId};

/// Action to take when a hook matches. Executable bodies share the invoking
/// context and identity; list discovery supports only denial and logging.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookActionWire {
    /// Resolve a named builtin hook body from the broker registry.
    BuiltinInvoke { name: String },
    /// Run inline kaish source when this hook fires. Exit 0 continues; exit 3
    /// asks for approval using the last 512 stderr characters. Exit 124 or a
    /// runtime fault also asks for approval; other nonzero exits deny.
    /// Owner cancellation stops evaluation and waits for cleanup without
    /// creating a new ask. Control characters in diagnostics are escaped.
    Kaish { body: String },
    /// Snapshot a stored script when adding the hook. Later edits to that
    /// script do not change installed hooks; re-add the hook to take a new
    /// snapshot. Execution follows the inline kaish exit contract.
    KaishScript { script_id: String },
    /// Read kaish source from this VFS path whenever the hook fires.
    /// Editing the file affects later invocations. The path need not exist
    /// when installing the hook; an unreadable path denies at invocation.
    KaishPath { path: String },
    /// Return a synthetic result in lieu of calling the server.
    ShortCircuit {
        result_text: String,
        is_error: Option<bool>,
    },
    /// Stop the phase and return this reason to the caller.
    Deny { reason: String },
    /// Emit a tracing event at this level without writing a block.
    Log {
        target: Option<String>,
        level: String,
    },
    /// Ask for approval before continuing this phase. An omitted description
    /// uses the instance and tool name. An unavailable approval service is
    /// reported as a control failure; a rejected answer is reported as a denial.
    Ask { description: Option<String> },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookAddParams {
    /// `pre_call`, `post_call`, `on_error`, `on_notification`, or `list_tools`.
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
    /// Filter to a single phase; omit for all phases.
    pub phase: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookInspectParams {
    pub hook_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookScriptAddParams {
    /// Caller-supplied id; else a UUID v4 is generated. Must be unique
    /// per kernel.
    pub script_id: Option<String>,
    /// Inline kaish source body.
    pub body: String,
    /// Optional human-readable description (shown in `hook_script_list`).
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookScriptUpdateParams {
    pub script_id: String,
    pub body: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookScriptInspectParams {
    pub script_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HookScriptRemoveParams {
    pub script_id: String,
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

fn parse_phase(s: &str) -> McpResult<McpHookPhase> {
    match s {
        "pre_call" => Ok(McpHookPhase::PreCall),
        "post_call" => Ok(McpHookPhase::PostCall),
        "on_error" => Ok(McpHookPhase::OnError),
        "on_notification" => Ok(McpHookPhase::OnNotification),
        "list_tools" => Ok(McpHookPhase::ListTools),
        other => Err(McpError::Protocol(format!(
            "unknown hook phase: {other:?}"
        ))),
    }
}

fn phase_to_str(phase: McpHookPhase) -> &'static str {
    match phase {
        McpHookPhase::PreCall => "pre_call",
        McpHookPhase::PostCall => "post_call",
        McpHookPhase::OnError => "on_error",
        McpHookPhase::OnNotification => "on_notification",
        McpHookPhase::ListTools => "list_tools",
    }
}

/// List discovery supports logging and denial. It cannot execute a body,
/// return a call result, or wait for an approval answer per tool.
fn validate_action_for_phase(phase: McpHookPhase, action: &HookActionWire) -> McpResult<()> {
    if phase == McpHookPhase::ListTools {
        match action {
            HookActionWire::BuiltinInvoke { .. }
            | HookActionWire::ShortCircuit { .. }
            | HookActionWire::Kaish { .. }
            | HookActionWire::KaishScript { .. }
            | HookActionWire::KaishPath { .. }
            | HookActionWire::Ask { .. } => return Err(McpError::Unsupported),
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

/// Parse an admin-wire `HookActionWire` into a `(HookAction,
/// kaish_script_id)` pair. The second element is `Some(script_id)` when
/// the wire variant is `KaishScript` so the caller can tag the resulting
/// `HookEntry` for persistence; `None` for every other variant.
///
/// `KaishScript` resolves the script body via the broker so the returned
/// `HookAction::Invoke(HookBody::Kaish(body))` is runnable immediately —
/// there's no second resolution at fire time.
async fn build_hook_action(
    broker: &Arc<Broker>,
    action: HookActionWire,
    instance: &InstanceId,
) -> McpResult<(HookAction, Option<String>)> {
    Ok(match action {
        HookActionWire::BuiltinInvoke { name } => {
            let hook = broker
                .builtin_hooks()
                .build(&name)
                .ok_or_else(|| McpError::ToolNotFound {
                    instance: instance.clone(),
                    tool: format!("builtin:{name}"),
                })?;
            (HookAction::Invoke(HookBody::Builtin { name, hook }), None)
        }
        HookActionWire::Kaish { body } => {
            (HookAction::Invoke(HookBody::Kaish(body)), None)
        }
        HookActionWire::KaishScript { script_id } => {
            let body = broker
                .get_hook_script_body(&script_id)
                .await
                .ok_or_else(|| {
                    McpError::Protocol(format!(
                        "hook script '{script_id}' not found; create it with hook_script_add first"
                    ))
                })?;
            (
                HookAction::Invoke(HookBody::Kaish(body)),
                Some(script_id),
            )
        }
        HookActionWire::KaishPath { path } => {
            (HookAction::Invoke(HookBody::KaishPath(path)), None)
        }
        HookActionWire::ShortCircuit {
            result_text,
            is_error,
        } => (
            HookAction::ShortCircuit(KernelToolResult {
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
        HookActionWire::Ask { description } => {
            (HookAction::Ask(AskSpec { description }), None)
        }
    })
}

/// JSON summary of one entry (list output; body detail for inspect).
fn entry_summary_json(phase: McpHookPhase, entry: &HookEntry, full: bool) -> serde_json::Value {
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
        HookAction::Invoke(HookBody::KaishPath(path)) => {
            serde_json::json!({ "type": "kaish_path", "path": path })
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
        let script_add_schema = schemars::schema_for!(HookScriptAddParams);
        let script_update_schema = schemars::schema_for!(HookScriptUpdateParams);
        let script_inspect_schema = schemars::schema_for!(HookScriptInspectParams);
        let script_remove_schema = schemars::schema_for!(HookScriptRemoveParams);
        let add_val = serde_json::to_value(&add_schema).map_err(McpError::InvalidParams)?;
        let remove_val =
            serde_json::to_value(&remove_schema).map_err(McpError::InvalidParams)?;
        let list_val = serde_json::to_value(&list_schema).map_err(McpError::InvalidParams)?;
        let inspect_val =
            serde_json::to_value(&inspect_schema).map_err(McpError::InvalidParams)?;
        let script_add_val =
            serde_json::to_value(&script_add_schema).map_err(McpError::InvalidParams)?;
        let script_update_val =
            serde_json::to_value(&script_update_schema).map_err(McpError::InvalidParams)?;
        let script_inspect_val =
            serde_json::to_value(&script_inspect_schema).map_err(McpError::InvalidParams)?;
        let script_remove_val =
            serde_json::to_value(&script_remove_schema).map_err(McpError::InvalidParams)?;
        // hook_script_list takes no params; advertise an empty object schema.
        let script_list_val = serde_json::json!({"type": "object", "properties": {}});
        Ok(vec![
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_add".to_string(),
                description: Some(
                    "Register a hook entry on the named phase table (pre_call / \
                     post_call / on_error / on_notification / list_tools)."
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
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_script_add".to_string(),
                description: Some(
                    "Store kaish source for hooks to snapshot. Add a hook with \
                     action type `kaish_script` and this `script_id`."
                        .to_string(),
                ),
                input_schema: script_add_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_script_update".to_string(),
                description: Some(
                    "Replace stored kaish source. Existing hooks keep their installed \
                     snapshot, including after restart. Remove and re-add a hook \
                     to use the updated source."
                        .to_string(),
                ),
                input_schema: script_update_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_script_list".to_string(),
                description: Some(
                    "List shared kaish hook scripts (id + description + \
                     body length)."
                        .to_string(),
                ),
                input_schema: script_list_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_script_inspect".to_string(),
                description: Some(
                    "Return the full body of one shared hook script.".to_string(),
                ),
                input_schema: script_inspect_val,
            },
            KernelTool {
                instance: self.instance_id.clone(),
                name: "hook_script_remove".to_string(),
                description: Some(
                    "Delete a shared hook script. Refuses if any hook \
                     still references it; drop the referencing hooks \
                     first."
                        .to_string(),
                ),
                input_schema: script_remove_val,
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
                let p: HookAddParams = decode_params(params.arguments.clone())?;
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
                let (action, kaish_script_id) =
                    build_hook_action(&broker, p.action, &self.instance_id).await?;
                let id = p
                    .hook_id
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let entry = HookEntry {
                    id: HookId(id.clone()),
                    match_instance: p.match_instance.map(GlobPattern),
                    match_tool: p.match_tool.map(GlobPattern),
                    match_context,
                    match_principal,
                    kaish_script_id,
                    action,
                    priority: p.priority.unwrap_or(0),
                };
                // Durable store FIRST. A failed write is a major error the
                // operator must see — never a silent warn — and the in-memory
                // mirror stays untouched so it cannot diverge from what
                // survives a restart. No DB wired (tests / early bootstrap)
                // is `Ok`: in-memory is authoritative there.
                broker.persist_hook_insert(phase, &entry).await.map_err(|e| {
                    McpError::Protocol(format!(
                        "hook_add: failed to persist hook {id}: {e}"
                    ))
                })?;
                {
                    let mut hooks = broker.hooks().write().await;
                    let table = phase_table_mut(&mut hooks, phase);
                    table.entries.push(entry.clone());
                }
                let json = serde_json::json!({ "hook_id": id });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_remove" => {
                let p: HookRemoveParams = decode_params(params.arguments.clone())?;
                // Durable delete FIRST — same contract as `hook_add`: a
                // failed write surfaces as an error and the mirror is left
                // alone so the two stores cannot disagree. Idempotent, so a
                // delete of an unknown id is `Ok` and callers may retry.
                broker.persist_hook_delete(&p.hook_id).await.map_err(|e| {
                    McpError::Protocol(format!(
                        "hook_remove: failed to persist delete of {}: {e}",
                        p.hook_id
                    ))
                })?;
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
                let json = serde_json::json!({ "removed": removed });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_list" => {
                let p: HookListParams = decode_params(params.arguments.clone())?;
                let filter_phase = p.phase.as_deref().map(parse_phase).transpose()?;
                let hooks = broker.hooks().read().await;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for (phase, table) in [
                    (McpHookPhase::PreCall, &hooks.pre_call),
                    (McpHookPhase::PostCall, &hooks.post_call),
                    (McpHookPhase::OnError, &hooks.on_error),
                    (McpHookPhase::OnNotification, &hooks.on_notification),
                    (McpHookPhase::ListTools, &hooks.list_tools),
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
                let p: HookInspectParams = decode_params(params.arguments.clone())?;
                let hooks = broker.hooks().read().await;
                let mut found: Option<serde_json::Value> = None;
                for (phase, table) in [
                    (McpHookPhase::PreCall, &hooks.pre_call),
                    (McpHookPhase::PostCall, &hooks.post_call),
                    (McpHookPhase::OnError, &hooks.on_error),
                    (McpHookPhase::OnNotification, &hooks.on_notification),
                    (McpHookPhase::ListTools, &hooks.list_tools),
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
            "hook_script_add" => {
                let p: HookScriptAddParams = decode_params(params.arguments.clone())?;
                let script_id = p
                    .script_id
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let now = kaijutsu_types::now_millis() as i64;
                let row = crate::kernel_db::HookScriptRow {
                    script_id: script_id.clone(),
                    body: p.body,
                    description: p.description,
                    created_at: now,
                    created_by: PrincipalId::system(),
                    updated_at: now,
                };
                broker.insert_hook_script(row).await?;
                let json = serde_json::json!({ "script_id": script_id });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_script_update" => {
                let p: HookScriptUpdateParams = decode_params(params.arguments.clone())?;
                let updated = broker
                    .update_hook_script(&p.script_id, &p.body, p.description.as_deref())
                    .await?;
                if !updated {
                    return Err(McpError::ToolNotFound {
                        instance: self.instance_id.clone(),
                        tool: format!("hook_script:{}", p.script_id),
                    });
                }
                let json = serde_json::json!({ "updated": true });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_script_list" => {
                let scripts = broker.list_hook_scripts().await?;
                let out: Vec<serde_json::Value> = scripts
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "script_id": s.script_id,
                            "description": s.description,
                            "body_len": s.body.len(),
                            "created_at": s.created_at,
                            "updated_at": s.updated_at,
                        })
                    })
                    .collect();
                let json = serde_json::json!({ "scripts": out });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_script_inspect" => {
                let p: HookScriptInspectParams = decode_params(params.arguments.clone())?;
                let body = broker
                    .get_hook_script_body(&p.script_id)
                    .await
                    .ok_or_else(|| McpError::ToolNotFound {
                        instance: self.instance_id.clone(),
                        tool: format!("hook_script:{}", p.script_id),
                    })?;
                let json = serde_json::json!({
                    "script_id": p.script_id,
                    "body": body,
                });
                Ok(KernelToolResult {
                    is_error: false,
                    content: vec![ToolContent::Json(json.clone())],
                    structured: Some(json),
                })
            }
            "hook_script_remove" => {
                let p: HookScriptRemoveParams = decode_params(params.arguments.clone())?;
                let removed = broker.delete_hook_script(&p.script_id).await?;
                let json = serde_json::json!({ "removed": removed });
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
    phase: McpHookPhase,
) -> &mut HookTable {
    match phase {
        McpHookPhase::PreCall => &mut hooks.pre_call,
        McpHookPhase::PostCall => &mut hooks.post_call,
        McpHookPhase::OnError => &mut hooks.on_error,
        McpHookPhase::OnNotification => &mut hooks.on_notification,
        McpHookPhase::ListTools => &mut hooks.list_tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::policy::InstancePolicy;
    use kaijutsu_types::RefusalKind;

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

    /// Kaish bodies install successfully — `script_id` carries the inline
    /// kaish source. (Schema column is named `action_kaish_script_id` for
    /// historical reasons; today it holds the body itself.) Evaluation
    /// requires `Broker::set_kernel`; without that wired, fire-time
    /// returns a kaish-cannot-run error which the broker maps to Deny.
    #[tokio::test]
    async fn hook_add_kaish_installs() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let res = broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "action": {
                            "type": "kaish",
                            "body": "exit 0",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("hook_add should accept kaish bodies");
        assert!(!res.is_error, "hook_add reported failure: {res:?}");

        let hooks = broker.hooks().read().await;
        assert_eq!(hooks.pre_call.entries.len(), 1);
    }

    /// `ListTools` phase still rejects Kaish — list-filter bodies have no
    /// coherent execution semantics (D-56 keeps this rejection alongside
    /// `Invoke`/`ShortCircuit`).
    #[tokio::test]
    async fn hook_add_kaish_rejected_for_list_tools() {
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
                        "phase": "list_tools",
                        "action": {
                            "type": "kaish",
                            "body": "exit 0",
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
            "expected Unsupported for list_tools+kaish, got {err:?}"
        );
    }

    /// D-56/D-57: `Ask` has no coherent list-filter semantics either — a
    /// list-filter can't block-wait per tool. Same rejection shape as
    /// `Kaish`.
    #[tokio::test]
    async fn hook_add_ask_rejected_for_list_tools() {
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
                        "phase": "list_tools",
                        "action": {
                            "type": "ask",
                            "description": "should never install",
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
            "expected Unsupported for list_tools+ask, got {err:?}"
        );
    }

    /// D-57: `hook_add` → `hook_list` → `hook_inspect` → `hook_remove`
    /// round-trip using an `Ask` action, mirroring
    /// `admin_round_trip_with_builtin_log_hook`. Exercises the wire
    /// surface an rc script / hook config uses to declare
    /// `HookAction::Ask` — the description round-trips through the admin
    /// JSON, not just the DB row (that's `kernel_db`'s
    /// `hook_insert_roundtrip_preserves_all_action_variants`).
    #[tokio::test]
    async fn hook_add_ask_installs_and_round_trips() {
        let broker = Arc::new(Broker::new());
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let add = broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "match_tool": "shell.exec",
                        "hook_id": "confirm-shell",
                        "action": {
                            "type": "ask",
                            "description": "about to run a shell command",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!add.is_error);

        let inspect = broker
            .call_tool(
                call_params(
                    "hook_inspect",
                    serde_json::json!({"hook_id": "confirm-shell"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let action = inspect
            .structured
            .as_ref()
            .and_then(|v| v.get("action"))
            .cloned()
            .unwrap();
        assert_eq!(action.get("type").and_then(|v| v.as_str()), Some("ask"));
        assert_eq!(
            action.get("description").and_then(|v| v.as_str()),
            Some("about to run a shell command"),
        );

        // The installed hook is a live `HookAction::Ask` in the broker's
        // pre_call table, not just admin-surface JSON.
        let hooks = broker.hooks().read().await;
        let entry = hooks
            .pre_call
            .entries
            .iter()
            .find(|e| e.id.0 == "confirm-shell")
            .unwrap();
        match &entry.action {
            HookAction::Ask(spec) => {
                assert_eq!(spec.description.as_deref(), Some("about to run a shell command"));
            }
            other => panic!("expected HookAction::Ask, got {other:?}"),
        }
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
        // must return a `RefusalKind::Denied` refusal.
        {
            let mut hooks = broker.hooks().write().await;
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
            err.is_refusal_from(RefusalKind::Denied, "lockout"),
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

    // ── hook_scripts admin flow ────────────────────────────────────

    fn broker_with_db() -> Arc<Broker> {
        use crate::kernel_db::KernelDb;
        let broker = Arc::new(Broker::new());
        let db: crate::block_store::DbHandle =
            Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let broker_clone = broker.clone();
        // set_db is async; resolve synchronously via a one-shot block_on
        // — these tests are already inside `tokio::test`, but the
        // helper itself isn't async. Grafted via `tokio::runtime::Handle`.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                broker_clone.set_db(db).await;
            })
        });
        broker
    }

    /// A `hook_add` whose durable write fails must (1) surface the error to
    /// the caller and (2) leave the in-memory mirror untouched — otherwise
    /// the running kernel would carry a hook that silently vanishes on the
    /// next restart. Forced with a duplicate `hook_id`, which the PK rejects
    /// on the second insert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_add_persist_failure_leaves_mirror_untouched() {
        let broker = broker_with_db();
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let add = |id: &str| {
            call_params(
                "hook_add",
                serde_json::json!({
                    "phase": "pre_call",
                    "match_tool": "*",
                    "hook_id": id,
                    "action": {
                        "type": "log",
                        "level": "info",
                        "target": "kaijutsu::hooks::audit",
                    },
                }),
            )
        };

        // First add lands in both stores.
        broker
            .call_tool(add("dup"), &CallContext::test(), CancellationToken::new())
            .await
            .expect("first hook_add succeeds");

        // Second add with the same id: the persist hits a PK conflict.
        let err = broker
            .call_tool(add("dup"), &CallContext::test(), CancellationToken::new())
            .await
            .expect_err("duplicate persist must surface as an error, not Ok");
        assert!(
            matches!(&err, McpError::Protocol(m) if m.contains("persist")),
            "expected a persist-failure Protocol error, got {err:?}",
        );

        // The mirror still holds exactly one entry — the failed write did
        // not push a phantom second copy that a restart would drop.
        let hooks = broker.hooks().read().await;
        let dup_count = hooks
            .pre_call
            .entries
            .iter()
            .filter(|e| e.id.0 == "dup")
            .count();
        assert_eq!(dup_count, 1, "mirror must not diverge from the DB on a failed write");
    }

    /// `hook_script_add` → `hook_script_inspect` → `hook_script_list`
    /// — verify the body round-trips and `body_len` is reported in
    /// list output.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_script_admin_round_trip() {
        let broker = broker_with_db();
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        let add = broker
            .call_tool(
                call_params(
                    "hook_script_add",
                    serde_json::json!({
                        "script_id": "audit-passthrough",
                        "body": "echo audited; exit 0",
                        "description": "Mark every call as audited.",
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("hook_script_add should succeed");
        assert_eq!(
            add.structured
                .as_ref()
                .and_then(|v| v.get("script_id"))
                .and_then(|v| v.as_str()),
            Some("audit-passthrough"),
        );

        let inspect = broker
            .call_tool(
                call_params(
                    "hook_script_inspect",
                    serde_json::json!({"script_id": "audit-passthrough"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("hook_script_inspect should succeed");
        assert_eq!(
            inspect
                .structured
                .as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|v| v.as_str()),
            Some("echo audited; exit 0"),
        );

        let list = broker
            .call_tool(
                call_params("hook_script_list", serde_json::json!({})),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let arr = list
            .structured
            .as_ref()
            .and_then(|v| v.get("scripts"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("body_len").and_then(|v| v.as_u64()), Some(20));
    }

    /// `hook_add` with a `kaish_script` action resolves the script body
    /// at add time and tags the live entry with `kaish_script_id` so
    /// persistence can write it to `action_kaish_script_id`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_add_kaish_script_resolves_and_tags_entry() {
        let broker = broker_with_db();
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_script_add",
                    serde_json::json!({
                        "script_id": "shared-pass",
                        "body": "exit 0",
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "action": {
                            "type": "kaish_script",
                            "script_id": "shared-pass",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .expect("hook_add with kaish_script should succeed");

        let hooks = broker.hooks().read().await;
        let entry = hooks.pre_call.entries.first().expect("entry installed");
        assert_eq!(entry.kaish_script_id.as_deref(), Some("shared-pass"));
        match &entry.action {
            HookAction::Invoke(HookBody::Kaish(body)) => assert_eq!(body, "exit 0"),
            other => panic!("expected Invoke(Kaish), got {other:?}"),
        }
    }

    /// Referencing a script that doesn't exist is a clear error at
    /// add time (rather than a silent install that fires-then-fails).
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_add_kaish_script_unknown_id_fails_fast() {
        let broker = broker_with_db();
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
                            "type": "kaish_script",
                            "script_id": "nope",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Protocol(ref msg) if msg.contains("nope")),
            "expected Protocol error mentioning the script_id, got {err:?}",
        );
    }

    /// Snapshot semantics: editing a `hook_script` body after a hook
    /// has been installed from it does NOT propagate to the live
    /// entry. The body is captured at hook_add time; updates affect
    /// only future installs (per
    /// `feedback_script_snapshot_on_instantiation`). The originating
    /// `script_id` stays as provenance on the entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_script_update_does_not_propagate_to_existing_hooks() {
        let broker = broker_with_db();
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_script_add",
                    serde_json::json!({"script_id": "snap", "body": "exit 0"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        "match_instance": "svc",
                        "action": {
                            "type": "kaish_script",
                            "script_id": "snap",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Mutate the source script — should NOT affect the live entry.
        broker
            .call_tool(
                call_params(
                    "hook_script_update",
                    serde_json::json!({"script_id": "snap", "body": "exit 99"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let hooks = broker.hooks().read().await;
        let entry = hooks.pre_call.entries.first().expect("entry installed");
        match &entry.action {
            HookAction::Invoke(HookBody::Kaish(body)) => {
                assert_eq!(
                    body, "exit 0",
                    "snapshot must survive hook_script_update; got {body:?}"
                );
            }
            other => panic!("expected Invoke(Kaish), got {other:?}"),
        }
        assert_eq!(
            entry.kaish_script_id.as_deref(),
            Some("snap"),
            "provenance preserved",
        );
    }

    /// `hook_script_remove` refuses to delete a script that's still
    /// referenced by a persisted hook — operators must drop the
    /// referencing hooks first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn hook_script_remove_refuses_when_referenced() {
        let broker = broker_with_db();
        let server = Arc::new(BuiltinHooksServer::new(Arc::downgrade(&broker)));
        broker
            .register(server, InstancePolicy::default())
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_script_add",
                    serde_json::json!({"script_id": "needed", "body": "exit 0"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        broker
            .call_tool(
                call_params(
                    "hook_add",
                    serde_json::json!({
                        "phase": "pre_call",
                        // Scope to a non-admin instance so this hook
                        // doesn't fire on the `hook_script_remove`
                        // call below — that admin path is what we're
                        // testing, and a non-wired kaish body would
                        // produce a spurious Deny.
                        "match_instance": "svc",
                        "action": {
                            "type": "kaish_script",
                            "script_id": "needed",
                        },
                    }),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let err = broker
            .call_tool(
                call_params(
                    "hook_script_remove",
                    serde_json::json!({"script_id": "needed"}),
                ),
                &CallContext::test(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Protocol(ref msg) if msg.contains("referenced by")),
            "expected Protocol error explaining the reference count, got {err:?}",
        );
    }
}
