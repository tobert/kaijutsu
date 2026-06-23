//! In-app vi editor — the renderer + key-forwarder side (Design A).
//!
//! The kernel owns the editor session; the app is one *renderer* of it. The
//! `open_editor` peer signal lands here as [`EditorOpenRequested`], drives
//! `Screen::Editor`, and stores the live session in [`ActiveEditor`]. The MSDF
//! panel renderer (fed by the `editor_state` subscription) and key forwarding to
//! `editor_keys` land in later slices (docs/vi.md steps 4–5); this is the
//! screen/landing foundation.

use bevy::prelude::*;

use crate::ui::screen::Screen;

/// A kernel `open_editor` signal landed: the submitter ran `vi <path>` and the
/// kernel opened session `session`. Written by the peer dispatcher
/// (`peers::systems`), consumed by [`handle_editor_open`].
#[derive(Message, Clone, Debug)]
pub struct EditorOpenRequested {
    /// The kernel editor session handle to render.
    pub session: u64,
    /// The path being edited (display / title).
    pub path: String,
}

/// The editor session the app is currently rendering, if any. Set when an
/// `open_editor` signal lands; cleared on exit. The MSDF renderer (step 4) reads
/// the session id to subscribe/fetch `editor_state`.
#[derive(Resource, Default, Debug)]
pub struct ActiveEditor {
    /// The live session, or `None` when no editor is open.
    pub session: Option<EditorSessionView>,
}

/// The minimal session identity the renderer needs.
// `session`/`path` are written by the landing handler now and read by the MSDF
// renderer + key forwarder in docs/vi.md steps 4–5 (not yet wired).
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct EditorSessionView {
    /// Kernel editor session handle (the currency of `editor_keys`/`editor_state`).
    pub session: u64,
    /// The path being edited.
    pub path: String,
}

/// Land an `open_editor` signal: store the session and reveal `Screen::Editor`.
/// The mirror of `handle_context_switch`'s screen reveal — the editor drives its
/// own screen from its own landing handler, not through `view/sync.rs`.
fn handle_editor_open(
    mut events: MessageReader<EditorOpenRequested>,
    mut active: ResMut<ActiveEditor>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    for ev in events.read() {
        info!("open_editor: session {} on {}", ev.session, ev.path);
        active.session = Some(EditorSessionView {
            session: ev.session,
            path: ev.path.clone(),
        });
        next_screen.set(Screen::Editor);
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
            .add_systems(Update, handle_editor_open)
            .add_systems(Update, editor_exit_on_esc.run_if(in_state(Screen::Editor)));
    }
}
