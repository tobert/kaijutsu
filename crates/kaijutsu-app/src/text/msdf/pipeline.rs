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
    view::ViewTarget,
    Extract,
};
use bevy::render::render_resource::binding_types::{sampler, texture_2d, uniform_buffer};
use bytemuck::{Pod, Zeroable};

use super::atlas::MsdfAtlas;
use super::buffer::{MsdfTextAreaConfig, MsdfTextBuffer, PositionedGlyph, TextBounds};
use super::{MsdfText, SdfTextEffects};

/// MSDF textures are generated at 32 pixels per em.
pub const MSDF_PX_PER_EM: f32 = 32.0;

/// Label for the MSDF text render node.
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct MsdfTextRenderNodeLabel;

/// GPU vertex for MSDF text rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MsdfVertex {
    /// Position in screen space.
    pub position: [f32; 2],
    /// UV coordinates in atlas.
    pub uv: [f32; 2],
    /// Color (RGBA8).
    pub color: [u8; 4],
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
    /// Padding for alignment.
    _padding: f32,
    /// Glow color.
    pub glow_color: [f32; 4],
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
            _padding: 0.0,
            glow_color: [0.4, 0.6, 1.0, 0.5],
        }
    }
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
                depth_stencil_attachment: None,
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
) {
    // Extract atlas data for render world
    commands.insert_resource(ExtractedMsdfAtlas {
        regions: atlas.regions.clone(),
        texture: atlas.texture.clone(),
        width: atlas.width,
        height: atlas.height,
        msdf_range: atlas.msdf_range,
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

/// Prepare MSDF text resources for rendering.
pub fn prepare_msdf_texts(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    pipeline: Res<MsdfTextPipeline>,
    extracted: Option<Res<ExtractedMsdfTexts>>,
    atlas: Option<Res<ExtractedMsdfAtlas>>,
    images: Res<bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>>,
    time: Res<Time>,
    mut resources: ResMut<MsdfTextResources>,
    windows: Res<bevy::render::view::ExtractedWindows>,
) {
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

    // Get viewport resolution
    let resolution = windows
        .windows
        .values()
        .next()
        .map(|w| [w.physical_width as f32, w.physical_height as f32])
        .unwrap_or([1280.0, 720.0]);

    // Determine effects from first text (simplified - could be per-text)
    let effects = extracted.texts.first().map(|t| &t.effects);
    let rainbow = effects.map(|e| e.rainbow).unwrap_or(false);
    let glow = effects.and_then(|e| e.glow.as_ref());

    // Update uniforms
    let uniforms = MsdfUniforms {
        resolution,
        msdf_range: atlas.msdf_range,
        time: time.elapsed_secs(),
        rainbow: if rainbow { 1 } else { 0 },
        glow_intensity: glow.map(|g| g.intensity).unwrap_or(0.0),
        glow_spread: glow.map(|g| g.spread).unwrap_or(2.0),
        _padding: 0.0,
        glow_color: glow
            .map(|g| {
                let c = g.color.to_linear();
                [c.red, c.green, c.blue, c.alpha]
            })
            .unwrap_or([0.4, 0.6, 1.0, 0.5]),
    };

    queue.write_buffer(&resources.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

    // Build vertex buffer
    let mut vertices: Vec<MsdfVertex> = Vec::new();

    // MSDF textures are generated at 32 px/em (use module constant)

    #[cfg(debug_assertions)]
    let mut debug_logged_first = false;

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

            // Two triangles for the quad
            // V coordinates are flipped because msdfgen bitmaps have Y=0 at bottom
            vertices.push(MsdfVertex { position: [x0, y0], uv: [u0, v1], color: glyph.color });
            vertices.push(MsdfVertex { position: [x1, y0], uv: [u1, v1], color: glyph.color });
            vertices.push(MsdfVertex { position: [x0, y1], uv: [u0, v0], color: glyph.color });

            vertices.push(MsdfVertex { position: [x1, y0], uv: [u1, v1], color: glyph.color });
            vertices.push(MsdfVertex { position: [x1, y1], uv: [u1, v0], color: glyph.color });
            vertices.push(MsdfVertex { position: [x0, y1], uv: [u0, v0], color: glyph.color });
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
pub fn init_msdf_resources(
    device: Res<RenderDevice>,
    pipeline_res: Res<MsdfTextPipeline>,
    pipeline_cache: Res<PipelineCache>,
    mut commands: Commands,
    windows: Res<bevy::render::view::ExtractedWindows>,
) {
    // Get surface format
    let Some(format) = windows
        .windows
        .values()
        .next()
        .and_then(|w| w.swap_chain_texture_format)
    else {
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
    let vertex_layout = VertexBufferLayout {
        array_stride: std::mem::size_of::<MsdfVertex>() as u64,
        step_mode: VertexStepMode::Vertex,
        attributes: vec![
            VertexAttribute {
                format: VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            VertexAttribute {
                format: VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
            VertexAttribute {
                format: VertexFormat::Unorm8x4,
                offset: 16,
                shader_location: 2,
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
                blend: Some(BlendState::ALPHA_BLENDING),
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
}
