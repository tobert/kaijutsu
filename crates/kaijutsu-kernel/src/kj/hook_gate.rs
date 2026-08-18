//! Builds a [`GateSpec`] for a hook's `Ask` action — the sibling of
//! [`crate::kj::shell_gate`], for the other origin that opens a gate.
//!
//! ## What a hook ask can honestly describe
//!
//! There is no source program here to plan. `HookAction::Ask` fires from
//! the PreCall phase of a tool call, so the one thing that exists to show a
//! human is **the call itself**: the instance, the tool, and the arguments
//! as they stand at fire time. That is the single [`GatedStatement`] this
//! builds, and it is the whole of what the ask covers.
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
//! ## No free variables, and what follows
//!
//! Arguments are concrete JSON by the time a hook sees them: nothing here
//! is a `${VAR}` waiting to be substituted, the way `kj cc send`'s message
//! body is. So [`VarBinding`]-driven refusal of allow-always never fires on
//! a hook ask — every one of them is eligible to be remembered. That is a
//! consequence of the shape rather than a policy choice, and it is written
//! down here because the opposite (a gate whose asks can never be
//! remembered) would look like a bug rather than a fact.

use approval_ledger::types::Origin;

use super::gate::{GateSpec, GatedStatement};
use crate::mcp::types::KernelCallParams;

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
            // See the module docs: concrete arguments, nothing free.
            vars: vec![],
            // No source program, so no position within one.
            source_index: None,
        }],
    }
}
