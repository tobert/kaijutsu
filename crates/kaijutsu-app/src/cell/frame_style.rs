//! Frame style configuration loaded from RON files.
//!
//! Defines the visual appearance of cell frames via 9-slice shader configurations.
//! Styles are loaded from `assets/frames/*.frame.ron` files.

use bevy::{
    asset::{io::Reader, AssetLoader, LoadContext},
    prelude::*,
};
use serde::Deserialize;
use std::collections::HashMap;

// ============================================================================
// FRAME STYLE ASSET
// ============================================================================

/// A frame style defines the visual appearance of a cell frame.
///
/// Loaded from RON files in `assets/frames/`.
#[derive(Asset, TypePath, Debug, Clone, Deserialize)]
pub struct FrameStyle {
    /// Human-readable name for this style
    pub name: String,

    /// Size of corner shader regions (pixels)
    #[serde(default = "default_corner_size")]
    pub corner_size: f32,

    /// Thickness of edge shader regions (pixels)
    #[serde(default = "default_edge_thickness")]
    pub edge_thickness: f32,

    /// Padding between content and frame inner edge (pixels)
    #[serde(default = "default_content_padding")]
    pub content_padding: f32,

    /// Corner shader definition (required)
    pub corner: ShaderDef,

    /// Horizontal edge shader (optional - corners meet if omitted)
    #[serde(default)]
    pub edge_h: Option<EdgeDef>,

    /// Vertical edge shader (optional)
    #[serde(default)]
    pub edge_v: Option<EdgeDef>,

    /// Animation synchronization mode
    #[serde(default)]
    pub animation: AnimationMode,

    /// State-specific overrides (focus, insert, etc.)
    #[serde(default)]
    pub states: HashMap<String, StateOverride>,
}

fn default_corner_size() -> f32 {
    48.0
}
fn default_edge_thickness() -> f32 {
    6.0
}
fn default_content_padding() -> f32 {
    8.0
}

impl Default for FrameStyle {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            corner_size: 48.0,
            edge_thickness: 6.0,
            content_padding: 8.0,
            corner: ShaderDef::default(),
            edge_h: None,
            edge_v: None,
            animation: AnimationMode::default(),
            states: HashMap::new(),
        }
    }
}

// ============================================================================
// SHADER DEFINITION
// ============================================================================

/// Defines shader parameters for a frame piece.
#[derive(Debug, Clone, Deserialize)]
pub struct ShaderDef {
    /// Path to the shader file (relative to assets/)
    #[serde(default = "default_corner_shader")]
    pub path: String,

    /// Base color (RGBA)
    #[serde(default = "default_color")]
    pub color: [f32; 4],

    /// Shader parameters (glow_radius, intensity, pulse_speed, bracket_length)
    #[serde(default = "default_params")]
    pub params: [f32; 4],

    /// Optional texture path for hybrid procedural+texture shaders
    #[serde(default)]
    pub texture: Option<String>,
}

fn default_corner_shader() -> String {
    "shaders/frame_corner.wgsl".to_string()
}
fn default_color() -> [f32; 4] {
    [0.7, 0.5, 0.9, 1.0]
}
fn default_params() -> [f32; 4] {
    [0.15, 1.2, 1.5, 0.25]
}

impl Default for ShaderDef {
    fn default() -> Self {
        Self {
            path: default_corner_shader(),
            color: default_color(),
            params: default_params(),
            texture: None,
        }
    }
}

impl ShaderDef {
    /// Convert color array to Vec4
    pub fn color_vec4(&self) -> Vec4 {
        Vec4::new(self.color[0], self.color[1], self.color[2], self.color[3])
    }

    /// Convert params array to Vec4
    pub fn params_vec4(&self) -> Vec4 {
        Vec4::new(
            self.params[0],
            self.params[1],
            self.params[2],
            self.params[3],
        )
    }
}

// ============================================================================
// EDGE DEFINITION
// ============================================================================

/// Defines an edge shader with tiling/stretching mode.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeDef {
    /// Shader definition
    pub shader: ShaderDef,

    /// Tiling mode
    #[serde(default)]
    pub mode: EdgeMode,
}

impl Default for EdgeDef {
    fn default() -> Self {
        Self {
            shader: ShaderDef {
                path: "shaders/frame_edge.wgsl".to_string(),
                color: [0.5, 0.4, 0.8, 0.6],
                params: [0.1, 0.8, 1.0, 0.0],
                texture: None,
            },
            mode: EdgeMode::default(),
        }
    }
}

/// How edges should fill their space.
#[derive(Debug, Clone, Copy, Deserialize)]
pub enum EdgeMode {
    /// Stretch the pattern to fit
    Stretch,
    /// Tile at the given pixel size
    Tile(f32),
}

impl Default for EdgeMode {
    fn default() -> Self {
        EdgeMode::Tile(24.0)
    }
}

impl EdgeMode {
    /// Get tile info values for shader uniform
    pub fn tile_info(&self) -> (f32, f32) {
        match self {
            EdgeMode::Stretch => (0.0, 0.0),       // tile_size=0, mode=0 (stretch)
            EdgeMode::Tile(size) => (*size, 1.0), // tile_size, mode=1 (tile)
        }
    }
}

// ============================================================================
// ANIMATION MODE
// ============================================================================

/// How animation timing is synchronized across frame pieces.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
pub enum AnimationMode {
    /// All pieces share the same time (coherent animation)
    #[default]
    Synced,
    /// Each piece has independent timing (varied animation)
    Independent,
}

// ============================================================================
// STATE OVERRIDES
// ============================================================================

/// Overrides for specific states (focus, insert mode, etc.)
#[derive(Debug, Clone, Deserialize, Default)]
pub struct StateOverride {
    /// Override color (if Some)
    pub color: Option<[f32; 4]>,

    /// Override shader params (if Some)
    pub params: Option<[f32; 4]>,

    /// Additional intensity multiplier
    #[serde(default = "default_intensity")]
    pub intensity: f32,
}

fn default_intensity() -> f32 {
    1.0
}

impl StateOverride {
    /// Apply this override to a base color
    pub fn apply_color(&self, base: Vec4) -> Vec4 {
        match self.color {
            Some(c) => Vec4::new(c[0], c[1], c[2], c[3]) * self.intensity,
            None => base * self.intensity,
        }
    }

    /// Apply this override to base params
    pub fn apply_params(&self, base: Vec4) -> Vec4 {
        match self.params {
            Some(p) => Vec4::new(p[0], p[1], p[2], p[3]),
            None => base,
        }
    }
}

// ============================================================================
// FRAME STYLE MAPPING
// ============================================================================

/// Maps cell kinds to frame styles.
///
/// This resource defines which frame style each cell type uses.
#[derive(Resource)]
pub struct FrameStyleMapping {
    /// Style for code cells
    pub code: Handle<FrameStyle>,
    /// Style for output cells
    pub output: Handle<FrameStyle>,
    /// Style for markdown cells
    pub markdown: Handle<FrameStyle>,
    /// Style for system cells
    pub system: Handle<FrameStyle>,
    /// Style for user messages
    pub user_message: Handle<FrameStyle>,
    /// Style for agent messages
    pub agent_message: Handle<FrameStyle>,
    /// Fallback default style
    pub default: Handle<FrameStyle>,
}

impl FrameStyleMapping {
    /// Get the style handle for a cell kind
    pub fn style_for(&self, kind: super::CellKind) -> Handle<FrameStyle> {
        match kind {
            super::CellKind::Code => self.code.clone(),
            super::CellKind::Output => self.output.clone(),
            super::CellKind::Markdown => self.markdown.clone(),
            super::CellKind::System => self.system.clone(),
            super::CellKind::UserMessage => self.user_message.clone(),
            super::CellKind::AgentMessage => self.agent_message.clone(),
        }
    }
}

// ============================================================================
// ASSET LOADER
// ============================================================================

/// Asset loader for `.frame.ron` files.
#[derive(Default, bevy::reflect::TypePath)]
pub struct FrameStyleLoader;

impl AssetLoader for FrameStyleLoader {
    type Asset = FrameStyle;
    type Settings = ();
    type Error = FrameStyleLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let style: FrameStyle = ron::de::from_bytes(&bytes)?;
        Ok(style)
    }

    fn extensions(&self) -> &[&str] {
        &["frame.ron"]
    }
}

/// Errors that can occur when loading frame styles.
#[derive(Debug, thiserror::Error)]
pub enum FrameStyleLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("RON parse error: {0}")]
    Ron(#[from] ron::error::SpannedError),
}

