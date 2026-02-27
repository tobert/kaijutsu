#![allow(dead_code)] // FontHandles-based helpers for future call site migration
//! Vello text spawn helpers (with explicit font handle).
//!
//! These accept a `Handle<VelloFont>` parameter for explicit font control.
//! For simpler helpers that use the sync system, see `ui::form::text`.

use bevy::prelude::*;
use bevy_vello::prelude::*;

use super::components::{bevy_color_to_brush, KjUiText};

/// Spawn a full-width Vello text label as a child. Height auto-derived from font size.
pub fn vello_label(
    parent: &mut ChildSpawnerCommands,
    font: &Handle<VelloFont>,
    text: &str,
    font_size: f32,
    color: Color,
) -> Entity {
    parent
        .spawn((
            KjUiText::new(text).with_font_size(font_size).with_color(color),
            UiVelloText {
                value: text.to_string(),
                style: VelloTextStyle {
                    font: font.clone(),
                    brush: bevy_color_to_brush(color),
                    font_size,
                    ..default()
                },
                ..default()
            },
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a full-width Vello text label with an additional marker component.
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
            KjUiText::new(text).with_font_size(font_size).with_color(color),
            UiVelloText {
                value: text.to_string(),
                style: VelloTextStyle {
                    font: font.clone(),
                    brush: bevy_color_to_brush(color),
                    font_size,
                    ..default()
                },
                ..default()
            },
            Node {
                width: Val::Percent(100.0),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}

/// Spawn a fixed-width Vello text entity (for button labels, badges, etc.).
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
            KjUiText::new(text).with_font_size(font_size).with_color(color),
            UiVelloText {
                value: text.to_string(),
                style: VelloTextStyle {
                    font: font.clone(),
                    brush: bevy_color_to_brush(color),
                    font_size,
                    ..default()
                },
                ..default()
            },
            Node {
                width: Val::Px(width),
                height: Val::Px((font_size * 1.2).ceil()),
                ..default()
            },
        ))
        .id()
}
