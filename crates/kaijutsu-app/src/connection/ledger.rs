//! The approval ledger, mirrored app-side: one resource holding the kernel's
//! open asks plus the last few that closed, for every renderer that wants to
//! show them (`docs/approval-identity.md`).
//!
//! One mirror, many readers. [`LedgerMirror`] is the only place in the app
//! that reads the ledger or answers an ask, so a lamp, a dock badge, the ask
//! sheet and the ledger ribbon all read the same open set and cannot disagree
//! about it. The ledger is kernel-wide: nothing here takes a context.
//!
//! **The open set comes from the actor's watch.** `ActorHandle::ledger` holds
//! the open asks, relisted on every (re)connect and kept current by the
//! kernel's push. [`follow_ledger`] diffs each new value against the mirror:
//! an ask that appears is offered, and an ask that leaves was answered,
//! cancelled, or expired, from any surface. There is no timer and no polling.
//!
//! **Full records are read once per ask.** The watch carries an
//! [`AskSummary`]; the sheet also shows the tool, cwd, and environment, so an
//! ask that arrives has its [`AskDetail`] read with `get_ask`, and an ask
//! that leaves is read once more so `recent` carries its decision. A read
//! that fails is retried on the next ledger change, and until it lands the
//! summary stands in for the record.
//!
//! **An answer is a ledger row and nothing else.** A decision key sends
//! `decide_ask`; it authors no block in any transcript. The push that closes
//! the ask is what takes the sheet down.
//!
//! **A result from a replaced connection is discarded.** Each read carries
//! the actor generation and connection epoch it started under, and the drain
//! drops anything else (`docs/issues.md`, "App bootstrap results need
//! consistent connection scoping").
//!
//! **Design split**: the diff, the read bookkeeping, and the answer
//! classification are pure and unit-tested with no Bevy app; only the
//! systems at the bottom touch `bevy` or the RPC handle.

use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use kaijutsu_client::LedgerState;
use kaijutsu_types::{
    AskAnswerFailureKind, AskDetail, AskSummary, AskVerdict, ContextId, Remember, RememberScope,
};
use tokio::sync::watch;

use super::actor_plugin::{RpcActor, RpcConnectionState};

/// How many closed asks the mirror keeps behind the open set. Enough for a
/// renderer to show what just happened; the ledger itself is the history
/// (`kj ledger list --history`).
pub const RECENT_CAP: usize = 3;

// ============================================================================
// The mirror
// ============================================================================

/// The kernel's approval ledger as this app last read it.
///
/// `open` is oldest first, so every renderer shows asks in the order they
/// were raised rather than inventing one. `recent` holds asks that left the
/// open set, most recent decision first, each re-read so its decision is
/// known.
#[derive(Resource, Default, Debug)]
pub struct LedgerMirror {
    /// The ledger generation `open` reflects.
    pub generation: i64,
    /// Whether `open` was read on the live connection. False until the first
    /// listing lands and again while a reconnect relists; the last-known
    /// open set stays meanwhile, so the lamps do not blink.
    pub synced: bool,
    /// Every open ask, oldest first.
    open: Vec<AskSummary>,
    /// Full records read for open asks, by request id.
    records: HashMap<String, AskDetail>,
    /// Open ids whose record is being read now.
    reading: HashSet<String>,
    /// Why an open ask's record could not be read, by request id.
    unread: HashMap<String, String>,
    /// Asks that left the open set, most recent decision first, capped at
    /// [`RECENT_CAP`].
    recent: Vec<AskDetail>,
}

/// What one watch value changed: the request ids that arrived in the open
/// set and the ones that left it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LedgerDiff {
    /// Ids newly open, oldest first.
    pub arrived: Vec<String>,
    /// Ids no longer open, in the order the mirror held them.
    pub departed: Vec<String>,
}

impl LedgerDiff {
    pub fn is_empty(&self) -> bool {
        self.arrived.is_empty() && self.departed.is_empty()
    }
}

impl LedgerMirror {
    /// Every open ask, oldest first.
    pub fn open(&self) -> &[AskSummary] {
        &self.open
    }

    /// How many asks are waiting for a decision, across every context.
    pub fn pending_count(&self) -> usize {
        self.open.len()
    }

    /// The open asks raised in one context, oldest first.
    pub fn pending_for_context(&self, context_id: ContextId) -> impl Iterator<Item = &AskSummary> {
        self.open
            .iter()
            .filter(move |ask| ask.context_id == Some(context_id))
    }

    /// Whether any ask in `context_id` is waiting — the switchboard lamp's
    /// one question (`view::room::switchboard`).
    pub fn context_is_asking(&self, context_id: ContextId) -> bool {
        self.pending_for_context(context_id).next().is_some()
    }

    /// One open ask's summary. `None` once it has left the open set.
    pub fn summary(&self, request_id: &str) -> Option<&AskSummary> {
        self.open.iter().find(|ask| ask.request_id == request_id)
    }

    /// The best record this mirror holds for an ask: an open ask's full
    /// record with its live summary, the summary alone while the record is
    /// unread, or a closed ask from `recent`. `None` means this mirror does
    /// not hold it, not that the ledger lacks it.
    ///
    /// The live summary replaces the one read with the record, because a
    /// reassignment changes the reviewer of an ask that stays open.
    pub fn record(&self, request_id: &str) -> Option<AskDetail> {
        if let Some(summary) = self.summary(request_id) {
            let mut record = self
                .records
                .get(request_id)
                .cloned()
                .unwrap_or_else(|| summary_record(summary));
            record.summary = summary.clone();
            return Some(record);
        }
        self.recent
            .iter()
            .find(|ask| ask.summary.request_id == request_id)
            .cloned()
    }

    /// Every open ask's [`Self::record`], oldest first.
    pub fn open_records(&self) -> Vec<AskDetail> {
        self.open
            .iter()
            .filter_map(|ask| self.record(&ask.request_id))
            .collect()
    }

    /// Asks that left the open set, most recent decision first.
    pub fn recent(&self) -> &[AskDetail] {
        &self.recent
    }

    /// Why an open ask shows its summary in place of its full record:
    /// `reading` while the read runs, the failure once one has failed, and
    /// `None` when the record is held.
    pub fn record_gap(&self, request_id: &str) -> Option<String> {
        if self.records.contains_key(request_id) || self.summary(request_id).is_none() {
            return None;
        }
        Some(match self.unread.get(request_id) {
            Some(error) => format!("full record not read: {error}"),
            None => "reading the full record".to_string(),
        })
    }

    /// Fold one watch value in and say what changed.
    ///
    /// An unsynced value changes nothing but `synced`: until the relisting
    /// lands, the last-known open set is the better answer. A record held for
    /// an ask that left is dropped; the departure read replaces it.
    pub fn apply_state(&mut self, state: &LedgerState) -> LedgerDiff {
        if !state.synced {
            self.synced = false;
            return LedgerDiff::default();
        }
        let mut next: Vec<AskSummary> = state.open.values().cloned().collect();
        next.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.request_id.cmp(&b.request_id))
        });

        let held: HashSet<&str> = self.open.iter().map(|a| a.request_id.as_str()).collect();
        let diff = LedgerDiff {
            arrived: next
                .iter()
                .filter(|a| !held.contains(a.request_id.as_str()))
                .map(|a| a.request_id.clone())
                .collect(),
            departed: self
                .open
                .iter()
                .filter(|a| !state.open.contains_key(&a.request_id))
                .map(|a| a.request_id.clone())
                .collect(),
        };
        for id in &diff.departed {
            self.records.remove(id);
            self.unread.remove(id);
        }
        self.open = next;
        self.generation = state.generation;
        self.synced = true;
        diff
    }

    /// The open ids whose full record should be read now: not held and not
    /// already being read. Each is marked as being read. A read that failed
    /// is asked for again here, so it retries on the next ledger change.
    pub fn take_reads(&mut self) -> Vec<String> {
        let wanted: Vec<String> = self
            .open
            .iter()
            .map(|ask| ask.request_id.clone())
            .filter(|id| !self.records.contains_key(id) && !self.reading.contains(id))
            .collect();
        self.reading.extend(wanted.iter().cloned());
        wanted
    }

    /// An open ask's record read landed. A read for an ask that has since
    /// left the open set is dropped: the departure read answers for it.
    pub fn on_record(&mut self, request_id: &str, result: Result<Option<AskDetail>, String>) {
        self.reading.remove(request_id);
        if self.summary(request_id).is_none() {
            return;
        }
        match result {
            Ok(Some(record)) => {
                self.unread.remove(request_id);
                self.records.insert(request_id.to_string(), record);
            }
            Ok(None) => {
                self.unread
                    .insert(request_id.to_string(), "the ledger has no such ask".to_string());
            }
            Err(error) => {
                self.unread.insert(request_id.to_string(), error);
            }
        }
    }

    /// A closed ask's record landed: it goes to the front of `recent`, newest
    /// decision first, capped at [`RECENT_CAP`]. An ask closed without a
    /// decision time sorts after the dated ones rather than ahead of them.
    pub fn on_departed(&mut self, record: AskDetail) {
        self.recent
            .retain(|ask| ask.summary.request_id != record.summary.request_id);
        self.recent.push(record);
        self.recent
            .sort_by(|a, b| b.summary.decided_at_ms.cmp(&a.summary.decided_at_ms));
        self.recent.truncate(RECENT_CAP);
    }

    /// The connection was replaced: reads still running belong to it and
    /// are discarded by the drain, so none count as running now. The open
    /// set stays until the new connection's listing replaces it.
    pub fn on_connection_changed(&mut self) {
        self.reading.clear();
        self.synced = false;
    }
}

/// An ask's summary as a record with none of the record's own fields read.
pub fn summary_record(summary: &AskSummary) -> AskDetail {
    AskDetail {
        summary: summary.clone(),
        instance: None,
        tool: None,
        hook_id: None,
        label: None,
        tool_call_block_id: None,
        exec_source: None,
        cwd: None,
        env: Vec::new(),
        decision: None,
        redeemed_at_ms: None,
        publication_abandoned: None,
        reassignments: Vec::new(),
    }
}

// ============================================================================
// Pure: the decision
// ============================================================================

/// What a decision key asks the kernel for, named the way the key line names
/// them (`docs/tui.md`, "Asks").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `a` — allow, once.
    AllowOnce,
    /// `A` — allow, and remember the exact statement everywhere.
    AllowAlways,
    /// `d` — deny.
    Deny,
}

impl Decision {
    /// The words a notice uses, matching the decision option as the kernel
    /// records it: `allow once`, `allow always`, `deny`.
    pub fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow once",
            Self::AllowAlways => "allow always",
            Self::Deny => "deny",
        }
    }

    /// The verdict and standing rule `decide_ask` takes.
    pub fn answer(self) -> (AskVerdict, Option<Remember>) {
        match self {
            Self::AllowOnce => (AskVerdict::Allow, None),
            Self::AllowAlways => (
                AskVerdict::Allow,
                Some(Remember {
                    scope: RememberScope::Always,
                    family: false,
                }),
            ),
            Self::Deny => (AskVerdict::Deny, None),
        }
    }
}

/// A surface asks the kernel to decide one ask. The ask sheet and the ledger
/// ribbon both write this rather than calling RPC themselves, so the app
/// answers asks in exactly one place.
#[derive(Message, Debug, Clone)]
pub struct AskDecisionRequested {
    pub request_id: String,
    pub decision: Decision,
}

/// How the kernel answered a decision. Every arm is reported to the player:
/// a refused write is never a silent nothing (`docs/tui.md`, "Asks").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    /// The ledger took it; the push that closes the ask follows. `unlearned`
    /// says why a requested standing rule was not written — the answer
    /// stands either way.
    Accepted { unlearned: Option<String> },
    /// Another answer, an expiry, or a cancellation got there first — a race
    /// lost to another surface, which the ledger's claim makes normal rather
    /// than exceptional.
    AlreadyDecided,
    /// The ledger declined, or the call could not be made at all.
    Failed(String),
}

/// The asks this app has sent a decision for and not yet heard back about.
///
/// A second decision key on such an ask is ignored: sending it would make
/// the first answer's own sender lose a race against itself. The reply
/// clears the entry, and the push that closes the ask takes it off screen.
#[derive(Resource, Default, Debug)]
pub struct DecisionsInFlight(HashSet<String>);

impl DecisionsInFlight {
    /// Claim `request_id` for a decision. False when one is already in
    /// flight for it.
    pub fn begin(&mut self, request_id: &str) -> bool {
        self.0.insert(request_id.to_string())
    }

    /// The reply for `request_id` landed.
    pub fn end(&mut self, request_id: &str) {
        self.0.remove(request_id);
    }

    pub fn contains(&self, request_id: &str) -> bool {
        self.0.contains(request_id)
    }
}

/// One finished decision, for a surface to turn into a notice.
#[derive(Message, Debug, Clone)]
pub struct AskDecisionSettled {
    pub request_id: String,
    pub decision: Decision,
    pub outcome: DecisionOutcome,
}

/// Read one `decide_ask` reply into an outcome. The outer `Err` is a call
/// that did not reach the ledger.
pub fn classify_answer(reply: Result<kaijutsu_client::AskAnswer, String>) -> DecisionOutcome {
    match reply {
        Ok(Ok(answered)) => DecisionOutcome::Accepted {
            unlearned: answered
                .remembered
                .filter(|result| !result.learned)
                .map(|result| result.note),
        },
        Ok(Err(failure)) if failure.kind == AskAnswerFailureKind::AlreadyAnswered => {
            DecisionOutcome::AlreadyDecided
        }
        Ok(Err(failure)) => DecisionOutcome::Failed(failure.message),
        Err(error) => DecisionOutcome::Failed(error),
    }
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

/// An ask left the open set while it was in the mirror, with its record as
/// the departure read found it. `None` is a read that failed.
#[derive(Message, Debug, Clone)]
pub struct AskLeft {
    pub request_id: String,
    pub record: Option<AskDetail>,
}

/// What one record read was for.
enum ReadKind {
    /// An ask that arrived in the open set.
    Open,
    /// An ask that left it.
    Departed,
}

/// One finished record read, tagged with the connection it ran on.
struct LedgerRead {
    generation: u64,
    connection_epoch: u64,
    kind: ReadKind,
    request_id: String,
    result: Result<Option<AskDetail>, String>,
}

/// Drain-once channel for finished reads. A dedicated channel rather than
/// `RpcResultMessage`: a read is consumed by exactly one system, and the
/// record moves into the mirror instead of being cloned out of Bevy's
/// message storage.
#[derive(Resource)]
struct LedgerReadChannel {
    tx: crossbeam_channel::Sender<LedgerRead>,
    rx: crossbeam_channel::Receiver<LedgerRead>,
}

impl LedgerReadChannel {
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
/// [`LedgerReadChannel`] uses and for the same reason.
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
/// them current. Add after `ActorPlugin`: these systems read `RpcActor`, and
/// every renderer that reads [`LedgerMirror`] needs the resource to exist.
pub struct LedgerMirrorPlugin;

impl Plugin for LedgerMirrorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LedgerMirror>()
            .insert_resource(LedgerReadChannel::new())
            .insert_resource(DecisionChannel::new())
            .init_resource::<DecisionsInFlight>()
            .add_message::<AskDecisionRequested>()
            .add_message::<AskDecisionSettled>()
            .add_message::<AskLeft>()
            .add_systems(
                Update,
                (
                    follow_ledger,
                    drain_ledger_reads,
                    start_ask_decisions,
                    drain_ask_decisions,
                )
                    .chain()
                    // Before the surfaces derive their state, so an ask that
                    // closed this frame takes its sheet down this frame.
                    .before(crate::input::InputPhase::SyncContext),
            );
    }
}

/// Carry each requested decision to the kernel.
///
/// The surfaces do not change their own state on a keypress: this sends
/// `decide_ask`, and the ledger push that closes the ask is what takes it
/// off screen. A request with no live connection is reported straight back
/// as a failure rather than dropped, so a key never does nothing. A request
/// for an ask whose decision is still in flight is ignored
/// ([`DecisionsInFlight`]).
fn start_ask_decisions(
    actor: Option<Res<RpcActor>>,
    conn: Res<RpcConnectionState>,
    channel: Res<DecisionChannel>,
    mut in_flight: ResMut<DecisionsInFlight>,
    mut requests: MessageReader<AskDecisionRequested>,
    mut settled: MessageWriter<AskDecisionSettled>,
) {
    for AskDecisionRequested {
        request_id,
        decision,
    } in requests.read()
    {
        if in_flight.contains(request_id) {
            log::debug!(
                "ignoring {} on ask {}: a decision is already in flight",
                decision.label(),
                short_request_id(request_id)
            );
            continue;
        }
        let Some(actor) = actor.as_ref().filter(|_| conn.connected) else {
            settled.write(AskDecisionSettled {
                request_id: request_id.clone(),
                decision: *decision,
                outcome: DecisionOutcome::Failed("no live connection".to_string()),
            });
            continue;
        };

        in_flight.begin(request_id);
        let handle = actor.handle.clone();
        let tx = channel.tx.clone();
        let request_id = request_id.clone();
        let decision = *decision;
        let (verdict, remember) = decision.answer();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                let reply = handle
                    .decide_ask(request_id.clone(), verdict, remember)
                    .await
                    .map_err(|e| format!("{e}"));
                let _ = tx.send(DecisionResult {
                    request_id,
                    decision,
                    outcome: classify_answer(reply),
                });
            })
            .detach();
    }
}

/// Publish finished decisions, and free each ask for another decision.
fn drain_ask_decisions(
    channel: Res<DecisionChannel>,
    mut in_flight: ResMut<DecisionsInFlight>,
    mut settled: MessageWriter<AskDecisionSettled>,
    event_loop_proxy: Option<Res<EventLoopProxyWrapper>>,
) {
    let mut any = false;
    for result in channel.rx.try_iter() {
        any = true;
        in_flight.end(&result.request_id);
        if let DecisionOutcome::Failed(detail) = &result.outcome {
            log::warn!(
                "{} on ask {} failed: {detail}",
                result.decision.label(),
                short_request_id(&result.request_id)
            );
        }
        settled.write(AskDecisionSettled {
            request_id: result.request_id,
            decision: result.decision,
            outcome: result.outcome,
        });
    }
    if any && let Some(proxy) = event_loop_proxy {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Follow the actor's ledger watch: fold each new value into the mirror and
/// start the record reads it calls for.
///
/// A new actor brings a new watch, and the mirror disowns the reads the old
/// one started. The mirror is written only on a frame where something
/// changed, because every renderer gates on its change tick.
fn follow_ledger(
    actor: Option<Res<RpcActor>>,
    channel: Res<LedgerReadChannel>,
    mut mirror: ResMut<LedgerMirror>,
    mut receiver: Local<Option<watch::Receiver<LedgerState>>>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let Some(actor) = actor else { return };

    if actor.is_changed() {
        let mut rx = actor.handle.ledger();
        // The value already in the watch has not been folded in yet.
        rx.mark_changed();
        *receiver = Some(rx);
        mirror.on_connection_changed();
    }

    let Some(rx) = receiver.as_mut() else { return };
    match rx.has_changed() {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            log::warn!("the ledger watch closed; the mirror keeps its last open set");
            *receiver = None;
            return;
        }
    }
    let state = rx.borrow_and_update().clone();
    let diff = mirror.apply_state(&state);
    let reads = mirror.take_reads();

    let generation = actor.generation;
    let connection_epoch = actor.handle.connection_epoch();
    let spawn_read = |kind: ReadKind, request_id: String| {
        let handle = actor.handle.clone();
        let tx = channel.tx.clone();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                let result = handle
                    .get_ask(request_id.clone())
                    .await
                    .map_err(|e| format!("{e}"));
                let _ = tx.send(LedgerRead {
                    generation,
                    connection_epoch,
                    kind,
                    request_id,
                    result,
                });
            })
            .detach();
    };
    for id in diff.departed.iter().cloned() {
        spawn_read(ReadKind::Departed, id);
    }
    for id in reads {
        spawn_read(ReadKind::Open, id);
    }
    if !diff.is_empty() {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Fold finished record reads into the mirror, dropping any that belong to
/// a connection the app has already replaced.
///
/// A dropped departure read still reports the departure with no record:
/// the new connection will not read that ask again, and a surface waiting
/// on it must not wait forever.
fn drain_ledger_reads(
    actor: Option<Res<RpcActor>>,
    channel: Res<LedgerReadChannel>,
    mut mirror: ResMut<LedgerMirror>,
    mut left: MessageWriter<AskLeft>,
    event_loop_proxy: Option<Res<EventLoopProxyWrapper>>,
) {
    let mut applied_any = false;
    for read in channel.rx.try_iter() {
        let live = actor
            .as_ref()
            .map(|actor| {
                actor.generation == read.generation
                    && actor.handle.connection_epoch() == read.connection_epoch
            })
            .unwrap_or(false);
        if !live {
            log::debug!(
                "discarding a ledger read from actor generation {} epoch {}",
                read.generation,
                read.connection_epoch
            );
            if let ReadKind::Departed = read.kind {
                left.write(AskLeft {
                    request_id: read.request_id,
                    record: None,
                });
            }
            continue;
        }
        applied_any = true;
        let short = short_request_id(&read.request_id).to_string();
        match read.kind {
            ReadKind::Open => {
                if let Err(error) = &read.result {
                    log::warn!("reading ask {short} failed: {error}");
                }
                mirror.on_record(&read.request_id, read.result);
            }
            ReadKind::Departed => {
                let record = match read.result {
                    Ok(record) => record,
                    Err(error) => {
                        log::warn!("reading closed ask {short} failed: {error}");
                        None
                    }
                };
                if let Some(record) = &record {
                    mirror.on_departed(record.clone());
                }
                left.write(AskLeft {
                    request_id: read.request_id,
                    record,
                });
            }
        }
    }
    if applied_any && let Some(proxy) = event_loop_proxy {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{
        AskAnswerFailure, AskAnswered, AskOrigin, AskStatus, PrincipalId, PrincipalRef,
        RememberResult,
    };

    fn ask(id: &str, context: Option<ContextId>, created_at_ms: i64) -> AskSummary {
        AskSummary {
            request_id: id.to_string(),
            status: AskStatus::Pending,
            origin: AskOrigin::ShellGate,
            context_id: context,
            description: format!("ask {id}"),
            statements: Vec::new(),
            requester: None,
            performer: None,
            reviewer: None,
            created_at_ms,
            decided_at_ms: None,
        }
    }

    fn closed(id: &str, decided_at_ms: Option<i64>) -> AskDetail {
        let mut summary = ask(id, None, 0);
        summary.status = AskStatus::Allowed;
        summary.decided_at_ms = decided_at_ms;
        summary_record(&summary)
    }

    fn state(asks: &[AskSummary], generation: i64) -> LedgerState {
        LedgerState::from_listing(generation, asks.to_vec())
    }

    fn ctx(n: u8) -> ContextId {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        ContextId::from_bytes(bytes)
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn open_ids(mirror: &LedgerMirror) -> Vec<String> {
        mirror.open().iter().map(|ask| ask.request_id.clone()).collect()
    }

    fn recent_ids(mirror: &LedgerMirror) -> Vec<String> {
        mirror
            .recent()
            .iter()
            .map(|ask| ask.summary.request_id.clone())
            .collect()
    }

    // ── the diff ──

    #[test]
    fn a_first_listing_arrives_whole_and_oldest_first() {
        let mut mirror = LedgerMirror::default();
        let diff = mirror.apply_state(&state(
            &[ask("c", None, 30), ask("a", None, 10), ask("b", None, 20)],
            4,
        ));
        assert_eq!(diff.arrived, ids(&["a", "b", "c"]));
        assert!(diff.departed.is_empty());
        assert_eq!(open_ids(&mirror), ids(&["a", "b", "c"]));
        assert_eq!(mirror.generation, 4);
        assert!(mirror.synced);
    }

    #[test]
    fn a_push_names_only_what_changed() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1), ask("b", None, 2)], 1));
        let diff = mirror.apply_state(&state(&[ask("b", None, 2), ask("c", None, 3)], 2));
        assert_eq!(diff.arrived, ids(&["c"]));
        assert_eq!(diff.departed, ids(&["a"]));
        assert_eq!(open_ids(&mirror), ids(&["b", "c"]));
    }

    #[test]
    fn the_same_open_set_again_is_not_news() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        assert!(mirror.apply_state(&state(&[ask("a", None, 1)], 2)).is_empty());
    }

    /// A reconnect relists; until it lands, the last-known open set is the
    /// better answer, and nothing is reported as having left.
    #[test]
    fn an_unsynced_state_keeps_the_last_known_open_set() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        let diff = mirror.apply_state(&LedgerState::default());
        assert!(diff.is_empty());
        assert!(!mirror.synced);
        assert_eq!(open_ids(&mirror), ids(&["a"]));
    }

    // ── record reads ──

    #[test]
    fn an_arrived_ask_is_read_once() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        assert_eq!(mirror.take_reads(), ids(&["a"]));
        assert!(mirror.take_reads().is_empty(), "a read in flight is not started again");
        mirror.on_record("a", Ok(Some(summary_record(&ask("a", None, 1)))));
        assert!(mirror.take_reads().is_empty(), "a held record is not read again");
        assert_eq!(mirror.record_gap("a"), None);
    }

    /// A failed read is retried on the next ledger change rather than
    /// hiding a waiting ask's record for good, and says why meanwhile.
    #[test]
    fn a_failed_read_says_why_and_is_asked_for_again() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        assert_eq!(mirror.record_gap("a").as_deref(), Some("reading the full record"));
        mirror.take_reads();
        mirror.on_record("a", Err("timed out".into()));
        assert_eq!(
            mirror.record_gap("a").as_deref(),
            Some("full record not read: timed out")
        );
        assert_eq!(mirror.take_reads(), ids(&["a"]));
    }

    /// Until the record lands, the summary stands in for it — the ask is
    /// still offered and still counted.
    #[test]
    fn an_unread_ask_is_still_shown_from_its_summary() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", Some(ctx(1)), 1)], 1));
        let record = mirror.record("a").expect("an open ask has a record");
        assert_eq!(record.summary.description, "ask a");
        assert_eq!(record.tool, None);
        assert!(mirror.context_is_asking(ctx(1)));
    }

    /// A reassignment changes the reviewer while the ask stays open; the
    /// record shows the live summary, not the one read with it.
    #[test]
    fn a_record_carries_the_live_summary() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        mirror.take_reads();
        let mut read = summary_record(&ask("a", None, 1));
        read.tool = Some("shell_write".into());
        mirror.on_record("a", Ok(Some(read)));

        let mut moved = ask("a", None, 1);
        moved.reviewer = Some(PrincipalRef {
            id: PrincipalId::from_bytes([7; 16]),
            name: "banto".into(),
        });
        mirror.apply_state(&state(&[moved.clone()], 2));
        let record = mirror.record("a").expect("open");
        assert_eq!(record.summary, moved);
        assert_eq!(record.tool.as_deref(), Some("shell_write"));
    }

    #[test]
    fn a_read_for_an_ask_that_already_left_is_dropped() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        mirror.take_reads();
        mirror.apply_state(&state(&[], 2));
        mirror.on_record("a", Ok(Some(summary_record(&ask("a", None, 1)))));
        assert!(mirror.record("a").is_none(), "the departure read answers for it");
    }

    /// A replaced connection's reads are discarded, so the mirror must not
    /// wait on them: every unread ask is read again.
    #[test]
    fn a_replaced_connection_rereads_what_was_in_flight() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        mirror.take_reads();
        mirror.on_connection_changed();
        assert!(!mirror.synced);
        assert_eq!(mirror.take_reads(), ids(&["a"]));
    }

    // ── recent ──

    #[test]
    fn departed_asks_go_to_recent_newest_decision_first() {
        let mut mirror = LedgerMirror::default();
        mirror.on_departed(closed("old", Some(10)));
        mirror.on_departed(closed("undated", None));
        mirror.on_departed(closed("new", Some(20)));
        assert_eq!(recent_ids(&mirror), ids(&["new", "old", "undated"]));
        assert_eq!(mirror.record("new").map(|a| a.summary.request_id), Some("new".into()));
    }

    #[test]
    fn recent_is_capped_and_keeps_the_newest() {
        let mut mirror = LedgerMirror::default();
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            mirror.on_departed(closed(id, Some(i as i64)));
        }
        assert_eq!(recent_ids(&mirror), ids(&["d", "c", "b"]));
        assert_eq!(mirror.recent().len(), RECENT_CAP);
    }

    // ── reads ──

    #[test]
    fn pending_for_context_selects_that_context_oldest_first() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(
            &[
                ask("a", Some(ctx(1)), 1),
                ask("b", Some(ctx(2)), 2),
                ask("c", Some(ctx(1)), 3),
            ],
            1,
        ));
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
    fn an_ask_with_no_context_is_in_no_context_lamp() {
        let mut mirror = LedgerMirror::default();
        mirror.apply_state(&state(&[ask("a", None, 1)], 1));
        assert_eq!(mirror.pending_count(), 1);
        assert!(!mirror.context_is_asking(ctx(1)));
    }

    // ── the decision ──

    fn answered(remembered: Option<RememberResult>) -> kaijutsu_client::AskAnswer {
        Ok(AskAnswered {
            summary: ask("a", None, 1),
            remembered,
        })
    }

    fn refused(kind: AskAnswerFailureKind, message: &str) -> kaijutsu_client::AskAnswer {
        Err(AskAnswerFailure {
            kind,
            message: message.into(),
        })
    }

    #[test]
    fn an_answer_the_ledger_took_is_accepted() {
        assert_eq!(
            classify_answer(Ok(answered(None))),
            DecisionOutcome::Accepted { unlearned: None }
        );
        let learned = RememberResult { learned: true, note: "learned".into() };
        assert_eq!(
            classify_answer(Ok(answered(Some(learned)))),
            DecisionOutcome::Accepted { unlearned: None }
        );
    }

    /// The answer stands when its standing rule was not written, but the
    /// player asked for one and must hear that it is missing.
    #[test]
    fn a_rule_that_was_not_learned_is_reported() {
        let unlearned = RememberResult { learned: false, note: "compound statement".into() };
        assert_eq!(
            classify_answer(Ok(answered(Some(unlearned)))),
            DecisionOutcome::Accepted { unlearned: Some("compound statement".into()) }
        );
    }

    /// A lost race is normal, not a failure — two players share one ledger
    /// and the kernel makes exactly one of them win.
    #[test]
    fn an_already_answered_ask_is_a_lost_race() {
        assert_eq!(
            classify_answer(Ok(refused(AskAnswerFailureKind::AlreadyAnswered, "taken"))),
            DecisionOutcome::AlreadyDecided
        );
    }

    /// Every other refusal keeps the ledger's own words, and a call that
    /// never arrived says so.
    #[test]
    fn any_other_refusal_reports_what_the_kernel_said() {
        for kind in [
            AskAnswerFailureKind::NotFound,
            AskAnswerFailureKind::NotReviewer,
            AskAnswerFailureKind::Archived,
            AskAnswerFailureKind::Refused,
        ] {
            assert_eq!(
                classify_answer(Ok(refused(kind, "the ledger said no"))),
                DecisionOutcome::Failed("the ledger said no".into()),
                "{kind:?}"
            );
        }
        assert_eq!(
            classify_answer(Err("not connected".into())),
            DecisionOutcome::Failed("not connected".into())
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
        assert_eq!(Decision::AllowOnce.answer(), (AskVerdict::Allow, None));
        assert_eq!(Decision::Deny.answer(), (AskVerdict::Deny, None));
        assert_eq!(
            Decision::AllowAlways.answer(),
            (
                AskVerdict::Allow,
                Some(Remember { scope: RememberScope::Always, family: false })
            )
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

    // ── the systems ──

    /// `start_ask_decisions` and the two drains, with no actor: every
    /// decision fails for want of a connection and every read is stale.
    fn decision_app() -> App {
        let mut app = App::new();
        app.init_resource::<RpcConnectionState>()
            .init_resource::<LedgerMirror>()
            .init_resource::<DecisionsInFlight>()
            .insert_resource(DecisionChannel::new())
            .insert_resource(LedgerReadChannel::new())
            .add_message::<AskDecisionRequested>()
            .add_message::<AskDecisionSettled>()
            .add_message::<AskLeft>()
            .add_systems(
                Update,
                (start_ask_decisions, drain_ask_decisions, drain_ledger_reads).chain(),
            );
        app
    }

    fn settled(app: &mut App) -> Vec<(String, DecisionOutcome)> {
        app.world_mut()
            .resource_mut::<Messages<AskDecisionSettled>>()
            .drain()
            .map(|s| (s.request_id, s.outcome))
            .collect()
    }

    /// A second key on an ask whose answer is still in flight is not sent:
    /// it could only lose the race to the first and report that loss to the
    /// player who won it.
    #[test]
    fn a_decision_in_flight_takes_no_second_answer() {
        let mut app = decision_app();
        app.world_mut().resource_mut::<DecisionsInFlight>().begin("a");
        for id in ["a", "b"] {
            app.world_mut().write_message(AskDecisionRequested {
                request_id: id.into(),
                decision: Decision::AllowOnce,
            });
        }
        app.update();
        assert_eq!(
            settled(&mut app),
            vec![("b".to_string(), DecisionOutcome::Failed("no live connection".into()))],
            "the in-flight ask is ignored; another is still answered"
        );
    }

    /// The reply frees the ask, so a key after it is sent again.
    #[test]
    fn a_landed_reply_frees_the_ask_for_another_decision() {
        let mut app = decision_app();
        app.world_mut().resource_mut::<DecisionsInFlight>().begin("a");
        app.world().resource::<DecisionChannel>().tx.send(DecisionResult {
            request_id: "a".into(),
            decision: Decision::Deny,
            outcome: DecisionOutcome::AlreadyDecided,
        }).unwrap();
        app.update();
        assert_eq!(
            settled(&mut app),
            vec![("a".to_string(), DecisionOutcome::AlreadyDecided)],
            "a genuine lost race is still reported"
        );
        assert!(!app.world().resource::<DecisionsInFlight>().contains("a"));
    }

    /// A departure read from a replaced connection is not folded in, but
    /// the departure is still reported, with no record, so nothing waits on
    /// it forever. An open-ask read from it reports nothing.
    #[test]
    fn a_stale_departure_read_still_reports_the_departure() {
        let mut app = decision_app();
        let tx = app.world().resource::<LedgerReadChannel>().tx.clone();
        for (kind, id) in [(ReadKind::Departed, "gone"), (ReadKind::Open, "open")] {
            tx.send(LedgerRead {
                generation: 1,
                connection_epoch: 1,
                kind,
                request_id: id.into(),
                result: Ok(Some(closed(id, Some(5)))),
            })
            .unwrap();
        }
        app.update();
        let left: Vec<(String, bool)> = app
            .world_mut()
            .resource_mut::<Messages<AskLeft>>()
            .drain()
            .map(|l| (l.request_id, l.record.is_some()))
            .collect();
        assert_eq!(left, vec![("gone".to_string(), false)]);
        assert!(app.world().resource::<LedgerMirror>().recent().is_empty());
    }
}
