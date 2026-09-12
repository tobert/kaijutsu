//! Builds a [`GateSpec`] for a hook's `Ask` action — the sibling of
//! [`crate::kj::shell_gate`], for the other origin that opens a gate.
//!
//! ## What a hook ask can honestly describe
//!
//! `HookAction::Ask` fires from the PreCall phase of a tool call, so the one
//! thing that exists to show a human is **the call itself**: the instance,
//! the tool, and the arguments as they stand at fire time. That is the
//! single [`GatedStatement`] this builds, and it is the whole of what the
//! ask covers.
//!
//! It does **not** cover what the tool will then do with those arguments.
//! A hook on `builtin.file.edit` shows the path and the replacement text; a
//! hook on a shell-shaped tool shows the command string but cannot tell you
//! what an interpreter inside it will run. That limit is the same one
//! [`crate::kj::shell_gate`] documents at length, and for the same reason:
//! the gate can only be honest about the layer it can actually read.
//!
//! ## Arguments are rendered whole, on purpose
//!
//! The ledger keys a statement by a digest of its rendered text, and a
//! remembered ALLOW rule redeems future asks by that digest. So truncating
//! a long argument blob for display would make two different calls share
//! one identity — and a rule taught for the first would silently redeem the
//! second. A large ask row is a storage cost; a colliding digest is a
//! wrong answer. Render whole.
//!
//! ## A shell-shaped call is source, and is treated as source
//!
//! When the hooked tool is `shell` or `shell_write`, the `command` argument
//! is a kaish program. It is planned here exactly as
//! [`crate::kj::shell_gate`] plans a direct submission, and the ask carries
//! what that plan yields: the command as `exec_source`, so an approval runs
//! it (`docs/gate-shape-b.md`, "Slice 5"); the planned statements, so the
//! gate snapshots the free variables' values; and the free and bound names
//! on the statement, so the ledger refuses to remember an ALLOW rule for a
//! command whose meaning depends on a `${VAR}`. Without that last part a
//! rule taught on `dd of=${DEV}` would redeem every future value of `DEV`.
//!
//! A command that does not parse could not run either; its ask carries no
//! source and its caller retries, the same as before.
//!
//! ## Every other hook ask has no free variables
//!
//! Arguments to any other tool are concrete JSON by the time a hook sees
//! them: nothing there is a `${VAR}` waiting to be substituted. So
//! [`VarBinding`]-driven refusal of allow-always never fires on those, and
//! every one of them is eligible to be remembered. That is a consequence of
//! the shape rather than a policy choice.

use approval_ledger::types::{Origin, VarBinding};

use super::gate::{GateSpec, GatedStatement};
use crate::mcp::types::KernelCallParams;

/// The planned form of a shell-shaped hook call: the command text and its
/// statements. `None` for any other tool, a missing or non-string
/// `command`, or a command that does not parse.
fn shell_source(params: &KernelCallParams) -> Option<(String, Vec<kaish_kernel::PlannedStatement>)> {
    if !matches!(params.tool.as_str(), "shell" | "shell_write") {
        return None;
    }
    let command = params.arguments.get("command")?.as_str()?;
    let planned = kaish_kernel::plan_program(command).ok()?;
    Some((command.trim().to_string(), planned))
}

/// Build the gate ask for one hook `Ask` firing.
///
/// `description` is the hook's own `AskSpec::description` when it set one;
/// the caller supplies the `"{instance}.{tool}"` fallback it already
/// computes, so this function never has to invent one.
pub(crate) fn build_hook_gate_spec(
    hook_id: &str,
    description: String,
    params: &KernelCallParams,
) -> GateSpec {
    let instance = params.instance.as_str().to_string();
    let tool = params.tool.clone();

    // `serde_json` renders object keys in sorted order, so two identical
    // calls render identically and share a digest — which is what makes a
    // remembered rule useful. If that ever changes (the `preserve_order`
    // feature switches maps to insertion order), the failure is that
    // semantically identical calls stop sharing a digest: rules match less
    // often, never wrongly. Conservative in the direction that matters.
    let rendered = format!("{instance}.{tool} {}", params.arguments);

    let (exec_source, planned, vars) = match shell_source(params) {
        Some((source, planned)) => {
            // One statement stands for the whole call, so it carries every
            // name the program reads or binds, deduplicated in first-seen
            // order — the same mapping `shell_gate` applies per statement.
            let mut vars: Vec<(String, VarBinding)> = Vec::new();
            for ps in &planned {
                for name in &ps.plan.free_variables {
                    if !vars.iter().any(|(n, _)| n == name) {
                        vars.push((name.clone(), VarBinding::Free));
                    }
                }
                for name in &ps.plan.bound_variables {
                    if !vars.iter().any(|(n, _)| n == name) {
                        vars.push((name.clone(), VarBinding::Bound));
                    }
                }
            }
            (Some(source), planned, vars)
        }
        None => (None, Vec::new(), Vec::new()),
    };

    let exec_stdin = exec_source.as_ref().and_then(|_| {
        params.arguments.get("stdin").and_then(serde_json::Value::as_str).map(str::to_owned)
    });
    GateSpec {
        origin: Origin::Hook,
        instance,
        tool,
        hook_id: Some(hook_id.to_string()),
        description,
        // The raw typed reference for a hook ask is the tool being called —
        // there is no separate target to resolve, and it is the scope a
        // human would mean by "allow this".
        authorized_label: format!("{}.{}", params.instance.as_str(), params.tool),
        statements: vec![GatedStatement {
            rendered,
            statement_kind: "tool_call".into(),
            vars,
            // No position within a source program: the call is the unit.
            source_index: None,
        }],
        exec_source,
        exec_stdin,
        planned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::InstanceId;

    fn call(tool: &str, arguments: serde_json::Value) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new("builtin.shell_write"),
            tool: tool.to_string(),
            arguments,
        }
    }

    fn free_names(spec: &GateSpec) -> Vec<String> {
        spec.statements[0]
            .vars
            .iter()
            .filter(|(_, b)| matches!(b, VarBinding::Free))
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Falsified by `exec_source: None` for a shell-shaped call: the ask
    /// would say "run the same command again" and the executor would have
    /// nothing to run.
    #[test]
    fn a_shell_write_hook_ask_carries_the_command_as_exec_source() {
        let params = call("shell_write", serde_json::json!({ "command": " dd if=/dev/zero of=${DEV} \n" }));
        let spec = build_hook_gate_spec("lfm2d-advisory", "d".into(), &params);
        assert_eq!(spec.exec_source.as_deref(), Some("dd if=/dev/zero of=${DEV}"));
        assert_eq!(spec.planned.len(), 1, "the planned statements ride the spec for the env snapshot");
    }

    #[test]
    fn a_shell_hook_ask_captures_separate_stdin() {
        let params = call("shell_write", serde_json::json!({
            "command": "cat", "stdin": "exact input\n",
        }));
        let spec = build_hook_gate_spec("review", "review input".into(), &params);
        assert_eq!(spec.exec_stdin.as_deref(), Some("exact input\n"));
        assert!(spec.statements[0].rendered.contains("exact input"));
    }

    /// Falsified by `vars: vec![]` on the statement: the ledger's refusal of
    /// an ALLOW rule over a free variable would never fire for a hook ask,
    /// and a rule taught on one value of `DEV` would redeem every other.
    #[test]
    fn a_shell_write_hook_ask_names_its_free_variables() {
        let params = call(
            "shell_write",
            serde_json::json!({ "command": "dd if=/dev/zero of=${DEV} && echo \"${FOO}-${DEV}\"" }),
        );
        let spec = build_hook_gate_spec("h", "d".into(), &params);
        assert_eq!(free_names(&spec), vec!["DEV".to_string(), "FOO".to_string()]);
    }

    /// Only a shell-shaped tool has source. Falsified by planning every
    /// tool's `command` argument regardless of the tool.
    #[test]
    fn a_non_shell_hook_ask_carries_no_source_and_no_variables() {
        let params = call("edit", serde_json::json!({ "command": "echo ${FOO}", "path": "/x" }));
        let spec = build_hook_gate_spec("h", "d".into(), &params);
        assert_eq!(spec.exec_source, None);
        assert!(spec.planned.is_empty());
        assert!(spec.statements[0].vars.is_empty());
    }

    /// A command that does not parse could not run; its ask keeps the retry
    /// shape rather than promising an execution that cannot happen.
    #[test]
    fn an_unparseable_shell_command_carries_no_source() {
        let params = call("shell_write", serde_json::json!({ "command": "echo ${" }));
        let spec = build_hook_gate_spec("h", "d".into(), &params);
        assert_eq!(spec.exec_source, None);
        assert!(spec.planned.is_empty());
    }
}
