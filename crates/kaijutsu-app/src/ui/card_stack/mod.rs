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
//! Custom StackCardMaterial provides per-card opacity and role-colored
//! edge glow via `stack_card.wgsl`.

pub mod camera;
pub mod layout;
pub mod material;
pub mod sync;

use bevy::prelude::*;

use crate::ui::card_stack::material::StackCardMaterial;
use crate::ui::screen::Screen;

pub use camera::StackCameraTag;
pub use layout::{
    CardLod, CardStackLayout, CardStackState, GapMarker, ReadingTransition, StackAnimPhase,
    StackViewMode,
};
pub use sync::StackCard;

/// Plugin for the Conversation Stack 3D view.
pub struct CardStackPlugin;

impl Plugin for CardStackPlugin {
    fn build(&self, app: &mut App) {
        // Resources
        app.init_resource::<CardStackState>()
            .init_resource::<CardStackLayout>()
            .init_resource::<StackAnimPhase>()
            .init_resource::<StackViewMode>()
            .init_resource::<ReadingTransition>();

        // Material registration
        app.add_plugins(MaterialPlugin::<StackCardMaterial>::default());

        // Type registration for BRP inspection
        app.register_type::<CardStackState>()
            .register_type::<CardStackLayout>()
            .register_type::<CardLod>()
            .register_type::<StackCard>()
            .register_type::<StackCameraTag>()
            .register_type::<StackAnimPhase>()
            .register_type::<StackViewMode>()
            .register_type::<ReadingTransition>()
            .register_type::<GapMarker>();

        // Screen transitions
        app.add_systems(
            OnEnter(Screen::ConversationStack),
            (
                camera::spawn_stack_camera,
                sync::sync_stack_cards,
                |mut state: ResMut<CardStackState>,
                 mut anim: ResMut<StackAnimPhase>,
                 mut view_mode: ResMut<StackViewMode>,
                 mut reading_tx: ResMut<ReadingTransition>| {
                    state.current_focus = state.focused_index as f32;
                    state.last_focus = state.current_focus;
                    *anim = StackAnimPhase::Entering { progress: 0.0 };
                    *view_mode = StackViewMode::Browse;
                    reading_tx.progress = 0.0;
                    reading_tx.target = 0.0;
                    reading_tx.scroll_offset = 0.0;
                },
            )
                .chain(),
        );
        app.add_systems(
            OnExit(Screen::ConversationStack),
            (
                camera::despawn_stack_camera,
                sync::despawn_all_cards,
                |mut view_mode: ResMut<StackViewMode>,
                 mut reading_tx: ResMut<ReadingTransition>| {
                    *view_mode = StackViewMode::Browse;
                    reading_tx.progress = 0.0;
                    reading_tx.target = 0.0;
                    reading_tx.scroll_offset = 0.0;
                },
            ),
        );

        // Per-frame systems (only when stack is active)
        app.add_systems(
            Update,
            (
                sync::sync_stack_cards,
                layout::tick_stack_anim,
                layout::interpolate_stack_focus,
                layout::tick_reading_transition,
                layout::manage_gap_marker,
                layout::compute_card_layout,
            )
                .chain()
                .run_if(in_state(Screen::ConversationStack)),
        );
    }
}
