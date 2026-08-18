//! Instanced MSDF glyph renderer for the conversation surface.
//!
//! Sibling to [`super::renderer::MsdfBlockRenderer`], with one structural
//! difference that is the entire point of the conversation-surface rewrite:
//! **the vertex buffer does not change when you scroll**. Glyph instances
//! carry *document-space logical* coordinates, uploaded once per window
//! rebuild; the scroll offset lives in a uniform, so a wheel detent costs one
//! `write_buffer` of 64 bytes instead of rebuilding every quad on the CPU.
//!
//! # What moved into the shader, and why
//!
//! `MsdfBlockRenderer::build_vertices` snaps each glyph's baseline to an
//! integer *physical* pixel row (Y snapped, X left sub-pixel) while building
//! NDC on the CPU. That doctrine is unchanged — but the snap depends on the
//! scroll offset, which is only known at draw time here, so it happens in
//! WGSL instead. [`snap_baseline_phys`] is the Rust mirror of that one line
//! and is what the tests pin; `assets/shaders/msdf_surface.wgsl` must keep
//! the identical formula (both sides carry a comment saying so).
//!
//! Everything that does *not* depend on the scroll offset stays on the CPU,
//! at instance-build time: atlas UV resolution, the em-unit anchor offsets,
//! and the quad size derived from `font_size / MSDF_PX_PER_EM`. That keeps
//! the shader to "apply scale, subtract scroll, snap, place corner" and
//! keeps the atlas lookup (a `HashMap` probe that can *fail* — a glyph whose
//! MSDF hasn't been generated yet is skipped, exactly as `build_vertices`
//! skips it) on the side that can express failure.
//!
//! # Geometry
//!
//! Six vertices, non-indexed, `TriangleList` — the same two-triangle winding
//! `build_vertices` emits — drawn `instance_count` times with a single
//! instance-stepped vertex buffer. Six vertices rather than four + an index
//! buffer because it matches the existing pass exactly and costs one extra
//! vertex invocation per glyph; there is no index buffer to allocate, bind,
//! or keep in sync.

use bevy::mesh::VertexBufferLayout;
use bevy::prelude::*;
use bevy::render::{
    render_asset::RenderAssets,
    render_resource::{
        binding_types::{sampler as sampler_binding, texture_2d, uniform_buffer},
        *,
    },
    renderer::RenderDevice,
    texture::GpuImage,
};
use bytemuck::{Pod, Zeroable};

use super::renderer::{ExtractedMsdfAtlas, MSDF_PX_PER_EM};
use crate::view::surface::chrome::ChromeInstance;
use crate::view::surface::window::{SurfaceGeometryRun, SurfaceQuad, SurfaceRun};

/// Vertices per glyph quad (non-indexed triangle list).
pub const SURFACE_QUAD_VERTICES: u32 = 6;

/// One glyph, in document-space logical pixels.
///
/// Field order is the vertex-attribute order; see
/// [`ConversationSurfaceRenderer::instance_layout`]. `#[repr(C)]` with no
/// padding (8+8+8+16+4+4 = 48 bytes) so `bytemuck` can cast a slice of these
/// straight into the buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct GlyphInstance {
    /// The glyph's pen position in **document space, logical pixels**:
    /// `(run.x_offset + glyph.x, run.doc_y + glyph.y)`. Y is a baseline.
    pub baseline_doc: [f32; 2],
    /// Offset from that pen position to the quad's top-left corner, in
    /// logical pixels — the atlas region's em-unit anchor times the glyph's
    /// font size, negated. Applied *after* the baseline snap, which is why
    /// it is a separate field rather than folded into `baseline_doc`.
    pub quad_offset: [f32; 2],
    /// Quad size in logical pixels: the atlas region's pixel size scaled by
    /// `font_size / MSDF_PX_PER_EM`.
    pub quad_size: [f32; 2],
    /// Atlas UV rect `[u0, v0, u1, v1]`. The V axis is flipped when the
    /// corner UV is interpolated (msdfgen bitmaps are Y-up, GPU textures are
    /// Y-down) — the same flip `build_vertices` does by hand.
    pub uv_rect: [f32; 4],
    /// Straight (non-premultiplied) RGBA8; the fragment shader premultiplies.
    pub color: [u8; 4],
    /// Semantic weight — 0.5 normal, higher bolder. Same meaning as
    /// `PositionedGlyph::importance`.
    pub importance: f32,
}

/// Per-surface uniforms.
///
/// The first four fields are the surface's own; the rest are the MSDF
/// quality knobs `msdf_block.wgsl` reads, carried verbatim so the fragment
/// shader could be shared with `msdf_block.wgsl` if the duplication is ever
/// paid down (a tracked follow-up in the plan's Risks).
///
/// Layout is hand-checked against the WGSL `Uniforms` struct: every member is
/// 4-byte aligned, the three `vec2`s sit at offsets 0, 16 and 56 (8-aligned as
/// WGSL requires), and the block is 64 bytes so the uniform binding is a
/// multiple of 16. Both `msdf_surface.wgsl` and `surface_chrome.wgsl` mirror
/// it — a field added here is added twice.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, ShaderType)]
pub struct SurfaceUniforms {
    /// Render-target size in PHYSICAL pixels.
    pub viewport_phys: [f32; 2],
    /// Scroll offset in **logical** document pixels — the one value that
    /// changes when the user scrolls inside a built window.
    pub scroll_offset: f32,
    /// Logical → physical scale (the window's HiDPI factor).
    pub scale: f32,
    /// `(1/atlas_width, 1/atlas_height)` for gradient sampling.
    pub sdf_texel: [f32; 2],
    /// MSDF range in atlas pixels.
    pub msdf_range: f32,
    pub hint_amount: f32,
    pub stem_darkening: f32,
    pub horz_scale: f32,
    pub vert_scale: f32,
    pub text_bias: f32,
    pub gamma_correction: f32,
    /// Seconds, for animated effects — the chrome pass's breathe/pulse/chase
    /// clock (`surface_chrome.wgsl`). The glyph pass ignores it.
    pub time: f32,
    /// Where document (0, 0) sits inside the render target, in **logical**
    /// px.
    ///
    /// The composite node is absolutely positioned at all four insets zero,
    /// so the texture covers the pane's **padding** box, while every document
    /// coordinate in the model is measured from the **content** box (that is
    /// what `logical_content_size` gives the window and the shaper). The
    /// difference is the pane's padding, and carrying it here is what keeps
    /// the surface's text and chrome landing on the same pixels the legacy
    /// per-block cells land on — see `view/surface/target.rs`.
    ///
    /// It occupies the two trailing pads the struct used to carry, so the
    /// uniform is still 64 bytes and still 8-byte aligned where WGSL needs a
    /// `vec2`.
    pub origin_logical: [f32; 2],
}

impl Default for SurfaceUniforms {
    fn default() -> Self {
        Self {
            viewport_phys: [1.0, 1.0],
            scroll_offset: 0.0,
            scale: 1.0,
            sdf_texel: [1.0 / 1024.0, 1.0 / 1024.0],
            msdf_range: 4.0,
            hint_amount: 0.8,
            stem_darkening: 0.15,
            horz_scale: 1.1,
            vert_scale: 0.6,
            text_bias: 0.5,
            gamma_correction: 0.85,
            time: 0.0,
            origin_logical: [0.0, 0.0],
        }
    }
}

/// The color target every surface pipeline writes: premultiplied-alpha over,
/// into the surface's non-sRGB RTT.
///
/// One function rather than four copies — they are not merely similar, they
/// must be *identical*, because the four passes composite into the same
/// texture and a blend factor that drifted on one of them would show up as a
/// halo nobody could attribute.
fn premultiplied_target() -> ColorTargetState {
    ColorTargetState {
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
    }
}

/// The baseline's physical-pixel row for a document-space Y at a given
/// scroll offset and HiDPI scale.
///
/// **Mirror of `msdf_surface.wgsl`'s vertex shader** — the single line
///
/// ```wgsl
/// let y_base_phys = round((in.baseline_doc.y - u.scroll_offset) * u.scale);
/// ```
///
/// (The shader folds `u.origin_logical.y` into the doc-space Y before this —
/// a constant document offset, not a change to the formula.)
///
/// Snapping happens *after* the scroll subtraction and *in physical space*,
/// for the reason `MsdfBlockRenderer::build_vertices` documents at length: at
/// a fractional scale factor a logical-integer baseline is not a physical
/// scanline, and horizontal stems blur when you snap in the wrong space. X is
/// deliberately never snapped — sub-pixel advances are what keep kerning and
/// monospace columns correct.
///
/// This also absorbs the chunk-stacking quantization slop that `chunk.rs`
/// documents (parley re-quantizes baselines per layout, so a chunk stacked at
/// a fractional offset can sit up to <1 logical px off whole-block shaping):
/// the snap collapses the difference to whichever physical row is nearest,
/// and it does not accumulate.
///
/// Deliberately has no Rust caller — the GPU does this at draw time. It exists
/// so the formula is *testable*, which is the only way a shader line that
/// decides whether text swims can be pinned at all.
#[allow(dead_code)]
pub fn snap_baseline_phys(doc_y: f32, scroll: f32, scale: f32) -> f32 {
    ((doc_y - scroll) * scale).round()
}

/// One flat-colored geometry vertex, in document-space logical pixels.
///
/// Not instanced — a triangle list is what the builders produce
/// (`text::msdf::geometry`, `text::diff`, `text::sparkline`), and inventing an
/// instance shape for arbitrary triangulated shapes would mean re-deriving
/// them.
///
/// `anchor_doc_y` is what keeps the shapes rigid: every vertex of a run
/// carries the same anchor, which the shader snaps to a physical row exactly
/// as it snaps a glyph baseline, and `pos_doc.y` is an unsnapped offset from
/// it. Snapping each vertex on its own would make a 1.2px staff line
/// disappear or double depending on where the scroll happened to be.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct SurfaceGeometryVertex {
    /// `x` = document x, logical px. `y` = offset from `anchor_doc_y`.
    pub pos_doc: [f32; 2],
    /// The run's document y — the value the shader snaps.
    pub anchor_doc_y: f32,
    /// Straight (non-premultiplied) RGBA8.
    pub color: [u8; 4],
}

/// One textured quad — an SVG raster — in document-space logical pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct QuadInstance {
    /// `[x, y, w, h]`, logical document px.
    pub rect_doc: [f32; 4],
}

/// Render-world resource: the four pipelines a conversation surface draws
/// with — instanced glyphs, instanced chrome quads, flat geometry, and
/// textured quads.
///
/// Four pipelines, one pass, one uniform buffer. Each gets the bind group
/// layout it actually needs — chrome and geometry are uniform-only, so
/// borders and staff lines draw on a frame where the glyph atlas hasn't
/// reached the GPU yet instead of inheriting the text pass's readiness.
#[derive(Resource)]
pub struct ConversationSurfaceRenderer {
    pub pipeline: CachedRenderPipelineId,
    pub bind_group_layout: BindGroupLayout,
    pub sampler: Sampler,
    pub chrome_pipeline: CachedRenderPipelineId,
    pub chrome_bind_group_layout: BindGroupLayout,
    pub geometry_pipeline: CachedRenderPipelineId,
    pub geometry_bind_group_layout: BindGroupLayout,
    pub quad_pipeline: CachedRenderPipelineId,
    pub quad_bind_group_layout: BindGroupLayout,
}

/// What a surface pass actually managed to encode. Split rather than a bare
/// `bool` because "cleared the target" and "drew glyphs" fail for different
/// reasons (target not GPU-prepared vs pipeline/atlas not ready), and a
/// caller that conflates them either warns too loudly or goes silent when
/// text never appears.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePassResult {
    pub cleared: bool,
    pub drawn: u32,
}

impl ConversationSurfaceRenderer {
    /// The instance-stepped vertex buffer layout. Attribute offsets mirror
    /// [`GlyphInstance`]'s field order exactly.
    pub fn instance_layout() -> VertexBufferLayout {
        VertexBufferLayout {
            array_stride: std::mem::size_of::<GlyphInstance>() as u64,
            step_mode: VertexStepMode::Instance,
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
                    format: VertexFormat::Float32x2,
                    offset: 16,
                    shader_location: 2,
                },
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: 24,
                    shader_location: 3,
                },
                VertexAttribute {
                    format: VertexFormat::Unorm8x4,
                    offset: 40,
                    shader_location: 4,
                },
                VertexAttribute {
                    format: VertexFormat::Float32,
                    offset: 44,
                    shader_location: 5,
                },
            ],
        }
    }

    /// The chrome instance layout. Attribute offsets mirror
    /// [`ChromeInstance`]'s field order exactly; both are pinned by
    /// `chrome_instance_layout_matches_the_struct`.
    pub fn chrome_instance_layout() -> VertexBufferLayout {
        VertexBufferLayout {
            array_stride: std::mem::size_of::<ChromeInstance>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: vec![
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: 0,
                    shader_location: 0,
                },
                VertexAttribute {
                    format: VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 1,
                },
                VertexAttribute {
                    format: VertexFormat::Float32x2,
                    offset: 32,
                    shader_location: 2,
                },
                VertexAttribute {
                    format: VertexFormat::Float32x2,
                    offset: 40,
                    shader_location: 3,
                },
                VertexAttribute {
                    format: VertexFormat::Unorm8x4,
                    offset: 48,
                    shader_location: 4,
                },
                VertexAttribute {
                    format: VertexFormat::Uint32,
                    offset: 52,
                    shader_location: 5,
                },
            ],
        }
    }

    /// The geometry vertex layout — vertex-stepped, mirroring
    /// [`SurfaceGeometryVertex`]'s fields.
    pub fn geometry_vertex_layout() -> VertexBufferLayout {
        VertexBufferLayout {
            array_stride: std::mem::size_of::<SurfaceGeometryVertex>() as u64,
            step_mode: VertexStepMode::Vertex,
            attributes: vec![
                VertexAttribute {
                    format: VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                VertexAttribute {
                    format: VertexFormat::Float32,
                    offset: 8,
                    shader_location: 1,
                },
                VertexAttribute {
                    format: VertexFormat::Unorm8x4,
                    offset: 12,
                    shader_location: 2,
                },
            ],
        }
    }

    /// The quad instance layout — one `vec4` rect per instance.
    pub fn quad_instance_layout() -> VertexBufferLayout {
        VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadInstance>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: vec![VertexAttribute {
                format: VertexFormat::Float32x4,
                offset: 0,
                shader_location: 0,
            }],
        }
    }

    /// Initialize the renderer in the render world (needs `RenderDevice` +
    /// `PipelineCache`, so it belongs in a plugin's `finish`, not `build` —
    /// same as `MsdfBlockRenderer::init`).
    pub fn init(
        device: &RenderDevice,
        pipeline_cache: &PipelineCache,
        asset_server: &AssetServer,
    ) -> Self {
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("msdf_surface_atlas_sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Nearest,
            ..default()
        });

        let layout_entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                uniform_buffer::<SurfaceUniforms>(false),
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler_binding(SamplerBindingType::Filtering),
            ),
        );

        let bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "msdf_surface_bind_group_layout",
            &layout_entries,
        );

        let bind_group_layout = device.create_bind_group_layout(
            "msdf_surface_bind_group_layout",
            &layout_entries,
        );

        let shader = asset_server.load("shaders/msdf_surface.wgsl");

        let pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("msdf_surface_pipeline".into()),
            layout: vec![bind_group_layout_descriptor],
            vertex: VertexState {
                shader: shader.clone(),
                entry_point: Some("vertex".into()),
                shader_defs: vec![],
                buffers: vec![Self::instance_layout()],
            },
            fragment: Some(FragmentState {
                shader,
                entry_point: Some("fragment".into()),
                shader_defs: vec![],
                targets: vec![Some(ColorTargetState {
                    // Premultiplied-alpha over, matching msdf_block: the
                    // fragment shader emits premultiplied RGB.
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

        // ── Chrome ──────────────────────────────────────────────────────
        //
        // Uniform only: no atlas, no sampler. Borders must be able to draw on
        // a frame the glyph atlas hasn't reached the GPU on.
        let chrome_layout_entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (uniform_buffer::<SurfaceUniforms>(false),),
        );
        let chrome_bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "surface_chrome_bind_group_layout",
            &chrome_layout_entries,
        );
        let chrome_bind_group_layout = device.create_bind_group_layout(
            "surface_chrome_bind_group_layout",
            &chrome_layout_entries,
        );
        let chrome_shader = asset_server.load("shaders/surface_chrome.wgsl");
        let chrome_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("surface_chrome_pipeline".into()),
            layout: vec![chrome_bind_group_layout_descriptor],
            vertex: VertexState {
                shader: chrome_shader.clone(),
                entry_point: Some("vertex".into()),
                shader_defs: vec![],
                buffers: vec![Self::chrome_instance_layout()],
            },
            fragment: Some(FragmentState {
                shader: chrome_shader,
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

        // ── Geometry ────────────────────────────────────────────────────
        //
        // Uniform only, like chrome: a staff line samples nothing.
        let geometry_layout_entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (uniform_buffer::<SurfaceUniforms>(false),),
        );
        let geometry_bind_group_layout_descriptor = BindGroupLayoutDescriptor::new(
            "surface_geometry_bind_group_layout",
            &geometry_layout_entries,
        );
        let geometry_bind_group_layout = device.create_bind_group_layout(
            "surface_geometry_bind_group_layout",
            &geometry_layout_entries,
        );
        let geometry_shader = asset_server.load("shaders/surface_geometry.wgsl");
        let geometry_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("surface_geometry_pipeline".into()),
            layout: vec![geometry_bind_group_layout_descriptor],
            vertex: VertexState {
                shader: geometry_shader.clone(),
                entry_point: Some("vertex".into()),
                shader_defs: vec![],
                buffers: vec![Self::geometry_vertex_layout()],
            },
            fragment: Some(FragmentState {
                shader: geometry_shader,
                entry_point: Some("fragment".into()),
                shader_defs: vec![],
                targets: vec![Some(premultiplied_target())],
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

        // ── Textured quads (SVG rasters) ────────────────────────────────
        //
        // Its own sampler is not worth a second allocation — the atlas
        // sampler is already linear/clamp, which is what a downscaled raster
        // wants too.
        let quad_layout_entries = BindGroupLayoutEntries::sequential(
            ShaderStages::VERTEX_FRAGMENT,
            (
                uniform_buffer::<SurfaceUniforms>(false),
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler_binding(SamplerBindingType::Filtering),
            ),
        );
        let quad_bind_group_layout_descriptor =
            BindGroupLayoutDescriptor::new("surface_quad_bind_group_layout", &quad_layout_entries);
        let quad_bind_group_layout =
            device.create_bind_group_layout("surface_quad_bind_group_layout", &quad_layout_entries);
        let quad_shader = asset_server.load("shaders/surface_quad.wgsl");
        let quad_pipeline = pipeline_cache.queue_render_pipeline(RenderPipelineDescriptor {
            label: Some("surface_quad_pipeline".into()),
            layout: vec![quad_bind_group_layout_descriptor],
            vertex: VertexState {
                shader: quad_shader.clone(),
                entry_point: Some("vertex".into()),
                shader_defs: vec![],
                buffers: vec![Self::quad_instance_layout()],
            },
            fragment: Some(FragmentState {
                shader: quad_shader,
                entry_point: Some("fragment".into()),
                shader_defs: vec![],
                targets: vec![Some(premultiplied_target())],
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

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            chrome_pipeline,
            chrome_bind_group_layout,
            geometry_pipeline,
            geometry_bind_group_layout,
            quad_pipeline,
            quad_bind_group_layout,
        }
    }

    /// Encode one surface's pass into a shared `encoder` (no submit — the
    /// caller batches every pane for the frame and submits once, the
    /// `render_msdf_block_textures` pattern).
    ///
    /// The pass **always clears** when the target is available, even with
    /// nothing to draw: the surface owns every pixel of its viewport, so a
    /// frame that draws no glyphs must still repaint the background rather
    /// than leave the previous frame's text standing.
    ///
    /// **Draw order: chrome → geometry → rasters → glyphs.** Chrome first
    /// because `block_fx.wgsl` composites both the border stroke and its glow
    /// behind the block's text (see `view::surface::chrome`); geometry next
    /// because that is the order the legacy path composites it in
    /// (`render_msdf_block_textures` runs `MusicGeometryRenderer` before the
    /// glyph pass, so noteheads land on staff lines and diff text on its
    /// bands); rasters after geometry and before glyphs, where the legacy
    /// child `ImageNode` sits — above the block's background, below anything
    /// textual.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_render(
        &self,
        device: &RenderDevice,
        encoder: &mut CommandEncoder,
        pipeline_cache: &PipelineCache,
        gpu_images: &RenderAssets<GpuImage>,
        atlas_image: &Handle<Image>,
        target_image: &Handle<Image>,
        instances: Option<(&Buffer, u32)>,
        chrome: Option<(&Buffer, u32)>,
        geometry: Option<(&Buffer, u32)>,
        quads: Option<(&Buffer, &[Handle<Image>])>,
        uniforms: &SurfaceUniforms,
        clear: bevy::color::LinearRgba,
    ) -> SurfacePassResult {
        let Some(target_gpu) = gpu_images.get(target_image) else {
            // Expected for a frame or two after a resize (Bevy prepares
            // GpuImage from the asset event on its own schedule); the caller
            // escalates if it persists.
            return SurfacePassResult::default();
        };

        // Everything the draw needs, or nothing: a missing pipeline/atlas
        // still clears (background paints; text appears when ready).
        let draw = instances
            .filter(|(_, count)| *count > 0)
            .and_then(|(buffer, count)| {
                let pipeline = pipeline_cache.get_render_pipeline(self.pipeline)?;
                let atlas_gpu = gpu_images.get(atlas_image)?;
                Some((pipeline, atlas_gpu, buffer, count))
            });

        let chrome_draw = chrome
            .filter(|(_, count)| *count > 0)
            .and_then(|(buffer, count)| {
                let pipeline = pipeline_cache.get_render_pipeline(self.chrome_pipeline)?;
                Some((pipeline, buffer, count))
            });

        let geometry_draw = geometry
            .filter(|(_, count)| *count > 0)
            .and_then(|(buffer, count)| {
                let pipeline = pipeline_cache.get_render_pipeline(self.geometry_pipeline)?;
                Some((pipeline, buffer, count))
            });

        // Rasters resolve per quad: a texture that hasn't reached the GPU yet
        // skips its own quad rather than the whole pass.
        let quad_draw = quads
            .filter(|(_, images)| !images.is_empty())
            .and_then(|(buffer, images)| {
                let pipeline = pipeline_cache.get_render_pipeline(self.quad_pipeline)?;
                Some((pipeline, buffer, images))
            });

        // One uniform buffer for every pipeline — the bind group layouts
        // differ only in what else they carry.
        let uniform_buffer = (draw.is_some()
            || chrome_draw.is_some()
            || geometry_draw.is_some()
            || quad_draw.is_some())
        .then(|| {
            device.create_buffer_with_data(&BufferInitDescriptor {
                label: Some("msdf_surface_uniforms"),
                contents: bytemuck::bytes_of(uniforms),
                usage: BufferUsages::UNIFORM,
            })
        });

        let bind_group = draw.zip(uniform_buffer.as_ref()).map(
            |((_, atlas_gpu, _, _), uniform_buffer)| {
                device.create_bind_group(
                    "msdf_surface_bind_group",
                    &self.bind_group_layout,
                    &BindGroupEntries::sequential((
                        uniform_buffer.as_entire_binding(),
                        &atlas_gpu.texture_view,
                        &self.sampler,
                    )),
                )
            },
        );

        let chrome_bind_group = chrome_draw.zip(uniform_buffer.as_ref()).map(
            |(_, uniform_buffer)| {
                device.create_bind_group(
                    "surface_chrome_bind_group",
                    &self.chrome_bind_group_layout,
                    &BindGroupEntries::sequential((uniform_buffer.as_entire_binding(),)),
                )
            },
        );

        let geometry_bind_group = geometry_draw.zip(uniform_buffer.as_ref()).map(
            |(_, uniform_buffer)| {
                device.create_bind_group(
                    "surface_geometry_bind_group",
                    &self.geometry_bind_group_layout,
                    &BindGroupEntries::sequential((uniform_buffer.as_entire_binding(),)),
                )
            },
        );

        // One bind group per raster, built up front so the render pass borrows
        // them rather than the device. A handful at most are ever visible.
        let quad_bind_groups: Vec<(u32, BindGroup)> = match (&quad_draw, uniform_buffer.as_ref()) {
            (Some((_, _, images)), Some(uniform_buffer)) => images
                .iter()
                .enumerate()
                .filter_map(|(i, handle)| {
                    let gpu = gpu_images.get(handle)?;
                    Some((
                        i as u32,
                        device.create_bind_group(
                            "surface_quad_bind_group",
                            &self.quad_bind_group_layout,
                            &BindGroupEntries::sequential((
                                uniform_buffer.as_entire_binding(),
                                &gpu.texture_view,
                                &self.sampler,
                            )),
                        ),
                    ))
                })
                .collect(),
            _ => Vec::new(),
        };

        let mut drawn = 0u32;
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("msdf_surface_render_pass"),
                multiview_mask: None,
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &target_gpu.texture_view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(clear.into()),
                        store: StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if let (Some((pipeline, buffer, count)), Some(bind_group)) =
                (chrome_draw, &chrome_bind_group)
            {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.set_vertex_buffer(0, *buffer.slice(..));
                render_pass.draw(0..SURFACE_QUAD_VERTICES, 0..count);
            }

            if let (Some((pipeline, buffer, count)), Some(bind_group)) =
                (geometry_draw, &geometry_bind_group)
            {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.set_vertex_buffer(0, *buffer.slice(..));
                // Vertex-stepped, not instanced: `count` is vertices.
                render_pass.draw(0..count, 0..1);
            }

            if let Some((pipeline, buffer, _)) = quad_draw
                && !quad_bind_groups.is_empty()
            {
                render_pass.set_pipeline(pipeline);
                render_pass.set_vertex_buffer(0, *buffer.slice(..));
                for (index, bind_group) in &quad_bind_groups {
                    render_pass.set_bind_group(0, bind_group, &[]);
                    render_pass.draw(0..SURFACE_QUAD_VERTICES, *index..*index + 1);
                }
            }

            if let (Some((pipeline, _, buffer, count)), Some(bind_group)) = (draw, &bind_group) {
                render_pass.set_pipeline(pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.set_vertex_buffer(0, *buffer.slice(..));
                render_pass.draw(0..SURFACE_QUAD_VERTICES, 0..count);
                drawn = count;
            }
        }

        SurfacePassResult {
            cleared: true,
            drawn,
        }
    }
}

/// Turn assembled document-space runs into GPU instances.
///
/// Pure and render-world-only: this is the port of
/// `MsdfBlockRenderer::build_vertices`'s CPU half (atlas region lookup, UV
/// rect, `font_size / MSDF_PX_PER_EM` quad sizing, em-unit anchor offsets),
/// minus everything that depends on the scroll offset or the viewport.
///
/// A glyph whose MSDF hasn't been generated yet has no atlas region and is
/// **skipped**, exactly as `build_vertices` skips it — the atlas version bump
/// that lands it also bumps the window's `buffer_version`, which is what
/// re-runs this.
pub fn build_instances(
    runs: &[SurfaceRun],
    atlas: &ExtractedMsdfAtlas,
    out: &mut Vec<GlyphInstance>,
) {
    out.clear();
    for run in runs {
        for glyph in run.glyphs.iter() {
            let Some(region) = atlas.regions.get(&glyph.key) else {
                continue;
            };
            let scale = glyph.font_size / MSDF_PX_PER_EM;
            out.push(GlyphInstance {
                baseline_doc: [run.x_offset + glyph.x, run.doc_y + glyph.y],
                quad_offset: [
                    -region.anchor_x * glyph.font_size,
                    -region.anchor_y * glyph.font_size,
                ],
                quad_size: [
                    region.width as f32 * scale,
                    region.height as f32 * scale,
                ],
                uv_rect: region.uv_rect(atlas.width, atlas.height),
                color: glyph.color,
                importance: glyph.importance,
            });
        }
    }
}

/// Turn assembled document-space geometry runs into GPU vertices.
///
/// The only work is folding the run's `x_offset` into each vertex's x and
/// hanging the run's `doc_y` on every vertex as the snap anchor — the shapes
/// themselves were built in chunk-local logical px by the same builders the
/// legacy path uses, and nothing here reinterprets them.
pub fn build_geometry_vertices(runs: &[SurfaceGeometryRun], out: &mut Vec<SurfaceGeometryVertex>) {
    out.clear();
    for run in runs {
        for vertex in run.vertices.iter() {
            out.push(SurfaceGeometryVertex {
                pos_doc: [run.x_offset + vertex.x, vertex.y],
                anchor_doc_y: run.doc_y,
                color: vertex.color,
            });
        }
    }
}

/// Split assembled quads into the instance buffer's contents and the parallel
/// list of textures to bind, one per instance.
///
/// Parallel rather than paired because the instance buffer is `Pod` and a
/// `Handle<Image>` is not; the index into one is the instance index into the
/// other, which is exactly what `encode_render` draws with.
pub fn build_quad_instances(
    quads: &[SurfaceQuad],
    out: &mut Vec<QuadInstance>,
    images: &mut Vec<Handle<Image>>,
) {
    out.clear();
    images.clear();
    for quad in quads {
        out.push(QuadInstance {
            rect_doc: quad.doc_rect,
        });
        images.push(quad.image.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::msdf::atlas::AtlasRegion;
    use crate::text::msdf::glyph::{FontId, GlyphKey, PositionedGlyph};
    use crate::text::msdf::renderer::MsdfBlockRenderer;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn atlas_with_region(key: GlyphKey, region: AtlasRegion) -> ExtractedMsdfAtlas {
        let mut regions = HashMap::new();
        regions.insert(key, region);
        ExtractedMsdfAtlas {
            regions,
            texture: Handle::default(),
            width: 256,
            height: 256,
            msdf_range: 4.0,
            version: 1,
        }
    }

    // ---- snap_baseline_phys ---------------------------------------------

    /// The formula, spelled out at the three scales that matter. 1.5x is the
    /// case that motivated snapping in physical rather than logical space:
    /// a logical-integer baseline (20.0) is *not* a physical scanline there
    /// (30.0 happens to be, but 20.5 → 30.75 → 31 is the general case).
    #[test]
    fn snap_baseline_phys_rounds_the_scrolled_baseline_to_a_physical_row() {
        // 1x: an integer logical baseline is already a physical row.
        assert_eq!(snap_baseline_phys(20.0, 0.0, 1.0), 20.0);
        assert_eq!(snap_baseline_phys(20.4, 0.0, 1.0), 20.0);
        assert_eq!(snap_baseline_phys(20.6, 0.0, 1.0), 21.0);

        // 1.5x: (20.5 - 0) * 1.5 = 30.75 → 31.
        assert_eq!(snap_baseline_phys(20.5, 0.0, 1.5), 31.0);
        // The scroll offset is subtracted BEFORE the snap — a fractional
        // scroll must be able to move a glyph to a different physical row,
        // which is exactly what makes fine scrolling smooth.
        assert_eq!(snap_baseline_phys(20.5, 0.1, 1.5), 31.0); // 20.4*1.5=30.6→31
        assert_eq!(snap_baseline_phys(20.5, 0.5, 1.5), 30.0); // 20.0*1.5=30.0

        // 2x: half-logical-pixel positions are exact physical rows.
        assert_eq!(snap_baseline_phys(20.25, 0.0, 2.0), 41.0); // 40.5 → 41 (banker's-free round)
        assert_eq!(snap_baseline_phys(20.5, 0.0, 2.0), 41.0);
    }

    /// Scrolling by a whole physical pixel must move the snapped baseline by
    /// exactly that pixel — no drift, no doubled step. This is the property
    /// that keeps text from swimming as the offset eases.
    #[test]
    fn snap_baseline_phys_tracks_scroll_one_physical_pixel_at_a_time() {
        let scale = 1.25;
        let doc_y = 137.3;
        let base = snap_baseline_phys(doc_y, 0.0, scale);
        for step in 1..8 {
            // One physical pixel of scroll = 1/scale logical.
            let scroll = step as f32 / scale;
            let snapped = snap_baseline_phys(doc_y, scroll, scale);
            assert_eq!(
                snapped,
                base - step as f32,
                "scrolling {step} physical px moved the baseline to {snapped}, expected {}",
                base - step as f32,
            );
        }
    }

    /// The whole invariant in one assertion: two document positions that
    /// differ only by the scroll offset land on the same physical row.
    #[test]
    fn snap_baseline_phys_depends_only_on_the_scrolled_distance() {
        assert_eq!(
            snap_baseline_phys(1000.37, 900.0, 1.5),
            snap_baseline_phys(100.37, 0.0, 1.5),
        );
    }

    // ---- build_instances -------------------------------------------------

    fn glyph(key: GlyphKey, x: f32, y: f32, font_size: f32) -> PositionedGlyph {
        PositionedGlyph {
            key,
            x,
            y,
            font_size,
            color: [200, 210, 220, 255],
            importance: 0.5,
        }
    }

    fn run(doc_y: f32, x_offset: f32, glyphs: Vec<PositionedGlyph>) -> SurfaceRun {
        SurfaceRun {
            doc_y,
            x_offset,
            glyphs: Arc::new(glyphs),
        }
    }

    /// Apply the WGSL vertex shader's math in Rust for one corner, so the
    /// instance path can be compared against `build_vertices` in the same
    /// units. Mirrors `msdf_surface.wgsl` — if that shader changes, this
    /// changes with it.
    fn instance_corner_phys(
        inst: &GlyphInstance,
        corner: (f32, f32),
        scroll: f32,
        scale: f32,
    ) -> (f32, f32) {
        let x = (inst.baseline_doc[0] + inst.quad_offset[0] + corner.0 * inst.quad_size[0]) * scale;
        let y_base = snap_baseline_phys(inst.baseline_doc[1], scroll, scale);
        let y = y_base + (inst.quad_offset[1] + corner.1 * inst.quad_size[1]) * scale;
        (x, y)
    }

    fn ndc_to_phys(ndc: [f32; 2], tex_w: f32, tex_h: f32) -> (f32, f32) {
        ((ndc[0] + 1.0) * tex_w / 2.0, (1.0 - ndc[1]) * tex_h / 2.0)
    }

    /// The port is exact: at scroll 0 an instance places its quad on exactly
    /// the physical pixels `MsdfBlockRenderer::build_vertices` places it on,
    /// snap included. This is the test that would catch an anchor sign flip,
    /// a missing `font_size / MSDF_PX_PER_EM`, or a snap moved to the wrong
    /// side of the scale multiply.
    #[test]
    fn instances_place_the_same_quad_as_build_vertices() {
        let key = GlyphKey::new(FontId::for_test(1), 42);
        let region = AtlasRegion {
            x: 12,
            y: 34,
            width: 48,
            height: 56,
            anchor_x: 0.1,
            anchor_y: 0.8,
        };
        let atlas = atlas_with_region(key, region);

        // The block sits at doc_y = 300.5 and is indented 24px; the glyph is
        // 7.25px into its chunk, on the chunk's second baseline.
        let g = glyph(key, 7.25, 21.79, 16.0);
        let runs = vec![run(300.5, 24.0, vec![g.clone()])];
        let mut instances = Vec::new();
        build_instances(&runs, &atlas, &mut instances);
        assert_eq!(instances.len(), 1);

        let scale = 1.25;
        let (tex_w, tex_h) = (1000.0, 800.0);

        // build_vertices works in a single flat coordinate space, so hand it
        // the same absolute position the run assembly produces.
        let flat = PositionedGlyph {
            x: 24.0 + g.x,
            y: 300.5 + g.y,
            ..g.clone()
        };
        let verts = MsdfBlockRenderer::build_vertices(
            std::slice::from_ref(&flat),
            &atlas,
            tex_w,
            tex_h,
            scale,
            false,
        );
        assert_eq!(verts.len(), 6);

        // Vertex 0 is the top-left corner, vertex 4 the bottom-right.
        let expect_tl = ndc_to_phys(verts[0].position, tex_w, tex_h);
        let expect_br = ndc_to_phys(verts[4].position, tex_w, tex_h);
        let got_tl = instance_corner_phys(&instances[0], (0.0, 0.0), 0.0, scale);
        let got_br = instance_corner_phys(&instances[0], (1.0, 1.0), 0.0, scale);

        assert!(
            (got_tl.0 - expect_tl.0).abs() < 1e-3 && (got_tl.1 - expect_tl.1).abs() < 1e-3,
            "top-left {got_tl:?} != build_vertices {expect_tl:?}",
        );
        assert!(
            (got_br.0 - expect_br.0).abs() < 1e-3 && (got_br.1 - expect_br.1).abs() < 1e-3,
            "bottom-right {got_br:?} != build_vertices {expect_br:?}",
        );

        // UVs are the region's rect, V-flipped at corner interpolation time
        // (the flip lives in the shader, so the instance carries the raw
        // rect — top-left samples v1, which is uv_rect[3]).
        assert_eq!(instances[0].uv_rect, region.uv_rect(atlas.width, atlas.height));
        assert_eq!(instances[0].color, g.color);
    }

    /// Scrolling by exactly the block's document offset must put the glyph
    /// where an unscrolled block at the top of the document would be — the
    /// document-space instance is scroll-independent by construction.
    #[test]
    fn scrolling_only_moves_the_quad_never_rebuilds_it() {
        let key = GlyphKey::new(FontId::for_test(2), 3);
        let region = AtlasRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 32,
            anchor_x: 0.0,
            anchor_y: 0.75,
        };
        let atlas = atlas_with_region(key, region);

        let mut deep = Vec::new();
        build_instances(&[run(5000.0, 0.0, vec![glyph(key, 3.5, 16.0, 16.0)])], &atlas, &mut deep);
        let mut top = Vec::new();
        build_instances(&[run(0.0, 0.0, vec![glyph(key, 3.5, 16.0, 16.0)])], &atlas, &mut top);

        let deep_tl = instance_corner_phys(&deep[0], (0.0, 0.0), 5000.0, 2.0);
        let top_tl = instance_corner_phys(&top[0], (0.0, 0.0), 0.0, 2.0);
        assert_eq!(deep_tl, top_tl);
    }

    /// A glyph the atlas hasn't generated yet is dropped, not drawn with
    /// garbage UVs — the existing `build_vertices` behavior.
    #[test]
    fn a_glyph_with_no_atlas_region_is_skipped() {
        let known = GlyphKey::new(FontId::for_test(3), 1);
        let unknown = GlyphKey::new(FontId::for_test(3), 2);
        let atlas = atlas_with_region(
            known,
            AtlasRegion {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
                anchor_x: 0.0,
                anchor_y: 0.5,
            },
        );

        let mut out = Vec::new();
        build_instances(
            &[run(
                0.0,
                0.0,
                vec![glyph(unknown, 0.0, 10.0, 16.0), glyph(known, 10.0, 10.0, 16.0)],
            )],
            &atlas,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].baseline_doc, [10.0, 10.0]);
    }

    /// `build_instances` is fed a reusable buffer; it must not accumulate
    /// across window rebuilds.
    #[test]
    fn build_instances_clears_the_output_buffer() {
        let key = GlyphKey::new(FontId::for_test(4), 9);
        let atlas = atlas_with_region(
            key,
            AtlasRegion {
                x: 0,
                y: 0,
                width: 8,
                height: 8,
                anchor_x: 0.0,
                anchor_y: 0.5,
            },
        );
        let mut out = Vec::new();
        build_instances(&[run(0.0, 0.0, vec![glyph(key, 0.0, 8.0, 16.0)])], &atlas, &mut out);
        build_instances(&[run(0.0, 0.0, vec![glyph(key, 0.0, 8.0, 16.0)])], &atlas, &mut out);
        assert_eq!(out.len(), 1);
    }

    // ---- layout contract -------------------------------------------------

    /// The instance stride and attribute offsets are a contract with the
    /// WGSL `@location`s; a field reorder that doesn't update
    /// `instance_layout` would silently read the wrong bytes on the GPU.
    #[test]
    fn instance_layout_matches_the_struct() {
        let layout = ConversationSurfaceRenderer::instance_layout();
        assert_eq!(layout.array_stride, 48);
        assert_eq!(layout.step_mode, VertexStepMode::Instance);
        let offsets: Vec<u64> = layout.attributes.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 16, 24, 40, 44]);
        assert_eq!(std::mem::size_of::<GlyphInstance>(), 48);
        // The uniform block must stay a multiple of 16 bytes, and the size
        // `encase` reports (which becomes the bind group layout's
        // `min_binding_size`) must equal what we actually upload with
        // `bytes_of` — and what naga computes for the WGSL struct, checked to
        // be 64 with the same naga version Bevy 0.19 ships.
        assert_eq!(std::mem::size_of::<SurfaceUniforms>(), 64);
        assert_eq!(std::mem::size_of::<SurfaceUniforms>() % 16, 0);
        assert_eq!(SurfaceUniforms::min_size().get(), 64);
    }

    // ---- geometry + quads ------------------------------------------------

    fn geom_vertex(x: f32, y: f32) -> crate::text::msdf::geometry::GeometryVertex {
        crate::text::msdf::geometry::GeometryVertex { x, y, color: [10, 20, 30, 255] }
    }

    /// The geometry builder folds the run's `x_offset` into each vertex's x
    /// and hangs the run's `doc_y` on every vertex as the SNAP ANCHOR, leaving
    /// the y a local offset. That split is the whole reason a 1.2px staff line
    /// keeps its width while scrolling: one rounding per run, not one per
    /// vertex.
    #[test]
    fn geometry_vertices_carry_a_shared_anchor_and_a_folded_x() {
        let runs = vec![
            SurfaceGeometryRun {
                doc_y: 100.0,
                x_offset: 12.0,
                vertices: Arc::new(vec![geom_vertex(0.0, 0.0), geom_vertex(4.0, 30.0)]),
            },
            SurfaceGeometryRun {
                doc_y: 200.0,
                x_offset: 0.0,
                vertices: Arc::new(vec![geom_vertex(1.0, 2.0)]),
            },
        ];
        let mut out = Vec::new();
        build_geometry_vertices(&runs, &mut out);

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].pos_doc, [12.0, 0.0]);
        assert_eq!(out[0].anchor_doc_y, 100.0);
        assert_eq!(out[1].pos_doc, [16.0, 30.0], "x folds the indent, y stays local");
        assert_eq!(
            out[1].anchor_doc_y, out[0].anchor_doc_y,
            "every vertex of a run shares the anchor, or the shape deforms on snap",
        );
        assert_eq!(out[2].anchor_doc_y, 200.0);
        assert_eq!(out[0].color, [10, 20, 30, 255]);
    }

    #[test]
    fn building_geometry_clears_the_previous_frames_vertices() {
        let mut out = vec![SurfaceGeometryVertex {
            pos_doc: [9.0, 9.0],
            anchor_doc_y: 9.0,
            color: [0; 4],
        }];
        build_geometry_vertices(&[], &mut out);
        assert!(out.is_empty());
    }

    /// The quad rects and their textures come out index-aligned — that
    /// pairing is what `encode_render` draws with (`draw(.., i..i+1)` under
    /// the i-th bind group), so a mismatch would put one block's picture in
    /// another block's rectangle.
    #[test]
    fn quad_instances_stay_index_aligned_with_their_textures() {
        let quads = vec![
            SurfaceQuad { doc_rect: [1.0, 2.0, 3.0, 4.0], image: Handle::default() },
            SurfaceQuad { doc_rect: [5.0, 6.0, 7.0, 8.0], image: Handle::default() },
        ];
        let (mut instances, mut images) = (Vec::new(), Vec::new());
        build_quad_instances(&quads, &mut instances, &mut images);
        assert_eq!(instances.len(), images.len());
        assert_eq!(instances[1].rect_doc, [5.0, 6.0, 7.0, 8.0]);

        build_quad_instances(&[], &mut instances, &mut images);
        assert!(instances.is_empty() && images.is_empty());
    }

    /// Stride and offsets for the two new pipelines, same contract as the
    /// glyph and chrome layouts above.
    #[test]
    fn geometry_and_quad_layouts_match_their_structs() {
        let geometry = ConversationSurfaceRenderer::geometry_vertex_layout();
        assert_eq!(geometry.array_stride, 16);
        assert_eq!(
            geometry.step_mode,
            VertexStepMode::Vertex,
            "geometry is a triangle list, not instanced — the draw call counts vertices",
        );
        let offsets: Vec<u64> = geometry.attributes.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 8, 12]);
        assert_eq!(std::mem::size_of::<SurfaceGeometryVertex>(), 16);

        let quad = ConversationSurfaceRenderer::quad_instance_layout();
        assert_eq!(quad.array_stride, 16);
        assert_eq!(quad.step_mode, VertexStepMode::Instance);
        assert_eq!(std::mem::size_of::<QuadInstance>(), 16);
    }

    /// Same contract for the chrome pipeline: the stride and offsets are what
    /// `surface_chrome.wgsl`'s `@location`s read, and `kind` is a `u32` —
    /// reading it as a float (or at the wrong offset) draws every border as
    /// the wrong kind, silently.
    #[test]
    fn chrome_instance_layout_matches_the_struct() {
        let layout = ConversationSurfaceRenderer::chrome_instance_layout();
        assert_eq!(layout.array_stride, 56);
        assert_eq!(layout.step_mode, VertexStepMode::Instance);
        let offsets: Vec<u64> = layout.attributes.iter().map(|a| a.offset).collect();
        assert_eq!(offsets, vec![0, 16, 32, 40, 48, 52]);
        assert_eq!(std::mem::size_of::<ChromeInstance>(), 56);
        assert_eq!(
            layout.attributes[5].format,
            VertexFormat::Uint32,
            "the border kind is an integer on both sides",
        );
    }

    /// The document origin lives in the two words the uniform used to pad
    /// with, so the block stays 64 bytes. A `vec2` in WGSL needs 8-byte
    /// alignment, and offset 56 is the last place one fits.
    #[test]
    fn the_document_origin_occupies_the_uniforms_trailing_pad() {
        let uniforms = SurfaceUniforms {
            origin_logical: [4.0, 4.0],
            ..SurfaceUniforms::default()
        };
        let bytes = bytemuck::bytes_of(&uniforms);
        assert_eq!(bytes.len(), 64);
        assert_eq!(&bytes[56..64], bytemuck::bytes_of(&[4.0_f32, 4.0_f32]));
    }
}
