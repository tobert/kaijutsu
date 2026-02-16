//! MSDF text rendering pipeline for Bevy.
//!
//! Provides GPU rendering of MSDF text with support for effects
//! like glow and rainbow coloring.

use bevy::prelude::*;
use bevy::mesh::VertexBufferLayout;
use bevy::render::{
    render_graph::{NodeRunError, RenderGraphContext, RenderLabel, ViewNode},
    render_resource::*,
    renderer::{RenderContext, RenderDevice, RenderQueue},
    view::{ExtractedWindows, ViewTarget},
    Extract,
};
use bevy::render::render_resource::binding_types::{sampler, texture_2d, uniform_buffer};
use bytemuck::{Pod, Zeroable};

use super::atlas::MsdfAtlas;
use super::buffer::{MsdfTextAreaConfig, MsdfTextBuffer, PositionedGlyph, TextBounds};
use super::{MsdfText, SdfTextEffects};
use crate::text::resources::MsdfRenderConfig;

/// MSDF textures are generated at 64 pixels per em.
/// Higher resolution provides 4px effective range at 16px rendering,
/// eliminating stroke weight instability from insufficient distance field fidelity.
pub const MSDF_PX_PER_EM: f32 = 64.0;

// ============================================================================
// DEBUG GEOMETRY HELPERS
// ============================================================================

/// Debug color constants (using near-zero alpha as marker for shader).
#[cfg(debug_assertions)]
mod debug_colors {
    /// Red for quad outlines.
    pub const RED: [u8; 4] = [255, 50, 50, 1];
    /// Green for pen position (glyph.x, glyph.y from cosmic-text).
    pub const GREEN: [u8; 4] = [50, 255, 50, 1];
    /// Blue for anchor point (where origin is in the MSDF bitmap).
    pub const BLUE: [u8; 4] = [50, 100, 255, 1];
    /// Yellow for quad top-left corner (final rendered position).
    pub const YELLOW: [u8; 4] = [255, 255, 50, 1];
}

/// Generate a small dot (quad) at the given screen position.
#[cfg(debug_assertions)]
fn debug_dot(
    vertices: &mut Vec<MsdfVertex>,
    screen_x: f32,
    screen_y: f32,
    resolution: [f32; 2],
    color: [u8; 4],
) {
    const DOT_SIZE: f32 = 4.0; // Dot size in pixels
    const DEBUG_Z: f32 = 0.0; // Debug geometry renders in front
    const DEBUG_IMPORTANCE: f32 = 0.5; // Normal weight for debug geometry

    let half = DOT_SIZE / 2.0;
    let x0 = (screen_x - half) * 2.0 / resolution[0] - 1.0;
    let y0 = 1.0 - (screen_y - half) * 2.0 / resolution[1];
    let x1 = (screen_x + half) * 2.0 / resolution[0] - 1.0;
    let y1 = 1.0 - (screen_y + half) * 2.0 / resolution[1];

    // Dummy UV coords (shader ignores them for debug primitives)
    let uv = [0.5, 0.5];

    // Two triangles for the quad
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE, effects: 0 });
}

/// Generate a rectangle outline (4 thin quads) for the given screen rect.
#[cfg(debug_assertions)]
fn debug_rect_outline(
    vertices: &mut Vec<MsdfVertex>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    resolution: [f32; 2],
    color: [u8; 4],
) {
    const LINE_WIDTH: f32 = 1.5; // Line width in pixels
    const DEBUG_Z: f32 = 0.0; // Debug geometry renders in front
    const DEBUG_IMPORTANCE: f32 = 0.5; // Normal weight for debug geometry

    // Convert to NDC helper
    let to_ndc = |px: f32, py: f32| -> [f32; 2] {
        [px * 2.0 / resolution[0] - 1.0, 1.0 - py * 2.0 / resolution[1]]
    };

    let uv = [0.5, 0.5];
    let imp = DEBUG_IMPORTANCE;

    // Top edge
    let [x0, y0] = to_ndc(x, y);
    let [x1, y1] = to_ndc(x + width, y + LINE_WIDTH);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });

    // Bottom edge
    let [x0, y0] = to_ndc(x, y + height - LINE_WIDTH);
    let [x1, y1] = to_ndc(x + width, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });

    // Left edge
    let [x0, y0] = to_ndc(x, y);
    let [x1, y1] = to_ndc(x + LINE_WIDTH, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });

    // Right edge
    let [x0, y0] = to_ndc(x + width - LINE_WIDTH, y);
    let [x1, y1] = to_ndc(x + width, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp, effects: 0 });
}

/// Label for the MSDF text render node.
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct MsdfTextRenderNodeLabel;

/// GPU vertex for MSDF text rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MsdfVertex {
    /// Position in screen space (x, y) and depth (z).
    /// Z is used for depth testing to prevent overlap artifacts.
    pub position: [f32; 3],
    /// UV coordinates in atlas.
    pub uv: [f32; 2],
    /// Color (RGBA8).
    pub color: [u8; 4],
    /// Semantic importance (0.0 = faded/thin, 0.5 = normal, 1.0 = bold/emphasized).
    /// Used by shader to adjust stroke weight based on cursor proximity or agent activity.
    pub importance: f32,
    /// Per-vertex effect flags bitfield.
    /// Bit 0: rainbow color cycling. Bits 1-31: reserved.
    pub effects: u32,
}

/// GPU uniform for MSDF rendering.
///
/// Glow is handled by the post-process bloom node (bloom.rs / msdf_bloom.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, ShaderType)]
pub struct MsdfUniforms {
    /// Viewport resolution.
    pub resolution: [f32; 2],
    /// MSDF range in pixels.
    pub msdf_range: f32,
    /// Time for animations.
    pub time: f32,
    /// Rainbow effect (0 = off, 1 = on).
    pub rainbow: u32,
    /// Debug mode: 0=off, 1=dots, 2=dots+quads.
    pub debug_mode: u32,
    /// SDF texel size (1.0 / atlas_width, 1.0 / atlas_height) for neighbor sampling.
    /// Used by shader-based hinting to detect stroke direction via gradient.
    pub sdf_texel: [f32; 2],
    /// Hinting strength (0.0 = off, 1.0 = full).
    /// Controls how aggressively horizontal strokes are sharpened.
    pub hint_amount: f32,
    /// Stem darkening strength (0.0 = off, ~0.15 = ClearType-like, 0.5 = max).
    /// Thickens thin strokes at small font sizes by shifting the SDF threshold inward.
    /// This is the #1 technique for matching ClearType quality at 12-16px sizes.
    pub stem_darkening: f32,
    /// TAA jitter offset in pixels (sub-pixel displacement for temporal accumulation).
    /// Applied to vertex positions to sample different sub-pixel locations each frame.
    /// Uses Halton(2,3) sequence for well-distributed 8-sample coverage.
    pub jitter_offset: [f32; 2],
    /// Current frame index in the TAA sequence (0-7, cycles).
    /// Used for debugging and potential future confidence tracking.
    pub taa_frame_index: u32,
    /// Whether TAA jitter is enabled (0 = off, 1 = on).
    /// Allows toggling jitter for A/B comparison without changing other settings.
    pub taa_enabled: u32,
    /// Horizontal stroke AA scale (1.0-1.3). Wider AA for vertical strokes.
    /// Higher values = softer vertical edges.
    pub horz_scale: f32,
    /// Vertical stroke AA scale (0.5-0.8). Sharper AA for horizontal strokes.
    /// Lower values = crisper horizontal edges (stems, crossbars).
    pub vert_scale: f32,
    /// SDF threshold for text rendering (0.45-0.55). Lower = thicker strokes.
    /// Default 0.5 is the edge of the signed distance field.
    pub text_bias: f32,
    /// Gamma correction for alpha (< 1.0 widens AA for light-on-dark, > 1.0 for dark-on-light).
    /// Default 0.85 compensates for perceptual thinning of light text on dark backgrounds.
    pub gamma_correction: f32,
}

impl Default for MsdfUniforms {
    fn default() -> Self {
        Self {
            resolution: [1280.0, 720.0],
            msdf_range: 4.0, // Must match MsdfAtlas::DEFAULT_RANGE
            time: 0.0,
            rainbow: 0,
            debug_mode: 0,
            sdf_texel: [1.0 / 1024.0, 1.0 / 1024.0], // Default atlas size
            hint_amount: 0.8, // Enable hinting by default (80% strength)
            // Stem darkening: 0.15 = ClearType-like weight for 12-16px text
            stem_darkening: 0.15,
            jitter_offset: [0.0, 0.0],
            taa_frame_index: 0,
            taa_enabled: 1, // Enable TAA jitter by default
            horz_scale: 1.1, // Wider AA for vertical strokes
            vert_scale: 0.6, // Sharper AA for horizontal strokes
            text_bias: 0.5,  // Standard SDF threshold
            gamma_correction: 0.85, // Gamma-correct alpha for light-on-dark
        }
    }
}

// ============================================================================
// TAA JITTER SEQUENCE
// ============================================================================

/// TAA sample count for the Halton sequence.
/// 8 samples provides good coverage with reasonable accumulation time.
pub const TAA_SAMPLE_COUNT: u32 = 8;

/// Halton sequence for TAA jitter (base 2, 3).
///
/// This sequence provides well-distributed sub-pixel offsets that:
/// - Cover the pixel area uniformly over 8 frames
/// - Have low discrepancy (avoid clustering)
/// - Match Bevy's TAA implementation for consistency
///
/// Values are in range [-0.5, 0.5] (centered on pixel).
const HALTON_SEQUENCE: [[f32; 2]; 8] = [
    // Halton(2, 3) sequence, offset to center on pixel
    [0.0, -0.3333333],     // n=1: (1/2, 1/3) - 0.5
    [-0.25, 0.3333333],    // n=2: (1/4, 2/3) - 0.5
    [0.25, -0.1111111],    // n=3: (3/4, 1/9) - 0.5
    [-0.375, 0.2222222],   // n=4: (1/8, 4/9) - 0.5
    [0.125, -0.4444444],   // n=5: (5/8, 7/9) - 0.5
    [-0.125, 0.0555556],   // n=6: (3/8, 5/9) - 0.5
    [0.375, 0.3888889],    // n=7: (7/8, 2/9) - 0.5
    [-0.4375, -0.2777778], // n=8: (1/16, 8/9) - 0.5
];

/// Get the jitter offset for a given frame index.
///
/// Returns sub-pixel offset in range [-0.5, 0.5] for both x and y.
/// The sequence cycles every `TAA_SAMPLE_COUNT` frames.
#[inline]
pub fn get_taa_jitter(frame_index: u32) -> [f32; 2] {
    HALTON_SEQUENCE[(frame_index % TAA_SAMPLE_COUNT) as usize]
}

/// Extracted text area for rendering.
#[allow(dead_code)]
pub struct ExtractedMsdfText {
    pub entity: Entity,
    pub glyphs: Vec<PositionedGlyph>,
    pub left: f32,
    pub top: f32,
    pub scale: f32,
    pub bounds: TextBounds,
    pub effects: SdfTextEffects,
    /// Per-vertex effect flags computed from `effects`.
    /// Bit 0: rainbow.
    pub effects_bits: u32,
    /// Raw text content for UI text that needs shaping (None if pre-shaped)
    pub raw_text: Option<String>,
    /// Color for UI text
    pub color: [u8; 4],
}

/// Resource containing extracted text areas.
#[derive(Resource, Default)]
pub struct ExtractedMsdfTexts {
    pub texts: Vec<ExtractedMsdfText>,
}

/// Extracted atlas data for the render world.
#[derive(Resource)]
pub struct ExtractedMsdfAtlas {
    /// Mapping from glyph keys to their atlas regions.
    pub regions: std::collections::HashMap<super::atlas::GlyphKey, super::atlas::AtlasRegion>,
    /// The GPU texture handle.
    pub texture: Handle<Image>,
    /// Atlas dimensions.
    pub width: u32,
    pub height: u32,
    /// MSDF range in pixels.
    pub msdf_range: f32,
}

/// Extracted debug overlay state for render world.
#[cfg(debug_assertions)]
#[derive(Resource, Default)]
pub struct ExtractedMsdfDebugMode {
    /// Debug mode: 0=off, 1=dots, 2=dots+quads.
    pub mode: u32,
}

/// Extracted render configuration for MSDF text.
///
/// Extracted from `MsdfRenderConfig` and `Theme` in the main world.
/// The pipeline will not render if this is not present or not initialized.
#[derive(Resource, Clone, Copy)]
pub struct ExtractedMsdfRenderConfig {
    /// Viewport resolution in physical pixels.
    pub resolution: [f32; 2],
    /// Texture format for the render target.
    pub format: TextureFormat,
    /// Whether this config is valid for rendering.
    pub initialized: bool,
    /// Window scale factor (logical to physical pixel multiplier).
    /// Used to convert layout bounds and positions to physical pixels.
    pub scale_factor: f32,
    /// True when resolution changed this frame - bounds may be stale.
    /// During resize, we skip batched scissor clipping to avoid zero-size rects.
    pub resize_in_progress: bool,

    // ═══════════════════════════════════════════════════════════════════════
    // Font rendering parameters (from Theme)
    // ═══════════════════════════════════════════════════════════════════════

    /// Stem darkening strength (0.0-0.5).
    pub stem_darkening: f32,
    /// Hinting strength (0.0-1.0).
    pub hint_amount: f32,
    /// TAA enabled flag.
    pub taa_enabled: bool,
    /// Number of frames for TAA to converge (4-16).
    pub taa_convergence_frames: u32,
    /// Initial blend weight (0.3-0.9).
    pub taa_initial_weight: f32,
    /// Final blend weight (0.05-0.3).
    pub taa_final_weight: f32,
    /// Horizontal stroke AA scale.
    pub horz_scale: f32,
    /// Vertical stroke AA scale.
    pub vert_scale: f32,
    /// SDF threshold for text rendering.
    pub text_bias: f32,
    /// Gamma correction for alpha.
    pub gamma_correction: f32,
    /// Glow intensity (0.0-1.0).
    pub glow_intensity: f32,
    /// Glow spread in pixels.
    pub glow_spread: f32,
    /// Glow color [r, g, b, a].
    pub glow_color: [f32; 4],
}

/// Extracted camera motion for TAA reprojection.
///
/// Contains the motion delta in UV space for history sampling offset.
#[derive(Resource, Default, Clone, Copy)]
pub struct ExtractedCameraMotion {
    /// Motion delta in UV space (0-1 = full screen).
    pub motion_uv: [f32; 2],
}

/// TAA state for MSDF text rendering.
///
/// Tracks the frame counter for Halton sequence jitter and TAA enable state.
/// Lives in the render world and persists across frames.
#[derive(Resource)]
pub struct MsdfTextTaaState {
    /// Current frame index in the TAA sequence (0 to TAA_SAMPLE_COUNT-1).
    pub frame_index: u32,
    /// Whether TAA jitter is enabled.
    pub enabled: bool,
}

impl Default for MsdfTextTaaState {
    fn default() -> Self {
        Self {
            frame_index: 0,
            enabled: true, // Enable by default for quality
        }
    }
}

/// TAA history textures for temporal accumulation.
///
/// Uses ping-pong double buffering: each frame reads from one texture
/// and writes the blended result to the other, then swaps.
///
/// Flow each frame:
/// 1. TAA blend: intermediate + history_read → history_write
/// 2. Blit: history_write → ViewTarget (handles format conversion)
#[derive(Resource)]
pub struct MsdfTextTaaResources {
    /// History texture A (ping).
    /// Note: Field appears unused but owns the GPU texture that history_a_view references.
    #[allow(dead_code)]
    pub history_a: Texture,
    /// History texture B (pong).
    /// Note: Field appears unused but owns the GPU texture that history_b_view references.
    #[allow(dead_code)]
    pub history_b: Texture,
    /// Texture view for history A.
    pub history_a_view: TextureView,
    /// Texture view for history B.
    pub history_b_view: TextureView,
    /// Sampler for reading history textures.
    pub history_sampler: Sampler,
    /// Which history texture to read from this frame (true = A, false = B).
    /// Flips each frame after the TAA pass writes.
    pub read_from_a: bool,
    /// Total frames accumulated (for confidence tracking).
    /// Resets on resolution change or when TAA is toggled.
    pub frames_accumulated: u32,
    /// Cached resolution for detecting resize.
    pub resolution: (u32, u32),
    /// Texture format for history textures (internal format, not swap chain).
    pub format: TextureFormat,
    /// Cached render pipeline for TAA blend.
    pub pipeline: Option<CachedRenderPipelineId>,
    /// Bind group for TAA blend pass (uniforms, intermediate, history_read, sampler).
    pub bind_group: Option<BindGroup>,
    /// Bind group for blit pass (uniforms, history_write, dummy, sampler).
    /// Uses TAA shader with taa_enabled=0 for passthrough.
    pub blit_bind_group: Option<BindGroup>,
}

/// A batch of vertices with common scissor bounds for clipped rendering.
///
/// Text areas with the same bounds are grouped into batches. Each batch
/// gets its own scissor rect to clip text to its designated area.
#[derive(Debug, Clone)]
pub struct TextBatch {
    /// Scissor rect [x, y, width, height] in physical pixels.
    pub scissor: [u32; 4],
    /// Starting vertex index in the vertex buffer.
    pub vertex_start: u32,
    /// Number of vertices in this batch.
    pub vertex_count: u32,
}

impl MsdfTextTaaResources {
    /// Create new TAA resources for the given resolution.
    ///
    /// `format` should match ViewTarget's main_texture_format() for copy compatibility.
    /// Bevy uses Rgba8Unorm internally (linear), with sRGB views for display.
    pub fn new(device: &RenderDevice, width: u32, height: u32, format: TextureFormat) -> Self {
        let size = Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        // Use linear format matching ViewTarget's main_texture_format()
        // Bevy uses Rgba8Unorm/Bgra8Unorm internally, not sRGB
        // view_formats allows sRGB interpretation when rendering
        let base_format = match format {
            TextureFormat::Bgra8UnormSrgb => TextureFormat::Bgra8Unorm,
            TextureFormat::Rgba8UnormSrgb => TextureFormat::Rgba8Unorm,
            other => other, // Keep as-is for HDR or other formats
        };

        let descriptor = TextureDescriptor {
            label: Some("msdf_taa_history"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: base_format,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::COPY_SRC
                | TextureUsages::COPY_DST,
            // Allow sRGB view for rendering
            view_formats: match base_format {
                TextureFormat::Bgra8Unorm => &[TextureFormat::Bgra8UnormSrgb],
                TextureFormat::Rgba8Unorm => &[TextureFormat::Rgba8UnormSrgb],
                _ => &[],
            },
        };

        let history_a = device.create_texture(&TextureDescriptor {
            label: Some("msdf_taa_history_a"),
            ..descriptor
        });
        let history_b = device.create_texture(&TextureDescriptor {
            label: Some("msdf_taa_history_b"),
            ..descriptor
        });

        // Create sRGB views for rendering (shader expects sRGB)
        let srgb_format = match base_format {
            TextureFormat::Bgra8Unorm => Some(TextureFormat::Bgra8UnormSrgb),
            TextureFormat::Rgba8Unorm => Some(TextureFormat::Rgba8UnormSrgb),
            _ => None,
        };

        let history_a_view = history_a.create_view(&TextureViewDescriptor {
            format: srgb_format,
            ..default()
        });
        let history_b_view = history_b.create_view(&TextureViewDescriptor {
            format: srgb_format,
            ..default()
        });

        // Linear sampler for smooth history reads
        let history_sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("msdf_taa_history_sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Nearest,
            ..default()
        });

        Self {
            history_a,
            history_b,
            history_a_view,
            history_b_view,
            history_sampler,
            read_from_a: true,
            frames_accumulated: 0,
            resolution: (width, height),
            format: base_format, // Store base format for history textures
            pipeline: None,
            bind_group: None,
            blit_bind_group: None,
        }
    }

    /// Recreate textures if resolution changed.
    /// Preserves the pipeline ID since it doesn't depend on resolution.
    pub fn resize_if_needed(&mut self, device: &RenderDevice, width: u32, height: u32) {
        if self.resolution != (width, height) && width > 0 && height > 0 {
            let format = self.format;
            let pipeline = self.pipeline.take(); // Preserve pipeline
            *self = Self::new(device, width, height, format);
            self.pipeline = pipeline; // Restore pipeline
        }
    }

    /// Get the history texture view to read from this frame.
    pub fn read_view(&self) -> &TextureView {
        if self.read_from_a {
            &self.history_a_view
        } else {
            &self.history_b_view
        }
    }

    /// Get the history texture view to write to this frame.
    pub fn write_view(&self) -> &TextureView {
        if self.read_from_a {
            &self.history_b_view
        } else {
            &self.history_a_view
        }
    }

    /// Swap read/write textures for next frame.
    /// Does NOT increment frames_accumulated - caller is responsible for that.
    pub fn swap(&mut self) {
        self.read_from_a = !self.read_from_a;
    }
}

/// Render world resources for MSDF text.
#[derive(Resource)]
#[allow(dead_code)]
pub struct MsdfTextResources {
    pub pipeline: CachedRenderPipelineId,
    pub bind_group_layout: BindGroupLayout,
    pub uniform_buffer: Buffer,
    pub vertex_buffer: Option<Buffer>,
    pub bind_group: Option<BindGroup>,
    pub vertex_count: u32,
    /// Batches of vertices grouped by scissor bounds.
    /// Each batch has a scissor rect in physical pixels.
    pub batches: Vec<TextBatch>,
    /// Cached resolution for resize detection.
    pub texture_size: (u32, u32),
    /// Intermediate texture for TAA - MSDF renders here, TAA blends to ViewTarget.
    /// Only used when TAA is enabled.
    pub intermediate_texture: Option<Texture>,
    pub intermediate_texture_view: Option<TextureView>,
    /// Texture format used for the pipeline (for creating compatible textures).
    pub format: TextureFormat,
}

/// MSDF text render pipeline setup.
#[derive(Resource)]
pub struct MsdfTextPipeline {
    /// The bind group layout for creating bind groups.
    pub bind_group_layout: BindGroupLayout,
    /// The layout descriptor for pipeline creation.
    pub bind_group_layout_descriptor: BindGroupLayoutDescriptor,
    pub shader: Handle<Shader>,
}

impl FromWorld for MsdfTextPipeline {
    fn from_world(world: &mut World) -> Self {
        let device = world.resource::<RenderDevice>();
        let asset_server = world.resource::<AssetServer>();

        // Create the layout entries
        let entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                // Uniforms
                uniform_buffer::<MsdfUniforms>(false),
                // Atlas texture
                texture_2d(TextureSampleType::Float { filterable: true }),
                // Atlas sampler
                sampler(SamplerBindingType::Filtering),
            ),
        );

        // Create bind group layout for runtime use
        let bind_group_layout = device.create_bind_group_layout(
            Some("msdf_text_bind_group_layout"),
            &entries,
        );

        // Create descriptor for pipeline creation
        let bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "msdf_text_bind_group_layout",
            entries.to_vec().as_slice(),
        );

        // Load shader from asset
        let shader = asset_server.load("shaders/msdf_text.wgsl");

        Self {
            bind_group_layout,
            bind_group_layout_descriptor,
            shader,
        }
    }
}

// ============================================================================
// TAA PIPELINE
// ============================================================================

/// GPU uniform for TAA blend pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, ShaderType)]
pub struct TaaUniforms {
    /// Resolution for UV calculations.
    pub resolution: [f32; 2],
    /// Number of frames accumulated.
    pub frames_accumulated: u32,
    /// Whether TAA is enabled.
    pub taa_enabled: u32,
    /// Camera motion delta (for Phase 4 reprojection).
    pub camera_motion: [f32; 2],
    /// Number of frames to converge (f32 for shader math).
    pub convergence_frames: f32,
    /// Initial blend weight (first frame opacity).
    pub initial_weight: f32,
    /// Final blend weight (steady-state blend).
    pub final_weight: f32,
    /// Padding for 16-byte alignment.
    pub _padding: f32,
}

impl Default for TaaUniforms {
    fn default() -> Self {
        Self {
            resolution: [1280.0, 720.0],
            frames_accumulated: 0,
            taa_enabled: 1,
            camera_motion: [0.0, 0.0],
            convergence_frames: 8.0,
            initial_weight: 0.5,
            final_weight: 0.1,
            _padding: 0.0,
        }
    }
}

/// Label for the TAA blend render node.
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct MsdfTextTaaNodeLabel;

/// TAA blend pipeline setup.
#[derive(Resource)]
pub struct MsdfTextTaaPipeline {
    pub bind_group_layout: BindGroupLayout,
    pub bind_group_layout_descriptor: BindGroupLayoutDescriptor,
    pub shader: Handle<Shader>,
    /// Uniform buffer for TAA blend pass.
    pub uniform_buffer: Buffer,
    /// Uniform buffer for blit pass (passthrough with taa_enabled=0).
    pub blit_uniform_buffer: Buffer,
}

impl FromWorld for MsdfTextTaaPipeline {
    fn from_world(world: &mut World) -> Self {
        let device = world.resource::<RenderDevice>();
        let asset_server = world.resource::<AssetServer>();

        // TAA bind group: uniforms, current texture, history texture, sampler
        let entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                // Uniforms
                uniform_buffer::<TaaUniforms>(false),
                // Current (intermediate) texture
                texture_2d(TextureSampleType::Float { filterable: true }),
                // History texture
                texture_2d(TextureSampleType::Float { filterable: true }),
                // Sampler
                sampler(SamplerBindingType::Filtering),
            ),
        );

        let bind_group_layout = device.create_bind_group_layout(
            Some("msdf_taa_bind_group_layout"),
            &entries,
        );

        let bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "msdf_taa_bind_group_layout",
            entries.to_vec().as_slice(),
        );

        let shader = asset_server.load("shaders/msdf_text_taa.wgsl");

        let uniform_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("msdf_taa_uniform_buffer"),
            size: std::mem::size_of::<TaaUniforms>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Separate uniform buffer for blit pass (always passthrough)
        let blit_uniform_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("msdf_taa_blit_uniform_buffer"),
            size: std::mem::size_of::<TaaUniforms>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            bind_group_layout,
            bind_group_layout_descriptor,
            shader,
            uniform_buffer,
            blit_uniform_buffer,
        }
    }
}

/// Render node for MSDF text.
#[derive(Default)]
pub struct MsdfTextRenderNode;

impl MsdfTextRenderNode {
    pub const NAME: MsdfTextRenderNodeLabel = MsdfTextRenderNodeLabel;
}

impl ViewNode for MsdfTextRenderNode {
    type ViewQuery = &'static ViewTarget;

    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        view_target: &ViewTarget,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let Some(resources) = world.get_resource::<MsdfTextResources>() else {
            return Ok(());
        };

        // Skip rendering during resize - detect by comparing config resolution
        // against our cached texture size. Due to pipelining, config may have new
        // resolution before our resources are recreated.
        let render_config = world.get_resource::<ExtractedMsdfRenderConfig>();
        if let Some(config) = render_config {
            let config_size = (config.resolution[0] as u32, config.resolution[1] as u32);
            if config_size != resources.texture_size && resources.texture_size != (0, 0) {
                // Resolution mismatch - skip this frame to avoid scissor errors
                return Ok(());
            }
        }

        if resources.vertex_count == 0 {
            return Ok(());
        }

        let Some(vertex_buffer) = &resources.vertex_buffer else {
            return Ok(());
        };

        let Some(bind_group) = &resources.bind_group else {
            return Ok(());
        };

        let pipeline_cache = world.resource::<PipelineCache>();
        let Some(pipeline) = pipeline_cache.get_render_pipeline(resources.pipeline) else {
            return Ok(());
        };

        // Always render to intermediate texture (bloom and TAA read from it).
        // During warmup, intermediate texture may not exist yet - fall back to ViewTarget.
        let (render_target, load_op) = if let Some(intermediate_view) = &resources.intermediate_texture_view {
            // Clear intermediate to transparent black
            (intermediate_view as &TextureView, LoadOp::Clear(Default::default()))
        } else {
            (view_target.out_texture(), LoadOp::Load)
        };

        let mut render_pass = render_context.command_encoder().begin_render_pass(
            &RenderPassDescriptor {
                label: Some("msdf_text_render_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: render_target,
                    resolve_target: None,
                    ops: Operations {
                        load: load_op,
                        store: StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            },
        );

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_vertex_buffer(0, *vertex_buffer.slice(..));

        // Get viewport dimensions for scissor rect clamping.
        // Note: resize_in_progress case is handled by early return at start of run().
        let render_config = world.get_resource::<ExtractedMsdfRenderConfig>();
        let (vp_w, vp_h) = render_config
            .map(|c| (c.resolution[0] as u32, c.resolution[1] as u32))
            .unwrap_or((1920, 1080));

        // Draw each batch with its scissor rect
        if resources.batches.is_empty() {
            // Fallback: no batches but have vertices - draw everything without scissor
            // This can happen if bounds data is stale during resize
            render_pass.set_scissor_rect(0, 0, vp_w, vp_h);
            render_pass.draw(0..resources.vertex_count, 0..1);
        } else {
            for batch in &resources.batches {
                if batch.vertex_count == 0 {
                    continue;
                }

                // Set scissor rect (clamp to viewport dimensions)
                let [x, y, w, h] = batch.scissor;
                let clamped_w = w.min(vp_w.saturating_sub(x));
                let clamped_h = h.min(vp_h.saturating_sub(y));

                // Skip batches with zero-size scissor (fully clipped)
                if clamped_w == 0 || clamped_h == 0 {
                    continue;
                } else {
                    render_pass.set_scissor_rect(
                        x.min(vp_w),
                        y.min(vp_h),
                        clamped_w,
                        clamped_h,
                    );
                }

                render_pass.draw(batch.vertex_start..batch.vertex_start + batch.vertex_count, 0..1);
            }
        }

        Ok(())
    }
}

/// TAA blend render node.
///
/// Reads from intermediate texture and history, blends, and outputs to ViewTarget.
/// When TAA is disabled, this node does nothing (MSDF renders directly to ViewTarget).
#[derive(Default)]
pub struct MsdfTextTaaNode;

impl MsdfTextTaaNode {
    pub const NAME: MsdfTextTaaNodeLabel = MsdfTextTaaNodeLabel;
}

impl ViewNode for MsdfTextTaaNode {
    type ViewQuery = &'static ViewTarget;

    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        view_target: &ViewTarget,
        world: &World,
    ) -> Result<(), NodeRunError> {
        // Skip during resize - detect by comparing config against resources
        let render_config = world.get_resource::<ExtractedMsdfRenderConfig>();
        if let Some(config) = render_config {
            if let Some(resources) = world.get_resource::<MsdfTextResources>() {
                let config_size = (config.resolution[0] as u32, config.resolution[1] as u32);
                if config_size != resources.texture_size && resources.texture_size != (0, 0) {
                    return Ok(());
                }
            }
        }

        // Get TAA state
        let Some(taa_state) = world.get_resource::<MsdfTextTaaState>() else {
            return Ok(());
        };

        // Get required resources - early returns are expected during warmup
        let Some(taa_resources) = world.get_resource::<MsdfTextTaaResources>() else {
            return Ok(());
        };

        let Some(msdf_resources) = world.get_resource::<MsdfTextResources>() else {
            return Ok(());
        };

        // Need intermediate texture — text always renders there now
        let Some(intermediate_view) = &msdf_resources.intermediate_texture_view else {
            return Ok(());
        };

        // Get cached pipeline
        let Some(pipeline_id) = taa_resources.pipeline else {
            return Ok(());
        };

        let pipeline_cache = world.resource::<PipelineCache>();
        let Some(pipeline) = pipeline_cache.get_render_pipeline(pipeline_id) else {
            return Ok(()); // Pipeline not yet compiled
        };

        // Need the blit bind group (always needed for final output)
        let Some(blit_bind_group) = &taa_resources.blit_bind_group else {
            return Ok(());
        };

        if taa_state.enabled {
            // === PASS 1: TAA Blend ===
            // Blend intermediate (current jittered frame) + history_read → history_write
            let Some(bind_group) = &taa_resources.bind_group else {
                return Ok(());
            };

            {
                let mut render_pass = render_context.command_encoder().begin_render_pass(
                    &RenderPassDescriptor {
                        label: Some("msdf_taa_blend_pass"),
                        color_attachments: &[Some(RenderPassColorAttachment {
                            view: taa_resources.write_view(),
                            resolve_target: None,
                            ops: Operations {
                                load: LoadOp::Clear(Default::default()),
                                store: StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    },
                );

                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.draw(0..3, 0..1);
            }
        }

        // === Blit to ViewTarget ===
        // Always needed: copies intermediate (or TAA history) → ViewTarget.
        // When TAA is enabled, blit_bind_group reads from history_write.
        // When TAA is disabled, blit_bind_group reads from intermediate directly.
        {
            let out_texture = view_target.out_texture();
            let mut render_pass = render_context.command_encoder().begin_render_pass(
                &RenderPassDescriptor {
                    label: Some("msdf_taa_blit_pass"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: out_texture,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load,
                            store: StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                },
            );

            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, blit_bind_group, &[]);
            render_pass.draw(0..3, 0..1);
        }

        let _ = intermediate_view; // Used in bind_group creation

        Ok(())
    }
}

/// Extract MSDF text areas from the main world.
#[allow(clippy::type_complexity)]
pub fn extract_msdf_texts(
    mut commands: Commands,
    // Cell text (MsdfTextBuffer + MsdfTextAreaConfig)
    cell_query: Extract<
        Query<
            (Entity, &MsdfTextBuffer, &MsdfTextAreaConfig, &InheritedVisibility, Option<&SdfTextEffects>),
            With<MsdfText>,
        >,
    >,
    // UI text (MsdfUiText + UiTextPositionCache)
    ui_query: Extract<
        Query<
            (Entity, &super::MsdfUiText, &super::UiTextPositionCache, &InheritedVisibility),
        >,
    >,
    // Atlas data
    atlas: Extract<Res<MsdfAtlas>>,
    // Debug overlay (only in debug builds)
    #[cfg(debug_assertions)]
    debug_overlay: Extract<Res<super::MsdfDebugOverlay>>,
) {
    // Extract atlas data for render world
    commands.insert_resource(ExtractedMsdfAtlas {
        regions: atlas.regions.clone(),
        texture: atlas.texture.clone(),
        width: atlas.width,
        height: atlas.height,
        msdf_range: atlas.msdf_range,
    });

    // Extract debug mode
    #[cfg(debug_assertions)]
    commands.insert_resource(ExtractedMsdfDebugMode {
        mode: debug_overlay.mode.as_u32(),
    });

    let mut texts = Vec::new();

    // Extract cell text (conversation blocks, prompt, etc.)
    for (entity, buffer, config, visibility, effects) in cell_query.iter() {
        if !visibility.get() {
            continue;
        }

        let fx = effects.cloned().unwrap_or_default();
        let effects_bits = if fx.rainbow { 1 } else { 0 };
        texts.push(ExtractedMsdfText {
            entity,
            glyphs: buffer.glyphs().to_vec(),
            left: config.left,
            top: config.top,
            scale: config.scale,
            bounds: config.bounds,
            effects: fx,
            effects_bits,
            raw_text: None, // Already shaped
            color: [220, 220, 240, 255],
        });
    }

    // Extract UI text (dashboard labels, status bar, etc.)
    for (entity, ui_text, position, visibility) in ui_query.iter() {
        if !visibility.get() || ui_text.text.is_empty() {
            continue;
        }

        // UI text needs shaping in prepare phase
        texts.push(ExtractedMsdfText {
            entity,
            glyphs: Vec::new(), // Will be shaped in prepare
            left: position.left,
            top: position.top,
            scale: 1.0,
            bounds: TextBounds {
                left: position.left as i32,
                top: position.top as i32,
                right: (position.left + position.width) as i32,
                bottom: (position.top + position.height) as i32,
            },
            effects: SdfTextEffects::default(),
            effects_bits: 0,
            raw_text: Some(ui_text.text.clone()),
            color: ui_text.color,
        });
    }

    commands.insert_resource(ExtractedMsdfTexts { texts });
}

/// Extract render configuration from main world.
///
/// This extracts `MsdfRenderConfig` and `Theme` so the render world has explicit access
/// to resolution, format, and font rendering parameters.
/// If the config is not initialized, rendering will be skipped.
/// Theme is optional - defaults are used when not available (e.g., in tests).
pub fn extract_msdf_render_config(
    mut commands: Commands,
    config: Extract<Res<MsdfRenderConfig>>,
    theme: Extract<Option<Res<crate::ui::theme::Theme>>>,
) {
    // Use theme values or fall back to defaults for test/headless scenarios
    let default_theme = crate::ui::theme::Theme::default();
    let theme = theme.as_ref().map(|t| t.as_ref()).unwrap_or(&default_theme);

    // Detect if resolution changed this frame - bounds may be stale
    let resize_in_progress = config.resolution != config.prev_resolution;

    // Use linear color for glow — shader math and blending happen in linear space
    let glow_linear = theme.font_glow_color.to_linear();
    commands.insert_resource(ExtractedMsdfRenderConfig {
        resolution: config.resolution,
        format: config.format,
        initialized: config.initialized,
        scale_factor: config.scale_factor,
        resize_in_progress,
        // Font rendering parameters from Theme
        stem_darkening: theme.font_stem_darkening,
        hint_amount: theme.font_hint_amount,
        taa_enabled: theme.font_taa_enabled,
        horz_scale: theme.font_horz_scale,
        vert_scale: theme.font_vert_scale,
        text_bias: theme.font_text_bias,
        gamma_correction: theme.font_gamma_correction,
        glow_intensity: theme.font_glow_intensity,
        glow_spread: theme.font_glow_spread,
        glow_color: [glow_linear.red, glow_linear.green, glow_linear.blue, glow_linear.alpha],
        // TAA convergence parameters
        taa_convergence_frames: theme.font_taa_convergence_frames,
        taa_initial_weight: theme.font_taa_initial_weight,
        taa_final_weight: theme.font_taa_final_weight,
    });
}

/// Extract TAA configuration from main world.
///
/// This syncs the `MsdfTaaConfig` resource (controlled by F10 toggle)
/// to the render world's `MsdfTextTaaState`.
pub fn extract_msdf_taa_config(
    taa_config: Extract<Option<Res<super::MsdfTaaConfig>>>,
    mut taa_state: ResMut<MsdfTextTaaState>,
) {
    // Sync enabled state from main world config
    if let Some(config) = taa_config.as_ref() {
        taa_state.enabled = config.enabled;
    }
}

/// Initialize TAA resources when resolution is available.
///
/// Creates the history textures for temporal accumulation and queues the TAA pipeline.
/// Runs once when `MsdfTextTaaResources` doesn't exist and `MsdfTextResources` exists.
pub fn init_msdf_taa_resources(
    mut commands: Commands,
    device: Res<RenderDevice>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
    msdf_resources: Res<MsdfTextResources>,
    taa_pipeline: Res<MsdfTextTaaPipeline>,
    pipeline_cache: Res<PipelineCache>,
) {
    let Some(config) = render_config else {
        return;
    };
    if !config.initialized {
        return;
    }

    let width = config.resolution[0] as u32;
    let height = config.resolution[1] as u32;

    if width == 0 || height == 0 {
        return;
    }

    // Use the same format as the MSDF pipeline (from swap chain)
    let format = msdf_resources.format;

    // Create history textures (same format as swap chain for copy to ViewTarget)
    let mut taa_resources = MsdfTextTaaResources::new(&device, width, height, format);

    // Queue the TAA blend pipeline
    let pipeline_id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("msdf_taa_blend_pipeline".into()),
        layout: vec![taa_pipeline.bind_group_layout_descriptor.clone()],
        push_constant_ranges: vec![],
        vertex: VertexState {
            shader: taa_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("vertex".into()),
            // Fullscreen triangle - no vertex buffers needed
            buffers: vec![],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..default()
        },
        depth_stencil: None, // No depth for fullscreen pass
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            shader: taa_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("fragment".into()),
            targets: vec![Some(ColorTargetState {
                format, // Use same format as MSDF pipeline (from swap chain)
                // Premultiplied alpha: MSDF pipeline outputs premultiplied RGB,
                // so blit must use src*One (not src*SrcAlpha) to avoid alpha^2
                blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: ColorWrites::ALL,
            })],
        }),
        zero_initialize_workgroup_memory: false,
    });

    taa_resources.pipeline = Some(pipeline_id);
    commands.insert_resource(taa_resources);
    trace!("TAA resources initialized: {}x{} format={:?}", width, height, format);
}

/// Prepare MSDF text resources for rendering.
///
/// Requires `ExtractedMsdfRenderConfig` to be present and initialized.
/// Will skip rendering if the config is not ready.
pub fn prepare_msdf_texts(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    pipeline: Res<MsdfTextPipeline>,
    taa_pipeline: Res<MsdfTextTaaPipeline>,
    extracted: Option<Res<ExtractedMsdfTexts>>,
    atlas: Option<Res<ExtractedMsdfAtlas>>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
    camera_motion: Option<Res<ExtractedCameraMotion>>,
    images: Res<bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>>,
    time: Res<Time>,
    mut resources: ResMut<MsdfTextResources>,
    mut taa_state: ResMut<MsdfTextTaaState>,
    mut taa_resources: Option<ResMut<MsdfTextTaaResources>>,
    #[cfg(debug_assertions)]
    debug_mode: Option<Res<ExtractedMsdfDebugMode>>,
) {
    // Require render config to be present and initialized
    let Some(render_config) = render_config else {
        return;
    };
    if !render_config.initialized {
        return;
    }

    // Handle TAA resources resize if needed
    if let Some(ref mut taa_res) = taa_resources {
        let width = render_config.resolution[0] as u32;
        let height = render_config.resolution[1] as u32;
        taa_res.resize_if_needed(&device, width, height);
    }

    let Some(extracted) = extracted else {
        return;
    };

    let Some(atlas) = atlas else {
        return;
    };

    if extracted.texts.is_empty() {
        resources.vertex_count = 0;
        return;
    }

    // Get viewport resolution from extracted config
    let resolution = render_config.resolution;

    // Rainbow is now per-vertex (effects bitfield on each glyph).
    // Uniform kept at 0 for struct layout stability.

    // Get debug mode from extracted resource
    #[cfg(debug_assertions)]
    let debug_mode_value = debug_mode.map(|d| d.mode).unwrap_or(0);
    #[cfg(not(debug_assertions))]
    let debug_mode_value = 0u32;

    // Compute SDF texel size for gradient sampling in shader
    let sdf_texel = [
        1.0 / atlas.width as f32,
        1.0 / atlas.height as f32,
    ];

    // === TAA JITTER ===
    // TAA state is controlled by theme.font_taa_enabled
    // Sync it from the extracted config each frame
    taa_state.enabled = render_config.taa_enabled;

    // Get jitter offset from Halton sequence and advance frame counter
    let jitter_offset = if taa_state.enabled {
        get_taa_jitter(taa_state.frame_index)
    } else {
        [0.0, 0.0]
    };
    let taa_frame_index = taa_state.frame_index;
    // Advance frame counter for next frame (cycles through TAA_SAMPLE_COUNT)
    taa_state.frame_index = (taa_state.frame_index + 1) % TAA_SAMPLE_COUNT;

    // Update uniforms - use theme values for rendering parameters
    let uniforms = MsdfUniforms {
        resolution,
        msdf_range: atlas.msdf_range,
        time: time.elapsed_secs(),
        rainbow: 0,
        debug_mode: debug_mode_value,
        sdf_texel,
        // Use theme values for rendering quality parameters
        hint_amount: render_config.hint_amount,
        stem_darkening: render_config.stem_darkening,
        // TAA jitter for temporal super-resolution
        jitter_offset,
        taa_frame_index,
        taa_enabled: if taa_state.enabled { 1 } else { 0 },
        // Shader hinting scale parameters from theme
        horz_scale: render_config.horz_scale,
        vert_scale: render_config.vert_scale,
        text_bias: render_config.text_bias,
        gamma_correction: render_config.gamma_correction,
    };

    queue.write_buffer(&resources.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

    // Build vertex buffer with batching by scissor bounds
    let mut vertices: Vec<MsdfVertex> = Vec::new();
    let mut batches: Vec<TextBatch> = Vec::new();

    // Scale factor for logical→physical pixel conversion
    let scale = render_config.scale_factor;

    // Track current batch state
    let mut current_batch_scissor: Option<[u32; 4]> = None;
    let mut batch_vertex_start: u32 = 0;

    // MSDF textures are generated at 32 px/em (use module constant)

    #[cfg(debug_assertions)]
    let mut debug_logged_first = false;

    for text in &extracted.texts {
        // Convert logical bounds to physical pixel scissor rect
        let scissor = [
            (text.bounds.left.max(0) as f32 * scale) as u32,
            (text.bounds.top.max(0) as f32 * scale) as u32,
            ((text.bounds.right - text.bounds.left).max(0) as f32 * scale) as u32,
            ((text.bounds.bottom - text.bounds.top).max(0) as f32 * scale) as u32,
        ];

        // Start new batch if bounds changed
        if current_batch_scissor != Some(scissor) {
            // Finalize previous batch if any
            if let Some(prev_scissor) = current_batch_scissor {
                let vertex_count = vertices.len() as u32 - batch_vertex_start;
                if vertex_count > 0 {
                    batches.push(TextBatch {
                        scissor: prev_scissor,
                        vertex_start: batch_vertex_start,
                        vertex_count,
                    });
                }
            }
            current_batch_scissor = Some(scissor);
            batch_vertex_start = vertices.len() as u32;
        }

        #[cfg(debug_assertions)]
        let mut first_glyph_in_text = true;

        for glyph in &text.glyphs {
            let Some(region) = atlas.regions.get(&glyph.key) else {
                continue;
            };

            let [u0, v0, u1, v1] = region.uv_rect(atlas.width, atlas.height);

            // Scale from MSDF texture pixels to user's font size
            let msdf_scale = glyph.font_size / MSDF_PX_PER_EM;

            // Quad dimensions from atlas region, scaled to font size
            let quad_width = region.width as f32 * msdf_scale;
            let quad_height = region.height as f32 * msdf_scale;

            // Apply anchor offset to position the glyph correctly
            // anchor is in em units (fraction of 1em), multiply by font_size to get pixels
            // SUBTRACT anchor to shift quad left/up so the glyph origin aligns with pen position
            let anchor_x = region.anchor_x * glyph.font_size;
            let anchor_y = region.anchor_y * glyph.font_size;

            let px_x = text.left + (glyph.x - anchor_x) * text.scale;
            let px_y = text.top + (glyph.y - anchor_y) * text.scale;

            // Debug logging for first glyph of first text area
            #[cfg(debug_assertions)]
            if !debug_logged_first && first_glyph_in_text {
                trace!(
                    "MSDF vertex: glyph_id={}, pos=({:.1}, {:.1}), font_size={:.1}, msdf_scale={:.3}, \
                     region={}x{}, quad={:.1}x{:.1}, anchor_em=({:.4}, {:.4}), anchor_px=({:.1}, {:.1}), \
                     text_offset=({:.1}, {:.1}), scale={:.2}, final_px=({:.1}, {:.1})",
                    glyph.key.glyph_id,
                    glyph.x, glyph.y,
                    glyph.font_size,
                    msdf_scale,
                    region.width, region.height,
                    quad_width, quad_height,
                    region.anchor_x, region.anchor_y,
                    anchor_x, anchor_y,
                    text.left, text.top,
                    text.scale,
                    px_x, px_y
                );
                debug_logged_first = true;
                first_glyph_in_text = false;
            }

            let x0 = px_x * 2.0 / resolution[0] - 1.0;
            let y0 = 1.0 - px_y * 2.0 / resolution[1];
            let x1 = x0 + (quad_width * text.scale) * 2.0 / resolution[0];
            let y1 = y0 - (quad_height * text.scale) * 2.0 / resolution[1];

            // Z is flat — depth testing is disabled. Overlap between adjacent
            // glyph quads is resolved by premultiplied alpha blending.
            let z = 0.5;

            // Two triangles for the quad
            // V coordinates are flipped because msdfgen bitmaps have Y=0 at bottom
            let imp = glyph.importance;
            let fx = text.effects_bits;
            vertices.push(MsdfVertex { position: [x0, y0, z], uv: [u0, v1], color: glyph.color, importance: imp, effects: fx });
            vertices.push(MsdfVertex { position: [x1, y0, z], uv: [u1, v1], color: glyph.color, importance: imp, effects: fx });
            vertices.push(MsdfVertex { position: [x0, y1, z], uv: [u0, v0], color: glyph.color, importance: imp, effects: fx });

            vertices.push(MsdfVertex { position: [x1, y0, z], uv: [u1, v1], color: glyph.color, importance: imp, effects: fx });
            vertices.push(MsdfVertex { position: [x1, y1, z], uv: [u1, v0], color: glyph.color, importance: imp, effects: fx });
            vertices.push(MsdfVertex { position: [x0, y1, z], uv: [u0, v0], color: glyph.color, importance: imp, effects: fx });

            // === DEBUG GEOMETRY ===
            // Generate debug visualization when debug mode is 1 or 2
            // (Skip for shader debug modes 3, 4, 5 to not obscure the output)
            #[cfg(debug_assertions)]
            if debug_mode_value > 0 && debug_mode_value < 3 {
                // Pen position from cosmic-text (green dot)
                let pen_x = text.left + glyph.x * text.scale;
                let pen_y = text.top + glyph.y * text.scale;
                debug_dot(&mut vertices, pen_x, pen_y, resolution, debug_colors::GREEN);

                // Anchor point in screen space (blue dot) - shows where glyph origin is in bitmap
                // The anchor is the distance from bitmap origin to glyph origin
                // So anchor position = pen position (conceptually, the anchor moves the quad so they align)
                let anchor_screen_x = pen_x;
                let anchor_screen_y = pen_y;
                debug_dot(&mut vertices, anchor_screen_x, anchor_screen_y + 6.0, resolution, debug_colors::BLUE);

                // Quad top-left corner (yellow dot)
                debug_dot(&mut vertices, px_x, px_y, resolution, debug_colors::YELLOW);

                // Quad outline (red) - only in mode 2
                if debug_mode_value >= 2 {
                    let scaled_quad_width = quad_width * text.scale;
                    let scaled_quad_height = quad_height * text.scale;
                    debug_rect_outline(
                        &mut vertices,
                        px_x, px_y,
                        scaled_quad_width, scaled_quad_height,
                        resolution,
                        debug_colors::RED,
                    );
                }
            }
        }
    }

    // Finalize the last batch
    if let Some(scissor) = current_batch_scissor {
        let vertex_count = vertices.len() as u32 - batch_vertex_start;
        if vertex_count > 0 {
            batches.push(TextBatch {
                scissor,
                vertex_start: batch_vertex_start,
                vertex_count,
            });
        }
    }

    // During resize, scissor bounds may be stale (computed with old resolution).
    // Clear batches to use the full-viewport fallback in the render node.
    // Text still renders correctly, just without per-area clipping for 1 frame.
    // Next frame, bounds are recalculated and normal clipping resumes.
    if render_config.resize_in_progress {
        batches.clear();
    }

    resources.vertex_count = vertices.len() as u32;
    resources.batches = batches;

    if vertices.is_empty() {
        return;
    }

    // Create or update vertex buffer
    let vertex_data = bytemuck::cast_slice(&vertices);
    if resources.vertex_buffer.as_ref().map(|b| b.size() as usize) != Some(vertex_data.len()) {
        resources.vertex_buffer = Some(device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("msdf_vertex_buffer"),
            contents: vertex_data,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        }));
    } else if let Some(buffer) = &resources.vertex_buffer {
        queue.write_buffer(buffer, 0, vertex_data);
    }

    // Track resolution for resize detection in render nodes.
    let width = resolution[0] as u32;
    let height = resolution[1] as u32;
    if resources.texture_size != (width, height) && width > 0 && height > 0 {
        resources.texture_size = (width, height);
        // Mark intermediate texture for recreation at new size
        resources.intermediate_texture = None;
        resources.intermediate_texture_view = None;
    }

    // Create intermediate texture unconditionally — both bloom and TAA read from it.
    // This must be checked every frame because resources may be initialized after
    // the texture size is already tracked.
    if resources.intermediate_texture_view.is_none()
        && width > 0
        && height > 0
    {
        let intermediate_texture = device.create_texture(&TextureDescriptor {
            label: Some("msdf_intermediate_texture"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            // Use the same format as the pipeline (from swap chain)
            format: resources.format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let intermediate_view = intermediate_texture.create_view(&TextureViewDescriptor::default());
        resources.intermediate_texture = Some(intermediate_texture);
        resources.intermediate_texture_view = Some(intermediate_view);
    }

    // Update bind group if atlas texture changed
    if let Some(gpu_image) = images.get(&atlas.texture) {
        resources.bind_group = Some(device.create_bind_group(
            "msdf_bind_group",
            &pipeline.bind_group_layout,
            &BindGroupEntries::sequential((
                resources.uniform_buffer.as_entire_binding(),
                &gpu_image.texture_view,
                &gpu_image.sampler,
            )),
        ));
    }

    // Update TAA bind groups — always needed since text renders to intermediate.
    if let Some(intermediate_view) = &resources.intermediate_texture_view {
        if let Some(taa_res) = &mut taa_resources {
            if taa_state.enabled {
                // === TAA ENABLED ===
                // Swap history textures FIRST (applies last frame's render result)
                if taa_res.frames_accumulated > 0 {
                    taa_res.swap();
                }

                // Get camera motion for reprojection
                let motion = camera_motion.as_ref().map(|m| m.motion_uv).unwrap_or([0.0, 0.0]);

                // Reset accumulation on significant camera motion (prevents ghosting)
                let motion_magnitude = (motion[0] * motion[0] + motion[1] * motion[1]).sqrt();
                if motion_magnitude > 0.001 {
                    taa_res.frames_accumulated = 0;
                }

                // TAA blend uniforms
                let taa_uniforms = TaaUniforms {
                    resolution,
                    frames_accumulated: taa_res.frames_accumulated,
                    taa_enabled: 1,
                    camera_motion: motion,
                    convergence_frames: render_config.taa_convergence_frames as f32,
                    initial_weight: render_config.taa_initial_weight,
                    final_weight: render_config.taa_final_weight,
                    _padding: 0.0,
                };
                queue.write_buffer(&taa_pipeline.uniform_buffer, 0, bytemuck::bytes_of(&taa_uniforms));

                // TAA blend bind group: intermediate + history_read → history_write
                taa_res.bind_group = Some(device.create_bind_group(
                    "msdf_taa_bind_group",
                    &taa_pipeline.bind_group_layout,
                    &BindGroupEntries::sequential((
                        taa_pipeline.uniform_buffer.as_entire_binding(),
                        intermediate_view,
                        taa_res.read_view(),
                        &taa_res.history_sampler,
                    )),
                ));

                // Blit bind group: history_write → ViewTarget (passthrough)
                let blit_uniforms = TaaUniforms {
                    resolution,
                    frames_accumulated: 0,
                    taa_enabled: 0,
                    camera_motion: [0.0, 0.0],
                    convergence_frames: 8.0,
                    initial_weight: 0.5,
                    final_weight: 0.1,
                    _padding: 0.0,
                };
                queue.write_buffer(&taa_pipeline.blit_uniform_buffer, 0, bytemuck::bytes_of(&blit_uniforms));

                taa_res.blit_bind_group = Some(device.create_bind_group(
                    "msdf_taa_blit_bind_group",
                    &taa_pipeline.bind_group_layout,
                    &BindGroupEntries::sequential((
                        taa_pipeline.blit_uniform_buffer.as_entire_binding(),
                        taa_res.write_view(),
                        taa_res.read_view(), // Dummy — passthrough ignores history
                        &taa_res.history_sampler,
                    )),
                ));

                taa_res.frames_accumulated = taa_res.frames_accumulated.saturating_add(1);
            } else {
                // === TAA DISABLED ===
                // Still need blit from intermediate → ViewTarget
                taa_res.bind_group = None; // No TAA blend pass
                taa_res.frames_accumulated = 0;

                let blit_uniforms = TaaUniforms {
                    resolution,
                    frames_accumulated: 0,
                    taa_enabled: 0,
                    camera_motion: [0.0, 0.0],
                    convergence_frames: 8.0,
                    initial_weight: 0.5,
                    final_weight: 0.1,
                    _padding: 0.0,
                };
                queue.write_buffer(&taa_pipeline.blit_uniform_buffer, 0, bytemuck::bytes_of(&blit_uniforms));

                // Blit reads from intermediate directly (not history)
                taa_res.blit_bind_group = Some(device.create_bind_group(
                    "msdf_taa_blit_bind_group",
                    &taa_pipeline.bind_group_layout,
                    &BindGroupEntries::sequential((
                        taa_pipeline.blit_uniform_buffer.as_entire_binding(),
                        intermediate_view,          // Read intermediate directly
                        intermediate_view,          // Dummy — passthrough ignores history
                        &taa_res.history_sampler,
                    )),
                ));
            }
        }
    }
}

/// Initialize MSDF text resources.
///
/// Requires either `ExtractedWindows` (for windowed mode) or `ExtractedMsdfRenderConfig`
/// (for headless/test mode) to determine the surface format.
///
/// In windowed mode, queries `ExtractedWindows` for the primary window's swap chain format.
/// In headless mode, falls back to `ExtractedMsdfRenderConfig.format`.
///
/// Will skip initialization if no format can be determined yet (defers to next frame).
pub fn init_msdf_resources(
    device: Res<RenderDevice>,
    pipeline_res: Res<MsdfTextPipeline>,
    pipeline_cache: Res<PipelineCache>,
    mut commands: Commands,
    extracted_windows: Option<Res<ExtractedWindows>>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
) {
    // Try to get format from ExtractedWindows first (windowed mode)
    // This is the authoritative source for the actual swap chain format
    let format_from_window = extracted_windows
        .as_ref()
        .and_then(|windows| windows.primary)
        .and_then(|entity| extracted_windows.as_ref()?.windows.get(&entity))
        .and_then(|window| window.swap_chain_texture_format);

    // Fall back to ExtractedMsdfRenderConfig (headless/test mode)
    let format_from_config = render_config
        .as_ref()
        .filter(|c| c.initialized)
        .map(|c| c.format);

    // Use window format preferentially, fall back to config format
    let Some(format) = format_from_window.or(format_from_config) else {
        // No format available yet - defer to next frame
        // This is normal on first frame before prepare_windows has run
        return;
    };

    // Create uniform buffer
    let uniform_buffer = device.create_buffer(&BufferDescriptor {
        label: Some("msdf_uniform_buffer"),
        size: std::mem::size_of::<MsdfUniforms>() as u64,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // Create pipeline descriptor
    // MsdfVertex layout: position(12) + uv(8) + color(4) + importance(4) + effects(4) = 32 bytes
    let vertex_layout = VertexBufferLayout {
        array_stride: std::mem::size_of::<MsdfVertex>() as u64,
        step_mode: VertexStepMode::Vertex,
        attributes: vec![
            // Position (x, y, z)
            VertexAttribute {
                format: VertexFormat::Float32x3,
                offset: 0,
                shader_location: 0,
            },
            // UV coordinates
            VertexAttribute {
                format: VertexFormat::Float32x2,
                offset: 12, // 3 * sizeof(f32)
                shader_location: 1,
            },
            // Color (RGBA8)
            VertexAttribute {
                format: VertexFormat::Unorm8x4,
                offset: 20, // 12 + 2 * sizeof(f32)
                shader_location: 2,
            },
            // Importance (semantic weight for text emphasis)
            VertexAttribute {
                format: VertexFormat::Float32,
                offset: 24, // 20 + 4 (color is 4 bytes)
                shader_location: 3,
            },
            // Per-vertex effect flags (bit 0: rainbow)
            VertexAttribute {
                format: VertexFormat::Uint32,
                offset: 28, // 24 + sizeof(f32)
                shader_location: 4,
            },
        ],
    };

    let pipeline_id = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("msdf_text_pipeline".into()),
        layout: vec![pipeline_res.bind_group_layout_descriptor.clone()],
        push_constant_ranges: vec![],
        vertex: VertexState {
            shader: pipeline_res.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("vertex".into()),
            buffers: vec![vertex_layout],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..default()
        },
        // No depth testing — overlap between adjacent glyph quads is handled
        // purely by premultiplied alpha blending. Outside the glyph shape,
        // the SDF evaluates to sd < 0.5, so text_alpha drops to ~0 naturally.
        depth_stencil: None,
        multisample: MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        fragment: Some(FragmentState {
            shader: pipeline_res.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("fragment".into()),
            targets: vec![Some(ColorTargetState {
                format,
                // Premultiplied alpha blending prevents double-blending in overlap regions.
                // When adjacent glyph quads overlap (due to MSDF padding), standard alpha
                // blending compounds their antialiasing alpha, filling gaps that should be empty.
                // With premultiplied alpha (src=ONE), each pixel's contribution is independent.
                blend: Some(BlendState {
                    color: BlendComponent {
                        src_factor: BlendFactor::One,
                        dst_factor: BlendFactor::OneMinusSrcAlpha,
                        operation: BlendOperation::Add,
                    },
                    alpha: BlendComponent {
                        src_factor: BlendFactor::One,
                        dst_factor: BlendFactor::OneMinusSrcAlpha,
                        operation: BlendOperation::Add,
                    },
                }),
                write_mask: ColorWrites::ALL,
            })],
        }),
        zero_initialize_workgroup_memory: false,
    });

    commands.insert_resource(MsdfTextResources {
        pipeline: pipeline_id,
        bind_group_layout: pipeline_res.bind_group_layout.clone(),
        uniform_buffer,
        vertex_buffer: None,
        bind_group: None,
        vertex_count: 0,
        batches: Vec::new(),
        texture_size: (0, 0),
        intermediate_texture: None,
        intermediate_texture_view: None,
        format, // Store the format used for pipeline creation
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test anchor-to-pixel conversion.
    ///
    /// The anchor is stored in em units (fraction of 1em).
    /// To convert to pixels: anchor_px = anchor_em * font_size
    #[test]
    fn anchor_to_pixel_conversion() {
        let anchor_em: f32 = 0.25; // 1/4 em offset
        let font_size: f32 = 16.0; // 16px font
        let anchor_px = anchor_em * font_size;

        assert!((anchor_px - 4.0).abs() < 0.001, "0.25em at 16px should be 4px");

        // Test with larger font
        let font_size_32: f32 = 32.0;
        let anchor_px_32 = anchor_em * font_size_32;
        assert!((anchor_px_32 - 8.0).abs() < 0.001, "0.25em at 32px should be 8px");
    }

    /// Test MSDF scale calculation.
    ///
    /// MSDF textures are generated at 64px/em. When rendering at a different
    /// font size, we scale the atlas region accordingly.
    #[test]
    fn msdf_scale_calculation() {
        // 16px font = quarter the MSDF generation size
        let font_size: f32 = 16.0;
        let scale = font_size / MSDF_PX_PER_EM;
        assert!((scale - 0.25).abs() < 0.001, "16px font should be 0.25x scale");

        // 64px font = same as MSDF generation size
        let font_size_64: f32 = 64.0;
        let scale_64 = font_size_64 / MSDF_PX_PER_EM;
        assert!((scale_64 - 1.0).abs() < 0.001, "64px font should be 1.0x scale");

        // 128px font = double the MSDF generation size
        let font_size_128: f32 = 128.0;
        let scale_128 = font_size_128 / MSDF_PX_PER_EM;
        assert!((scale_128 - 2.0).abs() < 0.001, "128px font should be 2.0x scale");
    }

    /// Test quad size calculation from atlas region.
    ///
    /// The rendered quad size = region_size * msdf_scale
    #[test]
    fn quad_size_from_region() {
        let region_width: f32 = 40.0; // MSDF was generated with 40px wide bitmap
        let msdf_scale: f32 = 0.25; // 16px font at 64px/em
        let quad_width = region_width * msdf_scale;

        assert!((quad_width - 10.0).abs() < 0.001, "40px region at 0.25x scale = 10px quad");

        // At native MSDF size (64px font)
        let msdf_scale_1: f32 = 1.0;
        let quad_width_1 = region_width * msdf_scale_1;
        assert!((quad_width_1 - 40.0).abs() < 0.001, "40px region at 1.0x scale = 40px quad");
    }

    /// Test final pixel position calculation.
    ///
    /// px_x = text.left + (glyph.x - anchor_px) * text.scale
    /// px_y = text.top + (glyph.y - anchor_py) * text.scale
    ///
    /// The anchor represents where the glyph origin is within the MSDF bitmap.
    /// We SUBTRACT the anchor to shift the quad so the origin aligns with pen position.
    #[test]
    fn final_pixel_position() {
        let text_left: f32 = 100.0;
        let text_top: f32 = 50.0;
        let text_scale: f32 = 1.0;
        let glyph_x: f32 = 10.0; // Pen position from layout
        let glyph_y: f32 = 20.0; // Baseline position
        let anchor_x_em: f32 = 0.125; // 1/8 em
        let anchor_y_em: f32 = 0.25; // 1/4 em
        let font_size: f32 = 16.0;

        let anchor_x_px = anchor_x_em * font_size; // = 2.0
        let anchor_y_px = anchor_y_em * font_size; // = 4.0

        // Subtract anchor to shift quad left/up, aligning glyph origin with pen position
        let px_x = text_left + (glyph_x - anchor_x_px) * text_scale;
        let px_y = text_top + (glyph_y - anchor_y_px) * text_scale;

        assert!((px_x - 108.0).abs() < 0.001, "px_x should be 100 + (10 - 2) = 108");
        assert!((px_y - 66.0).abs() < 0.001, "px_y should be 50 + (20 - 4) = 66");
    }

    /// Test NDC (Normalized Device Coordinates) conversion.
    ///
    /// NDC x: px * 2 / width - 1  (maps 0..width to -1..1)
    /// NDC y: 1 - py * 2 / height (maps 0..height to 1..-1, Y flipped)
    #[test]
    fn ndc_conversion() {
        let resolution: [f32; 2] = [1280.0, 720.0];

        // Center of screen
        let px_x: f32 = 640.0;
        let px_y: f32 = 360.0;
        let ndc_x = px_x * 2.0 / resolution[0] - 1.0;
        let ndc_y = 1.0 - px_y * 2.0 / resolution[1];

        assert!(ndc_x.abs() < 0.001, "Center X should be 0.0 NDC");
        assert!(ndc_y.abs() < 0.001, "Center Y should be 0.0 NDC");

        // Top-left corner
        let px_x2: f32 = 0.0;
        let px_y2: f32 = 0.0;
        let ndc_x = px_x2 * 2.0 / resolution[0] - 1.0;
        let ndc_y = 1.0 - px_y2 * 2.0 / resolution[1];

        assert!((ndc_x - (-1.0)).abs() < 0.001, "Top-left X should be -1.0 NDC");
        assert!((ndc_y - 1.0).abs() < 0.001, "Top-left Y should be 1.0 NDC");
    }

    /// Test the complete vertex position calculation chain.
    #[test]
    fn complete_vertex_calculation() {
        // Setup - mirroring the prepare_msdf_texts calculation
        let text_left: f32 = 50.0;
        let text_scale: f32 = 1.0;
        let glyph_x: f32 = 0.0; // First glyph at origin
        let font_size: f32 = 16.0;
        let region_width: u32 = 40;
        let region_anchor_x: f32 = 0.25; // em units (MSDF padding / px_per_em)

        // Calculations (mirroring prepare_msdf_texts)
        let msdf_scale = font_size / MSDF_PX_PER_EM; // 0.25
        let quad_width = region_width as f32 * msdf_scale; // 10.0
        let anchor_x = region_anchor_x * font_size; // 4.0
        // Subtract anchor to align glyph origin with pen position
        let px_x = text_left + (glyph_x - anchor_x) * text_scale; // 50 + (0 - 4) = 46

        assert!((msdf_scale - 0.25).abs() < 0.001);
        assert!((quad_width - 10.0).abs() < 0.001);
        assert!((anchor_x - 4.0).abs() < 0.001);
        assert!((px_x - 46.0).abs() < 0.001);
    }

    // ========================================================================
    // TAA JITTER TESTS
    // ========================================================================

    /// Test that the Halton sequence values are within expected range.
    #[test]
    fn halton_sequence_range() {
        for i in 0..TAA_SAMPLE_COUNT {
            let jitter = get_taa_jitter(i);
            assert!(
                jitter[0] >= -0.5 && jitter[0] <= 0.5,
                "Jitter X at frame {} should be in [-0.5, 0.5], got {}",
                i, jitter[0]
            );
            assert!(
                jitter[1] >= -0.5 && jitter[1] <= 0.5,
                "Jitter Y at frame {} should be in [-0.5, 0.5], got {}",
                i, jitter[1]
            );
        }
    }

    /// Test that the Halton sequence cycles correctly.
    #[test]
    fn halton_sequence_cycles() {
        for i in 0..TAA_SAMPLE_COUNT {
            let jitter1 = get_taa_jitter(i);
            let jitter2 = get_taa_jitter(i + TAA_SAMPLE_COUNT);
            assert_eq!(jitter1, jitter2, "Sequence should cycle after {} samples", TAA_SAMPLE_COUNT);
        }
    }

    /// Test that the Halton sequence provides diverse samples (no duplicates).
    #[test]
    fn halton_sequence_diversity() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..TAA_SAMPLE_COUNT {
            let jitter = get_taa_jitter(i);
            // Convert to string key for HashSet (floats aren't hashable)
            let key = format!("{:.6},{:.6}", jitter[0], jitter[1]);
            assert!(
                seen.insert(key),
                "Halton sequence should have unique samples, duplicate at frame {}",
                i
            );
        }
    }

    /// Test NDC jitter conversion (as done in shader).
    ///
    /// The shader converts pixel jitter to NDC:
    /// jitter_ndc = jitter_px * 2.0 / resolution
    #[test]
    fn jitter_to_ndc_conversion() {
        let resolution = [1280.0_f32, 720.0_f32];

        // Test 0.5 pixel jitter (max Halton value)
        let jitter_px = [0.5_f32, 0.5_f32];
        let jitter_ndc = [
            jitter_px[0] * 2.0 / resolution[0],
            jitter_px[1] * 2.0 / resolution[1],
        ];

        // 0.5 pixels at 1280px resolution = 0.5 * 2 / 1280 ≈ 0.00078125
        assert!(
            (jitter_ndc[0] - 0.00078125).abs() < 0.0001,
            "X jitter NDC should be ~0.00078125, got {}",
            jitter_ndc[0]
        );
        // 0.5 pixels at 720px resolution = 0.5 * 2 / 720 ≈ 0.00139
        assert!(
            (jitter_ndc[1] - 0.00138889).abs() < 0.0001,
            "Y jitter NDC should be ~0.00139, got {}",
            jitter_ndc[1]
        );
    }

    /// Test TAA sample count is 8 (matching Bevy TAA).
    #[test]
    fn taa_sample_count_is_8() {
        assert_eq!(TAA_SAMPLE_COUNT, 8, "TAA should use 8 samples like Bevy TAA");
    }
}
