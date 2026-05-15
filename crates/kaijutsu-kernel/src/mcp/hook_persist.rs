//! Serialization bridge between live `HookEntry` (runtime, with
//! `Arc<dyn Hook>` bodies) and `HookRow` (DB row, pure data).
//!
//! Callers:
//! - `Broker::persist_hook_insert` → `entry_to_row` (lossy for
//!   `Arc<dyn Hook>`; stores only the builtin name).
//! - `Broker::hydrate_hooks_from_db` → `row_to_entry`, which resolves
//!   `action_builtin_name` against `BuiltinHookRegistry` to get a fresh
//!   `Arc<dyn Hook>`. Rows with an unknown name return
//!   `Err(RowParseError::UnknownBuiltin(..))` so the caller can
//!   `tracing::warn!` + skip.
//!
//! Phase / tracing-level string conversions are also here so both the
//! persist path and the admin surface share one source of truth.

use super::error::HookId;
use super::hook_table::{
    GlobPattern, HookAction, HookBody, HookEntry, HookPhase, LogSpec,
};
use super::hooks_builtin::BuiltinHookRegistry;
use super::types::{KernelToolResult, ToolContent};
use crate::kernel_db::HookRow;

pub const ACTION_BUILTIN_INVOKE: &str = "builtin_invoke";
pub const ACTION_KAISH_INVOKE: &str = "kaish_invoke";
pub const ACTION_SHORT_CIRCUIT: &str = "shortcircuit";
pub const ACTION_DENY: &str = "deny";
pub const ACTION_LOG: &str = "log";

pub fn phase_to_str(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::PreCall => "pre_call",
        HookPhase::PostCall => "post_call",
        HookPhase::OnError => "on_error",
        HookPhase::OnNotification => "on_notification",
        HookPhase::ListTools => "list_tools",
    }
}

pub fn parse_phase(s: &str) -> Option<HookPhase> {
    match s {
        "pre_call" => Some(HookPhase::PreCall),
        "post_call" => Some(HookPhase::PostCall),
        "on_error" => Some(HookPhase::OnError),
        "on_notification" => Some(HookPhase::OnNotification),
        "list_tools" => Some(HookPhase::ListTools),
        _ => None,
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

fn parse_level(s: &str) -> Option<tracing::Level> {
    match s.to_ascii_lowercase().as_str() {
        "trace" => Some(tracing::Level::TRACE),
        "debug" => Some(tracing::Level::DEBUG),
        "info" => Some(tracing::Level::INFO),
        "warn" | "warning" => Some(tracing::Level::WARN),
        "error" => Some(tracing::Level::ERROR),
        _ => None,
    }
}

/// Encode a live `HookEntry` into the persistable `HookRow` shape. Lossy
/// for `HookBody::Builtin`: the `Arc<dyn Hook>` is dropped; only the
/// registry name is stored. Lossy for `ShortCircuit`'s `structured`
/// field (MCP-side JSON) — persisted hooks do not carry structured
/// content, only `result_text` + `is_error`.
pub fn entry_to_row(phase: HookPhase, entry: &HookEntry) -> HookRow {
    let phase_str = phase_to_str(phase).to_string();
    let match_instance = entry.match_instance.as_ref().map(|g| g.0.clone());
    let match_tool = entry.match_tool.as_ref().map(|g| g.0.clone());
    let match_context = entry.match_context;
    let match_principal = entry.match_principal;

    let mut row = HookRow {
        hook_id: entry.id.0.clone(),
        phase: phase_str,
        priority: entry.priority,
        match_instance,
        match_tool,
        match_context,
        match_principal,
        action_kind: String::new(),
        action_builtin_name: None,
        action_kaish_body: None,
        action_kaish_script_id: None,
        action_result_text: None,
        action_is_error: None,
        action_deny_reason: None,
        action_log_target: None,
        action_log_level: None,
    };

    match &entry.action {
        HookAction::Invoke(HookBody::Builtin { name, .. }) => {
            row.action_kind = ACTION_BUILTIN_INVOKE.into();
            row.action_builtin_name = Some(name.clone());
        }
        HookAction::Invoke(HookBody::Kaish(body)) => {
            row.action_kind = ACTION_KAISH_INVOKE.into();
            // Snapshot-at-instantiation per
            // [[feedback_script_snapshot_on_instantiation]]:
            // ALWAYS persist the resolved body. When the hook came
            // from a shared script, also record the originating
            // `script_id` as provenance — but the body is what fires,
            // not a fresh re-resolution. Editing the script after
            // install does not propagate to existing hooks.
            row.action_kaish_body = Some(body.clone());
            if let Some(script_id) = &entry.kaish_script_id {
                row.action_kaish_script_id = Some(script_id.clone());
            }
        }
        HookAction::ShortCircuit(result) => {
            row.action_kind = ACTION_SHORT_CIRCUIT.into();
            row.action_is_error = Some(result.is_error);
            // Preserve the first Text content chunk as result_text; other
            // content kinds (Json, Image) aren't part of the persisted
            // shape today. A full multi-chunk preservation would need a
            // child table; ShortCircuit bodies in practice are simple
            // text responses.
            row.action_result_text = result
                .content
                .iter()
                .find_map(|c| match c {
                    ToolContent::Text(s) => Some(s.clone()),
                    _ => None,
                });
        }
        HookAction::Deny(reason) => {
            row.action_kind = ACTION_DENY.into();
            row.action_deny_reason = Some(reason.clone());
        }
        HookAction::Log(spec) => {
            row.action_kind = ACTION_LOG.into();
            row.action_log_target = Some(spec.target.clone());
            row.action_log_level = Some(level_to_str(spec.level).to_string());
        }
    }

    row
}

/// Why a row failed to reconstruct into a live `HookEntry`. Broker logs
/// + skips on any of these at hydrate time.
#[derive(Debug)]
pub enum RowParseError {
    UnknownPhase(String),
    UnknownActionKind(String),
    /// `action_kind = "builtin_invoke"` but no name column was set.
    MissingBuiltinName,
    /// `action_kind = "builtin_invoke"` with a name the registry doesn't
    /// know — either a previously-registered builtin has been removed
    /// or a typo was inserted directly via SQL.
    UnknownBuiltin(String),
    /// `action_kind = "kaish_invoke"` row without `action_kaish_body`
    /// — shape invariant violated. The body is the snapshot that
    /// fires at hook evaluation; `action_kaish_script_id` (provenance)
    /// alone is not sufficient to reconstruct a runnable hook.
    MissingKaishBody,
    /// `action_kind = "shortcircuit"` without a result_text; shape
    /// invariant violated.
    MissingShortCircuitText,
    /// `action_kind = "deny"` without a reason; shape invariant violated.
    MissingDenyReason,
    /// `action_kind = "log"` with an unknown level string.
    UnknownLogLevel(String),
    /// `action_kind = "log"` with no level column set.
    MissingLogFields,
}

impl std::fmt::Display for RowParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RowParseError::UnknownPhase(s) => write!(f, "unknown phase {s:?}"),
            RowParseError::UnknownActionKind(s) => write!(f, "unknown action_kind {s:?}"),
            RowParseError::MissingBuiltinName => f.write_str("builtin_invoke without name"),
            RowParseError::UnknownBuiltin(s) => {
                write!(f, "builtin hook name {s:?} not in registry")
            }
            RowParseError::MissingKaishBody => {
                f.write_str("kaish_invoke row missing action_kaish_body (the body is the snapshot)")
            }
            RowParseError::MissingShortCircuitText => {
                f.write_str("shortcircuit without result_text")
            }
            RowParseError::MissingDenyReason => f.write_str("deny without reason"),
            RowParseError::UnknownLogLevel(s) => write!(f, "unknown log level {s:?}"),
            RowParseError::MissingLogFields => f.write_str("log without level/target"),
        }
    }
}

/// Reconstruct the live `(HookPhase, HookEntry)` pair. Returns
/// `RowParseError` on any shape violation so the caller
/// (`Broker::hydrate_hooks_from_db`) can skip and warn rather than abort
/// the whole hydrate.
pub fn row_to_entry(
    row: &HookRow,
    registry: &BuiltinHookRegistry,
) -> Result<(HookPhase, HookEntry), RowParseError> {
    let phase = parse_phase(&row.phase).ok_or_else(|| RowParseError::UnknownPhase(row.phase.clone()))?;

    let action = match row.action_kind.as_str() {
        ACTION_BUILTIN_INVOKE => {
            let name = row
                .action_builtin_name
                .as_ref()
                .ok_or(RowParseError::MissingBuiltinName)?
                .clone();
            let hook = registry
                .build(&name)
                .ok_or_else(|| RowParseError::UnknownBuiltin(name.clone()))?;
            HookAction::Invoke(HookBody::Builtin { name, hook })
        }
        ACTION_KAISH_INVOKE => {
            // Snapshot-at-instantiation: the body persisted in
            // `action_kaish_body` is the source of truth. When
            // `action_kaish_script_id` is also set, that's provenance
            // metadata — surfaced on `HookEntry.kaish_script_id` so
            // admin tooling can show "this came from script X" — but
            // we do NOT re-resolve the script body here. Edits to the
            // script after install don't leak into existing hooks.
            let body = row
                .action_kaish_body
                .clone()
                .ok_or(RowParseError::MissingKaishBody)?;
            HookAction::Invoke(HookBody::Kaish(body))
        }
        ACTION_SHORT_CIRCUIT => {
            let result_text = row
                .action_result_text
                .clone()
                .ok_or(RowParseError::MissingShortCircuitText)?;
            HookAction::ShortCircuit(KernelToolResult {
                is_error: row.action_is_error.unwrap_or(false),
                content: vec![ToolContent::Text(result_text)],
                structured: None,
            })
        }
        ACTION_DENY => {
            let reason = row
                .action_deny_reason
                .clone()
                .ok_or(RowParseError::MissingDenyReason)?;
            HookAction::Deny(reason)
        }
        ACTION_LOG => {
            let level_str = row
                .action_log_level
                .as_ref()
                .ok_or(RowParseError::MissingLogFields)?;
            let level = parse_level(level_str)
                .ok_or_else(|| RowParseError::UnknownLogLevel(level_str.clone()))?;
            let target = row
                .action_log_target
                .clone()
                .unwrap_or_else(|| "kaijutsu::hooks".to_string());
            HookAction::Log(LogSpec { target, level })
        }
        other => return Err(RowParseError::UnknownActionKind(other.to_string())),
    };

    let entry = HookEntry {
        id: HookId(row.hook_id.clone()),
        match_instance: row.match_instance.as_ref().map(|s| GlobPattern(s.clone())),
        match_tool: row.match_tool.as_ref().map(|s| GlobPattern(s.clone())),
        match_context: row.match_context,
        kaish_script_id: row.action_kaish_script_id.clone(),
        match_principal: row.match_principal,
        action,
        priority: row.priority,
    };
    Ok((phase, entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::KernelToolResult;

    #[test]
    fn builtin_invoke_round_trip() {
        let registry = BuiltinHookRegistry::new();
        // Build a live entry using a real registry name.
        let hook = registry.build("tracing_audit").unwrap();
        let entry = HookEntry {
            id: HookId("rt-builtin".into()),
            match_instance: Some(GlobPattern("builtin.*".into())),
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin {
                name: "tracing_audit".into(),
                hook,
            }),
            priority: 7,
            kaish_script_id: None,
        };
        let row = entry_to_row(HookPhase::PreCall, &entry);
        assert_eq!(row.action_kind, "builtin_invoke");
        assert_eq!(row.action_builtin_name.as_deref(), Some("tracing_audit"));

        let (phase2, entry2) = row_to_entry(&row, &registry).unwrap();
        assert_eq!(phase2, HookPhase::PreCall);
        assert_eq!(entry2.id.0, "rt-builtin");
        assert_eq!(entry2.priority, 7);
        assert_eq!(entry2.match_instance.as_ref().unwrap().0, "builtin.*");
        match entry2.action {
            HookAction::Invoke(HookBody::Builtin { name, .. }) => {
                assert_eq!(name, "tracing_audit");
            }
            other => panic!("expected Invoke(Builtin), got {other:?}"),
        }
    }

    #[test]
    fn unknown_builtin_fails_to_reconstruct() {
        let registry = BuiltinHookRegistry::new();
        // Row points at a builtin that isn't in the registry — hydrate
        // must return an error so the caller can warn + skip.
        let row = HookRow {
            hook_id: "h".into(),
            phase: "pre_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: ACTION_BUILTIN_INVOKE.into(),
            action_builtin_name: Some("removed_hook_name".into()),
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        let err = row_to_entry(&row, &registry).unwrap_err();
        match err {
            RowParseError::UnknownBuiltin(name) => assert_eq!(name, "removed_hook_name"),
            other => panic!("expected UnknownBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn shortcircuit_round_trip() {
        let registry = BuiltinHookRegistry::new();
        let entry = HookEntry {
            id: HookId("sc".into()),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::ShortCircuit(KernelToolResult {
                is_error: true,
                content: vec![ToolContent::Text("synthetic".into())],
                structured: None,
            }),
            priority: 0,
            kaish_script_id: None,
        };
        let row = entry_to_row(HookPhase::OnError, &entry);
        let (_phase, entry2) = row_to_entry(&row, &registry).unwrap();
        match entry2.action {
            HookAction::ShortCircuit(r) => {
                assert!(r.is_error);
                assert!(matches!(r.content.first(), Some(ToolContent::Text(t)) if t == "synthetic"));
            }
            other => panic!("expected ShortCircuit, got {other:?}"),
        }
    }

    #[test]
    fn kaish_row_round_trip() {
        // Kaish rows now reconstruct into a runnable HookBody::Kaish.
        // The persisted column carries the inline body (per the
        // documented wart in `build_hook_action`).
        let registry = BuiltinHookRegistry::new();
        let row = HookRow {
            hook_id: "k".into(),
            phase: "pre_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: ACTION_KAISH_INVOKE.into(),
            action_builtin_name: None,
            action_kaish_body: Some("echo hi".into()),
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        let (_phase, entry) = row_to_entry(&row, &registry).expect("kaish row reconstructs");
        match entry.action {
            HookAction::Invoke(HookBody::Kaish(body)) => assert_eq!(body, "echo hi"),
            other => panic!("expected Invoke(Kaish), got {other:?}"),
        }
    }

    #[test]
    fn kaish_row_missing_body_errors() {
        let registry = BuiltinHookRegistry::new();
        let row = HookRow {
            hook_id: "k".into(),
            phase: "pre_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: ACTION_KAISH_INVOKE.into(),
            action_builtin_name: None,
            action_kaish_body: None,
            action_kaish_script_id: None,
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        assert!(matches!(
            row_to_entry(&row, &registry),
            Err(RowParseError::MissingKaishBody)
        ));
    }

    /// Snapshot semantics: a row with both `action_kaish_body` and
    /// `action_kaish_script_id` set runs the body verbatim and
    /// surfaces the script_id as provenance metadata. The script_id
    /// is NOT re-resolved at hydrate — editing the source script
    /// after install does not propagate (per
    /// `feedback_script_snapshot_on_instantiation`).
    #[test]
    fn kaish_row_with_body_and_script_id_uses_body_keeps_provenance() {
        let registry = BuiltinHookRegistry::new();
        let row = HookRow {
            hook_id: "k".into(),
            phase: "pre_call".into(),
            priority: 0,
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action_kind: ACTION_KAISH_INVOKE.into(),
            action_builtin_name: None,
            // Body deliberately differs from any plausible "current"
            // script body — proves there is no re-resolution.
            action_kaish_body: Some("snapshot-body".into()),
            action_kaish_script_id: Some("audit-1".into()),
            action_result_text: None,
            action_is_error: None,
            action_deny_reason: None,
            action_log_target: None,
            action_log_level: None,
        };
        let (_phase, entry) =
            row_to_entry(&row, &registry).expect("snapshot row reconstructs");
        match entry.action {
            HookAction::Invoke(HookBody::Kaish(body)) => {
                assert_eq!(body, "snapshot-body", "must use snapshot, not re-resolved")
            }
            other => panic!("expected Invoke(Kaish), got {other:?}"),
        }
        assert_eq!(entry.kaish_script_id.as_deref(), Some("audit-1"));
    }

    /// Inline hooks persist body only (no provenance). Script-sourced
    /// hooks persist BOTH the body (snapshot) and the script_id
    /// (provenance). The columns are no longer mutually exclusive.
    #[test]
    fn entry_to_row_writes_body_always_and_script_id_as_provenance() {
        let inline = HookEntry {
            id: HookId("inline".into()),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Kaish("exit 0".into())),
            priority: 0,
            kaish_script_id: None,
        };
        let row = entry_to_row(HookPhase::PreCall, &inline);
        assert_eq!(row.action_kaish_body.as_deref(), Some("exit 0"));
        assert_eq!(row.action_kaish_script_id, None);

        let scripted = HookEntry {
            id: HookId("scripted".into()),
            match_instance: None,
            match_tool: None,
            match_context: None,
            match_principal: None,
            action: HookAction::Invoke(HookBody::Kaish("exit 0".into())),
            priority: 0,
            kaish_script_id: Some("shared-script".into()),
        };
        let row = entry_to_row(HookPhase::PreCall, &scripted);
        assert_eq!(
            row.action_kaish_body.as_deref(),
            Some("exit 0"),
            "snapshot body persisted regardless of script_id presence",
        );
        assert_eq!(
            row.action_kaish_script_id.as_deref(),
            Some("shared-script"),
            "script_id persisted as provenance",
        );
    }
}
