//! Input system — focus-based action dispatch.
//!
//! Replaces the vim-style modal input with a focus-based model inspired by 4X strategy games.
//! What's focused determines available actions, not a global mode.
//!
//! ## Architecture
//!
//! ```text
//! Raw Input (Keyboard, Gamepad, MouseWheel)
//!     │
//!     ▼
//! sync_input_context    — derives InputContext from FocusArea
//!     │
//!     ▼
//! dispatch_input        — ONE system, reads ALL raw input
//!     │                    matches bindings in active context
//!     │                    handles sequences (g→t)
//!     ├──────┬──────┐
//!     │      │      │
//!     ▼      ▼      ▼
//! ActionFired  TextInputReceived   (Bevy messages)
//!     │             │
//!     ▼             ▼
//! Domain handlers consume actions
//! ```
//!
//! ## BRP Introspection
//!
//! All key types are BRP-reflectable:
//! - `FocusArea` — what has focus right now
//! - `InputMap` — all bindings (readable + mutable)
//! - `ActiveInputContexts` — which contexts are active
//! - `SequenceState` — pending multi-key sequence

pub mod action;
pub mod binding;
pub mod context;
pub mod defaults;
pub mod dispatch;
pub mod events;
pub mod focus;
pub mod map;
pub mod sequence;
pub mod systems;

// Re-export core types for ergonomic use.
// FocusArea is consumed by cell, tiling_widgets, timeline, conversation, frame_assembly.
// others are pub API for future external consumers.
pub use focus::FocusArea;
#[allow(unused_imports)]
pub use action::Action;
#[allow(unused_imports)]
pub use context::InputContext;
#[allow(unused_imports)]
pub use events::{ActionFired, TextInputReceived};
#[allow(unused_imports)]
pub use map::InputMap;

use bevy::prelude::*;

/// System clipboard access via arboard.
///
/// Inserted as a resource during plugin init. When the OS clipboard is
/// unavailable (headless, no display server), this resource is absent
/// and clipboard actions silently no-op.
#[derive(Resource)]
pub struct SystemClipboard(pub arboard::Clipboard);

/// SystemSet for input dispatch — runs before all domain input handling.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputPhase {
    /// Derive active contexts from focus state + sync from screen
    SyncContext,
    /// Dispatch raw input → ActionFired / TextInputReceived
    Dispatch,
    /// Handle actions (focus cycling, debug, etc.)
    Handle,
    /// Defensive cleanup of stale state
    Cleanup,
}

/// Plugin that registers the focus-based input dispatch system.
///
/// Handles keyboard, mouse wheel, and gamepad input. All raw input flows
/// through `dispatch_input` which emits `ActionFired` / `TextInputReceived`
/// messages for domain systems to consume.
pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<events::ActionFired>()
            .add_message::<events::TextInputReceived>();

        // System clipboard (graceful fallback if unavailable)
        match arboard::Clipboard::new() {
            Ok(clipboard) => {
                app.insert_resource(SystemClipboard(clipboard));
            }
            Err(e) => {
                warn!("System clipboard unavailable: {e}. Copy/paste disabled.");
            }
        }

        // Register resources
        app.init_resource::<focus::FocusArea>()
            .init_resource::<focus::FocusStack>()
            .init_resource::<map::InputMap>()
            .init_resource::<context::ActiveInputContexts>()
            .init_resource::<sequence::SequenceState>()
            .init_resource::<events::AnalogInput>();

        // Register types for BRP reflection
        app.register_type::<focus::FocusArea>()
            .register_type::<focus::FocusStack>()
            .register_type::<map::InputMap>()
            .register_type::<context::ActiveInputContexts>()
            .register_type::<context::InputContext>()
            .register_type::<sequence::SequenceState>()
            .register_type::<events::AnalogInput>()
            .register_type::<events::ActionFired>()
            .register_type::<events::TextInputReceived>()
            .register_type::<action::Action>()
            .register_type::<binding::Binding>()
            .register_type::<binding::InputSource>()
            .register_type::<binding::Modifiers>();

        // Configure system ordering
        app.configure_sets(
            Update,
            (
                InputPhase::SyncContext,
                InputPhase::Dispatch.after(InputPhase::SyncContext),
                InputPhase::Handle.after(InputPhase::Dispatch),
                InputPhase::Cleanup.after(InputPhase::Handle),
            ),
        );

        // SyncContext phase: derive focus + contexts
        app.add_systems(
            Update,
            (
                context::sync_input_context,
            )
                .in_set(InputPhase::SyncContext),
        );

        // Dispatch phase: raw input → ActionFired/TextInputReceived
        app.add_systems(
            Update,
            dispatch::dispatch_input.in_set(InputPhase::Dispatch),
        );

        // Handle phase: consume ActionFired for focus management + domain actions
        app.add_systems(
            Update,
            (
                // Focus management (global)
                systems::handle_focus_cycle,
                systems::handle_focus_compose,
                systems::handle_unfocus,
                systems::handle_toggle_constellation,
                // App-level actions (global)
                systems::handle_quit,
                systems::handle_debug_toggle,
                systems::handle_screenshot,
                // Tiling pane management (global)
                systems::handle_tiling,
                // Navigation context
                systems::handle_navigate_blocks.run_if(focus::in_conversation),
                systems::handle_expand_block.run_if(focus::in_conversation),
                systems::handle_collapse_toggle.run_if(focus::in_conversation),
                systems::handle_timeline.run_if(focus::in_conversation),
                systems::handle_activate_navigation.run_if(focus::in_conversation),
                // Constellation context
                systems::handle_constellation_nav.run_if(focus::in_constellation),
                // Scrolling (multi-context)
                systems::handle_scroll.run_if(focus::scroll_context_active),
                // Text input contexts
                systems::handle_compose_input.run_if(focus::in_compose),
                systems::handle_block_edit_input.run_if(focus::in_editing_block),
            )
                .in_set(InputPhase::Handle),
        );

        // Cleanup phase: defensive logic
        app.add_systems(
            Update,
            (
                systems::cleanup_stale_editing_markers,
                systems::cleanup_stale_focused_markers,
            )
                .in_set(InputPhase::Cleanup),
        );
    }
}
