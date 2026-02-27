//! Form field container with active border highlight.
//!
//! Extracts the "labeled bordered section with active highlight" pattern used
//! by fork_form's three fields (Name, Model, Tools).

use bevy::prelude::*;

use crate::text::{MsdfUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker on a form field container. The `field_id` identifies which field this is.
#[derive(Component)]
pub struct FormField {
    pub field_id: u8,
}

/// Resource or component indicating which field is currently active.
/// Place on the same entity as the form root.
#[derive(Component)]
pub struct ActiveFormField(pub u8);

// ============================================================================
// SPAWN HELPER
// ============================================================================

/// Spawn a form field section with a label above a bordered container.
/// Returns the inner container entity (where you add content children).
#[allow(dead_code)] // Available for simpler forms that don't need inline children
pub fn spawn_form_field(
    parent: &mut ChildSpawnerCommands,
    field_id: u8,
    label: &str,
    theme: &Theme,
    is_active: bool,
    min_height: f32,
    max_height: Option<f32>,
) -> Entity {
    let outline_color = if is_active { theme.accent } else { theme.border };

    let mut container_id = Entity::PLACEHOLDER;

    parent
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(6.0),
            ..default()
        })
        .with_children(|section| {
            // Label
            section.spawn((
                MsdfUiText::new(label)
                    .with_font_size(12.0)
                    .with_color(theme.fg_dim),
                UiTextPositionCache::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(14.0),
                    ..default()
                },
            ));

            // Bordered container
            let mut node = Node {
                width: Val::Percent(100.0),
                min_height: Val::Px(min_height),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                row_gap: Val::Px(2.0),
                overflow: Overflow::scroll_y(),
                ..default()
            };
            if let Some(max) = max_height {
                node.max_height = Val::Px(max);
            }

            container_id = section
                .spawn((
                    FormField { field_id },
                    node,
                    BackgroundColor(theme.panel_bg),
                    BorderColor::all(outline_color),
                    Outline::new(Val::Px(1.0), Val::ZERO, outline_color),
                    Interaction::None, // Touch-ready
                ))
                .id();
        });

    container_id
}

// ============================================================================
// SYNC SYSTEM
// ============================================================================

/// Updates outline colors on `FormField` entities based on the `ActiveFormField` component.
///
/// Runs when `ActiveFormField` changes on any entity. Searches for `FormField` entities
/// that are descendants of the same form root.
pub fn sync_form_field_outlines(
    theme: Res<Theme>,
    active_query: Query<&ActiveFormField, Changed<ActiveFormField>>,
    mut fields: Query<(&FormField, &mut Outline, &mut BorderColor)>,
) {
    // Only run when ActiveFormField changes
    let Ok(active) = active_query.single() else {
        return;
    };

    for (field, mut outline, mut border) in fields.iter_mut() {
        let color = if field.field_id == active.0 {
            theme.accent
        } else {
            theme.border
        };
        outline.color = color;
        *border = BorderColor::all(color);
    }
}
