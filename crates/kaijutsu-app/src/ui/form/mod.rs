//! Reusable form primitives for kaijutsu-app UI.
//!
//! Shared components and spawn helpers extracted from fork_form and model_picker.

pub mod async_slot;
pub mod field;
pub mod selectable;
pub mod text;
pub mod tree;

use bevy::prelude::*;

pub use async_slot::AsyncSlot;
pub use field::{ActiveFormField, FormField};
pub use selectable::{ListItem, SelectableList};
#[allow(unused_imports)] // Part of public API, used by consumers for entity queries
pub use selectable::SelectableListRow;
pub use text::{msdf_label, msdf_text};
#[allow(unused_imports)] // Available for forms that need marker components on labels
pub use text::msdf_label_with;
pub use tree::{TreeCategory, TreeCursorTarget, TreeItem, TreeView};
#[allow(unused_imports)] // Part of public API, used by consumers for entity queries
pub use tree::TreeViewRow;

/// Plugin that registers form primitive systems.
pub struct FormPlugin;

impl Plugin for FormPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                selectable::sync_selectable_list_visuals,
                tree::rebuild_tree_view,
                field::sync_form_field_outlines,
            ),
        );
    }
}
