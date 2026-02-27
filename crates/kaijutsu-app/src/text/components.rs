//! Vello text components and style types for Kaijutsu.
//!
//! Thin wrappers and marker components that map kaijutsu concepts
//! onto bevy_vello's text rendering.

use bevy::prelude::*;
use bevy_vello::prelude::*;

/// Marker component for block-level text entities (conversation blocks, role headers).
///
/// Used by screen transitions to hide/show cell text that isn't parented
/// under the conversation root.
#[derive(Component, Default)]
pub struct KjText;

/// Rainbow color cycling effect marker.
///
/// When present, the text brush uses a gradient instead of a solid color.
#[derive(Component, Default, Clone, PartialEq, Eq)]
pub struct KjTextEffects {
    pub rainbow: bool,
}

/// Convenience wrapper for UI text with kaijutsu defaults.
///
/// Widget systems update this; sync system propagates to UiVelloText.
#[derive(Component, Clone)]
pub struct KjUiText {
    pub text: String,
    pub color: Color,
    pub font_size: f32,
}

impl KjUiText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: Color::WHITE,
            font_size: 16.0,
        }
    }

    pub fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn with_font_size(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    /// Convert to a `UiVelloText` using the given font handle.
    #[allow(dead_code)] // Available for spawn sites that want explicit control
    pub fn to_vello_text(&self, font: Handle<VelloFont>) -> UiVelloText {
        UiVelloText {
            value: self.text.clone(),
            style: VelloTextStyle {
                font,
                brush: bevy_color_to_brush(self.color),
                font_size: self.font_size,
                ..default()
            },
            ..default()
        }
    }
}

/// Convert a Bevy `Color` to a Vello `Brush::Solid`.
pub fn bevy_color_to_brush(color: Color) -> vello::peniko::Brush {
    let srgba = color.to_srgba();
    vello::peniko::Brush::Solid(vello::peniko::Color::from_rgba8(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    ))
}

/// Convert Bevy Color to RGBA8 array.
#[allow(dead_code)]
pub fn bevy_to_rgba8(color: Color) -> [u8; 4] {
    let srgba = color.to_srgba();
    [
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    ]
}
