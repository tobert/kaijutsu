//! Form field container with active border highlight.
//!
//! `FormField` marks bordered field containers. `ActiveFormField` tracks which
//! field is active. The `sync_form_field_outlines` system updates border colors.
//!
//! Field containers are spawned by `schema::build_form` — no standalone spawn helper.

use bevy::prelude::*;

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
