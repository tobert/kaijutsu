//! The approval ledger, mirrored app-side: one resource holding the kernel's
//! pending asks plus the last few that were decided, for every renderer that
//! wants to show them (`docs/approval-identity.md`).
//!
//! One mirror, many readers. [`LedgerMirror`] is the only place in the app
//! that talks to `kj ledger list`/`show`, so a lamp, a dock badge, and a
//! future review panel all read the same pending set and cannot disagree
//! about it. The shared client model does the wire work
//! (`kaijutsu_client::ledger`); nothing here parses ledger JSON.
//!
//! **A round runs only when the kernel says the ledger changed.** The
//! kernel's own change stream (`ActorHandle::subscribe_ledger_events`) hands
//! us a generation number per change; a bump sets [`LedgerMirror::dirty`] and
//! one round follows. Connecting and a stream lag also set it, because
//! neither tells us what we missed. There is no timer and no per-frame RPC:
//! a round is `kj ledger list` plus one `kj ledger show` per ask that
//! arrived or left, run in the app's current context.
//!
//! **Rounds are single-flight.** [`LedgerMirror::in_flight`] holds while one
//! runs, and a bump arriving during it leaves `dirty` set, so exactly one
//! more round follows however many bumps land. The generation is a hint to
//! re-read, never a cursor: every round lists the whole pending set and the
//! kernel's answer wins.
//!
//! **A result from a replaced connection is discarded.** Each round carries
//! the actor generation and connection epoch it started under, and the drain
//! drops anything else — a reply queued before a reconnect must not overwrite
//! the mirror the new connection rebuilt (`docs/issues.md`, "App bootstrap
//! results need consistent connection scoping").
//!
//! **Design split**: the round plan, the fold, and the dirty/in-flight latch
//! are pure and unit-tested with no Bevy app; only the three systems at the
//! bottom touch `bevy` or the RPC handle (`view::room::switchboard`'s stance).

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use kaijutsu_client::AskDetail;
use kaijutsu_types::ContextId;
use tokio::sync::broadcast;

use super::actor_plugin::{RpcActor, RpcConnectionState};

/// How many decided asks the mirror keeps behind the pending set. Enough for
/// a renderer to show what just happened; the ledger itself is the history
/// (`kj ledger list --history`).
pub const RECENT_CAP: usize = 3;

/// How many rounds in a row may fail before the mirror stops retrying and
/// waits for the next generation bump or connection. A failing kernel must
/// not be re-read on every frame, and there is no timer here to slow a
/// retry down — the budget is the whole mechanism.
pub const RETRY_LIMIT: u32 = 3;

// ============================================================================
// The mirror
// ============================================================================

/// The kernel's approval ledger as this app last read it.
///
/// `pending` is in the order `kj ledger list` returned, so every renderer
/// shows asks in ledger order rather than inventing one. `recent` holds asks
/// that left the pending set, most recent first, each re-read so its decision
/// is known.
#[derive(Resource, Default, Debug)]
pub struct LedgerMirror {
    /// The newest ledger generation the change stream reported, or `None`
    /// before the first change and after a connection is replaced.
    pub generation: Option<i64>,
    /// Every pending ask, in ledger order.
    pub pending: Vec<AskDetail>,
    /// Asks that left the pending set, most recent first, capped at
    /// [`RECENT_CAP`].
    pub recent: Vec<AskDetail>,
    /// A round is owed: the ledger changed, the connection is new, or the
    /// last round could not read every ask.
    pub dirty: bool,
    /// A round is running. Cleared by that round's own result.
    pub in_flight: bool,
    /// Why the last round failed, for a renderer to show. Cleared by the
    /// next round that succeeds.
    pub last_error: Option<String>,
    /// Rounds that have failed since the last one that succeeded. Reset by a
    /// generation bump, a new connection, and a round that reads everything.
    retries: u32,
}

impl LedgerMirror {
    /// How many asks are waiting for a decision, across every context.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The pending asks raised in one context, in ledger order.
    pub fn pending_for_context(&self, context_id: ContextId) -> impl Iterator<Item = &AskDetail> {
        self.pending
            .iter()
            .filter(move |ask| ask.context_id == Some(context_id))
    }

    /// Whether any ask in `context_id` is waiting — the switchboard lamp's
    /// one question (`view::room::switchboard`).
    pub fn context_is_asking(&self, context_id: ContextId) -> bool {
        self.pending_for_context(context_id).next().is_some()
    }

    /// One ask by request id, pending first, then the decided few. `None`
    /// means this mirror has never held it, not that the ledger lacks it.
    pub fn get(&self, request_id: &str) -> Option<&AskDetail> {
        self.pending
            .iter()
            .chain(self.recent.iter())
            .find(|ask| ask.request_id == request_id)
    }

    /// Whether a round should start now. The caller adds its own conditions:
    /// a live connection and a current context to run `kj` in.
    pub fn should_start_round(&self) -> bool {
        self.dirty && !self.in_flight
    }

    /// A ledger generation arrived. Every bump earns a round, including one
    /// that lands mid-round: `dirty` stays set through
    /// [`Self::on_round_start`], so exactly one more round follows. A bump is
    /// also a fresh [`RETRY_LIMIT`] budget — whatever was failing may not be
    /// failing now.
    pub fn on_generation(&mut self, generation: i64) {
        self.generation = Some(match self.generation {
            Some(known) => known.max(generation),
            None => generation,
        });
        self.dirty = true;
        self.retries = 0;
    }

    /// This connection can no longer answer for the mirror: a round owed, no
    /// known generation, and any round still running disowned. Its result is
    /// discarded by the drain, so the latch is cleared here instead.
    ///
    /// The pending asks stay until the next round replaces them. A reconnect
    /// rebinds the same kernel, so the last-known set is the best answer
    /// available while the round runs, and the lamps do not blink.
    pub fn on_connection_changed(&mut self) {
        self.generation = None;
        self.dirty = true;
        self.in_flight = false;
        self.retries = 0;
    }

    /// The change stream dropped generations: a round is owed and the retry
    /// budget starts over, because what was missed is unknown.
    pub fn on_stream_gap(&mut self) {
        self.dirty = true;
        self.retries = 0;
    }

    /// A round is starting. `dirty` clears here so a bump during the round
    /// sets it again.
    pub fn on_round_start(&mut self) {
        self.dirty = false;
        self.in_flight = true;
    }

    /// A round finished. `retry` asks for one more round — an ask this round
    /// could not read, or a round that failed outright.
    ///
    /// Retries are bounded by [`RETRY_LIMIT`]: past it the mirror goes quiet
    /// and keeps [`Self::last_error`], rather than re-reading a failing
    /// ledger for as long as the app runs. The next bump or connection
    /// starts again.
    pub fn on_round_done(&mut self, retry: bool) {
        self.in_flight = false;
        if !retry {
            self.retries = 0;
            return;
        }
        self.retries += 1;
        self.dirty = self.retries < RETRY_LIMIT;
    }
}

// ============================================================================
// Pure: the round
// ============================================================================

/// What one round has to fetch, from the pending ids the mirror holds and the
/// ids `kj ledger list` just returned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoundPlan {
    /// Listed ids the mirror has no detail for, in listed order.
    pub fetch_new: Vec<String>,
    /// Ids the mirror holds that the ledger no longer lists, in mirror order.
    /// Read once more so `recent` carries the decision.
    pub departed: Vec<String>,
    /// Listed ids the mirror already holds, in listed order. Their detail is
    /// reused rather than re-read: a pending ask's fields do not change.
    pub keep: Vec<String>,
}

/// Plan one round: which listed asks are new, which held asks are gone, and
/// which are unchanged.
pub fn plan_round(current_pending_ids: &[String], listed_ids: &[String]) -> RoundPlan {
    let held: std::collections::HashSet<&str> =
        current_pending_ids.iter().map(String::as_str).collect();
    let listed: std::collections::HashSet<&str> = listed_ids.iter().map(String::as_str).collect();
    let mut plan = RoundPlan::default();
    for id in listed_ids {
        if held.contains(id.as_str()) {
            plan.keep.push(id.clone());
        } else {
            plan.fetch_new.push(id.clone());
        }
    }
    for id in current_pending_ids {
        if !listed.contains(id.as_str()) {
            plan.departed.push(id.clone());
        }
    }
    plan
}

/// One finished round's reads. `None` in either detail list is a
/// `kj ledger show` that could not be read this round.
#[derive(Debug, Default)]
pub struct RoundData {
    /// Every pending request id, in the order `kj ledger list` returned.
    pub listed: Vec<String>,
    /// Detail for the ids [`RoundPlan::fetch_new`] named.
    pub fetched: Vec<(String, Option<AskDetail>)>,
    /// Detail for the ids [`RoundPlan::departed`] named.
    pub departed: Vec<(String, Option<AskDetail>)>,
}

/// Fold a finished round into the mirror, and return the pending ids it could
/// not resolve.
///
/// The new pending list is in listed order: a fetched detail for a new ask, a
/// retained one for an ask the mirror already held. An ask whose
/// `kj ledger show` failed is left out of `pending` and named in the return,
/// so the caller can set `dirty` and retry it rather than hide a waiting ask
/// for good.
///
/// Departed asks go to the front of `recent`, newest decision first, capped
/// at [`RECENT_CAP`]. A departed ask whose read failed is dropped: the ledger
/// has already said it is not pending, and no later round will plan it again.
pub fn apply_round(mirror: &mut LedgerMirror, round: RoundData) -> Vec<String> {
    let RoundData {
        listed,
        fetched,
        departed,
    } = round;

    let mut held: std::collections::HashMap<String, AskDetail> = std::mem::take(&mut mirror.pending)
        .into_iter()
        .map(|ask| (ask.request_id.clone(), ask))
        .collect();
    for (id, detail) in fetched {
        match detail {
            Some(detail) => {
                held.insert(id, detail);
            }
            None => {
                held.remove(&id);
            }
        }
    }

    let mut unresolved = Vec::new();
    let mut pending = Vec::with_capacity(listed.len());
    for id in &listed {
        match held.remove(id) {
            Some(detail) => pending.push(detail),
            None => unresolved.push(id.clone()),
        }
    }
    mirror.pending = pending;

    let mut left: Vec<AskDetail> = departed.into_iter().filter_map(|(_, detail)| detail).collect();
    // Newest decision first, and an undecided departure (expired, abandoned,
    // or a row whose `decided_at` the kernel does not carry) after the dated
    // ones rather than ahead of them.
    left.sort_by(|a, b| b.decided_at.cmp(&a.decided_at));
    left.append(&mut mirror.recent);
    left.truncate(RECENT_CAP);
    mirror.recent = left;

    unresolved
}

// ============================================================================
// Pure: the decision
// ============================================================================

/// What a decision key asks the kernel for. The three options
/// `kj ledger allow|deny` carries, named the way the key line names them
/// (`docs/tui.md`, "Asks").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `a` — `kj ledger allow <id>`.
    AllowOnce,
    /// `A` — `kj ledger allow <id> --remember always`.
    AllowAlways,
    /// `d` — `kj ledger deny <id>`.
    Deny,
}

impl Decision {
    /// The words a notice uses, matching `decided_option` as the kernel
    /// records it: `allow once`, `allow always`, `deny`.
    pub fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow once",
            Self::AllowAlways => "allow always",
            Self::Deny => "deny",
        }
    }

    /// `(allow, remember)` as [`kaijutsu_client::decide_ask_remember`] takes
    /// them.
    fn verb(self) -> (bool, Option<kaijutsu_client::RememberScope>) {
        match self {
            Self::AllowOnce => (true, None),
            Self::AllowAlways => (true, Some(kaijutsu_client::RememberScope::Always)),
            Self::Deny => (false, None),
        }
    }
}

/// A surface asks the kernel to decide one ask. The ask sheet and the ledger
/// ribbon both write this rather than calling RPC themselves, so `kj ledger`
/// keeps exactly one caller in the app.
#[derive(Message, Debug, Clone)]
pub struct AskDecisionRequested {
    pub request_id: String,
    pub decision: Decision,
}

/// How the kernel answered a decision. Every arm is reported to the player:
/// a refused write is never a silent nothing (`docs/tui.md`, "Asks").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    /// The kernel took it. The mirror's next round brings the ask down.
    Accepted,
    /// The kernel refused because the ask was already answered — a race lost
    /// to another surface, which the ledger's `claim`+`decide` transaction
    /// makes normal rather than exceptional (`kaijutsu_client::ledger`).
    AlreadyDecided,
    /// The write could not be made at all.
    Failed(String),
}

/// One finished decision, for a surface to turn into a notice.
#[derive(Message, Debug, Clone)]
pub struct AskDecisionSettled {
    pub request_id: String,
    pub decision: Decision,
    pub outcome: DecisionOutcome,
}

/// Read one `kj ledger allow|deny` exit into an outcome.
///
/// A nonzero exit is a lost race when the kernel says so and a plain failure
/// otherwise; `stderr` decides which, because the exit code alone does not
/// distinguish them.
pub fn classify_decision(exit_code: i32, stderr: &str) -> DecisionOutcome {
    if exit_code == 0 {
        return DecisionOutcome::Accepted;
    }
    let lowered = stderr.to_lowercase();
    if lowered.contains("already decided")
        || lowered.contains("alreadydecided")
        || lowered.contains("not pending")
    {
        return DecisionOutcome::AlreadyDecided;
    }
    let detail = stderr.trim();
    DecisionOutcome::Failed(if detail.is_empty() {
        format!("exit {exit_code}")
    } else {
        detail.lines().next().unwrap_or(detail).to_string()
    })
}

/// The first segment of a request id — `01a04eb6` of
/// `01a04eb6-aaaa-…` — the handle `kj ledger list` keys on and every notice
/// names (`docs/tui.md`, "Asks").
pub fn short_request_id(request_id: &str) -> &str {
    request_id.split('-').next().unwrap_or(request_id)
}

// ============================================================================
// Bevy glue
// ============================================================================

/// One round's result, tagged with the connection it ran on.
struct LedgerRound {
    generation: u64,
    connection_epoch: u64,
    result: Result<RoundData, String>,
}

/// Drain-once channel for finished rounds. A dedicated channel rather than
/// `RpcResultMessage`: a round result is read by exactly one system, and
/// `RoundData` moves into the mirror instead of being cloned out of Bevy's
/// message storage.
#[derive(Resource)]
struct LedgerRoundChannel {
    tx: crossbeam_channel::Sender<LedgerRound>,
    rx: crossbeam_channel::Receiver<LedgerRound>,
}

impl LedgerRoundChannel {
    fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx }
    }
}

/// One finished decision on its way back from the task that made it.
struct DecisionResult {
    request_id: String,
    decision: Decision,
    outcome: DecisionOutcome,
}

/// Drain-once channel for finished decisions, the shape
/// [`LedgerRoundChannel`] uses and for the same reason.
#[derive(Resource)]
struct DecisionChannel {
    tx: crossbeam_channel::Sender<DecisionResult>,
    rx: crossbeam_channel::Receiver<DecisionResult>,
}

impl DecisionChannel {
    fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx }
    }
}

/// The ledger mirror, the decision write path, and the systems that keep
/// them current. Add after `ActorPlugin`: the poll systems read `RpcActor`,
/// and every renderer that reads [`LedgerMirror`] needs the resource to
/// exist.
pub struct LedgerMirrorPlugin;

impl Plugin for LedgerMirrorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LedgerMirror>()
            .insert_resource(LedgerRoundChannel::new())
            .insert_resource(DecisionChannel::new())
            .add_message::<AskDecisionRequested>()
            .add_message::<AskDecisionSettled>()
            .add_systems(
                Update,
                (
                    poll_ledger_events,
                    start_ledger_round,
                    drain_ledger_rounds,
                    start_ask_decisions,
                    drain_ask_decisions,
                )
                    .chain(),
            );
    }
}

/// Carry each requested decision to the kernel.
///
/// The surfaces do not change their own state on a keypress: this writes
/// `kj ledger allow|deny` and the mirror's next round — the kernel bumps the
/// ledger generation on a decision — is what brings the ask down. A request
/// with no live connection is reported straight back as a failure rather
/// than dropped, so a key never does nothing.
fn start_ask_decisions(
    actor: Option<Res<RpcActor>>,
    conn: Res<RpcConnectionState>,
    doc_cache: Res<crate::view::document::DocumentCache>,
    channel: Res<DecisionChannel>,
    mut requests: MessageReader<AskDecisionRequested>,
    mut settled: MessageWriter<AskDecisionSettled>,
) {
    for AskDecisionRequested {
        request_id,
        decision,
    } in requests.read()
    {
        let ready = actor
            .as_ref()
            .filter(|_| conn.connected)
            .zip(doc_cache.active_id());
        let Some((actor, context_id)) = ready else {
            settled.write(AskDecisionSettled {
                request_id: request_id.clone(),
                decision: *decision,
                outcome: DecisionOutcome::Failed("no live connection".to_string()),
            });
            continue;
        };

        let handle = actor.handle.clone();
        let tx = channel.tx.clone();
        let request_id = request_id.clone();
        let decision = *decision;
        let (allow, remember) = decision.verb();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                let outcome = match kaijutsu_client::decide_ask_remember(
                    &handle,
                    context_id,
                    &request_id,
                    allow,
                    remember,
                )
                .await
                {
                    Ok(result) => classify_decision(result.exit_code, &result.stderr),
                    Err(e) => DecisionOutcome::Failed(format!("{e}")),
                };
                let _ = tx.send(DecisionResult {
                    request_id,
                    decision,
                    outcome,
                });
            })
            .detach();
    }
}

/// Publish finished decisions, and mark the mirror dirty on one the kernel
/// took.
///
/// The generation bump normally covers that, but a bump we somehow miss must
/// not leave an answered ask on screen — so this asks for a round outright
/// rather than trusting the stream for the one change we started ourselves.
fn drain_ask_decisions(
    channel: Res<DecisionChannel>,
    mut mirror: ResMut<LedgerMirror>,
    mut settled: MessageWriter<AskDecisionSettled>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let mut any = false;
    for result in channel.rx.try_iter() {
        any = true;
        match &result.outcome {
            DecisionOutcome::Accepted | DecisionOutcome::AlreadyDecided => mirror.dirty = true,
            DecisionOutcome::Failed(detail) => log::warn!(
                "kj ledger {} {} failed: {detail}",
                result.decision.label(),
                short_request_id(&result.request_id)
            ),
        }
        settled.write(AskDecisionSettled {
            request_id: result.request_id,
            decision: result.decision,
            outcome: result.outcome,
        });
    }
    if any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Subscribe to the kernel's ledger change stream, and drain the generations
/// it pushes.
///
/// The subscription is installed on every new actor and marks the mirror
/// dirty: a fresh connection knows nothing about the pending set, so the
/// mirror is rebuilt from the kernel rather than trusted. A lag is the same
/// answer — the generations that were dropped cannot be recovered, so the
/// next round re-lists everything.
fn poll_ledger_events(
    actor: Option<Res<RpcActor>>,
    mut mirror: ResMut<LedgerMirror>,
    mut receiver: Local<Option<broadcast::Receiver<i64>>>,
) {
    let Some(actor) = actor else { return };

    if actor.is_changed() {
        *receiver = Some(actor.handle.subscribe_ledger_events());
        mirror.on_connection_changed();
    }

    let Some(rx) = receiver.as_mut() else { return };
    loop {
        match rx.try_recv() {
            Ok(generation) => mirror.on_generation(generation),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                log::warn!("ledger change stream lagged by {n}; re-reading the ledger");
                mirror.on_stream_gap();
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => {
                *receiver = None;
                break;
            }
        }
    }
}

/// Start one round when the mirror owes one, nothing is in flight, the
/// connection is live, and there is a current context to run `kj` in.
///
/// `dirty` stays set while any of those is missing, so the round happens as
/// soon as they hold.
fn start_ledger_round(
    actor: Option<Res<RpcActor>>,
    conn: Res<RpcConnectionState>,
    doc_cache: Res<crate::view::document::DocumentCache>,
    channel: Res<LedgerRoundChannel>,
    mut mirror: ResMut<LedgerMirror>,
) {
    if !mirror.should_start_round() || !conn.connected {
        return;
    }
    let Some(actor) = actor else { return };
    let Some(context_id) = doc_cache.active_id() else {
        return;
    };

    let held: Vec<String> = mirror.pending.iter().map(|ask| ask.request_id.clone()).collect();
    mirror.on_round_start();

    let handle = actor.handle.clone();
    let generation = actor.generation;
    let connection_epoch = handle.connection_epoch();
    let tx = channel.tx.clone();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            let result = run_round(&handle, context_id, held).await;
            let _ = tx.send(LedgerRound {
                generation,
                connection_epoch,
                result,
            });
        })
        .detach();
}

/// One round against the kernel: list the pending asks, then read the detail
/// of each ask that arrived or left. Every await lives here.
async fn run_round(
    handle: &kaijutsu_client::ActorHandle,
    context_id: ContextId,
    held: Vec<String>,
) -> Result<RoundData, String> {
    let listed = kaijutsu_client::list_pending(handle, context_id)
        .await
        .map_err(|e| format!("{e}"))?;
    let plan = plan_round(&held, &listed);

    let mut fetched = Vec::with_capacity(plan.fetch_new.len());
    for id in plan.fetch_new {
        let detail = kaijutsu_client::show_ask_detail(handle, context_id, &id)
            .await
            .unwrap_or_else(|e| {
                log::warn!("kj ledger show {id} failed: {e}");
                None
            });
        fetched.push((id, detail));
    }

    let mut departed = Vec::with_capacity(plan.departed.len());
    for id in plan.departed {
        let detail = kaijutsu_client::show_ask_detail(handle, context_id, &id)
            .await
            .unwrap_or_else(|e| {
                log::warn!("kj ledger show {id} failed for a decided ask: {e}");
                None
            });
        departed.push((id, detail));
    }

    Ok(RoundData {
        listed,
        fetched,
        departed,
    })
}

/// Fold finished rounds into the mirror, dropping any that belong to a
/// connection the app has already replaced.
fn drain_ledger_rounds(
    actor: Option<Res<RpcActor>>,
    channel: Res<LedgerRoundChannel>,
    mut mirror: ResMut<LedgerMirror>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let mut applied_any = false;
    for round in channel.rx.try_iter() {
        let live = actor
            .as_ref()
            .map(|actor| {
                actor.generation == round.generation
                    && actor.handle.connection_epoch() == round.connection_epoch
            })
            .unwrap_or(false);
        if !live {
            log::debug!(
                "discarding a ledger round from actor generation {} epoch {}",
                round.generation,
                round.connection_epoch
            );
            continue;
        }
        applied_any = true;
        match round.result {
            Ok(data) => {
                let unresolved = apply_round(&mut mirror, data);
                if unresolved.is_empty() {
                    mirror.on_round_done(false);
                    mirror.last_error = None;
                } else {
                    log::warn!(
                        "{} pending ask(s) could not be read; re-reading the ledger",
                        unresolved.len()
                    );
                    mirror.last_error = Some(format!(
                        "{} pending ask(s) could not be read",
                        unresolved.len()
                    ));
                    mirror.on_round_done(true);
                }
            }
            Err(error) => {
                log::warn!("ledger round failed: {error}");
                mirror.last_error = Some(error);
                mirror.on_round_done(true);
            }
        }
        if !mirror.dirty && mirror.last_error.is_some() {
            log::warn!(
                "the ledger mirror is stale after {RETRY_LIMIT} failed round(s); \
                 waiting for the next ledger change"
            );
        }
    }
    if applied_any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(id: &str, context: Option<ContextId>, decided_at: Option<i64>) -> AskDetail {
        AskDetail {
            request_id: id.to_string(),
            context_id: context,
            principal_id: None,
            principal_name: None,
            actor_id: None,
            actor_name: None,
            reviewer_id: None,
            reviewer_name: None,
            status: if decided_at.is_some() { "allowed".into() } else { "pending".into() },
            origin: "shell_gate".into(),
            tool: None,
            hook_id: None,
            instance: None,
            description: format!("ask {id}"),
            authorized_label: None,
            statements: Vec::new(),
            exec_source: None,
            cwd: None,
            env: Vec::new(),
            created_at: Some(1),
            decided_at,
            decided_by: None,
            decided_by_name: None,
            decided_option: None,
            remember_scope: None,
            redeemed_at: None, publication_abandoned: None,
        }
    }

    fn ctx(n: u8) -> ContextId {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        ContextId::from_bytes(bytes)
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn mirror_with(pending: &[&str]) -> LedgerMirror {
        LedgerMirror {
            pending: pending.iter().map(|id| ask(id, None, None)).collect(),
            ..Default::default()
        }
    }

    fn pending_ids(mirror: &LedgerMirror) -> Vec<String> {
        mirror.pending.iter().map(|ask| ask.request_id.clone()).collect()
    }

    fn recent_ids(mirror: &LedgerMirror) -> Vec<String> {
        mirror.recent.iter().map(|ask| ask.request_id.clone()).collect()
    }

    // ── plan_round ──

    #[test]
    fn plan_round_fetches_only_the_ids_the_mirror_lacks() {
        let plan = plan_round(&ids(&["a"]), &ids(&["a", "b", "c"]));
        assert_eq!(plan.fetch_new, ids(&["b", "c"]));
        assert_eq!(plan.keep, ids(&["a"]));
        assert!(plan.departed.is_empty());
    }

    #[test]
    fn plan_round_reports_held_ids_the_ledger_no_longer_lists() {
        let plan = plan_round(&ids(&["a", "b"]), &ids(&["b"]));
        assert_eq!(plan.departed, ids(&["a"]));
        assert_eq!(plan.keep, ids(&["b"]));
        assert!(plan.fetch_new.is_empty());
    }

    #[test]
    fn plan_round_keeps_listed_order_for_the_ids_it_fetches() {
        let plan = plan_round(&[], &ids(&["c", "a", "b"]));
        assert_eq!(plan.fetch_new, ids(&["c", "a", "b"]));
    }

    #[test]
    fn plan_round_on_an_empty_ledger_departs_everything_held() {
        let plan = plan_round(&ids(&["a", "b"]), &[]);
        assert_eq!(plan.departed, ids(&["a", "b"]));
        assert!(plan.fetch_new.is_empty());
        assert!(plan.keep.is_empty());
    }

    // ── apply_round ──

    #[test]
    fn apply_round_orders_pending_the_way_the_ledger_listed_it() {
        let mut mirror = mirror_with(&["a"]);
        let unresolved = apply_round(
            &mut mirror,
            RoundData {
                listed: ids(&["b", "a", "c"]),
                fetched: vec![
                    ("b".into(), Some(ask("b", None, None))),
                    ("c".into(), Some(ask("c", None, None))),
                ],
                departed: Vec::new(),
            },
        );
        assert!(unresolved.is_empty());
        assert_eq!(pending_ids(&mirror), ids(&["b", "a", "c"]));
    }

    #[test]
    fn apply_round_reuses_the_detail_it_already_held() {
        let mut mirror = mirror_with(&["a"]);
        let mut fresh = ask("b", None, None);
        fresh.description = "fresh".into();
        apply_round(
            &mut mirror,
            RoundData {
                listed: ids(&["b", "a"]),
                fetched: vec![("b".into(), Some(fresh))],
                departed: Vec::new(),
            },
        );
        assert_eq!(pending_ids(&mirror), ids(&["b", "a"]));
        assert_eq!(mirror.pending[0].description, "fresh");
        assert_eq!(mirror.pending[1].description, "ask a", "a held ask is not re-read");
    }

    #[test]
    fn apply_round_reports_an_ask_it_could_not_read_and_leaves_it_out() {
        let mut mirror = LedgerMirror::default();
        let unresolved = apply_round(
            &mut mirror,
            RoundData {
                listed: ids(&["a", "b"]),
                fetched: vec![("a".into(), None), ("b".into(), Some(ask("b", None, None)))],
                departed: Vec::new(),
            },
        );
        assert_eq!(unresolved, ids(&["a"]));
        assert_eq!(pending_ids(&mirror), ids(&["b"]));
    }

    #[test]
    fn apply_round_pushes_departed_asks_onto_recent_newest_first() {
        let mut mirror = mirror_with(&["old", "new"]);
        apply_round(
            &mut mirror,
            RoundData {
                listed: Vec::new(),
                fetched: Vec::new(),
                departed: vec![
                    ("old".into(), Some(ask("old", None, Some(10)))),
                    ("new".into(), Some(ask("new", None, Some(20)))),
                ],
            },
        );
        assert_eq!(recent_ids(&mirror), ids(&["new", "old"]));
        assert!(mirror.pending.is_empty());
    }

    #[test]
    fn apply_round_caps_recent_and_keeps_the_newest() {
        let mut mirror = mirror_with(&["d"]);
        mirror.recent = ["c", "b", "a"].iter().map(|id| ask(id, None, Some(1))).collect();
        apply_round(
            &mut mirror,
            RoundData {
                listed: Vec::new(),
                fetched: Vec::new(),
                departed: vec![("d".into(), Some(ask("d", None, Some(9))))],
            },
        );
        assert_eq!(recent_ids(&mirror), ids(&["d", "c", "b"]));
        assert_eq!(mirror.recent.len(), RECENT_CAP);
    }

    #[test]
    fn apply_round_drops_a_departed_ask_it_could_not_read() {
        let mut mirror = mirror_with(&["a"]);
        let unresolved = apply_round(
            &mut mirror,
            RoundData {
                listed: Vec::new(),
                fetched: Vec::new(),
                departed: vec![("a".into(), None)],
            },
        );
        assert!(unresolved.is_empty(), "a decided ask is not re-read on a later round");
        assert!(mirror.pending.is_empty());
        assert!(mirror.recent.is_empty());
    }

    // ── the dirty / in-flight latch ──

    #[test]
    fn a_clean_mirror_starts_no_round() {
        assert!(!LedgerMirror::default().should_start_round());
    }

    #[test]
    fn a_generation_bump_earns_one_round() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(7);
        assert_eq!(mirror.generation, Some(7));
        assert!(mirror.should_start_round());
        mirror.on_round_start();
        assert!(!mirror.should_start_round(), "one bump, one round");
        mirror.on_round_done(false);
        assert!(!mirror.should_start_round());
    }

    #[test]
    fn bumps_during_a_round_schedule_exactly_one_more() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(1);
        mirror.on_round_start();
        mirror.on_generation(2);
        mirror.on_generation(3);
        assert!(!mirror.should_start_round(), "rounds are single-flight");
        mirror.on_round_done(false);
        assert!(mirror.should_start_round());
        mirror.on_round_start();
        mirror.on_round_done(false);
        assert!(!mirror.should_start_round(), "three bumps do not earn three rounds");
    }

    #[test]
    fn an_older_generation_still_earns_a_round_without_moving_the_mirror_back() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(9);
        mirror.on_round_start();
        mirror.on_round_done(false);
        mirror.on_generation(4);
        assert_eq!(mirror.generation, Some(9));
        assert!(mirror.should_start_round());
    }

    #[test]
    fn a_round_that_left_work_undone_earns_another() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(1);
        mirror.on_round_start();
        mirror.on_round_done(true);
        assert!(mirror.should_start_round());
    }

    #[test]
    fn rounds_that_keep_failing_stop_retrying_until_the_next_trigger() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(1);
        for _ in 0..RETRY_LIMIT {
            assert!(mirror.should_start_round(), "a retry is owed inside the budget");
            mirror.on_round_start();
            mirror.on_round_done(true);
        }
        assert!(
            !mirror.should_start_round(),
            "a failing ledger must not be re-read on every frame"
        );

        mirror.on_generation(2);
        assert!(mirror.should_start_round(), "a fresh bump earns a fresh budget");
    }

    #[test]
    fn a_round_that_succeeds_restores_the_retry_budget() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(1);
        mirror.on_round_start();
        mirror.on_round_done(true);
        mirror.on_round_start();
        mirror.on_round_done(false);
        mirror.on_generation(2);
        for _ in 0..RETRY_LIMIT {
            assert!(mirror.should_start_round());
            mirror.on_round_start();
            mirror.on_round_done(true);
        }
        assert!(!mirror.should_start_round());
    }

    #[test]
    fn a_replaced_connection_owes_a_round_and_disowns_the_running_one() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(5);
        mirror.on_round_start();
        mirror.on_connection_changed();
        assert_eq!(mirror.generation, None, "the new connection's generations are unknown");
        assert!(mirror.should_start_round());
    }

    #[test]
    fn a_stream_gap_owes_a_round_with_a_fresh_budget() {
        let mut mirror = LedgerMirror::default();
        mirror.on_generation(1);
        for _ in 0..RETRY_LIMIT {
            mirror.on_round_start();
            mirror.on_round_done(true);
        }
        assert!(!mirror.should_start_round());
        mirror.on_stream_gap();
        assert!(mirror.should_start_round());
        assert_eq!(mirror.generation, Some(1), "a gap says nothing about the generation");
    }

    // ── reads ──

    #[test]
    fn pending_for_context_selects_that_context_in_ledger_order() {
        let mut mirror = LedgerMirror::default();
        mirror.pending = vec![
            ask("a", Some(ctx(1)), None),
            ask("b", Some(ctx(2)), None),
            ask("c", Some(ctx(1)), None),
        ];
        let selected: Vec<String> = mirror
            .pending_for_context(ctx(1))
            .map(|ask| ask.request_id.clone())
            .collect();
        assert_eq!(selected, ids(&["a", "c"]));
        assert_eq!(mirror.pending_count(), 3);
        assert!(mirror.context_is_asking(ctx(2)));
        assert!(!mirror.context_is_asking(ctx(3)));
    }

    #[test]
    fn get_finds_an_ask_whether_it_is_pending_or_decided() {
        let mut mirror = LedgerMirror::default();
        mirror.pending = vec![ask("a", None, None)];
        mirror.recent = vec![ask("b", None, Some(5))];
        assert_eq!(mirror.get("a").map(|a| a.request_id.as_str()), Some("a"));
        assert_eq!(mirror.get("b").map(|a| a.request_id.as_str()), Some("b"));
        assert!(mirror.get("c").is_none(), "never held is not the same as not in the ledger");
    }

    // ── the decision ──

    #[test]
    fn a_clean_exit_is_the_kernel_taking_the_decision() {
        assert_eq!(classify_decision(0, ""), DecisionOutcome::Accepted);
    }

    /// A lost race is normal, not a failure — two players share one ledger
    /// and the kernel makes exactly one of them win.
    #[test]
    fn a_refusal_that_names_an_answered_ask_is_a_lost_race() {
        for stderr in [
            "error: ask already decided",
            "AlreadyDecided",
            "request 01a04eb6 is not pending",
        ] {
            assert_eq!(
                classify_decision(1, stderr),
                DecisionOutcome::AlreadyDecided,
                "{stderr}"
            );
        }
    }

    /// Every other refusal keeps its own words, and a silent one still says
    /// something — a key must never do nothing.
    #[test]
    fn any_other_refusal_reports_what_the_kernel_said() {
        assert_eq!(
            classify_decision(2, "permission denied\nbacktrace..."),
            DecisionOutcome::Failed("permission denied".to_string()),
            "the first line is the message, not the backtrace"
        );
        assert_eq!(
            classify_decision(2, "   "),
            DecisionOutcome::Failed("exit 2".to_string())
        );
    }

    #[test]
    fn a_decision_names_itself_the_way_the_ledger_records_it() {
        assert_eq!(Decision::AllowOnce.label(), "allow once");
        assert_eq!(Decision::AllowAlways.label(), "allow always");
        assert_eq!(Decision::Deny.label(), "deny");
    }

    #[test]
    fn allow_always_is_the_only_option_that_remembers() {
        assert_eq!(Decision::AllowOnce.verb(), (true, None));
        assert_eq!(Decision::Deny.verb(), (false, None));
        assert_eq!(
            Decision::AllowAlways.verb(),
            (true, Some(kaijutsu_client::RememberScope::Always))
        );
    }

    /// The handle every notice uses is the ask's first id segment, the one
    /// `kj ledger list` keys on.
    #[test]
    fn the_short_id_is_the_first_segment() {
        assert_eq!(
            short_request_id("01a04eb6-aaaa-bbbb-cccc-000000000001"),
            "01a04eb6"
        );
        assert_eq!(short_request_id("bare"), "bare");
    }

    #[test]
    fn an_ask_with_no_context_is_in_no_context_lamp() {
        let mut mirror = LedgerMirror::default();
        mirror.pending = vec![ask("a", None, None)];
        assert_eq!(mirror.pending_count(), 1);
        assert!(!mirror.context_is_asking(ctx(1)));
    }
}
