//! In-app vi editor — the renderer + key-forwarder side (Design A).
//!
//! The kernel owns the editor session; the app is one *renderer* of it. The
//! `open_editor` peer signal lands here as [`EditorOpenRequested`], drives
//! `Screen::Editor`, and stores the live session in [`ActiveEditor`]. The MSDF
//! panel renderer (fed by the `editor_state` subscription) and key forwarding to
//! `editor_keys` land in later slices (docs/vi.md steps 4–5); this is the
//! screen/landing foundation.

use bevy::prelude::*;
use kaijutsu_client::{EditorState, ServerEvent};

use crate::connection::ServerEventMessage;
use crate::ui::screen::Screen;

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

/// Provisional exit: Esc leaves the editor back to the conversation. Step 5
/// replaces this with real key forwarding (Esc → normal mode) plus `ZZ`/`ZQ`;
/// for now it keeps the editor screen from being a trap during runner testing.
fn editor_exit_on_esc(
    keys: Res<ButtonInput<KeyCode>>,
    mut active: ResMut<ActiveEditor>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        active.session = None;
        next_screen.set(Screen::Conversation);
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
            .add_systems(Update, editor_exit_on_esc.run_if(in_state(Screen::Editor)));
    }
}
