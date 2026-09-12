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
    /// Screen::Room, zoomed into a station with no keyboard of its own
    /// (Radiators)
    StationZoomed,
    /// The quick-context overlay is HELD (`Ctrl+A h`, `ui::quick_context`).
    /// Active on top of whatever surface is underneath, and outranked only
    /// by `Dialog` — a modal still owns Esc. Its single binding is Esc →
    /// `UnpinQuickContext`, which is how "exactly one PopLevel" survives an
    /// overlay that floats over every screen: the higher-priority context
    /// claims the key, so no `PopLevel` is emitted at all and the level
    /// underneath stays put (docs/input.md "Escape — two meanings total").
    QuickContext,
    /// The ask sheet is up (`ui::ask_sheet`) — an approval ask waiting on
    /// this player, raised in the context on screen. While it is up this is
    /// the ONLY surface context derived and the compose grab is suspended:
    /// the sheet owns `a A d v j k ] [` and Esc, and typed text is held, so
    /// a decision key and a typed letter can never be confused
    /// (docs/tui.md, "Asks").
    AskSheet,
    /// The ledger ribbon is up (`Ctrl+A l`, `ui::ledger_ribbon`) — every
    /// pending ask across every context, plus the last few decisions. Owns
    /// the keyboard the same way [`Self::AskSheet`] does, and outranks it:
    /// the ribbon is what `v` on the sheet opens, so its Esc returns to the
    /// sheet rather than putting the ask aside.
    LedgerRibbon,
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

/// Which approval-review surface is on screen (`ui::ask_sheet`,
/// `ui::ledger_ribbon`).
///
/// Both can be up at once — `v` on the ask sheet opens the ribbon over it —
/// so this is two flags rather than one enum, and [`Self::claimant`] is the
/// one place that says which of them has the keyboard.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ApprovalSurfaces {
    /// The ask sheet is showing an ask.
    pub sheet: bool,
    /// The ledger ribbon is open.
    pub ribbon: bool,
}

impl ApprovalSurfaces {
    /// The context that claims the keyboard, or `None` when neither surface
    /// is up. The ribbon outranks the sheet: it is what the sheet's `v`
    /// opens, so Esc there closes the ribbon and returns to the sheet
    /// instead of putting the ask aside.
    pub fn claimant(self) -> Option<InputContext> {
        if self.ribbon {
            Some(InputContext::LedgerRibbon)
        } else if self.sheet {
            Some(InputContext::AskSheet)
        } else {
            None
        }
    }
}

/// Pure derivation: (screen, zoomed station, focus, held overlay, approval
/// surfaces) → (contexts, grab).
///
/// Kept free of ECS types on the input side so it unit-tests without a
/// schedule (see `gotcha_bevy_b0001`: unit suites never init schedules).
///
/// `quick_context_held` and `approval` ride alongside the screen rather than
/// being derived from it: both float over several screens, so each is an
/// independent axis, not a state of any one surface.
pub fn derive_contexts(
    screen: Screen,
    zoomed: Option<Station>,
    focus: &FocusArea,
    quick_context_held: bool,
    approval: ApprovalSurfaces,
) -> (Vec<InputContext>, KeyboardGrab) {
    // An approval surface takes the keyboard on the screens it can raise on:
    // its own context and nothing else beneath it, so `a`/`d` cannot also
    // mean archive or demote, and `KeyboardGrab::None`, which suspends the
    // compose VimMachine and is what holds typed text (docs/tui.md, "Asks").
    //
    // Never on `Editor` or `Diff`: a vi surface owns the keys wherever it is
    // live, and under its grab only Global bindings match anyway. The raise
    // logic already declines those screens; this arm makes the doctrine hold
    // even if it did not.
    if matches!(screen, Screen::Conversation | Screen::Room)
        && let Some(claimant) = approval.claimant()
    {
        let mut contexts = vec![InputContext::Global, claimant];
        // A modal still outranks everything — the one rank these surfaces do
        // not take.
        if matches!(focus, FocusArea::Dialog) {
            contexts.push(InputContext::Dialog);
            contexts.push(InputContext::TextInput);
        }
        return (contexts, KeyboardGrab::None);
    }

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
                Some(_) => InputContext::StationZoomed,
            });
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
    sheet: Res<crate::ui::ask_sheet::AskSheetState>,
    ribbon: Res<crate::ui::ledger_ribbon::LedgerRibbonState>,
    mut active: ResMut<ActiveInputContexts>,
    mut grab: ResMut<KeyboardGrab>,
) {
    // Only update if an input changed (RoomState changes on zoom/unzoom).
    if !focus.is_changed()
        && !screen.is_changed()
        && !room.is_changed()
        && !quick.is_changed()
        && !sheet.is_changed()
        && !ribbon.is_changed()
        && !active.is_added()
    {
        return;
    }

    let approval = ApprovalSurfaces {
        sheet: sheet.up(),
        ribbon: ribbon.open,
    };
    let (contexts, new_grab) =
        derive_contexts(*screen.get(), room.zoomed, &focus, quick.held, approval);
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
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Compose, false, surfaces(false, false));
        assert!(ctxs.contains(&InputContext::Global));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }

    #[test]
    fn conversation_navigation_no_grab() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Conversation, false, surfaces(false, false));
        assert!(ctxs.contains(&InputContext::Navigation));
        assert!(!ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn dialog_gets_both_contexts() {
        let (ctxs, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Dialog, false, surfaces(false, false));
        assert!(ctxs.contains(&InputContext::Dialog));
        assert!(ctxs.contains(&InputContext::TextInput));
        assert_eq!(grab, KeyboardGrab::None);
    }

    #[test]
    fn editor_is_a_grab_with_global_only() {
        // Focus parks on Conversation while the editor owns the screen —
        // the grab must not depend on focus (the Ctrl+1/2/3 stray bug).
        let (ctxs, grab) = derive_contexts(Screen::Editor, None, &FocusArea::Conversation, false, surfaces(false, false));
        assert_eq!(ctxs, vec![InputContext::Global]);
        assert_eq!(grab, KeyboardGrab::EditorSession);
    }

    #[test]
    fn diff_view_is_a_grab_with_global_only() {
        // Same contract as the editor: focus parks on Conversation while the
        // viewer owns the screen, so the grab must not depend on focus. Only
        // Global stays matchable — in particular Navigation must NOT, or `q`
        // would fire the app's Quit binding instead of reaching DiffCore.
        let (ctxs, grab) = derive_contexts(Screen::Diff, None, &FocusArea::Conversation, false, surfaces(false, false));
        assert_eq!(ctxs, vec![InputContext::Global]);
        assert_eq!(grab, KeyboardGrab::DiffView);
    }

    #[test]
    fn diff_view_grab_holds_even_with_compose_focus() {
        // A stale FocusArea::Compose (e.g. the viewer opened from a state that
        // hadn't parked focus yet) must not hand the keyboard to the compose
        // VimMachine — the screen decides, not the focus.
        let (_, grab) = derive_contexts(Screen::Diff, None, &FocusArea::Compose, false, surfaces(false, false));
        assert_eq!(grab, KeyboardGrab::DiffView);
    }

    #[test]
    fn room_unzoomed_is_carousel() {
        let (ctxs, grab) = derive_contexts(Screen::Room, None, &FocusArea::Conversation, false, surfaces(false, false));
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
            surfaces(false, false),
        );
        assert!(ctxs.contains(&InputContext::WellZoomed));
        assert!(!ctxs.contains(&InputContext::RoomNav));
    }

    #[test]
    fn room_zoomed_plain_station() {
        let (ctxs, _) = derive_contexts(
            Screen::Room,
            Some(Station::Radiators),
            &FocusArea::Conversation,
            false,
            surfaces(false, false),
        );
        assert!(ctxs.contains(&InputContext::StationZoomed));
        assert!(!ctxs.contains(&InputContext::WellZoomed));
    }

    /// A held quick-context overlay adds its context ON TOP of whatever
    /// surface is underneath, on every screen — it floats, so it must not be
    /// a state of any one surface.
    #[test]
    fn a_held_overlay_layers_over_every_surface() {
        for (screen, zoomed, under) in [
            (Screen::Conversation, None, InputContext::Navigation),
            (Screen::Room, None, InputContext::RoomNav),
            (
                Screen::Room,
                Some(Station::TimeWell),
                InputContext::WellZoomed,
            ),
        ] {
            let (ctxs, _) = derive_contexts(screen, zoomed, &FocusArea::Conversation, true, surfaces(false, false));
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
        let (ctxs, _) = derive_contexts(Screen::Room, None, &FocusArea::Conversation, false, surfaces(false, false));
        assert!(!ctxs.contains(&InputContext::QuickContext));
    }

    /// The four approval-surface cases the tests below exercise.
    fn surfaces(sheet: bool, ribbon: bool) -> ApprovalSurfaces {
        ApprovalSurfaces { sheet, ribbon }
    }

    /// A vi surface still owns the keyboard with the overlay held: the grab
    /// is unchanged, and under a grab only `Global` bindings match, so the
    /// overlay's Esc never reaches vi's.
    #[test]
    fn a_held_overlay_never_steals_the_keyboard_from_vi() {
        let (_, grab) = derive_contexts(Screen::Editor, None, &FocusArea::Conversation, true, surfaces(false, false));
        assert_eq!(grab, KeyboardGrab::EditorSession);
        let (_, grab) = derive_contexts(Screen::Conversation, None, &FocusArea::Compose, true, surfaces(false, false));
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }

    // ── the approval surfaces ─────────────────────────────────────────────

    /// The sheet owns the keyboard outright: only its own context is
    /// derived, and the compose grab is SUSPENDED — that suspension is what
    /// holds typed text, so `a` on the sheet is an allow and never an `a`
    /// in the draft (docs/tui.md, "Asks").
    #[test]
    fn the_ask_sheet_owns_the_keyboard_and_suspends_the_compose_grab() {
        let (ctxs, grab) = derive_contexts(
            Screen::Conversation,
            None,
            &FocusArea::Compose,
            false,
            surfaces(true, false),
        );
        assert_eq!(ctxs, vec![InputContext::Global, InputContext::AskSheet]);
        assert_eq!(grab, KeyboardGrab::None, "typed text is held, not grabbed");
    }

    /// The sheet raises on the room too — the switchboard lamp says an ask
    /// is waiting there, so the answer must be reachable without leaving.
    #[test]
    fn the_ask_sheet_layers_over_the_room_as_well() {
        let (ctxs, grab) = derive_contexts(
            Screen::Room,
            Some(Station::TimeWell),
            &FocusArea::Conversation,
            false,
            surfaces(true, false),
        );
        assert_eq!(ctxs, vec![InputContext::Global, InputContext::AskSheet]);
        assert!(
            !ctxs.contains(&InputContext::WellZoomed),
            "the well's own keys must not fight the sheet's"
        );
        assert_eq!(grab, KeyboardGrab::None);
    }

    /// A vi surface owns the keys where it is live, so neither approval
    /// surface is derived on `Editor` or `Diff` — even if one were somehow
    /// flagged up, the grab wins and the contexts stay the vi ones.
    #[test]
    fn no_approval_surface_takes_keys_from_a_vi_screen() {
        for (screen, expected) in [
            (Screen::Editor, KeyboardGrab::EditorSession),
            (Screen::Diff, KeyboardGrab::DiffView),
        ] {
            for surface in [surfaces(true, false), surfaces(false, true)] {
                let (ctxs, grab) =
                    derive_contexts(screen, None, &FocusArea::Conversation, false, surface);
                assert_eq!(ctxs, vec![InputContext::Global], "{screen:?} {surface:?}");
                assert_eq!(grab, expected, "{screen:?} {surface:?}");
            }
        }
    }

    /// The ribbon is what `v` on the sheet opens, so while both are up the
    /// ribbon has the keys and the sheet waits behind it.
    #[test]
    fn the_ribbon_outranks_the_sheet_while_both_are_up() {
        let (ctxs, grab) = derive_contexts(
            Screen::Conversation,
            None,
            &FocusArea::Conversation,
            false,
            surfaces(true, true),
        );
        assert_eq!(ctxs, vec![InputContext::Global, InputContext::LedgerRibbon]);
        assert_eq!(grab, KeyboardGrab::None);
    }

    /// A modal outranks everything, the one rule the approval surfaces do
    /// not get to break.
    #[test]
    fn a_dialog_still_outranks_an_approval_surface() {
        let (ctxs, _) = derive_contexts(
            Screen::Conversation,
            None,
            &FocusArea::Dialog,
            false,
            surfaces(true, false),
        );
        assert!(ctxs.contains(&InputContext::Dialog));
        assert!(ctxs.contains(&InputContext::AskSheet));
    }

    /// Down again, nothing lingers: the ordinary surface contexts and the
    /// compose grab come straight back.
    #[test]
    fn dismissing_the_sheet_restores_the_surface_underneath() {
        let (ctxs, grab) = derive_contexts(
            Screen::Conversation,
            None,
            &FocusArea::Compose,
            false,
            surfaces(false, false),
        );
        assert!(ctxs.contains(&InputContext::TextInput));
        assert!(!ctxs.contains(&InputContext::AskSheet));
        assert_eq!(grab, KeyboardGrab::ComposeVim);
    }
}
