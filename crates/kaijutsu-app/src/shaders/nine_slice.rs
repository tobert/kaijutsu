//! 9-Slice Frame Materials
//!
//! Provides corner and edge materials for the 9-slice frame system.
//! Corners use flip uniforms to render all four corners with one shader.
//! Edges support tiling or stretching modes.

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

// ============================================================================
// CORNER MATERIAL
// ============================================================================

/// Corner shader with flip support for all four orientations.
///
/// One shader handles all four corners via `flip` uniform:
/// - Top-left: flip = (0, 0) - no flip
/// - Top-right: flip = (1, 0) - flip_x
/// - Bottom-left: flip = (0, 1) - flip_y
/// - Bottom-right: flip = (1, 1) - flip both
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct CornerMaterial {
    /// Base color (RGBA) - tinted by state
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=glow_radius, y=intensity, z=pulse_speed, w=bracket_length
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time
    #[uniform(2)]
    pub time: Vec4,
    /// Flip: x=flip_x (0 or 1), y=flip_y (0 or 1)
    #[uniform(3)]
    pub flip: Vec4,
    /// Dimensions: x=width_px, y=height_px, z=corner_size_px, w=scale
    #[uniform(4)]
    pub dimensions: Vec4,
}

impl Default for CornerMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.7, 0.5, 0.9, 1.0),
            params: Vec4::new(0.15, 1.2, 1.5, 0.25),
            time: Vec4::ZERO,
            flip: Vec4::ZERO, // Top-left by default
            dimensions: Vec4::new(48.0, 48.0, 48.0, 1.0),
        }
    }
}


impl UiMaterial for CornerMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/frame_corner.wgsl".into()
    }
}

/// Helper to identify which corner this entity represents
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Reflect)]
#[reflect(Component)]
pub enum CornerPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}


// ============================================================================
// EDGE MATERIAL
// ============================================================================

/// Edge shader with tiling or stretching support.
///
/// Edges fill the space between corners and can either stretch
/// a pattern to fit or tile it at a fixed size.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct EdgeMaterial {
    /// Base color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=glow_radius, y=intensity, z=pulse_speed, w=unused
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time
    #[uniform(2)]
    pub time: Vec4,
    /// Tile info: x=tile_size, y=mode (0=stretch, 1=tile), z=length_px, w=thickness_px
    #[uniform(3)]
    pub tile_info: Vec4,
    /// Orientation: x=is_vertical (0=horizontal, 1=vertical)
    #[uniform(4)]
    pub orientation: Vec4,
}

impl Default for EdgeMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.5, 0.4, 0.8, 0.6),
            params: Vec4::new(0.1, 0.8, 1.0, 0.0),
            time: Vec4::ZERO,
            tile_info: Vec4::new(24.0, 1.0, 100.0, 6.0), // tile_size, tile_mode, length, thickness
            orientation: Vec4::ZERO, // Horizontal by default
        }
    }
}


impl UiMaterial for EdgeMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/frame_edge.wgsl".into()
    }
}

/// Helper to identify edge position
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Reflect)]
#[reflect(Component)]
pub enum EdgePosition {
    Top,
    Bottom,
    Left,
    Right,
}

// ============================================================================
// ERROR FRAME MATERIAL
// ============================================================================

/// Fallback shader for missing frame assets.
///
/// Displays a red dashed border to indicate configuration error.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ErrorFrameMaterial {
    /// Color (red by default)
    #[uniform(0)]
    pub color: Vec4,
    /// Time: x=elapsed_time (for animation)
    #[uniform(1)]
    pub time: Vec4,
}

impl Default for ErrorFrameMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(1.0, 0.2, 0.2, 0.9),
            time: Vec4::ZERO,
        }
    }
}

impl UiMaterial for ErrorFrameMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/error_frame.wgsl".into()
    }
}

// ============================================================================
// FRAME PIECE MARKERS
// ============================================================================

/// Marker component for any frame piece (corner or edge)
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct FramePiece;

/// Marker for corner pieces specifically
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct CornerMarker;

/// Marker for edge pieces specifically
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct EdgeMarker;

// ============================================================================
// CHASING BORDER MATERIAL
// ============================================================================

/// Single-node border with traveling neon light effect.
///
/// Unlike the 9-slice frame system (which uses 8 child nodes), this material
/// creates a complete animated border in a single node. Ideal for containers
/// like dashboard columns where you want the "chasing light" effect.
///
/// The chase light travels around the perimeter in a continuous loop.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ChasingBorderMaterial {
    /// Base border color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=border_thickness, y=glow_radius, z=glow_intensity, w=chase_speed
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time
    #[uniform(2)]
    pub time: Vec4,
    /// Chase params: x=chase_width (0-1), y=chase_intensity, z=chase_tail_length, w=_
    #[uniform(3)]
    pub chase: Vec4,
    /// Secondary color for the chase highlight (can be accent2 for hot pink on cyan)
    #[uniform(4)]
    pub chase_color: Vec4,
}

impl Default for ChasingBorderMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.0, 1.0, 1.0, 0.8),          // Cyan border
            params: Vec4::new(2.0, 0.3, 1.0, 0.5),         // 2px thick, medium glow, speed 0.5
            time: Vec4::ZERO,
            chase: Vec4::new(0.15, 2.0, 0.3, 0.0),         // 15% width, 2x intensity, tail
            chase_color: Vec4::new(1.0, 0.0, 0.5, 1.0),    // Hot pink chase
        }
    }
}

impl ChasingBorderMaterial {
    /// Create from theme colors
    pub fn from_theme(border_color: Color, chase_color: Color) -> Self {
        Self {
            color: border_color.to_linear().to_vec4(),
            chase_color: chase_color.to_linear().to_vec4(),
            ..default()
        }
    }

    /// Set border thickness in pixels (will be converted to UV space in shader)
    pub fn with_thickness(mut self, pixels: f32) -> Self {
        self.params.x = pixels;
        self
    }

    /// Set chase animation speed (cycles per second)
    pub fn with_chase_speed(mut self, speed: f32) -> Self {
        self.params.w = speed;
        self
    }

    /// Set glow parameters
    pub fn with_glow(mut self, radius: f32, intensity: f32) -> Self {
        self.params.y = radius;
        self.params.z = intensity;
        self
    }

    /// Set chase width (0-1, portion of perimeter the chase covers)
    pub fn with_chase_width(mut self, width: f32) -> Self {
        self.chase.x = width;
        self
    }

    /// Set color cycle speed (0 = static white, >0 = rainbow cycle speed)
    pub fn with_color_cycle(mut self, speed: f32) -> Self {
        self.chase_color.w = speed;
        self
    }
}

impl UiMaterial for ChasingBorderMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/chasing_border.wgsl".into()
    }
}

/// Marker component for entities using the chasing border
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct ChasingBorder;
