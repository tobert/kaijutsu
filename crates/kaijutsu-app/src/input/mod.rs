//! Input system — focus-based action dispatch.
//!
//! Replaces the vim-style modal input with a focus-based model inspired by 4X strategy games.
//! What's focused determines available actions, not a global mode.
//!
//! ## Architecture (docs/input.md)
//!
//! ```text
//! Raw Input (Keyboard, Gamepad, MouseWheel)
//!     │
//!     ▼
//! sync_input_context    — derives InputContexts + KeyboardGrab from
//!     │                    FocusArea + Screen + RoomState
//!     ▼
//! dispatch_input        — the ONLY raw-keyboard reader
//!     │                    matches bindings in active contexts
//!     │                    (Global only while a grab is held)
//!     ├──────────┬───────────────┐
//!     ▼          ▼               ▼
//! ActionFired  GrabbedKey    AnalogInput
//!     │          │ (compose VimMachine / vi editor session)
//!     ▼          ▼
//! Domain handlers consume actions; grab owners consume keys
//! ```
//!
//! ## BRP Introspection
//!
//! All key types are BRP-reflectable:
//! - `FocusArea` — what has focus right now
//! - `InputMap` — all bindings (readable + mutable)
//! - `ActiveInputContexts` — which contexts are active

pub mod action;
pub mod binding;
pub mod context;
pub mod defaults;
pub mod dispatch;
pub mod interrupt;
pub mod events;
pub mod focus;
pub mod map;
pub mod bindings_config;
pub mod prefix;
pub mod systems;
pub mod tap;
pub mod vim;

// Re-export core types for ergonomic use.
// FocusArea is consumed by cell, dock, timeline, conversation, frame_assembly.
// others are pub API for future external consumers.
#[allow(unused_imports)]
pub use action::Action;
#[allow(unused_imports)]
pub use context::InputContext;
#[allow(unused_imports)]
pub use events::{ActionFired, TextInputReceived};
pub use focus::FocusArea;
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
        // Write default config files (bindings.toml + theme.toml) if not present
        crate::config::write_default_configs_if_missing();

        // Register messages
        app.add_message::<events::ActionFired>()
            .add_message::<events::TextInputReceived>()
            .add_message::<events::GrabbedKey>()
            .add_message::<events::LiteralPrefix>();

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
            .init_resource::<focus::ActiveSurface>()
            .init_resource::<map::InputMap>()
            .init_resource::<context::ActiveInputContexts>()
            .init_resource::<context::KeyboardGrab>()
            .init_resource::<prefix::PrefixState>()
            .init_resource::<events::AnalogInput>()
            .init_resource::<interrupt::InterruptState>()
            .insert_resource(vim::VimMachineResource::new())
            .init_resource::<vim::dispatch::VimMotionState>()
            .init_resource::<vim::dismiss::EscapeDismissState>();

        // Register types for BRP reflection
        app.register_type::<focus::FocusArea>()
            .register_type::<focus::ActiveSurface>()
            .register_type::<map::InputMap>()
            .register_type::<context::ActiveInputContexts>()
            .register_type::<context::InputContext>()
            .register_type::<context::KeyboardGrab>()
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
            (context::sync_input_context,).in_set(InputPhase::SyncContext),
        );

        // Dispatch phase: raw input → ActionFired/GrabbedKey.
        // dispatch_input is the only raw-keyboard reader; grab owners consume
        // the GrabbedKey stream it emits, so they must run after it to see
        // this frame's keys (the vi editor orders itself the same way).
        app.add_systems(
            Update,
            (
                dispatch::dispatch_input,
                vim::dispatch::vim_dispatch_compose
                    .run_if(bevy::ecs::prelude::resource_equals(
                        context::KeyboardGrab::ComposeVim,
                    ))
                    .after(dispatch::dispatch_input),
            )
                .in_set(InputPhase::Dispatch),
        );

        // Handle phase: consume ActionFired for focus management + domain actions
        app.add_systems(
            Update,
            (
                // Focus management (global)
                systems::handle_focus_cycle,
                systems::handle_focus_compose,
                systems::handle_toggle_surface,
                systems::handle_pop_level,
                systems::handle_detach,
                systems::handle_prompt_prefill,
                systems::handle_interrupt,
                // App-level actions (global)
                systems::handle_quit,
                systems::handle_debug_toggle,
                systems::handle_screenshot,
                // Tiling pane management (global)
                systems::handle_tiling,
                // Navigation context
                systems::handle_navigate_blocks.run_if(focus::in_conversation),
                systems::handle_collapse_toggle.run_if(focus::in_conversation),
                systems::handle_toggle_block_excluded.run_if(focus::in_conversation),
                // Scrolling (multi-context)
                systems::handle_scroll.run_if(focus::scroll_context_active),
                // Text input context
                systems::handle_compose_input.run_if(focus::in_compose),
            )
                .in_set(InputPhase::Handle),
        );

        // Cleanup phase: defensive logic
        app.add_systems(
            Update,
            systems::cleanup_stale_focused_markers.in_set(InputPhase::Cleanup),
        );
    }
}
