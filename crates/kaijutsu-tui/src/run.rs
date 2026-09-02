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
use crate::compose::Compose;
use crate::keys::{Intent, Keys};
use crate::render;
use crate::shell::{CtrlZ, ShellAction};

use crossterm::event::KeyEvent;
use kaijutsu_client::{PeerConfig, PeerInvocation};

use crate::diff::{self, DiffKey};
use crate::editor::{self, AltScreen, EditorOpen, ScreenMode};

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

    // Subscribed before the first hydrate, not inside the loop: the actor's
    // kernel-wide event bus drops an event it has no receiver for, and the
    // hydrate itself produces a burst of them.
    let server_events = bridge.actor().subscribe_events();

    let (feed_tx, feed_rx) = mpsc::channel::<TaggedFeed>(256);
    watch_context(&bridge, &mut app, start.id, &feed_tx).await?;
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
    app.principal = Some(bridge.principal().await?);
    app.compose = Compose::over(&bridge.read_input(start.id).await.unwrap_or_default());

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
        editor_opens: open_rx,
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
    // The alternate screen, when one is up. `draw` owns the transition: the
    // screen mode is state on `App`, and the terminal follows it.
    let mut alt: Option<AltScreen> = None;

    app.connection = Some(bridge.actor().current_status());

    while !app.quit {
        tokio::select! {
            Some(event) = wires.key_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        dirty = true;
                        if act(bridge, app, &mut keys, key, &wires.feed_tx).await? == Acted::Suspend {
                            suspend(terminal, &wires.term_lock)?;
                        }
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
                if collapse_thinking_on_turn_end(app, &event) {
                    dirty = true;
                }
                if observe_editor_event(bridge, app, &event).await {
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
                app.connection = Some(connection);
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
                    draw(terminal, &mut alt, &wires.term_lock, app, keys.armed())?;
                }
            }
        }
    }
    if let Some(screen) = alt.take() {
        let _guard = wires.term_lock.lock();
        editor::leave(screen);
    }
    Ok(())
}

/// One frame: print what completed into scrollback, then redraw the live
/// region.
fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    alt: &mut Option<AltScreen>,
    term_lock: &TermLock,
    app: &mut App,
    armed: bool,
) -> Result<()> {
    // The alternate screen is a mode of the whole client, and this is the one
    // place the terminal follows it. Taking and giving it back here — rather
    // than at every site that opens or closes a surface — is what keeps the
    // inline viewport's transcript in scrollback untouched (`docs/tui.md`,
    // ruling 1).
    if app.screen.is_alternate() {
        let _guard = term_lock.lock();
        if alt.is_none() {
            *alt = Some(editor::enter().context("take the alternate screen")?);
        }
        let screen = alt.as_mut().expect("just entered");
        return draw_alternate(screen, app);
    }
    if let Some(screen) = alt.take() {
        let _guard = term_lock.lock();
        editor::leave(screen);
    }

    let width = terminal.size()?.width;
    let prints = render::take_settled_prints(app, width);
    let _guard = term_lock.lock();
    render::print_scrollback(terminal, &prints)?;
    render::draw_live(terminal, app, kaijutsu_types::now_millis(), armed)?;
    Ok(())
}

/// One frame of whichever surface holds the alternate screen.
fn draw_alternate(alt: &mut AltScreen, app: &mut App) -> Result<()> {
    let size = alt.size()?;
    let palette = app.palette;
    match &mut app.screen {
        ScreenMode::Editor(screen) => {
            let frame = editor::editor_frame(screen, size.width, size.height, &palette);
            alt.draw(frame.lines, Some(frame.cursor))?;
        }
        ScreenMode::Diff(screen) => {
            let lines = screen.frame(size.height, &palette);
            alt.draw(lines, None)?;
        }
        // Unreachable: the caller checked. Drawing nothing beats a panic in a
        // frame path.
        ScreenMode::Inline => {}
    }
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
    key: crossterm::event::KeyEvent,
    feed_tx: &mpsc::Sender<TaggedFeed>,
) -> Result<Acted> {
    // The editor is the sanctioned raw key reader (`docs/input.md`): while a
    // vi surface holds the alternate screen every key belongs to it, so the
    // `Ctrl+A` prefix and the `Ctrl+C` double-tap are bypassed here rather
    // than being taught to stand aside.
    if editor::route_key(app) == editor::KeyRoute::AlternateScreen {
        act_alternate(bridge, app, key).await?;
        return Ok(Acted::Continue);
    }
    match keys.interpret(key) {
        Intent::Ignored | Intent::LegendChanged => {}
        Intent::Interrupt => {
            app.press_ctrl_c(std::time::Instant::now());
        }
        Intent::InputKey(key) => {
            if app.shell.active {
                shell_key(bridge, app, key).await?;
            } else {
                compose_key(bridge, app, key).await?;
            }
        }
        Intent::ShellToggle => {
            let gesture = app.shell.press_ctrl_z(std::time::Instant::now());
            if app.shell.active {
                // Both prompt cursors are resolved live, so the cwd is read
                // when the surface comes up rather than cached from startup.
                // Read before the suspend, so the frame `fg` returns to is
                // the frame that was there.
                if let Some(ctx) = app.current {
                    app.shell.set_cwd(bridge.context_cwd(ctx).await.unwrap_or(None));
                }
                app.note("shell surface — Ctrl+Z leaves, Ctrl+Z Ctrl+Z suspends");
            } else {
                app.clear_notice();
            }
            if gesture == CtrlZ::Suspend {
                return Ok(Acted::Suspend);
            }
        }
        Intent::SwitchSeat(n) => match app.seat_context(n) {
            Some(id) => {
                watch_context(bridge, app, id, feed_tx).await?;
                app.switch_to(id);
                // The draft is per context, so the compose buffer follows the
                // switch rather than carrying the old context's text along.
                app.compose = Compose::over(&bridge.read_input(id).await.unwrap_or_default());
                app.shell.set_cwd(bridge.context_cwd(id).await.unwrap_or(None));
                app.clear_notice();
            }
            None => app.note(format!("no context on seat {n}")),
        },
        Intent::LastContext => match app.switch_to_previous() {
            Some(id) => {
                app.compose = Compose::over(&bridge.read_input(id).await.unwrap_or_default());
                app.shell.set_cwd(bridge.context_cwd(id).await.unwrap_or(None));
            }
            None => app.note("no previous context"),
        },
        Intent::OpenDiff => match open_diff(app) {
            Some(screen) => {
                app.screen = ScreenMode::Diff(screen);
                app.clear_notice();
            }
            None => app.note("no diff block in this context"),
        },
        Intent::NotYet(message) => app.note(message),
    }
    Ok(Acted::Continue)
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
    let action = app.compose.press(key, std::time::Instant::now());
    for op in &action.ops {
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
    if action.unfocus {
        app.note("compose unfocused — i to type");
    }
    if action.submit {
        if app.compose.text().trim().is_empty() {
            return Ok(());
        }
        match bridge.submit_input(ctx).await {
            Ok(_) => {
                app.compose.reset();
                app.clear_notice();
            }
            Err(e) => app.note(format!("submit failed: {e}")),
        }
    }
    Ok(())
}

/// One keystroke on the shell surface. A line runs through `shell_execute`,
/// the gated human path; its output arrives as blocks on the context feed and
/// the transcript prints it, so nothing is echoed here.
async fn shell_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
) -> Result<()> {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return Ok(());
    };
    if let ShellAction::Run(line) = app.shell.press(key) {
        match bridge.shell_execute(ctx, &line).await {
            Ok(_) => app.clear_notice(),
            Err(e) => app.note(format!("shell failed: {e}")),
        }
        // `cd` moves one of the two cursors, so re-read it after every line
        // rather than modeling kaish's own state here.
        app.shell.set_cwd(bridge.context_cwd(ctx).await.unwrap_or(None));
    }
    Ok(())
}

/// Hand the terminal back to the host shell: leave raw mode, stop ourselves
/// the way a shell job does, and re-anchor the inline viewport when `SIGCONT`
/// brings us back. The transcript stays in scrollback either way, so nothing
/// else needs restoring (`docs/tui.md`, "Shell (`Ctrl+Z`)").
fn suspend(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
) -> Result<()> {
    // Held across the stop so the key reader cannot read in cooked mode, and
    // so the cursor query on the way back has crossterm's reader to itself.
    let _guard = term_lock.lock();
    let _ = terminal.flush();
    let _ = disable_raw_mode();
    println!();

    raise_stop();

    // Back from SIGCONT. The host shell moved the cursor, so the viewport is
    // re-anchored where the cursor is now rather than where it used to be.
    enable_raw_mode()?;
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
                app.apply_collapse_change(context_id, change);
            }
            if let Some(view) = app.views.get_mut(&context_id) {
                if let Err(e) = view.mirror.receive(delivery) {
                    tracing::warn!(context = %context_id.short(), error = %e, "mirror rejected a delivery");
                }
                view.seed_collapse();
            }
            reconcile_draft(app, context_id);
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
                    reconcile_draft(app, context_id);
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

// ────────────────────────────────────────────────────────────────────────────
// The alternate screen: editor and diff (docs/tui.md, "Editor and diff")
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
/// mode, so it alone decides a quit — and it is what gives the inline viewport
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

/// A key while the alternate screen is up.
async fn act_alternate(bridge: &KernelBridge, app: &mut App, key: KeyEvent) -> Result<()> {
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
                    // inline viewport back instead of freezing.
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
            app.screen = ScreenMode::Inline;
        }
    }
    Ok(())
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

/// Collapse a context's `Thinking` blocks when its turn ends.
///
/// `ServerEvent::TurnCompleted`/`TurnFailed` are the signal, not any one
/// block's status — the push that replaces inferring completion from block
/// status (`kaijutsu_client::subscriptions`, `ServerEvent::TurnCompleted`
/// doc). Runs whether or not the context is on screen, so a background
/// turn's reasoning is already collapsed by the time you switch to it —
/// unlike [`mark_activity`], which deliberately skips the current context.
fn collapse_thinking_on_turn_end(app: &mut App, event: &ServerEvent) -> bool {
    let context_id = match event {
        ServerEvent::TurnCompleted { context_id, .. } | ServerEvent::TurnFailed { context_id, .. } => {
            *context_id
        }
        _ => return false,
    };
    app.views
        .get_mut(&context_id)
        .is_some_and(ContextView::collapse_thinking)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_client::{ContextMirror, TurnCompletedStopReason, TurnOrigin};
    use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, PrincipalId, Role};

    fn thinking_block(context: ContextId) -> kaijutsu_types::BlockSnapshot {
        BlockSnapshotBuilder::new(BlockId::new(context, PrincipalId::new(), 1), BlockKind::Thinking)
            .role(Role::Model)
            .content("considering")
            .build()
    }

    /// The turn's own end collapses a context's live `Thinking` block, even
    /// for a context that is not the one on screen right now — a background
    /// turn's reasoning is already tidy by the time you switch to it.
    #[test]
    fn turn_completed_collapses_a_background_contexts_thinking_block() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(vec![thinking_block(id)], 1)
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        app.current = None; // not the context on screen

        let event = ServerEvent::TurnCompleted {
            context_id: id,
            principal_id: PrincipalId::new(),
            output_block_id: None,
            stop_reason: TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert!(collapse_thinking_on_turn_end(&mut app, &event));
        assert!(app.views[&id].collapsed.values().all(|c| *c));
    }

    /// A `TurnFailed` also ends the turn — the reasoning that led to the
    /// failure is no more live than a successful one's.
    #[test]
    fn turn_failed_also_collapses_thinking() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let mut mirror = ContextMirror::new(id);
        mirror
            .apply_snapshot(vec![thinking_block(id)], 1)
            .expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));

        let event = ServerEvent::TurnFailed {
            context_id: id,
            principal_id: PrincipalId::new(),
            error: "provider stream error".to_string(),
            origin: TurnOrigin::Autonomous,
        };
        assert!(collapse_thinking_on_turn_end(&mut app, &event));
    }

    /// An event for a context nobody is watching is not a bug — `false`,
    /// not a panic.
    #[test]
    fn turn_completed_for_an_unwatched_context_is_a_no_op() {
        let mut app = App::new("amy");
        let event = ServerEvent::TurnCompleted {
            context_id: ContextId::new(),
            principal_id: PrincipalId::new(),
            output_block_id: None,
            stop_reason: TurnCompletedStopReason::EndTurn,
            origin: TurnOrigin::Interactive,
        };
        assert!(!collapse_thinking_on_turn_end(&mut app, &event));
    }

    /// Every other `ServerEvent` variant is `mark_activity`'s business, not
    /// this function's.
    #[test]
    fn an_unrelated_event_is_ignored() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        app.views.insert(id, ContextView::new(ContextMirror::new(id)));
        let event = ServerEvent::ContextSwitched { context_id: id };
        assert!(!collapse_thinking_on_turn_end(&mut app, &event));
    }
}
