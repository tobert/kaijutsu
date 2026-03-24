//! MSDF per-block texture renderer.
//!
//! Renders MSDF glyph quads to per-block textures via custom wgpu pipeline.
//! Each block gets its own render pass, targeting the block's GpuImage texture.

use bevy::mesh::VertexBufferLayout;
use bevy::prelude::*;
use bevy::render::{
    render_asset::RenderAssets,
    render_resource::{
        binding_types::{sampler as sampler_binding, texture_2d, uniform_buffer},
        *,
    },
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};
use bytemuck::{Pod, Zeroable};

use super::atlas::{AtlasRegion, MsdfAtlas};
use super::glyph::PositionedGlyph;

/// MSDF textures are generated at 64 pixels per em.
pub const MSDF_PX_PER_EM: f32 = 64.0;

/// GPU vertex for MSDF text rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct MsdfVertex {
    /// Position in NDC [-1, 1].
    pub position: [f32; 2],
    /// UV coordinates in atlas.
    pub uv: [f32; 2],
    /// Color (RGBA8).
    pub color: [u8; 4],
    /// Semantic importance (0.0 = faded, 0.5 = normal, 1.0 = bold).
    pub importance: f32,
}

/// GPU uniforms for per-block MSDF rendering.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, ShaderType)]
pub struct MsdfBlockUniforms {
    /// Block texture resolution (physical pixels).
    pub resolution: [f32; 2],
    /// MSDF range in atlas pixels.
    pub msdf_range: f32,
    /// Time for animations.
    pub time: f32,
    /// SDF texel size (1/atlas_width, 1/atlas_height) for gradient sampling.
    pub sdf_texel: [f32; 2],
    /// Hinting strength (0.0 = off, 1.0 = full). Default 0.8.
    pub hint_amount: f32,
    /// Stem darkening strength (0.0 = off, ~0.15 = ClearType-like). Default 0.15.
    pub stem_darkening: f32,
    /// Horizontal stroke AA scale (1.0-1.3). Default 1.1.
    pub horz_scale: f32,
    /// Vertical stroke AA scale (0.5-0.8). Default 0.6.
    pub vert_scale: f32,
    /// SDF threshold for text rendering (0.45-0.55). Default 0.5.
    pub text_bias: f32,
    /// Gamma correction for alpha. Default 0.85.
    pub gamma_correction: f32,
}

impl Default for MsdfBlockUniforms {
    fn default() -> Self {
        Self {
            resolution: [1.0, 1.0],
            msdf_range: 4.0,
            time: 0.0,
            sdf_texel: [1.0 / 1024.0, 1.0 / 1024.0],
            hint_amount: 0.8,
            stem_darkening: 0.15,
            horz_scale: 1.1,
            vert_scale: 0.6,
            text_bias: 0.5,
            gamma_correction: 0.85,
        }
    }
}

/// Render-world resource for the MSDF per-block pipeline.
#[derive(Resource)]
pub struct MsdfBlockRenderer {
    /// Cached render pipeline ID.
    pub pipeline: CachedRenderPipelineId,
    /// Bind group layout for the MSDF pipeline.
    pub bind_group_layout: BindGroupLayout,
    /// Linear sampler for atlas texture sampling.
    pub sampler: Sampler,
}

impl MsdfBlockRenderer {
    /// Initialize the MSDF renderer in the render world.
    pub fn init(
        device: &RenderDevice,
        pipeline_cache: &PipelineCache,
        asset_server: &AssetServer,
    ) -> Self {
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("msdf_atlas_sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Nearest,
            ..default()
        });

        let layout_entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                uniform_buffer::<MsdfBlockUniforms>(false),
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler_binding(SamplerBindingType::Filtering),
            ),
        );

        let bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "msdf_block_bind_group_layout",
            &layout_entries,
        );

        let bind_group_layout = device.create_bind_group_layout(
            "msdf_block_bind_group_layout",
            &layout_entries,
        );

        let shader = asset_server.load("shaders/msdf_block.wgsl");

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
                VertexAttribute {
                    format: VertexFormat::Float32,
                    offset: 20,
                    shader_location: 3,
                },
            ],
        };

        let pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("msdf_block_pipeline".into()),
            layout: vec![bind_group_layout_descriptor],
            vertex: VertexState {
                shader: shader.clone(),
                entry_point: Some("vertex".into()),
                shader_defs: vec![],
                buffers: vec![vertex_layout],
            },
            fragment: Some(FragmentState {
                shader,
                entry_point: Some("fragment".into()),
                shader_defs: vec![],
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::Rgba8Unorm,
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
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                ..default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            push_constant_ranges: vec![],
            zero_initialize_workgroup_memory: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
        }
    }

    /// Build a vertex buffer for a single block's MSDF glyphs.
    ///
    /// Converts positioned glyphs to NDC quads using atlas region data.
    /// `tex_width`/`tex_height` are in logical (pre-scale) pixels matching
    /// the coordinate space of `PositionedGlyph`.
    pub fn build_vertices(
        glyphs: &[PositionedGlyph],
        atlas: &ExtractedMsdfAtlas,
        tex_width: f32,
        tex_height: f32,
    ) -> Vec<MsdfVertex> {
        let mut vertices = Vec::with_capacity(glyphs.len() * 6);

        for glyph in glyphs {
            let Some(region) = atlas.regions.get(&glyph.key) else {
                continue;
            };

            let [u0, v0, u1, v1] = region.uv_rect(atlas.width, atlas.height);

            // Compute quad size in block-local pixels
            let scale = glyph.font_size / MSDF_PX_PER_EM;
            let quad_w = region.width as f32 * scale;
            let quad_h = region.height as f32 * scale;

            // Quad position: pen position - anchor offset (anchor is in em units).
            // anchor_x/y is the distance from bitmap origin to glyph origin in ems.
            let qx = glyph.x - region.anchor_x * glyph.font_size;
            let qy = glyph.y - region.anchor_y * glyph.font_size;

            // Convert to NDC [-1, 1]
            let x0 = qx / tex_width * 2.0 - 1.0;
            let y0 = 1.0 - qy / tex_height * 2.0;
            let x1 = (qx + quad_w) / tex_width * 2.0 - 1.0;
            let y1 = 1.0 - (qy + quad_h) / tex_height * 2.0;

            let color = glyph.color;
            let importance = glyph.importance;

            // Two triangles per quad
            vertices.push(MsdfVertex { position: [x0, y0], uv: [u0, v0], color, importance });
            vertices.push(MsdfVertex { position: [x1, y0], uv: [u1, v0], color, importance });
            vertices.push(MsdfVertex { position: [x0, y1], uv: [u0, v1], color, importance });
            vertices.push(MsdfVertex { position: [x1, y0], uv: [u1, v0], color, importance });
            vertices.push(MsdfVertex { position: [x1, y1], uv: [u1, v1], color, importance });
            vertices.push(MsdfVertex { position: [x0, y1], uv: [u0, v1], color, importance });
        }

        vertices
    }

    /// Render MSDF glyphs to a block texture via a render pass.
    pub fn render_to_texture(
        &self,
        device: &RenderDevice,
        queue: &RenderQueue,
        pipeline_cache: &PipelineCache,
        gpu_images: &RenderAssets<GpuImage>,
        atlas_image: &Handle<Image>,
        target_image: &Handle<Image>,
        vertices: &[MsdfVertex],
        uniforms: &MsdfBlockUniforms,
        clear: bool,
    ) -> bool {
        if vertices.is_empty() {
            return false;
        }

        let Some(pipeline) = pipeline_cache.get_render_pipeline(self.pipeline) else {
            return false; // Pipeline still compiling
        };

        let Some(target_gpu) = gpu_images.get(target_image) else {
            return false;
        };

        let Some(atlas_gpu) = gpu_images.get(atlas_image) else {
            return false;
        };

        // Create uniform buffer
        let uniform_buffer = device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("msdf_block_uniforms"),
            contents: bytemuck::bytes_of(uniforms),
            usage: BufferUsages::UNIFORM,
        });

        // Create bind group
        let bind_group = device.create_bind_group(
            "msdf_block_bind_group",
            &self.bind_group_layout,
            &BindGroupEntries::sequential((
                uniform_buffer.as_entire_binding(),
                &atlas_gpu.texture_view,
                &self.sampler,
            )),
        );

        // Create vertex buffer
        let vertex_buffer = device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("msdf_block_vertices"),
            contents: bytemuck::cast_slice(vertices),
            usage: BufferUsages::VERTEX,
        });

        // Record render pass
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("msdf_block_encoder"),
        });

        {
            let load_op = if clear {
                LoadOp::Clear(Default::default())
            } else {
                LoadOp::Load
            };

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("msdf_block_render_pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &target_gpu.texture_view,
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
            });

            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.set_vertex_buffer(0, *vertex_buffer.slice(..));
            render_pass.draw(0..vertices.len() as u32, 0..1);
        }

        queue.submit([encoder.finish()]);
        true
    }
}

/// Extracted atlas data available in the render world.
#[derive(Resource)]
pub struct ExtractedMsdfAtlas {
    pub regions: std::collections::HashMap<super::glyph::GlyphKey, AtlasRegion>,
    pub texture: Handle<Image>,
    pub width: u32,
    pub height: u32,
    pub msdf_range: f32,
}

impl Default for ExtractedMsdfAtlas {
    fn default() -> Self {
        Self {
            regions: std::collections::HashMap::new(),
            texture: Handle::default(),
            width: 1024,
            height: 1024,
            msdf_range: MsdfAtlas::DEFAULT_RANGE,
        }
    }
}
