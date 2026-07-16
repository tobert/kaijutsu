//! Input context — derived from FocusArea, Screen, and RoomState to determine
//! which bindings are active.
//!
//! Each frame `sync_input_context` derives the active `InputContext` set and
//! the active `KeyboardGrab`. The dispatcher checks bindings against active
//! contexts to determine matches; the grab (vi editor session, compose
//! VimMachine) receives the raw keyboard stream that the dispatcher doesn't
//! claim. See `docs/input.md`.

use bevy::prelude::*;

use super::focus::FocusArea;
use crate::ui::screen::Screen;
use crate::view::room::nav::Station;

/// Binding context — determines when a binding is active.
///
/// Multiple contexts can be active simultaneously (e.g. Global + Navigation).
/// The dispatcher matches bindings whose context is in the active set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Reflect)]
pub enum InputContext {
    /// Always active regardless of focus: F1, F12, tiling keys
    Global,
    /// Active when Compose or EditingBlock has focus: text chars, editing actions
    TextInput,
    /// Active when Conversation block list has focus: j/k, f, Tab
    Navigation,
    /// Active when a modal dialog is open: Enter/Escape/j/k
    Dialog,
    /// Screen::Room, not zoomed — the octagon station carousel
    RoomNav,
    /// Screen::Room, zoomed into the time well
    WellZoomed,
    /// Screen::Room, zoomed into the patch bay
    PatchBayZoomed,
    /// Screen::Room, zoomed into a station with no keyboard of its own
    /// (Tracks / Vfs / Radiators)
    StationZoomed,
    /// Screen::Fsn — landscape camera fly + select
    FsnFly,
}

/// Exclusive keyboard capture — who receives raw keyboard events that the
/// dispatcher doesn't claim via Global bindings.
///
/// When a grab is active the dispatcher matches **only Global-context
/// bindings** (F1/F12/tiling stay live everywhere); every other pressed key
/// is routed to the grab owner as a [`super::events::GrabbedKey`] message.
/// This replaces the old implicit rule "vim owns the keyboard when TextInput
/// is active" and the Editor/Room context-suppression list.
#[derive(Resource, Clone, Copy, Default, PartialEq, Eq, Debug, Reflect)]
#[reflect(Resource)]
pub enum KeyboardGrab {
    /// No grab — bindings match across all active contexts.
    #[default]
    None,
    /// The compose overlay's VimMachine (chat or shell surface).
    ComposeVim,
    /// The in-app vi editor forwarding to a kernel editor session.
    EditorSession,
}

/// Resource tracking which input contexts are currently active.
///
/// Derived each frame by `sync_input_context` from `FocusArea` +
/// `State<Screen>` + `RoomState`. The dispatcher reads this to determine
/// which bindings to evaluate.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct ActiveInputContexts(pub Vec<InputContext>);

impl ActiveInputContexts {
    /// Check if a context is currently active.
    pub fn contains(&self, ctx: InputContext) -> bool {
        self.0.contains(&ctx)
    }
}

/// Pure derivation: (screen, zoomed station, focus) → (contexts, grab).
///
/// Kept free of ECS types on the input side so it unit-tests without a
/// schedule (see `gotcha_bevy_b0001`: unit suites never init schedules).
pub fn derive_contexts(
    screen: Screen,
    zoomed: Option<Station>,
    focus: &FocusArea,
) -> (Vec<InputContext>, KeyboardGrab) {
    let mut contexts = vec![InputContext::Global];

    match screen {
        // The vi editor owns the keyboard as an explicit grab; only Global
        // bindings stay matchable (F12 screenshot in the editor still works).
        Screen::Editor => return (contexts, KeyboardGrab::EditorSession),

        // The room derives its context from the zoom state: the carousel at
        // room scale, a per-station context while zoomed. The old rule
        // "Room screen suppresses everything but Global" is now expressed
        // positively — conversation contexts simply aren't derived here.
        Screen::Room => {
            contexts.push(match zoomed {
                None => InputContext::RoomNav,
                Some(Station::TimeWell) => InputContext::WellZoomed,
                Some(Station::PatchBay) => InputContext::PatchBayZoomed,
                Some(_) => InputContext::StationZoomed,
            });
            return (contexts, KeyboardGrab::None);
        }

        // The FSN landscape was previously *forgotten* by the suppression
        // list (latent Esc double-fire); deriving its own context fixes
        // that structurally.
        Screen::Fsn => {
            contexts.push(InputContext::FsnFly);
            return (contexts, KeyboardGrab::None);
        }

        Screen::Conversation => {}
    }

    // Within-conversation focus areas.
    match focus {
        FocusArea::Compose => {
            contexts.push(InputContext::TextInput);
            // The VimMachine owns the keyboard while composing.
            return (contexts, KeyboardGrab::ComposeVim);
        }
        FocusArea::Conversation => {
            contexts.push(InputContext::Navigation);
        }
        FocusArea::Dialog => {
            contexts.push(InputContext::Dialog);
            contexts.push(InputContext::TextInput);
        }
    }

    (contexts, KeyboardGrab::None)
}

/// System: derive active input contexts + keyboard grab each frame.
pub fn sync_input_context(
    focus: Res<FocusArea>,
    screen: Res<State<Screen>>,
    room: Res<crate::view::room::RoomState>,
    mut active: ResMut<ActiveInputContexts>,
    mut grab: ResMut<KeyboardGrab>,
) {
    // Only update if an input changed (RoomState changes on zoom/unzoom).
    if !focus.is_changed() && !screen.is_changed() && !room.is_changed() && !active.is_added() {
        return;
    }

    let (contexts, new_grab) = derive_contexts(*screen.get(), room.zoomed, &focus);
    active.0 = contexts;
    // Avoid spurious change-detection on the grab resource.
    if *grab != new_grab {
        *grab = new_grab;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_compose_grabs_for_vim() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Compose);
        assert!(ctxs.contains(&InputContext::Global));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }

    #[test]
    fn conversation_navigation_no_grab() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Conversation);
        assert!(ctxs.contains(&InputContext::Navigation));
        assert!(!ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn dialog_gets_both_contexts() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Dialog);
        assert!(ctxs.contains(&InputContext::Dialog));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn editor_is_a_grab_with_global_only() {
        // Focus parks on Conversation while the editor owns the screen —
        // the grab must not depend on focus (the Ctrl+1/2/3 stray bug).
        let (ctxs, grab) = derive_contexts(Screen::Editor, None, &FocusArea::Conversation);
        assert_eq!(ctxs, vec![InputContext::Global]);
        assert_eq!(grab, KeyboardGrab::EditorSession);
    }

    #[test]
    fn room_unzoomed_is_carousel() {
        let (ctxs, grab) = derive_contexts(Screen::Room, None, &FocusArea::Conversation);
        assert!(ctxs.contains(&InputContext::RoomNav));
        assert!(!ctxs.contains(&InputContext::Navigation));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn room_zoomed_well() {
        let (ctxs, _) = derive_contexts(
            Screen::Room,
            Some(Station::TimeWell),
            &FocusArea::Conversation,
        );
        assert!(ctxs.contains(&InputContext::WellZoomed));
        assert!(!ctxs.contains(&InputContext::RoomNav));
    }

    #[test]
    fn room_zoomed_patch_bay() {
        let (ctxs, _) = derive_contexts(
            Screen::Room,
            Some(Station::PatchBay),
            &FocusArea::Conversation,
        );
        assert!(ctxs.contains(&InputContext::PatchBayZoomed));
    }

    #[test]
    fn room_zoomed_plain_station() {
        let (ctxs, _) = derive_contexts(
            Screen::Room,
            Some(Station::Tracks),
            &FocusArea::Conversation,
        );
        assert!(ctxs.contains(&InputContext::StationZoomed));
        assert!(!ctxs.contains(&InputContext::WellZoomed));
    }

    #[test]
    fn fsn_has_its_own_context_not_navigation() {
        // The old suppression list forgot Fsn — Navigation leaked in and
        // central Esc→pop double-fired with fsn_keyboard's Esc.
        let (ctxs, grab) = derive_contexts(Screen::Fsn, None, &FocusArea::Conversation);
        assert!(ctxs.contains(&InputContext::FsnFly));
        assert!(!ctxs.contains(&InputContext::Navigation));
        assert_eq!(grab, KeyboardGrab::None);
    }
}
