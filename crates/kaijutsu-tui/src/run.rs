//! The event loop: crossterm keys, context feeds, kernel events and a redraw
//! tick as `tokio::select!` arms.
//!
//! The loop coalesces: any arm that changes state marks the frame dirty and
//! the tick draws once. A burst of streaming appends therefore costs one
//! redraw, not one per token.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::style::Print;
use crossterm::event::Event;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, SetTitle, disable_raw_mode, enable_raw_mode,
};
use kaijutsu_audio::RefDisposition;
use kaijutsu_client::{ActorHandle, ContextInfo, FeedEvent, ServerEvent};
use parking_lot::Mutex;
use kaijutsu_types::ContextId;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::{App, ContextView};
use crate::asks;
use crate::inflight;
use crate::bridge::KernelBridge;
use crate::cmdline::{self, ColonVerb};
use crate::compose::{Compose, CursorShape};
use crate::completion;
use crate::interrupt::{self, Step as InterruptStep};
use crate::keys::{Intent, Keys};
use crate::picker::{self, Outcome as PickerOutcome};
use crate::refresh::{self, decision_words, short_ask};
use crate::render;

use crossterm::event::KeyEvent;
use kaijutsu_client::{PeerConfig, PeerInvocation};

use crate::copy;
use crate::diff::{self, DiffKey};
use crate::editor::{self, EditorOpen, ScreenMode};

/// How often the rank, the cache figures and the pending-ask count are
/// refreshed. Slow on purpose: none of them is an interaction-rate fact.
const REFRESH: Duration = Duration::from_secs(5);

/// The redraw tick. Fast enough that a stream looks live, slow enough that a
/// terminal over ssh is not the bottleneck.
const TICK: Duration = Duration::from_millis(80);

/// How long the key reader blocks on one poll before releasing the terminal
/// lock. It bounds how long a redraw waits for the lock, so it is short.
const KEY_POLL: Duration = Duration::from_millis(20);

/// `KAIJUTSU_TUI_PROBE_PANIC`: unset, or `frame` to panic inside the frame
/// — where a synchronized update is open and the restore path has to end it
/// — or `task` to panic inside a `spawn_local` task the loop never joins,
/// the way `Feeds::pump` or a hydrate round would — or any other value to
/// panic on the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbePanic {
    Off,
    OnKey,
    InFrame,
}
    InTask,

impl ProbePanic {
    fn from_env() -> Self {
        match std::env::var("KAIJUTSU_TUI_PROBE_PANIC").ok().as_deref() {
            None => Self::Off,
            Some("frame") => Self::InFrame,
            Some(_) => Self::OnKey,
            Some("task") => Self::InTask,
        }
    }
}

/// One feed delivery, tagged with the context it belongs to — several
/// The event loop's own thread, recorded once when the panic hook is
/// installed (`run`). Only a panic on this thread that is also outside any
/// tokio task unwinds `LocalSet::run_until`'s root future — the hook's own
/// [`panic_unwinds_the_loop`] check.
static LOOP_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// A panic the hook saw but did not unwind the loop for — inside a
/// `spawn_local` task nobody joins (`Feeds::pump`, a hydrate round), or on a
/// background thread (the key reader, the peer thread). Recorded here so
/// `event_loop` can notice it and end the loop with an `Err` instead of
/// leaving a panicked task's silence to draw over a screen the hook left
/// alone. `event_loop` takes it, so a second unnoticed panic before the
/// first is drained overwrites rather than queues — the loop is about to end
/// either way.
static TASK_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Whether a panic on `panicking_thread` unwinds `run_until`'s root future —
/// the only case [`restore_terminal`] may run from inside the hook.
/// `loop_thread` is [`LOOP_THREAD`] as the hook read it; `task_id` is
/// `tokio::task::try_id()` on the panicking thread.
///
/// A `spawn_local` task shares the loop's own OS thread (`main.rs` drives
/// every task through one `LocalSet::run_until` on the runtime's main
/// thread) but panics without reaching the root future itself —
/// `tokio::task::try_id()` is `Some` inside one and `None` at the root — so
/// a thread-id check alone cannot tell a task panic from the root's own. A
/// panic on any other thread does not reach the root future either.
fn panic_unwinds_the_loop(
    panicking_thread: std::thread::ThreadId,
    loop_thread: Option<std::thread::ThreadId>,
    task_id: Option<tokio::task::Id>,
) -> bool {
    Some(panicking_thread) == loop_thread && task_id.is_none()
}

/// contexts are watched at once and they share one loop.
type TaggedFeed = (ContextId, FeedEvent);

/// The context feeds this client is pumping: one forwarder task per watched
/// context, and the one channel they all forward into.
///
/// Releasing a context aborts its forwarder here and tells the actor to stop
/// re-issuing `subscribeContext` for it (`ActorHandle::unsubscribe_context`).
/// That ends the actor's pump, which drops the observer capability, which is
/// what actually ends the feed on the wire (`kaijutsu_client::rpc`). Held by
/// the loop rather than by [`App`], which stays free of tasks and
/// connections.
struct Feeds {
    tx: mpsc::Sender<TaggedFeed>,
    tasks: std::collections::HashMap<ContextId, tokio::task::JoinHandle<()>>,
    /// Where a hydrated context is delivered — cloned into each round's
    /// task, received by the loop's own arm.
    hydrated_tx: mpsc::Sender<Hydrated>,
    /// The contexts a background hydrate is in flight for. The round owns
    /// The actor whose `context_feeds` map a release must also clear — see
    /// [`Self::stop`].
    actor: ActorHandle,
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
    hydrating: std::collections::HashSet<ContextId>,
}

impl Feeds {
    fn new(tx: mpsc::Sender<TaggedFeed>, hydrated_tx: mpsc::Sender<Hydrated>, actor: ActorHandle) -> Self {
        Self {
            tx,
            tasks: std::collections::HashMap::new(),
            hydrated_tx,
            hydrating: std::collections::HashSet::new(),
        }
            actor,
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
    fn stop(&mut self, context_id: ContextId) {
    ///
    /// Non-blocking: `unsubscribe_context` only touches the actor's own
    /// in-memory map, no wire round trip, so this stays safe to call from
    /// inside `event_loop` without awaiting anything.
        if let Some(task) = self.tasks.remove(&context_id) {
            task.abort();
        }
    }
        self.actor.unsubscribe_context(context_id);
}

/// crossterm's internal event reader is one shared resource, and a blocking
/// read holds it. A reader mid-poll while raw mode goes away reads a cooked
/// terminal, and the keys typed at the shell prompt are lost. One mutex
/// arbitrates: the key reader takes it for the length of one [`KEY_POLL`],
/// and every terminal operation takes it too.
type TermLock = Arc<Mutex<()>>;

/// The loop's wake sources, bundled so the event loop takes one handle
/// instead of a parameter per channel.
struct Wires {
    term_lock: TermLock,
    /// What `F12` does, so the terminal-fit probes can see what the panic
    /// hook leaves behind.
    probe_panic: ProbePanic,
    key_rx: mpsc::Receiver<Event>,
    feed_rx: mpsc::Receiver<TaggedFeed>,
    /// Kept alongside the receiver: a context switch watches a new context,
    /// which needs a forwarder task of its own, and a release stops one.
    feeds: Feeds,
    /// Hot contexts the background round has hydrated, one delivery each as
    /// it lands ([`start_hydrate`]).
    hydrated_rx: mpsc::Receiver<Hydrated>,
    server_events: tokio::sync::broadcast::Receiver<ServerEvent>,
    /// `open_editor` peer signals — the only notification that a vi session
    /// opened. See [`attach_editor_peer`].
    editor_opens: mpsc::Receiver<EditorOpen>,
}

/// Run the client until it quits, restoring the terminal on the way out.
pub async fn run(
    bridge: KernelBridge,
    start: ContextInfo,
    identity: String,
    diff: Option<Vec<String>>,
) -> Result<()> {
    let mut app = App::new(identity);
    app.set_contexts(bridge.list_contexts().await?);
    // Best-effort: a kernel that cannot answer this yet just means slash
    // completion offers nothing until a later poll fills it in, never a
    // failure to start.
    app.kj_catalog = bridge.kj_command_catalog(start.id).await.unwrap_or_default();

    // Subscribed before the first hydrate, not inside the loop: the actor's
    // kernel-wide event bus drops an event it has no receiver for, and the
    // hydrate itself produces a burst of them.
    let server_events = bridge.actor().subscribe_events();

    let (feed_tx, feed_rx) = mpsc::channel::<TaggedFeed>(256);
    // Comfortably more than a round has answers to give — the ACTIVE ring
    // is kernel-capped at ten seats — so a round never blocks on the loop
    // reading them, and [`HydrateGuard`]'s `try_send` always has a slot
    // while the loop is still draining.
    let (hydrated_tx, hydrated_rx) = mpsc::channel::<Hydrated>(64);
    let mut feeds = Feeds::new(feed_tx, hydrated_tx, bridge.actor().clone());
    watch_context(&bridge, &mut app, start.id, &mut feeds).await?;
    app.switch_to(start.id);

    // Before raw mode: a `--diff` that cannot run says so on a plain line
    // rather than under a viewport.
    if let Some(args) = &diff
        && let Err(e) = open_kj_diff(&bridge, &mut app, start.id, args).await
    {
        eprintln!("kaijutsu-tui: {e}");
    }

    // Attached before the terminal is touched: a `vi` that runs during startup
    // must find a peer to signal, and a failure here is a plain message rather
    // than one written over a raw-mode screen.
    let (open_tx, open_rx) = mpsc::channel::<EditorOpen>(8);
    if let Err(e) = attach_editor_peer(&bridge, open_tx).await {
        tracing::warn!(error = %e, "no open_editor peer; `vi` will not open a screen here");
    }

    // The draft block is per (context, principal), so compose needs to know
    // which principal is ours before it can pick its own draft out of the
    // mirror. A draft this or another client left behind is picked up rather
    // than overwritten.
    let identity = bridge.identity().await?;
    app.identity = identity.display_name;
    app.principal = Some(identity.principal_id);
    app.compose = Compose::over(&bridge.read_input(start.id).await.unwrap_or_default());

    // A panic on this thread, outside any task, past this point unwinds
    // through the screen without reaching `leave_terminal`. The hook
    // restores the terminal first, so the panic message prints on a cooked
    // main screen and the shell that follows reads keys. No terminal lock
    // here: the panicking thread may hold it.
    //
    // A panic that does *not* unwind the loop this way — inside a
    // `spawn_local` task nobody joins, or on a background thread — must not
    // restore here: the loop is still drawing, and tearing the screen out
    // from under it leaves the client painting onto the shell's own screen
    // (`docs/issues.md` before this fix; `panic_unwinds_the_loop`). It goes
    // into `TASK_PANIC` instead, for `event_loop` to notice and end the loop
    // with, which reaches `leave_terminal` the normal way.
    *LOOP_THREAD.lock() = Some(std::thread::current().id());
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if panic_unwinds_the_loop(std::thread::current().id(), *LOOP_THREAD.lock(), tokio::task::try_id()) {
            restore_terminal();
        } else {
            *TASK_PANIC.lock() = Some(info.to_string());
        }
        default_panic(info);
    }));

    // The screen is taken after the connection is up, so a failure to
    // connect is a plain line on the shell's own screen.
    let mut terminal = enter_terminal().context("take the alternate screen")?;
    let term_lock: TermLock = Arc::new(Mutex::new(()));
    let (key_tx, key_rx) = mpsc::channel::<Event>(64);
    // Stopped on the way out, before raw mode goes, so keys typed at the
    // shell prompt while the connection tears down reach the shell.
    let reader_stop = spawn_key_reader(term_lock.clone(), key_tx);

    let mut wires = Wires {
        term_lock: term_lock.clone(),
        probe_panic: ProbePanic::from_env(),
        key_rx,
        feed_rx,
        feeds,
        hydrated_rx,
        server_events,
        editor_opens: open_rx,
    };
    let result = event_loop(&bridge, &mut app, &mut terminal, &mut wires).await;
    leave_terminal(&mut terminal, &term_lock, &reader_stop);
    result
}

/// Read keys on a blocking thread, one bounded poll at a time.
///
/// A dedicated thread rather than `crossterm::event::EventStream`: the stream
/// polls with no timeout, which holds crossterm's internal reader forever and
/// leaves no window for the exit path to stop it.
fn spawn_key_reader(term_lock: TermLock, tx: mpsc::Sender<Event>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        loop {
            // Checked under the lock: `leave_terminal` sets the flag and
            // then takes the lock, so a reader that was waiting for it sees
            // the flag before it can poll a cooked terminal.
            let ready = {
                let _guard = term_lock.lock();
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                crossterm::event::poll(KEY_POLL).unwrap_or(false)
            };
            if ready {
                let event = {
                    let _guard = term_lock.lock();
                    crossterm::event::read()
                };
                match event {
                    Ok(event) => {
                        if tx.blocking_send(event).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "terminal input ended");
                        return;
                    }
                }
            }
            // Unlocked, so a redraw waiting on the lock always gets a window.
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    stop
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
async fn watch_context(
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
fn adopt(
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
enum HydrateOutcome {
    Fresh(Result<(kaijutsu_client::ContextMirror, mpsc::Receiver<FeedEvent>)>),
    Rebuilt(Result<(Vec<kaijutsu_types::BlockSnapshot>, u64)>),
}

/// One hydrate or rehydrate answered in the background: the id asked for,
/// and what the kernel answered.
type Hydrated = (ContextId, HydrateOutcome);

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
fn start_hydrate(bridge: &KernelBridge, app: &App, feeds: &mut Feeds) {
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

/// Answers the loop for every id a round did not reach.
///
/// A round that panics, or is dropped because the runtime is going away,
/// would otherwise leave its ids in [`Feeds::hydrating`] for the life of the
/// session, and those contexts could never go resident again — a switch to
/// one would wait forever on a round that ended. The guard sends an `Err`
/// for each id still outstanding as it drops, which the loop's own error arm
/// clears.
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

struct HydrateGuard {
    /// The round's ids, each removed once its own answer has been sent.
    remaining: Vec<ContextId>,
    tx: mpsc::Sender<Hydrated>,
}

impl Drop for HydrateGuard {
    fn drop(&mut self) {
        for context_id in self.remaining.drain(..) {
            // `try_send`, because a `Drop` cannot await. The channel holds
            // more slots than a round has answers to give, so a full channel
            // means the loop has stopped draining it — it is shutting down,
    /// How a stranded id's error is wrapped for the landing arm:
    /// [`fresh_failed`] for [`start_hydrate`]'s round, [`rebuilt_failed`] for
    /// [`start_rehydrate`]'s single id — the two ask the kernel for
    /// different things and the landing arm's `Err` handling differs
    /// accordingly.
    wrap: fn(anyhow::Error) -> HydrateOutcome,
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

/// Drop every watched context that has left the hot set, and stop its feed.
/// No kernel call, so a key path may run it.
///
/// The forwarder goes first and the view second: a delivery that reached the
/// loop before the abort finds no view and is discarded ([`apply_feed`]), and
/// nothing can start forwarding into a view that is on its way out.
fn release_cold(app: &mut App, feeds: &mut Feeds) {
    for context_id in app.cold_contexts() {
fn fresh_failed(e: anyhow::Error) -> HydrateOutcome {
    HydrateOutcome::Fresh(Err(e))
}

fn rebuilt_failed(e: anyhow::Error) -> HydrateOutcome {
    HydrateOutcome::Rebuilt(Err(e))
}

        feeds.stop(context_id);
        app.release(context_id);
        tracing::debug!(context = %context_id.short(), "released a cold context");
    }
}

async fn event_loop(
    bridge: &KernelBridge,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    wires: &mut Wires,
) -> Result<()> {
    let mut keys = Keys::new();
    let mut interrupt_ladder = interrupt::Ladder::new();
    let mut status = bridge.actor().watch_status();
    // Ask polling is driven by the ledger's own change stream, not by the
    // refresh timer (`refresh::Request::poll_asks`).
    let mut ledger_events = bridge.actor().subscribe_ledger_events();
    let mut ledger_open = true;
    let mut seen_asks = std::collections::HashSet::new();
    let mut poll_asks = true;
    // The session's first ledger poll is a baseline: it reports every ask
    // already pending as new, including asks raised before this client
    // attached. Taken by the round that polls; `round_baseline` carries it
    // to that round's answer (`asks::NotifySeen::baseline`).
    let mut first_poll = true;
    let mut round_baseline = false;
    let mut stop_signals = StopSignals::listen().context("listen for SIGTERM and SIGHUP")?;
    let mut refresh = tokio::time::interval(REFRESH);
    // The round in flight, if one is. Its result lands through the join arm
    // below; the loop itself never awaits the kernel for a refresh.
    let mut refresh_task: Option<tokio::task::JoinHandle<refresh::Refreshed>> = None;
    // A round was asked for out of turn (`App::roster_changed`). Stays set
    // while a round is in flight, because that round started before the
    // change and its answer is already stale.
    let mut refresh_wanted = false;
    let mut tick = tokio::time::interval(TICK);
    let mut dirty = true;
    let mut last_strip_frame = Instant::now();
    // The cursor shape last sent to the terminal; `None` until the first
    // frame and again after a suspend, since the host shell may have set
    // its own.
    let mut cursor_shape: Option<CursorShape> = None;
    // The window title last sent; `None` until the first context is on
    // screen and again after a suspend, since the host shell titled the
    // window while it had it.
    let mut title: Option<String> = None;
    // The screen mode the last frame drew, so crossing between the
    // conversation and a full-screen surface can say the cursor shape again.
    let mut was_full_screen = false;
    // `KAIJUTSU_TUI_PROBE_PANIC=frame` armed by `F12`: the next frame panics
    // with a synchronized update open.
    let mut panic_in_frame = false;
    let mut beat_wake: Option<Instant> = None;
    let mut beat_tempo_bps: f64 = 0.0;

    app.connection = Some(bridge.actor().current_status());

    while !app.quit {
        tokio::select! {
            Some(event) = wires.key_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        dirty = true;
                        // Input only reaches a focused terminal, whatever
                        // the last focus report said (`App::saw_input`).
                        app.saw_input();
        // A task the loop never joins panicked (`TASK_PANIC`, set by the
        // hook installed in `run`); the hook did not restore the terminal
        // for it, so this is the loop's own chance to end normally and
        // reach `leave_terminal` — checked once an iteration, which bounds
        // the delay by whatever else is ready to wake the loop and, absent
        // that, by `TICK`.
        if let Some(message) = TASK_PANIC.lock().take() {
            return Err(anyhow::anyhow!(message));
        }
                        if key.code == crossterm::event::KeyCode::F(12) {
                            match wires.probe_panic {
                                ProbePanic::OnKey => panic!("KAIJUTSU_TUI_PROBE_PANIC: F12 pressed"),
                                ProbePanic::InFrame => panic_in_frame = true,
                                ProbePanic::Off => {}
                            }
                        }
                        let presentation = (app.current, app.ask_card.as_ref().map(|card| card.request_id.clone()));
                        if app.picker.is_some() {
                            handle_picker_key(bridge, app, key, &mut wires.feeds).await?;
                        } else {
                            let width = terminal.size()?.width;
                                // Never joined, like `Feeds::pump`'s forwarder
                                // or a hydrate round: the loop must notice
                                // this panic through `TASK_PANIC`, not by
                                // unwinding.
                                ProbePanic::InTask => {
                                    tokio::task::spawn_local(async {
                                        panic!("KAIJUTSU_TUI_PROBE_PANIC: F12 pressed inside a task");
                                    });
                                }
                            if act(bridge, app, &mut keys, &mut interrupt_ladder, key, &mut wires.feeds, &wires.term_lock, width)
                                .await?
                                == Acted::Suspend
                            {
                                suspend(terminal, &wires.term_lock)?;
                                cursor_shape = None;
                                title = None;
                                // `fg` put this terminal back in front of
                                // the player; a report may never say so.
                                app.saw_input();
                            }
                        }
                        if presentation != (app.current, app.ask_card.as_ref().map(|card| card.request_id.clone())) {
                            poll_asks = true;
                        }
                        if std::mem::take(&mut app.roster_changed) {
                            refresh_wanted = true;
                            refresh.reset_immediately();
                        }
                    }
                    Event::Paste(text) => {
                        dirty = true;
                        app.saw_input();
                        paste_text(bridge, app, text).await;
                    }
                    // The frame is rebuilt at the new size on the next tick:
                    // the transcript re-wraps and the band follows.
                    Event::Resize(..) => dirty = true,
                    // Focus reporting (DECSET 1004). Coming back draws a
                    // frame, so what is on screen is current rather than
                    // whatever was there when focus left; leaving disarms
                    // the beat timer, and `render::strip_animating` stops
                    // the spinner (`docs/tui.md`, "What owning the screen
                    // lets us use").
                    Event::FocusGained => {
                        app.focused = true;
                        rearm_beat_wake(app, &mut beat_wake, &mut beat_tempo_bps);
                        dirty = true;
                    }
                    // The timer is disarmed rather than left armed and
                    // ignored: a missed onset is missed, never replayed, and
                    // `rearm_beat_wake` above re-anchors on the live phasor
                    // (`docs/midi.md`, "The one timebase").
                    Event::FocusLost => {
                        app.focused = false;
                        beat_wake = None;
                    }
                    _ => {}
                }
            }
            Some((context_id, event)) = wires.feed_rx.recv() => {
                dirty = true;
                apply_feed(bridge, app, context_id, event, &mut wires.feeds);
            }
            received = wires.server_events.recv() => {
                let event = match received {
                    Ok(event) => event,
                    // A lag drops events unseen, and among them may be the
                    // `TurnCompleted` that would have cleared a turn flag.
                    // The client no longer knows what is running; say so and
                    // forget rather than carry a flag nothing will clear.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        app.forget_turn_liveness();
                        app.note(format!("{n} kernel events lost; turn liveness reset"));
                        dirty = true;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => continue,
                };
                if mark_activity(app, &event) {
                    dirty = true;
                }
                if mark_turn_liveness(app, &event) {
                    dirty = true;
                }
                if observe_editor_event(bridge, app, &event).await {
                    dirty = true;
                }
                // The picker's tail buffer (`docs/tui.md`, "The picker") is
                // fed ungated, like the app's `ContextTails` — every context,
                // not just the ones watched.
                if app.tails.observe(&event, kaijutsu_types::now_millis()) {
                    dirty = true;
                }
                if let ServerEvent::BeatSync { context_id, beat_ref } = &event {
                    observe_beat_sync(app, *context_id, *beat_ref, bridge.actor().kernel_now_ns());
                    dirty = true;
                }
            }
            Some(open) = wires.editor_opens.recv() => {
                editor::enter_editor(app, open);
                dirty = true;
            }
            Ok(()) = status.changed() => {
                let connection = status.borrow().clone();
                editor::leave_on_disconnect(app, &connection);
                // Leaving `Connected` means the event stream is broken: any
                // turn end that happens before the next subscribe is never
                // seen, so what this client believed is forgotten with it.
                if !matches!(connection, kaijutsu_client::ConnectionStatus::Connected { .. })
                    && app.forget_turn_liveness()
                {
                    app.note("kernel connection lost; turn liveness reset");
                }
                app.connection = Some(connection);
                if matches!(app.connection, Some(kaijutsu_client::ConnectionStatus::Connected { .. })) {
                    poll_asks = true;
                }
                dirty = true;
            }
            received = ledger_events.recv(), if ledger_open => {
                match received {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => poll_asks = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => ledger_open = false,
                }
            }
            signal = stop_signals.recv() => {
                // The loop ends as `:q` ends it, so the terminal is
                // restored on the way out instead of dying in raw mode.
                tracing::info!(signal, "stopping on signal");
                app.quit = true;
            }
            _ = refresh.tick() => {
                // Single-flight: a round still running is left to finish,
                // and the next tick starts the next one.
                if refresh_task.is_none() {
                    refresh_wanted = false;
                    let poll = poll_asks && app.current.is_some();
                    if poll {
                        poll_asks = false;
                    }
                    round_baseline = poll && std::mem::take(&mut first_poll);
                    let request = refresh::Request {
                        current: app.current,
                        poll_asks: poll,
                        seen_asks: seen_asks.clone(),
                        card: app.ask_card.as_ref().map(|card| (card.request_id.clone(), card.context_id)),
                    };
                    refresh_task = Some(tokio::spawn(refresh::fetch(bridge.clone(), request)));
                }
            }
            joined = async { refresh_task.as_mut().expect("guarded by is_some").await }, if refresh_task.is_some() => {
                refresh_task = None;
                let refreshed = joined.context("background refresh round")?;
                let presentation = (app.current, app.ask_card.as_ref().map(|card| card.request_id.clone()));
                // Read before the fold, which consumes the round: the ask
                // whose arrival may be worth a desktop notification.
                let raised_in = refreshed
                    .asks
                    .as_ref()
                    .and_then(|poll| poll.new_asks.first())
                    .map(|ask| ask.info.context_id);
                let baseline = std::mem::take(&mut round_baseline);
                refresh::apply(app, refreshed, &mut seen_asks);
                // An ask that landed while nobody was looking says so to the
                // desktop; one that landed in front of the player is already
                // the card on screen (`docs/tui.md`, "Asks"). Decided after
                // the fold, so `current` is the seat a switch may have moved
                // to while the round was in flight and the label is the one
                // this round listed.
                let seen = asks::NotifySeen { focused: app.focused, baseline, raised_in, current: app.current };
                if let Some(context_id) = asks::notify_target(seen)
                    && let Some(notification) = asks::ask_notification(app.focused, &app.label_for(context_id))
                {
                    let _guard = wires.term_lock.lock();
                    crossterm::execute!(io::stdout(), Print(notification))
                        .context("raise a desktop notification")?;
                }
                // The rank the hot set reads was just recomputed, so this is
                // where a promote makes a context resident and a demote lets
                // one go. Releasing is local; the hydrate rides its own task.
                release_cold(app, &mut wires.feeds);
                start_hydrate(bridge, app, &mut wires.feeds);
                if presentation != (app.current, app.ask_card.as_ref().map(|card| card.request_id.clone())) {
                    poll_asks = true;
                }
                if refresh_wanted {
                    refresh.reset_immediately();
                }
                rearm_beat_wake(app, &mut beat_wake, &mut beat_tempo_bps);
                dirty = true;
            }
            Some((context_id, hydrated)) = wires.hydrated_rx.recv() => {
                wires.feeds.hydrating.remove(&context_id);
                match hydrated {
                    // The hot set can move while a hydrate is in flight, and
                    // a switch can have hydrated the same context first. A
                    // mirror nobody wants is dropped, and the feed receiver
                    // with it, which ends the subscription.
                    HydrateOutcome::Fresh(Ok((mirror, rx)))
                        if app.hot_set().contains(&context_id)
                            && !app.views.contains_key(&context_id) =>
                    {
                        adopt(app, &mut wires.feeds, context_id, mirror, rx);
                        // The switch that waited for this round said so on
                        // the status row; the transcript answers now. Only
                        // that exact line comes down, so a notice posted
                        // since stands.
                        if app.notice() == Some(hydrating_notice(app, context_id).as_str()) {
                            app.clear_notice();
                        }
                        dirty = true;
                    }
                    HydrateOutcome::Fresh(Ok(_)) => {}
                    // Not resident, so the next round asks again. A context
                    // on screen with no view renders nothing at all, so it
                    // says so rather than look like an empty conversation.
                    HydrateOutcome::Fresh(Err(e)) => {
                        tracing::warn!(
                            context = %context_id.short(),
                            error = %e,
                            "cannot watch a hot context"
                        );
                        if app.current == Some(context_id) {
                            app.note(format!("cannot watch {}: {e}", app.label_for(context_id)));
                            dirty = true;
                        }
                    }
                }
            }
            // The beat-driven redraw: armed at the playing track's predicted
            // next onset, re-armed at `scheduled + period` inside the arm
            // body — never `actual_wake + period` (`docs/tui.md`, "Timing to
            // music"; `docs/midi.md`, "The one timebase"). The `if` guard
            // skips this arm entirely while nothing is playing, so it never
            // busy-polls a zero sleep.
                    // A rebuilt snapshot lands on the view it belongs to, or
                    // nowhere if the context was released while the round
                    // was in flight — the same drop `apply_delivery` already
                    // gives an ordinary feed delivery with no view to land
                    // on.
                    HydrateOutcome::Rebuilt(Ok((blocks, version))) => {
                        if land_rehydrate(app, &mut wires.feeds, context_id, blocks, version) {
                            dirty = true;
                        }
                    }
                    // The round trip itself failed — never reached a
                    // snapshot at all, so there is nothing to apply. See
                    // `abandon_rehydrate` for why this releases rather than
                    // retries or waits.
                    HydrateOutcome::Rebuilt(Err(e)) => {
                        abandon_rehydrate(app, &mut wires.feeds, context_id, e);
                        dirty = true;
                    }
            _ = tokio::time::sleep(beat_wake.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default()), if beat_wake.is_some() => {
                if let Some(scheduled) = beat_wake {
                    beat_wake = Some(picker::rearm(scheduled, beat_tempo_bps));
                }
                dirty = true;
            }
            _ = tick.tick() => {
                // The strip's spinner and breath: a redraw every phase step
                // while a tool call is running, and none otherwise.
                if !dirty
                    && last_strip_frame.elapsed() >= Duration::from_millis(inflight::PHASE_MILLIS)
                    && render::strip_animating(app)
                {
                    last_strip_frame = Instant::now();
                    dirty = true;
                }
                app.screen_rows = terminal.size()?.height;
                if dirty {
                    dirty = false;
                    draw(terminal, &wires.term_lock, app, keys.armed(), panic_in_frame)?;
                    // A terminal may keep a cursor shape per screen buffer,
                    // and a full-screen surface owns its own, so crossing
                    // either way forgets what was sent.
                    if app.screen.is_full_screen() != was_full_screen {
                        was_full_screen = app.screen.is_full_screen();
                        cursor_shape = None;
                    }
                    let _guard = wires.term_lock.lock();
                    set_cursor_shape(&mut cursor_shape, wanted_cursor_shape(app))?;
                    set_title(&mut title, wanted_title(app))?;
                }
            }
        }
    }
    Ok(())
}

/// The `Thinking` blocks of `context_id` that are still streaming — what a
/// rehydrated mirror offers the thinking pane's latch in place of the
/// changed blocks a delivery names.
fn streaming_thinking(app: &App, context_id: ContextId) -> Vec<kaijutsu_types::BlockId> {
    let Some(view) = app.views.get(&context_id) else {
        return Vec::new();
    };
    view.mirror
        .blocks()
        .iter()
        .filter(|b| {
            b.kind == kaijutsu_types::BlockKind::Thinking && !render::is_settled(b)
        })
        .map(|b| b.id)
        .collect()
}

/// Fold one `ServerEvent::BeatSync` into `app.beats`, the same
/// fold/touch/drop routing as the app's `time_well::live::ingest_live_events`
/// (`docs/tui.md`, "TRACKS + beat"). `now_epoch_ns` is the kernel-domain
/// wallclock (`ActorHandle::kernel_now_ns`), so the age ladder runs on the
/// one timebase (`docs/midi.md`, "The one timebase").
fn observe_beat_sync(
    app: &mut App,
    context_id: ContextId,
    beat_ref: kaijutsu_audio::BeatRef,
    now_epoch_ns: u64,
) {
    let now_inst = Instant::now();
    match beat_ref.disposition(now_inst, now_epoch_ns) {
        RefDisposition::Fold(at) => {
            app.beats.observe(context_id, beat_ref, at, now_inst);
        }
        RefDisposition::Touch | RefDisposition::Drop => {
            app.beats.touch(&context_id, now_inst);
        }
    }
}

/// Whether the beat timer should be armed at all: a track is playing and
/// someone is looking. Every [`rearm_beat_wake`] goes through it, so the
/// refresh round cannot re-arm what a `FocusLost` disarmed
/// (`docs/tui.md`, "What owning the screen lets us use").
fn beat_wake_armed(app: &App) -> bool {
    app.focused && app.playing_track().is_some()
}

/// Re-arm the beat timer from the playing track's live phasor position and
/// its last-polled tempo. `None` while nothing is playing or its phasor
/// hasn't anchored yet — the timer arm's `if beat_wake.is_some()` guard then
/// simply stays off until the next refresh finds one.
fn rearm_beat_wake(app: &App, beat_wake: &mut Option<Instant>, beat_tempo_bps: &mut f64) {
    if !beat_wake_armed(app) {
        *beat_wake = None;
        return;
    }
    let Some(track) = app.playing_track() else {
        *beat_wake = None;
        return;
    };
    let now = Instant::now();
    let Some(position) = app.beats.beat_position(&track.score_context_id, now) else {
        *beat_wake = None;
        return;
    };
    *beat_tempo_bps = track.tempo_bps();
    *beat_wake = Some(picker::next_onset(position, *beat_tempo_bps, now));
}

/// Route one key to the open picker, then act on its [`PickerOutcome`].
/// A placement verb that ran marks the roster changed (`App::roster_changed`)
/// so the loop starts a refresh round now rather than on the next tick;
/// the round's `apply` rebuilds the open picker.
async fn handle_picker_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
    feeds: &mut Feeds,
) -> Result<()> {
    let Some(picker) = app.picker.as_mut() else {
        return Ok(());
    };
    match picker.handle_key(key) {
        PickerOutcome::None => {}
        PickerOutcome::Dismiss => app.picker = None,
        PickerOutcome::Switch(id) => {
            // The picker comes down first, so the switch's own notice is
            // the one on screen and the band shrinks on the same frame.
            app.picker = None;
            switch_seat(bridge, app, id, feeds).await;
        }
        PickerOutcome::Placement { context_id, argv } => match bridge.execute_kj(context_id, argv).await {
            Ok(result) if result.latch.is_some() => {
                let message = result.latch.map(|l| l.message).unwrap_or_default();
                app.note(message);
            }
            Ok(result) => {
                app.note(result.stdout.lines().next().unwrap_or("done").to_string());
                app.roster_changed = true;
            }
            Err(e) => app.note(format!("placement failed: {e}")),
        },
    }
    Ok(())
}

/// One frame of the owned screen, inside one synchronized update.
///
/// `?2026` brackets the frame so a terminal that supports it shows the whole
/// redraw at once; one that does not ignores the two sequences.
fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    app: &mut App,
    armed: bool,
    probe_panic: bool,
) -> Result<()> {
    // The status line's live pulse dot — the one place this crate samples
    // the phasor's envelope against a real clock; `App::track_figure` only
    // projects the value stamped here.
    app.track_pulse = app
        .playing_track()
        .is_some_and(|t| app.beats.envelope(&t.score_context_id, Instant::now()) > 0.5);

    let _guard = term_lock.lock();
    crossterm::execute!(io::stdout(), BeginSynchronizedUpdate).context("begin a frame")?;
    let drawn = draw_frame(terminal, app, armed, probe_panic);
    // The update is ended whatever the frame did: a terminal left inside a
    // synchronized update shows nothing at all. A panic unwinds past this
    // one, which is why `restore_terminal` ends it too.
    let ended = crossterm::execute!(io::stdout(), EndSynchronizedUpdate).context("end a frame");
    // The frame's own error is the one that says what went wrong.
    drawn?;
    ended
}

/// The frame itself: a full-screen surface when one has the screen, the
/// conversation otherwise.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    armed: bool,
    probe_panic: bool,
) -> Result<()> {
    assert!(!probe_panic, "KAIJUTSU_TUI_PROBE_PANIC: panicking inside a frame");
    let size = terminal.size()?;
    let palette = app.palette;
    match &mut app.screen {
        ScreenMode::Conversation => {
            render::draw_screen(terminal, app, kaijutsu_types::now_millis(), armed)?;
        }
        ScreenMode::Editor(screen) => {
            let frame = editor::editor_frame(screen, size.width, size.height, &palette);
            render::draw_surface(terminal, frame.lines, Some(frame.cursor))?;
        }
        ScreenMode::Diff(screen) => {
            let lines = screen.frame(size.height, &palette);
            render::draw_surface(terminal, lines, None)?;
        }
    }
    Ok(())
}

/// The cursor shape a frame of `app` wants: the draft's mode in the
/// conversation, the buffer's mode in the editor, and a block on the screens
/// that read rather than type (`docs/tui.md`, "Compose").
fn wanted_cursor_shape(app: &App) -> CursorShape {
    match &app.screen {
        ScreenMode::Conversation => app.compose.cursor_shape(),
        ScreenMode::Editor(screen) => CursorShape::for_mode(screen.state.mode.as_deref()),
        ScreenMode::Diff(_) => CursorShape::Block,
    }
}

/// The window title a frame of `app` wants: the context on screen, named
/// the way the status line names it (`docs/tui.md`, "What owning the screen
/// lets us use"). `None` before a context is attached — the shell's own
/// title stands until there is something to say.
fn wanted_title(app: &App) -> Option<String> {
    app.current.map(|id| format!("{} — kaijutsu", app.label_for(id)))
}

/// The title to write, and `sent` updated — `None` when the terminal
/// already carries it, or when there is nothing to name. Pure, so the rule
/// is tested without a terminal; [`set_title`] is the write.
fn title_to_send(sent: &mut Option<String>, want: Option<String>) -> Option<String> {
    let want = want?;
    if sent.as_deref() == Some(want.as_str()) {
        return None;
    }
    *sent = Some(want.clone());
    Some(want)
}

/// Set the window title when it differs from what was last sent (OSC 0).
/// The title the shell had is on xterm's title stack, pushed when the
/// screen was taken and popped by [`restore_terminal`].
fn set_title(sent: &mut Option<String>, want: Option<String>) -> Result<()> {
    let Some(title) = title_to_send(sent, want) else {
        return Ok(());
    };
    crossterm::execute!(io::stdout(), SetTitle(title)).context("set the window title")
}

/// Send `want` to the terminal when it differs from what was last sent —
/// vim's `t_SI`/`t_EI`, as DECSCUSR. Steady shapes: blink is the terminal's
/// own preference and it keeps it for the default shape restored on exit.
fn set_cursor_shape(sent: &mut Option<CursorShape>, want: CursorShape) -> Result<()> {
    if *sent == Some(want) {
        return Ok(());
    }
    let style = match want {
        CursorShape::Block => SetCursorStyle::SteadyBlock,
        CursorShape::Bar => SetCursorStyle::SteadyBar,
        CursorShape::Underline => SetCursorStyle::SteadyUnderScore,
    };
    crossterm::execute!(io::stdout(), style).context("set the cursor shape")?;
    *sent = Some(want);
    Ok(())
}

/// Whether a key asked the loop to hand the terminal back to the host shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acted {
    Continue,
    Suspend,
}

/// Act on one key.
async fn act(
    bridge: &KernelBridge,
    app: &mut App,
    keys: &mut Keys,
    interrupt_ladder: &mut interrupt::Ladder,
    key: crossterm::event::KeyEvent,
    feeds: &mut Feeds,
    term_lock: &TermLock,
    width: u16,
) -> Result<Acted> {
    // The editor is the sanctioned raw key reader (`docs/input.md`): while a
    // vi surface has the screen every key belongs to it, so the `Ctrl+A`
    // prefix and the `Ctrl+C` double-tap are bypassed here rather than being
    // taught to stand aside.
    if editor::route_key(app) == editor::KeyRoute::FullScreen {
        act_full_screen(bridge, app, key).await?;
        return Ok(Acted::Continue);
    }
    // The ledger view captures every key while open — its j/k/Esc keys are
    // never compose text or a `Ctrl+A` chord (`docs/tui.md`, "The ledger").
    if app.ledger_view.is_some() {
        handle_ledger_key(bridge, app, key).await;
        return Ok(Acted::Continue);
    }
    // Off the live tail the transcript owns copy mode's keys, and every
    // other key snaps it back and is then handled here as if it had been
    // typed at the tail (`docs/tui.md`, "Scrolling is copy mode"). An ask
    // card keeps its own a/A/d/v/Esc: the card is the more urgent surface,
    // and the scrolled view waits under it. The `Ctrl+A` prefix is not the
    // transcript's either ([`Keys::claims`]): the chords that switch seats
    // leave the view where the reader put it, which is what makes a context
    // left scrolled still scrolled on return.
    if app.scrolled().is_some() && app.ask_card.is_none() && !keys.claims(&key) {
        if scrolled_key(app, &key, term_lock, width) == ScrolledKey::Handled {
            return Ok(Acted::Continue);
        }
        app.snap_transcript();
    }

    // The ask card owns a/A/d/v/Esc and holds compose text; a `Ctrl+A`
    // chord, `Ctrl+C` and `Ctrl+Z` act under it as they would under no card
    // (`docs/tui.md`, "Asks").
    let intent = if app.ask_card.is_some() {
        match asks::route_under_card(key, keys) {
            asks::CardRoute::Card(asks::AskCardKey::Decide(decision)) => {
                let card = app.ask_card.take().expect("checked Some above");
                handle_ask_decision(bridge, app, card, decision).await;
                return Ok(Acted::Continue);
            }
            asks::CardRoute::Card(asks::AskCardKey::Aside) => {
                let card = app.ask_card.take().expect("checked Some above");
                app.note(format!("ask {} set aside, still pending (Ctrl+A l)", short_ask(&card.request_id)));
                return Ok(Acted::Continue);
            }
            asks::CardRoute::Held => return Ok(Acted::Continue),
            asks::CardRoute::Chord(intent) => intent,
        }
    } else {
        keys.interpret(key)
    };

    match intent {
        Intent::Ignored | Intent::LegendChanged => {}
        Intent::Interrupt => {
            interrupt_ctrl_c(bridge, app, interrupt_ladder).await;
        }
        Intent::InputKey(key) => match tail_key(app, &key, width) {
            TailKey::Draft => compose_key(bridge, app, key).await?,
            // The tail is already on screen; there is nothing below it.
            TailKey::Nothing => {}
            TailKey::LeaveTail(step) => leave_tail(app, width, step),
        },
        Intent::Suspend => {
            return Ok(Acted::Suspend);
        }
        Intent::SwitchSeat(n) => match app.seat_context(n) {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note(format!("no context on seat {n}")),
        },
        Intent::StepSeat(step) => match app.seat_neighbor(step) {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note("no seats"),
        },
        Intent::Paste => match (app.current, app.paste_buffer.clone()) {
            (Some(ctx), Some(text)) => {
                let ops = app.compose.paste(&text);
                mirror_ops(bridge, app, ctx, &ops).await;
            }
            (None, _) => app.note("no context attached"),
            (_, None) => app.note("paste buffer empty — scroll up, then v and y to fill it"),
        },
        // `switch_seat` re-watches, which is what makes the toggle work
        // after a feed ended and dropped the context's view: a context with
        // no view renders nothing at all.
        Intent::LastContext => match app.previous {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note("no previous context"),
        },
        Intent::OpenDiff => match open_diff(app) {
            Some(screen) => {
                app.screen = ScreenMode::Diff(screen);
                app.clear_notice();
            }
            None => app.note("no diff block in this context"),
        },
        Intent::CopyMode => match app.current {
            // Already off the tail: the chord is where the reader is, not a
            // second entry that would re-anchor them at the live tail.
            Some(_) if !app.following() => app.clear_notice(),
            Some(_) => {
                leave_tail(app, width, Step::Still);
                app.clear_notice();
            }
            None => app.note("no context to read"),
        },
        Intent::NotYet(message) => app.note(message),
        Intent::OpenLedger => open_ledger(bridge, app).await,
        Intent::Tab => {
            if app.compose.kj_typed().is_some() {
                apply_kj_completion(app);
            } else {
                let tab = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Tab);
                compose_key(bridge, app, tab).await?;
            }
        Intent::Unbound(code) => {
            app.note(format!("Ctrl+A {} is not bound", crate::keys::key_label(code)))
        }
        }
        Intent::TogglePicker => app.open_picker(kaijutsu_types::now_millis()),
    }
    Ok(Acted::Continue)
}

/// Whether the scrolled transcript took a key, or handed it back to the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrolledKey {
    Handled,
    Snap,
}

/// Act on one key while the transcript is off the live tail
/// (`docs/tui.md`, "Scrolling is copy mode").
///
/// The motions, the mark and the prompt are `copy.rs`'s; the two acts that
/// need more than the view are this side's — a search reads the whole
/// transcript to find its row, and a yank renders the marked rows, writes
/// them over OSC 52 and keeps them in the paste buffer.
fn scrolled_key(
    app: &mut App,
    key: &crossterm::event::KeyEvent,
    term_lock: &TermLock,
    width: u16,
) -> ScrolledKey {
    // Release events arrive only where the terminal negotiated the enhanced
    // keyboard protocol; acting on both edges would scroll twice a press,
    // the same reason `Keys::interpret` drops them.
    if key.kind == crossterm::event::KeyEventKind::Release {
        return ScrolledKey::Handled;
    }
    // Keys arrive faster than frames — a wheel tick is three of them, and a
    // committed search is followed straight away by `v` — so the view is
    // settled against the current row counts before it reads one. Without
    // this a key would act on the rows the last frame drew.
    let index = render::row_index(app, width);
    let height = render::transcript_height(app, width);
    let outcome = {
        let Some(view) = app.scrolled_mut() else {
            return ScrolledKey::Snap;
        };
        view.settle(&index, height);
        copy::handle_key(view, key)
    };
    match outcome {
        copy::Outcome::Snap => ScrolledKey::Snap,
        copy::Outcome::Moved => ScrolledKey::Handled,
        copy::Outcome::Leave => {
            app.snap_transcript();
            ScrolledKey::Handled
        }
        copy::Outcome::Find { needle, from, forward, skip_current } => {
            let rows: Vec<String> = render::transcript_all_lines(app, width)
                .iter()
                .map(copy::line_text)
                .collect();
            if let Some(target) = copy::find(&rows, &needle, from, forward, skip_current)
                && let Some(view) = app.scrolled_mut()
            {
                view.jump_to(target);
            }
            ScrolledKey::Handled
        }
        copy::Outcome::Yank(start, end) => {
            let text = render::transcript_rows_text(app, width, start, end);
            app.snap_transcript();
            match write_osc52(term_lock, &text) {
                Ok(()) => app.note(format!("yanked {} lines — Ctrl+A ] pastes", text.lines().count())),
                Err(e) => app.note(format!("clipboard write failed: {e}")),
            }
            app.paste_buffer = Some(text);
            ScrolledKey::Handled
        }
    }
}

/// How far leaving the live tail scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `Ctrl+A [`: the same rows, now under copy mode's keys.
    Still,
    /// `Up`, which is also one third of a wheel tick.
    Line,
    /// `PageUp`.
    Page,
}

/// What a key means while the transcript is on the live tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailKey {
    /// The draft's, as ever.
    Draft,
    /// Nothing to do: the tail is already what is on screen.
    Nothing,
    /// Leave the tail and scroll.
    LeaveTail(Step),
}

/// `Up` at the top edge of the draft scrolls the transcript and `Down` at
/// the live tail does nothing — one rule for a typed arrow and for the wheel,
/// which the terminal sends as arrow keys and neither vim nor this client can
/// tell apart (`docs/tui.md`, "The mouse stays the terminal's").
fn tail_key(app: &mut App, key: &crossterm::event::KeyEvent, width: u16) -> TailKey {
    use crossterm::event::KeyCode;
    // The `:` bar keeps `Up` and `Down` for its own history.
    if app.compose.command_line().is_some() {
        return TailKey::Draft;
    }
    match key.code {
        // Measured in the draft's *drawn* rows, not its logical lines: a
        // one-line draft wider than the screen has rows above and below the
        // cursor, and an `Up` from one of them belongs to the draft. A
        // one-row draft is the common case, so every `Up` scrolls.
        KeyCode::Up if app.compose.cursor_on_first_row(width) => TailKey::LeaveTail(Step::Line),
        KeyCode::PageUp => TailKey::LeaveTail(Step::Page),
        KeyCode::Down if app.compose.cursor_on_last_row(width) => TailKey::Nothing,
        KeyCode::PageDown => TailKey::Nothing,
        _ => TailKey::Draft,
    }
}

/// Leave the live tail: the view is anchored where it already sits, so the
/// screen does not move, and then it scrolls by `step`. The reader lands on
/// the view's last row, the way tmux enters copy mode at the current screen.
fn leave_tail(app: &mut App, width: u16, step: Step) {
    let page = render::transcript_height(app, width);
    let mut view = copy::Scrolled::entering(page);
    match step {
        Step::Still => {}
        Step::Line => view.scroll(-1),
        Step::Page => view.scroll(-(page as isize)),
    }
    // Settled here rather than on the first frame, so the readout the band
    // draws next says where the reader is instead of `line 0/0`.
    let index = render::row_index(app, width);
    view.settle(&index, page);
    // The place belongs to the context on screen. A context whose feed
    // ended has no transcript to place a reader in and renders nothing at
    // all, so say so rather than drop the gesture.
    if !app.set_scrolled(view) {
        app.note("no transcript here; Ctrl+A <digit> reattaches");
    }
}

/// A pasted newline as `\n`, whatever the terminal sent: xterm sends
/// `\r`, some send `\r\n`. Order matters — the pair first, or its `\r`
/// becomes a second newline.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Where a bracketed paste goes.
#[derive(Debug, PartialEq, Eq)]
enum PasteTarget {
    Draft,
    CommandLine,
    Refused(&'static str),
}

/// A paste is text for the draft or the `:` bar. Every other surface
/// refuses it with a notice rather than reading it as keys: the alternate
/// screen's `editor_keys` notation cannot carry a literal `<`, and the
/// picker's and the ledger's filters and an ask card are not wired for
/// one.
fn paste_target(app: &App) -> PasteTarget {
    if editor::route_key(app) == editor::KeyRoute::FullScreen {
        return PasteTarget::Refused("paste on the alternate screen is not wired; use the draft");
    }
    if app.ledger_view.is_some() || app.picker.is_some() || app.ask_card.is_some() {
        return PasteTarget::Refused("paste is not wired into this surface; Esc to the draft first");
    }
    if app.compose.command_line().is_some() { PasteTarget::CommandLine } else { PasteTarget::Draft }
}

/// Act on one `Event::Paste`. Line endings are normalized to `\n`, since
/// terminals differ on what they send for a pasted newline. The draft
/// takes it as one edit at the cursor, like `Ctrl+A ]`; the `:` bar takes
/// it flattened onto one line at its end.
async fn paste_text(bridge: &KernelBridge, app: &mut App, text: String) {
    let text = normalize_paste(&text);
    match paste_target(app) {
        PasteTarget::Refused(why) => app.note(why),
        PasteTarget::CommandLine => {
            let body = app.compose.command_line().map(|line| line[1..].to_string()).unwrap_or_default();
            let flat = text.lines().collect::<Vec<_>>().join(" ");
            app.compose.set_command_body(&format!("{body}{flat}"));
        }
        PasteTarget::Draft => {
            let Some(ctx) = app.current else {
                app.note("no context attached");
                return;
            };
            let ops = app.compose.paste(&text);
            mirror_ops(bridge, app, ctx, &ops).await;
        }
    }
}

/// Switch the screen to `id`: watch it, make it current, and load its draft.
/// The draft is per context, so the compose buffer follows the switch rather
/// than carrying the old context's text along — `load_draft` keeps the `:`
/// line's own history, which is this session's, not this draft's.
///
/// **Every path that changes the context on screen goes through here**: the
/// prefix chords, `Ctrl+A Ctrl+A`, and the picker. A path that watched and
/// switched on its own got the switch without the draft, and
/// `Compose::acked` then kept the change feed from correcting it.
///
/// A switch never fails silently and never tears the loop down: a context
/// that cannot be watched leaves the screen where it was with a notice, and
/// a draft that cannot be read says so rather than blanking the line.
async fn switch_seat(
    bridge: &KernelBridge,
    app: &mut App,
    id: ContextId,
    feeds: &mut Feeds,
) {
    if let Err(e) = watch_context(bridge, app, id, feeds).await {
        app.note(format!("cannot watch {}: {e}", app.label_for(id)));
        return;
    }
    let draft = bridge.read_input(id).await;
    app.switch_to(id);
    app.compose.load_draft(draft.as_deref().unwrap_or(""));
    // The switch moved both ends of the hot set — the context on screen and
    // the one `Ctrl+A Ctrl+A` goes back to — so whatever fell out of it is
    // released now. Only the release half: the watch half is a kernel round
    // trip and belongs to the refresh round, and the one context this switch
    // made hot is already watched above.
    release_cold(app, feeds);
    match draft {
        Err(e) => app.note(format!("draft of {} unread: {e}", app.label_for(id))),
        // The background round has this one and the view lands when it does.
        // The transcript draws a placeholder meanwhile
        // (`render::transcript_window`); the status row says why.
        Ok(_) if feeds.hydrating.contains(&id) => app.note(hydrating_notice(app, id)),
        // The transcript is the new context's own from the next frame, so
        // the switch has nothing to say about what is on screen.
        Ok(_) => app.clear_notice(),
    }
}

/// What the status row says while the context on screen is still being
/// hydrated. One spelling, so the notice can be taken back down again by
/// exactly the line that put it up.
fn hydrating_notice(app: &App, id: ContextId) -> String {
    format!("hydrating {}…", app.label_for(id))
}

/// Mirror what the vi engine did to the draft onto the context's draft
/// block, one `edit_input` per op.
async fn mirror_ops(bridge: &KernelBridge, app: &mut App, ctx: ContextId, ops: &[kaijutsu_editor::EditOp]) {
    for op in ops {
        match bridge
            .edit_input(ctx, op.offset as u64, &op.insert, op.delete as u64)
            .await
        {
            Ok(version) => app.compose.record_ack(version),
            // The draft is the kernel's copy; a failed edit means the two have
            // diverged, and saying so beats typing into a line that is no
            // longer going anywhere.
            Err(e) => app.note(format!("draft edit failed: {e}")),
        }
    }
}

/// One keystroke on the compose surface: mirror what the vi engine did onto
/// the context's draft block, and submit when it asks.
async fn compose_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
) -> Result<()> {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return Ok(());
    };
    let action = app.compose.press(key);
    mirror_ops(bridge, app, ctx, &action.ops).await;
    if let Some(line) = action.command {
        handle_colon_line(bridge, app, ctx, line).await;
        return Ok(());
    }
    if action.submit {
        if app.compose.text().trim().is_empty() {
            return Ok(());
        }
        tracing::debug!(context = %ctx.short(), "submitting the draft");
        // The edge as it stood when Enter was pressed — computed before the
        // submit call, not after `mark_submitted`, which would read the
        // view after the kernel has already moved it (`docs/prompts.md`,
        // "The submit verb").
        let edge = app.views.get(&ctx).and_then(|view| view.edge());
        match bridge.submit_input(ctx, edge).await {
            Ok(block_id) => {
                app.mark_submitted(block_id);
                app.compose.reset();
                app.clear_notice();
                // The partial turn-liveness signal (`docs/tui.md`, "Ctrl+C
                // reclaimed"): our own submit is one of the two ways this
                // client learns a turn started, the other being
                // `ServerEvent::TurnStarted` (`mark_turn_liveness`).
                app.mark_turn_running(ctx);
            }
            Err(e) => app.note(format!("submit failed: {e}")),
        }
    }
    Ok(())
}

/// Dispatch one submitted `:` line (`docs/tui.md`, "The `:` line"). The tui
/// parses it itself (`cmdline::parse`) — the core's own `:w`/`:q` ex-command
/// dialect answers the alternate-screen editor, not this bar.
async fn handle_colon_line(bridge: &KernelBridge, app: &mut App, ctx: ContextId, line: String) {
    match cmdline::parse(&line) {
        ColonVerb::Kj(argv) => match bridge.execute_kj(ctx, argv).await {
            Ok(result) if result.latch.is_some() => {
                app.note(result.latch.map(|l| l.message).unwrap_or_default());
            }
            Ok(result) => {
                app.note(result.stdout.lines().next().unwrap_or("done").to_string());
                // Any kj verb may have changed the roster (fork, promote,
                // archive); the picker and the rank should not wait a tick.
                app.roster_changed = true;
            }
            Err(e) => app.note(format!(":kj failed: {e:#}")),
        },
        ColonVerb::Shell(statement) => match bridge.shell_execute(ctx, &statement).await {
            Ok(_) => app.clear_notice(),
            Err(e) => app.note(format!("shell failed: {e}")),
        },
        ColonVerb::Quit { force } => {
            if force || !app.any_turn_running() {
                app.quit = true;
            } else {
                app.note("a turn this client started is still running — :q! quits anyway");
            }
        }
        ColonVerb::Unknown => app.note(format!("not a tui command: {line}")),
    }
}

/// `Ctrl+C`: the escalation ladder (`docs/tui.md`, "Ctrl+C reclaimed"). The
/// call is fire-and-forget the way the app's own `handle_interrupt` treats
/// it — the notice already says what was asked for, and a failed RPC here
/// would just repeat what `interrupt_context`'s own `Err` already logs.
async fn interrupt_ctrl_c(bridge: &KernelBridge, app: &mut App, ladder: &mut interrupt::Ladder) {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return;
    };
    let running = app.turn_running(ctx);
    match ladder.press(std::time::Instant::now(), running) {
        InterruptStep::Nothing => app.note("nothing to interrupt — :q quits"),
        InterruptStep::Soft => {
            let _ = bridge.interrupt_context(ctx, false).await;
            app.note("interrupting after this tool call — Ctrl+C again to abort");
        }
        InterruptStep::Hard => {
            let _ = bridge.interrupt_context(ctx, true).await;
            app.note("aborted");
        }
        InterruptStep::HardAndClear => {
            let _ = bridge.interrupt_context(ctx, true).await;
            clear_draft(bridge, app, ctx).await;
            app.note("aborted, draft cleared");
        }
    }
}

/// The 3rd `Ctrl+C` press: clear the draft both locally and on the kernel's
/// copy — `edit_input` deletes the whole text, then `reset` puts compose
/// back in its fresh-draft shape (`docs/tui.md`, "Ctrl+C reclaimed").
async fn clear_draft(bridge: &KernelBridge, app: &mut App, ctx: ContextId) {
    let len = app.compose.text().chars().count() as u64;
    if len > 0
        && let Ok(version) = bridge.edit_input(ctx, 0, "", len).await
    {
        app.compose.record_ack(version);
    }
    app.compose.reset();
}

/// `Tab` while `:kj ` is being typed in the bar: recompute candidates for
/// the current text, cycling the selection when it's the same prefix as
/// last time (repeated `Tab` walks the list, the way a shell's does), and
/// rewrite the bar to the selected candidate. Purely local — the bar is not
/// a kernel-mirrored block, unlike the compose draft, so no RPC is needed.
fn apply_kj_completion(app: &mut App) {
    let Some(typed) = app.compose.kj_typed() else {
        return;
    };
    let fresh = completion::complete(&typed, &app.kj_catalog);
    app.completion = match (app.completion.take(), fresh) {
        (Some(mut prev), Some(next)) if prev.prefix == next.prefix => {
            prev.cycle();
            Some(prev)
        }
        (_, next) => next,
    };
    let Some(candidate) = app.completion.as_ref().and_then(|c| c.current()) else {
        return;
    };
    let takes_input = !app
        .kj_catalog
        .iter()
        .find(|k| k.name == candidate.name)
        .is_some_and(|k| k.input_hint.is_empty());
    let body = completion::accept(&candidate.name, !takes_input);
    app.compose.set_command_body(&body);
}

/// Track which contexts this client believes have a turn running — the
/// partial signal `App::turns_running` documents: it is set here on
/// `ServerEvent::TurnStarted` (the other setter is `compose_key`'s own
/// submit) and cleared on `TurnCompleted`/`TurnFailed`.
fn mark_turn_liveness(app: &mut App, event: &ServerEvent) -> bool {
    match event {
        ServerEvent::TurnStarted { context_id, .. } => {
            let started = app.mark_turn_running(*context_id);
            // The start rides the kernel-wide event stream and the reasoning
            // rides the context feed, so the reasoning can land first — when
            // `observe_thinking` has no live turn to latch on and drops it.
            // Latch here on the reasoning the mirror already holds.
            let streaming = streaming_thinking(app, *context_id);
            let latched = app.observe_thinking(*context_id, &streaming);
            started || latched
        }
        ServerEvent::TurnCompleted { context_id, .. } | ServerEvent::TurnFailed { context_id, .. } => {
            app.mark_turn_ended(*context_id)
        }
        _ => false,
    }
}

/// The signals that mean "stop": `SIGTERM` from a runner or `kill`, and
/// `SIGHUP` when the terminal goes away. Either ends the event loop the way
/// `:q` does. Off unix there are none, and `recv` never resolves.
#[cfg(unix)]
struct StopSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn listen() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self { term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())? })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

#[cfg(not(unix))]
struct StopSignals;

#[cfg(not(unix))]
impl StopSignals {
    fn listen() -> io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> &'static str {
        std::future::pending().await
    }
}

/// Hand the terminal back to the host shell: give the screen back, leave raw
/// mode, stop ourselves the way a shell job does, and take the screen again
/// when `SIGCONT` brings us back — vim's `stoptermcap`/`starttermcap` order
/// (`docs/tui.md`, "The `:` line": `Ctrl+Z` is a single suspend, not a
/// toggle).
fn suspend(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
) -> Result<()> {
    // Held across the stop so the key reader cannot read in cooked mode.
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    pop_title();
    let _ = crossterm::execute!(
        io::stdout(),
        Print(ALTERNATE_SCROLL_OFF),
        DisableFocusChange,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste
    );
    editor::abandon();
    let _ = disable_raw_mode();

    raise_stop();

    // Back from SIGCONT, onto a fresh alternate screen: the terminal cleared
    // it on the way out, so the next frame has to paint every cell rather
    // than diff against what ratatui last drew.
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    editor::take_screen()?;
    crossterm::execute!(io::stdout(), Print(ALTERNATE_SCROLL_ON), EnableFocusChange)?;
    push_title();
    // A resize at the current size, not `Terminal::clear`: clear asks the
    // terminal where the cursor is, and this client never does. Both clear
    // the screen and reset the back buffer, which is what a freshly retaken
    // screen needs before the next frame diffs against it.
    let size = terminal.size()?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    Ok(())
}

/// Stop this process with `SIGTSTP` — the unix suspend, not an emulation of
/// it, so the shell's job control sees a stopped job and `fg` resumes it.
#[cfg(unix)]
fn raise_stop() {
    // SAFETY: `raise` takes an integer and is async-signal-safe.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
}

/// There is no `SIGTSTP` off unix; the toggle still works and the second tap
/// says why nothing happened.
#[cfg(not(unix))]
fn raise_stop() {
    tracing::warn!("suspend is a unix gesture; nothing to raise here");
}

/// Open the ledger view (`Ctrl+A l`): every pending and answered ask for the
/// kernel, resolved to full detail. `kj ledger list`/`list --history` name
/// the ids; `kj ledger show` is one round trip per id — acceptable at the
/// default `--limit` (20 pending + 20 history) and no worse than the
/// kernel's own listing commands already accept
/// (`kaijutsu-kernel/src/kj/ledger.rs`, `truncation_notice`).
async fn open_ledger(bridge: &KernelBridge, app: &mut App) {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return;
    };
    let pending_ids = match kaijutsu_client::list_pending(bridge.actor(), ctx).await {
        Ok(ids) => ids,
        Err(error) => {
            app.note(format!("cannot read pending ledger asks: {error}"));
            return;
        }
    };
    let history_ids = match kaijutsu_client::list_history(bridge.actor(), ctx).await {
        Ok(ids) => ids,
        Err(error) => {
            app.note(format!("cannot read ledger history: {error}"));
            return;
        }
    };

    let mut rows = Vec::with_capacity(pending_ids.len() + history_ids.len());
    for id in pending_ids {
        match kaijutsu_client::show_ask_detail(bridge.actor(), ctx, &id).await {
            Ok(Some(detail)) => rows.push(asks::LedgerRow::Pending(pending_row(app, &detail))),
            Ok(None) => app.note(format!("cannot decode pending ask {}", short_ask(&id))),
            Err(error) => app.note(format!("cannot read pending ask {}: {error}", short_ask(&id))),
        }
    }
    for id in history_ids {
        match kaijutsu_client::show_ask_detail(bridge.actor(), ctx, &id).await {
            Ok(Some(detail)) => rows.push(asks::LedgerRow::Answered(answered_row(app, &detail))),
            Ok(None) => app.note(format!("cannot decode answered ask {}", short_ask(&id))),
            Err(error) => app.note(format!("cannot read answered ask {}: {error}", short_ask(&id))),
        }
    }
    app.ledger_view = Some(asks::LedgerViewState {
        rows,
        filter: String::new(),
        selected: 0,
        filtering: false,
        detail: None,
    });
}

/// One `AskDetail` as the ledger view's PENDING row.
fn pending_row(app: &App, detail: &kaijutsu_client::AskDetail) -> asks::PendingRow {
    let (context_label, context_type) = detail
        .context_id
        .map(|ctx| asks::context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
    asks::PendingRow {
        request_id: detail.request_id.clone(),
        age: detail.created_at.map(|at| {
            let now = kaijutsu_types::now_millis();
            crate::status::format_age(std::time::Duration::from_millis(now.saturating_sub(at as u64)))
        }),
        context_label,
        context_type,
        hook: detail.tool.clone().unwrap_or_else(|| "-".to_string()),
        asker: detail.actor_name.clone(),
        reviewer: detail.reviewer_name.clone(),
        reviewable: app.principal.is_some_and(|principal| detail.can_review(principal)),
        statement: detail.statements.first().cloned().unwrap_or_else(|| detail.description.clone()),
    }
}

/// One `AskDetail` as the ledger view's ANSWERED row: when it was decided,
/// how (`allow once`/`allow always`/`deny`, or `status` for an expired or
/// abandoned ask), and by whom (`you`, a principal's short id, or `—` for
/// a rule's auto-decision).
fn answered_row(app: &App, detail: &kaijutsu_client::AskDetail) -> asks::AnsweredRow {
    let (context_label, _context_type) = detail
        .context_id
        .map(|ctx| asks::context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
    let redeemed = match detail.redeemed_at {
        Some(at) => asks::RedeemedMark::At(render::wallclock(at as u64)),
        None => asks::RedeemedMark::Never,
    };
    asks::AnsweredRow {
        request_id: detail.request_id.clone(),
        time: detail.decided_at.map(|at| render::wallclock(at as u64)),
        context_label,
        decision: Some(decision_words(detail)),
        principal: detail.decided_by.map(|by| {
            if app.principal == Some(by) {
                "you".to_string()
            } else {
                detail.decided_by_name.clone().unwrap_or_else(|| by.short())
            }
        }),
        redeemed,
        statement: detail.statements.first().cloned().unwrap_or_else(|| detail.description.clone()),
    }
}

/// Answer the ask card's own ask, then close it.
async fn handle_ask_decision(
    bridge: &KernelBridge,
    app: &mut App,
    card: asks::AskCardState,
    decision: asks::AskDecision,
) {
    if matches!(decision, asks::AskDecision::ViewLedger) {
        open_ledger(bridge, app).await;
        return;
    }
    if !app.principal.is_some_and(|principal| card.detail.can_review(principal)) {
        app.note(format!(
            "ask {} awaits its assigned reviewer; cancel it or ask that reviewer to escalate",
            short_ask(&card.request_id)
        ));
        app.ask_card = Some(card);
        return;
    }
    let allow = !matches!(decision, asks::AskDecision::Deny);
    let remember = matches!(decision, asks::AskDecision::AllowAlways)
        .then_some(kaijutsu_client::RememberScope::Always);
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return;
    };
    report_decision(
        app,
        &card.request_id,
        allow,
        kaijutsu_client::decide_ask_remember(bridge.actor(), ctx, &card.request_id, allow, remember).await,
    );
}

/// One key inside the ledger view: navigate, filter, show, or answer the
/// selected row. Closes the view after any decision — the next
/// `ledger_events` generation bump refreshes the seat flags and the pending
/// count; re-fetching and re-selecting inline is a follow-up, not this
/// pass's scope.
async fn handle_ledger_key(bridge: &KernelBridge, app: &mut App, key: crossterm::event::KeyEvent) {
    if app.ledger_view.as_ref().is_some_and(|view| view.detail.is_some()) {
        if key.code == crossterm::event::KeyCode::Esc && key.modifiers.is_empty() {
            if let Some(view) = app.ledger_view.as_mut() {
                view.detail = None;
            }
        }
        return;
    }
    let Some(view) = app.ledger_view.as_mut() else { return };
    let filtering = view.filtering;
    match asks::ledger_key_to_action(key, filtering) {
        asks::LedgerAction::Back => app.ledger_view = None,
        asks::LedgerAction::Up => view.move_up(),
        asks::LedgerAction::Down => view.move_down(),
        asks::LedgerAction::StartFilter => view.filtering = true,
        asks::LedgerAction::FilterInsert(c) => view.filter.push(c),
        asks::LedgerAction::FilterBackspace => {
            view.filter.pop();
        }
        asks::LedgerAction::CommitFilter | asks::LedgerAction::CancelFilter => view.filtering = false,
        asks::LedgerAction::Show => {
            let Some(request_id) = view.selected_request_id() else { return };
            let Some(ctx) = app.current else { return };
            match kaijutsu_client::show_ask_detail(bridge.actor(), ctx, &request_id).await {
                Ok(Some(detail)) => {
                    if let Some(view) = app.ledger_view.as_mut() {
                        view.detail = Some(detail);
                    }
                }
                Ok(None) => app.note(format!("cannot decode ask {}", short_ask(&request_id))),
                Err(error) => app.note(format!("cannot read ask {}: {error}", short_ask(&request_id))),
            }
        }
        asks::LedgerAction::AllowOnce | asks::LedgerAction::AllowAlways | asks::LedgerAction::Deny => {
            let Some(request_id) = view.selected_request_id() else { return };
            let reviewable = asks::filtered_rows(&view.rows, &view.filter)
                .get(view.selected)
                .is_some_and(|row| row.reviewable());
            if !reviewable {
                app.note(format!(
                    "ask {} awaits its assigned reviewer; cancel it or ask that reviewer to escalate",
                    short_ask(&request_id)
                ));
                return;
            }
            let Some(ctx) = app.current else { return };
            let action = asks::ledger_key_to_action(key, filtering);
            let allow = !matches!(action, asks::LedgerAction::Deny);
            let remember = matches!(action, asks::LedgerAction::AllowAlways)
                .then_some(kaijutsu_client::RememberScope::Always);
            app.ledger_view = None;
            report_decision(
                app,
                &request_id,
                allow,
                kaijutsu_client::decide_ask_remember(bridge.actor(), ctx, &request_id, allow, remember).await,
            );
        }
        asks::LedgerAction::Ignored => {}
    }
}

/// Post the status-line notice for one `kj ledger allow|deny` round trip —
/// shared by the ask card and the ledger view so the two surfaces report a
/// decision, a lost race, or a call failure the same way.
fn report_decision(
    app: &mut App,
    request_id: &str,
    allow: bool,
    result: std::result::Result<kaijutsu_client::rpc::KjExecutionResult, kaijutsu_client::actor::CallError>,
) {
    let verb = if allow { "allowed" } else { "denied" };
    match result {
        Ok(r) if r.exit_code == 0 => app.note(format!("{verb} ask {request_id}")),
        Ok(r) => app.note(format!("kj ledger {verb}: {}", r.stderr.trim())),
        Err(e) => app.note(format!("kj ledger {verb}: {e}")),
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
fn apply_feed(
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
            let touched: Vec<kaijutsu_types::BlockId> =
                delivery.changes().map(crate::app::touched_block).collect();
            for change in delivery.changes() {
                // A block the kernel dropped will never render again, so its
                // wrapped lines go with it.
                if let kaijutsu_client::ContextChange::BlockDeleted { block_id } = change {
                    app.wrap.forget(block_id);
                }
                app.apply_collapse_change(context_id, change);
            }
            if let Some(view) = app.views.get_mut(&context_id) {
                if let Err(e) = view.mirror.receive(delivery) {
                    tracing::warn!(context = %context_id.short(), error = %e, "mirror rejected a delivery");
                }
                view.seed_collapse();
            }
            reconcile_draft(app, context_id);
            app.observe_thinking(context_id, &touched);
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
fn land_rehydrate(
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
        view.seed_collapse();
    }
    match applied {
        Ok(()) => {
            let mut hydrated = false;
            // The feed only says `Resubscribed` on a new connection,
            // so the stream that clears a turn flag was broken. The
            // status watch coalesces, so a fast reconnect can leave
            // this the only sign: forget what this client believed
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
            // was running rather than carry a flag nothing clears
            // (`docs/tui.md`, "Turn liveness is a partial signal").
            app.forget_turn_liveness();
            // A rebuilt mirror names no changed blocks, so the pane
            // latches on reasoning that is still streaming — once a
            // `TurnStarted` says a turn is running again.
            let streaming = streaming_thinking(app, context_id);
            app.observe_thinking(context_id, &streaming);
            app.note("reconnected; context rehydrated; turn liveness reset");
        }
        Err(e) => abandon_rehydrate(app, feeds, context_id, e),
    }
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
fn mark_activity(app: &mut App, event: &ServerEvent) -> bool {
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

/// Take the screen for the session: raw mode, bracketed paste, the alternate
/// screen, and one full-screen `Terminal` over it.
///
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
fn abandon_rehydrate(app: &mut App, feeds: &mut Feeds, context_id: ContextId, error: impl std::fmt::Display) {
    feeds.stop(context_id);
    app.release(context_id);
    app.note(format!(
        "{} could not finish reconnecting: {error}; Ctrl+A <digit> reattaches",
        app.label_for(context_id)
    ));
/// One terminal for the whole session. A full-screen viewport is anchored to
/// nothing, so no frame asks the terminal where the cursor is and a slow hop
/// never stalls a redraw (`docs/tui.md`, "The owned screen").
fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    // A paste arrives as one `Event::Paste` — text, not keystrokes — so a
    // newline inside it is a newline in the draft, not an Enter.
    crossterm::execute!(io::stdout(), EnableBracketedPaste)?;
    editor::take_screen()?;
    crossterm::execute!(io::stdout(), Print(ALTERNATE_SCROLL_ON), EnableFocusChange)?;
    push_title();
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// Alternate scroll, DECSET 1007. With mouse reporting off — which this
/// client never turns on — a terminal holding the alternate screen turns
/// each wheel tick into arrow-key presses, which is how the wheel scrolls
/// the transcript (`docs/tui.md`, "The mouse stays the terminal's"). xterm
/// needs the mode; kitty and foot do it by default. crossterm has no
/// constant for it, so the bytes go out as they are.
const ALTERNATE_SCROLL_ON: &str = "\x1b[?1007h";
const ALTERNATE_SCROLL_OFF: &str = "\x1b[?1007l";

/// xterm's title stack: push the shell's own title before the first
/// [`set_title`], pop it back on the way out. The stack is how the title is
/// given back exactly as it was found — an empty title on exit would leave
/// a wezterm tab blank instead. crossterm has no constant for either, so
/// the bytes go out as they are.
const TITLE_PUSH: &str = "\x1b[22;0t";
const TITLE_POP: &str = "\x1b[23;0t";

/// Whether this process has a title on the terminal's stack, the same shape
/// as `editor::ENTERED` and for the same reason: [`restore_terminal`] is
/// idempotent and runs from the exit path, the panic hook and the signal
/// path, while a pop that answers no push takes the *shell's* own saved
/// title off the stack.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// Claim a one-way flag: true when this call is the one that changed it.
/// The push and the pop are each done once, whoever gets there first.
fn claim(flag: &AtomicBool, want: bool) -> bool {
    flag.swap(want, Ordering::SeqCst) != want
}

/// Push the shell's own window title, once.
fn push_title() {
    if claim(&TITLE_PUSHED, true) {
        let _ = crossterm::execute!(io::stdout(), Print(TITLE_PUSH));
    }
}

/// Pop the title this process pushed, if it pushed one; a no-op otherwise.
fn pop_title() {
    if claim(&TITLE_PUSHED, false) {
        let _ = crossterm::execute!(io::stdout(), Print(TITLE_POP));
    }
}

/// Leave the terminal the way we found it. Best-effort on every step: a
/// failure here must not mask the error that ended the loop.
///
/// Nothing is printed on the way out: leaving the alternate screen restores
/// the shell's own screen, and `:q` is quiet (`docs/tui.md`, "The owned
/// screen").
fn leave_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    reader_stop: &AtomicBool,
) {
    // The reader finishes the poll it is in, sees the flag, and stops; the
    // lock below waits out that poll before raw mode goes.
    reader_stop.store(true, Ordering::SeqCst);
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    restore_terminal();
}

/// Put the terminal back the way the host shell had it: the main screen if
/// this process took the alternate one, the shell's own cursor shape, cooked
/// input.
/// Idempotent and best-effort. The normal exit, the panic hook, and the
/// signal path all come through here, so every way out agrees.
pub fn restore_terminal() {
    // A panic inside a frame unwinds past that frame's own
    // `EndSynchronizedUpdate`, and a terminal left inside an update paints
    // nothing. Ending one that was never begun is a no-op.
    let _ = crossterm::execute!(io::stdout(), EndSynchronizedUpdate);
    editor::abandon();
    pop_title();
    // `?1004l` and `?1007l` go out blind: resetting a mode the terminal
    // never had on is a no-op, unlike a title pop, which would take the
    // shell's own saved title off the stack.
    let _ = crossterm::execute!(
        io::stdout(),
        Print(ALTERNATE_SCROLL_OFF),
        DisableFocusChange,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste
    );
    let _ = disable_raw_mode();
}

// ────────────────────────────────────────────────────────────────────────────
// The full-screen surfaces: editor and diff (docs/tui.md, "Editor and diff")
// ────────────────────────────────────────────────────────────────────────────

/// The nick this client attaches to the kernel's peer registry under.
///
/// Not `kaijutsu-app`: that nick is the fallback the kernel signals when the
/// submitter principal owns no window, and taking it would put a terminal
/// between the app and a headless `vi`.
const PEER_NICK: &str = "kaijutsu-tui";

/// Attach as a peer so `vi <path>` and `kj editor` reach this terminal.
///
/// This is the **only** notification that an editor session opened. The
/// `subscribeEditor` push channel carries state changes and closes; the kernel
/// publishes neither at open (`Kernel::editor_open_as`), so a client watching
/// that stream alone never learns a session exists. `signal_open_editor` fans
/// the `open_editor` invoke out to the submitter principal's attached peers,
/// which is why this attaches rather than polling.
///
/// The fan-out is by principal, so a `vi` run by a *model's* principal in a
/// sibling context does not reach here — it reaches the `kaijutsu-app`
/// fallback instead. Exact-window targeting is `docs/vi.md`'s open item.
async fn attach_editor_peer(bridge: &KernelBridge, tx: mpsc::Sender<EditorOpen>) -> Result<()> {
    let (invocation_tx, invocation_rx) = std::sync::mpsc::channel::<PeerInvocation>();
    bridge
        .actor()
        .attach_peer(
            PeerConfig {
                // Per-process, so two terminals for one user coexist in the
                // registry instead of evicting each other.
                nick: PEER_NICK.to_string(),
                instance: uuid::Uuid::new_v4().to_string(),
            },
            invocation_tx,
        )
        .await
        .context("attach as a peer for open_editor signals")?;
    // A plain thread, not a task: `PeerInvocation` arrives on a std channel and
    // `blocking_send` would panic on a runtime thread.
    std::thread::spawn(move || serve_invocations(invocation_rx, tx));
    Ok(())
}

/// Answer peer invocations until the kernel drops the channel.
///
/// The reply is sent as soon as the signal is decoded and forwarded, which is
/// what it claims: the signal was received. The kernel bounds its wait on this
/// reply, so it must never wait for a frame.
fn serve_invocations(rx: std::sync::mpsc::Receiver<PeerInvocation>, tx: mpsc::Sender<EditorOpen>) {
    while let Ok(invocation) = rx.recv() {
        let answer = match invocation.action.as_str() {
            "open_editor" => match editor::parse_open_signal(&invocation.params) {
                Ok(open) => {
                    let echo = serde_json::json!({
                        "session": open.state.session,
                        "path": open.path,
                    });
                    if tx.blocking_send(open).is_err() {
                        return;
                    }
                    serde_json::to_vec(&echo).map_err(|e| format!("serialize: {e}"))
                }
                Err(e) => Err(e),
            },
            other => Err(format!("kaijutsu-tui serves open_editor, not {other}")),
        };
        let _ = invocation.reply.send(answer);
    }
}

/// Apply an editor push to the screen. Returns whether the frame changed.
///
/// `EditorClosed` is what `:q`/`ZZ`/`ZQ` produce — the kernel alone knows the
/// mode, so it alone decides a quit — and it is what gives the conversation
/// back. A `Reconnected` probes the session with an empty key batch, because a
/// kernel restart leaves the id unknown while the connection looks ordinary.
async fn observe_editor_event(bridge: &KernelBridge, app: &mut App, event: &ServerEvent) -> bool {
    if editor::apply_push(app, event) {
        return true;
    }
    match event {
        ServerEvent::Reconnected => {
            let Some(session) = app.screen.editor().map(|s| s.session) else {
                return false;
            };
            // An empty batch applies no keys and mutates nothing; the only
            // thing it can report is whether the session still exists.
            match bridge.actor().editor_keys(session, "").await {
                Ok(_) => false,
                Err(e) if editor::is_session_lost(&e.to_string()) => editor::leave_on_session_lost(
                    app,
                    "editor session lost across the reconnect; reopen with vi",
                ),
                Err(e) => {
                    tracing::warn!(session, error = %e, "editor liveness probe failed");
                    false
                }
            }
        }
        _ => false,
    }
}

/// A key while a full-screen surface has the screen.
async fn act_full_screen(bridge: &KernelBridge, app: &mut App, key: KeyEvent) -> Result<()> {
    if let Some(session) = app.screen.editor().map(|s| s.session) {
        // A key with no notation is refused rather than sent: the kernel's
        // `parse_keys` drops an unknown `<...>` token silently, so forwarding
        // one would look like a working key that does nothing.
        let Some(notation) = editor::key_notation(&key) else {
            return Ok(());
        };
        // One awaited call per key: the loop handles keys in order and never
        // has two in flight, so keystrokes cannot reorder on the wire and no
        // ordering pipe is needed.
        match bridge.actor().editor_keys(session, &notation).await {
            Ok(state) => {
                if let Some(screen) = app.screen.editor_mut()
                    && screen.session == state.session
                {
                    screen.state = state;
                }
            }
            Err(e) => {
                let message = e.to_string();
                if editor::is_session_lost(&message) {
                    // A kernel restart: the sessions are in memory and the
                    // persisted kernel id is unchanged, so the buffer on
                    // screen is dead and typing echoes nothing. Give the
                    // conversation back instead of freezing.
                    editor::leave_on_session_lost(
                        app,
                        "editor session lost (kernel restarted?); reopen with vi",
                    );
                } else {
                    tracing::warn!(session, keys = %notation, error = %message, "editor_keys failed");
                }
            }
        }
        return Ok(());
    }

    if let ScreenMode::Diff(screen) = &mut app.screen {
        let body_h = screen.body_h();
        if diff::handle_key(screen, &key, body_h) == DiffKey::Close {
            app.screen = ScreenMode::Conversation;
        }
    }

    Ok(())
}

/// Write the OSC 52 clipboard sequence (`copy::osc52_sequence`) directly to
/// stdout, under the same lock every other terminal write takes — a yank in
/// the scrolled transcript is the only caller (`docs/tui.md`, "Scrolling is
/// copy mode").
fn write_osc52(term_lock: &TermLock, text: &str) -> io::Result<()> {
    use std::io::Write as _;
    let _guard = term_lock.lock();
    let mut stdout = io::stdout();
    stdout.write_all(copy::osc52_sequence(text).as_bytes())?;
    stdout.flush()
}

/// `Ctrl+A v` — the newest block of the current context the diff viewer opens
/// on, frozen into a screen.
///
/// Nothing on the wire opens a diff view: `kj diff` authors a
/// `ContentType::Diff` block and the client decides. The rule is the app's
/// [`crate::diff::openable_diff`], so the two clients never disagree about
/// what is a diff.
fn open_diff(app: &App) -> Option<crate::diff::DiffScreen> {
    let view = app.current_view()?;
    let (block, how) = view.mirror.blocks().iter().rev().find_map(|b| {
        crate::diff::openable_diff(b.content_type, &b.content).map(|how| (b, how))
    })?;
    let title = format!("block #{}", block.id.seq);
    Some(match crate::diff::parse_open(&how, &block.content) {
        Ok(model) => crate::diff::DiffScreen::new(title, &model, &app.palette),
        // A declared diff that will not parse is a visible error, never an
        // empty viewer (`kaijutsu-diff`'s viewer contract).
        Err(e) => {
            crate::diff::DiffScreen::unparsed(title, &e.to_string(), &block.content, &app.palette)
        }
    })
}

/// `--diff <a> [b]` — run `kj diff` in the starting context and freeze its
/// output into the diff screen.
///
/// The kernel's verb is the one source: `kj diff` resolves both sides through
/// `resolve_editor_target`, the same function the vi editor binds with, so the
/// TUI never has its own idea of what document a path names.
async fn open_kj_diff(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    args: &[String],
) -> Result<()> {
    let mut argv = vec!["diff".to_string()];
    argv.extend(args.iter().cloned());
    let title = format!("kj {}", argv.join(" "));
    let result = bridge.execute_kj(context_id, argv).await?;
    if result.exit_code != 0 {
        anyhow::bail!(
            "{title} exited {}: {}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    let how = crate::diff::OpenAs::Declared;
    app.screen = ScreenMode::Diff(match crate::diff::parse_open(&how, &result.stdout) {
        Ok(model) => crate::diff::DiffScreen::new(title, &model, &app.palette),
        Err(e) => {
            crate::diff::DiffScreen::unparsed(title, &e.to_string(), &result.stdout, &app.palette)
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_client::{
        ContextChange, ContextDelivery, ContextMirror, TurnCompletedStopReason, TurnOrigin, VersionedChange,
    };
    use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId, Role};

    /// Watch a context with an empty mirror — what `watch_context` leaves
    /// behind, without a kernel to hydrate from.
    fn watched(app: &mut App, id: ContextId) {
        app.views.insert(id, ContextView::new(ContextMirror::new(id)));
    }

    // ────────────────────────────────────────────────────────────────────
    // Turn liveness (docs/tui.md, "Ctrl+C reclaimed")
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn turn_started_marks_the_context_running() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let event = ServerEvent::TurnStarted { context_id: id, principal_id: PrincipalId::new() };
        assert!(mark_turn_liveness(&mut app, &event));
        assert!(app.turn_running(id));
    }

    /// The turn's start and its reasoning ride different channels — the
    /// kernel-wide event stream and the context feed — so the reasoning can
    /// land first, when `observe_thinking` has no live turn to latch on.
    /// `TurnStarted` latches on what the mirror already holds.
    #[test]
    fn a_turn_that_starts_after_its_reasoning_still_opens_the_pane() {
        use kaijutsu_client::ContextMirror;
        use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, Role, Status};

        let id = ContextId::new();
        let mut app = App::new("amy");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(
                vec![
                    BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), 1), BlockKind::Thinking)
                        .role(Role::Model)
                        .status(Status::Running)
                        .content("the unlink path first")
                        .build(),
                ],
                1,
            )
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        app.switch_to(id);
        assert!(!app.thinking_pane_latched(id), "no turn is known running yet");

        let event = ServerEvent::TurnStarted { context_id: id, principal_id: PrincipalId::new() };
        assert!(mark_turn_liveness(&mut app, &event));
        assert!(
            app.thinking_pane_latched(id),
            "the feed won the race and the pane never opened"
        );
    }

    #[test]
    fn turn_completed_clears_a_running_context() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.mark_turn_running(id);
        let event = ServerEvent::TurnCompleted {
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
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            output_block_id: None,
            stop_reason: TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert!(!mark_turn_liveness(&mut app, &event));
    }

    /// The context on screen changes in one place. `switch_seat` watches
    /// the new context, switches to it, and loads its draft; a path that
    /// calls `App::switch_to` itself gets the switch without one of those
    /// three, and the miss is invisible on the frame it happens — the
    /// picker's own switch kept the previous context's draft on the compose
    /// line, and `Compose::acked` then refused the feed's correction.
    ///
    /// `run()`'s startup attach is the one sanctioned direct call: there is
    /// no seat to switch from, and it loads its own draft.
    #[test]
    fn every_context_switch_goes_through_switch_seat() {
        let source = include_str!("run.rs");
        // The module's own code, not this module's tests — the assertion
        // below names the pattern it is looking for.
        let source = source.split_once("\n#[cfg(test)]").expect("run.rs has tests").0;
        let mut current = "<file scope>";
        for line in source.lines() {
            if let Some(name) = top_level_fn_name(line) {
                current = name;
            }
            if line.contains("app.switch_to(") && !line.trim_start().starts_with("//") {
                assert!(
                    matches!(current, "run" | "switch_seat"),
                    "`{current}` switches the context on its own; call `switch_seat`, \
                     which watches the new context and loads its draft too"
                );
            }
        }
    }

    /// `Ctrl+A Ctrl+A` re-watches like every other switch. A feed that ends
    /// drops the context's view (`apply_feed`'s `Terminated`), and a
    /// context with no view renders nothing at all — no transcript, no
    /// stream — so a toggle that only set `current` left a blank band under
    /// the context we came from.
    #[test]
    fn the_last_context_toggle_goes_through_switch_seat() {
        let source = include_str!("run.rs");
        let arm = source
            .split_once("Intent::LastContext =>")
            .expect("run.rs has a LastContext arm")
            .1;
        let arm = &arm[..arm.find("Intent::OpenDiff").expect("the next arm follows")];
        assert!(
            arm.contains("switch_seat("),
            "the LastContext arm does not route through `switch_seat`: {arm}"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // The window title (docs/tui.md, "What owning the screen lets us use")
    // ────────────────────────────────────────────────────────────────────

    /// The title names the context on screen, so a wezterm tab reads like a
    /// tmux window. Nothing is named before a context is attached.
    #[test]
    fn the_window_title_names_the_context_on_screen() {
        let mut app = App::new("amy");
        assert_eq!(wanted_title(&app), None, "no context, nothing to name");

        let id = ContextId::new();
        watched(&mut app, id);
        app.switch_to(id);
        assert_eq!(
            wanted_title(&app),
            Some(format!("{} — kaijutsu", id.short())),
            "an unlabeled context is named by its short id, as the status line names it"
        );
    }

    /// The one place the title is sent: a repeat is not written again, and a
    /// switch to another context is.
    #[test]
    fn a_title_is_sent_once_per_change() {
        let mut sent: Option<String> = None;
        let mut written: Vec<String> = Vec::new();
        let mut send = |sent: &mut Option<String>, want: Option<String>| {
            if let Some(title) = title_to_send(sent, want) {
                written.push(title);
            }
        };
        send(&mut sent, Some("probe — kaijutsu".to_string()));
        send(&mut sent, Some("probe — kaijutsu".to_string()));
        send(&mut sent, Some("other — kaijutsu".to_string()));
        send(&mut sent, None);
        assert_eq!(written, vec!["probe — kaijutsu".to_string(), "other — kaijutsu".to_string()]);
    }

    /// The title stack is popped exactly as often as it is pushed.
    ///
    /// [`restore_terminal`] is idempotent and runs from the exit path, the
    /// panic hook and the signal path — `leave_terminal` then the hook is
    /// two calls — while a panic before the screen is taken runs it with no
    /// push behind it. A pop that answers no push takes the *shell's* own
    /// saved title off the stack.
    #[test]
    fn a_title_is_popped_once_and_only_after_a_push() {
        let flag = AtomicBool::new(false);
        assert!(!claim(&flag, false), "nothing to pop before a push");
        assert!(claim(&flag, true), "the first push is the one that writes");
        assert!(!claim(&flag, true), "a second push writes nothing");
        assert!(claim(&flag, false), "the first pop is the one that writes");
        assert!(!claim(&flag, false), "a second pop writes nothing");
    }

    /// Input implies focus, at every arm input arrives on. A terminal that
    /// reports `FocusLost` and never reports again would otherwise leave the
    /// client believing nobody is looking for the rest of the session
    /// (`App::saw_input`).
    #[test]
    fn every_input_arm_says_the_terminal_is_focused() {
        let source = include_str!("run.rs");
        let source = source.split_once("\n#[cfg(test)]").expect("run.rs has tests").0;
        for arm in ["Event::Key(key) => {", "Event::Paste(text) => {"] {
            let body = source.split_once(arm).unwrap_or_else(|| panic!("run.rs has a `{arm}` arm")).1;
            let body = &body[..body.find("\n                    }").expect("the arm ends")];
            assert!(
                body.contains("saw_input()"),
                "the `{arm}` arm does not say the terminal is focused: {body}"
            );
        }
    }

    /// The beat timer is armed only while someone is looking: the refresh
    /// round re-arms it on its own cadence, and an unfocused client that
    /// re-armed there would animate the pulse again a round after the
    /// `FocusLost` that stopped it.
    #[test]
    fn the_beat_timer_stays_disarmed_while_unfocused() {
        let mut app = App::new("amy");
        app.tracks = vec![crate::picker::TrackRow {
            id: "t1".to_string(),
            score_context_id: ContextId::new(),
            playing: true,
            bpm: 120,
            bar: 1,
            beat: 1,
        }];
        assert!(beat_wake_armed(&app), "a playing track arms the timer while focused");
        app.focused = false;
        assert!(!beat_wake_armed(&app), "nothing animates for a terminal nobody is looking at");

        let mut beat_wake = Some(Instant::now());
        let mut tempo = 2.0;
        rearm_beat_wake(&app, &mut beat_wake, &mut tempo);
        assert_eq!(beat_wake, None, "an unfocused re-arm disarms instead");
    }

    /// The name of a `fn` declared at the top level of a module — the
    /// enclosing function for every line until the next one.
    fn top_level_fn_name(line: &str) -> Option<&str> {
        let rest = line
            .strip_prefix("fn ")
            .or_else(|| line.strip_prefix("pub fn "))
            .or_else(|| line.strip_prefix("async fn "))
            .or_else(|| line.strip_prefix("pub async fn "))?;
        Some(&rest[..rest.find('(')?])
    }

    /// `Up` at the top edge of the draft scrolls the transcript and `Down`
    /// at the live tail does nothing — one rule for a typed arrow and for
    /// the wheel (`docs/tui.md`, "The mouse stays the terminal's").
    #[test]
    fn up_scrolls_only_once_the_drafts_cursor_is_on_its_first_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut app = App::new("amy");

        // A one-row draft is the common case, so every `Up` scrolls.
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Nothing, "the tail is already on screen");

        // Inside a taller draft `Up` moves the draft's cursor first, as vim
        // does, and scrolls once it is on the first row.
        app.compose.load_draft("one\ntwo");
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::Draft);
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Nothing, "the cursor is on the last row");
        app.compose.press(up);
        assert!(app.compose.cursor_on_first_row(80), "the draft's own cursor moved up");
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Draft, "there is a draft row below");
    }

    /// The rows are the drawn ones, not the logical lines: one long line
    /// wraps, and an `Up` from a continuation row belongs to the draft.
    #[test]
    fn a_wrapped_one_line_draft_keeps_its_own_arrows() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut app = App::new("amy");
        // One logical line, three rows at twenty columns; `load_draft` rests
        // the cursor on the last character.
        app.compose.load_draft(&"x".repeat(50));
        assert_eq!(app.compose.cursor_visual_row(20).1, 3, "three drawn rows");
        assert_eq!(tail_key(&mut app, &up, 20), TailKey::Draft, "a continuation row is the draft's");
        assert_eq!(tail_key(&mut app, &down, 20), TailKey::Nothing, "the last row");

        // `0` is what reaches the first drawn row: the draft's own `Up` is
        // modalkit's logical-line motion, which has nowhere to go inside one
        // wrapped line — vim's `k` behaves the same, and `gk` is the motion
        // that would not (`docs/issues.md`).
        app.compose.press(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert!(app.compose.cursor_on_first_row(20), "on the first drawn row now");
        assert_eq!(tail_key(&mut app, &up, 20), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 20), TailKey::Draft, "rows below it in the draft");
    }

    #[test]
    fn the_page_keys_leave_the_tail_upward_and_do_nothing_downward() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(tail_key(&mut app, &page_up, 80), TailKey::LeaveTail(Step::Page));
        assert_eq!(tail_key(&mut app, &page_down, 80), TailKey::Nothing);
    }

    /// The `:` bar keeps `Up` and `Down` for its own history.
    #[test]
    fn the_colon_bar_keeps_the_arrow_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        app.compose.press(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert_eq!(tail_key(&mut app, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 80), TailKey::Draft);
        assert_eq!(tail_key(&mut app, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 80), TailKey::Draft);
    }

    /// Leaving the tail anchors the view where it already is, so the screen
    /// does not move, and a page step is the transcript's own height.
    #[test]
    fn leaving_the_tail_takes_the_transcripts_height_for_a_page() {
        let mut app = App::new("amy");
        let id = ContextId::new();
        watched(&mut app, id);
        app.switch_to(id);
        app.screen_rows = 24;
        leave_tail(&mut app, 80, Step::Page);
        let view = app.scrolled().expect("off the tail");
        assert_eq!(view.page(), 20, "24 rows less the band's four");
        assert!(!app.following());
    }

    /// A `Feeds` with no tasks and nowhere to deliver but the test's own
    /// channels — enough for the paths that only read `hydrating` or stop a
    /// forwarder that was never started.
    fn feeds_for_test() -> (Feeds, mpsc::Receiver<Hydrated>) {
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

    /// The place the reader scrolled to belongs to the context, not to the
    /// screen: switching away and back finds it where it was, and a context
    /// at its tail is still at its tail (`docs/tui.md`, "The buffer").
    ///
    /// The page height fingerprints the view, so this reads the same
    /// `Scrolled` back rather than merely some scrolled state.
    #[test]
    fn switching_seats_keeps_each_contexts_own_place() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let mut app = App::new("amy");
        watched(&mut app, a);
        watched(&mut app, b);
        app.switch_to(a);
        assert!(app.set_scrolled(copy::Scrolled::entering(7)), "a is watched");

        app.switch_to(b);
        assert!(app.following(), "b opens on its own live tail");

        app.switch_to(a);
        assert_eq!(
            app.scrolled().map(copy::Scrolled::page),
            Some(7),
            "a came back to the place the reader left it"
        );
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


        app.switch_to(b);
        assert!(app.following(), "b never left its tail");
    }

    /// A released context's place goes with it: the next visit hydrates and
    /// opens on the live tail, which is what a cold context should do.
    #[test]
    fn releasing_a_context_takes_its_scrolled_place_with_it() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let mut app = App::new("amy");
        watched(&mut app, a);
        watched(&mut app, b);
        app.switch_to(a);
        assert!(app.set_scrolled(copy::Scrolled::entering(7)), "a is watched");

        app.switch_to(b);
        assert!(app.cold_contexts().is_empty(), "a is the previous context, and hot");
        app.switch_to(ContextId::new());
        let cold = app.cold_contexts();
        assert_eq!(cold, vec![a], "a left both the screen and the ring");
        for id in cold {
            app.release(id);
        }

        watched(&mut app, a);
        app.switch_to(a);
        assert!(app.following(), "a came back cold, on its tail");
    }

    #[test]
    fn a_paste_goes_to_the_draft_or_the_colon_bar_and_nowhere_else() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        assert_eq!(paste_target(&app), PasteTarget::Draft);

        app.compose.press(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert_eq!(paste_target(&app), PasteTarget::CommandLine);
        app.compose.press(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.open_picker(0);
        assert!(matches!(paste_target(&app), PasteTarget::Refused(_)), "the picker has no text field");
        app.picker = None;

        app.screen = ScreenMode::Diff(crate::diff::DiffScreen::unparsed(
            "probe",
            "not a diff",
            "one\ntwo",
            &app.palette,
        ));
        assert!(matches!(paste_target(&app), PasteTarget::Refused(_)), "the alternate screen refuses");
    }

    #[test]
    fn a_pasted_newline_is_one_newline_whatever_the_terminal_sent() {
        assert_eq!(normalize_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    /// The loop's own kernel calls live on their own tasks — the refresh
    /// round in `refresh::fetch`, the hot set's hydrate in `start_hydrate`
    /// — and land through a join arm. Inside the loop they would hold every
    /// key until a busy kernel answered (`docs/tui.md`, "Keys").
    #[test]
    fn the_event_loop_never_awaits_the_kernel_for_background_work() {
        let source = include_str!("run.rs");
        let start = source.find("async fn event_loop(").expect("event_loop is in run.rs");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("event_loop ends");
        let mut scanned = body[..end].to_string();
        for helper in HELPERS {
            scanned.push('\n');
            scanned.push_str(function_body(source, helper));
        }
        for call in [
            "list_contexts(",
            "poll_new_asks(",
            "list_tracks(",
            "show_ask_detail(",
            "hydrate_context(",
        ] {
            assert!(
                !scanned.contains(call),
                "`{call}` is awaited on the loop's feed-apply path; run it on its own task and \
                 land the result through a join arm, so keys never queue behind the kernel"
            );
        }
    }

    #[test]
    fn an_unrelated_event_carries_no_turn_liveness_either() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let event = ServerEvent::ContextSwitched { context_id: id };
        assert!(!mark_turn_liveness(&mut app, &event));
    }
}
    ///
    /// A violation can hide one hop down: `event_loop`'s own text calls
    /// `apply_feed` (in the `feed_rx` arm), and `apply_feed` is where a
    /// `Resubscribed` delivery decides it needs a rehydrate and where an
    /// ordinary `Changed` delivery reconciles the draft — both places a
    /// straight-through kernel call has lived before. Scanning
    /// `event_loop`'s literal body alone misses that, so this also scans the
    /// named `HELPERS` below — the feed-apply path's own helpers — for the
    /// same forbidden names. It stays a short, explicit list rather than a
    /// full call-graph walk on purpose: the key path legitimately awaits the
    /// kernel directly for a keystroke's own round trip (`switch_seat`
    /// awaiting `read_input`, `open_ledger` reading `kj ledger show` to open
    /// a view, both reached through `act`'s intent dispatch), and walking
    /// the whole graph would flag those too. `start_rehydrate` and
    /// `start_hydrate` are deliberately NOT in `HELPERS`: both hand their
    /// kernel call to `tokio::spawn`, so scanning their bodies would flag
    /// the exact off-loop pattern this test exists to require. Add a name to
    /// `HELPERS` when the feed-apply chain grows another synchronous hop.
        const HELPERS: &[&str] = &["apply_feed", "apply_delivery", "land_rehydrate"];

            "rehydrate_context(",
    /// The literal source of a top-level `fn name(` or `async fn name(` in
    /// `run.rs`: from the `fn` keyword to its closing brace at column zero —
    /// the same heuristic [`the_event_loop_never_awaits_the_kernel_for_background_work`]
    /// has always used for `event_loop` itself, which holds because this
    /// file's inner blocks stay indented.
    ///
    /// Panics when `name` is not found. A watched helper is named because it
    /// sits on the loop's feed-apply path; a silent `None` for a renamed or
    /// removed one would drop it out of the scan without saying so, which is
    /// exactly the failure mode `HELPERS` exists to prevent one level up —
    /// the whole point is that this test cannot go quiet on its own.
    fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("`{name}` is in HELPERS but no `fn {name}(` exists in run.rs any more"));
        let body = &source[start..];
        let end = body.find("\n}\n").unwrap_or_else(|| panic!("`{name}`'s body never closes at column zero"));
        &body[..end]
    }


    // ────────────────────────────────────────────────────────────────────
    // The panic hook (docs/tui.md, "Every way out restores the terminal")
    // ────────────────────────────────────────────────────────────────────

    /// The design fact the hook's restore-or-not decision rests on:
    /// `tokio::task::try_id()` is `None` for `LocalSet::run_until`'s own
    /// root future and `Some` inside a task `spawn_local` puts on that same
    /// `LocalSet` — even though both run on the very thread that drives the
    /// `LocalSet`, so a thread id alone cannot tell them apart.
    #[test]
    fn try_id_is_some_inside_a_spawned_task_and_none_at_the_root() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("current-thread runtime");
        let local = tokio::task::LocalSet::new();
        let (root_id, task_id) = runtime.block_on(local.run_until(async {
            let root_id = tokio::task::try_id();
            let task_id = tokio::task::spawn_local(async { tokio::task::try_id() })
                .await
                .expect("the spawned task did not panic");
            (root_id, task_id)
        }));
        assert_eq!(root_id, None, "the run_until root future is not itself inside a task");
        assert!(task_id.is_some(), "a spawn_local task carries a task id");
    }

    /// [`panic_unwinds_the_loop`] says yes only for the root panic on the
    /// loop's own thread — a task sharing that thread, and a panic on any
    /// other thread, both say no (`docs/issues.md` before this fix named a
    /// thread-id check alone, which the test above falsifies).
    #[test]
    fn panic_unwinds_the_loop_only_at_the_root_on_the_loop_thread() {
        let loop_thread = std::thread::current().id();
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("current-thread runtime");
        let handle = runtime.spawn(async {});
        let task_id = handle.id();
        runtime.block_on(handle).expect("the spawned task did not panic");
        let other_thread =
            std::thread::spawn(|| std::thread::current().id()).join().expect("the probe thread joins");

        assert!(
            panic_unwinds_the_loop(loop_thread, Some(loop_thread), None),
            "the root panic, on the loop thread and in no task, unwinds it"
        );
        assert!(
            !panic_unwinds_the_loop(loop_thread, Some(loop_thread), Some(task_id)),
            "a task panic, even on the loop thread, does not unwind it"
        );
        assert!(
            !panic_unwinds_the_loop(other_thread, Some(loop_thread), None),
            "a panic on another thread never unwinds the loop"
        );
        assert!(
            !panic_unwinds_the_loop(loop_thread, None, None),
            "no recorded loop thread means the hook cannot tell, so it must not restore"
        );
    }
