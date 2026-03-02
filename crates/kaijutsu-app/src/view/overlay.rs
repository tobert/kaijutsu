//! Input overlay — ephemeral compose surface.
//!
//! The InputOverlay is the sole compose surface.
//! It's an absolute-positioned UI entity, shown/hidden based on FocusArea.

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy_vello::prelude::UiVelloText;

use crate::cell::{InputOverlay, InputOverlayMarker};
use crate::input::FocusArea;
use crate::text::{KjText, KjTextEffects, TextMetrics, FontHandles, bevy_color_to_brush};
use crate::ui::theme::Theme;

/// Spawn the singleton InputOverlay entity (root-level, absolute positioned).
///
/// Starts hidden (Visibility::Hidden). Shown/hidden by `sync_overlay_visibility`
/// based on FocusArea::Compose.
pub fn spawn_input_overlay(
    mut commands: Commands,
    existing: Query<Entity, With<InputOverlayMarker>>,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    text_metrics: Res<TextMetrics>,
) {
    if !existing.is_empty() {
        return;
    }
    commands.spawn((
        InputOverlayMarker,
        InputOverlay::default(),
        KjText,
        KjTextEffects { rainbow: true },
        UiVelloText {
            value: "[chat] shell │ ".to_string(),
            style: bevy_vello::prelude::VelloTextStyle {
                font: font_handles.mono.clone(),
                brush: bevy_color_to_brush(theme.fg_dim),
                font_size: text_metrics.cell_font_size,
                ..default()
            },
            ..default()
        },
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(40.0),
            left: Val::Px(20.0),
            right: Val::Px(20.0),
            min_height: Val::Px(40.0),
            padding: UiRect::all(Val::Px(12.0)),
            border: UiRect::all(Val::Px(1.0)),
            border_radius: BorderRadius::all(Val::Px(4.0)),
            ..default()
        },
        BorderColor::all(theme.compose_border),
        BackgroundColor(theme.compose_bg),
        Visibility::Hidden,
        ZIndex(crate::constants::ZLayer::MODAL),
    ));
    info!("Spawned InputOverlay entity");
}

/// Show/hide the InputOverlay entity based on FocusArea.
pub fn sync_overlay_visibility(
    focus: Res<FocusArea>,
    mut overlay_query: Query<&mut Visibility, With<InputOverlayMarker>>,
) {
    if !focus.is_changed() {
        return;
    }
    for mut vis in overlay_query.iter_mut() {
        *vis = if matches!(*focus, FocusArea::Compose) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Sync InputOverlay text to its UiVelloText.
pub fn sync_input_overlay_buffer(
    theme: Res<Theme>,
    mut overlay_query: Query<
        (&InputOverlay, &mut UiVelloText),
        Changed<InputOverlay>,
    >,
) {
    for (overlay, mut vello_text) in overlay_query.iter_mut() {
        let display = overlay.display_text();
        let new_brush = if overlay.is_empty() {
            bevy_color_to_brush(theme.fg_dim)
        } else if overlay.is_shell() {
            let validation = crate::kaish::validate(&overlay.text);
            if !validation.valid && !validation.incomplete {
                bevy_color_to_brush(theme.block_tool_error)
            } else {
                bevy_color_to_brush(theme.block_user)
            }
        } else {
            bevy_color_to_brush(theme.block_user)
        };

        if vello_text.style.brush != new_brush {
            vello_text.style.brush = new_brush;
        }
        if vello_text.value != display {
            vello_text.value = display;
        }
    }
}

/// Keep InputOverlay's `max_advance` in sync with its `ComputedNode` width.
///
/// This prevents long compose text from overflowing or wrapping unexpectedly.
/// Runs on `Changed<ComputedNode>` to handle window resizes.
pub fn sync_overlay_max_advance(
    mut overlay_query: Query<
        (&mut UiVelloText, &ComputedNode),
        (With<InputOverlayMarker>, Changed<ComputedNode>),
    >,
) {
    for (mut vello_text, computed_node) in overlay_query.iter_mut() {
        let width = computed_node.size().x;
        if width > 0.0 {
            let new_advance = Some(width);
            if vello_text.max_advance != new_advance {
                vello_text.max_advance = new_advance;
            }
        }
    }
}
