//! The event loop: crossterm keys, context feeds, kernel events and a redraw
//! tick as `tokio::select!` arms.
//!
//! The loop coalesces: any arm that changes state marks the frame dirty and
//! the tick draws once. A burst of streaming appends therefore costs one
//! redraw, not one per token.

use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::Event;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use kaijutsu_audio::RefDisposition;
use kaijutsu_client::{ContextInfo, FeedEvent, ServerEvent};
use parking_lot::Mutex;
use kaijutsu_types::ContextId;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use crate::app::{App, ContextView};
use crate::asks;
use crate::bridge::KernelBridge;
use crate::cmdline::{self, ColonVerb};
use crate::compose::Compose;
use crate::completion;
use crate::interrupt::{self, Step as InterruptStep};
use crate::keys::{Intent, Keys};
use crate::picker::{self, Outcome as PickerOutcome};
use crate::render;

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
    // Best-effort: a kernel that cannot answer this yet just means slash
    // completion offers nothing until a later poll fills it in, never a
    // failure to start.
    app.kj_catalog = bridge.kj_command_catalog(start.id).await.unwrap_or_default();

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
    let mut interrupt_ladder = interrupt::Ladder::new();
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
    // The inline viewport's current height (`Viewport::Inline` has no public
    // runtime resize — see `set_viewport_height`) and the beat-driven redraw
    // wake, both `None`/base until a track is found playing.
    let mut viewport_height = render::VIEWPORT_LINES;
    let mut beat_wake: Option<Instant> = None;
    let mut beat_tempo_bps: f64 = 0.0;

    app.connection = Some(bridge.actor().current_status());

    while !app.quit {
        tokio::select! {
            Some(event) = wires.key_rx.recv() => {
                match event {
                    Event::Key(key) => {
                        dirty = true;
                        if app.picker.is_some() {
                            handle_picker_key(bridge, app, key, &wires.feed_tx).await?;
                        } else if act(bridge, app, &mut keys, &mut interrupt_ladder, key, &wires.feed_tx).await? == Acted::Suspend {
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
                if collapse_thinking_on_turn_end(app, &event) {
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
                    observe_beat_sync(app, *context_id, *beat_ref);
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
                            app.note_ask(ask.request_id.clone(), ask.info.context_id);
                            // Auto-show only the current context's own ask,
                            // never a modal for a seat you are not looking
                            // at (`docs/tui.md`, "Asks") — those surface as
                            // the seat's `!` and the status line's `!n`.
                            if app.ask_card.is_none() && app.current == Some(ask.info.context_id) {
                                open_ask_card(bridge, app, &ask.request_id, ask.info.context_id).await;
                            }
                        }
                        app.forget_asks_not_pending(&seen_asks);
                        app.pending_asks = seen_asks.len();
                    }
                }
                if let Ok(tracks) = bridge.actor().list_tracks().await {
                    app.tracks = tracks.iter().map(picker::track_row_from).collect();
                }
                rearm_beat_wake(app, &mut beat_wake, &mut beat_tempo_bps);
                dirty = true;
            }
            // The beat-driven redraw: armed at the playing track's predicted
            // next onset, re-armed at `scheduled + period` inside the arm
            // body — never `actual_wake + period` (`docs/tui.md`, "Timing to
            // music"; `docs/midi.md`, "The one timebase"). The `if` guard
            // skips this arm entirely while nothing is playing, so it never
            // busy-polls a zero sleep.
            _ = tokio::time::sleep(beat_wake.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default()), if beat_wake.is_some() => {
                if let Some(scheduled) = beat_wake {
                    beat_wake = Some(picker::rearm(scheduled, beat_tempo_bps));
                }
                dirty = true;
            }
            _ = tick.tick() => {
                let want = render::viewport_lines(app, terminal.size()?.width);
                if want != viewport_height {
                    set_viewport_height(&wires.term_lock, terminal, want)?;
                    viewport_height = want;
                }
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

/// Fold one `ServerEvent::BeatSync` into `app.beats`, the same
/// fold/touch/drop routing as the app's `time_well::live::ingest_live_events`
/// (`docs/tui.md`, "TRACKS + beat").
fn observe_beat_sync(app: &mut App, context_id: ContextId, beat_ref: kaijutsu_audio::BeatRef) {
    let now_inst = Instant::now();
    let now_epoch_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    match beat_ref.disposition(now_inst, now_epoch_ns) {
        RefDisposition::Fold(at) => {
            app.beats.observe(context_id, beat_ref, at, now_inst);
        }
        RefDisposition::Touch | RefDisposition::Drop => {
            app.beats.touch(&context_id, now_inst);
        }
    }
}

/// Re-arm the beat timer from the playing track's live phasor position and
/// its last-polled tempo. `None` while nothing is playing or its phasor
/// hasn't anchored yet — the timer arm's `if beat_wake.is_some()` guard then
/// simply stays off until the next refresh finds one.
fn rearm_beat_wake(app: &App, beat_wake: &mut Option<Instant>, beat_tempo_bps: &mut f64) {
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

/// Recreate the inline viewport at a new height. `Viewport::Inline`'s height
/// is fixed at construction (`ratatui-core` exposes no runtime setter — see
/// `terminal/resize.rs`'s `resize()`, which only recomputes the ORIGIN from
/// the height already stored on `Terminal::with_options`), so a grown/shrunk
/// view re-enters the inline viewport at the new height, anchored to the
/// cursor's current row exactly as the first `enter_terminal` call was.
fn set_viewport_height(
    term_lock: &TermLock,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    height: u16,
) -> Result<()> {
    let _guard = term_lock.lock();
    // A freshly constructed `Terminal` has no memory of what the OLD one
    // painted, so it diffs against an empty buffer and never emits the
    // blanks needed to erase what is still on screen. Blank the current
    // viewport through the OLD terminal (which still has last frame's
    // buffer to diff against) before swapping it out, or a shrink/regrow
    // leaves stale rows behind.
    terminal
        .draw(|frame| frame.render_widget(ratatui::widgets::Clear, frame.area()))
        .context("clear the viewport before resizing it")?;
    let _ = terminal.flush();
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions { viewport: Viewport::Inline(height) },
    )
    .context("resize inline viewport")?;
    Ok(())
}

/// Route one key to the open picker, then act on its [`PickerOutcome`].
async fn handle_picker_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
    feed_tx: &mpsc::Sender<TaggedFeed>,
) -> Result<()> {
    let Some(picker) = app.picker.as_mut() else {
        return Ok(());
    };
    match picker.handle_key(key) {
        PickerOutcome::None => {}
        PickerOutcome::Dismiss => app.picker = None,
        PickerOutcome::Switch(id) => {
            watch_context(bridge, app, id, feed_tx).await?;
            app.switch_to(id);
            app.picker = None;
            app.clear_notice();
        }
        PickerOutcome::Placement { context_id, argv } => match bridge.execute_kj(context_id, argv).await {
            Ok(result) if result.latch.is_some() => {
                let message = result.latch.map(|l| l.message).unwrap_or_default();
                app.note(message);
            }
            Ok(result) => app.note(result.stdout.lines().next().unwrap_or("done").to_string()),
            Err(e) => app.note(format!("placement failed: {e}")),
        },
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
    // The status line's live pulse dot — the one place this crate samples
    // the phasor's envelope against a real clock; `App::track_figure` only
    // projects the value stamped here.
    app.track_pulse = app
        .playing_track()
        .is_some_and(|t| app.beats.envelope(&t.score_context_id, Instant::now()) > 0.5);
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
    interrupt_ladder: &mut interrupt::Ladder,
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
    // The ask card and the ledger view capture every key while open — their
    // own a/A/d/v/j/k/Esc keys are never compose text or a `Ctrl+A` chord
    // (`docs/tui.md`, "Asks" / "The ledger").
    if app.ask_card.is_some() {
        if let Some(decision) = asks::ask_key_to_decision(key) {
            let card = app.ask_card.take().expect("checked Some above");
            handle_ask_decision(bridge, app, card, decision).await;
        }
        return Ok(Acted::Continue);
    }
    if app.ledger_view.is_some() {
        handle_ledger_key(bridge, app, key).await;
        return Ok(Acted::Continue);
    }

    match keys.interpret(key) {
        Intent::Ignored | Intent::LegendChanged => {}
        Intent::Interrupt => {
            interrupt_ctrl_c(bridge, app, interrupt_ladder).await;
        }
        Intent::InputKey(key) => {
            compose_key(bridge, app, key).await?;
        }
        Intent::Suspend => {
            return Ok(Acted::Suspend);
        }
        Intent::SwitchSeat(n) => match app.seat_context(n) {
            Some(id) => {
                watch_context(bridge, app, id, feed_tx).await?;
                app.switch_to(id);
                // The draft is per context, so the compose buffer follows the
                // switch rather than carrying the old context's text along —
                // `load_draft` keeps the `:` line's own history, which is
                // this session's, not this draft's.
                app.compose.load_draft(&bridge.read_input(id).await.unwrap_or_default());
                app.clear_notice();
            }
            None => app.note(format!("no context on seat {n}")),
        },
        Intent::LastContext => match app.switch_to_previous() {
            Some(id) => {
                app.compose.load_draft(&bridge.read_input(id).await.unwrap_or_default());
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
        Intent::OpenLedger => open_ledger(bridge, app).await,
        Intent::Tab => {
            if app.compose.kj_typed().is_some() {
                apply_kj_completion(app);
            } else {
                let tab = crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Tab);
                compose_key(bridge, app, tab).await?;
            }
        }
        Intent::TogglePicker => {
            let tracks = bridge.actor().list_tracks().await.unwrap_or_default();
            app.picker = Some(crate::picker::PickerModel::build(
                &app.contexts,
                &tracks,
                &app.views.iter().filter(|(_, v)| v.activity).map(|(id, _)| *id).collect(),
                &app.tails,
                kaijutsu_types::now_millis(),
            ));
        }
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
        app.note("compose unfocused — i to type, : for a command");
    }
    if let Some(line) = action.command {
        handle_colon_line(bridge, app, ctx, line).await;
        return Ok(());
    }
    if action.submit {
        if app.compose.text().trim().is_empty() {
            return Ok(());
        }
        match bridge.submit_input(ctx).await {
            Ok(_) => {
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
            Ok(result) => app.note(result.stdout.lines().next().unwrap_or("done").to_string()),
            Err(e) => app.note(format!(":kj failed: {e}")),
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
/// submit) and cleared on `TurnCompleted`/`TurnFailed`, the same two events
/// [`collapse_thinking_on_turn_end`] already matches.
fn mark_turn_liveness(app: &mut App, event: &ServerEvent) -> bool {
    match event {
        ServerEvent::TurnStarted { context_id, .. } => app.mark_turn_running(*context_id),
        ServerEvent::TurnCompleted { context_id, .. } | ServerEvent::TurnFailed { context_id, .. } => {
            app.mark_turn_ended(*context_id)
        }
        _ => false,
    }
}

/// Hand the terminal back to the host shell: leave raw mode, stop ourselves
/// the way a shell job does, and re-anchor the inline viewport when `SIGCONT`
/// brings us back. The transcript stays in scrollback either way, so nothing
/// else needs restoring (`docs/tui.md`, "The `:` line": `Ctrl+Z` is a single
/// suspend now, not a toggle).
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
    let pending_ids = kaijutsu_client::list_pending(bridge.actor(), ctx).await.unwrap_or_default();
    let history_ids = kaijutsu_client::list_history(bridge.actor(), ctx).await.unwrap_or_default();

    let mut rows = Vec::with_capacity(pending_ids.len() + history_ids.len());
    for id in pending_ids {
        if let Ok(Some(detail)) = kaijutsu_client::show_ask_detail(bridge.actor(), ctx, &id).await {
            rows.push(asks::LedgerRow::Pending(pending_row(app, &detail)));
        }
    }
    for id in history_ids {
        if let Ok(Some(detail)) = kaijutsu_client::show_ask_detail(bridge.actor(), ctx, &id).await {
            rows.push(asks::LedgerRow::Answered(answered_row(app, &detail)));
        }
    }
    app.ledger_view = Some(asks::LedgerViewState { rows, filter: String::new(), selected: 0, filtering: false });
}

/// One `AskDetail` as the ledger view's PENDING row.
fn pending_row(app: &App, detail: &kaijutsu_client::AskDetail) -> asks::PendingRow {
    let (context_label, context_type) = detail
        .context_id
        .map(|ctx| asks::context_facts(app, ctx))
        .unwrap_or_else(|| ("(unknown)".to_string(), "default".to_string()));
    asks::PendingRow {
        request_id: detail.request_id.clone(),
        // `created_at` is not on `kj ledger show`'s `.data` yet
        // (`kaijutsu_client::AskDetail`'s doc names the gap).
        age: None,
        context_label,
        context_type,
        hook: detail.tool.clone().unwrap_or_else(|| "-".to_string()),
        statement: detail.statements.first().cloned().unwrap_or_else(|| detail.description.clone()),
    }
}

/// One `AskDetail` as the ledger view's ANSWERED row. `decision`/`time`/
/// `principal` are only as precise as the wire is today: `status` alone
/// (`allowed`/`denied`) stands in for `decided_option`
/// (`allow_once`/`allow_always`/`deny`), and `time`/`principal` stay `None`
/// — `kaijutsu_client::AskDetail`'s doc names the same gap.
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
        time: None,
        context_label,
        decision: Some(detail.status.clone()),
        principal: None,
        redeemed,
        statement: detail.statements.first().cloned().unwrap_or_else(|| detail.description.clone()),
    }
}

/// Fetch one ask's full detail and open the ask card over it. Silent on
/// failure — the poll loop tries again next generation bump
/// (`ledger_events`), and a card that never opens still shows as the seat's
/// `!` and the status line's `!n`.
async fn open_ask_card(bridge: &KernelBridge, app: &mut App, request_id: &str, context_id: ContextId) {
    if let Ok(Some(detail)) = kaijutsu_client::show_ask_detail(bridge.actor(), context_id, request_id).await {
        app.ask_card = Some(asks::AskCardState {
            request_id: request_id.to_string(),
            context_id,
            detail,
        });
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
    let allow = !matches!(decision, asks::AskDecision::Deny);
    let remember = matches!(decision, asks::AskDecision::AllowAlways)
        .then_some(kaijutsu_client::RememberScope::Always);
    report_decision(
        app,
        &card.request_id,
        allow,
        kaijutsu_client::decide_ask_remember(bridge.actor(), card.context_id, &card.request_id, allow, remember)
            .await,
    );
}

/// One key inside the ledger view: navigate, filter, show, or answer the
/// selected row. Closes the view after any decision — the next
/// `ledger_events` generation bump refreshes the seat flags and the pending
/// count; re-fetching and re-selecting inline is a follow-up, not this
/// pass's scope.
async fn handle_ledger_key(bridge: &KernelBridge, app: &mut App, key: crossterm::event::KeyEvent) {
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
            // Full-detail render (`render_ask_detail`) needs its own grown
            // view to host it; wiring that is a follow-up, not this pass.
            if let Some(id) = view.selected_request_id() {
                app.note(format!("ask {id}: full detail view not yet wired"));
            }
        }
        asks::LedgerAction::AllowOnce | asks::LedgerAction::AllowAlways | asks::LedgerAction::Deny => {
            let Some(request_id) = view.selected_request_id() else { return };
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

    #[test]
    fn an_unrelated_event_carries_no_turn_liveness_either() {
        let id = ContextId::new();
        let mut app = App::new("amy");
        let event = ServerEvent::ContextSwitched { context_id: id };
        assert!(!mark_turn_liveness(&mut app, &event));
    }
}
