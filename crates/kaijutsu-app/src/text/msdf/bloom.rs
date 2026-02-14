//! Post-process bloom for MSDF text glow.
//!
//! Renders a Gaussian-blurred glow behind sharp text by operating on the
//! intermediate texture that `MsdfTextRenderNode` writes to.
//!
//! Pipeline:
//! ```text
//! MsdfTextRenderNode → intermediate texture
//!     ↓
//! MsdfBloomNode (4 sub-passes):
//!   1. Extract: intermediate alpha → half-res glow_source (tinted with glow_color)
//!   2. Blur H:  glow_source → glow_temp (9-tap separable Gaussian)
//!   3. Blur V:  glow_temp → glow_source
//!   4. Composite: glow_source → intermediate ("behind" blend)
//!     ↓
//! MsdfTextTaaNode → ViewTarget
//! ```
//!
//! When `glow_intensity == 0.0`, the bloom node early-returns (no GPU cost).

use bevy::prelude::*;
use bevy::render::{
    render_graph::{NodeRunError, RenderGraphContext, RenderLabel, ViewNode},
    render_resource::{
        binding_types::{sampler, texture_2d, uniform_buffer},
        *,
    },
    renderer::{RenderContext, RenderDevice, RenderQueue},
    view::ViewTarget,
};
use bytemuck::{Pod, Zeroable};

use super::pipeline::{ExtractedMsdfRenderConfig, MsdfTextResources};

/// Label for the MSDF bloom render node.
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct MsdfBloomNodeLabel;

/// GPU uniform for bloom passes.
///
/// Field order matches WGSL alignment: vec4 first, then vec2 fields together,
/// then f32 scalars with padding. This avoids alignment mismatches between
/// WGSL (vec2 align 8) and Rust ([f32; 2] align 4).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, ShaderType)]
pub struct BloomUniforms {
    /// Glow color (RGB) and alpha.
    pub glow_color: [f32; 4],
    /// Blur direction: (1,0) for horizontal, (0,1) for vertical.
    pub blur_direction: [f32; 2],
    /// Texel size (1/width, 1/height) of the texture being sampled.
    pub texel_size: [f32; 2],
    /// Glow intensity (0.0-1.0).
    pub glow_intensity: f32,
    /// Padding to 48 bytes (struct align = 16 from vec4).
    pub _padding1: f32,
    pub _padding2: f32,
    pub _padding3: f32,
}

/// Render-world resource holding bloom GPU state.
#[derive(Resource)]
pub struct MsdfBloomResources {
    // --- Textures (half-resolution) ---
    /// Glow source / blur ping texture.
    #[allow(dead_code)]
    pub glow_source: Texture,
    pub glow_source_view: TextureView,
    /// Blur pong texture.
    #[allow(dead_code)]
    pub glow_temp: Texture,
    pub glow_temp_view: TextureView,
    /// Linear sampler shared across passes.
    pub sampler: Sampler,

    // --- Pipelines ---
    pub extract_pipeline: CachedRenderPipelineId,
    pub blur_pipeline: CachedRenderPipelineId,
    pub composite_pipeline: CachedRenderPipelineId,

    // --- Bind groups (rebuilt each frame in prepare) ---
    /// Extract pass: reads intermediate texture.
    pub extract_bind_group: Option<BindGroup>,
    /// Blur H pass: reads glow_source.
    pub blur_h_bind_group: Option<BindGroup>,
    /// Blur V pass: reads glow_temp.
    pub blur_v_bind_group: Option<BindGroup>,
    /// Composite pass: reads glow_source (blurred result).
    pub composite_bind_group: Option<BindGroup>,

    // --- Uniform buffers ---
    pub extract_uniform_buffer: Buffer,
    pub blur_h_uniform_buffer: Buffer,
    pub blur_v_uniform_buffer: Buffer,
    pub composite_uniform_buffer: Buffer,

    /// Cached half-resolution for resize detection.
    pub half_resolution: (u32, u32),

    /// Number of blur iterations for current glow_spread.
    pub blur_iterations: u32,
}

/// Bloom pipeline setup (created once via FromWorld).
#[derive(Resource)]
pub struct MsdfBloomPipeline {
    pub bind_group_layout: BindGroupLayout,
    pub bind_group_layout_descriptor: BindGroupLayoutDescriptor,
    pub shader: Handle<Shader>,
}

impl FromWorld for MsdfBloomPipeline {
    fn from_world(world: &mut World) -> Self {
        let device = world.resource::<RenderDevice>();
        let asset_server = world.resource::<AssetServer>();

        let entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                // Uniforms
                uniform_buffer::<BloomUniforms>(false),
                // Input texture
                texture_2d(TextureSampleType::Float { filterable: true }),
                // Sampler
                sampler(SamplerBindingType::Filtering),
            ),
        );

        let bind_group_layout = device.create_bind_group_layout(
            Some("msdf_bloom_bind_group_layout"),
            &entries,
        );

        let bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "msdf_bloom_bind_group_layout",
            entries.to_vec().as_slice(),
        );

        let shader = asset_server.load("shaders/msdf_bloom.wgsl");

        Self {
            bind_group_layout,
            bind_group_layout_descriptor,
            shader,
        }
    }
}

/// Initialize bloom resources when MSDF resources exist.
pub fn init_bloom_resources(
    mut commands: Commands,
    device: Res<RenderDevice>,
    bloom_pipeline: Res<MsdfBloomPipeline>,
    msdf_resources: Res<MsdfTextResources>,
    pipeline_cache: Res<PipelineCache>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
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

    let half_w = (width / 2).max(1);
    let half_h = (height / 2).max(1);
    let format = msdf_resources.format;

    // Create half-res textures for bloom
    let (glow_source, glow_source_view) = create_bloom_texture(&device, half_w, half_h, format, "msdf_glow_source");
    let (glow_temp, glow_temp_view) = create_bloom_texture(&device, half_w, half_h, format, "msdf_glow_temp");

    let bloom_sampler = device.create_sampler(&SamplerDescriptor {
        label: Some("msdf_bloom_sampler"),
        address_mode_u: AddressMode::ClampToEdge,
        address_mode_v: AddressMode::ClampToEdge,
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        ..default()
    });

    let layout_desc = bloom_pipeline.bind_group_layout_descriptor.clone();

    // Queue extract pipeline
    let extract_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("msdf_bloom_extract".into()),
        layout: vec![layout_desc.clone()],
        push_constant_ranges: vec![],
        vertex: VertexState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("fullscreen_vertex".into()),
            buffers: vec![],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..default()
        },
        depth_stencil: None,
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("extract".into()),
            targets: vec![Some(ColorTargetState {
                format,
                blend: None, // Overwrite
                write_mask: ColorWrites::ALL,
            })],
        }),
        zero_initialize_workgroup_memory: false,
    });

    // Queue blur pipeline
    let blur_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("msdf_bloom_blur".into()),
        layout: vec![layout_desc.clone()],
        push_constant_ranges: vec![],
        vertex: VertexState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("fullscreen_vertex".into()),
            buffers: vec![],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..default()
        },
        depth_stencil: None,
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("blur".into()),
            targets: vec![Some(ColorTargetState {
                format,
                blend: None, // Overwrite
                write_mask: ColorWrites::ALL,
            })],
        }),
        zero_initialize_workgroup_memory: false,
    });

    // Queue composite pipeline with "behind" blend
    let composite_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("msdf_bloom_composite".into()),
        layout: vec![layout_desc],
        push_constant_ranges: vec![],
        vertex: VertexState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("fullscreen_vertex".into()),
            buffers: vec![],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..default()
        },
        depth_stencil: None,
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            shader: bloom_pipeline.shader.clone(),
            shader_defs: vec![],
            entry_point: Some("composite".into()),
            targets: vec![Some(ColorTargetState {
                format,
                // "Behind" blend: glow only where intermediate is transparent.
                // dst has the sharp text (from MsdfTextRenderNode).
                // src is the blurred glow.
                // Result: glow * (1 - dst_alpha) + dst
                blend: Some(BlendState {
                    color: BlendComponent {
                        src_factor: BlendFactor::OneMinusDstAlpha,
                        dst_factor: BlendFactor::One,
                        operation: BlendOperation::Add,
                    },
                    alpha: BlendComponent {
                        src_factor: BlendFactor::OneMinusDstAlpha,
                        dst_factor: BlendFactor::One,
                        operation: BlendOperation::Add,
                    },
                }),
                write_mask: ColorWrites::ALL,
            })],
        }),
        zero_initialize_workgroup_memory: false,
    });

    // Create uniform buffers
    let uniform_size = std::mem::size_of::<BloomUniforms>() as u64;
    let create_uniform = |label| {
        device.create_buffer(&BufferDescriptor {
            label: Some(label),
            size: uniform_size,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };

    commands.insert_resource(MsdfBloomResources {
        glow_source,
        glow_source_view,
        glow_temp,
        glow_temp_view,
        sampler: bloom_sampler,
        extract_pipeline,
        blur_pipeline,
        composite_pipeline,
        extract_bind_group: None,
        blur_h_bind_group: None,
        blur_v_bind_group: None,
        composite_bind_group: None,
        extract_uniform_buffer: create_uniform("msdf_bloom_extract_uniforms"),
        blur_h_uniform_buffer: create_uniform("msdf_bloom_blur_h_uniforms"),
        blur_v_uniform_buffer: create_uniform("msdf_bloom_blur_v_uniforms"),
        composite_uniform_buffer: create_uniform("msdf_bloom_composite_uniforms"),
        half_resolution: (half_w, half_h),
        blur_iterations: 1,
    });

    trace!("Bloom resources initialized: {}x{} (half: {}x{})", width, height, half_w, half_h);
}

/// Prepare bloom bind groups and uniforms each frame.
pub fn prepare_bloom(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    bloom_pipeline: Res<MsdfBloomPipeline>,
    render_config: Option<Res<ExtractedMsdfRenderConfig>>,
    msdf_resources: Res<MsdfTextResources>,
    mut bloom_resources: ResMut<MsdfBloomResources>,
) {
    let Some(config) = render_config else {
        return;
    };
    if !config.initialized {
        return;
    }

    let width = config.resolution[0] as u32;
    let height = config.resolution[1] as u32;
    let half_w = (width / 2).max(1);
    let half_h = (height / 2).max(1);

    // Resize textures if needed
    if bloom_resources.half_resolution != (half_w, half_h) && half_w > 0 && half_h > 0 {
        let format = msdf_resources.format;
        let (gs, gsv) = create_bloom_texture(&device, half_w, half_h, format, "msdf_glow_source");
        let (gt, gtv) = create_bloom_texture(&device, half_w, half_h, format, "msdf_glow_temp");
        bloom_resources.glow_source = gs;
        bloom_resources.glow_source_view = gsv;
        bloom_resources.glow_temp = gt;
        bloom_resources.glow_temp_view = gtv;
        bloom_resources.half_resolution = (half_w, half_h);
    }

    // Compute blur iterations from glow_spread
    bloom_resources.blur_iterations = match config.glow_spread as u32 {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    };

    // Half-res texel size for blur shader
    let half_texel = [1.0 / half_w as f32, 1.0 / half_h as f32];

    // Write extract uniforms
    let extract_uniforms = BloomUniforms {
        glow_color: config.glow_color,
        blur_direction: [0.0, 0.0],
        texel_size: [0.0, 0.0],
        glow_intensity: config.glow_intensity,
        _padding1: 0.0,
        _padding2: 0.0,
        _padding3: 0.0,
    };
    queue.write_buffer(&bloom_resources.extract_uniform_buffer, 0, bytemuck::bytes_of(&extract_uniforms));

    // Write blur H uniforms
    let blur_h_uniforms = BloomUniforms {
        glow_color: [0.0; 4],
        blur_direction: [1.0, 0.0],
        texel_size: half_texel,
        glow_intensity: 0.0,
        _padding1: 0.0,
        _padding2: 0.0,
        _padding3: 0.0,
    };
    queue.write_buffer(&bloom_resources.blur_h_uniform_buffer, 0, bytemuck::bytes_of(&blur_h_uniforms));

    // Write blur V uniforms
    let blur_v_uniforms = BloomUniforms {
        glow_color: [0.0; 4],
        blur_direction: [0.0, 1.0],
        texel_size: half_texel,
        glow_intensity: 0.0,
        _padding1: 0.0,
        _padding2: 0.0,
        _padding3: 0.0,
    };
    queue.write_buffer(&bloom_resources.blur_v_uniform_buffer, 0, bytemuck::bytes_of(&blur_v_uniforms));

    // Write composite uniforms (unused fields, but buffer must be valid)
    let composite_uniforms = BloomUniforms {
        glow_color: [0.0; 4],
        blur_direction: [0.0, 0.0],
        texel_size: [0.0, 0.0],
        glow_intensity: 0.0,
        _padding1: 0.0,
        _padding2: 0.0,
        _padding3: 0.0,
    };
    queue.write_buffer(&bloom_resources.composite_uniform_buffer, 0, bytemuck::bytes_of(&composite_uniforms));

    // Get intermediate texture view for extract pass input
    let Some(intermediate_view) = &msdf_resources.intermediate_texture_view else {
        return;
    };

    let layout = &bloom_pipeline.bind_group_layout;

    // Extract bind group: reads intermediate texture
    bloom_resources.extract_bind_group = Some(device.create_bind_group(
        "msdf_bloom_extract_bg",
        layout,
        &BindGroupEntries::sequential((
            bloom_resources.extract_uniform_buffer.as_entire_binding(),
            intermediate_view,
            &bloom_resources.sampler,
        )),
    ));

    // Blur H bind group: reads glow_source
    bloom_resources.blur_h_bind_group = Some(device.create_bind_group(
        "msdf_bloom_blur_h_bg",
        layout,
        &BindGroupEntries::sequential((
            bloom_resources.blur_h_uniform_buffer.as_entire_binding(),
            &bloom_resources.glow_source_view,
            &bloom_resources.sampler,
        )),
    ));

    // Blur V bind group: reads glow_temp
    bloom_resources.blur_v_bind_group = Some(device.create_bind_group(
        "msdf_bloom_blur_v_bg",
        layout,
        &BindGroupEntries::sequential((
            bloom_resources.blur_v_uniform_buffer.as_entire_binding(),
            &bloom_resources.glow_temp_view,
            &bloom_resources.sampler,
        )),
    ));

    // Composite bind group: reads glow_source (final blurred result)
    bloom_resources.composite_bind_group = Some(device.create_bind_group(
        "msdf_bloom_composite_bg",
        layout,
        &BindGroupEntries::sequential((
            bloom_resources.composite_uniform_buffer.as_entire_binding(),
            &bloom_resources.glow_source_view,
            &bloom_resources.sampler,
        )),
    ));
}

/// Bloom render node — 4 sub-passes.
#[derive(Default)]
pub struct MsdfBloomNode;

impl MsdfBloomNode {
    pub const NAME: MsdfBloomNodeLabel = MsdfBloomNodeLabel;
}

impl ViewNode for MsdfBloomNode {
    type ViewQuery = &'static ViewTarget;

    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        _view_target: &ViewTarget,
        world: &World,
    ) -> Result<(), NodeRunError> {
        // Early exit if glow is disabled
        let Some(config) = world.get_resource::<ExtractedMsdfRenderConfig>() else {
            return Ok(());
        };
        if config.glow_intensity <= 0.0 {
            return Ok(());
        }

        let Some(bloom) = world.get_resource::<MsdfBloomResources>() else {
            return Ok(());
        };

        let Some(msdf) = world.get_resource::<MsdfTextResources>() else {
            return Ok(());
        };

        // Need intermediate texture
        let Some(_intermediate_view) = &msdf.intermediate_texture_view else {
            return Ok(());
        };

        // Need all bind groups
        let (Some(extract_bg), Some(blur_h_bg), Some(blur_v_bg), Some(composite_bg)) = (
            &bloom.extract_bind_group,
            &bloom.blur_h_bind_group,
            &bloom.blur_v_bind_group,
            &bloom.composite_bind_group,
        ) else {
            return Ok(());
        };

        let pipeline_cache = world.resource::<PipelineCache>();

        let Some(extract_pipeline) = pipeline_cache.get_render_pipeline(bloom.extract_pipeline)
        else {
            return Ok(());
        };
        let Some(blur_pipeline) = pipeline_cache.get_render_pipeline(bloom.blur_pipeline) else {
            return Ok(());
        };
        let Some(composite_pipeline) =
            pipeline_cache.get_render_pipeline(bloom.composite_pipeline)
        else {
            return Ok(());
        };

        // Skip during resize
        let config_size = (config.resolution[0] as u32, config.resolution[1] as u32);
        if config_size != msdf.texture_size && msdf.texture_size != (0, 0) {
            return Ok(());
        }

        // === PASS 1: Extract ===
        // Intermediate (full-res) → glow_source (half-res), tinted with glow_color
        {
            let mut pass = render_context.command_encoder().begin_render_pass(
                &RenderPassDescriptor {
                    label: Some("msdf_bloom_extract"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: &bloom.glow_source_view,
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
            pass.set_pipeline(extract_pipeline);
            pass.set_bind_group(0, extract_bg, &[]);
            pass.draw(0..3, 0..1);
        }

        // === PASSES 2-3: Blur (possibly multiple iterations) ===
        for _ in 0..bloom.blur_iterations {
            // Blur H: glow_source → glow_temp
            {
                let mut pass = render_context.command_encoder().begin_render_pass(
                    &RenderPassDescriptor {
                        label: Some("msdf_bloom_blur_h"),
                        color_attachments: &[Some(RenderPassColorAttachment {
                            view: &bloom.glow_temp_view,
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
                pass.set_pipeline(blur_pipeline);
                pass.set_bind_group(0, blur_h_bg, &[]);
                pass.draw(0..3, 0..1);
            }

            // Blur V: glow_temp → glow_source
            {
                let mut pass = render_context.command_encoder().begin_render_pass(
                    &RenderPassDescriptor {
                        label: Some("msdf_bloom_blur_v"),
                        color_attachments: &[Some(RenderPassColorAttachment {
                            view: &bloom.glow_source_view,
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
                pass.set_pipeline(blur_pipeline);
                pass.set_bind_group(0, blur_v_bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        // === PASS 4: Composite ===
        // Glow_source (blurred, half-res) → intermediate (full-res) with "behind" blend
        // The intermediate already has sharp text. "Behind" blend adds glow only
        // in transparent regions around the text.
        {
            let intermediate_view = msdf.intermediate_texture_view.as_ref().unwrap();
            let mut pass = render_context.command_encoder().begin_render_pass(
                &RenderPassDescriptor {
                    label: Some("msdf_bloom_composite"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: intermediate_view,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load, // Preserve sharp text!
                            store: StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                },
            );
            pass.set_pipeline(composite_pipeline);
            pass.set_bind_group(0, composite_bg, &[]);
            pass.draw(0..3, 0..1);
        }

        Ok(())
    }
}

/// Create a half-res bloom texture with view.
fn create_bloom_texture(
    device: &RenderDevice,
    width: u32,
    height: u32,
    format: TextureFormat,
    label: &str,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}
