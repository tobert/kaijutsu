//! `session/request_permission` — answering approval-ledger asks from ACP.
//!
//! `HookAction::Ask` (and `shell_write`'s gate) leave a durable row in the
//! approval ledger and wait; this module is the ACP side of answering one.
//! The ledger is the record of an answer: a decision goes to the kernel as
//! a typed call with no context, and authors no block in any transcript.
//! See `docs/gate-resume.md`.
//!
//! The client actor keeps the open asks current (`ActorHandle::ledger`).
//! What is ACP-only here is deciding *whether this bridge is the one to
//! answer* an ask, and the `session/request_permission` call/response
//! mapping.
//!
//! # Shape
//!
//! 1. [`start_permission_pump`] watches the actor's [`LedgerState`] and the
//!    session registry's binds.
//! 2. On each change, [`diff_open`] compares the open asks with the prompts
//!    this pump has raised. An ask is ours to raise when this connection's
//!    principal is its reviewer ([`AskSummary::answerable_by`]).
//! 3. Each new ask is routed to the session bound to its context, or to any
//!    session of this connection, and its round trip is spawned
//!    (`cx.spawn`): a `session/request_permission` call to the client, and
//!    on an explicit answer, `ActorHandle::decide_ask`. The request names
//!    the model's tool call that raised the ask, after the session has
//!    announced it.
//! 4. When a raised ask stops being ours to answer (another surface
//!    answered it, it was cancelled or expired, or it was reassigned), the
//!    pump withdraws its prompt: the outgoing request is cancelled with
//!    `$/cancel_request` and nothing is recorded.
//!
//! # The kernel is the authority, and nothing expires
//!
//! There is no `PERMISSION_ASK_TIMEOUT` budget owned by this module, and
//! there is no kernel-side one either: the gate records an ask and returns,
//! and an unanswered ask stays answerable indefinitely
//! (`docs/gate-resume.md`). An ask this pump offers and nobody answers is
//! not a leak — it is the open question it looks like until its reviewer
//! decides or it is explicitly abandoned. [`REQUEST_PERMISSION_TIMEOUT`]
//! bounds only one outgoing `session/request_permission` call, so a wedged
//! ACP client (stdio never reads the request) cannot leave one of this
//! pump's spawned tasks parked forever. When a request times out, the same
//! task offers the ask again until it is answered or withdrawn: a model
//! turn may be holding on it (`docs/gate-resume.md`, "The turn holds").
//! Answering late still works; there is no deadline to beat.
//!
//! Only an explicit answer records a verdict. A timed-out request, a
//! transport error, or an option this bridge never offered leaves the ask
//! pending. A cancelled prompt is an answer: it records
//! [`AskVerdict::PromptCancelled`], a denial.
//!
//! A round trip that ends with no answer reaching the ledger (the ledger
//! could not read the ask, the prompt failed, an unplaceable option, or
//! the answer call failed) releases its ask after [`REOFFER_AFTER`], and
//! the pump offers it again if it is still ours. A failure that repeats
//! re-offers at that pace.
//!
//! # Racing is fine and expected
//!
//! A reviewer can answer the same ask from a shell or another client while
//! this pump's prompt is on the client's screen. The ledger's claim makes
//! exactly one answerer win; the loser gets
//! [`AskAnswerFailureKind::AlreadyAnswered`]. Usually the push arrives first
//! and the prompt is withdrawn; when the client answered before it did, the
//! client is told the answer did not apply.
//!
//! # Not ours to answer
//!
//! A lead may review a coder context without an ACP session attached to that
//! coder. The request uses an existing ACP session for the same reviewer;
//! an ask with another reviewer stays pending for that reviewer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, SessionNotification, SessionUpdate, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_client::LedgerState;
use kaijutsu_client::rpc::AskAnswer;
use kaijutsu_types::{
    AskAnswerFailureKind, AskDetail, AskSummary, AskVerdict, BlockId, ContextId, PrincipalId, Remember, RememberResult,
    RememberScope,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bridge::KernelBridge;
use crate::rank;
use crate::session::{Session, SessionRegistry};
use crate::AcpBridge;

fn decision_failure_message(request_id: &str, verb: &str, reason: &str) -> String {
    format!("Approval {verb} for {request_id} did not apply: {reason}")
}

fn remember_failure_message(request_id: &str, verb: &str, reason: &str) -> String {
    format!("Approval {verb} for {request_id} applied to this ask only; no standing rule was remembered: {reason}")
}

/// Why an answer that asked for a standing rule learned none. `None` when
/// a rule was learned or none was asked for.
fn rule_not_learned(remembered: Option<&RememberResult>) -> Option<&str> {
    remembered.filter(|result| !result.learned).map(|result| result.note.as_str())
}

fn permission_prompt_failure_message(request_id: &str, reason: &str) -> String {
    format!("Approval prompt for {request_id} failed: {reason}; ask remains pending")
}

fn notify_decision_failure(cx: &ConnectionTo<Client>, session_id: &SessionId, message: String) {
    let _ = cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(crate::update::text_chunk(&message)),
    ));
}

/// Logged, as a field, each time the ledger takes up an answer this bridge
/// sent: `ask_answer="recorded"` when it recorded it, `ask_answer="refused"`
/// when it declined it (another answer got there first, or the caller is
/// not the reviewer). An answer authors no block, so this log line is how
/// a test harness reading the agent's stderr knows the ledger has it.
pub const ASK_ANSWER_LOGGED: &str = "ask_answer=";

/// Bound on one outgoing `session/request_permission` call — NOT a budget
/// for the ledger ask itself (see module docs, "The kernel is the
/// authority, and nothing expires").
pub const REQUEST_PERMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a request waits for its session to announce the tool call it
/// names. The call block exists before its ask does, so this wait is short;
/// past it the request goes out anyway, with a warning.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(5);
const ANNOUNCE_POLL: Duration = Duration::from_millis(20);

/// How long a round trip that ended without an answer the ledger took up
/// keeps its ask from being offered again. A repeated failure re-offers
/// at this pace, not in a loop.
pub const REOFFER_AFTER: Duration = Duration::from_secs(10);

/// Watch the actor's open asks and the session registry for the life of
/// the connection — the `.with_spawned` task `lib.rs::serve_stdio`
/// registers.
pub async fn start_permission_pump(bridge: &Arc<AcpBridge>, cx: ConnectionTo<Client>) {
    let ledger = bridge.kernel.actor().ledger();
    let binds = bridge.sessions.subscribe_binds();
    run_permission_pump(ledger, binds, bridge, cx).await;
}

/// Reconcile raised prompts with the open asks after every ledger change
/// and every session bind, forever. A bind matters because an ask with no
/// session to carry it waits for one. Never itself an error: a pump that
/// failed should stop pumping, not hang up the ACP connection.
pub async fn run_permission_pump(
    ledger: watch::Receiver<LedgerState>,
    binds: watch::Receiver<()>,
    bridge: &Arc<AcpBridge>,
    cx: ConnectionTo<Client>,
) {
    let timing = Timing { request_timeout: REQUEST_PERMISSION_TIMEOUT, reoffer_after: REOFFER_AFTER };
    pump(ledger, binds, bridge.kernel.clone(), &bridge.sessions, cx, timing).await;
}

/// The kernel calls the pump and its round trips make. [`KernelBridge`] is
/// the real one; tests script it.
trait AskKernel: Clone + Send + Sync + 'static {
    fn whoami(&self) -> impl Future<Output = Result<PrincipalId, String>> + Send;
    fn get_ask(&self, request_id: String) -> impl Future<Output = Result<Option<AskDetail>, String>> + Send;
    fn decide_ask(
        &self,
        request_id: String,
        verdict: AskVerdict,
        remember: Option<Remember>,
    ) -> impl Future<Output = Result<AskAnswer, String>> + Send;
}

impl AskKernel for KernelBridge {
    async fn whoami(&self) -> Result<PrincipalId, String> {
        self.actor().whoami().await.map(|identity| identity.principal_id).map_err(|e| e.to_string())
    }

    async fn get_ask(&self, request_id: String) -> Result<Option<AskDetail>, String> {
        self.actor().get_ask(request_id).await.map_err(|e| e.to_string())
    }

    async fn decide_ask(&self, request_id: String, verdict: AskVerdict, remember: Option<Remember>) -> Result<AskAnswer, String> {
        self.actor().decide_ask(request_id, verdict, remember).await.map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    /// Bounds one outgoing `session/request_permission` call.
    request_timeout: Duration,
    /// See [`REOFFER_AFTER`].
    reoffer_after: Duration,
}

/// The pump's handle on one raised prompt. Dropping `withdraw` withdraws
/// the prompt; `raise` tells this raise from a later one of the same ask.
struct Raised {
    raise: u64,
    #[allow(dead_code, reason = "held only so that dropping it withdraws the prompt")]
    withdraw: oneshot::Sender<()>,
}

/// A round trip that ended without an answer the ledger took up, sent
/// [`Timing::reoffer_after`] later: the ask id and its raise.
type Unanswered = (String, u64);

async fn pump<K: AskKernel>(
    mut ledger: watch::Receiver<LedgerState>,
    mut binds: watch::Receiver<()>,
    kernel: K,
    sessions: &SessionRegistry,
    cx: ConnectionTo<Client>,
    timing: Timing,
) {
    let mut raised: HashMap<String, Raised> = HashMap::new();
    let mut next_raise = 0u64;
    let (unanswered_tx, mut unanswered) = mpsc::unbounded_channel::<Unanswered>();
    let mut me: Option<PrincipalId> = None;
    loop {
        let state = ledger.borrow_and_update().clone();
        if state.synced {
            if me.is_none() {
                match kernel.whoami().await {
                    Ok(principal) => me = Some(principal),
                    Err(error) => {
                        tracing::warn!(%error, "cannot identify the ACP reviewer; asks wait for the next ledger change");
                    }
                }
            }
            if let Some(me) = me {
                let round = Round { kernel: &kernel, sessions, cx: &cx, timing, unanswered: &unanswered_tx };
                reconcile(&round, &state, me, &mut raised, &mut next_raise);
            }
        }
        tokio::select! {
            changed = ledger.changed() => if changed.is_err() {
                tracing::info!("ledger state closed; permission pump exiting");
                return;
            },
            changed = binds.changed() => if changed.is_err() {
                tracing::info!("session registry closed; permission pump exiting");
                return;
            },
            Some((id, raise)) = unanswered.recv() => {
                // Only the raise that ended; a later raise of the same ask
                // keeps its entry.
                if raised.get(&id).is_some_and(|entry| entry.raise == raise) {
                    raised.remove(&id);
                }
            },
        }
    }
}

/// What one reconcile needs to spawn round trips.
struct Round<'a, K> {
    kernel: &'a K,
    sessions: &'a SessionRegistry,
    cx: &'a ConnectionTo<Client>,
    timing: Timing,
    unanswered: &'a mpsc::UnboundedSender<Unanswered>,
}

/// What one ledger state asks of the pump.
#[derive(Debug, PartialEq, Eq)]
struct LedgerDiff {
    /// Answerable by this connection and not yet raised, oldest key first.
    raise: Vec<AskSummary>,
    /// Raised here and no longer answerable by this connection.
    withdraw: Vec<String>,
}

/// Compare a synced `state` with the prompts raised so far. An ask is ours
/// while it is open and `me` is its reviewer; one that stops being ours,
/// for any reason, is withdrawn.
fn diff_open<V>(state: &LedgerState, me: PrincipalId, raised: &HashMap<String, V>) -> LedgerDiff {
    let ours = |id: &str| state.open.get(id).is_some_and(|ask| ask.answerable_by(me));
    let raise = state.open.values()
        .filter(|ask| ask.answerable_by(me) && !raised.contains_key(&ask.request_id))
        .cloned()
        .collect();
    let mut withdraw: Vec<String> = raised.keys().filter(|id| !ours(id)).cloned().collect();
    withdraw.sort();
    LedgerDiff { raise, withdraw }
}

/// Withdraw every raised prompt that is no longer ours, and spawn a round
/// trip for every new ask this connection may answer.
fn reconcile<K: AskKernel>(
    round: &Round<'_, K>,
    state: &LedgerState,
    me: PrincipalId,
    raised: &mut HashMap<String, Raised>,
    next_raise: &mut u64,
) {
    let LedgerDiff { raise, withdraw } = diff_open(state, me, raised);
    for id in withdraw {
        raised.remove(&id);
    }

    let sessions = round.sessions;
    for ask in raise {
        // A lead can review a coder's ask without attaching ACP directly to
        // the coder context. Prefer that context's session, then send the
        // request through any live session owned by this connection. With
        // no session at all, the ask waits for the next bind.
        let own = ask.context_id.map(rank::session_id_of).filter(|id| sessions.get(id).is_some());
        let Some(session_id) = own.or_else(|| sessions.any_session_id()) else {
            tracing::debug!(request = %ask.request_id, "no ACP session to carry this ask yet");
            continue;
        };
        let Some(session) = sessions.get(&session_id) else { continue };

        *next_raise += 1;
        let raise = *next_raise;
        let (withdraw, mut withdrawn) = oneshot::channel();
        let id = ask.request_id.clone();

        let kernel = round.kernel.clone();
        let cx_task = round.cx.clone();
        let unanswered = round.unanswered.clone();
        let timing = round.timing;
        let task_id = id.clone();
        let spawned = round.cx.spawn(async move {
            let ended = answer_ask(&kernel, &cx_task, &session, session_id, ask, timing.request_timeout, &mut withdrawn).await;
            if ended == RoundTrip::Unanswered {
                // Hold the ask back for a while, then let the pump offer it
                // again if it is still ours. A withdraw ends the wait.
                tokio::select! {
                    biased;
                    _ = &mut withdrawn => {}
                    () = tokio::time::sleep(timing.reoffer_after) => {
                        let _ = unanswered.send((task_id, raise));
                    }
                }
            }
            Ok(())
        });
        match spawned {
            // Not raised: the next ledger change or bind offers it again.
            Err(e) => tracing::warn!(request = %id, error = %e, "failed to spawn permission round trip"),
            Ok(()) => {
                raised.insert(id, Raised { raise, withdraw });
            }
        }
    }
}

/// The tool call a request to a session bound to `session_context` may
/// name: the model call that raised the ask, when that session announced
/// it. A session bound to another context never did.
fn tool_call_for(call: Option<BlockId>, session_context: ContextId) -> Option<BlockId> {
    call.filter(|call| call.context_id == session_context)
}

/// Wait until `session` has announced `call`, so a request naming it
/// arrives after the `tool_call` it names. Bounded by [`ANNOUNCE_WAIT`].
async fn wait_until_announced(session: &Session, call: BlockId, request_id: &str) {
    let deadline = tokio::time::Instant::now() + ANNOUNCE_WAIT;
    while !session.mapper.lock().has_announced(call) {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                request = %request_id,
                call = %call.to_key(),
                "the tool call this ask names was not announced in time; asking anyway"
            );
            return;
        }
        tokio::time::sleep(ANNOUNCE_POLL).await;
    }
}

/// How one round trip ended, for the pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundTrip {
    /// The ledger took up an answer (recorded or refused), or the pump
    /// withdrew the prompt. Nothing more to offer.
    Settled,
    /// No answer reached the ledger, and the ask may still be pending.
    Unanswered,
}

/// Run one ask's round trip: prompt the client, and record an explicit
/// answer with `ActorHandle::decide_ask`. The request names the model call
/// that raised the ask when `session` announced it, and the ask otherwise.
async fn answer_ask<K: AskKernel>(
    kernel: &K,
    cx: &ConnectionTo<Client>,
    session: &Session,
    session_id: SessionId,
    ask: AskSummary,
    request_timeout: Duration,
    withdrawn: &mut oneshot::Receiver<()>,
) -> RoundTrip {
    let request_id = ask.request_id;
    let tool_call = match kernel.get_ask(request_id.clone()).await {
        Ok(Some(detail)) => tool_call_for(detail.tool_call_block_id, session.context_id),
        Ok(None) => {
            tracing::warn!(request = %request_id, "the ledger has no such ask; not offering it now");
            return RoundTrip::Unanswered;
        }
        Err(error) => {
            tracing::warn!(request = %request_id, %error, "cannot read the ask's tool call; the request names the ask instead");
            None
        }
    };
    let title = if ask.description.is_empty() { request_id.clone() } else { ask.description };

    let decision = match prompt(cx, session, &session_id, &request_id, tool_call, title, request_timeout, withdrawn).await {
        Prompted::Answered(decision) => decision,
        Prompted::Withdrawn => {
            tracing::info!(request = %request_id, "ask is no longer ours to answer; prompt withdrawn");
            return RoundTrip::Settled;
        }
        Prompted::LeftPending => return RoundTrip::Unanswered,
    };

    // The ledger checks this connection's principal is the ask's reviewer
    // and records it as a remembered rule's creator. An "always" answer
    // learns an exact-text rule, the same one the tui and app "always" keys
    // learn. A cancelled prompt is this reviewer declining to allow: the
    // ask is denied, so a turn holding on it reads the refusal and goes on.
    let (verdict, remember) = verdict_of(decision);
    let verb = if verdict == AskVerdict::Allow { "allow" } else { "deny" };
    match kernel.decide_ask(request_id.clone(), verdict, remember).await {
        Ok(Ok(answered)) => {
            tracing::info!(request = %request_id, ?verdict, remember = remember.is_some(), ask_answer = "recorded", "ledger ask answered");
            if let Some(reason) = rule_not_learned(answered.remembered.as_ref()) {
                tracing::info!(request = %request_id, verb, %reason, "the answer stands; no rule was remembered");
                notify_decision_failure(cx, &session_id, remember_failure_message(&request_id, verb, reason));
            }
            RoundTrip::Settled
        }
        Ok(Err(failure)) => {
            // Another surface answering first is normal; every other
            // refusal (including an ineligible reviewer) needs its reason
            // to diagnose the contract.
            if failure.kind == AskAnswerFailureKind::AlreadyAnswered {
                tracing::info!(request = %request_id, verb, %failure, ask_answer = "refused", "another answer got there first");
            } else {
                tracing::warn!(request = %request_id, verb, kind = ?failure.kind, %failure, ask_answer = "refused", "the ledger refused the answer");
            }
            notify_decision_failure(cx, &session_id, decision_failure_message(&request_id, verb, &failure.message));
            RoundTrip::Settled
        }
        Err(error) => {
            tracing::warn!(request = %request_id, verb, %error, "answering the ask failed; it stays pending");
            notify_decision_failure(cx, &session_id, decision_failure_message(&request_id, verb, &error));
            RoundTrip::Unanswered
        }
    }
}

/// How one prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prompted {
    /// The client answered with an option this bridge offered, or cancelled.
    Answered(Decision),
    /// The pump withdrew the prompt; any outstanding request was cancelled.
    Withdrawn,
    /// The prompt failed or the answer could not be placed; the ask stays
    /// pending.
    LeftPending,
}

/// Offer the ask to the client until it answers, the prompt fails, or the
/// pump withdraws it, including while it waits for `tool_call` to be
/// announced. A request that outlives `timeout` is cancelled and
/// sent again. Each request is sent under the session's emission lock, so
/// any `tool_call` the mapper recorded as sent is on the wire before it.
#[allow(clippy::too_many_arguments)]
async fn prompt(
    cx: &ConnectionTo<Client>,
    session: &Session,
    session_id: &SessionId,
    request_id: &str,
    tool_call: Option<BlockId>,
    title: String,
    timeout: Duration,
    withdrawn: &mut oneshot::Receiver<()>,
) -> Prompted {
    if let Some(call) = tool_call {
        tokio::select! {
            biased;
            _ = &mut *withdrawn => return Prompted::Withdrawn,
            () = wait_until_announced(session, call, request_id) => {}
        }
    }
    loop {
        if !matches!(withdrawn.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return Prompted::Withdrawn;
        }
        let (options, kinds) = build_options();
        let request = permission_request(session_id.clone(), tool_call, request_id, title.clone(), options);
        let sent = {
            let _emission = session.emission.lock();
            cx.send_request(request)
        };
        // Dropping the pending response sends `$/cancel_request` for it.
        let response = tokio::select! {
            biased;
            _ = &mut *withdrawn => return Prompted::Withdrawn,
            response = tokio::time::timeout(timeout, sent.block_task()) => response,
        };
        match response {
            Ok(Ok(response)) => {
                return match map_response(&response, &kinds) {
                    Some(decision) => Prompted::Answered(decision),
                    None => {
                        tracing::warn!(request = %request_id, "the client chose an option this bridge never offered; leaving the ask pending");
                        Prompted::LeftPending
                    }
                };
            }
            Ok(Err(e)) => {
                tracing::warn!(request = %request_id, error = %e, "permission request failed; leaving the ask pending");
                notify_decision_failure(cx, session_id, permission_prompt_failure_message(request_id, &e.to_string()));
                return Prompted::LeftPending;
            }
            Err(_) => {
                tracing::warn!(
                    request = %request_id,
                    ?timeout,
                    "permission request timed out waiting on the client; offering it again"
                );
                notify_decision_failure(cx, session_id, permission_prompt_failure_message(request_id, "timed out waiting for the client"));
            }
        }
    }
}

/// The ledger answer a decision records. "Always" learns an exact-text
/// rule, never a family rule.
fn verdict_of(decision: Decision) -> (AskVerdict, Option<Remember>) {
    let always = |remember: bool| remember.then_some(Remember { scope: RememberScope::Always, family: false });
    match decision {
        Decision::Allow { remember } => (AskVerdict::Allow, always(remember)),
        Decision::Deny { remember } => (AskVerdict::Deny, always(remember)),
        Decision::Cancelled => (AskVerdict::PromptCancelled, None),
    }
}

/// Ids for the options this bridge always offers: the kernel's ledger asks
/// carry no `options` of their own, so this is the only shape a client is
/// ever offered. "Always" answers with a remembered rule ([`verdict_of`]).
const OPT_ALLOW: &str = "allow";
const OPT_ALLOW_ALWAYS: &str = "allow-always";
const OPT_DENY: &str = "deny";
const OPT_DENY_ALWAYS: &str = "deny-always";

type OptionKinds = [(&'static str, PermissionOptionKind); 4];

/// A client's answer: allow or deny, and whether to remember it as a
/// standing rule, or a cancelled prompt, which denies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Allow { remember: bool },
    Deny { remember: bool },
    Cancelled,
}

/// Build the fixed ACP options, plus the id→kind map [`map_response`] needs
/// to turn a selected id back into a decision.
fn build_options() -> (Vec<PermissionOption>, OptionKinds) {
    let kinds = [
        (OPT_ALLOW, PermissionOptionKind::AllowOnce),
        (OPT_ALLOW_ALWAYS, PermissionOptionKind::AllowAlways),
        (OPT_DENY, PermissionOptionKind::RejectOnce),
        (OPT_DENY_ALWAYS, PermissionOptionKind::RejectAlways),
    ];
    let options = vec![
        PermissionOption::new(OPT_ALLOW, "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(OPT_ALLOW_ALWAYS, "Always allow this command", PermissionOptionKind::AllowAlways),
        PermissionOption::new(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce),
        PermissionOption::new(OPT_DENY_ALWAYS, "Always deny this command", PermissionOptionKind::RejectAlways),
    ];
    (options, kinds)
}

/// Shape the outgoing `session/request_permission`. `toolCall.toolCallId`
/// names the model call that raised the ask, so a client attaches the
/// request to that call; an ask no model call raised names itself. The ask
/// id is always in `_meta.kaijutsu.askId`.
fn permission_request(
    session_id: SessionId,
    tool_call: Option<BlockId>,
    request_id: &str,
    title: impl Into<String>,
    options: Vec<PermissionOption>,
) -> RequestPermissionRequest {
    let tool_call_id = match tool_call {
        Some(call) => crate::update::tool_call_id(call),
        None => request_id.to_string().into(),
    };
    let mut meta = serde_json::Map::new();
    meta.insert("kaijutsu".into(), serde_json::json!({ "askId": request_id }));
    RequestPermissionRequest::new(
        session_id,
        ToolCallUpdate::new(tool_call_id, ToolCallUpdateFields::new().title(title.into())),
        options,
    )
    .meta(meta)
}

/// Read a client's answer against the id→kind map [`build_options`] built
/// for this ask. A cancelled prompt is an answer. Anything else this cannot place
/// — an unrecognised option id, a future outcome, or a future
/// `PermissionOptionKind` variant the `#[non_exhaustive]` wire types gain
/// later — leaves the durable ask pending. Only an option this bridge
/// explicitly offered, or a cancellation, records a verdict.
fn map_response(response: &RequestPermissionResponse, kinds: &OptionKinds) -> Option<Decision> {
    let selected = match &response.outcome {
        RequestPermissionOutcome::Selected(selected) => selected,
        RequestPermissionOutcome::Cancelled => return Some(Decision::Cancelled),
        _ => return None,
    };
    let id = selected.option_id.0.as_ref();
    match kinds.iter().find(|(k, _)| *k == id).map(|(_, kind)| kind) {
        Some(PermissionOptionKind::AllowOnce) => Some(Decision::Allow { remember: false }),
        Some(PermissionOptionKind::AllowAlways) => Some(Decision::Allow { remember: true }),
        Some(PermissionOptionKind::RejectOnce) => Some(Decision::Deny { remember: false }),
        Some(PermissionOptionKind::RejectAlways) => Some(Decision::Deny { remember: true }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionId, SelectedPermissionOutcome};
    use kaijutsu_types::{AskOrigin, AskStatus, PrincipalRef};

    fn selected(response: &str) -> RequestPermissionResponse {
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PermissionOptionId::new(response.to_string())),
        ))
    }

    #[test]
    fn build_options_offers_once_and_always_for_allow_and_deny() {
        let (options, kinds) = build_options();
        let offered: Vec<(&str, PermissionOptionKind)> =
            options.iter().map(|o| (o.option_id.0.as_ref(), o.kind)).collect();
        assert_eq!(
            offered,
            vec![
                (OPT_ALLOW, PermissionOptionKind::AllowOnce),
                (OPT_ALLOW_ALWAYS, PermissionOptionKind::AllowAlways),
                (OPT_DENY, PermissionOptionKind::RejectOnce),
                (OPT_DENY_ALWAYS, PermissionOptionKind::RejectAlways),
            ]
        );
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn selecting_always_remembers_the_decision() {
        let (_, kinds) = build_options();
        assert_eq!(map_response(&selected(OPT_ALLOW_ALWAYS), &kinds), Some(Decision::Allow { remember: true }));
        assert_eq!(map_response(&selected(OPT_DENY_ALWAYS), &kinds), Some(Decision::Deny { remember: true }));
    }

    #[test]
    fn a_rule_the_ledger_refused_to_learn_is_reported() {
        let refused = RememberResult { learned: false, note: "statement 1 has a free variable $DIR".into() };
        assert_eq!(rule_not_learned(Some(&refused)), Some("statement 1 has a free variable $DIR"));
        let learned = RememberResult { learned: true, note: "learned 1 rule".into() };
        assert_eq!(rule_not_learned(Some(&learned)), None);
        assert_eq!(rule_not_learned(None), None);
        assert_eq!(
            remember_failure_message("req-1", "allow", "has a free variable"),
            "Approval allow for req-1 applied to this ask only; no standing rule was remembered: has a free variable"
        );
    }

    #[test]
    fn the_request_names_the_session_and_carries_every_option() {
        let (options, _) = build_options();
        let req = permission_request(SessionId::new("s"), None, "req-1", "rm -rf /", options);
        assert_eq!(req.session_id, SessionId::new("s"));
        assert_eq!(req.tool_call.tool_call_id.0.as_ref(), "req-1", "an ask no model call raised names itself");
        assert_eq!(req.tool_call.fields.title.as_deref(), Some("rm -rf /"));
        assert_eq!(req.options.len(), 4);
    }

    #[test]
    fn the_request_names_the_model_call_and_carries_the_ask_id() {
        let (options, _) = build_options();
        let call = BlockId::new(ContextId::new(), PrincipalId::new(), 3);
        let req = permission_request(SessionId::new("s"), Some(call), "req-1", "mkdir x", options);
        assert_eq!(req.tool_call.tool_call_id, crate::update::tool_call_id(call));
        let meta = serde_json::Value::Object(req.meta.expect("the request carries its ask"));
        assert_eq!(meta["kaijutsu"]["askId"], "req-1");
    }

    #[test]
    fn a_call_is_named_only_to_the_session_of_its_own_context() {
        let call = BlockId::new(ContextId::new(), PrincipalId::new(), 3);
        assert_eq!(tool_call_for(Some(call), call.context_id), Some(call));
        assert_eq!(tool_call_for(Some(call), ContextId::new()), None, "another session never announced it");
        assert_eq!(tool_call_for(None, call.context_id), None);
    }

    #[test]
    fn selecting_allow_maps_to_true() {
        let (_, kinds) = build_options();
        assert_eq!(map_response(&selected(OPT_ALLOW), &kinds), Some(Decision::Allow { remember: false }));
    }

    #[test]
    fn selecting_deny_maps_to_false() {
        let (_, kinds) = build_options();
        assert_eq!(map_response(&selected(OPT_DENY), &kinds), Some(Decision::Deny { remember: false }));
    }

    #[test]
    fn decision_failures_name_the_action_and_kernel_reason() {
        assert_eq!(
            decision_failure_message("req-1", "allow", "already decided"),
            "Approval allow for req-1 did not apply: already decided"
        );
    }

    #[test]
    fn prompt_failures_tell_the_client_that_the_ask_remains_pending() {
        assert_eq!(
            permission_prompt_failure_message("req-1", "timed out waiting for the client"),
            "Approval prompt for req-1 failed: timed out waiting for the client; ask remains pending"
        );
    }

    #[test]
    fn a_cancelled_prompt_denies_the_ask() {
        let r = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        let (_, kinds) = build_options();
        assert_eq!(map_response(&r, &kinds), Some(Decision::Cancelled));
    }

    #[test]
    fn an_option_id_we_never_offered_leaves_the_ask_pending() {
        let (_, kinds) = build_options();
        assert_eq!(map_response(&selected("some-future-option"), &kinds), None);
    }

    fn summary(id: &str, reviewer: Option<PrincipalId>) -> AskSummary {
        let reference = |id: PrincipalId| PrincipalRef { id, name: id.to_string() };
        AskSummary {
            request_id: id.into(), status: AskStatus::Pending, origin: AskOrigin::ShellGate,
            context_id: Some(ContextId::new()), description: format!("run {id}"), statements: Vec::new(),
            requester: None, performer: Some(reference(PrincipalId::new())), reviewer: reviewer.map(reference),
            created_at_ms: 0, decided_at_ms: None,
        }
    }

    fn state(asks: Vec<AskSummary>) -> LedgerState {
        LedgerState::from_listing(1, asks)
    }

    fn raised(ids: &[&str]) -> HashMap<String, ()> {
        ids.iter().map(|id| (id.to_string(), ())).collect()
    }

    #[test]
    fn a_new_ask_this_reviewer_may_answer_is_raised_once() {
        let me = PrincipalId::new();
        let open = state(vec![summary("a", Some(me)), summary("b", Some(PrincipalId::new())), summary("c", None)]);
        let diff = diff_open(&open, me, &raised(&[]));
        assert_eq!(diff.raise.iter().map(|ask| ask.request_id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        assert!(diff.withdraw.is_empty());
        let again = diff_open(&open, me, &raised(&["a"]));
        assert!(again.raise.is_empty(), "a raised ask is not offered twice");
        assert!(again.withdraw.is_empty());
    }

    #[test]
    fn an_ask_that_left_the_queue_is_withdrawn() {
        let me = PrincipalId::new();
        let diff = diff_open(&state(vec![summary("b", Some(me))]), me, &raised(&["a", "b"]));
        assert_eq!(diff.withdraw, vec!["a".to_string()]);
        assert!(diff.raise.is_empty());
    }

    #[test]
    fn an_ask_reassigned_to_another_reviewer_is_withdrawn() {
        let me = PrincipalId::new();
        let diff = diff_open(&state(vec![summary("a", Some(PrincipalId::new()))]), me, &raised(&["a"]));
        assert_eq!(diff.withdraw, vec!["a".to_string()]);
    }

    #[test]
    fn a_claimed_ask_is_still_open_and_still_ours() {
        let me = PrincipalId::new();
        let mut ask = summary("a", Some(me));
        ask.status = AskStatus::Claimed;
        let diff = diff_open(&state(vec![ask]), me, &raised(&["a"]));
        assert_eq!(diff, LedgerDiff { raise: Vec::new(), withdraw: Vec::new() });
    }

    #[test]
    fn always_remembers_an_exact_rule_and_a_cancelled_prompt_records_its_own_verdict() {
        let exact = Some(Remember { scope: RememberScope::Always, family: false });
        assert_eq!(verdict_of(Decision::Allow { remember: false }), (AskVerdict::Allow, None));
        assert_eq!(verdict_of(Decision::Allow { remember: true }), (AskVerdict::Allow, exact));
        assert_eq!(verdict_of(Decision::Deny { remember: false }), (AskVerdict::Deny, None));
        assert_eq!(verdict_of(Decision::Deny { remember: true }), (AskVerdict::Deny, exact));
        assert_eq!(verdict_of(Decision::Cancelled), (AskVerdict::PromptCancelled, None));
    }

    /// What a test client saw: each permission request's ask id, and each
    /// one the agent cancelled.
    #[derive(Default)]
    struct Seen {
        requests: Vec<String>,
        cancelled: Vec<String>,
        /// The agent's marker sent after `prompt` returned. Messages arrive
        /// in order, so every request the prompt sent was seen before it.
        finished: bool,
    }

    const FINISHED: &str = "prompt-finished";

    fn ask_id(request: &RequestPermissionRequest) -> String {
        request.meta.as_ref().and_then(|meta| meta["kaijutsu"]["askId"].as_str()).unwrap_or_default().to_string()
    }

    /// Run `prompt` for ask "req-1" on an agent wired to a client that
    /// answers request `n` (zero-based) with `answer(n)`, or holds it
    /// unanswered when that is `None`. `drive` runs on the client side with
    /// the withdraw handle, which is `None` when `withdrawn_at_start`
    /// withdrew the prompt before the agent started, and returns once its
    /// checks are done.
    async fn prompt_against_client(
        timeout: Duration,
        tool_call: Option<BlockId>,
        withdrawn_at_start: bool,
        answer: impl Fn(usize) -> Option<RequestPermissionResponse> + Send + Sync + 'static,
        drive: impl AsyncFnOnce(Arc<std::sync::Mutex<Seen>>, Option<oneshot::Sender<()>>, oneshot::Receiver<Prompted>) + Send + 'static,
    ) {
        use agent_client_protocol::{Agent, Responder};

        let seen: Arc<std::sync::Mutex<Seen>> = Arc::default();
        let held: Arc<std::sync::Mutex<Vec<Responder<RequestPermissionResponse>>>> = Arc::default();
        let (withdraw, withdrawn) = oneshot::channel::<()>();
        let withdraw = (!withdrawn_at_start).then_some(withdraw);
        let (outcome_tx, outcome_rx) = oneshot::channel::<Prompted>();
        let withdrawn = std::sync::Mutex::new(Some(withdrawn));
        let outcome_tx = std::sync::Mutex::new(Some(outcome_tx));

        let agent = Agent.builder().name("permission-test").with_spawned(move |cx: ConnectionTo<Client>| {
            let mut withdrawn = withdrawn.lock().unwrap().take().expect("spawned once");
            let outcome_tx = outcome_tx.lock().unwrap().take().expect("spawned once");
            async move {
                let session_id = SessionId::new("s");
                let session = Session::new(ContextId::new(), "test".into(), crate::update::UpdateMapper::new(session_id.clone()), Vec::new());
                let outcome = prompt(&cx, &session, &session_id, "req-1", tool_call, "mkdir x".into(), timeout, &mut withdrawn).await;
                notify_decision_failure(&cx, &session_id, FINISHED.into());
                let _ = outcome_tx.send(outcome);
                Ok(())
            }
        });

        let seen_handler = Arc::clone(&seen);
        let seen_notes = Arc::clone(&seen);
        let seen_drive = Arc::clone(&seen);
        Client
            .builder()
            .name("permission-test-client")
            .on_receive_request(
                async move |request: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            cx: ConnectionTo<Agent>| {
                    let id = ask_id(&request);
                    let n = {
                        let mut seen = seen_handler.lock().unwrap();
                        seen.requests.push(id.clone());
                        seen.requests.len() - 1
                    };
                    if let Some(response) = answer(n) {
                        return responder.respond(response);
                    }
                    let cancellation = responder.cancellation();
                    held.lock().unwrap().push(responder);
                    let seen = Arc::clone(&seen_handler);
                    cx.spawn(async move {
                        cancellation.cancelled().await;
                        seen.lock().unwrap().cancelled.push(id);
                        Ok(())
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |note: SessionNotification, _cx: ConnectionTo<Agent>| {
                    if format!("{:?}", note.update).contains(FINISHED) {
                        seen_notes.lock().unwrap().finished = true;
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, async move |_cx| {
                drive(seen_drive, withdraw, outcome_rx).await;
                Ok(())
            })
            .await
            .expect("the test connection runs");
    }

    async fn finished(outcome: oneshot::Receiver<Prompted>) -> Prompted {
        tokio::time::timeout(Duration::from_secs(5), outcome)
            .await
            .expect("the prompt finished in time")
            .expect("the prompt reported its outcome")
    }

    async fn until(seen: &std::sync::Mutex<Seen>, done: impl Fn(&Seen) -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done(&seen.lock().unwrap()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the client saw what the test waited for");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_withdrawn_prompt_cancels_its_request_and_records_nothing() {
        prompt_against_client(Duration::from_secs(30), None, false, |_| None, async |seen, withdraw, outcome| {
            until(&seen, |seen| seen.requests.len() == 1).await;
            drop(withdraw);
            assert_eq!(finished(outcome).await, Prompted::Withdrawn);
            until(&seen, |seen| seen.finished && seen.cancelled == ["req-1"]).await;
            assert_eq!(seen.lock().unwrap().requests, ["req-1"], "a withdrawn ask is not offered again");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_withdrawn_before_it_is_sent_sends_nothing() {
        prompt_against_client(Duration::from_secs(30), None, true, |_| None, async |seen, _withdraw, outcome| {
            assert_eq!(finished(outcome).await, Prompted::Withdrawn);
            until(&seen, |seen| seen.finished).await;
            assert!(seen.lock().unwrap().requests.is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_withdrawn_while_its_call_is_unannounced_ends_at_once() {
        // The session never announces this call, so only the withdraw can
        // end the wait before ANNOUNCE_WAIT.
        let call = BlockId::new(ContextId::new(), PrincipalId::new(), 3);
        prompt_against_client(Duration::from_secs(30), Some(call), false, |_| None, async |seen, withdraw, outcome| {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(withdraw);
            let outcome = tokio::time::timeout(ANNOUNCE_WAIT / 5, outcome)
                .await
                .expect("a withdrawn ask stops waiting for its call's announcement")
                .expect("the prompt reported its outcome");
            assert_eq!(outcome, Prompted::Withdrawn);
            until(&seen, |seen| seen.finished).await;
            assert!(seen.lock().unwrap().requests.is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_timed_out_request_is_cancelled_and_the_ask_offered_again() {
        let answer = |n: usize| (n == 1).then(|| selected(OPT_ALLOW));
        prompt_against_client(Duration::from_millis(100), None, false, answer, async |seen, _withdraw, outcome| {
            assert_eq!(finished(outcome).await, Prompted::Answered(Decision::Allow { remember: false }));
            until(&seen, |seen| seen.cancelled == ["req-1"]).await;
            assert_eq!(seen.lock().unwrap().requests, ["req-1", "req-1"]);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancelled_prompt_is_an_answer() {
        let answer = |_| Some(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
        prompt_against_client(Duration::from_secs(30), None, false, answer, async |_seen, _withdraw, outcome| {
            assert_eq!(finished(outcome).await, Prompted::Answered(Decision::Cancelled));
        })
        .await;
    }

    /// A kernel whose answers the test scripts: `get_ask` returns `None`
    /// for its first `missing` calls, and `decide_ask` records each verdict.
    #[derive(Clone)]
    struct ScriptedKernel {
        me: PrincipalId,
        ask: AskSummary,
        missing: Arc<std::sync::Mutex<usize>>,
        decided: Arc<std::sync::Mutex<Vec<AskVerdict>>>,
    }

    impl AskKernel for ScriptedKernel {
        async fn whoami(&self) -> Result<PrincipalId, String> {
            Ok(self.me)
        }

        async fn get_ask(&self, _request_id: String) -> Result<Option<AskDetail>, String> {
            let mut missing = self.missing.lock().unwrap();
            if *missing > 0 {
                *missing -= 1;
                return Ok(None);
            }
            Ok(Some(AskDetail {
                summary: self.ask.clone(), instance: None, tool: None, hook_id: None, label: None,
                tool_call_block_id: None, exec_source: None, cwd: None, env: Vec::new(), decision: None,
                redeemed_at_ms: None, publication_abandoned: None, reassignments: Vec::new(),
            }))
        }

        async fn decide_ask(&self, _request_id: String, verdict: AskVerdict, _remember: Option<Remember>) -> Result<AskAnswer, String> {
            self.decided.lock().unwrap().push(verdict);
            Ok(Ok(kaijutsu_types::AskAnswered { summary: self.ask.clone(), remembered: None }))
        }
    }

    /// Run the pump over one open ask this connection may answer, with a
    /// session bound to the ask's context, against a client that answers
    /// request `n` (zero-based) with `answer(n)`: `Some(Ok)` a response,
    /// `Some(Err)` a JSON-RPC error. `drive` runs on the client side and
    /// returns once its checks are done.
    async fn pump_against_client(
        missing: usize,
        answer: impl Fn(usize) -> Result<RequestPermissionResponse, ()> + Send + Sync + 'static,
        drive: impl AsyncFnOnce(Arc<std::sync::Mutex<Seen>>, Arc<std::sync::Mutex<Vec<AskVerdict>>>) + Send + 'static,
    ) {
        use agent_client_protocol::{Agent, Responder};

        let me = PrincipalId::new();
        let mut ask = summary("req-1", Some(me));
        let context = ContextId::new();
        ask.context_id = Some(context);
        let kernel = ScriptedKernel {
            me,
            ask: ask.clone(),
            missing: Arc::new(std::sync::Mutex::new(missing)),
            decided: Arc::default(),
        };
        let decided = Arc::clone(&kernel.decided);
        let timing = Timing { request_timeout: Duration::from_secs(30), reoffer_after: Duration::from_millis(100) };

        let agent = Agent.builder().name("pump-test").with_spawned(move |cx: ConnectionTo<Client>| async move {
            let (_ledger_tx, ledger) = watch::channel(LedgerState::from_listing(1, vec![ask]));
            let sessions = SessionRegistry::default();
            let binds = sessions.subscribe_binds();
            let session_id = rank::session_id_of(context);
            let mapper = crate::update::UpdateMapper::new(session_id.clone());
            sessions.bind(session_id, Session::new(context, "test".into(), mapper, Vec::new()));
            pump(ledger, binds, kernel, &sessions, cx, timing).await;
            Ok(())
        });

        let seen: Arc<std::sync::Mutex<Seen>> = Arc::default();
        let seen_handler = Arc::clone(&seen);
        Client
            .builder()
            .name("pump-test-client")
            .on_receive_request(
                async move |request: RequestPermissionRequest,
                            responder: Responder<RequestPermissionResponse>,
                            _cx: ConnectionTo<Agent>| {
                    let n = {
                        let mut seen = seen_handler.lock().unwrap();
                        seen.requests.push(ask_id(&request));
                        seen.requests.len() - 1
                    };
                    match answer(n) {
                        Ok(response) => responder.respond(response),
                        Err(()) => responder.respond_with_error(agent_client_protocol::util::internal_error("client fault")),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, async move |_cx| {
                drive(seen, decided).await;
                Ok(())
            })
            .await
            .expect("the test connection runs");
    }

    async fn until_decided(decided: &std::sync::Mutex<Vec<AskVerdict>>, verdicts: &[AskVerdict]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while decided.lock().unwrap().as_slice() != verdicts {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the pump recorded the expected answers");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_that_failed_in_transport_is_offered_again() {
        let answer = |n: usize| if n == 0 { Err(()) } else { Ok(selected(OPT_ALLOW)) };
        pump_against_client(0, answer, async |seen, decided| {
            until_decided(&decided, &[AskVerdict::Allow]).await;
            assert_eq!(seen.lock().unwrap().requests, ["req-1", "req-1"]);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_ask_the_ledger_could_not_read_is_offered_again() {
        pump_against_client(1, |_| Ok(selected(OPT_DENY)), async |seen, decided| {
            until_decided(&decided, &[AskVerdict::Deny]).await;
            assert_eq!(seen.lock().unwrap().requests, ["req-1"]);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_repeating_failure_is_offered_again_at_the_reoffer_pace() {
        pump_against_client(0, |_| Err(()), async |seen, decided| {
            tokio::time::sleep(Duration::from_millis(450)).await;
            let offers = seen.lock().unwrap().requests.len();
            assert!((2..=6).contains(&offers), "{offers} offers in 450ms at one per 100ms");
            assert!(decided.lock().unwrap().is_empty(), "a failed prompt records nothing");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_answered_ask_is_not_offered_again() {
        pump_against_client(0, |_| Ok(selected(OPT_ALLOW)), async |seen, decided| {
            until_decided(&decided, &[AskVerdict::Allow]).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(seen.lock().unwrap().requests, ["req-1"]);
            assert_eq!(decided.lock().unwrap().as_slice(), [AskVerdict::Allow]);
        })
        .await;
    }
}
