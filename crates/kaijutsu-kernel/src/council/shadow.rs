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
//! After the council decides a judged seat's call, [`spawn_judge`] reads the
//! decision's configured contexts and the shadow on the judge's server,
//! under the same spec and case. The answer is recorded as an observation
//! of that decision and added to the call's outcome turn. A call the static
//! rules decided is recorded but not judged.
//!
//! A shadow records and decides nothing. Every failure here is logged at
//! error level, or recorded as a miss, and leaves the gate's decision alone.

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

/// Bounds a judge read: preparing the server and the decision together.
/// The read runs off the gate's path, so it waits longer than a decision.
const JUDGE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// A call [`record_call`] wrote: the shadow and the user turn.
pub(crate) struct ShadowCall {
    shadow: ContextId,
    call: BlockId,
    judge: Option<JudgeRead>,
}

impl ShadowCall {
    /// The judge read for this call, when the judge is on a council server.
    pub(crate) fn judge_read(&self) -> Option<JudgeRead> {
        self.judge.clone()
    }
}

/// What a judge read after the gate's decision needs.
#[derive(Clone, Debug)]
pub(crate) struct JudgeRead {
    seat: ContextId,
    seat_name: String,
    shadow: ContextId,
    call: BlockId,
    server: String,
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
    let read = judge.server.clone().map(|server| JudgeRead {
        seat,
        seat_name: judge.seat_name.clone(),
        shadow,
        call: call.clone(),
        server,
    });
    Some(ShadowCall { shadow, call, judge: read })
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

/// Reads the judge about the call `read` names, after the council decision
/// `decision_id` committed: `council`'s contexts and the spec's own, then the
/// shadow, on the judge's server, under the decision's shell spec and case
/// `state`. Records the answer, or the miss, as an observation of the
/// decision, and adds an answer to the call's outcome turn.
pub(crate) fn spawn_judge(
    kernel: Arc<crate::Kernel>,
    council: crate::kj::gate_policy::CouncilConfig,
    decision_id: Vec<u8>,
    state: kaijutsu_mk::Json,
    read: JudgeRead,
) {
    tokio::spawn(async move {
        let row = judge_one(&kernel, council, &decision_id, &state, &read).await;
        let recorded = approval_ledger::council_observation::insert_council_observation(
            kernel.kernel_db().lock().conn_for_ledger(),
            &row.observation,
        );
        if let Err(error) = recorded {
            tracing::error!(target: "kaijutsu::council", shadow = %read.shadow, "shadow: the judge's answer could not be recorded: {error}");
        }
        match (row.choice, &row.observation.miss_cause) {
            (Some(choice), _) => {
                tracing::info!(target: "kaijutsu::council", shadow = %read.shadow, %choice, p = row.p, "a judge answered");
                add_answer(&kernel, &read, &format!("judge: {choice} (p={:.2})", row.p));
            }
            (None, cause) => tracing::warn!(
                target: "kaijutsu::council", shadow = %read.shadow,
                miss_cause = cause.as_deref().unwrap_or("unknown"), "a judge gave no answer"
            ),
        }
    });
}

/// One judge read: the observation row, and the pooled probability of its
/// choice.
struct Judged {
    observation: approval_ledger::council_observation::NewCouncilObservation,
    choice: Option<String>,
    p: f64,
}

async fn judge_one(
    kernel: &crate::Kernel,
    mut council: crate::kj::gate_policy::CouncilConfig,
    decision_id: &[u8],
    state: &kaijutsu_mk::Json,
    read: &JudgeRead,
) -> Judged {
    use approval_ledger::council::CouncilServer;
    use approval_ledger::council_observation::{CouncilObservationOutcome, NewCouncilObservation};
    use kaijutsu_mk::council::wire::PooledAnswer;

    let started = std::time::Instant::now();
    let label = shadow_label(&read.seat_name);
    let spec = super::gate::shell_spec(&council).cloned();
    let mut judged = Judged {
        observation: NewCouncilObservation {
            decision_id: decision_id.to_vec(),
            voice_label: label.clone(),
            voice_context_id: Some(read.shadow.as_bytes().to_vec()),
            spec_name: spec.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            spec_id: String::new(),
            server: CouncilServer {
                model: String::new(),
                weight_hash: String::new(),
                tokenizer_hash: String::new(),
                template: String::new(),
                engine: String::new(),
            },
            outcome: CouncilObservationOutcome::Miss,
            choice: None,
            miss_cause: None,
            snapshot: None,
            expected_head: None,
            queue_ms: 0,
            ms: 0,
            questions: Vec::new(),
        },
        choice: None,
        p: 0.0,
    };
    let miss = |mut judged: Judged, cause: String| {
        judged.observation.miss_cause = Some(cause);
        judged.observation.ms = started.elapsed().as_millis() as i64;
        judged
    };
    let Some(spec) = spec else {
        return miss(judged, "gate.toml declares no council shell spec for the seat's type".into());
    };
    council.server = read.server.clone();
    council.deadline_ms = JUDGE_DEADLINE.as_millis() as u64;
    let deadline = tokio::time::Instant::now() + JUDGE_DEADLINE;
    let mut labels = council.contexts.clone();
    labels.extend(spec.contexts.iter().cloned());
    let seat = council.house_rules.then_some(read.seat);
    let mut prepared = match super::gate::prepare_within(kernel, &council, &spec.name, &labels, seat, deadline).await {
        Ok(prepared) => prepared,
        Err(cause) => return miss(judged, cause),
    };
    let primed = kernel.council_sync().prime_shadow(kernel, &read.server, &spec.name, read.shadow, &read.seat_name);
    let head = match tokio::time::timeout_at(deadline, primed).await {
        Ok(Ok(head)) => head,
        Ok(Err(super::sync::PrepareMiss(cause))) => return miss(judged, cause),
        Err(_) => return miss(judged, "the shadow could not be sent in time".into()),
    };
    judged.observation.expected_head = Some(head.to_string());
    judged.observation.spec_id = prepared.spec_id.to_string();
    prepared.contexts.push(super::sync::PreparedContext {
        label: label.clone(),
        context_id: read.shadow,
        head,
        house_rules: false,
    });
    let request = super::gate::decision_request(&prepared, &council, state.clone());
    let asked = std::time::Instant::now();
    let response = match super::gate::ask_within(kernel, &prepared, &request, &council, deadline).await {
        Ok(response) => response,
        Err(cause) => return miss(judged, cause),
    };
    if let Err(mismatch) = kaijutsu_mk::council::math::verify(&response, &request) {
        return miss(judged, format!("the answer's numbers do not recompute ({mismatch})"));
    }
    judged.observation.server = CouncilServer {
        model: response.identity.model.clone(),
        weight_hash: response.identity.weight_hash.clone(),
        tokenizer_hash: response.identity.tokenizer_hash.clone(),
        template: response.identity.template.clone(),
        engine: response.identity.engine.clone(),
    };
    judged.observation.queue_ms = response.queue_ms.unwrap_or(0.0).round() as i64;
    let shadow_id = read.shadow.to_string();
    if let Some(shadow_read) = response.reads.iter().find(|r| r.context.as_deref() == Some(shadow_id.as_str())) {
        judged.observation.snapshot = shadow_read.snapshot.as_ref().map(|s| s.to_string());
        judged.observation.questions =
            shadow_read.answers.iter().map(|(id, answer)| super::gate::read_question(id, answer)).collect();
    }
    let Some(PooledAnswer::Choice(pooled)) = response.answers.get(super::gate::VERDICT) else {
        return miss(judged, format!("the answer has no pooled `{}` choice", super::gate::VERDICT));
    };
    judged.p = pooled.probabilities.get(&pooled.choice).copied().unwrap_or(0.0);
    judged.choice = Some(pooled.choice.clone());
    judged.observation.choice = Some(pooled.choice.clone());
    judged.observation.outcome = CouncilObservationOutcome::Answered;
    judged.observation.ms = asked.elapsed().as_millis() as i64;
    judged
}

/// Adds `answer` to the model turn after the call `read` names. The gate
/// writes that turn before any judge answers; a turn not there yet is
/// logged and left alone.
fn add_answer(kernel: &crate::Kernel, read: &JudgeRead, answer: &str) {
    let blocks = kernel.blocks();
    let snapshots = match blocks.block_snapshots(read.shadow) {
        Ok(snapshots) => snapshots,
        Err(error) => {
            tracing::error!(shadow = %read.shadow, "shadow: cannot read the dialogue to add the judge's answer: {error}");
            return;
        }
    };
    let after_call = snapshots.iter().skip_while(|b| b.id != read.call).nth(1);
    let Some(outcome) = after_call.filter(|b| b.role == Role::Model) else {
        tracing::warn!(shadow = %read.shadow, "shadow: the call has no outcome turn yet; the judge's answer is only in the record");
        return;
    };
    let text = format!("{}; {answer}", outcome.content);
    let replaced = blocks.replace_text_if_unchanged_as(read.shadow, &outcome.id, &outcome.content, &text, Some(PrincipalId::system()));
    if let Err(error) = replaced {
        tracing::error!(shadow = %read.shadow, "shadow: cannot add the judge's answer: {error}");
    }
}

/// A judge shadow's label: `judge-` and the seat's name.
fn shadow_label(seat_name: &str) -> String {
    format!("{JUDGE}-{seat_name}")
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
        label: Some(shadow_label(&seat_name)),
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
