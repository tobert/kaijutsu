//! Conversation Stack — 3D cascading card layout for conversation history.
//!
//! Each conversation turn (role-group) becomes a flat rectangular mesh in
//! perspective 3D space. One card is focused in the foreground at full size;
//! behind it, cards cascade into z-depth with per-step x,y offset.
//!
//! ## Architecture
//!
//! Cards are M:N with blocks — a card groups consecutive blocks by role.
//! Each block within a card is its own child quad mesh, sharing the block's
//! existing RTT texture handle via StandardMaterial (unlit).
//!
//! Custom StackCardMaterial with LOD + holographic glow is deferred until
//! the AsBindGroup shader binding issue is resolved.

pub mod camera;
pub mod layout;
pub mod material; // deferred — custom shader needs debugging
pub mod sync;

use bevy::prelude::*;

use crate::ui::screen::Screen;

pub use camera::StackCameraTag;
pub use layout::{CardLod, CardStackLayout, CardStackState};
pub use sync::StackCard;

/// Plugin for the Conversation Stack 3D view.
pub struct CardStackPlugin;

impl Plugin for CardStackPlugin {
    fn build(&self, app: &mut App) {
        // Resources
        app.init_resource::<CardStackState>()
            .init_resource::<CardStackLayout>();

        // Type registration for BRP inspection
        app.register_type::<CardStackState>()
            .register_type::<CardStackLayout>()
            .register_type::<CardLod>()
            .register_type::<StackCard>()
            .register_type::<StackCameraTag>();

        // Screen transitions
        app.add_systems(
            OnEnter(Screen::ConversationStack),
            (
                camera::spawn_stack_camera,
                sync::sync_stack_cards,
            )
                .chain(),
        );
        app.add_systems(
            OnExit(Screen::ConversationStack),
            (
                camera::despawn_stack_camera,
                sync::despawn_all_cards,
            ),
        );

        // Per-frame systems (only when stack is active)
        app.add_systems(
            Update,
            (
                sync::sync_stack_cards,
                layout::compute_card_layout,
            )
                .chain()
                .run_if(in_state(Screen::ConversationStack)),
        );
    }
}
