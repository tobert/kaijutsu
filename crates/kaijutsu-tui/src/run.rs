//! The event loop: crossterm keys, context feeds, kernel events and a redraw
//! tick as `tokio::select!` arms.
//!
//! The loop coalesces: any arm that changes state marks the frame dirty and
//! the tick draws once. A burst of streaming appends therefore costs one
//! redraw, not one per token.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::Event;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use kaijutsu_client::{ContextInfo, FeedEvent, ServerEvent};
use parking_lot::Mutex;
use kaijutsu_types::ContextId;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::app::{App, ContextView};
use crate::bridge::KernelBridge;
use crate::keys::{Intent, Keys};
use crate::render;

/// How often the rank, the cache figures and the pending-ask count are
/// refreshed. Slow on purpose: none of them is an interaction-rate fact.
const REFRESH: Duration = Duration::from_secs(5);

/// The redraw tick. Fast enough that a stream looks live, slow enough that a
/// terminal over ssh is not the bottleneck.
const TICK: Duration = Duration::from_millis(80);

/// How long the key reader blocks on one poll before releasing the terminal
/// lock. It bounds how long a redraw waits for the lock, so it is short.
const KEY_POLL: Duration = Duration::from_millis(20);

/// One feed delivery, tagged with the context it belongs to — several
/// contexts are watched at once and they share one loop.
type TaggedFeed = (ContextId, FeedEvent);

/// crossterm's internal event reader is one shared resource, and a blocking
/// read holds it. `Terminal`'s cursor-position query — the inline viewport's
/// resize path — needs the same reader, and a query issued while a read is
/// blocked times out with "the cursor position could not be read within a
/// normal duration". One mutex arbitrates: the key reader takes it for the
/// length of one [`KEY_POLL`], and every terminal operation takes it too.
type TermLock = Arc<Mutex<()>>;

/// The loop's wake sources, bundled so the event loop takes one handle
/// instead of a parameter per channel.
struct Wires {
    term_lock: TermLock,
    key_rx: mpsc::Receiver<Event>,
    feed_rx: mpsc::Receiver<TaggedFeed>,
    /// Kept alongside the receiver: a context switch watches a new context,
    /// which needs a sender to clone for its forwarder task.
    feed_tx: mpsc::Sender<TaggedFeed>,
    server_events: tokio::sync::broadcast::Receiver<ServerEvent>,
}

/// Run the client until it quits, restoring the terminal on the way out.
pub async fn run(bridge: KernelBridge, start: ContextInfo, identity: String) -> Result<()> {
    let mut app = App::new(identity);
    app.set_contexts(bridge.list_contexts().await?);

    // Subscribed before the first hydrate, not inside the loop: the actor's
    // kernel-wide event bus drops an event it has no receiver for, and the
    // hydrate itself produces a burst of them.
    let server_events = bridge.actor().subscribe_events();

    let (feed_tx, feed_rx) = mpsc::channel::<TaggedFeed>(256);
    watch_context(&bridge, &mut app, start.id, &feed_tx).await?;
    app.switch_to(start.id);

    // The inline viewport's first cursor query runs before the key reader
    // exists, so nothing is holding the reader it needs.
    let mut terminal = enter_terminal().context("enter the inline viewport")?;
    let term_lock: TermLock = Arc::new(Mutex::new(()));
    let (key_tx, key_rx) = mpsc::channel::<Event>(64);
    spawn_key_reader(term_lock.clone(), key_tx);

    let mut wires = Wires {
        term_lock: term_lock.clone(),
        key_rx,
        feed_rx,
        feed_tx,
        server_events,
    };
    let result = event_loop(&bridge, &mut app, &mut terminal, &mut wires).await;
    leave_terminal(&mut terminal, &term_lock);
    result
}

/// Read keys on a blocking thread, one bounded poll at a time.
///
/// A dedicated thread rather than `crossterm::event::EventStream`: the stream
/// polls with no timeout, which holds crossterm's internal reader forever and
/// starves the cursor-position query the inline viewport makes on resize.
fn spawn_key_reader(term_lock: TermLock, tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        loop {
            let ready = {
                let _guard = term_lock.lock();
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
}

/// Subscribe to a context's feed, hydrate its mirror, and forward its
/// deliveries into the shared loop channel.
async fn watch_context(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    feed_tx: &mpsc::Sender<TaggedFeed>,
) -> Result<()> {
    if app.views.contains_key(&context_id) {
        return Ok(());
    }
    let (mirror, mut rx) = bridge.hydrate_context(context_id).await?;
    app.views.insert(context_id, ContextView::new(mirror));

    let tx = feed_tx.clone();
    // `spawn_local`: the actor's Cap'n Proto types are `!Send`, so this task
    // belongs to the caller's `LocalSet`.
    tokio::task::spawn_local(async move {
        while let Some(event) = rx.recv().await {
            if tx.send((context_id, event)).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

async fn event_loop(
    bridge: &KernelBridge,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    wires: &mut Wires,
) -> Result<()> {
    let mut keys = Keys::new();
    let mut status = bridge.actor().watch_status();
    // Ask polling is driven by the ledger's own change stream, not by the
    // refresh timer: `poll_new_asks` runs `kj ledger list` in the context,
    // which authors a ToolCall/ToolResult pair — on a timer that fills the
    // transcript with the client's own bookkeeping.
    let mut ledger_events = bridge.actor().subscribe_ledger_events();
    let mut seen_asks = std::collections::HashSet::new();
    let mut poll_asks = true;
    let mut refresh = tokio::time::interval(REFRESH);
    let mut tick = tokio::time::interval(TICK);
    let mut dirty = true;

    app.connection = Some(bridge.actor().current_status());

    while !app.quit {
        tokio::select! {
            Some(event) = wires.key_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        dirty = true;
                        act(bridge, app, &mut keys, key, &wires.feed_tx).await?;
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
            Some((context_id, event)) = wires.feed_rx.recv() => {
                dirty = true;
                apply_feed(bridge, app, context_id, event).await;
            }
            Ok(event) = wires.server_events.recv() => {
                if mark_activity(app, &event) {
                    dirty = true;
                }
            }
            Ok(()) = status.changed() => {
                app.connection = Some(status.borrow().clone());
                dirty = true;
            }
            Ok(_generation) = ledger_events.recv() => {
                poll_asks = true;
            }
            _ = refresh.tick() => {
                if let Ok(contexts) = bridge.list_contexts().await {
                    app.set_contexts(contexts);
                }
                if poll_asks
                    && let Some(ctx) = app.current
                {
                    poll_asks = false;
                    if let Ok(new_asks) =
                        kaijutsu_client::poll_new_asks(bridge.actor(), ctx, &mut seen_asks).await
                    {
                        for ask in &new_asks {
                            seen_asks.insert(ask.request_id.clone());
                        }
                        app.pending_asks = seen_asks.len();
                    }
                }
                dirty = true;
            }
            _ = tick.tick() => {
                if dirty {
                    dirty = false;
                    draw(terminal, &wires.term_lock, app, keys.armed())?;
                }
            }
        }
    }
    Ok(())
}

/// One frame: print what completed into scrollback, then redraw the live
/// region.
fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    app: &mut App,
    armed: bool,
) -> Result<()> {
    let width = terminal.size()?.width;
    let prints = render::take_settled_prints(app, width);
    let _guard = term_lock.lock();
    render::print_scrollback(terminal, &prints)?;
    render::draw_live(terminal, app, kaijutsu_types::now_millis(), armed)?;
    Ok(())
}

/// Act on one key.
async fn act(
    bridge: &KernelBridge,
    app: &mut App,
    keys: &mut Keys,
    key: crossterm::event::KeyEvent,
    feed_tx: &mpsc::Sender<TaggedFeed>,
) -> Result<()> {
    match keys.interpret(key) {
        Intent::Ignored | Intent::LegendChanged => {}
        Intent::Interrupt => {
            app.press_ctrl_c(std::time::Instant::now());
        }
        Intent::ComposeInsert(c) => {
            app.clear_notice();
            app.compose.push(c);
        }
        Intent::ComposeBackspace => {
            app.clear_notice();
            app.compose.pop();
        }
        Intent::Submit => {
            let text = std::mem::take(&mut app.compose);
            if text.trim().is_empty() {
                return Ok(());
            }
            let Some(ctx) = app.current else {
                app.note("no context attached");
                return Ok(());
            };
            // The kernel-owned input surface (`edit_input` / `submit_input`),
            // reached through the bridge. The modalkit compose lane replaces
            // the whole-line rewrite, not this call.
            match bridge.send_prompt(ctx, &text).await {
                Ok(_) => app.clear_notice(),
                Err(e) => app.note(format!("submit failed: {e}")),
            }
        }
        Intent::SwitchSeat(n) => match app.seat_context(n) {
            Some(id) => {
                watch_context(bridge, app, id, feed_tx).await?;
                app.switch_to(id);
                app.clear_notice();
            }
            None => app.note(format!("no context on seat {n}")),
        },
        Intent::LastContext => {
            if app.switch_to_previous().is_none() {
                app.note("no previous context");
            }
        }
        Intent::NotYet(message) => app.note(message),
    }
    Ok(())
}

/// Apply one feed event to its mirror, and say so when it touches a block
/// already printed to scrollback.
async fn apply_feed(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    event: FeedEvent,
) {
    match event {
        FeedEvent::Changed(delivery) => {
            for change in delivery.changes() {
                app.observe_change(context_id, change);
            }
            if let Some(view) = app.views.get_mut(&context_id) {
                if let Err(e) = view.mirror.receive(delivery) {
                    tracing::warn!(context = %context_id.short(), error = %e, "mirror rejected a delivery");
                }
                view.seed_collapse();
            }
        }
        FeedEvent::Resubscribed => {
            // The actor already re-subscribed on this receiver's behalf;
            // nothing published during the outage rides it. Throw the mirror
            // away and rehydrate on the same receiver.
            match bridge.rehydrate_context(context_id).await {
                Ok(fresh) => {
                    if let Some(view) = app.views.get_mut(&context_id) {
                        view.mirror = fresh;
                        view.seed_collapse();
                    }
                    app.note("reconnected; context rehydrated");
                }
                Err(e) => app.note(format!("rehydrate failed: {e}")),
            }
        }
        FeedEvent::Terminated { reason, .. } => {
            // The subscriber fell behind and this receiver is dead. The
            // forwarder task ends with it; drop the view so the next switch
            // re-subscribes from scratch.
            app.views.remove(&context_id);
            app.note(format!("context feed ended ({reason:?}); press Ctrl+A to reattach"));
        }
    }
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

fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(render::VIEWPORT_LINES),
        },
    )
}

/// Leave the terminal the way we found it. Best-effort on every step: a
/// failure here must not mask the error that ended the loop.
///
/// The viewport is not cleared — `Terminal::clear` queries the cursor, and
/// leaving the last frame in place is what the inline viewport is for
/// anyway: the transcript stays in scrollback and the host shell's prompt
/// appears under it.
fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>, term_lock: &TermLock) {
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    let _ = disable_raw_mode();
    println!();
}
