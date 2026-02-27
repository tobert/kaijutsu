//! MSDF text spawn helpers.
//!
//! Collapses the 8-line `MsdfUiText` + `UiTextPositionCache` + `Node` spawn pattern
//! into single function calls.

use bevy::prelude::*;

use crate::text::{MsdfUiText, UiTextPositionCache};

/// Spawn a full-width MSDF text label as a child. Height auto-derived from font size.
pub fn msdf_label(parent: &mut ChildSpawnerCommands, text: &str, font_size: f32, color: Color) -> Entity {
    parent
        .spawn((
            MsdfUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiTextPositionCache::default(),
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a full-width MSDF text label with an additional marker component for later query.
#[allow(dead_code)] // Available for forms that need marker components on labels
pub fn msdf_label_with<M: Component>(
    parent: &mut ChildSpawnerCommands,
    marker: M,
    text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    parent
        .spawn((
            marker,
            MsdfUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiTextPositionCache::default(),
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a fixed-width MSDF text entity (for button labels, badges, etc.).
pub fn msdf_text(
    parent: &mut ChildSpawnerCommands,
    text: &str,
    font_size: f32,
    color: Color,
    width: f32,
) -> Entity {
    parent
        .spawn((
            MsdfUiText::new(text)
                .with_font_size(font_size)
                .with_color(color),
            UiTextPositionCache::default(),
            Node {
                width: Val::Px(width),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}
