//! In-app vi editor — the renderer + key-forwarder side (Design A).
//!
//! The kernel owns the editor session; the app is one *renderer* of it. The
//! `open_editor` peer signal lands here as [`EditorOpenRequested`], drives
//! `Screen::Editor`, and stores the live session in [`ActiveEditor`]. The MSDF
//! panel renderer (fed by the `editor_state` subscription) and key forwarding to
//! `editor_keys` land in later slices (docs/vi.md steps 4–5); this is the
//! screen/landing foundation.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_client::{EditorState, ServerEvent};

use crate::connection::{RpcActor, ServerEventMessage};
use crate::ui::screen::Screen;

mod keys;
mod render;

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
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ev in events.read() {
        info!(
            "open_editor: session {} on {}",
            ev.state.session, ev.path
        );
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
                    next_screen.set(Screen::Conversation);
                }
            }
            _ => {}
        }
    }
}

/// Forward keystrokes on `Screen::Editor` to the kernel's `editor_keys`. The app
/// is a pure key forwarder: it ships one keystroke per call in vi notation and
/// lets the kernel's `EditorCore` (the real VimMachine) interpret it. The push
/// channel echoes the resulting state back to [`ActiveEditor`], so the panel
/// re-renders as you type.
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
    actor: Option<Res<RpcActor>>,
) {
    let Some(view) = active.session.as_ref() else {
        keyboard.clear();
        return;
    };
    let Some(actor) = actor.as_ref() else {
        keyboard.clear();
        return;
    };
    let session = view.session;

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }
        // Bare modifier presses (Shift/Ctrl/… on their own) are never vi keys.
        if keys::is_modifier_key(event.key_code) {
            continue;
        }
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        let Some(notation) = keys::bevy_to_vi_notation(event, ctrl) else {
            continue;
        };
        let handle = actor.handle.clone();
        IoTaskPool::get()
            .spawn(async move {
                if let Err(e) = handle.editor_keys(session, &notation).await {
                    warn!("editor_keys({session}, {notation:?}) failed: {e}");
                }
                // The new state arrives via the editor push subscription; a
                // `ZZ`/`ZQ` instead arrives as an `EditorClosed` push.
            })
            .detach();
    }
}

/// Wires the editor's screen/landing foundation. The MSDF renderer + key
/// forwarding are added by later slices (docs/vi.md steps 4–5).
pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<EditorOpenRequested>()
            .init_resource::<ActiveEditor>()
            .add_systems(Update, (handle_editor_open, handle_editor_events))
            .add_systems(OnEnter(Screen::Editor), render::spawn_editor_panel)
            .add_systems(OnExit(Screen::Editor), render::despawn_editor_panel)
            .add_systems(
                Update,
                (
                    render::render_editor_panel,
                    editor_dispatch_keys,
                )
                    .run_if(in_state(Screen::Editor)),
            );
    }
}
