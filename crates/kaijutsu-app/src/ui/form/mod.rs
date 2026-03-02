//! Reusable form primitives for kaijutsu-app UI.
//!
//! Shared components and spawn helpers extracted from fork_form and model_picker.
//!
//! ## Schema system
//!
//! `Form` + `FormPresentation` components describe a form declaratively.
//! The `build_form` system fires on `Added<Form>` and produces the entity tree.
//! Domain code queries `FormFieldContainer(id)` to insert content, and calls
//! `handle_form_action()` from its input handler for Tab/j/k/Enter/Esc.

pub mod async_slot;
pub mod field;
pub mod navigation;
pub mod schema;
pub mod selectable;
pub mod text;
pub mod tree;

use bevy::prelude::*;

pub use async_slot::AsyncSlot;
pub use field::ActiveFormField;
pub use navigation::{handle_form_action, handle_form_space, FormActionResult};
pub use schema::{
    ButtonDesc, FieldDesc, Form, FormFieldContainer, FormLayout, FormLoadingText,
    FormPresentation,
};
pub use selectable::{ListItem, SelectableList};
pub use tree::{TreeCategory, TreeItem, TreeView};

/// Plugin that registers form primitive systems.
pub struct FormPlugin;

impl Plugin for FormPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                schema::build_form,
                selectable::sync_selectable_list_visuals,
                tree::rebuild_tree_view,
                field::sync_form_field_outlines,
                selectable::handle_selectable_list_click,
                tree::handle_tree_view_click,
            ),
        );
    }
}
