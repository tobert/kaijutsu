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
// Others are pub API for future external consumers.
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

/// SystemSet for input dispatch — runs before all domain input handling.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum InputPhase {
    /// Derive active contexts from focus state + sync from screen
    SyncContext,
    /// Dispatch raw input → ActionFired / TextInputReceived
    Dispatch,
    /// Handle actions (focus cycling, debug, etc.)
    Handle,
}

/// Plugin that registers the focus-based input dispatch system.
pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        // Register messages
        app.add_message::<events::ActionFired>()
            .add_message::<events::TextInputReceived>();

        // Register resources
        app.init_resource::<focus::FocusArea>()
            .init_resource::<map::InputMap>()
            .init_resource::<context::ActiveInputContexts>()
            .init_resource::<sequence::SequenceState>()
            .init_resource::<events::AnalogInput>();

        // Register types for BRP reflection
        app.register_type::<focus::FocusArea>()
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
            ),
        );

        // SyncContext phase: derive focus + contexts
        app.add_systems(
            Update,
            (
                systems::sync_focus_from_screen,
                context::sync_input_context.after(systems::sync_focus_from_screen),
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
                // Focus management
                systems::handle_focus_cycle,
                systems::handle_focus_compose,
                systems::handle_unfocus,
                systems::handle_toggle_constellation,
                // App-level actions
                systems::handle_quit,
                systems::handle_debug_toggle,
                systems::handle_screenshot,
                // Block navigation + scroll
                systems::handle_navigate_blocks,
                systems::handle_scroll,
                systems::handle_expand_block,
                systems::handle_collapse_toggle,
                systems::handle_view_pop,
                // Tiling pane management
                systems::handle_tiling,
                // Constellation spatial nav
                systems::handle_constellation_nav,
                // Text input (compose + inline block editing)
                systems::handle_compose_input,
                systems::handle_block_edit_input,
            )
                .in_set(InputPhase::Handle),
        );
    }
}
