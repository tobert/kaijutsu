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

impl CornerMaterial {
    /// Create a corner with specific flip orientation
    pub fn with_flip(mut self, flip_x: bool, flip_y: bool) -> Self {
        self.flip = Vec4::new(
            if flip_x { 1.0 } else { 0.0 },
            if flip_y { 1.0 } else { 0.0 },
            0.0,
            0.0,
        );
        self
    }

    /// Create a corner with specific dimensions
    pub fn with_dimensions(mut self, width: f32, height: f32, corner_size: f32) -> Self {
        self.dimensions = Vec4::new(width, height, corner_size, 1.0);
        self
    }
}

impl UiMaterial for CornerMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/corner.wgsl".into()
    }
}

/// Helper to identify which corner this entity represents
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CornerPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl CornerPosition {
    /// Get flip values for this corner position
    pub fn flip(&self) -> (bool, bool) {
        match self {
            CornerPosition::TopLeft => (false, false),
            CornerPosition::TopRight => (true, false),
            CornerPosition::BottomLeft => (false, true),
            CornerPosition::BottomRight => (true, true),
        }
    }
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

impl EdgeMaterial {
    /// Set edge to tile mode with given tile size
    pub fn with_tile(mut self, tile_size: f32) -> Self {
        self.tile_info.x = tile_size;
        self.tile_info.y = 1.0; // Tile mode
        self
    }

    /// Set edge to stretch mode
    pub fn with_stretch(mut self) -> Self {
        self.tile_info.y = 0.0; // Stretch mode
        self
    }

    /// Set edge dimensions
    pub fn with_dimensions(mut self, length: f32, thickness: f32) -> Self {
        self.tile_info.z = length;
        self.tile_info.w = thickness;
        self
    }

    /// Set edge as vertical
    pub fn vertical(mut self) -> Self {
        self.orientation.x = 1.0;
        self
    }

    /// Set edge as horizontal
    pub fn horizontal(mut self) -> Self {
        self.orientation.x = 0.0;
        self
    }
}

impl UiMaterial for EdgeMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/edge_glow.wgsl".into()
    }
}

/// Helper to identify edge position
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgePosition {
    Top,
    Bottom,
    Left,
    Right,
}

impl EdgePosition {
    /// Returns true if this is a vertical edge (left/right)
    pub fn is_vertical(&self) -> bool {
        matches!(self, EdgePosition::Left | EdgePosition::Right)
    }
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
#[derive(Component, Debug)]
pub struct FramePiece;

/// Marker for corner pieces specifically
#[derive(Component, Debug)]
pub struct CornerMarker;

/// Marker for edge pieces specifically
#[derive(Component, Debug)]
pub struct EdgeMarker;
