//! Shadow voices: a seat's outward calls as a dialogue in a fork child
//! (`docs/council.md`, "Shadow voice").
//!
//! The gate is the shadow's only writer. [`record_call`] runs before the
//! gate decides and appends the call as a user turn; [`record_outcome`] runs
//! after and appends what the gate did as the model turn after it. A seat
//! has a judge shadow when its cast has a [`JUDGE`] slot; the shadow is
//! created on the first call the gate evaluates for that seat.
//!
//! A shadow records and decides nothing. Every failure here is logged at
//! error level and leaves the gate's decision alone.

use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, ContextState, DocKind, PrincipalId, Role, Status};

use crate::kernel_db::{ContextRow, KernelDbError, KernelDbResult};
use crate::kj::gate::{GateOutcome, GateSpec, GateVerdict};

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

/// Appends the call `spec` describes to the judge shadow of `seat`, creating
/// the shadow on the seat's first call. `None` when the seat has no judge
/// slot in its cast, or when writing failed (logged).
pub(crate) fn record_call(kernel: &crate::Kernel, seat: Option<ContextId>, spec: &GateSpec) -> Option<ShadowCall> {
    let seat = seat?;
    let shadow = match judge_shadow(kernel, seat) {
        Ok(Some(shadow)) => shadow,
        Ok(None) => return None,
        Err(error) => {
            tracing::error!(%seat, "shadow: cannot find or create the judge shadow: {error}");
            return None;
        }
    };
    let text = call_text(&spec.tool, &spec.authorized_label);
    match append(kernel, shadow, Role::User, text) {
        Ok(call) => Some(ShadowCall { shadow, call }),
        Err(error) => {
            tracing::error!(%seat, %shadow, "shadow: cannot record the call: {error}");
            None
        }
    }
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
fn judge_shadow(kernel: &crate::Kernel, seat: ContextId) -> KernelDbResult<Option<ContextId>> {
    let _create = CREATE.lock();
    let row = {
        let db = kernel.kernel_db().lock();
        let Some(seat_row) = db.get_context(seat)? else {
            return Err(KernelDbError::Validation(format!("seat context {seat} has no row")));
        };
        let Some(cast_id) = seat_row.cast_id else { return Ok(None) };
        if db.get_cast_slot(cast_id, JUDGE)?.is_none() {
            return Ok(None);
        }
        if let Some(existing) = db.structural_children(seat)?.into_iter().find(|c| c.context_type == JUDGE) {
            return Ok(Some(existing.context_id));
        }
        let row = shadow_row(&seat_row, cast_id);
        db.in_transaction(|db| crate::kj::context::insert_new_context_rows(db, &row, Some(seat)))?;
        row
    };
    kernel
        .blocks()
        .create_document(row.context_id, DocKind::Conversation, None)
        .map_err(|e| KernelDbError::Validation(format!("shadow document: {e}")))?;
    let registered = kernel.drift().write().register(row.context_id, row.label.as_deref(), Some(seat), row.created_by);
    if let Err(error) = registered {
        tracing::error!(%seat, shadow = %row.context_id, "shadow: label not registered: {error}");
    }
    Ok(Some(row.context_id))
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
