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

/// MSDF textures are generated at 32 pixels per em.
pub const MSDF_PX_PER_EM: f32 = 32.0;

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
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: DEBUG_IMPORTANCE });
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
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });

    // Bottom edge
    let [x0, y0] = to_ndc(x, y + height - LINE_WIDTH);
    let [x1, y1] = to_ndc(x + width, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });

    // Left edge
    let [x0, y0] = to_ndc(x, y);
    let [x1, y1] = to_ndc(x + LINE_WIDTH, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });

    // Right edge
    let [x0, y0] = to_ndc(x + width - LINE_WIDTH, y);
    let [x1, y1] = to_ndc(x + width, y + height);
    vertices.push(MsdfVertex { position: [x0, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y0, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x1, y1, DEBUG_Z], uv, color, importance: imp });
    vertices.push(MsdfVertex { position: [x0, y1, DEBUG_Z], uv, color, importance: imp });
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
}

/// GPU uniform for MSDF rendering.
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
    /// Glow intensity (0 = off).
    pub glow_intensity: f32,
    /// Glow spread in pixels.
    pub glow_spread: f32,
    /// Debug mode: 0=off, 1=dots, 2=dots+quads.
    pub debug_mode: u32,
    /// Glow color.
    pub glow_color: [f32; 4],
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
}

impl Default for MsdfUniforms {
    fn default() -> Self {
        Self {
            resolution: [1280.0, 720.0],
            msdf_range: 8.0,
            time: 0.0,
            rainbow: 0,
            glow_intensity: 0.0,
            glow_spread: 2.0,
            debug_mode: 0,
            glow_color: [0.4, 0.6, 1.0, 0.5],
            sdf_texel: [1.0 / 1024.0, 1.0 / 1024.0], // Default atlas size
            hint_amount: 0.8, // Enable hinting by default (80% strength)
            // Stem darkening: 0.15 = ClearType-like weight for 12-16px text
            // Higher values (0.2-0.3) for heavier weight, 0.0 to disable
            stem_darkening: 0.15,
            jitter_offset: [0.0, 0.0],
            taa_frame_index: 0,
            taa_enabled: 1, // Enable TAA jitter by default
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
/// Extracted from `MsdfRenderConfig` in the main world.
/// The pipeline will not render if this is not present or not initialized.
#[derive(Resource, Clone, Copy)]
pub struct ExtractedMsdfRenderConfig {
    /// Viewport resolution in physical pixels.
    pub resolution: [f32; 2],
    /// Texture format for the render target.
    pub format: TextureFormat,
    /// Whether this config is valid for rendering.
    pub initialized: bool,
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
    /// Depth texture for per-glyph depth testing.
    pub depth_texture: Option<Texture>,
    pub depth_texture_view: Option<TextureView>,
    /// Cached resolution for depth texture recreation.
    pub depth_texture_size: (u32, u32),
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

        // Require depth texture for proper overlap handling
        let Some(depth_texture_view) = &resources.depth_texture_view else {
            return Ok(());
        };

        let out_texture = view_target.out_texture();

        let mut render_pass = render_context.command_encoder().begin_render_pass(
            &RenderPassDescriptor {
                label: Some("msdf_text_render_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: out_texture,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                // Use depth buffer for per-glyph depth testing
                depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                    view: depth_texture_view,
                    depth_ops: Some(Operations {
                        // Clear depth buffer at start of each frame
                        load: LoadOp::Clear(1.0),
                        store: StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            },
        );

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_vertex_buffer(0, *vertex_buffer.slice(..));
        render_pass.draw(0..resources.vertex_count, 0..1);

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

        texts.push(ExtractedMsdfText {
            entity,
            glyphs: buffer.glyphs().to_vec(),
            left: config.left,
            top: config.top,
            scale: config.scale,
            bounds: config.bounds,
            effects: effects.cloned().unwrap_or_default(),
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
            raw_text: Some(ui_text.text.clone()),
            color: ui_text.color,
        });
    }

    commands.insert_resource(ExtractedMsdfTexts { texts });
}

/// Extract render configuration from main world.
///
/// This extracts `MsdfRenderConfig` so the render world has explicit access
/// to resolution and format. If the config is not initialized, rendering will be skipped.
pub fn extract_msdf_render_config(
    mut commands: Commands,
    config: Extract<Res<MsdfRenderConfig>>,
) {
    commands.insert_resource(ExtractedMsdfRenderConfig {
        resolution: config.resolution,
        format: config.format,
        initialized: config.initialized,
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

/// Prepare MSDF text resources for rendering.
///
/// Requires `ExtractedMsdfRenderConfig` to be present and initialized.
/// Will skip rendering if the config is not ready.
pub fn prepare_msdf_texts(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    pipeline: Res<MsdfTextPipeline>,
    extracted: Option<Res<ExtractedMsdfTexts>>,
    atlas: Option<Res<ExtractedMsdfAtlas>>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
    images: Res<bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>>,
    time: Res<Time>,
    mut resources: ResMut<MsdfTextResources>,
    mut taa_state: ResMut<MsdfTextTaaState>,
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

    // Determine effects from first text (simplified - could be per-text)
    let effects = extracted.texts.first().map(|t| &t.effects);
    let rainbow = effects.map(|e| e.rainbow).unwrap_or(false);
    let glow = effects.and_then(|e| e.glow.as_ref());

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
    // Get jitter offset from Halton sequence and advance frame counter
    let jitter_offset = if taa_state.enabled {
        get_taa_jitter(taa_state.frame_index)
    } else {
        [0.0, 0.0]
    };
    let taa_frame_index = taa_state.frame_index;
    // Advance frame counter for next frame (cycles through TAA_SAMPLE_COUNT)
    taa_state.frame_index = (taa_state.frame_index + 1) % TAA_SAMPLE_COUNT;

    // Update uniforms
    let uniforms = MsdfUniforms {
        resolution,
        msdf_range: atlas.msdf_range,
        time: time.elapsed_secs(),
        rainbow: if rainbow { 1 } else { 0 },
        glow_intensity: glow.map(|g| g.intensity).unwrap_or(0.0),
        glow_spread: glow.map(|g| g.spread).unwrap_or(2.0),
        debug_mode: debug_mode_value,
        glow_color: glow
            .map(|g| {
                let c = g.color.to_linear();
                [c.red, c.green, c.blue, c.alpha]
            })
            .unwrap_or([0.4, 0.6, 1.0, 0.5]),
        sdf_texel,
        hint_amount: 0.8, // 80% hinting strength - can be made configurable
        // Stem darkening: 0.15 = ClearType-like weight for 12-16px text
        stem_darkening: 0.15,
        // TAA jitter for temporal super-resolution
        jitter_offset,
        taa_frame_index,
        taa_enabled: if taa_state.enabled { 1 } else { 0 },
    };

    queue.write_buffer(&resources.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

    // Build vertex buffer
    let mut vertices: Vec<MsdfVertex> = Vec::new();

    // MSDF textures are generated at 32 px/em (use module constant)

    #[cfg(debug_assertions)]
    let mut debug_logged_first = false;

    // Track glyph index for z-depth assignment.
    // Each glyph gets a unique z-value so depth testing resolves overlaps.
    // Earlier glyphs (lower index) have lower z and "win" in overlap regions.
    let mut glyph_index: u32 = 0;

    for text in &extracted.texts {
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

            // Z-depth: each glyph gets a unique depth value.
            // Using small increments ensures depth buffer precision is sufficient.
            // Earlier glyphs have lower z values, so they "win" in depth test (Less).
            let z = glyph_index as f32 * 0.0001;
            glyph_index += 1;

            // Two triangles for the quad
            // V coordinates are flipped because msdfgen bitmaps have Y=0 at bottom
            let imp = glyph.importance;
            vertices.push(MsdfVertex { position: [x0, y0, z], uv: [u0, v1], color: glyph.color, importance: imp });
            vertices.push(MsdfVertex { position: [x1, y0, z], uv: [u1, v1], color: glyph.color, importance: imp });
            vertices.push(MsdfVertex { position: [x0, y1, z], uv: [u0, v0], color: glyph.color, importance: imp });

            vertices.push(MsdfVertex { position: [x1, y0, z], uv: [u1, v1], color: glyph.color, importance: imp });
            vertices.push(MsdfVertex { position: [x1, y1, z], uv: [u1, v0], color: glyph.color, importance: imp });
            vertices.push(MsdfVertex { position: [x0, y1, z], uv: [u0, v0], color: glyph.color, importance: imp });

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

    resources.vertex_count = vertices.len() as u32;

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

    // Create or update depth texture if resolution changed
    let width = resolution[0] as u32;
    let height = resolution[1] as u32;
    if resources.depth_texture_size != (width, height) && width > 0 && height > 0 {
        let depth_texture = device.create_texture(&TextureDescriptor {
            label: Some("msdf_depth_texture"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Depth32Float,
            usage: TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_texture_view = depth_texture.create_view(&TextureViewDescriptor::default());
        resources.depth_texture = Some(depth_texture);
        resources.depth_texture_view = Some(depth_texture_view);
        resources.depth_texture_size = (width, height);
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
    // MsdfVertex layout: position(12) + uv(8) + color(4) + importance(4) = 28 bytes
    // Note: color is 4 bytes (u8x4), importance is 4 bytes (f32)
    let vertex_layout = VertexBufferLayout {
        array_stride: std::mem::size_of::<MsdfVertex>() as u64,
        step_mode: VertexStepMode::Vertex,
        attributes: vec![
            // Position (x, y, z) - z is used for depth testing
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
        // Enable depth testing so each glyph "wins" in overlapping regions.
        // Glyphs rendered first (lower z) take precedence, preventing double-blend.
        depth_stencil: Some(DepthStencilState {
            format: TextureFormat::Depth32Float,
            depth_write_enabled: true,
            depth_compare: CompareFunction::Less,
            stencil: StencilState::default(),
            bias: DepthBiasState::default(),
        }),
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
        depth_texture: None,
        depth_texture_view: None,
        depth_texture_size: (0, 0),
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
    /// MSDF textures are generated at 32px/em. When rendering at a different
    /// font size, we scale the atlas region accordingly.
    #[test]
    fn msdf_scale_calculation() {
        // 16px font = half the MSDF generation size
        let font_size: f32 = 16.0;
        let scale = font_size / MSDF_PX_PER_EM;
        assert!((scale - 0.5).abs() < 0.001, "16px font should be 0.5x scale");

        // 32px font = same as MSDF generation size
        let font_size_32: f32 = 32.0;
        let scale_32 = font_size_32 / MSDF_PX_PER_EM;
        assert!((scale_32 - 1.0).abs() < 0.001, "32px font should be 1.0x scale");

        // 64px font = double the MSDF generation size
        let font_size_64: f32 = 64.0;
        let scale_64 = font_size_64 / MSDF_PX_PER_EM;
        assert!((scale_64 - 2.0).abs() < 0.001, "64px font should be 2.0x scale");
    }

    /// Test quad size calculation from atlas region.
    ///
    /// The rendered quad size = region_size * msdf_scale
    #[test]
    fn quad_size_from_region() {
        let region_width: f32 = 40.0; // MSDF was generated with 40px wide bitmap
        let msdf_scale: f32 = 0.5; // 16px font
        let quad_width = region_width * msdf_scale;

        assert!((quad_width - 20.0).abs() < 0.001, "40px region at 0.5x scale = 20px quad");

        // At native MSDF size
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
        let msdf_scale = font_size / MSDF_PX_PER_EM; // 0.5
        let quad_width = region_width as f32 * msdf_scale; // 20.0
        let anchor_x = region_anchor_x * font_size; // 4.0
        // Subtract anchor to align glyph origin with pen position
        let px_x = text_left + (glyph_x - anchor_x) * text_scale; // 50 + (0 - 4) = 46

        assert!((msdf_scale - 0.5).abs() < 0.001);
        assert!((quad_width - 20.0).abs() < 0.001);
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
