//! The event loop: crossterm keys, context feeds, kernel events and a redraw
//! tick as `tokio::select!` arms.
//!
//! The loop coalesces: any arm that changes state marks the frame dirty and
//! the tick draws once. A burst of streaming appends therefore costs one
//! redraw, not one per token.

mod act;
mod arms;
mod asks_io;
mod draw;
mod feeds;
mod probe;
mod seat;
mod signals;
mod surfaces;
mod terminal;

pub use terminal::restore_terminal;

use std::io::Stdout;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::Event;
use kaijutsu_client::{ContextInfo, ServerEvent};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::App;
use crate::bridge::KernelBridge;
use crate::compose::Compose;
use kaijutsu_client::draft_outbox::{self, Landed};
use kaijutsu_client::editor_outbox;
use crate::picker;
use crate::refresh;

use crate::editor::{self, EditorOpen};

use self::act::KeyState;
use self::asks_io::AskLanded;
use self::arms::{join_refresh, on_ask_landed, on_hydrated, on_input, on_ledger_changed, on_refresh_joined, on_server_event, on_status_changed, on_tick};
use self::draw::{Beat, Presented, present};
use self::feeds::{Feeds, Hydrated, TaggedFeed, apply_feed, watch_context};
use self::probe::{LOOP_THREAD, ProbePanic, TASK_PANIC, panic_unwinds_the_loop};
use self::seat::{land_draft_edit, remember_view};
use self::signals::StopSignals;
use self::surfaces::{attach_editor_peer, land_editor_answer, open_kj_diff};
use self::terminal::{KITTY_REQUESTED, TermLock, enter_terminal, leave_terminal, spawn_key_reader};

/// How often the rank, the cache figures and the pending-ask count are
/// refreshed. Slow on purpose: none of them is an interaction-rate fact.
const REFRESH: Duration = Duration::from_secs(5);

/// The redraw tick. Fast enough that a stream looks live, slow enough that a
/// terminal over ssh is not the bottleneck.
const TICK: Duration = Duration::from_millis(80);

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
    /// Acks for our own draft edits ([`kaijutsu_client::draft_outbox`]).
    draft_landed: mpsc::UnboundedReceiver<Landed>,
    /// Answers to our own editor keys and pastes ([`kaijutsu_client::editor_outbox`]).
    editor_landed: mpsc::UnboundedReceiver<editor_outbox::Landed>,
}

/// Run the client until it quits, restoring the terminal on the way out.
///
/// `kitty_keyboard` is `--kitty-keyboard`: the kitty keyboard protocol is on
/// by request only, never by probe (`docs/tui.md`, "Open" — this client's
/// pinned zero-terminal-query invariant rules out `supports_keyboard_
/// enhancement`, which asks `CSI ? u`).
pub async fn run(
    bridge: KernelBridge,
    start: ContextInfo,
    identity: String,
    diff: Option<Vec<String>>,
    kitty_keyboard: bool,
) -> Result<()> {
    KITTY_REQUESTED.store(kitty_keyboard, Ordering::SeqCst);
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
    // reading them, and [`feeds::HydrateGuard`]'s `try_send` always has a slot
    // while the loop is still draining.
    let (hydrated_tx, hydrated_rx) = mpsc::channel::<Hydrated>(64);
    let mut feeds = Feeds::new(feed_tx, hydrated_tx, bridge.actor().clone());
    watch_context(&bridge, &mut app, start.id, &mut feeds).await?;
    app.switch_to(start.id);
    remember_view(&bridge, start.id).await;

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
    let (drafts, draft_landed) = draft_outbox::spawn_local(bridge.actor().clone());
    app.drafts = drafts;
    let (editor_keys, editor_landed) = editor_outbox::spawn_local(bridge.actor().clone());
    app.editor_keys = editor_keys;

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
    let mut terminal = match enter_terminal() {
        Ok(terminal) => terminal,
        Err(e) => {
            // `enter_terminal` can fail after it has taken the screen, raw
            // mode, the title and the kitty frame (`Terminal::new` is its
            // last step). Every one of those is given back before the
            // error reaches the shell, or the shell inherits them — a
            // pushed kitty frame in particular would keep encoding its
            // keys.
            restore_terminal();
            return Err(anyhow::Error::from(e).context("take the alternate screen"));
        }
    };
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
        draft_landed,
        editor_landed,
    };
    let result = event_loop(&bridge, &mut app, &mut terminal, &mut wires).await;
    leave_terminal(&mut terminal, &term_lock, &reader_stop);
    result
}

/// What the loop remembers from one turn of its `select!` to the next.
struct LoopState {
    key_state: KeyState,
    /// The connection status the actor reports; a change lands through
    /// [`on_status_changed`].
    status: tokio::sync::watch::Receiver<kaijutsu_client::ConnectionStatus>,
    /// The open asks arrive on the actor's ledger watch, which the kernel's
    /// push keeps current; nothing polls. Marked changed so the state the
    /// actor already holds is folded in on the first turn of the loop.
    ledger: tokio::sync::watch::Receiver<kaijutsu_client::LedgerState>,
    asks: AskPlumbing,
    stop_signals: StopSignals,
    refresh: Refresh,
    frame: Frame,
    beat: Beat,
}

/// The records a fold asks for (the card candidate's, an answered card's
/// decision) and the work a key asks for (the ledger view, an answer)
/// run on their own tasks and land here (`spawn_ask_read`,
/// `spawn_ask_work`).
struct AskPlumbing {
    read_tx: mpsc::UnboundedSender<AskLanded>,
    landed: mpsc::UnboundedReceiver<AskLanded>,
    /// The card candidate whose record is being read, so it is read once.
    card_read: Option<String>,
}

/// The background refresh round: the cadence, the round in flight and
/// whether another was asked for.
struct Refresh {
    interval: tokio::time::Interval,
    /// The round in flight, if one is. Its result lands through the join arm
    /// below; the loop itself never awaits the kernel for a refresh.
    task: Option<tokio::task::JoinHandle<refresh::Refreshed>>,
    /// A round was asked for out of turn (`App::roster_changed`). Stays set
    /// while a round is in flight, because that round started before the
    /// change and its answer is already stale.
    wanted: bool,
}

/// When the frame is drawn and what the last one told the terminal.
struct Frame {
    tick: tokio::time::Interval,
    dirty: bool,
    /// Something the player is waiting to see: drawn now, not on the tick.
    answer: bool,
    last_strip_frame: Instant,
    presented: Presented,
    /// `KAIJUTSU_TUI_PROBE_PANIC=frame` armed by `F12`: the next frame panics
    /// with a synchronized update open.
    panic_in_frame: bool,
}

impl LoopState {
    fn new(bridge: &KernelBridge) -> Result<Self> {
        let status = bridge.actor().watch_status();
        let mut ledger = bridge.actor().ledger();
        ledger.mark_changed();
        let (read_tx, landed) = mpsc::unbounded_channel::<AskLanded>();
        Ok(Self {
            key_state: KeyState::new(),
            status,
            ledger,
            asks: AskPlumbing { read_tx, landed, card_read: None },
            stop_signals: StopSignals::listen().context("listen for SIGTERM and SIGHUP")?,
            refresh: Refresh { interval: tokio::time::interval(REFRESH), task: None, wanted: false },
            frame: Frame {
                tick: tokio::time::interval(TICK),
                dirty: true,
                answer: false,
                last_strip_frame: Instant::now(),
                presented: Presented::default(),
                panic_in_frame: false,
            },
            beat: Beat { wake: None, tempo_bps: 0.0 },
        })
    }
}

async fn event_loop(
    bridge: &KernelBridge,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    wires: &mut Wires,
) -> Result<()> {
    let mut ls = LoopState::new(bridge)?;

    app.connection = Some(bridge.actor().current_status());

    while !app.quit {
        // A task the loop never joins panicked (`TASK_PANIC`, set by the
        // hook installed in `run`); the hook did not restore the terminal
        // for it, so this is the loop's own chance to end normally and
        // reach `leave_terminal` — checked once an iteration, which bounds
        // the delay by whatever else is ready to wake the loop and, absent
        // that, by `TICK`.
        if let Some(message) = TASK_PANIC.lock().take() {
            return Err(anyhow::anyhow!(message));
        }
        ls.frame.answer = false;
        tokio::select! {
            Some(event) = wires.key_rx.recv() => {
                on_input(bridge, app, terminal, &mut wires.feeds, &wires.term_lock, wires.probe_panic, &mut ls, event).await?;
            }
            Some(landed) = wires.editor_landed.recv() => {
                if land_editor_answer(app, landed, &wires.term_lock) {
                    ls.frame.dirty = true;
                    ls.frame.answer = true;
                }
            }
            Some(landed) = wires.draft_landed.recv() => {
                if land_draft_edit(app, landed) {
                    ls.frame.dirty = true;
                }
            }
            Some((context_id, event)) = wires.feed_rx.recv() => {
                ls.frame.dirty = true;
                apply_feed(bridge, app, context_id, event, &mut wires.feeds);
            }
            received = wires.server_events.recv() => {
                on_server_event(bridge, app, &mut ls.frame, received).await;
            }
            Some(open) = wires.editor_opens.recv() => {
                editor::enter_editor(app, open);
                ls.frame.dirty = true;
            }
            Ok(()) = ls.status.changed() => {
                on_status_changed(app, &mut ls);
            }
            Ok(()) = ls.ledger.changed() => {
                on_ledger_changed(bridge, app, &wires.term_lock, &mut ls)?;
            }
            Some(landed) = ls.asks.landed.recv() => {
                on_ask_landed(bridge, app, &mut ls, landed);
            }
            signal = ls.stop_signals.recv() => {
                // The loop ends as `:q` ends it, so the terminal is
                // restored on the way out instead of dying in raw mode.
                tracing::info!(signal, "stopping on signal");
                app.quit = true;
            }
            _ = ls.refresh.interval.tick() => {
                // Single-flight: a round still running is left to finish,
                // and the next tick starts the next one.
                if ls.refresh.task.is_none() {
                    ls.refresh.wanted = false;
                    ls.refresh.task = Some(tokio::spawn(refresh::fetch(bridge.clone(), app.listing_epoch())));
                }
            }
            joined = join_refresh(&mut ls.refresh.task), if ls.refresh.task.is_some() => {
                on_refresh_joined(bridge, app, &mut wires.feeds, &mut ls, joined)?;
            }
            Some((context_id, hydrated)) = wires.hydrated_rx.recv() => {
                on_hydrated(app, &mut wires.feeds, &mut ls.frame, context_id, hydrated);
            }
            // The beat-driven redraw: armed at the playing track's predicted
            // next onset, re-armed at `scheduled + period` inside the arm
            // body — never `actual_wake + period` (`docs/tui.md`, "Timing to
            // music"; `docs/midi.md`, "The one timebase"). The `if` guard
            // skips this arm entirely while nothing is playing, so it never
            // busy-polls a zero sleep.
            _ = tokio::time::sleep(ls.beat.wake.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default()), if ls.beat.wake.is_some() => {
                if let Some(scheduled) = ls.beat.wake {
                    ls.beat.wake = Some(picker::rearm(scheduled, ls.beat.tempo_bps));
                }
                ls.frame.dirty = true;
            }
            _ = ls.frame.tick.tick() => {
                on_tick(terminal, app, &wires.term_lock, &mut ls)?;
            }
        }
        // Input, and the editor states answering it, are drawn now rather
        // than at the next tick. A burst of keys draws once, after the last
        // key already read.
        if ls.frame.dirty && ls.frame.answer && wires.key_rx.is_empty() {
            ls.frame.dirty = false;
            present(terminal, &wires.term_lock, app, ls.key_state.keys.armed(), ls.frame.panic_in_frame, &mut ls.frame.presented)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The code of every file in the `run` module, tests left out, so a
    /// source-scanning test sees the loop and the helpers it calls wherever
    /// they live.
    fn module_source() -> String {
        [
            include_str!("run.rs"),
            include_str!("run/act.rs"),
            include_str!("run/arms.rs"),
            include_str!("run/asks_io.rs"),
            include_str!("run/draw.rs"),
            include_str!("run/feeds.rs"),
            include_str!("run/probe.rs"),
            include_str!("run/seat.rs"),
            include_str!("run/signals.rs"),
            include_str!("run/surfaces.rs"),
            include_str!("run/terminal.rs"),
        ]
        .iter()
        .map(|file| file.split_once("\n#[cfg(test)]").map_or(*file, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
    }

    /// The context on screen changes in one place. `switch_seat` watches
    /// the new context, switches to it, and loads its draft; a path that
    /// calls `App::switch_to` itself gets the switch without one of those
    /// three, and the miss is invisible on the frame it happens — the
    /// picker's own switch kept the previous context's draft on the compose
    /// line, and the draft ledger's ack floor then refused the feed's correction.
    ///
    /// `run()`'s startup attach is the one sanctioned direct call: there is
    /// no seat to switch from, and it loads its own draft.
    #[test]
    fn every_context_switch_goes_through_switch_seat() {
        // The module's own code, not its tests — the assertion below names
        // the pattern it is looking for.
        let source = module_source();
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
        let source = module_source();
        let arm = source
            .split_once("Intent::LastContext =>")
            .expect("the run module has a LastContext arm")
            .1;
        let arm = &arm[..arm.find("Intent::OpenDiff").expect("the next arm follows")];
        assert!(
            arm.contains("switch_seat("),
            "the LastContext arm does not route through `switch_seat`: {arm}"
        );
    }

    /// Input implies focus, at every arm input arrives on. A terminal that
    /// reports `FocusLost` and never reports again would otherwise leave the
    /// client believing nobody is looking for the rest of the session
    /// (`App::saw_input`).
    #[test]
    fn every_input_arm_says_the_terminal_is_focused() {
        let source = module_source();
        for arm in ["Event::Key(key) => {", "Event::Paste(text) => {"] {
            let body = source.split_once(arm).unwrap_or_else(|| panic!("the run module has a `{arm}` arm")).1;
            let body = &body[..body.find("\n        }").expect("the arm ends")];
            assert!(
                body.contains("saw_input()"),
                "the `{arm}` arm does not say the terminal is focused: {body}"
            );
        }
    }

    /// The name of a `fn` declared at the top level of a module — the
    /// enclosing function for every line until the next one.
    fn top_level_fn_name(line: &str) -> Option<&str> {
        let rest = line
            .strip_prefix("fn ")
            .or_else(|| line.strip_prefix("pub fn "))
            .or_else(|| line.strip_prefix("async fn "))
            .or_else(|| line.strip_prefix("pub async fn "))
            .or_else(|| line.strip_prefix("pub(super) fn "))
            .or_else(|| line.strip_prefix("pub(super) async fn "))?;
        Some(&rest[..rest.find('(')?])
    }

    /// The loop's own kernel calls live on their own tasks — the refresh
    /// round in `refresh::fetch`, the hot set's hydrate in `start_hydrate`,
    /// an ask's record in `spawn_ask_read` — and land through a select arm. Inside the loop they would hold every
    /// key until a busy kernel answered (`docs/tui.md`, "Keys").
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
    /// awaiting `read_input`, reached through `act`'s intent dispatch), and
    /// walking the whole graph would flag that too. `act` itself is in
    /// `HELPERS`: an ask key only records its work (`App::ask_work`), and
    /// `spawn_ask_work` runs it, so no ask call may appear in its body. `start_rehydrate` and
    /// `start_hydrate` are deliberately NOT in `HELPERS`: both hand their
    /// kernel call to `tokio::spawn`, as `spawn_ask_read` does, so scanning
    /// their bodies would flag
    /// the exact off-loop pattern this test exists to require. Add a name to
    /// `HELPERS` when the feed-apply chain grows another synchronous hop.
    ///
    /// The larger arms of `event_loop` run in the `on_*` functions of
    /// `run/arms.rs`, so they are in `HELPERS` too: the arm bodies stay under
    /// the same scan.
    #[test]
    fn the_event_loop_never_awaits_the_kernel_for_background_work() {
        const HELPERS: &[&str] = &[
            "apply_feed",
            "apply_delivery",
            "land_rehydrate",
            "act",
            "on_input",
            "on_server_event",
            "on_status_changed",
            "on_ledger_changed",
            "on_ask_landed",
            "join_refresh",
            "on_refresh_joined",
            "on_hydrated",
            "on_tick",
        ];

        let source = module_source();
        let start = source.find("async fn event_loop(").expect("event_loop is in the run module");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("event_loop ends");
        let mut scanned = body[..end].to_string();
        for helper in HELPERS {
            scanned.push('\n');
            scanned.push_str(function_body(&source, helper));
        }
        for call in [
            "list_contexts(",
            "list_tracks(",
            "get_ask(",
            "list_asks(",
            "decide_ask(",
            "escalate_ask(",
            "rehydrate_context(",
            "hydrate_context(",
        ] {
            assert!(
                !scanned.contains(call),
                "`{call}` is awaited on the loop's feed-apply path; run it on its own task and \
                 land the result through a join arm, so keys never queue behind the kernel"
            );
        }
    }

    /// The literal source of a top-level `fn name(` or `async fn name(` in
    /// the `run` module: from the `fn` keyword to its closing brace at column zero —
    /// the same heuristic [`the_event_loop_never_awaits_the_kernel_for_background_work`]
    /// has always used for `event_loop` itself, which holds because this
    /// module's inner blocks stay indented.
    ///
    /// Panics when `name` is not found. A watched helper is named because it
    /// sits on the loop's feed-apply path; a silent `None` for a renamed or
    /// removed one would drop it out of the scan without saying so, which is
    /// exactly the failure mode `HELPERS` exists to prevent one level up —
    /// the whole point is that this test cannot go quiet on its own.
    fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("`{name}` is in HELPERS but no `fn {name}(` exists in the run module any more"));
        let body = &source[start..];
        let end = body.find("\n}\n").unwrap_or_else(|| panic!("`{name}`'s body never closes at column zero"));
        &body[..end]
    }
}
