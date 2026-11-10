//! The context feeds and the mirrors they fill: watching, hydrating and
//! releasing contexts, and applying a delivery to a view.

use anyhow::Result;
use kaijutsu_client::{ActorHandle, FeedEvent, ServerEvent};
use kaijutsu_types::ContextId;
use tokio::sync::mpsc;

use crate::app::{App, ContextView};
use crate::bridge::KernelBridge;

use super::seat::hydrating_notice;

/// One feed delivery, tagged with the context it belongs to — several
/// contexts are watched at once and they share one loop.
pub(super) type TaggedFeed = (ContextId, FeedEvent);

/// The context feeds this client is pumping: one forwarder task per watched
/// context, and the one channel they all forward into.
///
/// Releasing a context aborts its forwarder here and tells the actor to stop
/// re-issuing `subscribeContext` for it (`ActorHandle::unsubscribe_context`).
/// That ends the actor's pump, which drops the observer capability, which is
/// what actually ends the feed on the wire (`kaijutsu_client::rpc`). Held by
/// the loop rather than by [`App`], which stays free of tasks and
/// connections.
pub(super) struct Feeds {
    tx: mpsc::Sender<TaggedFeed>,
    tasks: std::collections::HashMap<ContextId, tokio::task::JoinHandle<()>>,
    /// Where a hydrated context is delivered — cloned into each round's
    /// task, received by the loop's own arm.
    hydrated_tx: mpsc::Sender<Hydrated>,
    /// The actor whose `context_feeds` map a release must also clear — see
    /// [`Self::stop`].
    actor: ActorHandle,
    /// The contexts a background hydrate is in flight for. The round owns
    /// the task; the ids live here because every watch path already carries
    /// `Feeds` and every one of them has to check them.
    ///
    /// The actor keeps one feed sender per context and a second
    /// `subscribe_context` replaces it, so two hydrates of one context race:
    /// whichever subscribed second owns the reconnect, and the receiver the
    /// client kept may be the other one — a transcript that silently stops
    /// updating after a reconnect. One hydrate per context at a time closes
    /// it; a switch to a context already being hydrated waits for that one
    /// rather than starting a second.
    pub(super) hydrating: std::collections::HashSet<ContextId>,
}

impl Feeds {
    pub(super) fn new(tx: mpsc::Sender<TaggedFeed>, hydrated_tx: mpsc::Sender<Hydrated>, actor: ActorHandle) -> Self {
        Self {
            tx,
            tasks: std::collections::HashMap::new(),
            hydrated_tx,
            hydrating: std::collections::HashSet::new(),
            actor,
        }
    }

    /// Forward `rx`'s deliveries into the loop's channel, tagged with the
    /// context they belong to, until the feed ends or [`Self::stop`] aborts
    /// it. A second call for the same context replaces the first task.
    fn pump(&mut self, context_id: ContextId, mut rx: mpsc::Receiver<FeedEvent>) {
        let tx = self.tx.clone();
        // `spawn_local`: the actor's Cap'n Proto types are `!Send`, so this
        // task belongs to the caller's `LocalSet`.
        let task = tokio::task::spawn_local(async move {
            while let Some(event) = rx.recv().await {
                if tx.send((context_id, event)).await.is_err() {
                    break;
                }
            }
        });
        if let Some(previous) = self.tasks.insert(context_id, task) {
            previous.abort();
        }
    }

    /// Stop forwarding `context_id`'s feed and let the subscription go.
    ///
    /// Non-blocking: `unsubscribe_context` only touches the actor's own
    /// in-memory map, no wire round trip, so this stays safe to call from
    /// inside `event_loop` without awaiting anything.
    fn stop(&mut self, context_id: ContextId) {
        if let Some(task) = self.tasks.remove(&context_id) {
            task.abort();
        }
        self.actor.unsubscribe_context(context_id);
    }
}

/// Subscribe to a context's feed, hydrate its mirror, and forward its
/// deliveries into the shared loop channel.
///
/// The key path's way in: a switch to a cold context hydrates it here and
/// then shows it. A context already watched is a no-op, and so is one the
/// hot set is already hydrating in the background ([`Feeds::hydrating`]) —
/// that round lands through the loop's own arm a moment later and the
/// transcript fills then, which is the same wait a hydrate here would have
/// cost.
pub(super) async fn watch_context(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    feeds: &mut Feeds,
) -> Result<()> {
    if app.views.contains_key(&context_id) || feeds.hydrating.contains(&context_id) {
        return Ok(());
    }
    let (mirror, rx) = bridge.hydrate_context(context_id).await?;
    adopt(app, feeds, context_id, mirror, rx);
    Ok(())
}

/// Take a hydrated context into the client: its mirror becomes a view and
/// its feed starts forwarding.
///
/// The one place a context becomes resident, whether a switch hydrated it on
/// the key path or the hot set hydrated it in the background. The log line
/// is one per context made resident, which is what a probe counts to tell a
/// hydrate from a redraw.
pub(super) fn adopt(
    app: &mut App,
    feeds: &mut Feeds,
    context_id: ContextId,
    mirror: kaijutsu_client::ContextMirror,
    rx: mpsc::Receiver<FeedEvent>,
) {
    app.views.insert(context_id, ContextView::new(mirror));
    feeds.pump(context_id, rx);
    tracing::debug!(context = %context_id.short(), "watching context feed");
}

/// What one hydrate round answers for a context: a context newly watched
/// carries a fresh feed receiver along with its mirror
/// ([`KernelBridge::hydrate_context`], [`start_hydrate`]); a context already
/// watched, rebuilt after `FeedEvent::Resubscribed`, keeps the receiver it
/// already has and carries only a snapshot ([`KernelBridge::rehydrate_context`],
/// [`start_rehydrate`]) — the landing arm applies it to the SAME mirror
/// ([`land_rehydrate`]) rather than building a new one, since that mirror
/// switched to buffering before the round started and must filter what it
/// buffered against this exact snapshot. Each carries its own `Result`, so a
/// failure says which kind of request it was.
pub(super) enum HydrateOutcome {
    Fresh(Result<(kaijutsu_client::ContextMirror, mpsc::Receiver<FeedEvent>)>),
    Rebuilt(Result<(Vec<kaijutsu_types::BlockSnapshot>, u64)>),
}

/// One hydrate or rehydrate answered in the background: the id asked for,
/// and what the kernel answered.
pub(super) type Hydrated = (ContextId, HydrateOutcome);

/// The contexts a round hydrates: every hot context this client is not
/// watching and is not already hydrating. Pure, so a test can ask what a
/// round would take on without a kernel.
fn hydrate_round(app: &App, in_flight: &std::collections::HashSet<ContextId>) -> Vec<ContextId> {
    app.unwatched_hot()
        .into_iter()
        .filter(|id| !in_flight.contains(id))
        .collect()
}

/// Hydrate everything the hot set is missing, in one round.
///
/// Off the loop, the way a refresh round runs: the hydrates are kernel round
/// trips, and a key must never queue behind one (`docs/tui.md`, "Keys"). One
/// task takes the whole round and asks for each context in turn — the ring
/// is kernel-capped at ten seats, so this is a bounded burst rather than a
/// fan-out — and sends each answer as it lands, so a context goes resident
/// the moment it is ready instead of waiting for the slowest.
pub(super) fn start_hydrate(bridge: &KernelBridge, app: &App, feeds: &mut Feeds) {
    let round = hydrate_round(app, &feeds.hydrating);
    if round.is_empty() {
        return;
    }
    feeds.hydrating.extend(round.iter().copied());
    let bridge = bridge.clone();
    let tx = feeds.hydrated_tx.clone();
    tokio::spawn(async move {
        let mut guard = HydrateGuard { remaining: round.clone(), tx: tx.clone(), wrap: fresh_failed };
        for context_id in round {
            let hydrated = bridge.hydrate_context(context_id).await;
            if tx.send((context_id, HydrateOutcome::Fresh(hydrated))).await.is_err() {
                break;
            }
            guard.remaining.retain(|id| *id != context_id);
        }
    });
}

/// Ask for one watched context's mirror to be rebuilt after
/// `FeedEvent::Resubscribed`, off the loop — [`start_hydrate`]'s twin for a
/// context that is already resident rather than one the hot set is missing.
/// `rehydrate_context` does not re-subscribe (the actor already
/// re-subscribed on the reconnect and this client keeps the same receiver,
/// `KernelBridge::hydrate_context`'s doc comment) and does not build a
/// mirror either: only a snapshot comes back — [`HydrateOutcome::Rebuilt`]
/// carries that — for the landing arm to apply to the SAME mirror the
/// caller already put into buffering mode
/// ([`ContextMirror::begin_rehydrate`]) before calling this.
///
/// Guarded by the same [`Feeds::hydrating`] set `start_hydrate` uses: a
/// second `Resubscribed` for a context already being rebuilt waits for that
/// round instead of racing it.
fn start_rehydrate(bridge: &KernelBridge, context_id: ContextId, feeds: &mut Feeds) {
    if !feeds.hydrating.insert(context_id) {
        return;
    }
    let bridge = bridge.clone();
    let tx = feeds.hydrated_tx.clone();
    // `tokio::spawn`, not `spawn_local` like `Feeds::pump`: this task only
    // touches the `Send` `KernelBridge`/`ActorHandle` and plain data, no
    // Cap'n Proto types, so it does not need the caller's `LocalSet`.
    tokio::spawn(async move {
        let mut guard = HydrateGuard { remaining: vec![context_id], tx: tx.clone(), wrap: rebuilt_failed };
        let rehydrated = bridge.rehydrate_context(context_id).await;
        if tx.send((context_id, HydrateOutcome::Rebuilt(rehydrated))).await.is_ok() {
            guard.remaining.clear();
        }
    });
}

/// Answers the loop for every id a round did not reach.
///
/// A round that panics, or is dropped because the runtime is going away,
/// would otherwise leave its ids in [`Feeds::hydrating`] for the life of the
/// session, and those contexts could never go resident again — a switch to
/// one would wait forever on a round that ended. The guard sends an `Err`
/// for each id still outstanding as it drops, which the loop's own error arm
/// clears.
struct HydrateGuard {
    /// The round's ids, each removed once its own answer has been sent.
    remaining: Vec<ContextId>,
    tx: mpsc::Sender<Hydrated>,
    /// How a stranded id's error is wrapped for the landing arm:
    /// [`fresh_failed`] for [`start_hydrate`]'s round, [`rebuilt_failed`] for
    /// [`start_rehydrate`]'s single id — the two ask the kernel for
    /// different things and the landing arm's `Err` handling differs
    /// accordingly.
    wrap: fn(anyhow::Error) -> HydrateOutcome,
}

impl Drop for HydrateGuard {
    fn drop(&mut self) {
        for context_id in self.remaining.drain(..) {
            // `try_send`, because a `Drop` cannot await. The channel holds
            // more slots than a round has answers to give, so a full channel
            // means the loop has stopped draining it — it is shutting down,
            // and a stranded id no longer matters. It is said out loud
            // rather than swallowed either way.
            let answer = (self.wrap)(anyhow::anyhow!(
                "the hydrate round ended before {} was answered",
                context_id.short()
            ));
            if self.tx.try_send((context_id, answer)).is_err() {
                tracing::warn!(
                    context = %context_id.short(),
                    "a hydrate answer was dropped; the context stays marked in flight"
                );
            }
        }
    }
}

fn fresh_failed(e: anyhow::Error) -> HydrateOutcome {
    HydrateOutcome::Fresh(Err(e))
}

fn rebuilt_failed(e: anyhow::Error) -> HydrateOutcome {
    HydrateOutcome::Rebuilt(Err(e))
}

/// Drop every watched context that has left the hot set, and stop its feed.
/// No kernel call, so a key path may run it.
///
/// The forwarder goes first and the view second: a delivery that reached the
/// loop before the abort finds no view and is discarded ([`apply_feed`]), and
/// nothing can start forwarding into a view that is on its way out.
pub(super) fn release_cold(app: &mut App, feeds: &mut Feeds) {
    for context_id in app.cold_contexts() {
        feeds.stop(context_id);
        app.release(context_id);
        tracing::debug!(context = %context_id.short(), "released a cold context");
    }
}

/// Track which contexts this client believes have a turn running — the
/// partial signal `App::turns_running` documents: it is set here on
/// `ServerEvent::TurnStarted` (the other setter is `compose_key`'s own
/// submit) and cleared on `TurnCompleted`/`TurnFailed`.
pub(super) fn mark_turn_liveness(app: &mut App, event: &ServerEvent) -> bool {
    match event {
        ServerEvent::TurnStarted { context_id, .. } => app.mark_turn_running(*context_id),
        ServerEvent::TurnCompleted { context_id, .. } | ServerEvent::TurnFailed { context_id, .. } => {
            app.mark_turn_ended(*context_id)
        }
        _ => false,
    }
}

/// Apply one feed event to its mirror: the transcript redraws from the
/// mirror, so nothing here has to say what a change could not reach.
///
/// A `Resubscribed` delivery only asks for a rehydrate ([`start_rehydrate`]);
/// it never awaits one. Before asking, it puts the EXISTING mirror back into
/// buffering mode ([`ContextMirror::begin_rehydrate`]) rather than leaving it
/// open to ordinary deliveries: the loop keeps draining `feed_rx` while the
/// round is in flight, and a delivery that lands between the kernel's
/// snapshot and this client's next look at it must wait for that snapshot
/// instead of applying to — and then being discarded with — the mirror the
/// round is about to replace. The mirror stays in that state until
/// `hydrated_rx`'s answer lands, the same wait a cold context's own hydrate
/// already costs.
pub(super) fn apply_feed(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    event: FeedEvent,
    feeds: &mut Feeds,
) {
    if apply_delivery(app, feeds, context_id, event) == Rehydrate::Needed {
        // `apply_delivery` only returns `Needed` when the context still has
        // a view (`docs/tui.md`, "The buffer"), so this always finds one;
        // the guard stays rather than assumed, in case that ever changes.
        if let Some(view) = app.views.get_mut(&context_id) {
            view.mirror.begin_rehydrate();
            start_rehydrate(bridge, context_id, feeds);
        }
    }
}

/// Whether a delivery left a rehydrate for the caller — the one thing in a
/// feed event that needs the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rehydrate {
    No,
    Needed,
}

/// Everything one delivery does that needs no kernel call.
///
/// A context with no view was released or its feed ended, and a delivery
/// still in the loop's channel has nowhere to land: nothing here touches it,
/// no notice names it, and no arm can resurrect it — including the rehydrate,
/// which is never asked for. The picker's own tails and activity flags do
/// not come through here at all; they ride the kernel-wide `ServerEvent`
/// stream, ungated by what is watched (`docs/tui.md`, "The picker").
fn apply_delivery(
    app: &mut App,
    feeds: &mut Feeds,
    context_id: ContextId,
    event: FeedEvent,
) -> Rehydrate {
    if !app.views.contains_key(&context_id) {
        tracing::debug!(
            context = %context_id.short(),
            "a delivery for a context with no view; dropped"
        );
        return Rehydrate::No;
    }
    match event {
        FeedEvent::Changed(delivery) => {
            for change in delivery.changes() {
                // A block the kernel dropped will never render again, so its
                // wrapped lines go with it.
                if let kaijutsu_client::ContextChange::BlockDeleted { block_id } = change {
                    app.wrap.forget(block_id);
                }
                app.apply_collapse_change(context_id, change);
            }
            let mut hydrated = false;
            if let Some(view) = app.views.get_mut(&context_id) {
                if let Err(e) = view.mirror.receive(delivery) {
                    tracing::warn!(context = %context_id.short(), error = %e, "mirror rejected a delivery");
                }
                view.auto_fold();
                hydrated = view.mirror.is_hydrated();
            }
            // While buffering (`ContextMirror::begin_rehydrate`), the
            // mirror's draft text and version are frozen at the
            // pre-reconnect state. That is harmless for the context already
            // on screen — `Compose::reconcile` refuses a version older than
            // its own `acked` — but `switch_seat`'s `read_input` resets
            // `acked` to 0 when the player switches onto this context
            // mid-round (`compose::Compose::load_draft`), so a stale
            // reconcile here is no longer refused and would overwrite the
            // freshly loaded draft with the frozen one. `land_rehydrate`
            // reconciles again once the snapshot applies.
            if hydrated {
                reconcile_draft(app, context_id);
            }
        }
        FeedEvent::Resubscribed => return Rehydrate::Needed,
        FeedEvent::Terminated { reason, .. } => {
            // The subscriber fell behind and this receiver is dead. Stop the
            // forwarder and drop the view so the next switch re-subscribes
            // from scratch, and the wraps with it.
            feeds.stop(context_id);
            app.release(context_id);
            // A context with no view renders nothing — no transcript, no
            // stream — so name the gesture that re-subscribes rather than
            // the prefix alone. Any switch does it, including a switch to
            // the seat already on screen.
            app.note(format!(
                "{} feed ended ({reason:?}); Ctrl+A <digit> reattaches",
                app.label_for(context_id)
            ));
        }
    }
    Rehydrate::No
}

/// Land a rebuilt snapshot once [`start_rehydrate`]'s answer comes back
/// through `hydrated_rx` — applied to the SAME mirror the actor already
/// re-subscribed for this client ([`FeedEvent::Resubscribed`]) put into
/// buffering mode before the round started
/// ([`kaijutsu_client::ContextMirror::begin_rehydrate`]), so whatever
/// buffered mid-round is filtered against this exact snapshot rather than
/// applied to — and lost with — a wholesale replacement mirror.
///
/// Returns whether anything changed on screen. The context can have been
/// released while the round was in flight; a snapshot with no view to land
/// on is dropped the same way an ordinary feed delivery is when its view is
/// gone ([`apply_delivery`]) — every other exit from this function posts its
/// own notice (below, and [`abandon_rehydrate`]), which is what stands in
/// for clearing a stale `hydrating_notice`; this one posts nothing, so it
/// takes that notice down itself if it is the one still showing, rather
/// than leave the status row claiming a round is running that has already
/// answered with nowhere to land. A snapshot that fails to apply —
/// `MirrorError`, from something buffered during the round that the
/// snapshot's version does not cover — would otherwise leave the mirror
/// buffering forever with no round ever coming to finish it, so this
/// releases the view instead, the same recovery `FeedEvent::Terminated`
/// already gives a feed that cannot continue: the next switch re-subscribes
/// and hydrates from scratch.
pub(super) fn land_rehydrate(
    app: &mut App,
    feeds: &mut Feeds,
    context_id: ContextId,
    blocks: Vec<kaijutsu_types::BlockSnapshot>,
    version: u64,
) -> bool {
    let Some(view) = app.views.get_mut(&context_id) else {
        // The switch that waited for this round may have posted
        // `hydrating_notice` before the release; nothing else is ever
        // coming to answer it, so it comes down here the same way a
        // successful adopt already takes it down for a fresh hydrate.
        if app.notice() == Some(hydrating_notice(app, context_id).as_str()) {
            app.clear_notice();
            return true;
        }
        return false;
    };
    let applied = view.mirror.apply_snapshot(blocks, version);
    if applied.is_ok() {
        view.auto_fold();
    }
    match applied {
        Ok(()) => {
            reconcile_draft(app, context_id);
            // The feed only says `Resubscribed` on a new connection,
            // so the stream that clears a turn flag was broken. The
            // status watch coalesces, so a fast reconnect can leave
            // this the only sign: forget what this client believed
            // was running rather than carry a flag nothing clears
            // (`docs/tui.md`, "Turn liveness is a partial signal").
            app.forget_turn_liveness();
            app.note("reconnected; context rehydrated; turn liveness reset");
        }
        Err(e) => abandon_rehydrate(app, feeds, context_id, e),
    }
    true
}

/// Give up on a context's rehydrate: a failed round trip
/// ([`HydrateOutcome::Rebuilt`]'s own `Err`) or a snapshot that landed but
/// would not apply ([`land_rehydrate`]'s `MirrorError`). Either way the
/// mirror is stuck in the buffering mode `ContextMirror::begin_rehydrate`
/// put it in with no round left in flight to end it, so retrying is not
/// bounded (nothing here caps how many times a persistent failure would be
/// retried) and waiting is not recoverable on its own — releasing is. The
/// same recovery `FeedEvent::Terminated` already gives a feed that cannot
/// continue: the next switch re-subscribes and hydrates from scratch.
pub(super) fn abandon_rehydrate(app: &mut App, feeds: &mut Feeds, context_id: ContextId, error: impl std::fmt::Display) {
    feeds.stop(context_id);
    app.release(context_id);
    app.note(format!(
        "{} could not finish reconnecting: {error}; Ctrl+A <digit> reattaches",
        app.label_for(context_id)
    ));
}

/// Draw the compose line from kernel state rather than from the local buffer
/// alone. The draft is an ordinary block on the same change feed, so a
/// sibling typing into it arrives here with every other block edit.
fn reconcile_draft(app: &mut App, context_id: ContextId) {
    if app.current != Some(context_id) {
        return;
    }
    let Some((text, version)) = app.current_draft() else {
        return;
    };
    app.compose.reconcile(&text, version);
}

/// Mark a context as having moved since it was last on screen — screen's `@`
/// flag. Returns whether anything changed.
pub(super) fn mark_activity(app: &mut App, event: &ServerEvent) -> bool {
    let context_id = match event {
        ServerEvent::BlockInserted { context_id, .. }
        | ServerEvent::BlockStatusChanged { context_id, .. }
        | ServerEvent::BlockOutputChanged { context_id, .. }
        | ServerEvent::BlockMetadataChanged { context_id, .. } => *context_id,
        _ => return false,
    };
    if app.current == Some(context_id) {
        return false;
    }
    match app.views.get_mut(&context_id) {
        Some(view) if !view.activity => {
            view.activity = true;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::compose::Compose;
    use crate::run::seat::hydrating_notice;
    use kaijutsu_client::{
        ContextChange, ContextDelivery, ContextMirror, TurnCompletedStopReason, TurnOrigin, VersionedChange,
    };
    use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId, Role};

    /// Watch a context with an empty mirror — what `watch_context` leaves
    /// behind, without a kernel to hydrate from.
    pub(in crate::run) fn watched(app: &mut App, id: ContextId) {
        app.views.insert(id, ContextView::new(ContextMirror::new(id)));
    }

    // ────────────────────────────────────────────────────────────────────
    // Turn liveness (docs/tui.md, "Ctrl+C reclaimed")
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn turn_started_marks_the_context_running() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let event = ServerEvent::TurnStarted { turn_id: kaijutsu_types::TurnId::new(), context_id: id, principal_id: PrincipalId::new() };
        assert!(mark_turn_liveness(&mut app, &event));
        assert!(app.turn_running(id));
    }

    #[test]
    fn turn_completed_clears_a_running_context() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.mark_turn_running(id);
        let event = ServerEvent::TurnCompleted {
            turn_id: kaijutsu_types::TurnId::new(),
            context_id: id,
            principal_id: PrincipalId::new(),
            output_block_id: None,
            stop_reason: TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert!(mark_turn_liveness(&mut app, &event));
        assert!(!app.turn_running(id));
    }

    #[test]
    fn turn_failed_also_clears_a_running_context() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.mark_turn_running(id);
        let event = ServerEvent::TurnFailed {
            turn_id: kaijutsu_types::TurnId::new(),
            context_id: id,
            principal_id: PrincipalId::new(),
            error: "provider stream error".to_string(),
            origin: TurnOrigin::Autonomous,
        };
        assert!(mark_turn_liveness(&mut app, &event));
        assert!(!app.turn_running(id));
    }

    /// A completion for a context nobody marked running is not a bug — the
    /// set just has nothing to remove.
    #[test]
    fn turn_completed_for_a_context_with_no_known_running_turn_is_a_no_op() {
        let mut app = App::new("amy");
        let event = ServerEvent::TurnCompleted {
            turn_id: kaijutsu_types::TurnId::new(),
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            output_block_id: None,
            stop_reason: TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert!(!mark_turn_liveness(&mut app, &event));
    }

    /// A `Feeds` with no tasks and nowhere to deliver but the test's own
    /// channels — enough for the paths that only read `hydrating` or stop a
    /// forwarder that was never started.
    pub(in crate::run) fn feeds_for_test() -> (Feeds, mpsc::Receiver<Hydrated>) {
        let (tx, _feed_rx) = mpsc::channel::<TaggedFeed>(4);
        let (hydrated_tx, hydrated_rx) = mpsc::channel::<Hydrated>(8);
        // Never answers and nobody reads it: `stop`'s unsubscribe is
        // fire-and-forget, so these tests never wait on it either.
        let (actor, _cmds) = ActorHandle::never_answers_for_test();
        (Feeds::new(tx, hydrated_tx, actor), hydrated_rx)
    }

    // ────────────────────────────────────────────────────────────────────
    // Deliveries for a context that is gone (docs/tui.md, "The buffer")
    // ────────────────────────────────────────────────────────────────────

    /// A release stops the forwarder before it drops the view, but a
    /// delivery already in the loop's channel still arrives. It has nowhere
    /// to land: nothing changes, nothing is said, and the rehydrate a
    /// `Resubscribed` would ask for is never asked for — that call would
    /// resurrect a context this client deliberately let go.
    #[test]
    fn a_delivery_for_a_context_with_no_view_changes_nothing() {
        let gone = ContextId::new();
        let mut app = App::new("amy");
        let (mut feeds, _hydrated) = feeds_for_test();

        let terminated = FeedEvent::Terminated {
            reason: kaijutsu_client::subscriptions::SubscriptionEndReason::SlowSubscriber,
            delivered_version: 7,
        };
        assert_eq!(apply_delivery(&mut app, &mut feeds, gone, terminated), Rehydrate::No);
        assert_eq!(app.notice(), None, "a context that is gone is not named again");
        assert!(app.views.is_empty());

        assert_eq!(
            apply_delivery(&mut app, &mut feeds, gone, FeedEvent::Resubscribed),
            Rehydrate::No,
            "no kernel call for a context with no view"
        );
        assert_eq!(app.notice(), None);
    }

    /// A watched context still takes both: its feed ending says so, and a
    /// resubscribe still asks for the fresh snapshot.
    #[test]
    fn a_delivery_for_a_watched_context_still_lands() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        watched(&mut app, id);
        let (mut feeds, _hydrated) = feeds_for_test();

        assert_eq!(
            apply_delivery(&mut app, &mut feeds, id, FeedEvent::Resubscribed),
            Rehydrate::Needed
        );

        let terminated = FeedEvent::Terminated {
            reason: kaijutsu_client::subscriptions::SubscriptionEndReason::SlowSubscriber,
            delivered_version: 7,
        };
        assert_eq!(apply_delivery(&mut app, &mut feeds, id, terminated), Rehydrate::No);
        assert!(app.views.is_empty(), "the feed ended, so the view went");
        assert!(
            app.notice().is_some_and(|n| n.contains("feed ended")),
            "the player is told how to reattach: {:?}",
            app.notice()
        );
    }

    /// `apply_feed` on `Resubscribed` only asks for a rehydrate
    /// ([`start_rehydrate`]) — it never awaits one. Entering a runtime
    /// without driving it (no `block_on`, no `#[tokio::test]`) proves the
    /// point: `tokio::spawn` inside `apply_feed` needs a runtime context to
    /// register the task, but nothing here ever polls it, so a bridge that
    /// never answers is never asked to.
    #[test]
    fn a_resubscribed_delivery_requests_a_rehydrate_without_awaiting_one() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("build a runtime");
        let _entered = runtime.enter();

        let id = ContextId::new();
        let mut app = App::new("amy");
        watched(&mut app, id);
        let (mut feeds, _hydrated) = feeds_for_test();
        let bridge = KernelBridge::never_answers_for_test();

        apply_feed(&bridge, &mut app, id, FeedEvent::Resubscribed, &mut feeds);

        assert!(feeds.hydrating.contains(&id), "a rehydrate round was requested");
    }

    /// A `BlockSnapshot` for a mirror this test hydrates by hand — content
    /// and a distinct id, nothing else the mirror looks at.
    fn text_block(context_id: ContextId, seq: u64, content: &str) -> BlockSnapshot {
        BlockSnapshot::text(BlockId::new(context_id, PrincipalId::new(), seq), None, Role::User, content)
    }

    /// The hole a wholesale mirror swap left: the loop keeps draining
    /// `feed_rx` while a rehydrate round is in flight, and a delivery that
    /// lands between the kernel taking its snapshot and this client landing
    /// it must not be applied to — and then thrown away with — the mirror
    /// the round is about to replace. `apply_feed`'s `Resubscribed` handling
    /// puts the SAME mirror into buffering mode first
    /// (`ContextMirror::begin_rehydrate`), so a delivery the eventual
    /// snapshot does not cover waits for it instead of being lost.
    #[test]
    fn a_delivery_mid_rehydrate_survives_a_snapshot_that_predates_it() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("build a runtime");
        let _entered = runtime.enter();

        let id = ContextId::new();
        let a = text_block(id, 1, "a");
        let mut app = App::new("amy");
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(vec![a.clone()], 5).unwrap();
        app.views.insert(id, ContextView::new(mirror));
        let (mut feeds, _hydrated) = feeds_for_test();
        let bridge = KernelBridge::never_answers_for_test();

        // The reconnect: puts the existing mirror into buffering mode and
        // requests a rehydrate round (never driven here — see the test
        // above).
        apply_feed(&bridge, &mut app, id, FeedEvent::Resubscribed, &mut feeds);

        // A delivery lands mid-round, at a version past what the snapshot
        // below was read at.
        let x = text_block(id, 2, "x");
        let changed = FeedEvent::Changed(ContextDelivery {
            context_id: id,
            events: vec![VersionedChange {
                version: 8,
                change: ContextChange::BlockInserted { block: Box::new(x.clone()), after_id: Some(a.id) },
            }],
            version: 8,
        });
        apply_feed(&bridge, &mut app, id, changed, &mut feeds);

        // The round's own answer: the kernel's state as read at version 7 —
        // before the version-8 insert above.
        assert!(land_rehydrate(&mut app, &mut feeds, id, vec![a.clone()], 7));

        let ids: Vec<_> =
            app.views.get(&id).expect("the view survives a successful rehydrate").mirror.blocks().iter().map(|b| b.id).collect();
        assert!(ids.contains(&x.id), "the version-8 delivery must not be lost: {ids:?}");
    }

    /// Another hole the same buffering window opens: `apply_delivery`'s
    /// `Changed` arm reconciles the compose line against the mirror on every
    /// delivery, even while `ContextMirror::begin_rehydrate` has it frozen at
    /// the pre-reconnect draft and version. For the context already on
    /// screen that freeze is harmless — `Compose::reconcile` refuses a
    /// version older than its own `acked` — but `switch_seat`'s
    /// `read_input` + `Compose::load_draft` resets `acked` to 0 when the
    /// player switches onto a context mid-rehydrate, so the stale reconcile
    /// is no longer refused and overwrites the freshly loaded draft with the
    /// frozen one.
    #[test]
    fn a_changed_delivery_does_not_reconcile_a_buffering_mirrors_frozen_draft() {
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = App::new("amy");
        app.principal = Some(principal);
        app.current = Some(id);

        let draft_id = BlockId::new(id, principal, 1);
        let mut draft = BlockSnapshot::text(draft_id, None, Role::User, "old");
        draft.status = kaijutsu_types::Status::Draft;
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(vec![draft], 5).unwrap();
        app.views.insert(id, ContextView::new(mirror));

        // The reconnect: freezes the mirror's draft at "old"/version 5.
        app.views.get_mut(&id).unwrap().mirror.begin_rehydrate();

        // The switch onto this context mid-round: `read_input` loads the
        // kernel's fresh draft the way `switch_seat` does, which resets
        // `Compose`'s `acked` to 0 (`compose::Compose::load_draft`).
        app.compose = Compose::over("new");

        // Some other block changes while the round is still in flight.
        let other = BlockId::new(id, PrincipalId::new(), 2);
        let (mut feeds, _hydrated) = feeds_for_test();
        let changed = FeedEvent::Changed(ContextDelivery {
            context_id: id,
            events: vec![VersionedChange {
                version: 9,
                change: ContextChange::TextAppended { block_id: other, suffix: "x".into() },
            }],
            version: 9,
        });
        apply_delivery(&mut app, &mut feeds, id, changed);

        assert_eq!(
            app.compose.text(),
            "new",
            "a delivery against a still-buffering mirror must not overwrite the fresh draft"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // The draft after a submit (docs/tui.md, "Compose")
    // ────────────────────────────────────────────────────────────────────

    /// One change delivered to `id`'s feed at `version`.
    fn deliver(app: &mut App, feeds: &mut Feeds, id: ContextId, version: u64, change: ContextChange) {
        let delivery = ContextDelivery { context_id: id, events: vec![VersionedChange { version, change }], version };
        apply_delivery(app, feeds, id, FeedEvent::Changed(delivery));
    }

    /// `principal`'s draft block in `id` with sequence number `seq`.
    fn draft_block(id: ContextId, principal: PrincipalId, seq: u64, text: &str) -> BlockSnapshot {
        let mut draft = BlockSnapshot::text(BlockId::new(id, principal, seq), None, Role::User, text);
        draft.status = kaijutsu_types::Status::Draft;
        draft
    }

    /// An app on `id` whose mirror is hydrated at version 3 with no draft
    /// in it, as it is when Enter lands before the first keystroke echoes.
    fn app_on_an_unechoed_draft(id: ContextId, principal: PrincipalId) -> App {
        let mut app = App::new("amy");
        app.principal = Some(principal);
        app.current = Some(id);
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(vec![], 3).unwrap();
        app.views.insert(id, ContextView::new(mirror));
        app.compose = Compose::over("");
        app
    }

    /// A shell submit answers with its command block, which the kernel
    /// numbers after the draft it ran, so the draft's late echo is skipped
    /// even though the mirror never showed it before the submit.
    #[test]
    fn a_draft_the_mirror_had_not_seen_at_submit_is_not_restored() {
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = app_on_an_unechoed_draft(id, principal);
        app.mark_submitted(BlockId::new(id, principal, 9)); // the shell submit's command block
        app.compose.reset();
        let draft = draft_block(id, principal, 1, "echo hi");
        let (mut feeds, _h) = feeds_for_test();
        deliver(&mut app, &mut feeds, id, 10, ContextChange::BlockInserted { block: Box::new(draft.clone()), after_id: None });
        let after_insert = app.compose.text();
        deliver(&mut app, &mut feeds, id, 11, ContextChange::BlockDeleted { block_id: draft.id });
        assert_eq!((after_insert.as_str(), app.compose.text().as_str()), ("", ""));
    }

    /// The player starts the next draft before the submitted one's insert
    /// and delete have arrived. The old text never replaces theirs, and the
    /// new draft, numbered after the submit, is read again once it echoes.
    #[test]
    fn a_draft_typed_right_after_a_submit_keeps_its_text() {
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = app_on_an_unechoed_draft(id, principal);
        app.mark_submitted(BlockId::new(id, principal, 9));
        app.compose = Compose::over("x");
        let generation = app.compose.sent();
        let (mut feeds, _h) = feeds_for_test();
        let old = draft_block(id, principal, 1, "echo hi");
        deliver(&mut app, &mut feeds, id, 10, ContextChange::BlockInserted { block: Box::new(old.clone()), after_id: None });
        assert_eq!(app.compose.text(), "x", "the submitted draft's echo");
        deliver(&mut app, &mut feeds, id, 11, ContextChange::BlockDeleted { block_id: old.id });
        assert_eq!(app.compose.text(), "x", "the submitted draft's delete");
        app.compose.landed(generation, Some(13));
        let next = draft_block(id, principal, 10, "x");
        deliver(&mut app, &mut feeds, id, 13, ContextChange::BlockInserted { block: Box::new(next.clone()), after_id: None });
        assert_eq!(app.current_draft().map(|(text, _)| text).as_deref(), Some("x"), "the new draft is ours to read");
        deliver(&mut app, &mut feeds, id, 14, ContextChange::TextAppended { block_id: next.id, suffix: "y".into() });
        assert_eq!(app.compose.text(), "xy", "a sibling's newer edit to the new draft still lands");
    }

    /// After our submit, a sibling of the same principal starts the next
    /// draft. It is numbered after the submit, so it is the draft, and its
    /// text is newer than anything we sent.
    #[test]
    fn a_siblings_draft_started_after_our_submit_shows() {
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = app_on_an_unechoed_draft(id, principal);
        app.mark_submitted(BlockId::new(id, principal, 9));
        app.compose.reset();
        let (mut feeds, _h) = feeds_for_test();
        let next = draft_block(id, principal, 10, "from the app");
        deliver(&mut app, &mut feeds, id, 20, ContextChange::BlockInserted { block: Box::new(next), after_id: None });
        assert_eq!(app.compose.text(), "from the app");
    }

    /// A reconnect lands a snapshot instead of the deltas. One taken before
    /// the submitted draft was consumed still holds it; one taken after holds
    /// whatever draft came next.
    #[test]
    fn a_rehydrate_snapshot_does_not_restore_the_submitted_draft() {
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = app_on_an_unechoed_draft(id, principal);
        app.mark_submitted(BlockId::new(id, principal, 9));
        app.compose.reset();
        let (mut feeds, _h) = feeds_for_test();
        app.views.get_mut(&id).unwrap().mirror.begin_rehydrate();
        assert!(land_rehydrate(&mut app, &mut feeds, id, vec![draft_block(id, principal, 1, "echo hi")], 10));
        assert_eq!(app.compose.text(), "", "a snapshot from before the consume");
        app.views.get_mut(&id).unwrap().mirror.begin_rehydrate();
        assert!(land_rehydrate(&mut app, &mut feeds, id, vec![draft_block(id, principal, 10, "next")], 30));
        assert_eq!(app.compose.text(), "next", "a snapshot from after it");
    }

    /// Each context keeps its own submit: a submit in another context while
    /// this one's echo is still on the way does not let the echo back in.
    #[test]
    fn a_submit_elsewhere_does_not_forget_this_contexts_submit() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let principal = PrincipalId::new();
        let mut app = app_on_an_unechoed_draft(a, principal);
        app.mark_submitted(BlockId::new(a, principal, 9));
        app.compose.reset();
        watched(&mut app, b);
        app.current = Some(b);
        app.mark_submitted(BlockId::new(b, principal, 4));
        app.current = Some(a);
        app.compose = Compose::over("");
        let (mut feeds, _h) = feeds_for_test();
        let old = draft_block(a, principal, 1, "echo hi");
        deliver(&mut app, &mut feeds, a, 10, ContextChange::BlockInserted { block: Box::new(old), after_id: None });
        assert_eq!(app.compose.text(), "");
    }

    /// `X` was released (`Terminated` or `release_cold`) while its rehydrate
    /// round was still in flight, and the player switched to it before the
    /// round landed — `switch_seat` posts `hydrating_notice` for exactly
    /// this case. When the round lands there is no view to apply the
    /// snapshot to, so nothing else would ever take that notice down: the
    /// success path a `Fresh` adopt clears it on (`hydrated_rx`'s landing
    /// arm) does not run for a missing view either.
    #[test]
    fn a_landed_rehydrate_with_no_view_clears_a_stale_hydrating_notice() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.note(hydrating_notice(&app, id));
        let (mut feeds, _hydrated) = feeds_for_test();

        assert!(
            land_rehydrate(&mut app, &mut feeds, id, vec![], 5),
            "taking the stale notice down is itself a change worth a redraw"
        );
        assert_eq!(
            app.notice(),
            None,
            "a round with nowhere to land must not leave the status row claiming one is still running"
        );
    }

    /// A round that never answers — a panic, or a runtime going away —
    /// would otherwise strand its ids in `Feeds::hydrating` for the session,
    /// and a switch to one would wait on a round that ended. The guard
    /// answers for every id it did not reach, which the loop's error arm
    /// clears.
    #[test]
    fn a_dropped_hydrate_round_answers_every_id_it_did_not_reach() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let (tx, mut rx) = mpsc::channel::<Hydrated>(8);
        drop(HydrateGuard { remaining: vec![a, b], tx, wrap: fresh_failed });

        let mut answered = Vec::new();
        while let Ok((context_id, hydrated)) = rx.try_recv() {
            assert!(
                matches!(hydrated, HydrateOutcome::Fresh(Err(_))),
                "an id the round did not reach is an error, not a mirror"
            );
            answered.push(context_id);
        }
        assert_eq!(answered, vec![a, b], "every outstanding id is answered");
    }

    /// An id whose own answer was already sent is not answered twice: the
    /// round takes it off the guard as it goes.
    #[test]
    fn a_hydrate_round_never_answers_an_id_twice() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let (tx, mut rx) = mpsc::channel::<Hydrated>(8);
        let mut guard = HydrateGuard { remaining: vec![a, b], tx, wrap: fresh_failed };
        guard.remaining.retain(|id| *id != a);
        drop(guard);

        let mut answered = Vec::new();
        while let Ok((context_id, _)) = rx.try_recv() {
            answered.push(context_id);
        }
        assert_eq!(answered, vec![b]);
    }

    /// A round takes on every hot context that is missing, not one of them:
    /// two seats promoted between rounds both go resident on the next
    /// round rather than over two of them (`docs/tui.md`, "The buffer").
    #[test]
    fn one_round_hydrates_every_missing_hot_context() {
        let (seen, a, b) = (ContextId::new(), ContextId::new(), ContextId::new());
        let mut app = App::new("amy");
        watched(&mut app, seen);
        app.switch_to(seen);
        app.seats = vec![
            kaijutsu_client::RankedSeat { context_id: a, band: kaijutsu_viz::layout::Band::Active },
            kaijutsu_client::RankedSeat { context_id: b, band: kaijutsu_viz::layout::Band::Active },
        ];

        let round = hydrate_round(&app, &std::collections::HashSet::new());
        assert_eq!(round, vec![a, b], "both new seats ride the same round");

        // A context already in flight is never asked for twice: the actor
        // keeps one feed sender per context and the second subscribe would
        // orphan the receiver this client kept.
        let in_flight = std::collections::HashSet::from([a]);
        assert_eq!(hydrate_round(&app, &in_flight), vec![b]);
        assert!(
            hydrate_round(&app, &std::collections::HashSet::from([a, b])).is_empty(),
            "nothing is asked for while the whole round is in flight"
        );
    }

    #[test]
    fn an_unrelated_event_carries_no_turn_liveness_either() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let event = ServerEvent::ContextSwitched { context_id: id };
        assert!(!mark_turn_liveness(&mut app, &event));
    }
}
