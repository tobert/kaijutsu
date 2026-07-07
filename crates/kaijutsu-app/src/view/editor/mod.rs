//! In-app vi editor — the renderer + key-forwarder side (Design A).
//!
//! The kernel owns the editor session; the app is one *renderer* of it. The
//! `open_editor` peer signal lands here as [`EditorOpenRequested`], drives
//! `Screen::Editor`, and stores the live session in [`ActiveEditor`]. This
//! module is also home to the dedicated editor surface renderer (`render`,
//! fed by the `editor_state` subscription) and key forwarding to
//! `editor_keys` (`keys`, `editor_dispatch_keys`) — the screen/landing
//! foundation plus its renderer and forwarder.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_client::{ActorHandle, EditorState, ServerEvent};

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage, ServerEventMessage};
use crate::input::focus::{ActiveSurface, FocusArea};
use crate::ui::screen::Screen;

mod keys;
mod pipe;
pub mod render;

use pipe::{FailureVerdict, KeyPipe};

/// The ordered outbound keystroke pipe for the active editor session (see
/// [`pipe`]): one `editor_keys` batch on the wire at a time, everything typed
/// meanwhile queued behind it in keyboard order, one retry before a reported
/// drop. Cleared whenever the session ends (close, loss, replacement).
#[derive(Resource, Default)]
pub struct EditorKeyPipe(KeyPipe);

/// A kernel `open_editor` signal landed: the submitter ran `vi <path>` and the
/// kernel opened session `session`. Written by the peer dispatcher
/// (`peers::systems`), consumed by [`handle_editor_open`]. Carries the **initial
/// `EditorState`** (the signal is self-contained), so the renderer has text to
/// draw the instant it lands — subsequent `editor_state` pushes carry updates.
#[derive(Message, Clone, Debug)]
pub struct EditorOpenRequested {
    /// The path being edited (display / title).
    pub path: String,
    /// The initial editor state from the signal (carries the session id + text).
    pub state: EditorState,
}

/// The editor session the app is currently rendering, if any. Set when an
/// `open_editor` signal lands; cleared on exit/close. The MSDF renderer reads
/// `state` (text/cursor/mode) each frame.
#[derive(Resource, Default, Debug)]
pub struct ActiveEditor {
    /// The live session, or `None` when no editor is open.
    pub session: Option<EditorSessionView>,
}

/// The session the renderer draws: identity + the latest pushed state.
#[derive(Clone, Debug)]
pub struct EditorSessionView {
    /// Kernel editor session handle (the currency of `editor_keys`/`editor_state`).
    pub session: u64,
    /// The path being edited. Read by the panel renderer (title) in step 4b.
    #[allow(dead_code)]
    pub path: String,
    /// The latest renderer-facing snapshot (seeded from the open signal, kept
    /// fresh by `editor_state` pushes — the human's keystrokes and peer merges).
    pub state: EditorState,
}

/// Land an `open_editor` signal: store the session + its initial state and reveal
/// `Screen::Editor`. The mirror of `handle_context_switch`'s screen reveal — the
/// editor drives its own screen from its own landing handler, not via `view/sync`.
fn handle_editor_open(
    mut events: MessageReader<EditorOpenRequested>,
    mut active: ResMut<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ev in events.read() {
        info!(
            "open_editor: session {} on {}",
            ev.state.session, ev.path
        );
        // A new session replaces the old one; keystrokes queued for the old
        // session must not leak into it.
        key_pipe.0.clear();
        active.session = Some(EditorSessionView {
            session: ev.state.session,
            path: ev.path.clone(),
            state: ev.state.clone(),
        });
        next_screen.set(Screen::Editor);
    }
}

/// Keep the active session's state fresh from the editor push channel: an
/// `EditorStateChanged` for our session replaces the cached snapshot (the
/// human's own keystrokes echoing, or a peer's merge); an `EditorClosed` drops
/// the session and pops back to the conversation.
fn handle_editor_events(
    mut server_events: MessageReader<ServerEventMessage>,
    mut active: ResMut<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ServerEventMessage(event) in server_events.read() {
        match event {
            ServerEvent::EditorStateChanged { state } => {
                if let Some(view) = active.session.as_mut()
                    && view.session == state.session
                {
                    view.state = state.clone();
                }
            }
            ServerEvent::EditorClosed { session_id } => {
                if active
                    .session
                    .as_ref()
                    .is_some_and(|v| v.session == *session_id)
                {
                    info!("editor session {session_id} closed by server");
                    active.session = None;
                    key_pipe.0.clear();
                    next_screen.set(Screen::Conversation);
                }
            }
            _ => {}
        }
    }
}

/// Does an `editor_keys` failure mean the kernel session is gone (as opposed to
/// a transient timeout or a dropped connection)? The kernel returns
/// `editor: no such session N` (see `kaijutsu-kernel/src/editor.rs`) when the
/// session id isn't in `EditorSessions` — which is exactly what a kernel restart
/// produces: the sessions are in-memory and don't survive it, while the persisted
/// `kernel_id` is unchanged, so the reconnect looks ordinary. Match that verdict
/// (and not a generic error) so a momentary RPC hiccup never evicts a live editor.
fn is_session_lost_error(err: &str) -> bool {
    err.contains("no such session")
}

/// Drop a stale editor session the kernel has disowned. The keystroke path
/// (`editor_dispatch_keys`) reports `EditorSessionLost` when `editor_keys` comes
/// back `no such session` — the kernel restarted out from under an open editor,
/// so the buffer the app is showing is dead (typing echoes nothing; the
/// "frozen editor" trap). Pop back to the conversation so the user can reopen
/// cleanly. Scoped to the matching session so a stale report can't evict a
/// freshly-reopened one.
fn handle_editor_session_lost(
    mut results: MessageReader<RpcResultMessage>,
    mut active: ResMut<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for result in results.read() {
        let RpcResultMessage::EditorSessionLost { session } = result else {
            continue;
        };
        if active
            .session
            .as_ref()
            .is_some_and(|v| v.session == *session)
        {
            warn!("editor session {session} lost (kernel restarted?); returning to conversation");
            active.session = None;
            key_pipe.0.clear();
            next_screen.set(Screen::Conversation);
        }
    }
}

/// Forward keystrokes on `Screen::Editor` toward the kernel's `editor_keys`.
/// The app is a pure key forwarder: each keystroke becomes vi notation and
/// joins the ordered [`EditorKeyPipe`]; [`editor_pump_keys`] ships one batch at
/// a time so keystrokes can never reorder in flight (burst input — key repeat,
/// BRP `send_keys` — lands as one coalesced batch). The kernel's `EditorCore`
/// (the real VimMachine) interprets the keys; the push channel echoes the
/// resulting state back to [`ActiveEditor`], so the panel re-renders as you
/// type.
///
/// This replaces the provisional Esc-exits-screen hatch. `Esc` is now an
/// ordinary key (→ normal mode). `ZZ`/`ZQ` also travel as ordinary keys: the
/// kernel — which alone knows the true mode, so it never mistakes an *inserted*
/// `ZZ` for a quit — saves/discards and drops the session, then pushes
/// `EditorClosed`, which [`handle_editor_events`] turns into the screen pop. No
/// app-side mode tracking, no quit detection.
fn editor_dispatch_keys(
    mut keyboard: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    active: Res<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<ActiveSurface>,
) {
    if active.session.is_none() {
        keyboard.clear();
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }
        // Bare modifier presses (Shift/Ctrl/… on their own) are never vi keys.
        if keys::is_modifier_key(event.key_code) {
            continue;
        }
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);

        // Ctrl+Z suspends the editor to the shell (the job-control metaphor):
        // leave to the conversation with the shell focused, but keep
        // `ActiveEditor` — the session stays alive in the kernel as the
        // "suspended job", and `fg` re-foregrounds it. This is a *local*
        // intercept (no kernel round-trip), so it also frees a frozen editor when
        // the kernel is unreachable — the escape hatch. (The Action-system
        // Ctrl+Z↔ToggleSurface is suppressed on `Screen::Editor`, so there is no
        // double-handling.) The buffer isn't forwarded `<C-z>`. Keys queued
        // before the suspend stay in the pipe — the pump runs ungated, so they
        // still deliver to the live (suspended) session.
        if ctrl && event.key_code == KeyCode::KeyZ {
            next_screen.set(Screen::Conversation);
            *focus = FocusArea::Compose;
            *surface = ActiveSurface::Shell;
            return;
        }

        if let Some(notation) = keys::bevy_to_vi_notation(event, ctrl) {
            key_pipe.0.push(&notation);
        }
    }
}

/// Ship one `editor_keys` batch from the pipe, if the wire is free. Runs
/// ungated (not `Screen::Editor`-only) so keystrokes queued just before a
/// Ctrl+Z suspend still deliver; it no-ops with no session or an empty pipe.
fn editor_pump_keys(
    active: Res<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    actor: Option<Res<RpcActor>>,
    results: Res<RpcResultChannel>,
) {
    let Some(view) = active.session.as_ref() else {
        return;
    };
    let Some(actor) = actor.as_ref() else {
        return;
    };
    if let Some(batch) = key_pipe.0.take_batch() {
        ship_batch(actor.handle.clone(), view.session, batch, &results);
    }
}

/// Advance the pipe on an in-flight batch's outcome: success releases the next
/// batch (the pump ships it next frame); a transient failure retries the same
/// batch once, then drops it loudly. Session-lost failures arrive as
/// [`RpcResultMessage::EditorSessionLost`] instead and clear the pipe via
/// [`handle_editor_session_lost`].
fn handle_editor_keys_outcome(
    mut messages: MessageReader<RpcResultMessage>,
    active: Res<ActiveEditor>,
    mut key_pipe: ResMut<EditorKeyPipe>,
    actor: Option<Res<RpcActor>>,
    results: Res<RpcResultChannel>,
) {
    for message in messages.read() {
        let RpcResultMessage::EditorKeysOutcome { session, ok } = message else {
            continue;
        };
        // A stale outcome from a replaced session must not advance the new
        // session's pipe (the open handler already cleared it).
        if !active.session.as_ref().is_some_and(|v| v.session == *session) {
            continue;
        }
        if *ok {
            key_pipe.0.on_success();
            continue;
        }
        match key_pipe.0.on_failure() {
            FailureVerdict::Retry(batch) => {
                if let Some(actor) = actor.as_ref() {
                    info!("editor_keys({session}): retrying failed batch {batch:?}");
                    ship_batch(actor.handle.clone(), *session, batch, &results);
                } else {
                    // No actor to retry on (mid-reconnect); the batch is gone.
                    key_pipe.0.clear();
                    warn!("editor_keys({session}): dropped batch {batch:?} — no connection");
                }
            }
            FailureVerdict::Dropped(batch) => {
                // Never a silent drop: the keystrokes are lost, say so. An
                // in-editor surface for this (the strip is kernel-owned) is
                // part of the transient-error UX in docs/issues.md.
                warn!(
                    "editor_keys({session}): dropped batch {batch:?} after retry — \
                     keystrokes lost; buffer state re-syncs on the next push"
                );
            }
        }
    }
}

/// Spawn the async send of one keystroke batch. The outcome comes back over the
/// [`RpcResultChannel`]: `EditorKeysOutcome` for success/transient failure, or
/// `EditorSessionLost` when the kernel disowned the session (a restart) — the
/// "frozen editor" trap, which pops the editor instead of retrying.
fn ship_batch(
    handle: ActorHandle,
    session: u64,
    batch: String,
    results: &RpcResultChannel,
) {
    let tx = results.sender();
    IoTaskPool::get()
        .spawn(async move {
            match handle.editor_keys(session, &batch).await {
                Ok(_state) => {
                    // The new state arrives via the editor push subscription; a
                    // `ZZ`/`ZQ` instead arrives as an `EditorClosed` push.
                    let _ = tx.send(RpcResultMessage::EditorKeysOutcome { session, ok: true });
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!("editor_keys({session}, {batch:?}) failed: {msg}");
                    if is_session_lost_error(&msg) {
                        let _ = tx.send(RpcResultMessage::EditorSessionLost { session });
                    } else {
                        let _ =
                            tx.send(RpcResultMessage::EditorKeysOutcome { session, ok: false });
                    }
                }
            }
        })
        .detach();
}

/// Wires the editor's screen/landing foundation, its dedicated surface
/// renderer, and key forwarding.
pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<EditorOpenRequested>()
            .init_resource::<ActiveEditor>()
            .init_resource::<EditorKeyPipe>()
            .add_systems(
                Update,
                (
                    handle_editor_open,
                    handle_editor_events,
                    handle_editor_session_lost,
                ),
            )
            .add_systems(OnEnter(Screen::Editor), render::spawn_editor_panel)
            .add_systems(OnExit(Screen::Editor), render::despawn_editor_panel)
            // Chained so one frame can consume an outcome, enqueue fresh keys,
            // and ship the next batch. Only dispatch is editor-screen-gated:
            // the outcome/pump pair runs ungated so keys queued right before a
            // Ctrl+Z suspend still deliver (they no-op when idle).
            .add_systems(
                Update,
                (
                    handle_editor_keys_outcome,
                    editor_dispatch_keys.run_if(in_state(Screen::Editor)),
                    editor_pump_keys,
                )
                    .chain(),
            )
            // The surface build needs ComputedNode, so it runs post-layout; the
            // cursor sync reads the geometry the build just computed.
            .add_systems(
                PostUpdate,
                (
                    render::build_editor_surface.after(bevy::ui::UiSystems::Layout),
                    render::sync_editor_cursor.after(render::build_editor_surface),
                )
                    .run_if(in_state(Screen::Editor)),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_such_session_is_session_lost() {
        // The kernel's verdict for a restarted-away session.
        assert!(is_session_lost_error("editor: no such session 3"));
    }

    #[test]
    fn wire_wrapped_no_such_session_still_matches() {
        // Over the wire the message is wrapped by capnp/CallError; the
        // substring still identifies the verdict.
        assert!(is_session_lost_error(
            "RPC error: Cap'n Proto error: Failed: remote exception: \
             Failed: editor: no such session 3"
        ));
    }

    #[test]
    fn transient_errors_are_not_session_lost() {
        // A timeout or dropped connection must NOT evict a live editor — only
        // the kernel's explicit "no such session" does.
        assert!(!is_session_lost_error("connection closed"));
        assert!(!is_session_lost_error("editor_keys timed out"));
        assert!(!is_session_lost_error("editor: block not found in 019ef"));
    }
}
