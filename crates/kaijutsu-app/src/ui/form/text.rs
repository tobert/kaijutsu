//! Vello text spawn helpers.
//!
//! Collapses the `KjUiText` + `UiVelloText` + `Node` spawn pattern
//! into single function calls: `vello_label`, `vello_label_with`, `vello_text`.

use bevy::prelude::*;
use bevy_vello::prelude::UiVelloText;

use crate::text::KjUiText;

/// Spawn a full-width text label as a child. Height auto-derived from font size.
pub fn vello_label(parent: &mut ChildSpawnerCommands, text: &str, font_size: f32, color: Color) -> Entity {
    parent
        .spawn((
            KjUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiVelloText::default(),
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a full-width text label with an additional marker component for later query.
#[allow(dead_code)] // Available for forms that need marker components on labels
pub fn vello_label_with<M: Component>(
    parent: &mut ChildSpawnerCommands,
    marker: M,
    text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    parent
        .spawn((
            marker,
            KjUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiVelloText::default(),
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a fixed-width text entity (for button labels, badges, etc.).
pub fn vello_text(
    parent: &mut ChildSpawnerCommands,
    text: &str,
    font_size: f32,
    color: Color,
    width: f32,
) -> Entity {
    parent
        .spawn((
            KjUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiVelloText::default(),
            Node {
                width: Val::Px(width),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}
