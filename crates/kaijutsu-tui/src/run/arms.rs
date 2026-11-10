//! The larger arms of the event loop: what one input event, server event,
//! ledger change, ask landing, refresh round, hydrate landing or tick does.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::style::Print;
use crossterm::event::Event;
use kaijutsu_client::ServerEvent;
use kaijutsu_types::ContextId;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::asks;
use crate::inflight;
use crate::bridge::KernelBridge;
use crate::refresh;
use crate::render;

use crate::editor;

use super::{Frame, LoopState};
use super::act::{Acted, act, handle_picker_key, paste_text};
use super::asks_io::{AskLanded, AskReadFor, raise_card, spawn_ask_read, spawn_ask_work};
use super::draw::{observe_beat_sync, present, rearm_beat_wake};
use super::feeds::{Feeds, HydrateOutcome, abandon_rehydrate, adopt, land_rehydrate, mark_activity, mark_turn_liveness, release_cold, start_hydrate};
use super::probe::ProbePanic;
use super::seat::hydrating_notice;
use super::signals::suspend;
use super::surfaces::observe_editor_event;
use super::terminal::TermLock;

/// One terminal event: a key, a paste, a resize or a focus change.
pub(super) async fn on_input(
    bridge: &KernelBridge,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    feeds: &mut Feeds,
    term_lock: &TermLock,
    probe_panic: ProbePanic,
    ls: &mut LoopState,
    event: Event,
) -> Result<()> {
    match event {
        Event::Key(key) => {
            ls.frame.dirty = true;
            ls.frame.answer = true;
            // Input only reaches a focused terminal, whatever
            // the last focus report said (`App::saw_input`).
            app.saw_input();
            if key.code == crossterm::event::KeyCode::F(12) {
                match probe_panic {
                    ProbePanic::OnKey => panic!("KAIJUTSU_TUI_PROBE_PANIC: F12 pressed"),
                    ProbePanic::InFrame => ls.frame.panic_in_frame = true,
                    // Never joined, like `Feeds::pump`'s forwarder
                    // or a hydrate round: the loop must notice
                    // this panic through `TASK_PANIC`, not by
                    // unwinding.
                    ProbePanic::InTask => {
                        tokio::task::spawn_local(async {
                            panic!("KAIJUTSU_TUI_PROBE_PANIC: F12 pressed inside a task");
                        });
                    }
                    ProbePanic::Off => {}
                }
            }
            // The prefix is never the picker's key
            // ([`Keys::claims`]); `act` runs the chord over it.
            if app.picker.is_some() && !ls.key_state.keys.claims(&key) {
                handle_picker_key(bridge, app, key, feeds).await?;
            } else {
                let width = terminal.size()?.width;
                if act(bridge, app, &mut ls.key_state, key, feeds, term_lock, width)
                    .await?
                    == Acted::Suspend
                {
                    suspend(terminal, term_lock)?;
                    ls.key_state.ctrl_z_tap.reset();
                    ls.frame.presented.cursor_shape = None;
                    ls.frame.presented.title = None;
                    // `fg` put this terminal back in front of
                    // the player; a report may never say so.
                    app.saw_input();
                }
            }
            spawn_ask_work(bridge, app, &ls.asks.read_tx);
            // A switch, an `Esc`, or an answer may leave room for
            // a card; `raise_card` does nothing while one is up.
            raise_card(bridge, app, &ls.asks.read_tx, &mut ls.asks.card_read);
            if std::mem::take(&mut app.roster_changed) {
                ls.refresh.wanted = true;
                ls.refresh.interval.reset_immediately();
            }
        }
        Event::Paste(text) => {
            ls.frame.dirty = true;
            ls.frame.answer = true;
            app.saw_input();
            paste_text(app, text);
        }
        // The frame is rebuilt at the new size on the next tick:
        // the transcript re-wraps and the band follows.
        Event::Resize(..) => ls.frame.dirty = true,
        // Focus reporting (DECSET 1004). Coming back draws a
        // frame, so what is on screen is current rather than
        // whatever was there when focus left; leaving disarms
        // the beat timer, and `render::strip_animating` stops
        // the spinner (`docs/tui.md`, "What owning the screen
        // lets us use").
        Event::FocusGained => {
            app.focused = true;
            rearm_beat_wake(app, &mut ls.beat);
            ls.frame.dirty = true;
        }
        // The timer is disarmed rather than left armed and
        // ignored: a missed onset is missed, never replayed, and
        // `rearm_beat_wake` above re-anchors on the live phasor
        // (`docs/midi.md`, "The one timebase").
        Event::FocusLost => {
            app.focused = false;
            ls.beat.wake = None;
        }
        _ => {}
    }
    Ok(())
}

/// One kernel event off the actor's broadcast, or the lag or close that
/// stands in for it.
pub(super) async fn on_server_event(
    bridge: &KernelBridge,
    app: &mut App,
    frame: &mut Frame,
    received: Result<ServerEvent, tokio::sync::broadcast::error::RecvError>,
) {
    let event = match received {
        Ok(event) => event,
        // A lag drops events unseen, and among them may be the
        // `TurnCompleted` that would have cleared a turn flag.
        // The client no longer knows what is running; say so and
        // forget rather than carry a flag nothing will clear.
        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
            app.forget_turn_liveness();
            app.note(format!("{n} kernel events lost; turn liveness reset"));
            frame.dirty = true;
            return;
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
    };
    if mark_activity(app, &event) {
        frame.dirty = true;
    }
    if mark_turn_liveness(app, &event) {
        frame.dirty = true;
    }
    if observe_editor_event(bridge, app, &event).await {
        frame.dirty = true;
        // An editor push is the answer to a key, ours or a peer's.
        frame.answer = true;
    }
    // The picker's tail buffer (`docs/tui.md`, "The picker") is
    // fed ungated, like the app's `ContextTails` — every context,
    // not just the ones watched.
    if app.tails.observe(&event, kaijutsu_types::now_millis()) {
        frame.dirty = true;
    }
    if let ServerEvent::BeatSync { context_id, beat_ref } = &event {
        observe_beat_sync(app, *context_id, *beat_ref, bridge.actor().kernel_now_ns());
        frame.dirty = true;
    }
}

/// The connection status changed.
pub(super) fn on_status_changed(app: &mut App, ls: &mut LoopState) {
    let connection = ls.status.borrow().clone();
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
    // The actor reads the ledger before it reports `Connected`,
    // so an unsynced ledger here is a listing or subscription
    // that failed: the asks on screen stop moving, and the
    // player is told rather than left to trust them.
    if matches!(app.connection, Some(kaijutsu_client::ConnectionStatus::Connected { .. }))
        && !ls.ledger.borrow().synced
    {
        app.note("cannot follow the approval ledger; asks on screen may be stale");
    }
    ls.frame.dirty = true;
}

/// The approval ledger changed.
pub(super) fn on_ledger_changed(bridge: &KernelBridge, app: &mut App, term_lock: &TermLock, ls: &mut LoopState) -> Result<()> {
    let folded = {
        let state = ls.ledger.borrow_and_update();
        asks::fold_ledger(app, &state)
    };
    if let Some(fold) = folded {
        if let Some(request_id) = fold.answered {
            spawn_ask_read(bridge, &ls.asks.read_tx, request_id, AskReadFor::Answered);
        }
        raise_card(bridge, app, &ls.asks.read_tx, &mut ls.asks.card_read);
        // An ask that landed while nobody was looking says so to
        // the desktop; one that landed in front of the player is
        // the card on screen (`docs/tui.md`, "Asks").
        let seen = asks::NotifySeen {
            focused: app.focused,
            baseline: fold.baseline,
            raised_in: fold.raised_in,
            current: app.current,
        };
        if let Some(context_id) = asks::notify_target(seen)
            && let Some(notification) = asks::ask_notification(app.focused, &app.label_for(context_id))
        {
            let _guard = term_lock.lock();
            crossterm::execute!(io::stdout(), Print(notification))
                .context("raise a desktop notification")?;
        }
        ls.frame.dirty = true;
    }
    Ok(())
}

/// An ask's read or answer landed from its own task.
pub(super) fn on_ask_landed(bridge: &KernelBridge, app: &mut App, ls: &mut LoopState, landed: AskLanded) {
    match landed {
        AskLanded::Read { request_id, purpose: AskReadFor::Card, result } => {
            if ls.asks.card_read.as_deref() == Some(request_id.as_str()) {
                ls.asks.card_read = None;
            }
            asks::land_card_read(app, &request_id, result);
        }
        AskLanded::Read { request_id, purpose: AskReadFor::Answered, result } => {
            let detail = result.ok().flatten();
            app.note(asks::answered_notice(app.principal, &request_id, detail.as_ref()));
        }
        AskLanded::Ledger(reads) => asks::land_ledger(app, reads, kaijutsu_types::now_millis()),
        AskLanded::Shown { request_id, result } => asks::land_show(app, &request_id, result),
        AskLanded::Decided { request_id, answer } => asks::land_decision(app, &request_id, answer),
        AskLanded::Escalated { request_id, answer, took } => asks::land_escalation(app, &request_id, answer, took),
    }
    // The candidate may have moved on, or a failed answer put
    // its ask back; this reads the one to show now.
    raise_card(bridge, app, &ls.asks.read_tx, &mut ls.asks.card_read);
    ls.frame.dirty = true;
}

/// Waits for the refresh round in flight. Only polled while one is
/// ([`Refresh::task`]).
pub(super) async fn join_refresh(
    task: &mut Option<tokio::task::JoinHandle<refresh::Refreshed>>,
) -> Result<refresh::Refreshed, tokio::task::JoinError> {
    task.as_mut().expect("guarded by is_some").await
}

/// The refresh round in flight finished.
pub(super) fn on_refresh_joined(
    bridge: &KernelBridge,
    app: &mut App,
    feeds: &mut Feeds,
    ls: &mut LoopState,
    joined: Result<refresh::Refreshed, tokio::task::JoinError>,
) -> Result<()> {
    ls.refresh.task = None;
    let refreshed = joined.context("background refresh round")?;
    refresh::apply(app, refreshed);
    // The rank the hot set reads was just recomputed, so this is
    // where a promote makes a context resident and a demote lets
    // one go. Releasing is local; the hydrate rides its own task.
    release_cold(app, feeds);
    start_hydrate(bridge, app, feeds);
    if ls.refresh.wanted {
        ls.refresh.interval.reset_immediately();
    }
    rearm_beat_wake(app, &mut ls.beat);
    ls.frame.dirty = true;
    Ok(())
}

/// A hot context's hydrate round answered.
pub(super) fn on_hydrated(app: &mut App, feeds: &mut Feeds, frame: &mut Frame, context_id: ContextId, hydrated: HydrateOutcome) {
    feeds.hydrating.remove(&context_id);
    match hydrated {
        // The hot set can move while a hydrate is in flight, and
        // a switch can have hydrated the same context first. A
        // mirror nobody wants is dropped, and the feed receiver
        // with it, which ends the subscription.
        HydrateOutcome::Fresh(Ok((mirror, rx)))
            if app.hot_set().contains(&context_id)
                && !app.views.contains_key(&context_id) =>
        {
            adopt(app, feeds, context_id, mirror, rx);
            // The switch that waited for this round said so on
            // the status row; the transcript answers now. Only
            // that exact line comes down, so a notice posted
            // since stands.
            if app.notice() == Some(hydrating_notice(app, context_id).as_str()) {
                app.clear_notice();
            }
            frame.dirty = true;
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
                frame.dirty = true;
            }
        }
        // A rebuilt snapshot lands on the view it belongs to, or
        // nowhere if the context was released while the round
        // was in flight — the same drop `apply_delivery` already
        // gives an ordinary feed delivery with no view to land
        // on.
        HydrateOutcome::Rebuilt(Ok((blocks, version))) => {
            if land_rehydrate(app, feeds, context_id, blocks, version) {
                frame.dirty = true;
            }
        }
        // The round trip itself failed — never reached a
        // snapshot at all, so there is nothing to apply. See
        // `abandon_rehydrate` for why this releases rather than
        // retries or waits.
        HydrateOutcome::Rebuilt(Err(e)) => {
            abandon_rehydrate(app, feeds, context_id, e);
            frame.dirty = true;
        }
    }
}

/// The redraw tick.
pub(super) fn on_tick(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App, term_lock: &TermLock, ls: &mut LoopState) -> Result<()> {
    // The strip's spinner and breath: a redraw every phase step
    // while a tool call is running, and none otherwise.
    if !ls.frame.dirty
        && ls.frame.last_strip_frame.elapsed() >= Duration::from_millis(inflight::PHASE_MILLIS)
        && render::strip_animating(app)
    {
        ls.frame.last_strip_frame = Instant::now();
        ls.frame.dirty = true;
    }
    app.screen_rows = terminal.size()?.height;
    // A new ask card is drawn disarmed, and arms on the first
    // frame after its hold (`App::tick_ask_arming`).
    if app.tick_ask_arming(Instant::now()) {
        ls.frame.dirty = true;
    }
    if ls.frame.dirty {
        ls.frame.dirty = false;
        present(terminal, term_lock, app, ls.key_state.keys.armed(), ls.frame.panic_in_frame, &mut ls.frame.presented)?;
    }
    Ok(())
}
