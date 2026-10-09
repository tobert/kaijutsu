//! Shadow voices: a seat's outward calls as a dialogue in a fork child
//! (`docs/council.md`, "Shadow voice").
//!
//! The gate is the shadow's only writer. [`record_call`] runs before the
//! gate decides and appends the call as a user turn; [`record_outcome`] runs
//! after and appends what the gate did as the model turn after it. A seat
//! has a judge shadow when its cast has a [`JUDGE`] slot; the shadow is
//! created on the first call the gate evaluates for that seat.
//!
//! A judge on a council server (a backend of kind `mk`) is primed after
//! each call: the shadow is sent whole on a task of its own, warming the
//! seat's shell spec, so the server holds the dialogue before it is asked
//! about it. A judge on a chat backend is hydrated whole when it is asked.
//!
//! A shadow records and decides nothing. Every failure here is logged at
//! error level and leaves the gate's decision alone.

use std::sync::Arc;

use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, ContextState, DocKind, PrincipalId, Role, Status};

use crate::kernel_db::{ContextRow, KernelDbError, KernelDbResult};
use crate::kj::gate::{GateOutcome, GateSpec, GateVerdict};
use crate::kj::gate_policy::GateConfigLoad;

/// The context_type, and the cast slot role, of a judge shadow.
pub(crate) const JUDGE: &str = "judge";

/// The most bytes of a call's input a shadow turn keeps; the rest is cut
/// and marked.
pub(crate) const CALL_INPUT_BYTES: usize = 800;

/// Serializes finding and creating shadows, so two calls on one seat cannot
/// both create one.
static CREATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// A call [`record_call`] wrote: the shadow and the user turn.
pub(crate) struct ShadowCall {
    shadow: ContextId,
    call: BlockId,
}

/// A seat's judge shadow and where its model is served.
struct Judge {
    shadow: ContextId,
    /// The seat's label, or its short id when it has none.
    seat_name: String,
    seat_type: String,
    /// The council server that holds the shadow: the `judge` slot's backend
    /// address when its kind is `mk`. `None` for a chat backend.
    server: Option<String>,
}

/// Appends the call `spec` describes to the judge shadow of `seat`, creating
/// the shadow on the seat's first call, and primes a council-server judge.
/// `None` when the seat has no judge slot in its cast, or when writing
/// failed (logged).
pub(crate) fn record_call(
    kernel: &Arc<crate::Kernel>,
    seat: Option<ContextId>,
    spec: &GateSpec,
    config: &GateConfigLoad,
) -> Option<ShadowCall> {
    let seat = seat?;
    let judge = match judge_shadow(kernel, seat) {
        Ok(Some(judge)) => judge,
        Ok(None) => return None,
        Err(error) => {
            tracing::error!(%seat, "shadow: cannot find or create the judge shadow: {error}");
            return None;
        }
    };
    let shadow = judge.shadow;
    let text = call_text(&spec.tool, &spec.authorized_label);
    let call = match append(kernel, shadow, Role::User, text) {
        Ok(call) => call,
        Err(error) => {
            tracing::error!(%seat, %shadow, "shadow: cannot record the call: {error}");
            return None;
        }
    };
    spawn_prime(kernel, &judge, config);
    Some(ShadowCall { shadow, call })
}

/// Primes a council-server judge on a task of its own. The judge is read
/// under the seat's shell spec from `gate.toml`; a seat whose type has no
/// council there is not primed.
fn spawn_prime(kernel: &Arc<crate::Kernel>, judge: &Judge, config: &GateConfigLoad) {
    let Some(server) = judge.server.clone() else { return };
    let spec_name = config
        .as_ref()
        .ok()
        .and_then(|c| c.council_for(Some(&judge.seat_type)))
        .and_then(super::gate::shell_spec)
        .map(|s| s.name.clone());
    let Some(spec_name) = spec_name else {
        tracing::warn!(shadow = %judge.shadow, seat_type = %judge.seat_type, "shadow: not primed: gate.toml declares no council shell spec for the seat's type");
        return;
    };
    let (kernel, shadow, seat_name) = (kernel.clone(), judge.shadow, judge.seat_name.clone());
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        match kernel.council_sync().prime_shadow(&kernel, &server, &spec_name, shadow, &seat_name).await {
            Ok(head) => tracing::info!(
                target: "kaijutsu::council",
                %shadow, %server, head = %head, elapsed_ms = started.elapsed().as_millis() as u64,
                "shadow primed"
            ),
            Err(miss) => tracing::error!(target: "kaijutsu::council", %shadow, %server, "shadow: priming failed: {}", miss.0),
        }
    });
}

/// Appends what the gate did with a call as the model turn after it.
/// `bump_flavor` names the bump that refused it, when one did.
pub(crate) fn record_outcome(
    kernel: &crate::Kernel,
    call: ShadowCall,
    outcome: &GateOutcome,
    bump_flavor: Option<&str>,
) {
    let text = outcome_text(outcome.verdict, bump_flavor);
    if let Err(error) = append(kernel, call.shadow, Role::Model, text) {
        tracing::error!(shadow = %call.shadow, call = %call.call.to_key(), "shadow: cannot record the outcome: {error}");
    }
}

/// The user turn for one call: the tool name and its input, cut to
/// [`CALL_INPUT_BYTES`].
fn call_text(tool: &str, input: &str) -> String {
    if input.len() <= CALL_INPUT_BYTES {
        return format!("{tool}: {input}");
    }
    let mut end = CALL_INPUT_BYTES;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{tool}: {} [cut: {} more bytes]", &input[..end], input.len() - end)
}

/// The model turn for what the gate did.
fn outcome_text(verdict: GateVerdict, bump_flavor: Option<&str>) -> String {
    match (verdict, bump_flavor) {
        (GateVerdict::Allowed, _) => "ran".into(),
        (GateVerdict::Pending, _) => "asked the reviewer".into(),
        (GateVerdict::Denied, Some(flavor)) => format!("bumped ({flavor})"),
        (GateVerdict::Denied, None) => "refused".into(),
        (GateVerdict::Unavailable, _) => "not run: the gate was unavailable".into(),
    }
}

fn append(kernel: &crate::Kernel, shadow: ContextId, role: Role, text: String) -> Result<BlockId, String> {
    let blocks = kernel.blocks();
    let after = blocks.last_block_id(shadow);
    blocks
        .insert_block_as(
            shadow,
            None,
            after.as_ref(),
            role,
            BlockKind::Text,
            text,
            Status::Done,
            ContentType::Plain,
            Some(PrincipalId::system()),
        )
        .map_err(|e| e.to_string())
}

/// The seat's live judge shadow, created when the seat's cast has a
/// [`JUDGE`] slot and no shadow exists yet. The shadow is found by its fork
/// edge and type, never by its label.
fn judge_shadow(kernel: &crate::Kernel, seat: ContextId) -> KernelDbResult<Option<Judge>> {
    let _create = CREATE.lock();
    let (row, judge) = {
        let db = kernel.kernel_db().lock();
        let Some(seat_row) = db.get_context(seat)? else {
            return Err(KernelDbError::Validation(format!("seat context {seat} has no row")));
        };
        let Some(cast_id) = seat_row.cast_id else { return Ok(None) };
        let Some(slot) = db.get_cast_slot(cast_id, JUDGE)? else { return Ok(None) };
        let backend = db.list_backends()?.into_iter().find(|b| b.backend_id == slot.backend_id);
        let server = backend.filter(|b| b.kind == "mk").and_then(|b| b.base_url);
        let mut judge = Judge {
            shadow: seat,
            seat_name: seat_row.label.clone().unwrap_or_else(|| seat.short()),
            seat_type: seat_row.context_type.clone(),
            server,
        };
        if let Some(existing) = db.structural_children(seat)?.into_iter().find(|c| c.context_type == JUDGE) {
            judge.shadow = existing.context_id;
            return Ok(Some(judge));
        }
        let row = shadow_row(&seat_row, cast_id);
        db.in_transaction(|db| crate::kj::context::insert_new_context_rows(db, &row, Some(seat)))?;
        judge.shadow = row.context_id;
        (row, judge)
    };
    kernel
        .blocks()
        .create_document(row.context_id, DocKind::Conversation, None)
        .map_err(|e| KernelDbError::Validation(format!("shadow document: {e}")))?;
    let registered = kernel.drift().write().register(row.context_id, row.label.as_deref(), Some(seat), row.created_by);
    if let Err(error) = registered {
        tracing::error!(%seat, shadow = %row.context_id, "shadow: label not registered: {error}");
    }
    Ok(Some(judge))
}

/// A judge shadow's row: a fork child of the seat playing the seat's cast,
/// with no performer.
fn shadow_row(seat: &ContextRow, cast_id: kaijutsu_types::CastId) -> ContextRow {
    let seat_name = seat.label.clone().unwrap_or_else(|| seat.context_id.short());
    ContextRow {
        context_id: ContextId::new(),
        label: Some(format!("{JUDGE}-{seat_name}")),
        provider: None,
        model: None,
        system_prompt: None,
        context_state: ContextState::Live,
        context_type: JUDGE.into(),
        created_at: kaijutsu_types::now_millis() as i64,
        created_by: PrincipalId::system(),
        forked_from: Some(seat.context_id),
        fork_kind: None,
        archived_at: None,
        workspace_id: seat.workspace_id,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        cast_id: Some(cast_id),
        origin_host: None,
        played_by: None,
        reviewer_id: None,
        director_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_call_is_cut_on_a_char_boundary_and_says_how_much() {
        let input = format!("echo {}", "é".repeat(CALL_INPUT_BYTES));
        let text = call_text("shell_write", &input);
        assert!(text.starts_with("shell_write: echo é"));
        assert!(text.ends_with("more bytes]"), "{text}");
        assert!(text.len() < input.len());
    }
}
