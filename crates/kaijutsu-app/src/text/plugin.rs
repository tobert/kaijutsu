//! Text rendering plugin for Bevy.
//!
//! Sets up glyphon text rendering integrated with Bevy's render pipeline.

use bevy::prelude::*;
use bevy::render::{
    render_graph::{RenderGraphExt, ViewNodeRunner},
    renderer::{RenderDevice, RenderQueue},
    Extract, Render, RenderApp,
};
use bevy::ui::{ComputedNode, UiGlobalTransform, UiSystems};
use bevy::window::PrimaryWindow;
use glyphon::{Cache, Resolution, TextAtlas, TextRenderer, Viewport};

use super::render::TextRenderNode;
use super::resources::*;

/// Plugin that enables glyphon text rendering in Bevy.
pub struct TextRenderPlugin;

impl Plugin for TextRenderPlugin {
    fn build(&self, app: &mut App) {
        // Main world resources
        app.init_resource::<SharedFontSystem>()
            .init_resource::<SharedSwashCache>()
            .init_resource::<TextResolution>()
            .add_systems(Update, update_text_resolution)
            // Sync UI text positions after Bevy UI layout computes positions
            .add_systems(PostUpdate, sync_ui_text_positions.after(UiSystems::Layout));

        // Render world setup
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .add_systems(ExtractSchedule, extract_text_areas)
            .add_systems(Render, prepare_text);

        // Add render node to the graph - after Upscaling (final post-processing step)
        //
        // Why after Upscaling instead of EndMainPass?
        // The Core2d render graph runs: MainPass -> EndMainPass -> Tonemapping -> Upscaling
        // If we render after EndMainPass, our text gets overwritten by post-processing.
        // By rendering after Upscaling, we draw directly to the final swap chain texture.
        // This also means we use MSAA=1 since the output is non-MSAA (see prepare_text).
        use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
        render_app
            .add_render_graph_node::<ViewNodeRunner<TextRenderNode>>(Core2d, TextRenderNode::NAME)
            .add_render_graph_edges(Core2d, (Node2d::Upscaling, TextRenderNode::NAME));
    }

    fn finish(&self, app: &mut App) {
        // Clone shared resources from main world (they're Arc<Mutex> so cloning is cheap)
        let font_system = app.world().resource::<SharedFontSystem>().clone();
        let swash_cache = app.world().resource::<SharedSwashCache>().clone();

        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        // Initialize with None - will be lazily created when we know the surface format
        render_app.insert_resource(RenderTextResources(None));
        render_app.insert_resource(RenderTextBufferCache::default());
        render_app.insert_resource(font_system);
        render_app.insert_resource(swash_cache);
    }
}

/// Wrapper for render-world text resources.
#[derive(Resource, Default)]
pub struct RenderTextResources(pub Option<TextRenderResources>);

/// Extracted text areas for rendering.
#[derive(Resource, Default)]
pub struct ExtractedTextAreas {
    pub areas: Vec<ExtractedTextArea>,
}

/// A single extracted text area.
pub struct ExtractedTextArea {
    /// Entity ID for stable caching (enables buffer reuse across frames)
    pub entity: Entity,
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub scale: f32,
    pub bounds: glyphon::TextBounds,
    pub color: glyphon::Color,
    pub metrics: glyphon::Metrics,
    pub family: glyphon::Family<'static>,
}

// ============================================================================
// BUFFER CACHE (Performance Optimization)
// ============================================================================
//
// Per-frame buffer allocation was causing performance issues:
// - Every frame: allocate new Buffer for each text area
// - Call set_text() + shape_until_scroll() (expensive shaping)
// - With 20 visible texts = 20 allocations/frame
//
// Solution: Cache buffers keyed by Entity ID, reuse when text/metrics unchanged.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Cached buffer with metadata for invalidation checking.
struct CachedBuffer {
    buffer: glyphon::Buffer,
    /// Hash of text content for invalidation detection
    text_hash: u64,
    /// Metrics used when creating the buffer
    metrics: glyphon::Metrics,
    /// Wrap width used for this buffer
    wrap_width: f32,
}

/// Cache for glyphon text buffers, keyed by entity ID.
///
/// This dramatically reduces per-frame allocations by reusing buffers
/// when text content and metrics haven't changed.
#[derive(Resource, Default)]
pub struct RenderTextBufferCache {
    buffers: HashMap<Entity, CachedBuffer>,
}

/// Hash text content for cache invalidation.
fn hash_text(text: &str) -> u64 {
    use std::hash::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Compare metrics for equality (glyphon::Metrics doesn't implement Eq).
fn metrics_equal(a: &glyphon::Metrics, b: &glyphon::Metrics) -> bool {
    (a.font_size - b.font_size).abs() < f32::EPSILON
        && (a.line_height - b.line_height).abs() < f32::EPSILON
}

/// Update the text resolution when the window resizes.
fn update_text_resolution(
    windows: Query<&Window, With<PrimaryWindow>>,
    mut resolution: ResMut<TextResolution>,
) {
    if let Ok(window) = windows.single() {
        let new_res = Resolution {
            width: window.physical_width(),
            height: window.physical_height(),
        };
        if resolution.0.width != new_res.width || resolution.0.height != new_res.height {
            resolution.0 = new_res;
        }
    }
}

/// Sync UI text positions from Bevy UI layout to UiTextPositionCache.
fn sync_ui_text_positions(
    mut query: Query<
        (&ComputedNode, &UiGlobalTransform, &mut UiTextPositionCache),
        With<GlyphonUiText>,
    >,
) {
    for (computed, global_transform, mut cache) in query.iter_mut() {
        // UiGlobalTransform gives us the center position in screen space
        // (origin at top-left, Y increases downward).
        // Convert to top-left corner for glyphon.
        let (_, _, translation) = global_transform.to_scale_angle_translation();
        let size = computed.size();

        // Translation is the center of the node, convert to top-left corner
        cache.left = translation.x - size.x / 2.0;
        cache.top = translation.y - size.y / 2.0;
        cache.width = size.x;
        cache.height = size.y;
    }
}

/// Extract text areas from the main world to the render world.
///
/// Only extracts text entities that are visible according to Bevy's visibility system.
/// This respects parent Visibility::Hidden propagation through InheritedVisibility.
///
/// Note: We use InheritedVisibility (not ViewVisibility) because our glyphon text
/// uses a custom renderer that bypasses Bevy's camera visibility checking. ViewVisibility
/// is only set to true by camera frustum culling systems, which don't know about our text.
fn extract_text_areas(
    mut commands: Commands,
    // Existing GlyphonTextBuffer + TextAreaConfig query (for cells)
    buffer_query: Extract<Query<(Entity, &GlyphonTextBuffer, &TextAreaConfig, &InheritedVisibility), With<GlyphonText>>>,
    // New GlyphonUiText query (for UI labels)
    ui_text_query: Extract<Query<(Entity, &GlyphonUiText, &UiTextPositionCache, &InheritedVisibility)>>,
    resolution: Extract<Res<TextResolution>>,
) {
    let mut areas = Vec::new();

    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    // Extract GlyphonTextBuffer areas (cells use monospace)
    for (entity, buffer, config, inherited_visibility) in buffer_query.iter() {
        // Skip entities that aren't visible (respects parent Visibility::Hidden)
        if !inherited_visibility.get() {
            continue;
        }

        let text = buffer.text();
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            info!(
                "First text extraction: {} chars at ({}, {}) bounds: {:?}",
                text.len(),
                config.left,
                config.top,
                config.bounds
            );
        }
        areas.push(ExtractedTextArea {
            entity,
            text,
            left: config.left,
            top: config.top,
            scale: config.scale,
            bounds: config.bounds,
            color: config.default_color,
            metrics: glyphon::Metrics::new(14.0, 20.0), // Default metrics for cells
            family: glyphon::Family::Monospace,
        });
    }

    // Extract GlyphonUiText areas (UI labels)
    for (entity, ui_text, position, inherited_visibility) in ui_text_query.iter() {
        // Skip entities that aren't visible (respects parent Visibility::Hidden)
        if !inherited_visibility.get() {
            continue;
        }

        // Skip empty text
        if ui_text.text.is_empty() {
            continue;
        }
        areas.push(ExtractedTextArea {
            entity,
            text: ui_text.text.clone(),
            left: position.left,
            top: position.top,
            scale: 1.0,
            bounds: glyphon::TextBounds {
                left: position.left as i32,
                top: position.top as i32,
                right: (position.left + position.width.max(800.0)) as i32,
                bottom: (position.top + position.height.max(100.0)) as i32,
            },
            color: ui_text.color,
            metrics: ui_text.metrics,
            family: ui_text.family,
        });
    }

    commands.insert_resource(ExtractedTextAreas { areas });
    commands.insert_resource(TextResolution(resolution.0));
}

/// Prepare text for rendering.
fn prepare_text(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    mut render_resources: ResMut<RenderTextResources>,
    mut buffer_cache: ResMut<RenderTextBufferCache>,
    extracted: Option<Res<ExtractedTextAreas>>,
    resolution: Option<Res<TextResolution>>,
    font_system: Res<SharedFontSystem>,
    swash_cache: Res<SharedSwashCache>,
    windows: Res<bevy::render::view::ExtractedWindows>,
) {
    // Get the surface format from extracted windows
    let surface_format = windows
        .windows
        .values()
        .next()
        .and_then(|w| w.swap_chain_texture_format);

    // Lazily initialize resources when we first get the surface format
    if render_resources.0.is_none() {
        if let Some(format) = surface_format {
            info!("Initializing glyphon with surface format: {:?}", format);

            let cache = Cache::new(device.wgpu_device());
            let viewport = Viewport::new(device.wgpu_device(), &cache);
            let mut atlas = TextAtlas::new(device.wgpu_device(), queue.0.as_ref(), &cache, format);
            // MSAA=1 because we render after Upscaling to the final swap chain (non-MSAA).
            // See the render graph setup in build() for why we chose this approach.
            let multisample = bevy::render::render_resource::MultisampleState {
                count: 1,
                ..default()
            };
            let renderer = TextRenderer::new(
                &mut atlas,
                device.wgpu_device(),
                multisample,
                None,
            );

            render_resources.0 = Some(TextRenderResources {
                _cache: cache,
                viewport,
                atlas,
                renderer,
            });
        } else {
            // Surface format not available yet, skip this frame
            return;
        }
    }

    let Some(resources) = render_resources.0.as_mut() else {
        return;
    };

    // Get extracted text areas or skip if none
    let Some(extracted) = extracted else {
        return;
    };

    // Update viewport resolution if available
    if let Some(ref resolution) = resolution {
        static LOGGED_RES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED_RES.swap(true, std::sync::atomic::Ordering::Relaxed) {
            info!("Viewport resolution: {}x{}", resolution.0.width, resolution.0.height);
        }
        resources.viewport.update(&queue.0, resolution.0);
    } else {
        warn!("No resolution available for viewport");
    }

    // Skip if no text to render
    if extracted.areas.is_empty() {
        return;
    }

    // Lock the shared resources
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };
    let Ok(mut swash_cache) = swash_cache.0.lock() else {
        return;
    };

    // ========================================================================
    // BUFFER CACHE: Reuse buffers when text/metrics unchanged
    // ========================================================================
    //
    // Instead of allocating new buffers every frame, we cache them by entity.
    // This dramatically reduces per-frame allocations and expensive shaping.

    // Track which entities we see this frame for stale entry cleanup
    let mut seen_entities: HashSet<Entity> = HashSet::with_capacity(extracted.areas.len());

    // Phase 1: Update cache (all mutations happen here)
    for area in &extracted.areas {
        seen_entities.insert(area.entity);

        let wrap_width = (area.bounds.right - area.bounds.left) as f32;
        let text_hash = hash_text(&area.text);

        // Check if we have a valid cached buffer
        let needs_rebuild = buffer_cache
            .buffers
            .get(&area.entity)
            .map(|cached| {
                cached.text_hash != text_hash
                    || !metrics_equal(&cached.metrics, &area.metrics)
                    || (cached.wrap_width - wrap_width).abs() > 1.0 // Allow small float variance
            })
            .unwrap_or(true); // No cache entry = needs rebuild

        if needs_rebuild {
            // Create new buffer
            let mut buffer = glyphon::Buffer::new(&mut font_system, area.metrics);
            buffer.set_size(&mut font_system, Some(wrap_width), None);

            let attrs = glyphon::Attrs::new().family(area.family);
            buffer.set_text(
                &mut font_system,
                &area.text,
                &attrs,
                glyphon::Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut font_system, false);

            // Debug logging for first buffer
            static LOGGED_LAYOUT: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_LAYOUT.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let line_count = buffer.lines.len();
                let total_glyphs: usize = buffer
                    .lines
                    .iter()
                    .filter_map(|line| line.layout_opt())
                    .flat_map(|layout| layout.iter())
                    .map(|run| run.glyphs.len())
                    .sum();
                info!("Buffer has {} lines, {} total glyphs (cache miss)", line_count, total_glyphs);
            }

            // Store in cache
            buffer_cache.buffers.insert(
                area.entity,
                CachedBuffer {
                    buffer,
                    text_hash,
                    metrics: area.metrics,
                    wrap_width,
                },
            );
        }
    }

    // Remove stale cache entries (entities that weren't seen this frame)
    buffer_cache.buffers.retain(|entity, _| seen_entities.contains(entity));

    // Phase 2: Build text areas referencing cached buffers (immutable borrow)
    let text_areas: Vec<glyphon::TextArea> = extracted
        .areas
        .iter()
        .filter_map(|area| {
            buffer_cache.buffers.get(&area.entity).map(|cached| {
                glyphon::TextArea {
                    buffer: &cached.buffer,
                    left: area.left,
                    top: area.top,
                    scale: area.scale,
                    bounds: area.bounds,
                    default_color: area.color,
                    custom_glyphs: &[],
                }
            })
        })
        .collect();

    // Prepare the text for rendering
    match resources.renderer.prepare(
        device.wgpu_device(),
        &queue.0,
        &mut font_system,
        &mut resources.atlas,
        &resources.viewport,
        text_areas,
        &mut swash_cache,
    ) {
        Ok(()) => {
            static LOGGED_PREP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_PREP.swap(true, std::sync::atomic::Ordering::Relaxed) {
                info!("glyphon prepare() succeeded");
            }
        }
        Err(e) => {
            error!("glyphon prepare() failed: {:?}", e);
        }
    }
}
