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

#[cfg(test)]
mod tests;

pub use atlas::MsdfAtlas;
pub use buffer::{MsdfTextAreaConfig, MsdfTextBuffer, TextBounds};
pub use generator::MsdfGenerator;
// Font metrics infrastructure for future pixel-alignment work
#[allow(unused_imports)]
pub use generator::{FontMetricsCache, HintingMetrics};
pub use pipeline::{
    extract_msdf_render_config, extract_msdf_taa_config, extract_msdf_texts,
    init_msdf_resources, prepare_msdf_texts,
    ExtractedMsdfTexts, MsdfTextPipeline, MsdfTextRenderNode, MsdfTextResources,
    MsdfTextTaaState,
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

/// Debug information about MSDF text rendering.
///
/// This resource captures metrics about rendered glyphs for debugging
/// and inspection via BRP tools.
#[cfg(debug_assertions)]
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct MsdfDebugInfo {
    /// Number of glyphs rendered in the last frame.
    pub glyph_count: usize,
    /// Number of text areas rendered.
    pub text_area_count: usize,
    /// Atlas dimensions.
    pub atlas_size: (u32, u32),
    /// Atlas glyph count.
    pub atlas_glyph_count: usize,
    /// Last few rendered glyph samples for inspection.
    pub sample_glyphs: Vec<DebugGlyph>,
}

/// Debug info for a single glyph.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, Default, Reflect)]
pub struct DebugGlyph {
    /// The character (if decodable).
    pub char_code: u16,
    /// Glyph X position from layout.
    pub glyph_x: f32,
    /// Glyph Y position from layout.
    pub glyph_y: f32,
    /// Font size for this glyph.
    pub font_size: f32,
    /// Atlas region width.
    pub region_width: u32,
    /// Atlas region height.
    pub region_height: u32,
    /// Anchor X in em units.
    pub anchor_x: f32,
    /// Anchor Y in em units.
    pub anchor_y: f32,
    /// Computed quad width in pixels.
    pub quad_width: f32,
    /// Computed quad height in pixels.
    pub quad_height: f32,
}

/// Debug overlay mode for MSDF text rendering.
///
/// Toggle with F11. Shows visual debug information about glyph positioning.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum DebugOverlayMode {
    /// Debug overlay disabled.
    #[default]
    Off,
    /// Show dots: green=pen position, blue=anchor, yellow=quad corner.
    Dots,
    /// Show dots + red quad outlines.
    DotsAndQuads,
    /// Shader debug: show raw median distance (grayscale).
    RawDistance,
    /// Shader debug: show computed alpha (grayscale).
    ComputedAlpha,
    /// Shader debug: hard threshold at sd=0.5 (binary).
    HardThreshold,
}

#[cfg(debug_assertions)]
impl DebugOverlayMode {
    /// Cycle to next debug mode.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Dots,
            Self::Dots => Self::DotsAndQuads,
            Self::DotsAndQuads => Self::RawDistance,
            Self::RawDistance => Self::ComputedAlpha,
            Self::ComputedAlpha => Self::HardThreshold,
            Self::HardThreshold => Self::Off,
        }
    }

    /// Get the numeric mode value for the shader uniform.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Dots => 1,
            Self::DotsAndQuads => 2,
            Self::RawDistance => 3,
            Self::ComputedAlpha => 4,
            Self::HardThreshold => 5,
        }
    }

    /// Get human-readable description.
    pub fn description(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Dots => "DOTS (green=pen, blue=anchor, yellow=quad)",
            Self::DotsAndQuads => "DOTS + QUADS (red outlines)",
            Self::RawDistance => "RAW DISTANCE (grayscale MSDF median)",
            Self::ComputedAlpha => "COMPUTED ALPHA (after px_range)",
            Self::HardThreshold => "HARD THRESHOLD (binary at sd=0.5)",
        }
    }
}

/// Resource controlling MSDF debug visualization.
///
/// Press F11 to cycle through debug modes:
/// - Off: Normal rendering
/// - Dots: Shows pen position (green), anchor (blue), quad corner (yellow)
/// - DotsAndQuads: Dots + red quad outlines
#[cfg(debug_assertions)]
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct MsdfDebugOverlay {
    /// Current debug mode.
    pub mode: DebugOverlayMode,
    /// Show metrics HUD with first glyph info.
    pub show_hud: bool,
}

/// Resource controlling TAA jitter for MSDF text.
///
/// Press F10 to toggle TAA jitter on/off for A/B quality comparison.
/// When enabled, text is rendered with sub-pixel jitter each frame using
/// a Halton(2,3) sequence, which improves edge quality through temporal
/// super-resolution (when history accumulation is added in Phase 2+).
#[derive(Resource, Default, Reflect, Clone, Copy)]
#[reflect(Resource)]
pub struct MsdfTaaConfig {
    /// Whether TAA jitter is enabled (default: true).
    pub enabled: bool,
}

