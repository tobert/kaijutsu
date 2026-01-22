//! Shader Effect Context - GPU-reactive theme parameters
//!
//! Provides a shared context resource that syncs theme configuration to shaders.
//! Materials that want theme-reactive behavior read from this context and expose
//! the values via their bind group.
//!
//! # Architecture
//!
//! ```text
//! theme.rhai → Theme resource → ShaderEffectContext
//!                                        ↓
//!           ┌─────────────────────────────────────────────────────┐
//!           │  sync_effect_to_materials system                    │
//!           │  Updates material uniforms from ShaderEffectContext │
//!           └─────────────────────────────────────────────────────┘
//!                                        ↓
//!                    TextGlowMaterial.effect_* uniforms → GPU
//! ```

use bevy::prelude::*;

use crate::ui::theme::{color_to_linear_vec4, Theme};

/// Shared context for all shader effects.
///
/// This resource holds the current effect configuration derived from the theme.
/// Materials sync their uniforms from this context via the `sync_effect_to_materials` system.
#[derive(Resource, Clone, Default, Debug)]
pub struct ShaderEffectContext {
    // ═══════════════════════════════════════════════════════════════════════
    // Theme colors (linear space, Vec4 for GPU)
    // ═══════════════════════════════════════════════════════════════════════
    pub accent: Vec4,
    pub accent2: Vec4,
    pub fg: Vec4,
    pub bg: Vec4,

    // ═══════════════════════════════════════════════════════════════════════
    // Effect tuning parameters (from theme.rhai effect_* variables)
    // ═══════════════════════════════════════════════════════════════════════
    pub glow_radius: f32,
    pub glow_intensity: f32,
    pub glow_falloff: f32,
    pub sheen_speed: f32,
    pub sparkle_threshold: f32,
    pub breathe_speed: f32,
    pub breathe_amplitude: f32,
}

/// Plugin that sets up the shader effect context system.
pub struct ShaderEffectContextPlugin;

impl Plugin for ShaderEffectContextPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ShaderEffectContext>()
            .add_systems(Update, sync_theme_to_context.before(super::update_shader_time));
    }
}

/// System: sync Theme → ShaderEffectContext
///
/// Runs before material time updates so context is fresh when materials are synced.
pub fn sync_theme_to_context(theme: Res<Theme>, mut ctx: ResMut<ShaderEffectContext>) {
    // Only update if theme changed
    if !theme.is_changed() {
        return;
    }

    // Theme colors (convert to linear space for GPU)
    ctx.accent = color_to_linear_vec4(theme.accent);
    ctx.accent2 = color_to_linear_vec4(theme.accent2);
    ctx.fg = color_to_linear_vec4(theme.fg);
    ctx.bg = color_to_linear_vec4(theme.bg);

    // Effect parameters from theme
    ctx.glow_radius = theme.effect_glow_radius;
    ctx.glow_intensity = theme.effect_glow_intensity;
    ctx.glow_falloff = theme.effect_glow_falloff;
    ctx.sheen_speed = theme.effect_sheen_speed;
    ctx.sparkle_threshold = theme.effect_sheen_sparkle_threshold;
    ctx.breathe_speed = theme.effect_breathe_speed;
    ctx.breathe_amplitude = theme.effect_breathe_amplitude;

    info!(
        "ShaderEffectContext updated: glow_radius={}, breathe_speed={}",
        ctx.glow_radius, ctx.breathe_speed
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Text Geometry Components (for per-material geometry sync)
// ═══════════════════════════════════════════════════════════════════════════

/// Links a TextGlowMaterial to a text entity for geometry sync.
///
/// Spawn this component alongside a MaterialNode<TextGlowMaterial> to
/// automatically receive geometry updates from the target text entity.
///
/// # Example
///
/// ```ignore
/// // Text entity
/// let text_entity = commands.spawn((
///     GlyphonUiText::new("Hello"),
///     Node { ... },
/// )).id();
///
/// // Glow backing linked to text
/// commands.spawn((
///     Node { width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
///     MaterialNode(materials.add(TextGlowMaterial::subtle(theme.accent))),
///     TextGlowTarget(text_entity),
///     ZIndex(-1),
/// ));
/// ```
#[derive(Component, Clone, Copy, Debug)]
pub struct TextGlowTarget(pub Entity);

/// Computed text geometry for shader use.
///
/// This is populated by the sync system from UiTextPositionCache and text metrics.
/// Can be queried directly or used as intermediate storage.
#[derive(Clone, Default, Debug)]
pub struct TextGeometry {
    /// Bounding box: x, y, width, height (screen pixels)
    pub bounds: Vec4,
    /// Baseline Y position relative to top (estimated from metrics)
    pub baseline: f32,
    /// Line height from text metrics
    pub line_height: f32,
    /// Font size from text metrics
    pub font_size: f32,
    /// Ascent estimate (above baseline, typically ~0.8 * font_size)
    pub ascent: f32,
}

impl TextGeometry {
    /// Create geometry from position cache and metrics.
    pub fn from_position_and_metrics(
        left: f32,
        top: f32,
        width: f32,
        height: f32,
        font_size: f32,
        line_height: f32,
    ) -> Self {
        // Estimate ascent as ~80% of font size (typical for Latin fonts)
        let ascent = font_size * 0.8;
        // Baseline is ascent distance from top of first line
        let baseline = top + ascent;

        Self {
            bounds: Vec4::new(left, top, width, height),
            baseline,
            line_height,
            font_size,
            ascent,
        }
    }

    /// Pack geometry into Vec4s for shader uniforms.
    ///
    /// Returns (bounds, metrics) where:
    /// - bounds = [x, y, width, height]
    /// - metrics = [baseline, line_height, font_size, ascent]
    pub fn to_shader_vecs(&self) -> (Vec4, Vec4) {
        (
            self.bounds,
            Vec4::new(self.baseline, self.line_height, self.font_size, self.ascent),
        )
    }
}
