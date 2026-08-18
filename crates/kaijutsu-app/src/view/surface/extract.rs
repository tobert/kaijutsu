//! Render-world half of the conversation surface: extraction, persistent
//! instance buffers, and the draw.
//!
//! # Why the scroll offset is read *here*
//!
//! `ConversationScrollState.offset` is read in `ExtractSchedule` and nowhere
//! else. With pipelined rendering the render world runs a frame behind the
//! main world, so anything baked into main-world data during `Update` is one
//! frame stale by the time it reaches the GPU — for a scroll offset that is
//! visible judder, and it is the documented failure mode this rewrite exists
//! to avoid. Extraction is the last moment both worlds are synchronized, so
//! the offset is picked up there and travels to the shader as a uniform.
//!
//! # What is *not* rebuilt per frame
//!
//! Instance buffers are keyed by [`InstanceBufferKey`] — the surface's
//! `buffer_version` plus the atlas version — and rebuilt only when it moves.
//! `window::build_surface_window` bumps `buffer_version` only when the scroll
//! offset leaves the built band or a discrete `WindowKey` token changes, so
//! ordinary scrolling reaches the GPU as a 64-byte uniform write and nothing
//! else. Extraction consults the render-world buffer state before it does any
//! work at all: when the key is current it does not even assemble the runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy::color::LinearRgba;
use bevy::prelude::*;
use bevy::render::{
    Extract,
    render_asset::RenderAssets,
    render_resource::{
        Buffer, BufferDescriptor, BufferUsages, CommandEncoder, CommandEncoderDescriptor,
        PipelineCache,
    },
    renderer::{RenderDevice, RenderQueue},
    texture::GpuImage,
};

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::text::msdf::MsdfAtlas;
use crate::text::msdf::renderer::ExtractedMsdfAtlas;
use crate::text::msdf::surface_renderer::{
    ConversationSurfaceRenderer, GlyphInstance, QuadInstance, SurfaceGeometryVertex,
    SurfaceUniforms, build_geometry_vertices, build_instances, build_quad_instances,
};
use crate::view::surface::chrome::{BlockChromeCache, ChromeInstance, ChromeInstances};
use crate::ui::theme::Theme;
use crate::view::block_render::ExtractedMsdfRenderParams;
use crate::view::geometry::ConversationGeometry;
use crate::view::ui_rtt::UiRttTexture;

use super::rich::SvgRasterCache;
use super::shape_cache::{HeaderLabelCache, ShapedBlockCache, SurfaceMetricsEpoch};
use super::window::{
    SurfaceGeometryRun, SurfaceQuad, SurfaceRun, assemble_geometry, assemble_quads, assemble_runs,
};
use super::ConversationSurface;

/// Instances a fresh buffer starts at (≈48 KB) — enough for a screenful of
/// text without a single growth step, small enough to be uninteresting when a
/// pane exists but is empty.
pub const MIN_INSTANCE_CAPACITY: usize = 1024;

/// What an instance buffer's contents were built from.
///
/// `buffer_version` alone would very nearly do — the window's `WindowKey`
/// already includes the atlas version, so an atlas repack bumps it. The atlas
/// version rides along anyway because it is the one input whose staleness is
/// *invisible*: every UV in the buffer would still be a plausible number,
/// just pointing at the wrong pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InstanceBufferKey {
    pub buffer_version: u64,
    pub atlas_version: u64,
}

/// What a chrome buffer's contents were built from.
///
/// Its own key, and its own version: chrome changes on focus moves and status
/// pulses that touch no glyph, and glyph windows rebuild on scrolls that
/// touch no border. Sharing one version would make each pay the other's
/// uploads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChromeBufferKey {
    pub chrome_version: u64,
}

/// Capacity (in instances) to allocate when `needed` no longer fits.
///
/// Doubling from the current capacity, never shrinking: a conversation that
/// scrolled through one big block would otherwise realloc on the way back
/// out. A buffer is only ever freed when its surface goes away
/// ([`SurfaceGpuBuffers::retain_surfaces`]).
pub fn grow_capacity(current: usize, needed: usize) -> usize {
    let mut capacity = current.max(MIN_INSTANCE_CAPACITY);
    while capacity < needed {
        capacity *= 2;
    }
    capacity
}

struct SurfaceInstanceBuffer<K> {
    /// `None` until the surface first has something to draw.
    buffer: Option<Buffer>,
    /// Instances the allocation can hold.
    capacity: usize,
    /// Instances actually written.
    len: u32,
    key: K,
}

/// Write `instances` into a surface's entry in `map`, growing (never
/// shrinking) the allocation first if they don't fit.
///
/// Generic over the instance type because the glyph and chrome buffers differ
/// in nothing but stride and label — and over the key type because they are
/// versioned independently.
fn upload_into<T: bytemuck::Pod, K: Copy>(
    map: &mut HashMap<Entity, SurfaceInstanceBuffer<K>>,
    device: &RenderDevice,
    queue: &RenderQueue,
    surface: Entity,
    key: K,
    instances: &[T],
    label: &'static str,
) {
    let entry = map
        .entry(surface)
        .or_insert_with(|| SurfaceInstanceBuffer {
            buffer: None,
            capacity: 0,
            len: 0,
            key,
        });
    entry.key = key;
    entry.len = instances.len() as u32;

    if instances.is_empty() {
        // An empty window is a legitimate state (nothing shaped yet); the
        // key is still recorded so it isn't re-assembled every frame.
        return;
    }

    if entry.buffer.is_none() || entry.capacity < instances.len() {
        let capacity = grow_capacity(entry.capacity, instances.len());
        entry.buffer = Some(device.create_buffer(&BufferDescriptor {
            label: Some(label),
            size: (capacity * std::mem::size_of::<T>()) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        entry.capacity = capacity;
    }

    let buffer = entry
        .buffer
        .as_ref()
        .expect("instance buffer was just allocated");
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(instances));
}

/// The buffer + instance count to draw out of one map, if there is anything.
fn draw_range_of<K>(
    map: &HashMap<Entity, SurfaceInstanceBuffer<K>>,
    surface: Entity,
) -> Option<(&Buffer, u32)> {
    let entry = map.get(&surface)?;
    let buffer = entry.buffer.as_ref()?;
    if entry.len == 0 {
        return None;
    }
    Some((buffer, entry.len))
}

/// Persistent per-surface instance buffers, owned by the render world.
///
/// The geometry buffer shares the glyphs' [`InstanceBufferKey`] because it
/// shares their trigger: both are assembled from the window's rows, so they
/// go stale together and re-uploading one without the other would draw a
/// staff under last window's notes.
#[derive(Resource, Default)]
pub struct SurfaceGpuBuffers {
    entries: HashMap<Entity, SurfaceInstanceBuffer<InstanceBufferKey>>,
    chrome: HashMap<Entity, SurfaceInstanceBuffer<ChromeBufferKey>>,
    geometry: HashMap<Entity, SurfaceInstanceBuffer<InstanceBufferKey>>,
    quads: HashMap<Entity, SurfaceInstanceBuffer<InstanceBufferKey>>,
}

impl SurfaceGpuBuffers {
    /// Whether `surface`'s buffer already holds instances built from `key` —
    /// the question extraction asks before assembling anything.
    pub fn is_current(&self, surface: Entity, key: InstanceBufferKey) -> bool {
        self.entries
            .get(&surface)
            .is_some_and(|entry| entry.key == key)
    }

    /// Whether `surface`'s chrome buffer already holds `key`'s instances.
    pub fn chrome_is_current(&self, surface: Entity, key: ChromeBufferKey) -> bool {
        self.chrome
            .get(&surface)
            .is_some_and(|entry| entry.key == key)
    }

    /// The buffer + instance count to draw, if there is anything to draw.
    pub fn draw_range(&self, surface: Entity) -> Option<(&Buffer, u32)> {
        draw_range_of(&self.entries, surface)
    }

    /// The chrome buffer + instance count to draw.
    pub fn chrome_draw_range(&self, surface: Entity) -> Option<(&Buffer, u32)> {
        draw_range_of(&self.chrome, surface)
    }

    /// The geometry buffer + **vertex** count to draw (this one is
    /// vertex-stepped, not instanced).
    pub fn geometry_draw_range(&self, surface: Entity) -> Option<(&Buffer, u32)> {
        draw_range_of(&self.geometry, surface)
    }

    /// The quad instance buffer. The count comes from the extracted texture
    /// list instead, because a quad whose texture isn't on the GPU yet is
    /// skipped individually.
    pub fn quad_buffer(&self, surface: Entity) -> Option<&Buffer> {
        self.quads.get(&surface)?.buffer.as_ref()
    }

    /// Instances currently uploaded for a surface (0 when it has never been
    /// built). Exposed for tests and diagnostics.
    pub fn len(&self, surface: Entity) -> u32 {
        self.entries.get(&surface).map_or(0, |e| e.len)
    }

    /// Allocated capacity in instances (0 when never allocated).
    #[allow(dead_code)] // Buffer-growth accessor for tests and diagnostics.
    pub fn capacity(&self, surface: Entity) -> usize {
        self.entries.get(&surface).map_or(0, |e| e.capacity)
    }

    /// Write `instances` into `surface`'s buffer, growing (never shrinking)
    /// the allocation first if they don't fit, and record `key` as what the
    /// contents were built from.
    pub fn upload(
        &mut self,
        device: &RenderDevice,
        queue: &RenderQueue,
        surface: Entity,
        key: InstanceBufferKey,
        instances: &[GlyphInstance],
    ) {
        upload_into(
            &mut self.entries,
            device,
            queue,
            surface,
            key,
            instances,
            "conversation_surface_instances",
        );
    }

    /// Write `instances` into `surface`'s chrome buffer. Same capacity
    /// doubling, separate allocation.
    pub fn upload_chrome(
        &mut self,
        device: &RenderDevice,
        queue: &RenderQueue,
        surface: Entity,
        key: ChromeBufferKey,
        instances: &[ChromeInstance],
    ) {
        upload_into(
            &mut self.chrome,
            device,
            queue,
            surface,
            key,
            instances,
            "conversation_surface_chrome",
        );
    }

    /// Write `vertices` into `surface`'s geometry buffer.
    pub fn upload_geometry(
        &mut self,
        device: &RenderDevice,
        queue: &RenderQueue,
        surface: Entity,
        key: InstanceBufferKey,
        vertices: &[SurfaceGeometryVertex],
    ) {
        upload_into(
            &mut self.geometry,
            device,
            queue,
            surface,
            key,
            vertices,
            "conversation_surface_geometry",
        );
    }

    /// Write `instances` into `surface`'s textured-quad buffer.
    pub fn upload_quads(
        &mut self,
        device: &RenderDevice,
        queue: &RenderQueue,
        surface: Entity,
        key: InstanceBufferKey,
        instances: &[QuadInstance],
    ) {
        upload_into(
            &mut self.quads,
            device,
            queue,
            surface,
            key,
            instances,
            "conversation_surface_quads",
        );
    }

    /// Drop buffers for surfaces that no longer exist (pane closed, split
    /// rebuilt). Called once per frame from the render system with the set of
    /// surfaces that extracted.
    pub fn retain_surfaces(&mut self, live: &HashSet<Entity>) {
        self.entries.retain(|entity, _| live.contains(entity));
        self.chrome.retain(|entity, _| live.contains(entity));
        self.geometry.retain(|entity, _| live.contains(entity));
        self.quads.retain(|entity, _| live.contains(entity));
    }
}

/// One surface's worth of everything the draw needs.
pub struct ExtractedSurface {
    /// The `ConversationSurface` entity — the key its GPU buffer lives under.
    pub surface: Entity,
    /// The RTT this pane draws into.
    pub target: Handle<Image>,
    /// Render-target size in PHYSICAL pixels.
    pub viewport_phys: [f32; 2],
    /// This surface's logical → physical scale.
    pub scale: f32,
    /// Scroll offset in logical document px, read in `ExtractSchedule`.
    pub scroll_offset: f32,
    pub key: InstanceBufferKey,
    pub chrome_key: ChromeBufferKey,
    /// Where document (0,0) sits in the target, logical px — the pane's
    /// padding (see [`SurfaceUniforms::origin_logical`]).
    pub origin_logical: [f32; 2],
    /// Opaque background the pass clears to (see `target.rs` for why opaque,
    /// and why it is *linear*).
    pub clear: LinearRgba,
    /// `Some` when the instance buffer must be rebuilt from these runs;
    /// `None` when the GPU already holds instances built from `key`.
    pub rebuild: Option<Vec<SurfaceRun>>,
    /// Geometry runs for the same window, rebuilt under the same condition —
    /// they share `key` because they share the window that produced them.
    pub geometry_rebuild: Option<Vec<SurfaceGeometryRun>>,
    /// Textured quads (SVG rasters) in the window — assembled **every frame**,
    /// unlike the two buffers above.
    ///
    /// Not because they change every frame, but because the draw needs their
    /// `Handle<Image>`s every frame (a texture is bound per quad, and handles
    /// are not `Pod` so they cannot live in the buffer). Carrying the rects
    /// alongside them costs nothing next to that, and skipping the version
    /// dance removes the one way a texture list and an instance buffer could
    /// disagree about which rect belongs to which picture. `assemble_quads`
    /// returns immediately when no block has a raster, which is the usual
    /// case.
    pub quads: Vec<SurfaceQuad>,
    /// `Some` when the chrome buffer must be re-uploaded. Already built (the
    /// main world assembles chrome instances directly), so this is a pointer,
    /// not a copy.
    pub chrome_rebuild: Option<Arc<Vec<ChromeInstance>>>,
}

/// Everything extracted for this frame's surfaces.
#[derive(Resource, Default)]
pub struct ExtractedConversationSurfaces {
    pub items: Vec<ExtractedSurface>,
}

/// Extract each pane's surface: target, viewport, scroll offset, and (only
/// when the window changed) the document-space runs to upload.
#[allow(clippy::too_many_arguments)]
pub fn extract_conversation_surfaces(
    mut extracted: ResMut<ExtractedConversationSurfaces>,
    buffers: Res<SurfaceGpuBuffers>,
    surfaces: Extract<Query<(Entity, &ConversationSurface, &ChromeInstances, &UiRttTexture)>>,
    computed_nodes: Extract<Query<&ComputedNode>>,
    geometries: Extract<Query<&ConversationGeometry>>,
    entities: Extract<Res<EditorEntities>>,
    shaped: Extract<Res<ShapedBlockCache>>,
    rasters: Extract<Res<SvgRasterCache>>,
    headers: Extract<Res<HeaderLabelCache>>,
    // Border captions are placed against the border box, so assembly needs
    // the layouts chrome computed. Read-only here — it is `sync_block_chrome`
    // that writes them, a whole schedule earlier.
    chrome_cache: Extract<Res<BlockChromeCache>>,
    metrics_epoch: Extract<Res<SurfaceMetricsEpoch>>,
    scroll: Extract<Res<ConversationScrollState>>,
    atlas: Extract<Option<Res<MsdfAtlas>>>,
    theme: Extract<Res<Theme>>,
) {
    extracted.items.clear();

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };

    let atlas_version = atlas.as_ref().map(|a| a.version).unwrap_or(0);
    let metrics_epoch = metrics_epoch.get();
    // Alpha forced opaque: the composite is a plain `ImageNode`, and an
    // opaque texture is what makes premultiplied-vs-straight moot (target.rs).
    // LINEAR, not sRGB — a clear on a non-sRGB `Rgba8Unorm` target stores the
    // value verbatim, and Bevy's UI pipeline then treats the sampled texel as
    // linear on the way to the screen. Feeding it `to_srgba()` would paint the
    // pane a visibly lighter shade than the camera clear (`ClearColorConfig`
    // takes the same `Theme::bg` through Bevy's own linear conversion), which
    // is exactly the seam a viewport-sized surface must not show.
    let clear = LinearRgba {
        alpha: 1.0,
        ..theme.bg.to_linear()
    };

    for (entity, surface, chrome, rtt) in surfaces.iter() {
        // A texture that has never been allocated has nothing to draw into;
        // `resize_surface_targets` gets to it on the frame layout produces a
        // size, and this picks it up the frame after.
        if rtt.width == 0 || rtt.height == 0 || rtt.built_width <= 0.0 {
            continue;
        }

        let key = InstanceBufferKey {
            buffer_version: surface.buffer_version,
            atlas_version,
        };
        let (rebuild, geometry_rebuild) = if buffers.is_current(entity, key) {
            (None, None)
        } else {
            (
                Some(assemble_runs(
                    geom,
                    &shaped,
                    &headers,
                    &chrome_cache,
                    &theme,
                    metrics_epoch,
                    surface.window.row_range.clone(),
                )),
                Some(assemble_geometry(
                    geom,
                    &shaped,
                    surface.window.row_range.clone(),
                )),
            )
        };
        let quads = assemble_quads(geom, &shaped, &rasters, surface.window.row_range.clone());

        let chrome_key = ChromeBufferKey {
            chrome_version: chrome.version,
        };
        let chrome_rebuild = if buffers.chrome_is_current(entity, chrome_key) {
            None
        } else {
            Some(chrome.instances.clone())
        };

        // The composite node is inset to the pane's padding box, so document
        // (0,0) — measured from the CONTENT box — sits one padding in. A pane
        // that hasn't been laid out contributes zero, which is the same
        // origin slice 1 drew at.
        let origin_logical = computed_nodes
            .get(surface.pane)
            .map(|node| {
                // `ComputedNode::padding` is physical px, as `min_inset` /
                // `max_inset` from the border box's corners.
                let inv = node.inverse_scale_factor();
                (node.padding.min_inset * inv).to_array()
            })
            .unwrap_or([0.0, 0.0]);

        extracted.items.push(ExtractedSurface {
            surface: entity,
            target: rtt.image.clone(),
            viewport_phys: [rtt.width as f32, rtt.height as f32],
            // The texture is `built_* * scale` exactly (target.rs rounds so
            // `ui_rtt_texture_dims`' ceil is a no-op), so this recovers the
            // scale factor without assuming the window's — the same
            // per-surface rule `msdf_item_scale` follows for the block pass.
            scale: rtt.width as f32 / rtt.built_width,
            scroll_offset: scroll.offset,
            key,
            chrome_key,
            origin_logical,
            clear,
            rebuild,
            geometry_rebuild,
            quads,
            chrome_rebuild,
        });
    }
}

/// Draw every surface: one encoder for the frame, one pass per pane, in the
/// shape of `render_msdf_block_textures`.
#[allow(clippy::too_many_arguments)]
pub fn render_conversation_surfaces(
    renderer: Option<Res<ConversationSurfaceRenderer>>,
    atlas: Res<ExtractedMsdfAtlas>,
    params: Res<ExtractedMsdfRenderParams>,
    extracted: Res<ExtractedConversationSurfaces>,
    mut buffers: ResMut<SurfaceGpuBuffers>,
    mut instances: Local<Vec<GlyphInstance>>,
    mut geometry: Local<Vec<SurfaceGeometryVertex>>,
    mut quad_instances: Local<Vec<QuadInstance>>,
    mut quad_images: Local<Vec<Handle<Image>>>,
    mut skips: Local<HashMap<Entity, u32>>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    pipeline_cache: Res<PipelineCache>,
) {
    let Some(renderer) = renderer else {
        return;
    };
    if extracted.items.is_empty() {
        return;
    }

    let live: HashSet<Entity> = extracted.items.iter().map(|item| item.surface).collect();
    buffers.retain_surfaces(&live);
    skips.retain(|entity, _| live.contains(entity));

    let mut encoder: CommandEncoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("conversation_surface_encoder"),
    });
    let mut encoded_any = false;

    for item in &extracted.items {
        if let Some(runs) = &item.rebuild {
            build_instances(runs, &atlas, &mut instances);
            buffers.upload(&device, &queue, item.surface, item.key, &instances);
        }
        if let Some(runs) = &item.geometry_rebuild {
            build_geometry_vertices(runs, &mut geometry);
            buffers.upload_geometry(&device, &queue, item.surface, item.key, &geometry);
        }
        if let Some(chrome) = &item.chrome_rebuild {
            buffers.upload_chrome(&device, &queue, item.surface, item.chrome_key, chrome);
        }
        build_quad_instances(&item.quads, &mut quad_instances, &mut quad_images);
        buffers.upload_quads(&device, &queue, item.surface, item.key, &quad_instances);

        let uniforms = SurfaceUniforms {
            viewport_phys: item.viewport_phys,
            scroll_offset: item.scroll_offset,
            scale: item.scale,
            sdf_texel: [1.0 / atlas.width as f32, 1.0 / atlas.height as f32],
            msdf_range: atlas.msdf_range,
            hint_amount: params.hint_amount,
            stem_darkening: params.stem_darkening,
            horz_scale: params.horz_scale,
            vert_scale: params.vert_scale,
            text_bias: params.text_bias,
            gamma_correction: params.gamma_correction,
            time: params.time,
            origin_logical: item.origin_logical,
        };

        let result = renderer.encode_render(
            &device,
            &mut encoder,
            &pipeline_cache,
            &gpu_images,
            &atlas.texture,
            &item.target,
            buffers.draw_range(item.surface),
            buffers.chrome_draw_range(item.surface),
            buffers.geometry_draw_range(item.surface),
            buffers
                .quad_buffer(item.surface)
                .map(|buffer| (buffer, quad_images.as_slice())),
            &uniforms,
            item.clear,
        );

        if result.cleared {
            encoded_any = true;
            skips.remove(&item.surface);
            continue;
        }

        // A frame or two of "target not prepared" is the known-benign asset
        // pipeline lag (see render_msdf_block_textures); a surface that never
        // paints is a bug and must say so rather than sit dark.
        let attempts = skips.entry(item.surface).or_insert(0);
        *attempts += 1;
        let msg = format!(
            "conversation surface pass skipped (attempt {}): {}x{} target_gpu={} \
             pipeline={} atlas_gpu={} instances={}",
            attempts,
            item.viewport_phys[0],
            item.viewport_phys[1],
            gpu_images.get(&item.target).is_some(),
            pipeline_cache
                .get_render_pipeline(renderer.pipeline)
                .is_some(),
            gpu_images.get(&atlas.texture).is_some(),
            buffers.len(item.surface),
        );
        if *attempts > 2 {
            warn!("{msg}");
        } else {
            debug!("{msg}");
        }
    }

    if encoded_any {
        queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- grow_capacity ---------------------------------------------------

    #[test]
    fn a_fresh_buffer_starts_at_the_minimum_capacity() {
        assert_eq!(grow_capacity(0, 1), MIN_INSTANCE_CAPACITY);
        assert_eq!(grow_capacity(0, MIN_INSTANCE_CAPACITY), MIN_INSTANCE_CAPACITY);
    }

    #[test]
    fn capacity_doubles_until_it_fits() {
        assert_eq!(grow_capacity(0, MIN_INSTANCE_CAPACITY + 1), 2048);
        assert_eq!(grow_capacity(2048, 2049), 4096);
        assert_eq!(grow_capacity(1024, 10_000), 16_384);
    }

    #[test]
    fn capacity_never_shrinks() {
        assert_eq!(grow_capacity(8192, 3), 8192);
    }

    // ---- rebuild gating --------------------------------------------------

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).unwrap()
    }

    fn seeded(surface: Entity, key: InstanceBufferKey, len: u32) -> SurfaceGpuBuffers {
        let mut buffers = SurfaceGpuBuffers::default();
        // Seeded without a GPU: the gating logic under test is exactly the
        // bookkeeping around the allocation, not the allocation itself.
        buffers.entries.insert(
            surface,
            SurfaceInstanceBuffer {
                buffer: None,
                capacity: MIN_INSTANCE_CAPACITY,
                len,
                key,
            },
        );
        buffers
    }

    /// The invariant this whole module exists for: an unchanged window must
    /// not re-upload. Extraction asks `is_current` *before* assembling runs,
    /// so a `true` here is also the fast path that skips the CPU walk.
    #[test]
    fn the_same_buffer_version_is_already_current() {
        let e = entity(1);
        let key = InstanceBufferKey {
            buffer_version: 7,
            atlas_version: 3,
        };
        let buffers = seeded(e, key, 500);
        assert!(buffers.is_current(e, key));
    }

    #[test]
    fn a_bumped_buffer_version_forces_a_rebuild() {
        let e = entity(1);
        let key = InstanceBufferKey {
            buffer_version: 7,
            atlas_version: 3,
        };
        let buffers = seeded(e, key, 500);
        assert!(!buffers.is_current(
            e,
            InstanceBufferKey {
                buffer_version: 8,
                atlas_version: 3
            }
        ));
    }

    /// An atlas repack moves every UV without moving a glyph — the buffer's
    /// contents are silently wrong, so the key must catch it.
    #[test]
    fn an_atlas_version_bump_forces_a_rebuild() {
        let e = entity(1);
        let key = InstanceBufferKey {
            buffer_version: 7,
            atlas_version: 3,
        };
        let buffers = seeded(e, key, 500);
        assert!(!buffers.is_current(
            e,
            InstanceBufferKey {
                buffer_version: 7,
                atlas_version: 4
            }
        ));
    }

    #[test]
    fn an_unknown_surface_is_never_current() {
        let buffers = SurfaceGpuBuffers::default();
        assert!(!buffers.is_current(entity(9), InstanceBufferKey::default()));
        assert_eq!(buffers.len(entity(9)), 0);
        assert_eq!(buffers.capacity(entity(9)), 0);
    }

    /// Nothing to draw is not the same as nothing uploaded: a surface whose
    /// window came out empty still records its key (so it isn't re-assembled
    /// every frame) but must not be handed to `draw`.
    #[test]
    fn an_empty_upload_records_the_key_but_draws_nothing() {
        let e = entity(2);
        let key = InstanceBufferKey {
            buffer_version: 1,
            atlas_version: 1,
        };
        let buffers = seeded(e, key, 0);
        assert!(buffers.is_current(e, key));
        assert!(buffers.draw_range(e).is_none());
    }

    #[test]
    fn buffers_for_vanished_surfaces_are_dropped() {
        let live_entity = entity(3);
        let dead_entity = entity(4);
        let mut buffers = seeded(live_entity, InstanceBufferKey::default(), 10);
        buffers.entries.insert(
            dead_entity,
            SurfaceInstanceBuffer {
                buffer: None,
                capacity: 0,
                len: 0,
                key: InstanceBufferKey::default(),
            },
        );

        let live: HashSet<Entity> = [live_entity].into_iter().collect();
        buffers.retain_surfaces(&live);

        assert!(buffers.entries.contains_key(&live_entity));
        assert!(!buffers.entries.contains_key(&dead_entity));
    }
}
