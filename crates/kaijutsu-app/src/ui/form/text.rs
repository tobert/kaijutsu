//! Vello text spawn helpers.
//!
//! Build styled `UiVelloText` directly at spawn time — no intermediate wrapper.
//! Heights use `Val::Auto` so bevy_vello's `ContentSize` drives layout.

use bevy::prelude::*;
use bevy_vello::prelude::{UiVelloText, VelloFont};

use crate::text::vello_style;

/// Spawn a full-width text label as a child. Height driven by ContentSize.
pub fn vello_label(
    parent: &mut ChildSpawnerCommands,
    font: &Handle<VelloFont>,
    text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    parent
        .spawn((
            UiVelloText {
                value: text.into(),
                style: vello_style(font, color, font_size),
                ..default()
            },
            Node {
                width: Val::Percent(100.0),
                ..default()
            },
        ))
        .id()
}

/// Spawn a full-width text label with an additional marker component for later query.
#[allow(dead_code)] // Available for forms that need marker components on labels
pub fn vello_label_with<M: Component>(
    parent: &mut ChildSpawnerCommands,
    font: &Handle<VelloFont>,
    marker: M,
    text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    parent
        .spawn((
            marker,
            UiVelloText {
                value: text.into(),
                style: vello_style(font, color, font_size),
                ..default()
            },
            Node {
                width: Val::Percent(100.0),
                ..default()
            },
        ))
        .id()
}

/// Spawn a fixed-width text entity (for button labels, badges, etc.).
#[allow(dead_code)]
pub fn vello_text(
    parent: &mut ChildSpawnerCommands,
    font: &Handle<VelloFont>,
    text: &str,
    font_size: f32,
    color: Color,
    width: f32,
) -> Entity {
    parent
        .spawn((
            UiVelloText {
                value: text.into(),
                style: vello_style(font, color, font_size),
                ..default()
            },
            Node {
                width: Val::Px(width),
                ..default()
            },
        ))
        .id()
}
