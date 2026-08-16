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
    /// The quick-context overlay is HELD (`Ctrl+A h`, `ui::quick_context`).
    /// Active on top of whatever surface is underneath, and outranked only
    /// by `Dialog` — a modal still owns Esc. Its single binding is Esc →
    /// `UnpinQuickContext`, which is how "exactly one PopLevel" survives an
    /// overlay that floats over every screen: the higher-priority context
    /// claims the key, so no `PopLevel` is emitted at all and the level
    /// underneath stays put (docs/input.md "Escape — two meanings total").
    QuickContext,
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
    /// The full diff viewer's app-local `DiffCore` (`view::diff_view`) — the
    /// `ComposeVim` precedent: a modalkit machine behind the grab, so Global
    /// bindings and the Ctrl+A prefix still win, and Esc reaches the vi
    /// surface instead of popping the screen.
    DiffView,
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

/// Pure derivation: (screen, zoomed station, focus, held overlay) →
/// (contexts, grab).
///
/// Kept free of ECS types on the input side so it unit-tests without a
/// schedule (see `gotcha_bevy_b0001`: unit suites never init schedules).
///
/// `quick_context_held` rides alongside the screen rather than being derived
/// from it: the overlay floats over every screen, so it is a fourth
/// independent axis, not a state of any one surface.
pub fn derive_contexts(
    screen: Screen,
    zoomed: Option<Station>,
    focus: &FocusArea,
    quick_context_held: bool,
) -> (Vec<InputContext>, KeyboardGrab) {
    let mut contexts = vec![InputContext::Global];
    if quick_context_held {
        // Pushed before every early return below so the held overlay owns
        // its Esc on the scenes too. Under a keyboard grab (editor, diff,
        // compose vim) only Global bindings are matchable, so this is inert
        // there — which is the doctrine, not an oversight: Esc belongs to vi
        // wherever a vi surface is live, and `Ctrl+A h` releases the hold.
        contexts.push(InputContext::QuickContext);
    }

    match screen {
        // The vi editor owns the keyboard as an explicit grab; only Global
        // bindings stay matchable (F12 screenshot in the editor still works).
        Screen::Editor => return (contexts, KeyboardGrab::EditorSession),

        // The diff viewer is the editor's shape exactly: an explicit grab
        // feeding a vi machine, Global bindings still matchable. Its `q`
        // closes the screen and its Esc does NOT — both are the grab
        // owner's calls, not the dispatcher's.
        Screen::Diff => return (contexts, KeyboardGrab::DiffView),

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
    quick: Res<crate::ui::quick_context::QuickContextState>,
    mut active: ResMut<ActiveInputContexts>,
    mut grab: ResMut<KeyboardGrab>,
) {
    // Only update if an input changed (RoomState changes on zoom/unzoom).
    if !focus.is_changed()
        && !screen.is_changed()
        && !room.is_changed()
        && !quick.is_changed()
        && !active.is_added()
    {
        return;
    }

    let (contexts, new_grab) = derive_contexts(*screen.get(), room.zoomed, &focus, quick.held);
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
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Compose, false);
        assert!(ctxs.contains(&InputContext::Global));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }

    #[test]
    fn conversation_navigation_no_grab() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Conversation, false);
        assert!(ctxs.contains(&InputContext::Navigation));
        assert!(!ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn dialog_gets_both_contexts() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Dialog, false);
        assert!(ctxs.contains(&InputContext::Dialog));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn editor_is_a_grab_with_global_only() {
        // Focus parks on Conversation while the editor owns the screen —
        // the grab must not depend on focus (the Ctrl+1/2/3 stray bug).
        let (ctxs, grab) = derive_contexts(Screen::Editor, None, &FocusArea::Conversation, false);
        assert_eq!(ctxs, vec![InputContext::Global]);
        assert_eq!(grab, KeyboardGrab::EditorSession);
    }

    #[test]
    fn diff_view_is_a_grab_with_global_only() {
        // Same contract as the editor: focus parks on Conversation while the
        // viewer owns the screen, so the grab must not depend on focus. Only
        // Global stays matchable — in particular Navigation must NOT, or `q`
        // would fire the app's Quit binding instead of reaching DiffCore.
        let (ctxs, grab) = derive_contexts(Screen::Diff, None, &FocusArea::Conversation, false);
        assert_eq!(ctxs, vec![InputContext::Global]);
        assert_eq!(grab, KeyboardGrab::DiffView);
    }

    #[test]
    fn diff_view_grab_holds_even_with_compose_focus() {
        // A stale FocusArea::Compose (e.g. the viewer opened from a state that
        // hadn't parked focus yet) must not hand the keyboard to the compose
        // VimMachine — the screen decides, not the focus.
        let (_, grab) = derive_contexts(Screen::Diff, None, &FocusArea::Compose, false);
        assert_eq!(grab, KeyboardGrab::DiffView);
    }

    #[test]
    fn room_unzoomed_is_carousel() {
        let (ctxs, grab) = derive_contexts(Screen::Room, None, &FocusArea::Conversation, false);
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
            false,
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
            false,
        );
        assert!(ctxs.contains(&InputContext::PatchBayZoomed));
    }

    #[test]
    fn room_zoomed_plain_station() {
        let (ctxs, _) = derive_contexts(
            Screen::Room,
            Some(Station::Tracks),
            &FocusArea::Conversation,
            false,
        );
        assert!(ctxs.contains(&InputContext::StationZoomed));
        assert!(!ctxs.contains(&InputContext::WellZoomed));
    }

    #[test]
    fn fsn_has_its_own_context_not_navigation() {
        // The old suppression list forgot Fsn — Navigation leaked in and
        // central Esc→pop double-fired with fsn_keyboard's Esc.
        let (ctxs, grab) = derive_contexts(Screen::Fsn, None, &FocusArea::Conversation, false);
        assert!(ctxs.contains(&InputContext::FsnFly));
        assert!(!ctxs.contains(&InputContext::Navigation));
        assert_eq!(grab, KeyboardGrab::None);
    }

    /// A held quick-context overlay adds its context ON TOP of whatever
    /// surface is underneath, on every screen — it floats, so it must not be
    /// a state of any one surface.
    #[test]
    fn a_held_overlay_layers_over_every_surface() {
        for (screen, zoomed, under) in [
            (Screen::Conversation, None, InputContext::Navigation),
            (Screen::Room, None, InputContext::RoomNav),
            (Screen::Fsn, None, InputContext::FsnFly),
            (
                Screen::Room,
                Some(Station::TimeWell),
                InputContext::WellZoomed,
            ),
        ] {
            let (ctxs, _) = derive_contexts(screen, zoomed, &FocusArea::Conversation, true);
            assert!(
                ctxs.contains(&InputContext::QuickContext),
                "held overlay missing on {screen:?}"
            );
            assert!(
                ctxs.contains(&under),
                "the surface under the overlay must stay live on {screen:?}"
            );
        }
    }

    /// Releasing the hold takes the context away again — nothing lingers to
    /// swallow a later Esc.
    #[test]
    fn a_released_overlay_leaves_no_context_behind() {
        let (ctxs, _) = derive_contexts(Screen::Room, None, &FocusArea::Conversation, false);
        assert!(!ctxs.contains(&InputContext::QuickContext));
    }

    /// A vi surface still owns the keyboard with the overlay held: the grab
    /// is unchanged, and under a grab only `Global` bindings match, so the
    /// overlay's Esc never reaches vi's.
    #[test]
    fn a_held_overlay_never_steals_the_keyboard_from_vi() {
        let (_, grab) = derive_contexts(Screen::Editor, None, &FocusArea::Conversation, true);
        assert_eq!(grab, KeyboardGrab::EditorSession);
        let (_, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Compose, true);
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }
}
