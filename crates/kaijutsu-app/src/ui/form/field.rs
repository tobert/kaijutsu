//! Form field container with active border highlight, and PostUpdate scene sync
//! systems for responsive Vello rendering.
//!
//! `FormField` marks bordered field containers. `ActiveFormField` tracks which
//! field is active. The `sync_form_field_borders` system draws Vello scene borders.
//!
//! PostUpdate systems (after Layout):
//! - `sync_form_field_borders` — field border color + resize
//! - `sync_modal_panel_scene` — modal panel bg + border
//! - `sync_form_button_scenes` — button bg from computed size
//! - `sync_row_highlights` — list/tree row highlight from computed width

use bevy::prelude::*;
use bevy_vello::prelude::UiVelloScene;

use crate::ui::form::scene::{build_button_bg, build_form_field_border, build_modal_panel, build_overlay_bg, build_row_highlight};
use crate::ui::form::schema::{FormButton, FormOverlay, ModalPanel};
use crate::ui::form::selectable::{SelectableList, SelectableListRow};
use crate::ui::form::tree::{TreeView, TreeViewRow};
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
// SYNC SYSTEMS (PostUpdate, after Layout)
// ============================================================================

/// Draws Vello scene borders on `FormField` entities when `ActiveFormField` changes.
pub fn sync_form_field_borders(
    theme: Res<Theme>,
    active_query: Query<&ActiveFormField, Changed<ActiveFormField>>,
    mut fields: Query<(&FormField, &mut UiVelloScene, &ComputedNode)>,
) {
    let Ok(active) = active_query.single() else {
        return;
    };

    for (field, mut scene_component, computed) in fields.iter_mut() {
        rebuild_one_field_border(&theme, active.0, field, &mut scene_component, computed);
    }
}

/// Rebuilds field borders when a field's `ComputedNode` changes (resize).
pub fn sync_form_field_borders_on_resize(
    theme: Res<Theme>,
    active_query: Query<&ActiveFormField>,
    mut fields: Query<
        (&FormField, &mut UiVelloScene, &ComputedNode),
        Changed<ComputedNode>,
    >,
) {
    let Ok(active) = active_query.single() else {
        return;
    };

    for (field, mut scene_component, computed) in fields.iter_mut() {
        rebuild_one_field_border(&theme, active.0, field, &mut scene_component, computed);
    }
}

fn rebuild_one_field_border(
    theme: &Theme,
    active_id: u8,
    field: &FormField,
    scene_component: &mut UiVelloScene,
    computed: &ComputedNode,
) {
    let size = computed.size();
    if size.x < 1.0 || size.y < 1.0 {
        return;
    }

    let color = if field.field_id == active_id {
        theme.accent
    } else {
        theme.border
    };

    let mut scene = bevy_vello::vello::Scene::new();
    build_form_field_border(
        &mut scene,
        size.x as f64,
        size.y as f64,
        color,
        4.0,
        1.0,
    );
    *scene_component = UiVelloScene::from(scene);
}

/// Draws modal panel background + border from `ComputedNode` dimensions.
pub fn sync_modal_panel_scene(
    theme: Res<Theme>,
    mut panels: Query<(&mut UiVelloScene, &ComputedNode), (With<ModalPanel>, Changed<ComputedNode>)>,
) {
    for (mut scene_component, computed) in panels.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let mut scene = bevy_vello::vello::Scene::new();
        build_modal_panel(
            &mut scene,
            size.x as f64,
            size.y as f64,
            theme.panel_bg,
            theme.border,
            6.0,
            1.0,
        );
        *scene_component = UiVelloScene::from(scene);
    }
}

/// Draws button backgrounds from `ComputedNode` dimensions.
pub fn sync_form_button_scenes(
    theme: Res<Theme>,
    mut buttons: Query<(&FormButton, &mut UiVelloScene, &ComputedNode), Changed<ComputedNode>>,
) {
    for (button, mut scene_component, computed) in buttons.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let bg = if button.primary {
            theme.accent.with_alpha(0.3)
        } else {
            theme.fg_dim.with_alpha(0.2)
        };

        let mut scene = bevy_vello::vello::Scene::new();
        build_button_bg(&mut scene, size.x as f64, size.y as f64, bg, 4.0);
        *scene_component = UiVelloScene::from(scene);
    }
}

/// Draws row highlights for both `SelectableListRow` and `TreeViewRow` using
/// actual `ComputedNode` width instead of hardcoded values.
pub fn sync_row_highlights(
    theme: Res<Theme>,
    mut selectable_rows: Query<
        (&SelectableListRow, &mut UiVelloScene, &ComputedNode, &ChildOf),
        (Changed<ComputedNode>, Without<TreeViewRow>),
    >,
    mut tree_rows: Query<
        (&TreeViewRow, &mut UiVelloScene, &ComputedNode, &ChildOf),
        (Changed<ComputedNode>, Without<SelectableListRow>),
    >,
    selectable_lists: Query<&SelectableList>,
    tree_views: Query<&TreeView>,
) {
    // SelectableList rows
    for (row, mut scene_component, computed, child_of) in selectable_rows.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }
        let Ok(list) = selectable_lists.get(child_of.0) else {
            continue;
        };
        let is_selected = row.0 == list.selected
            && list.items.get(row.0).is_some_and(|item| !item.is_header);

        if is_selected {
            let mut scene = bevy_vello::vello::Scene::new();
            build_row_highlight(
                &mut scene,
                size.x as f64,
                size.y as f64,
                theme.accent.with_alpha(list.selected_bg_alpha),
            );
            *scene_component = UiVelloScene::from(scene);
        } else {
            *scene_component = UiVelloScene::default();
        }
    }

    // TreeView rows
    for (row, mut scene_component, computed, child_of) in tree_rows.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }
        let Ok(tree) = tree_views.get(child_of.0) else {
            continue;
        };
        let is_cursor = row.0 == tree.cursor;

        if is_cursor {
            let mut scene = bevy_vello::vello::Scene::new();
            build_row_highlight(
                &mut scene,
                size.x as f64,
                size.y as f64,
                theme.accent.with_alpha(0.1),
            );
            *scene_component = UiVelloScene::from(scene);
        } else {
            *scene_component = UiVelloScene::default();
        }
    }
}

/// Draws the full-viewport form overlay background via Vello scene.
///
/// Uses `UiVelloScene` instead of `BackgroundColor` because `BackgroundColor`
/// is rendered by Bevy's standard UI pass which composites ON TOP of the Vello
/// canvas, covering all Vello text.
pub fn sync_form_overlay_scene(
    theme: Res<Theme>,
    mut overlays: Query<(&mut UiVelloScene, &ComputedNode), (With<FormOverlay>, Changed<ComputedNode>)>,
) {
    for (mut scene_component, computed) in overlays.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let mut scene = bevy_vello::vello::Scene::new();
        build_overlay_bg(&mut scene, size.x as f64, size.y as f64, theme.bg);
        *scene_component = UiVelloScene::from(scene);
    }
}
