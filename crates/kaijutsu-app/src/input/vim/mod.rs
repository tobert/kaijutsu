//! Vi-modal editing via modalkit.
//!
//! Wraps modalkit's VimMachine as a Bevy Resource and provides the dispatch
//! system that routes compose keyboard input through the vim state machine.

pub mod dispatch;
pub mod keyconv;

use std::fmt;

use bevy::prelude::*;

use modalkit::editing::application::{
    ApplicationAction, ApplicationContentId, ApplicationError, ApplicationInfo, ApplicationStore,
    ApplicationWindowId,
};
use modalkit::editing::context::EditContext;
use modalkit::editing::store::Store;
use modalkit::env::vim::keybindings::{VimBindings, VimMachine};
use modalkit::key::TerminalKey;
use modalkit::keybindings::{BindingMachine, InputBindings, SequenceStatus};
use modalkit::prelude::CommandType;

// ---------------------------------------------------------------------------
// Application-specific types for modalkit
// ---------------------------------------------------------------------------

/// Actions specific to kaijutsu's compose overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KaijutsuAction {
    /// Submit the compose buffer (Enter in Normal mode).
    Submit,
    /// Cycle the mode ring (chat ↔ shell).
    CycleModeRing,
    /// Dismiss the compose overlay and return to conversation.
    DismissCompose,
}

impl ApplicationAction for KaijutsuAction {
    fn is_edit_sequence(&self, _ctx: &EditContext) -> SequenceStatus {
        SequenceStatus::Break
    }

    fn is_last_action(&self, _ctx: &EditContext) -> SequenceStatus {
        SequenceStatus::Atom
    }

    fn is_last_selection(&self, _ctx: &EditContext) -> SequenceStatus {
        SequenceStatus::Ignore
    }

    fn is_switchable(&self, _ctx: &EditContext) -> bool {
        false
    }
}

/// Error type for kaijutsu's modalkit integration.
#[derive(Debug)]
pub struct KaijutsuVimError(pub String);

impl fmt::Display for KaijutsuVimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ApplicationError for KaijutsuVimError {}

/// Application-specific store (empty for now — registers live in Store<KaijutsuInfo>).
pub struct KaijutsuStore;
impl ApplicationStore for KaijutsuStore {}

/// Window identifier (unused — we have one compose overlay).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KaijutsuWindowId;
impl ApplicationWindowId for KaijutsuWindowId {}

/// Content identifier for buffers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum KaijutsuContentId {
    /// The compose input buffer.
    Compose,
    /// Command bar buffer (: commands).
    Command(CommandType),
}
impl ApplicationContentId for KaijutsuContentId {}

/// The ApplicationInfo implementation that ties everything together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KaijutsuInfo {}

impl ApplicationInfo for KaijutsuInfo {
    type Error = KaijutsuVimError;
    type Action = KaijutsuAction;
    type Store = KaijutsuStore;
    type WindowId = KaijutsuWindowId;
    type ContentId = KaijutsuContentId;

    fn content_of_command(ct: CommandType) -> KaijutsuContentId {
        KaijutsuContentId::Command(ct)
    }
}

// ---------------------------------------------------------------------------
// Bevy Resource
// ---------------------------------------------------------------------------

/// Bevy Resource wrapping the modalkit VimMachine and its global store.
///
/// The VimMachine processes keystrokes and produces semantic editing actions.
/// The Store holds registers, digraphs, and other cross-buffer state.
// SAFETY: VimMachineResource is only accessed from Bevy's main thread via
// exclusive ResMut access in the Update schedule. The non-Send/Sync member is
// ModalMachine's internal Box<dyn Dialog> which we never use across threads.
// We don't use modalkit's Dialog feature (it's for TUI command-line dialogs).
unsafe impl Send for VimMachineResource {}
unsafe impl Sync for VimMachineResource {}

#[derive(Resource)]
pub struct VimMachineResource {
    /// The vim keybinding state machine.
    pub machine: VimMachine<TerminalKey, KaijutsuInfo>,
    /// Global editing store (registers, completions, etc.).
    pub store: Store<KaijutsuInfo>,
}

impl VimMachineResource {
    /// Create a new VimMachineResource with default vim keybindings.
    ///
    /// Uses `submit_on_enter()` so Enter submits the compose buffer
    /// (in all modes). Newlines are inserted via Shift+Enter or `o`/`O`.
    pub fn new() -> Self {
        let mut machine = VimMachine::<TerminalKey, KaijutsuInfo>::empty();
        VimBindings::default().submit_on_enter().setup(&mut machine);

        let store = Store::new(KaijutsuStore);

        VimMachineResource { machine, store }
    }

    /// Get a human-readable mode string (e.g. "-- INSERT --", "-- VISUAL --").
    /// Returns None in Normal mode.
    pub fn mode_display(&self) -> Option<String> {
        self.machine.show_mode()
    }
}
