//! Flat-colored triangle renderer for music notation geometry (staff
//! lines, barlines, stems, ledgers, beams, slurs, ties, repeat dots).
//!
//! Sibling to `renderer::MsdfBlockRenderer`, deliberately much simpler: no
//! atlas texture, no UV, no per-fragment SDF sampling — just a position +
//! color triangle list. Renders into the SAME per-block texture as the MSDF
//! glyph pass, sequenced to run FIRST (see `view::block_render`'s
//! `render_msdf_block_textures`, which now composites geometry → glyphs
//! into one texture per block), so music glyphs land on top of staff lines
//! and beams exactly as they did when vello drew the geometry layer.

use bevy::prelude::*;
use bevy::render::{
    render_asset::RenderAssets,
    render_resource::*,
    renderer::RenderDevice,
    texture::GpuImage,
};
use bytemuck::{Pod, Zeroable};

use super::geometry::GeometryVertex;

/// GPU vertex for flat-colored music geometry.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct GeomGpuVertex {
    /// Position in NDC [-1, 1].
    pub position: [f32; 2],
    /// Color (RGBA8, straight — premultiplied in the fragment shader).
    pub color: [u8; 4],
}

/// Render-world resource for the music-geometry pipeline.
#[derive(Resource)]
pub struct MusicGeometryRenderer {
    pub pipeline: CachedRenderPipelineId,
}

impl MusicGeometryRenderer {
    /// Initialize the geometry renderer in the render world. No bind group
    /// layout at all — this pipeline samples no texture and reads no
    /// uniforms, only the vertex buffer.
    pub fn init(pipeline_cache: &PipelineCache, asset_server: &AssetServer) -> Self {
        let shader = asset_server.load("shaders/music_geometry.wgsl");

        let vertex_layout = bevy::mesh::VertexBufferLayout {
            array_stride: std::mem::size_of::<GeomGpuVertex>() as u64,
            step_mode: VertexStepMode::Vertex,
            attributes: vec![
                VertexAttribute {
                    format: VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                VertexAttribute {
                    format: VertexFormat::Unorm8x4,
                    offset: 8,
                    shader_location: 1,
                },
            ],
        };

        let pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("music_geometry_pipeline".into()),
            layout: vec![],
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
            immediate_size: 0,
            zero_initialize_workgroup_memory: false,
        });

        Self { pipeline }
    }

    /// Convert block-local logical-pixel `GeometryVertex`es to physical NDC.
    /// Mirrors `MsdfBlockRenderer::build_vertices`'s logical→physical
    /// conversion exactly (same `tex_*_phys`/`scale_factor` contract) —
    /// geometry and glyphs must land in the same pixel grid since they
    /// composite into the same texture.
    pub fn build_vertices(
        geometry: &[GeometryVertex],
        tex_width_phys: f32,
        tex_height_phys: f32,
        scale_factor: f32,
    ) -> Vec<GeomGpuVertex> {
        geometry
            .iter()
            .map(|v| {
                let x_phys = v.x * scale_factor;
                let y_phys = v.y * scale_factor;
                let x_ndc = x_phys / tex_width_phys * 2.0 - 1.0;
                let y_ndc = 1.0 - y_phys / tex_height_phys * 2.0;
                GeomGpuVertex {
                    position: [x_ndc, y_ndc],
                    color: v.color,
                }
            })
            .collect()
    }

    /// Encode a geometry render pass for a block texture into a shared
    /// `encoder` (no submit — batched with the caller's other MSDF work).
    /// `clear` mirrors `MsdfBlockRenderer::encode_render`'s convention:
    /// `true` when nothing has drawn to this texture yet this frame.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_render(
        &self,
        device: &RenderDevice,
        encoder: &mut CommandEncoder,
        pipeline_cache: &PipelineCache,
        gpu_images: &RenderAssets<GpuImage>,
        target_image: &Handle<Image>,
        vertices: &[GeomGpuVertex],
        clear: bool,
    ) -> bool {
        if vertices.is_empty() {
            return false;
        }

        let Some(pipeline) = pipeline_cache.get_render_pipeline(self.pipeline) else {
            warn_once!(
                "music geometry pipeline not ready (state: {:?})",
                pipeline_cache.get_render_pipeline_state(self.pipeline)
            );
            return false;
        };

        let Some(target_gpu) = gpu_images.get(target_image) else {
            warn_once!("music geometry: target GpuImage not ready");
            return false;
        };

        let vertex_buffer = device.create_buffer_with_data(&BufferInitDescriptor {
            label: Some("music_geometry_vertices"),
            contents: bytemuck::cast_slice(vertices),
            usage: BufferUsages::VERTEX,
        });

        {
            let load_op = if clear {
                LoadOp::Clear(Default::default())
            } else {
                LoadOp::Load
            };

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("music_geometry_render_pass"),
                multiview_mask: None,
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
            render_pass.set_vertex_buffer(0, *vertex_buffer.slice(..));
            render_pass.draw(0..vertices.len() as u32, 0..1);
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_vertices_maps_top_left_block_origin_to_ndc_top_left() {
        let geom = [GeometryVertex { x: 0.0, y: 0.0, color: [1, 2, 3, 4] }];
        let verts = MusicGeometryRenderer::build_vertices(&geom, 200.0, 100.0, 1.0);
        assert_eq!(verts.len(), 1);
        assert!((verts[0].position[0] - -1.0).abs() < 1e-6);
        assert!((verts[0].position[1] - 1.0).abs() < 1e-6);
        assert_eq!(verts[0].color, [1, 2, 3, 4]);
    }

    #[test]
    fn build_vertices_maps_bottom_right_block_extent_to_ndc_bottom_right() {
        // Physical texture is tex_*_phys; a block-local point at the
        // texture's logical extent (accounting for scale_factor) must land
        // exactly at NDC (1, -1).
        let scale = 2.0;
        let tex_w = 400.0;
        let tex_h = 300.0;
        let geom = [GeometryVertex {
            x: tex_w / scale,
            y: tex_h / scale,
            color: [0; 4],
        }];
        let verts = MusicGeometryRenderer::build_vertices(&geom, tex_w, tex_h, scale);
        assert!((verts[0].position[0] - 1.0).abs() < 1e-5);
        assert!((verts[0].position[1] - -1.0).abs() < 1e-5);
    }

    #[test]
    fn build_vertices_scales_by_scale_factor() {
        let geom = [GeometryVertex { x: 10.0, y: 10.0, color: [0; 4] }];
        let verts_1x = MusicGeometryRenderer::build_vertices(&geom, 100.0, 100.0, 1.0);
        let verts_2x = MusicGeometryRenderer::build_vertices(&geom, 200.0, 200.0, 2.0);
        // Same logical position, texture doubled along with scale_factor —
        // NDC should be identical.
        assert!((verts_1x[0].position[0] - verts_2x[0].position[0]).abs() < 1e-6);
        assert!((verts_1x[0].position[1] - verts_2x[0].position[1]).abs() < 1e-6);
    }
}
