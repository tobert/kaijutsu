//! MSDF (Multi-channel Signed Distance Field) text rendering.
//!
//! This module provides GPU-accelerated text rendering using MSDF textures,
//! which enable smooth scaling at any zoom level and support effects like
//! glow and rainbow coloring.
//!
//! Architecture:
//! ```text
//! cosmic-text (shaping + layout)
//!     ↓
//! GlyphPositions (where each glyph goes)
//!     ↓
//! MsdfAtlas (glyph_id → texture region)
//!     ↓
//! msdf_text.wgsl (GPU rendering with effects)
//! ```

pub mod atlas;
pub mod buffer;
pub mod generator;
pub mod pipeline;

pub use atlas::MsdfAtlas;
pub use buffer::{MsdfTextAreaConfig, MsdfTextBuffer, TextBounds};
pub use generator::MsdfGenerator;
pub use pipeline::{
    extract_msdf_texts, init_msdf_resources, prepare_msdf_texts, ExtractedMsdfTexts,
    MsdfTextPipeline, MsdfTextRenderNode, MsdfTextResources,
};

use bevy::prelude::*;
use cosmic_text::{Family, Metrics};

use crate::text::resources::bevy_to_rgba8;

/// Text effects that can be applied to MSDF-rendered text.
///
/// These are defined but not yet wired into the rendering pipeline.
/// They'll be used when rainbow/glow shaders are implemented.
#[derive(Component, Default, Clone)]
pub struct SdfTextEffects {
    /// Enable rainbow color cycling effect.
    pub rainbow: bool,
    /// Optional glow effect configuration.
    pub glow: Option<GlowConfig>,
}

/// Configuration for text glow effect.
#[derive(Clone, Debug)]
pub struct GlowConfig {
    /// Glow color.
    pub color: Color,
    /// Glow intensity (0.0 - 1.0).
    pub intensity: f32,
    /// Glow spread in pixels.
    pub spread: f32,
}

impl Default for GlowConfig {
    fn default() -> Self {
        Self {
            color: Color::srgba(0.4, 0.6, 1.0, 0.5),
            intensity: 0.5,
            spread: 2.0,
        }
    }
}

/// Marker component for entities using MSDF text rendering.
#[derive(Component)]
#[require(Visibility)]
pub struct MsdfText;

/// Simple UI text component for labels and headers.
///
/// This is a simpler alternative to MsdfTextBuffer for UI elements
/// that don't need the full cosmic-text layout features.
#[derive(Component, Clone)]
pub struct MsdfUiText {
    /// Text content.
    pub text: String,
    /// Text color as RGBA8.
    pub color: [u8; 4],
    /// Font metrics.
    pub metrics: Metrics,
    /// Font family.
    pub family: Family<'static>,
}

#[allow(dead_code)]
impl MsdfUiText {
    /// Create a new UI text with default settings.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            color: [255, 255, 255, 255],
            metrics: Metrics::new(16.0, 20.0),
            family: Family::SansSerif,
        }
    }

    /// Set the text color.
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = bevy_to_rgba8(color);
        self
    }

    /// Set the font metrics.
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Set the font size (convenience method that updates metrics).
    pub fn with_font_size(mut self, size: f32) -> Self {
        self.metrics = Metrics::new(size, size * 1.2);
        self
    }

    /// Set the font family.
    pub fn with_family(mut self, family: Family<'static>) -> Self {
        self.family = family;
        self
    }
}

impl Default for MsdfUiText {
    fn default() -> Self {
        Self::new("")
    }
}

/// Cache for UI text screen positions.
///
/// Bevy's UI system computes layout positions, which we need to extract
/// for our custom text rendering. This component caches those positions.
#[derive(Component, Default, Clone)]
pub struct UiTextPositionCache {
    /// Left edge in screen coordinates.
    pub left: f32,
    /// Top edge in screen coordinates.
    pub top: f32,
    /// Width of the text area.
    pub width: f32,
    /// Height of the text area.
    pub height: f32,
}
