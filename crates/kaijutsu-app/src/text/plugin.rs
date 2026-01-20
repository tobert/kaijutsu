//! Text rendering plugin for Bevy.
//!
//! Sets up glyphon text rendering integrated with Bevy's render pipeline.

use bevy::prelude::*;
use bevy::render::{
    render_graph::{RenderGraphExt, ViewNodeRunner},
    renderer::{RenderDevice, RenderQueue},
    Extract, Render, RenderApp,
};
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
            .add_systems(Update, update_text_resolution);

        // Render world setup
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .add_systems(ExtractSchedule, extract_text_areas)
            .add_systems(Render, prepare_text);

        // Add render node to the graph - after EndMainPass (still MSAA)
        use bevy::core_pipeline::core_2d::graph::{Core2d, Node2d};
        render_app
            .add_render_graph_node::<ViewNodeRunner<TextRenderNode>>(Core2d, TextRenderNode::NAME)
            .add_render_graph_edges(Core2d, (Node2d::EndMainPass, TextRenderNode::NAME));
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
    pub text: String,
    pub left: f32,
    pub top: f32,
    pub scale: f32,
    pub bounds: glyphon::TextBounds,
    pub color: glyphon::Color,
    pub metrics: glyphon::Metrics,
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

/// Extract text areas from the main world to the render world.
fn extract_text_areas(
    mut commands: Commands,
    query: Extract<Query<(&TextBuffer, &TextAreaConfig), With<GlyphonText>>>,
    resolution: Extract<Res<TextResolution>>,
) {
    let mut areas = Vec::new();

    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    for (buffer, config) in query.iter() {
        let text = buffer.text();
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            info!("First text extraction: {} chars at ({}, {}) bounds: {:?}",
                  text.len(), config.left, config.top, config.bounds);
        }
        areas.push(ExtractedTextArea {
            text,
            left: config.left,
            top: config.top,
            scale: config.scale,
            bounds: config.bounds,
            color: config.default_color,
            metrics: glyphon::Metrics::new(14.0, 20.0), // Default metrics
        });
    }

    // Removed per-frame debug logging - too verbose

    commands.insert_resource(ExtractedTextAreas { areas });
    commands.insert_resource(TextResolution(resolution.0));
}

/// Prepare text for rendering.
fn prepare_text(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    mut render_resources: ResMut<RenderTextResources>,
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
            // EndMainPass is still MSAA (4x)
            let multisample = bevy::render::render_resource::MultisampleState {
                count: 4,
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

    // Create temporary buffers for each text area
    let mut buffers: Vec<glyphon::Buffer> = Vec::new();
    for area in &extracted.areas {
        let mut buffer = glyphon::Buffer::new(&mut font_system, area.metrics);

        // Use actual bounds width for wrap, unlimited height for layout
        let wrap_width = (area.bounds.right - area.bounds.left) as f32;
        buffer.set_size(&mut font_system, Some(wrap_width), None);

        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(
            &mut font_system,
            &area.text,
            &attrs,
            glyphon::Shaping::Advanced,
            None, // align
        );
        buffer.shape_until_scroll(&mut font_system, false);

        // Debug: check if buffer has layout runs
        static LOGGED_LAYOUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED_LAYOUT.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let line_count = buffer.lines.len();
            let total_glyphs: usize = buffer.lines.iter()
                .filter_map(|line| line.layout_opt())
                .flat_map(|layout| layout.iter())
                .map(|run| run.glyphs.len())
                .sum();
            info!("Buffer has {} lines, {} total glyphs", line_count, total_glyphs);
        }

        buffers.push(buffer);
    }

    // Build text areas for rendering
    let text_areas: Vec<glyphon::TextArea> = extracted
        .areas
        .iter()
        .zip(buffers.iter())
        .map(|(area, buffer)| glyphon::TextArea {
            buffer,
            left: area.left,
            top: area.top,
            scale: area.scale,
            bounds: area.bounds,
            default_color: area.color,
            custom_glyphs: &[],
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
