//! Shadow reviewer_contexts: a seat's outward calls as a dialogue in a fork child
//! (`docs/council.md`, "Shadow context").
//!
//! The gate is the shadow's only writer. [`record_call`] runs before the
//! gate decides and appends the call as a user turn; [`record_outcome`] runs
//! after and appends what the gate did as the model turn after it. A seat
//! has a judge shadow when its performer's council block names a judge character
//! (`judge = "<character>"`); the shadow is created on the first call the
//! gate evaluates for that seat. The shadow is performed by the judge
//! character, has the seat as its parent and its senpai, and copies its
//! slots from the judge character. It is found as the live child of the seat
//! that character performs.
//!
//! A new shadow runs its performer's `create` rc lifecycle (`judge/` on a
//! shipped host), whose stance becomes the shadow's framing
//! (`projection::project_shadow`).
//!
//! The shadow's `model` slot names its judge. A judge on a council server (a
//! backend of kind `mk`) is primed after each call: the shadow is sent whole
//! on a task of its own, warming the seat's shell spec, so the server holds
//! the dialogue before it is asked about it. A judge on a chat backend is
//! not asked: its shadow records the dialogue and nothing reads it. A judge
//! on an `mk` backend with no address, or a shadow with no `model` slot, is
//! never primed, and each judge read is a miss that names why.
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

use super::gate::SlotServer;

/// The name of the shipped judge character, which performs every judge
/// shadow.
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
    /// The shadow's own label, which names it in the record.
    shadow_label: String,
    call: BlockId,
    /// The judge's council server and served model, or why its `mk`
    /// backend names none.
    server: Result<SlotServer, String>,
}

/// A seat's judge shadow and where its model is served.
struct Judge {
    shadow: ContextId,
    /// The shadow's label, or its short id when it has none.
    shadow_label: String,
    /// The seat's label, or its short id when it has none.
    seat_name: String,
    /// The seat's performer, whose layers name the council the judge is
    /// read under.
    seat_performer: Option<crate::kj::gate_policy::GatePerformer>,
    /// The council server that holds the shadow: its `model` slot's backend
    /// address and model when its kind is `mk`, or why it names none. `None`
    /// for a chat backend.
    server: Option<Result<SlotServer, String>>,
}

/// Appends the call `spec` describes to the judge shadow of `seat`, creating
/// the shadow on the seat's first call, and primes a council-server judge.
/// `None` when the seat's council names no judge, or when writing failed
/// (logged).
pub(crate) async fn record_call(
    kernel: &Arc<crate::Kernel>,
    seat: Option<ContextId>,
    spec: &GateSpec,
    config: &GateConfigLoad,
) -> Option<ShadowCall> {
    let seat = seat?;
    let judge = match judge_shadow(kernel, seat, config) {
        Ok(Some((judge, created))) => {
            if created {
                run_create_lifecycle(kernel, judge.shadow).await;
            }
            judge
        }
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
        shadow_label: judge.shadow_label.clone(),
        call: call.clone(),
        server,
    });
    Some(ShadowCall { shadow, call, judge: read })
}

/// Primes a council-server judge on a task of its own. The judge is read
/// under the seat's shell spec from `gate.toml`; a seat whose performer's
/// layers enable no council there is not primed.
fn spawn_prime(kernel: &Arc<crate::Kernel>, judge: &Judge, config: &GateConfigLoad) {
    let Some(Ok(SlotServer { server, model })) = judge.server.clone() else { return };
    let spec_name = config
        .as_ref()
        .ok()
        .and_then(|c| c.council_for(judge.seat_performer.as_ref()))
        .and_then(|c| super::gate::shell_spec(&c).map(|s| s.name.clone()));
    let Some(spec_name) = spec_name else {
        let performer = judge.seat_performer.as_ref().map_or("none", |p| p.name.as_str());
        tracing::warn!(shadow = %judge.shadow, %performer, "shadow: not primed: gate.toml declares no council shell spec for the seat's performer");
        return;
    };
    let (kernel, shadow, seat_name) = (kernel.clone(), judge.shadow, judge.seat_name.clone());
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + super::sync::SHADOW_PRIME_TIMEOUT;
        let waits = crate::llm::endpoint::SlotWaits::default();
        match kernel.council_sync().prime_shadow(&kernel, &server, Some(&model), &spec_name, shadow, &seat_name, deadline, &waits).await {
            Ok((_, head)) => tracing::info!(
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
/// shadow, on the judge's server, under the decision's shell spec and
/// `case`. Records the answer, or the miss, as an observation of the
/// decision, and adds an answer to the call's outcome turn.
pub(crate) fn spawn_judge(
    kernel: Arc<crate::Kernel>,
    council: crate::kj::gate_policy::CouncilConfig,
    decision_id: Vec<u8>,
    state: serde_json::Value,
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
    state: &serde_json::Value,
    read: &JudgeRead,
) -> Judged {
    use approval_ledger::council::CouncilServer;
    use approval_ledger::council_observation::{CouncilObservationOutcome, NewCouncilObservation};
    use gogi_proto::decision::PooledAnswer;

    let started = std::time::Instant::now();
    let label = read.shadow_label.clone();
    let spec = super::gate::shell_spec(&council).cloned();
    let mut judged = Judged {
        observation: NewCouncilObservation {
            decision_id: decision_id.to_vec(),
            reviewer_label: label.clone(),
            reviewer_context_id: Some(read.shadow.as_bytes().to_vec()),
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
    let waits = crate::llm::endpoint::SlotWaits::default();
    let miss = |mut judged: Judged, cause: String| {
        judged.observation.miss_cause = Some(cause);
        judged.observation.ms = started.elapsed().as_millis() as i64;
        judged.observation.queue_ms = super::gate::queue_ms(&waits, None);
        judged
    };
    let server = match &read.server {
        Ok(slot) => slot.clone(),
        Err(why) => return miss(judged, format!("the judge shadow's `model` slot: {why}")),
    };
    let Some(spec) = spec else {
        return miss(judged, "gate.toml declares no council shell spec for the seat's type".into());
    };
    council.server = server.server.clone();
    council.model = Some(server.model.clone());
    council.deadline_ms = JUDGE_DEADLINE.as_millis() as u64;
    let deadline = tokio::time::Instant::now() + JUDGE_DEADLINE;
    let labels = judge_labels(&council, &spec);
    let seat = council.house_rules.then_some(read.seat);
    let shadow = std::slice::from_ref(&label);
    let mut prepared = match super::gate::prepare_within(kernel, &council, &spec.name, &labels, seat, shadow, deadline, &waits).await {
        Ok(prepared) => prepared,
        Err(cause) => return miss(judged, cause),
    };
    let primed =
        kernel.council_sync().prime_shadow(kernel, &server.server, Some(&server.model), &spec.name, read.shadow, &read.seat_name, deadline, &waits);
    let (shadow_seat, head) = match tokio::time::timeout_at(deadline, primed).await {
        Ok(Ok(primed)) => primed,
        Ok(Err(super::sync::PrepareMiss(cause))) => return miss(judged, cause),
        Err(_) => {
            let cause = match waits.open() {
                Some(wait) => format!("the shadow could not be sent in time: the deadline passed {wait}"),
                None => "the shadow could not be sent in time".into(),
            };
            return miss(judged, cause);
        }
    };
    judged.observation.expected_head = Some(head.to_string());
    judged.observation.spec_id = prepared.spec_id.to_string();
    prepared.contexts.push(super::sync::PreparedContext {
        label: label.clone(),
        context_id: read.shadow,
        seat: shadow_seat,
        head,
        house_rules: false,
    });
    let shadow_index = (prepared.contexts.len() - 1) as u64;
    let request = super::gate::decision_request(&prepared, &council, state.clone());
    let asked = std::time::Instant::now();
    let response = match super::gate::ask_within(kernel, &prepared, &request, &council, deadline, &waits).await {
        Ok(response) => response,
        Err(cause) => return miss(judged, cause),
    };
    if let Err(mismatch) = gogi_proto::math::verify(&response, &request.expect()) {
        return miss(judged, format!("the answer's numbers do not recompute ({mismatch})"));
    }
    judged.observation.server = CouncilServer {
        model: response.identity.model.clone(),
        weight_hash: response.identity.weight_hash.clone(),
        tokenizer_hash: response.identity.tokenizer_hash.clone(),
        template: response.identity.template.clone(),
        engine: response.identity.engine.clone(),
    };
    judged.observation.queue_ms = super::gate::queue_ms(&waits, response.queue_ms);
    if let Some(shadow_read) = response.reads.iter().find(|r| r.context == Some(shadow_index)) {
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
pub(crate) fn shadow_label(seat_name: &str) -> String {
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

/// Runs the new shadow's `create` lifecycle, in its performer's rc
/// directory (`judge/` on a shipped host), which writes its stance. A failure is logged; a shadow with no stance is never sent to a
/// council server (`projection::project_shadow`).
async fn run_create_lifecycle(kernel: &crate::Kernel, shadow: ContextId) {
    let Some(dispatcher) = kernel.broker().kj_dispatcher().await else {
        tracing::error!(%shadow, "shadow: no kj dispatcher, so the judge create lifecycle cannot run");
        return;
    };
    let admission = match kernel.admit_context(shadow) {
        Ok(admission) => admission,
        Err(error) => {
            tracing::error!(%shadow, "shadow: the judge create lifecycle was not admitted: {error}");
            return;
        }
    };
    let caller = crate::kj::KjCaller {
        principal_id: PrincipalId::system(),
        actor_id: PrincipalId::system(),
        reviewer_id: None,
        context_id: Some(shadow),
        session_id: kaijutsu_types::SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: true,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let invocation = crate::rc::RcInvocation::new(crate::rc::VERB_CREATE, &admission, &caller.cancel);
    if let Err(error) = crate::rc::run(&dispatcher, invocation, &caller).await {
        tracing::error!(%shadow, "shadow: the judge create lifecycle could not settle: {error}");
    }
}

/// The judge character `config` names for a seat `performer` plays: the
/// `judge` of the council its layers compose. `None` when they enable no
/// council, name no judge, or the config did not load.
fn judge_name(config: &GateConfigLoad, performer: Option<&crate::kj::gate_policy::GatePerformer>) -> Option<String> {
    config.as_ref().ok()?.council_for(performer)?.judge
}

/// The live character named `name`, or why there is none.
fn judge_character(db: &crate::kernel_db::KernelDb, name: &str) -> KernelDbResult<crate::kernel_db::CharacterRow> {
    match db.get_character_by_name(name)? {
        Some(sheet) if sheet.retired_at.is_none() => Ok(sheet),
        Some(_) => Err(KernelDbError::Validation(format!(
            "the council names judge {name}, which is retired; name another judge in gate.toml"
        ))),
        None => Err(KernelDbError::Validation(format!(
            "the council names judge {name}, which is no character; `kj character create {name} --rc judge` makes it"
        ))),
    }
}

/// The seat's live judge shadow: the live child of `seat` that `judge`
/// performs.
fn live_shadow(db: &crate::kernel_db::KernelDb, seat: ContextId, judge: PrincipalId) -> KernelDbResult<Option<ContextRow>> {
    Ok(db.fork_children(seat)?.into_iter().find(|c| c.performer == Some(judge)))
}

/// A context's label, or its short id when it has none.
fn name_of(row: &ContextRow) -> String {
    row.label.clone().unwrap_or_else(|| row.context_id.short())
}

/// The seat's live judge shadow, created when the seat's council names a
/// judge character and no shadow exists yet. The shadow is found by its fork
/// edge and its performer, never by its label or type. Returns the judge and
/// whether this call created its shadow.
fn judge_shadow(kernel: &crate::Kernel, seat: ContextId, config: &GateConfigLoad) -> KernelDbResult<Option<(Judge, bool)>> {
    let _create = CREATE.lock();
    let (row, judge) = {
        let db = kernel.kernel_db().lock();
        let Some(seat_row) = db.get_context(seat)? else {
            return Err(KernelDbError::Validation(format!("seat context {seat} has no row")));
        };
        let performer = crate::kj::gate_policy::performer_of(&db, Some(seat)).map_err(KernelDbError::Validation)?;
        let Some(name) = judge_name(config, performer.as_ref()) else { return Ok(None) };
        let sheet = judge_character(&db, &name)?;
        let mut judge = Judge {
            shadow: seat,
            shadow_label: String::new(),
            seat_name: name_of(&seat_row),
            seat_performer: performer,
            server: None,
        };
        if let Some(existing) = live_shadow(&db, seat, sheet.principal_id)? {
            judge.shadow = existing.context_id;
            judge.shadow_label = name_of(&existing);
            judge.server = shadow_server(&db, existing.context_id)?;
            return Ok(Some((judge, false)));
        }
        // A shadow is performed by the judge character, a child of its seat
        // that answers to it, with the judge character's slots.
        let row = shadow_row(&db, &seat_row)?;
        db.in_transaction(|db| {
            crate::kj::context::insert_new_context_rows(
                db,
                &row,
                crate::kj::context::CharacterPlacement::Start { performer: sheet.principal_id, senpai: Some(seat) },
            )
        })?;
        judge.shadow = row.context_id;
        judge.shadow_label = name_of(&row);
        judge.server = shadow_server(&db, row.context_id)?;
        if judge.server.is_none() {
            // The shadow row is already written, so a fault here names the
            // backend as unread rather than failing the creation.
            let backend = match super::gate::seat_slot_backend(&db, row.context_id, crate::model_resolution::MODEL) {
                Ok(Some((backend, _))) => format!("{} (kind {})", backend.name, backend.kind),
                Ok(None) => "none".into(),
                Err(e) => format!("unread: {e}"),
            };
            tracing::warn!(
                %seat, shadow = %judge.shadow, judge = %name, %backend,
                "shadow: the judge's model is not on an mk backend, so this judge shadow is created but its judge is never read"
            );
        }
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
    Ok(Some((judge, true)))
}

/// The council server a judge shadow reads on, from its own `model` slot:
/// the backend's address and the slot's model when the backend's kind is
/// `mk`, or why it names none. `None` for a chat backend, whose judge is not
/// read yet.
fn shadow_server(
    db: &crate::kernel_db::KernelDb,
    shadow: ContextId,
) -> KernelDbResult<Option<Result<SlotServer, String>>> {
    let Some((backend, model)) = super::gate::seat_slot_backend(db, shadow, crate::model_resolution::MODEL)? else {
        let performer = db.get_context(shadow)?.and_then(|row| row.performer);
        let name = performer.map(|p| db.name_for(p)).unwrap_or_else(|| "(none)".into());
        return Ok(Some(Err(format!(
            "the shadow has none: its judge character {name} had no model when the shadow was made; \
             `kj character model set {name} <mk-backend>/<model>`, then archive {} so the next gate call makes another",
            db.context_name(shadow)
        ))));
    };
    if !crate::model_resolution::is_council_kind(&backend.kind) {
        return Ok(None);
    }
    Ok(Some(super::gate::council_address(&backend, &model)))
}

/// The council server and served model the judge of `seat` reads on, for
/// building its seats ahead of a decision: the live shadow's `model` slot,
/// or, before the shadow is made, the model of the judge character
/// `council` names, which the shadow will copy. `None` when the council
/// names no judge, the judge is no live character, or its model is not on
/// an `mk` backend with an address.
pub(crate) fn judge_server(
    db: &crate::kernel_db::KernelDb,
    council: &crate::kj::gate_policy::CouncilConfig,
    seat: ContextId,
) -> KernelDbResult<Option<SlotServer>> {
    let Some(name) = council.judge.as_deref() else { return Ok(None) };
    let Ok(sheet) = judge_character(db, name) else { return Ok(None) };
    if let Some(shadow) = live_shadow(db, seat, sheet.principal_id)? {
        return Ok(shadow_server(db, shadow.context_id)?.and_then(Result::ok));
    }
    let Some(model) = db.get_character_model(sheet.principal_id)? else { return Ok(None) };
    let Some(backend) = db.get_backend(model.backend_id)? else { return Ok(None) };
    Ok(super::gate::council_address(&backend, &model.model).ok())
}

/// The labels a judge read sends before the shadow: `[council] contexts`
/// and the shell spec's own. The reviewer chain does not join a judge read.
pub(crate) fn judge_labels(
    council: &crate::kj::gate_policy::CouncilConfig,
    spec: &crate::kj::gate_policy::CouncilSpec,
) -> Vec<String> {
    super::reviewer_contexts::ReviewerContextChain::default().decision_labels(council, spec)
}

/// A judge shadow's row: a child of the seat, labeled `judge-<seat>`, or
/// the first free `judge-<seat>-<n>` when a live context holds that. The
/// caller gives it its performer and senpai.
fn shadow_row(db: &crate::kernel_db::KernelDb, seat: &ContextRow) -> KernelDbResult<ContextRow> {
    let label = crate::kj::context::first_free_label(db, &shadow_label(&name_of(seat)))?;
    Ok(ContextRow {
        context_id: ContextId::new(),
        label: Some(label),
        system_prompt: None,
        context_state: ContextState::Live,
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
        origin_host: None,
        performer: None, senpai_ctx: None, persona: None,
    })
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
