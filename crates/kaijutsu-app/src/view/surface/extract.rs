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
    ConversationSurfaceRenderer, GlyphInstance, SurfaceUniforms, build_instances,
};
use crate::ui::theme::Theme;
use crate::view::block_render::ExtractedMsdfRenderParams;
use crate::view::geometry::ConversationGeometry;
use crate::view::ui_rtt::UiRttTexture;

use super::shape_cache::{HeaderLabelCache, ShapedBlockCache, SurfaceMetricsEpoch};
use super::window::{SurfaceRun, assemble_runs};
use super::{ConversationRenderPath, ConversationSurface};

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

struct SurfaceInstanceBuffer {
    /// `None` until the surface first has something to draw.
    buffer: Option<Buffer>,
    /// Instances the allocation can hold.
    capacity: usize,
    /// Instances actually written.
    len: u32,
    key: InstanceBufferKey,
}

/// Persistent per-surface instance buffers, owned by the render world.
#[derive(Resource, Default)]
pub struct SurfaceGpuBuffers {
    entries: HashMap<Entity, SurfaceInstanceBuffer>,
}

impl SurfaceGpuBuffers {
    /// Whether `surface`'s buffer already holds instances built from `key` —
    /// the question extraction asks before assembling anything.
    pub fn is_current(&self, surface: Entity, key: InstanceBufferKey) -> bool {
        self.entries
            .get(&surface)
            .is_some_and(|entry| entry.key == key)
    }

    /// The buffer + instance count to draw, if there is anything to draw.
    pub fn draw_range(&self, surface: Entity) -> Option<(&Buffer, u32)> {
        let entry = self.entries.get(&surface)?;
        let buffer = entry.buffer.as_ref()?;
        if entry.len == 0 {
            return None;
        }
        Some((buffer, entry.len))
    }

    /// Instances currently uploaded for a surface (0 when it has never been
    /// built). Exposed for tests and diagnostics.
    pub fn len(&self, surface: Entity) -> u32 {
        self.entries.get(&surface).map_or(0, |e| e.len)
    }

    /// Allocated capacity in instances (0 when never allocated).
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
        let entry = self
            .entries
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
                label: Some("conversation_surface_instances"),
                size: (capacity * std::mem::size_of::<GlyphInstance>()) as u64,
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

    /// Drop buffers for surfaces that no longer exist (pane closed, split
    /// rebuilt). Called once per frame from the render system with the set of
    /// surfaces that extracted.
    pub fn retain_surfaces(&mut self, live: &HashSet<Entity>) {
        self.entries.retain(|entity, _| live.contains(entity));
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
    /// Opaque background the pass clears to (see `target.rs` for why opaque,
    /// and why it is *linear*).
    pub clear: LinearRgba,
    /// `Some` when the instance buffer must be rebuilt from these runs;
    /// `None` when the GPU already holds instances built from `key`.
    pub rebuild: Option<Vec<SurfaceRun>>,
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
    path: Extract<Res<ConversationRenderPath>>,
    surfaces: Extract<Query<(Entity, &ConversationSurface, &UiRttTexture)>>,
    geometries: Extract<Query<&ConversationGeometry>>,
    entities: Extract<Res<EditorEntities>>,
    shaped: Extract<Res<ShapedBlockCache>>,
    headers: Extract<Res<HeaderLabelCache>>,
    metrics_epoch: Extract<Res<SurfaceMetricsEpoch>>,
    scroll: Extract<Res<ConversationScrollState>>,
    atlas: Extract<Option<Res<MsdfAtlas>>>,
    theme: Extract<Res<Theme>>,
) {
    extracted.items.clear();

    if **path != ConversationRenderPath::Surface {
        return;
    }

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

    for (entity, surface, rtt) in surfaces.iter() {
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
        let rebuild = if buffers.is_current(entity, key) {
            None
        } else {
            Some(assemble_runs(
                geom,
                &shaped,
                &headers,
                metrics_epoch,
                surface.window.row_range.clone(),
            ))
        };

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
            clear,
            rebuild,
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
            _pad0: 0.0,
            _pad1: 0.0,
        };

        let result = renderer.encode_render(
            &device,
            &mut encoder,
            &pipeline_cache,
            &gpu_images,
            &atlas.texture,
            &item.target,
            buffers.draw_range(item.surface),
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
