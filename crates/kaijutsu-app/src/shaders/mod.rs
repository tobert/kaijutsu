//! Shader effects for Kaijutsu UI
//!
//! This module provides custom `UiMaterial` implementations for various visual effects:
//! - `GlowBorderMaterial` - Animated glowing border for focused elements (legacy)
//! - `ShimmerMaterial` - Sparkle/twinkle overlay for active states
//! - `PulseRingMaterial` - Expanding ring ripple effect
//! - `ScanlinesMaterial` - Subtle CRT/cyberpunk scanlines
//! - `HoloBorderMaterial` - Rainbow/gradient animated border
//! - `CornerMaterial` / `EdgeMaterial` - 9-slice frame system (new)
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

pub mod nine_slice;

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

use nine_slice::{CornerMaterial, EdgeMaterial, ErrorFrameMaterial};

/// Plugin that registers all shader effect materials.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
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
        ))
        .add_systems(Update, update_shader_time);
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
