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
    /// Screen::HorizonDive — snap navigation over the horizon search space
    /// (`docs/horizon-dive.md`). Active only while the query line does NOT
    /// hold the keyboard; typing is a grab, not a context.
    HorizonDive,
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
    /// The horizon dive's query line (`view::horizon_dive`). A text field
    /// needs the whole alphabet; asking for it as a declared grab is the
    /// sanctioned way (docs/input.md, "Keyboard grabs are explicit") and it
    /// keeps the Ctrl+A prefix, F1 and F12 alive while you type.
    HorizonQuery,
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
/// `dive_typing` is the horizon dive's query-line mode. It is a parameter
/// rather than something derived from `screen` because it is genuinely a
/// third axis: the same screen has two keyboard regimes, exactly like
/// `Screen::Conversation` does across `FocusArea`.
pub fn derive_contexts(
    screen: Screen,
    zoomed: Option<Station>,
    focus: &FocusArea,
    dive_typing: bool,
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

        // The dive's two regimes. While the query line has the keyboard the
        // navigation context is NOT active — that's what stops `p` (surface)
        // and `h/j/k/l` (snap) from firing out from under the letters you
        // meant to type, without a single "am I typing?" check inside a
        // domain handler.
        Screen::HorizonDive => {
            if dive_typing {
                return (contexts, KeyboardGrab::HorizonQuery);
            }
            contexts.push(InputContext::HorizonDive);
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
    // The dive's query-line mode. Its own small resource, not a field on
    // `HorizonDiveState`: that one changes every frame the lights do, and
    // reading the mode off it would defeat the change-detection gate below.
    dive: Res<crate::view::horizon_dive::DiveTyping>,
    mut active: ResMut<ActiveInputContexts>,
    mut grab: ResMut<KeyboardGrab>,
) {
    // Only update if an input changed (RoomState changes on zoom/unzoom).
    if !focus.is_changed()
        && !screen.is_changed()
        && !room.is_changed()
        && !dive.is_changed()
        && !active.is_added()
    {
        return;
    }

    let (contexts, new_grab) = derive_contexts(*screen.get(), room.zoomed, &focus, dive.0);
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

    #[test]
    fn horizon_dive_navigating_is_a_context_not_a_grab() {
        let (ctxs, grab) =
            derive_contexts(Screen::HorizonDive, None, &FocusArea::Conversation, false);
        assert!(ctxs.contains(&InputContext::HorizonDive));
        assert!(!ctxs.contains(&InputContext::Navigation));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn horizon_dive_typing_takes_the_grab_and_drops_the_nav_context() {
        // The whole point of routing the query line through a grab: while
        // you're typing, `p` must insert a letter, not surface a context.
        let (ctxs, grab) =
            derive_contexts(Screen::HorizonDive, None, &FocusArea::Conversation, true);
        assert_eq!(grab, KeyboardGrab::HorizonQuery);
        assert_eq!(
            ctxs,
            vec![InputContext::Global],
            "only Global survives a grab — F1/F12/tiling stay live while typing"
        );
    }

    #[test]
    fn the_dive_mode_does_not_leak_into_other_screens() {
        // `dive_typing` is only meaningful on `Screen::HorizonDive`; a stale
        // `true` must not grab the keyboard anywhere else.
        for screen in [Screen::Conversation, Screen::Room, Screen::Fsn] {
            let (_, grab) = derive_contexts(screen, None, &FocusArea::Conversation, true);
            assert_ne!(grab, KeyboardGrab::HorizonQuery, "{screen:?} took the dive's grab");
        }
    }
}
