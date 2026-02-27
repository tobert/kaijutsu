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

/// Build a scrolling rainbow gradient brush.
///
/// `offset` is a 0.0..1.0 phase that shifts the gradient over time,
/// creating the scrolling animation effect.
pub fn rainbow_brush(offset: f32, alpha: f32) -> vello::peniko::Brush {
    use vello::peniko::Gradient;
    use vello::peniko::color::DynamicColor;

    fn c(r: u8, g: u8, b: u8, a: f32) -> DynamicColor {
        vello::peniko::Color::from_rgba8(r, g, b, (a * 255.0) as u8).into()
    }

    // Tokyo Night palette rainbow — vibrant but theme-cohesive.
    // 7 stops wrapping red→red for smooth cycling.
    let palette: [(f32, DynamicColor); 7] = [
        (0.00, c(247, 118, 142, alpha)), // #f7768e red
        (0.17, c(224, 175, 104, alpha)), // #e0af68 amber
        (0.33, c(158, 206, 106, alpha)), // #9ece6a green
        (0.50, c(125, 207, 255, alpha)), // #7dcfff cyan
        (0.67, c(122, 162, 247, alpha)), // #7aa2f7 blue
        (0.83, c(187, 154, 247, alpha)), // #bb9af7 purple
        (1.00, c(247, 118, 142, alpha)), // wrap back to red
    ];

    // Shift stops by offset, wrapping around, then sort
    let mut stops: [(f32, DynamicColor); 7] = palette.map(|(pos, color)| {
        ((pos + offset) % 1.0, color)
    });
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    Gradient::new_linear((0.0, 0.0), (600.0, 0.0))
        .with_stops(stops)
        .into()
}
