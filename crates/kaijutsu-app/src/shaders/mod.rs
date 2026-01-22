//! Shader effects for Kaijutsu UI
//!
//! This module provides custom `UiMaterial` implementations for various visual effects:
//! - `GlowBorderMaterial` - Animated glowing border for focused elements (legacy)
//! - `ShimmerMaterial` - Sparkle/twinkle overlay for active states
//! - `PulseRingMaterial` - Expanding ring ripple effect
//! - `ScanlinesMaterial` - Subtle CRT/cyberpunk scanlines
//! - `HoloBorderMaterial` - Rainbow/gradient animated border
//! - `CornerMaterial` / `EdgeMaterial` - 9-slice frame system (new)
//! - `TextGlowMaterial` - Luminous backing for text with theme-reactive effects
//!
//! # Theme-Reactive Shaders
//!
//! The `ShaderEffectContext` resource syncs theme configuration to shaders.
//! Materials that want theme-reactive behavior read from this context.
//!
//! ```text
//! theme.rhai → Theme → ShaderEffectContext → Material uniforms → GPU
//! ```
//!
//! # Usage
//!
//! Add `ShaderFxPlugin` to your app, then spawn UI nodes with `MaterialNode<T>`:
//!
//! ```ignore
//! commands.spawn((
//!     Node { width: Val::Px(200.0), height: Val::Px(100.0), ..default() },
//!     MaterialNode(materials.add(GlowBorderMaterial {
//!         color: Vec4::new(0.34, 0.65, 1.0, 1.0),
//!         ..default()
//!     })),
//! ));
//! ```

pub mod context;
pub mod nine_slice;

pub use context::{ShaderEffectContext, ShaderEffectContextPlugin, TextGeometry, TextGlowTarget};

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

use nine_slice::{
    ChasingBorder, ChasingBorderMaterial, CornerMarker, CornerMaterial, CornerPosition, EdgeMarker,
    EdgeMaterial, EdgePosition, ErrorFrameMaterial, FramePiece,
};

/// Plugin that registers all shader effect materials.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            // Shared effect context (theme → GPU)
            ShaderEffectContextPlugin,
            // Legacy materials
            UiMaterialPlugin::<GlowBorderMaterial>::default(),
            UiMaterialPlugin::<ShimmerMaterial>::default(),
            UiMaterialPlugin::<PulseRingMaterial>::default(),
            UiMaterialPlugin::<ScanlinesMaterial>::default(),
            UiMaterialPlugin::<HoloBorderMaterial>::default(),
            UiMaterialPlugin::<CursorBeamMaterial>::default(),
            // 9-slice materials
            UiMaterialPlugin::<CornerMaterial>::default(),
            UiMaterialPlugin::<EdgeMaterial>::default(),
            UiMaterialPlugin::<ErrorFrameMaterial>::default(),
            // Chasing border effect
            UiMaterialPlugin::<ChasingBorderMaterial>::default(),
            // Text effects
            UiMaterialPlugin::<TextGlowMaterial>::default(),
        ))
        // Register frame types for BRP reflection
        .register_type::<FramePiece>()
        .register_type::<CornerMarker>()
        .register_type::<EdgeMarker>()
        .register_type::<CornerPosition>()
        .register_type::<EdgePosition>()
        .register_type::<ChasingBorder>()
        .add_systems(Update, (
            update_shader_time,
            sync_effect_context_to_text_glow,
            sync_text_geometry_to_materials,
        ));
    }
}

/// System to update time uniforms on all shader materials.
fn update_shader_time(
    time: Res<Time>,
    mut glow_materials: ResMut<Assets<GlowBorderMaterial>>,
    mut shimmer_materials: ResMut<Assets<ShimmerMaterial>>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    mut scanline_materials: ResMut<Assets<ScanlinesMaterial>>,
    mut holo_materials: ResMut<Assets<HoloBorderMaterial>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
    mut error_materials: ResMut<Assets<ErrorFrameMaterial>>,
    mut chasing_materials: ResMut<Assets<ChasingBorderMaterial>>,
    mut text_glow_materials: ResMut<Assets<TextGlowMaterial>>,
) {
    let t = time.elapsed_secs();

    // Legacy materials
    for (_, mat) in glow_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in shimmer_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in pulse_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in scanline_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in holo_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in cursor_materials.iter_mut() {
        mat.time.x = t;
    }

    // 9-slice materials
    for (_, mat) in corner_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in edge_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in error_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in chasing_materials.iter_mut() {
        mat.time.x = t;
    }

    // Text effects
    for (_, mat) in text_glow_materials.iter_mut() {
        mat.time.x = t;
    }
}

// ============================================================================
// GLOW BORDER MATERIAL
// ============================================================================

/// Cyberpunk corner bracket effect for cell frames.
///
/// Creates glowing L-shaped brackets at each corner with animated pulse.
/// Lavender/cyan color palette, fully transparent in the middle.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct GlowBorderMaterial {
    /// Base glow color (RGBA) - blended with lavender
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=glow_radius, y=glow_intensity, z=pulse_speed, w=bracket_length (0-1)
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time (other components unused, for alignment)
    #[uniform(2)]
    pub time: Vec4,
}

impl Default for GlowBorderMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.7, 0.5, 0.9, 1.0), // Lavender
            params: Vec4::new(0.15, 1.2, 1.5, 0.25), // radius, intensity, speed, bracket_length
            time: Vec4::ZERO,
        }
    }
}

impl UiMaterial for GlowBorderMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/glow_border.wgsl".into()
    }
}

// ============================================================================
// SHIMMER MATERIAL
// ============================================================================

/// Sparkle/twinkle effect overlay for active states.
///
/// Creates randomly twinkling star-like points across the surface.
/// Good for "thinking" or "processing" indicators.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ShimmerMaterial {
    /// Sparkle color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=density, y=speed, z=brightness, w=size
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time
    #[uniform(2)]
    pub time: Vec4,
}

impl Default for ShimmerMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(1.0, 1.0, 1.0, 0.9),
            params: Vec4::new(8.0, 3.0, 1.0, 0.08), // density, speed, brightness, size
            time: Vec4::ZERO,
        }
    }
}

impl UiMaterial for ShimmerMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/shimmer.wgsl".into()
    }
}

// ============================================================================
// PULSE RING MATERIAL
// ============================================================================

/// Expanding ring/ripple effect for focus or notification.
///
/// Creates concentric rings that expand outward from the center.
/// Good for drawing attention or indicating activity.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct PulseRingMaterial {
    /// Ring color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=ring_count, y=ring_width, z=speed, w=max_radius
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time, y=fade_start
    #[uniform(2)]
    pub time: Vec4,
}

impl Default for PulseRingMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.34, 0.65, 1.0, 0.6), // Cyan, semi-transparent
            params: Vec4::new(2.0, 0.05, 0.5, 1.2), // count, width, speed, max_radius
            time: Vec4::new(0.0, 0.5, 0.0, 0.0),    // time, fade_start
        }
    }
}

impl UiMaterial for PulseRingMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/pulse_ring.wgsl".into()
    }
}

// ============================================================================
// SCANLINES MATERIAL
// ============================================================================

/// Subtle CRT/cyberpunk scanline overlay.
///
/// Adds retro scanline effect with optional scroll, flicker, and noise.
/// Use sparingly for cyberpunk aesthetic.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ScanlinesMaterial {
    /// Tint color (RGBA)
    #[uniform(0)]
    pub tint: Vec4,
    /// Params1: x=line_count, y=line_intensity, z=scroll_speed, w=flicker
    #[uniform(1)]
    pub params1: Vec4,
    /// Params2: x=noise_amount, y=curvature, z=time, w=unused
    #[uniform(2)]
    pub params2: Vec4,
    /// Time: x=elapsed_time
    #[uniform(3)]
    pub time: Vec4,
}

impl Default for ScanlinesMaterial {
    fn default() -> Self {
        Self {
            tint: Vec4::new(1.0, 1.0, 1.0, 0.15), // Very subtle
            params1: Vec4::new(100.0, 0.1, 0.0, 0.0), // count, intensity, scroll, flicker
            params2: Vec4::new(0.0, 0.0, 0.0, 0.0),   // noise, curvature
            time: Vec4::ZERO,
        }
    }
}

impl UiMaterial for ScanlinesMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/scanlines.wgsl".into()
    }
}

// ============================================================================
// HOLO BORDER MATERIAL
// ============================================================================

/// Animated rainbow/gradient border with holographic shimmer.
///
/// Creates a border that cycles through colors with a holographic effect.
/// Modes: 0 = rainbow, 1 = cyber (pink/cyan), 2 = custom blend
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct HoloBorderMaterial {
    /// Base color (RGBA)
    #[uniform(0)]
    pub base_color: Vec4,
    /// Params1: x=saturation, y=speed, z=border_width, w=shimmer_scale
    #[uniform(1)]
    pub params1: Vec4,
    /// Params2: x=rainbow_spread, y=mode, z=unused, w=unused
    #[uniform(2)]
    pub params2: Vec4,
    /// Time: x=elapsed_time
    #[uniform(3)]
    pub time: Vec4,
}

/// Holo border color modes
#[derive(Clone, Copy)]
#[allow(dead_code)] // Builder pattern for shader configuration
pub enum HoloMode {
    Rainbow = 0,
    Cyber = 1,
    Custom = 2,
}

impl Default for HoloBorderMaterial {
    fn default() -> Self {
        Self {
            base_color: Vec4::new(1.0, 1.0, 1.0, 1.0),
            params1: Vec4::new(0.8, 0.3, 0.03, 20.0), // sat, speed, width, shimmer
            params2: Vec4::new(1.0, HoloMode::Cyber as u8 as f32, 0.0, 0.0), // spread, mode
            time: Vec4::ZERO,
        }
    }
}

impl UiMaterial for HoloBorderMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/holo_border.wgsl".into()
    }
}

// ============================================================================
// CURSOR BEAM MATERIAL
// ============================================================================

/// Glowing cursor beam with cyberpunk energy effects.
///
/// Supports three modes:
/// - Beam (0): Vertical line cursor (insert mode)
/// - Block (1): Filled block cursor (normal mode)
/// - Underline (2): Horizontal underline cursor
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct CursorBeamMaterial {
    /// Cursor color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=glow_width, y=intensity, z=pulse_speed, w=blink_rate
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time, y=mode (0=beam, 1=block, 2=underline)
    #[uniform(2)]
    pub time: Vec4,
}

/// Cursor display modes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    Beam = 0,      // Vertical line (insert mode)
    Block = 1,     // Filled block (normal mode)
    Underline = 2, // Bottom line
}

impl Default for CursorBeamMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(1.0, 0.5, 0.75, 0.95), // Hot pink
            // params: x=orb_size, y=intensity, z=wander_speed, w=blink_rate
            params: Vec4::new(0.25, 1.2, 2.0, 0.0),
            time: Vec4::new(0.0, 1.0, 0.0, 0.0), // time, mode (default block)
        }
    }
}


impl UiMaterial for CursorBeamMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/cursor_beam.wgsl".into()
    }
}

// ============================================================================
// TEXT GLOW MATERIAL
// ============================================================================

/// Subtle luminous backing for text - increases contrast and perceived sharpness.
///
/// Now theme-reactive: effect parameters come from `ShaderEffectContext` which
/// syncs from `theme.rhai`. The shader reads these from the `effect` uniform.
///
/// Renders behind text to create:
/// - Soft center glow (backlight effect)
/// - Optional edge enhancement (sharpening)
/// - Subtle top-light gradient (improves readability)
///
/// # Theme Configuration
///
/// In `theme.rhai`:
/// ```rhai
/// let effect_glow_radius = 0.3;
/// let effect_glow_intensity = 0.5;
/// let effect_breathe_speed = 1.9;
/// // ... etc
/// ```
///
/// # Usage
///
/// Spawn a Node with this material BEHIND your text (ZIndex(-1)):
/// ```ignore
/// // Glow backing
/// commands.spawn((
///     Node { width: Val::Px(200.0), height: Val::Px(30.0), ..default() },
///     MaterialNode(materials.add(TextGlowMaterial::subtle(theme.accent))),
///     ZIndex(-1),
/// ));
/// // Text on top (ZIndex 0, default)
/// commands.spawn((GlyphonUiText::new("Hello"), ...));
/// ```
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TextGlowMaterial {
    /// Glow color (RGBA) - typically matches or complements text color
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=radius (0.1-0.5), y=intensity (0.5-2.0), z=falloff (1.0-4.0), w=mode (0=glow, >0.5=icy)
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time (updated by update_shader_time system)
    #[uniform(2)]
    pub time: Vec4,
    /// Effect context from theme: [glow_radius, glow_intensity, glow_falloff, sheen_speed]
    #[uniform(3)]
    pub effect_glow: Vec4,
    /// Effect context from theme: [sparkle_threshold, breathe_speed, breathe_amplitude, _reserved]
    #[uniform(4)]
    pub effect_anim: Vec4,
    /// Theme colors: accent (linear space)
    #[uniform(5)]
    pub theme_accent: Vec4,
    /// Text geometry: bounds [x, y, width, height] in screen pixels
    #[uniform(6)]
    pub text_bounds: Vec4,
    /// Text geometry: metrics [baseline, line_height, font_size, ascent]
    #[uniform(7)]
    pub text_metrics: Vec4,
}

impl Default for TextGlowMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.5, 0.6, 0.9, 0.3), // Soft blue, low alpha
            params: Vec4::new(0.3, 0.8, 2.0, 0.0), // radius, intensity, falloff, mode
            time: Vec4::ZERO,
            // Defaults match Theme::default() effect params
            effect_glow: Vec4::new(0.3, 0.5, 2.5, 0.15),   // radius, intensity, falloff, sheen_speed
            effect_anim: Vec4::new(0.92, 1.9, 0.1, 0.0),   // sparkle_threshold, breathe_speed, breathe_amplitude
            theme_accent: Vec4::new(0.34, 0.65, 1.0, 1.0), // Default accent
            // Geometry defaults (will be populated by sync system if TextGlowTarget present)
            text_bounds: Vec4::new(0.0, 0.0, 100.0, 20.0), // Placeholder bounds
            text_metrics: Vec4::new(11.2, 20.0, 14.0, 11.2), // baseline, line_height, font_size, ascent
        }
    }
}

impl TextGlowMaterial {
    /// Create a subtle glow with given color (good for body text)
    pub fn subtle(color: Color) -> Self {
        let c = color.to_linear();
        Self {
            color: Vec4::new(c.red, c.green, c.blue, 0.25),
            params: Vec4::new(0.25, 0.6, 2.5, 0.0), // mode=0 (glow)
            ..Default::default()
        }
    }

    /// Create a prominent glow (good for headers/titles)
    pub fn prominent(color: Color) -> Self {
        let c = color.to_linear();
        Self {
            color: Vec4::new(c.red, c.green, c.blue, 0.15),
            params: Vec4::new(0.4, 0.5, 3.0, 0.0), // mode=0 (glow)
            ..Default::default()
        }
    }

    /// Create an intense "neon" glow effect
    pub fn neon(color: Color) -> Self {
        let c = color.to_linear();
        Self {
            color: Vec4::new(c.red, c.green, c.blue, 0.6),
            params: Vec4::new(0.4, 1.8, 1.5, 0.0), // mode=0 (glow)
            ..Default::default()
        }
    }

    /// Create an icy sheen effect (cool reflective plane)
    /// Uses params.w > 0.5 to signal "icy mode" to shader
    pub fn icy_sheen(color: Color) -> Self {
        let c = color.to_linear();
        Self {
            color: Vec4::new(
                c.red * 0.7 + 0.3,
                c.green * 0.7 + 0.3,
                c.blue * 0.8 + 0.2,
                0.25,
            ),
            params: Vec4::new(0.8, 0.6, 2.0, 1.0), // mode=1 (icy)
            ..Default::default()
        }
    }
}

impl UiMaterial for TextGlowMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/text_glow.wgsl".into()
    }
}

/// System: sync ShaderEffectContext → TextGlowMaterial uniforms
///
/// Updates all text glow materials with the current effect context from the theme.
/// This provides theme-reactive behavior without requiring material recreation.
fn sync_effect_context_to_text_glow(
    ctx: Res<ShaderEffectContext>,
    mut text_glow_materials: ResMut<Assets<TextGlowMaterial>>,
) {
    // Only update if context changed
    if !ctx.is_changed() {
        return;
    }

    // Pack effect params into Vec4s for uniform binding
    let effect_glow = Vec4::new(
        ctx.glow_radius,
        ctx.glow_intensity,
        ctx.glow_falloff,
        ctx.sheen_speed,
    );
    let effect_anim = Vec4::new(
        ctx.sparkle_threshold,
        ctx.breathe_speed,
        ctx.breathe_amplitude,
        0.0, // reserved
    );

    for (_, mat) in text_glow_materials.iter_mut() {
        mat.effect_glow = effect_glow;
        mat.effect_anim = effect_anim;
        mat.theme_accent = ctx.accent;
    }
}

/// System: sync text geometry → TextGlowMaterial uniforms
///
/// For each entity with a TextGlowTarget, looks up the target text entity's
/// position and metrics, then updates the material's geometry uniforms.
/// This enables position-aware shader effects (baseline glow, per-line effects, etc.)
fn sync_text_geometry_to_materials(
    glow_query: Query<(&TextGlowTarget, &MaterialNode<TextGlowMaterial>)>,
    text_query: Query<(&crate::text::UiTextPositionCache, &crate::text::GlyphonUiText)>,
    mut materials: ResMut<Assets<TextGlowMaterial>>,
) {
    for (target, material_node) in glow_query.iter() {
        // Look up the target text entity
        let Ok((position, ui_text)) = text_query.get(target.0) else {
            continue;
        };

        // Get the material handle and update
        let Some(mat) = materials.get_mut(material_node.0.id()) else {
            continue;
        };

        // Build geometry from position cache and text metrics
        let geometry = TextGeometry::from_position_and_metrics(
            position.left,
            position.top,
            position.width,
            position.height,
            ui_text.metrics.font_size,
            ui_text.metrics.line_height,
        );

        // Update material uniforms
        let (bounds, metrics) = geometry.to_shader_vecs();
        mat.text_bounds = bounds;
        mat.text_metrics = metrics;
    }
}
