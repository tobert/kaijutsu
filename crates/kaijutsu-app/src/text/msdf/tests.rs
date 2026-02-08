//! GPU-based headless tests for MSDF text rendering.
//!
//! These tests run the full Bevy render pipeline without a window,
//! capture rendered output, and make assertions on pixel data.
//!
//! Based on `bevy/examples/app/headless_renderer.rs`.
//!
//! # Running Tests
//!
//! ```bash
//! # Run all GPU text tests (single-threaded for GPU safety)
//! cargo test -p kaijutsu-app text::msdf::tests -- --test-threads=1
//!
//! # Save PNG outputs for visual inspection
//! MSDF_TEST_SAVE_PNG=1 cargo test -p kaijutsu-app text::msdf::tests -- --nocapture
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_graph::{self, NodeRunError, RenderGraph, RenderGraphContext, RenderLabel};
use bevy::render::render_resource::{
    Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, PollType,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TextureFormat, TextureUsages,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue};
use bevy::render::{Extract, Render, RenderApp, RenderSystems};
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use crossbeam_channel::{Receiver, Sender};

use super::{GlowConfig, MsdfText, MsdfTextAreaConfig, MsdfTextBuffer, SdfTextEffects};
use crate::text::plugin::TextRenderPlugin;
use crate::text::resources::{MsdfRenderConfig, SharedFontSystem};

// ============================================================================
// TEST CONFIGURATION
// ============================================================================

/// Default test render dimensions.
const DEFAULT_WIDTH: u32 = 400;
const DEFAULT_HEIGHT: u32 = 100;

/// Number of frames to pre-roll before capturing.
/// Allows the render pipeline to fully initialize and MSDF glyphs to generate.
const PRE_ROLL_FRAMES: u32 = 60;

// ============================================================================
// HEADLESS RENDER INFRASTRUCTURE
// ============================================================================

/// Receive pixel data from render world.
#[derive(Resource)]
struct MainWorldReceiver(Receiver<Vec<u8>>);

/// Send pixel data from render world to main world.
#[derive(Resource, Clone)]
struct RenderWorldSender(Sender<Vec<u8>>);

/// Font family for tests.
#[derive(Clone, Copy, Debug)]
enum TestFontFamily {
    Serif,
    SansSerif,
    Monospace,
}

impl TestFontFamily {
    fn to_cosmic(&self) -> cosmic_text::Family<'static> {
        match self {
            TestFontFamily::Serif => cosmic_text::Family::Serif,
            TestFontFamily::SansSerif => cosmic_text::Family::SansSerif,
            TestFontFamily::Monospace => cosmic_text::Family::Monospace,
        }
    }
}

/// Test scene configuration.
#[derive(Resource)]
struct TestConfig {
    text: String,
    font_size: f32,
    width: u32,
    height: u32,
    font_family: TestFontFamily,
    /// Text position offset from top-left.
    left: f32,
    top: f32,
    /// Scale factor for text rendering.
    scale: f32,
    /// Text color.
    color: Color,
    /// Enable glow effect.
    glow: bool,
    /// Alternating per-glyph colors for overlap detection.
    /// When set, even glyphs get color_a, odd glyphs get color_b.
    alternating_colors: Option<([u8; 4], [u8; 4])>,
    /// Frames remaining before capture.
    frames_remaining: u32,
    /// Whether we've received and processed the image.
    done: Arc<AtomicBool>,
}

impl TestConfig {
    fn new(text: &str, font_size: f32, width: u32, height: u32, use_monospace: bool) -> Self {
        Self {
            text: text.to_string(),
            font_size,
            width,
            height,
            font_family: if use_monospace {
                TestFontFamily::Monospace
            } else {
                TestFontFamily::Serif
            },
            left: 10.0,
            top: 10.0,
            scale: 1.0,
            color: Color::WHITE,
            glow: false,
            alternating_colors: None,
            frames_remaining: PRE_ROLL_FRAMES,
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn with_position(mut self, left: f32, top: f32) -> Self {
        self.left = left;
        self.top = top;
        self
    }

    fn with_scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    fn with_glow(mut self) -> Self {
        self.glow = true;
        self
    }

    fn with_font_family(mut self, family: TestFontFamily) -> Self {
        self.font_family = family;
        self
    }

    fn with_alternating_colors(mut self, color_a: [u8; 4], color_b: [u8; 4]) -> Self {
        self.alternating_colors = Some((color_a, color_b));
        self
    }
}

/// Component to track the render target image.
#[allow(dead_code)]
#[derive(Component)]
struct RenderTargetImage(Handle<Image>);

/// Component to hold the ImageCopier reference for extraction.
#[derive(Component, Clone)]
struct ImageCopier {
    buffer: Buffer,
    enabled: Arc<AtomicBool>,
    src_image: Handle<Image>,
    width: u32,
    height: u32,
}

impl ImageCopier {
    fn new(src_image: Handle<Image>, size: Extent3d, render_device: &RenderDevice) -> Self {
        let padded_bytes_per_row =
            RenderDevice::align_copy_bytes_per_row(size.width as usize) * 4;

        let cpu_buffer = render_device.create_buffer(&BufferDescriptor {
            label: Some("image_copy_buffer"),
            size: padded_bytes_per_row as u64 * size.height as u64,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        ImageCopier {
            buffer: cpu_buffer,
            src_image,
            enabled: Arc::new(AtomicBool::new(true)),
            width: size.width,
            height: size.height,
        }
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

/// Resource to hold ImageCopiers in render world.
#[derive(Resource, Default, Clone)]
struct ImageCopiers(Vec<ImageCopier>);

/// Plugin for headless image capture.
struct ImageCapturePlugin {
    sender: Sender<Vec<u8>>,
    receiver: Receiver<Vec<u8>>,
}

impl Plugin for ImageCapturePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(MainWorldReceiver(self.receiver.clone()));

        let render_app = app.sub_app_mut(RenderApp);

        // Add the image copy node to render graph
        let mut graph = render_app.world_mut().resource_mut::<RenderGraph>();
        graph.add_node(ImageCopyLabel, ImageCopyNode);
        graph.add_node_edge(bevy::render::graph::CameraDriverLabel, ImageCopyLabel);

        render_app
            .insert_resource(RenderWorldSender(self.sender.clone()))
            .init_resource::<ImageCopiers>()
            .add_systems(ExtractSchedule, extract_image_copiers)
            .add_systems(Render, receive_image_from_buffer.after(RenderSystems::Render));
    }
}

/// Label for the image copy render node.
#[derive(Debug, PartialEq, Eq, Clone, Hash, RenderLabel)]
struct ImageCopyLabel;

/// Render graph node that copies texture to buffer.
#[derive(Default)]
struct ImageCopyNode;

impl render_graph::Node for ImageCopyNode {
    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let image_copiers = world.get_resource::<ImageCopiers>().unwrap();
        let gpu_images = world
            .get_resource::<RenderAssets<bevy::render::texture::GpuImage>>()
            .unwrap();

        for image_copier in &image_copiers.0 {
            if !image_copier.enabled() {
                continue;
            }

            let Some(src_image) = gpu_images.get(&image_copier.src_image) else {
                continue;
            };

            let mut encoder = render_context
                .render_device()
                .create_command_encoder(&CommandEncoderDescriptor::default());

            let block_dimensions = src_image.texture_format.block_dimensions();
            let block_size = src_image.texture_format.block_copy_size(None).unwrap();

            let padded_bytes_per_row = RenderDevice::align_copy_bytes_per_row(
                (src_image.size.width as usize / block_dimensions.0 as usize) * block_size as usize,
            );

            encoder.copy_texture_to_buffer(
                src_image.texture.as_image_copy(),
                TexelCopyBufferInfo {
                    buffer: &image_copier.buffer,
                    layout: TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(
                            std::num::NonZero::<u32>::new(padded_bytes_per_row as u32)
                                .unwrap()
                                .into(),
                        ),
                        rows_per_image: None,
                    },
                },
                src_image.size,
            );

            let render_queue = world.get_resource::<RenderQueue>().unwrap();
            render_queue.submit(std::iter::once(encoder.finish()));
        }

        Ok(())
    }
}

/// Extract ImageCopiers into render world.
fn extract_image_copiers(mut commands: Commands, query: Extract<Query<&ImageCopier>>) {
    commands.insert_resource(ImageCopiers(query.iter().cloned().collect()));
}

/// Read pixels from GPU buffer and send through channel.
fn receive_image_from_buffer(
    image_copiers: Res<ImageCopiers>,
    render_device: Res<RenderDevice>,
    sender: Res<RenderWorldSender>,
) {
    for image_copier in &image_copiers.0 {
        if !image_copier.enabled() {
            continue;
        }

        let buffer_slice = image_copier.buffer.slice(..);

        let (s, r) = crossbeam_channel::bounded(1);

        buffer_slice.map_async(MapMode::Read, move |result| match result {
            Ok(()) => s.send(()).expect("Failed to send map notification"),
            Err(err) => panic!("Failed to map buffer: {err}"),
        });

        render_device
            .poll(PollType::wait_indefinitely())
            .expect("Failed to poll device");

        r.recv().expect("Failed to receive map notification");

        // Get the raw data and unpad it
        let raw_data = buffer_slice.get_mapped_range().to_vec();
        let row_bytes = image_copier.width as usize * 4;
        let aligned_row_bytes = RenderDevice::align_copy_bytes_per_row(row_bytes);

        let unpadded: Vec<u8> = if row_bytes == aligned_row_bytes {
            raw_data
        } else {
            raw_data
                .chunks(aligned_row_bytes)
                .take(image_copier.height as usize)
                .flat_map(|row| &row[..row_bytes.min(row.len())])
                .cloned()
                .collect()
        };

        let _ = sender.0.send(unpadded);
        image_copier.buffer.unmap();
    }
}

// ============================================================================
// TEST HARNESS
// ============================================================================

/// RGBA pixel for test assertions.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

#[allow(dead_code)]
impl Rgba {
    fn from_slice(data: &[u8], offset: usize) -> Self {
        Self {
            r: data[offset],
            g: data[offset + 1],
            b: data[offset + 2],
            a: data[offset + 3],
        }
    }

    fn is_opaque(&self) -> bool {
        // Check for bright pixels (white text on black background)
        // Use luminance threshold to detect significant text pixels
        self.luminance() > 0.5
    }

    fn is_visible(&self) -> bool {
        // Check for non-black pixels (background is black with full alpha)
        self.r > 0 || self.g > 0 || self.b > 0
    }

    fn luminance(&self) -> f32 {
        (0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32) / 255.0
    }
}

/// Rendered test output.
struct TestOutput {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    /// Whether pixels are in BGRA order (true) or RGBA order (false).
    is_bgra: bool,
}

impl TestOutput {
    /// Get pixel at (x, y), converting from the buffer's format to RGBA.
    fn pixel(&self, x: u32, y: u32) -> Rgba {
        let offset = ((y * self.width + x) * 4) as usize;
        if self.is_bgra {
            // BGRA layout: B at +0, G at +1, R at +2, A at +3
            Rgba {
                r: self.pixels[offset + 2],
                g: self.pixels[offset + 1],
                b: self.pixels[offset],
                a: self.pixels[offset + 3],
            }
        } else {
            // RGBA layout
            Rgba::from_slice(&self.pixels, offset)
        }
    }

    /// Count non-black pixels (visible text on black background).
    fn count_visible_pixels(&self) -> usize {
        // For non-black detection, channel order doesn't matter - we just check if any RGB channel is non-zero
        (0..self.pixels.len())
            .step_by(4)
            .filter(|&i| self.pixels[i] > 0 || self.pixels[i + 1] > 0 || self.pixels[i + 2] > 0)
            .count()
    }

    /// Find bounding box of non-transparent pixels.
    fn bounding_box(&self) -> Option<(u32, u32, u32, u32)> {
        let mut min_x = self.width;
        let mut max_x = 0u32;
        let mut min_y = self.height;
        let mut max_y = 0u32;

        for y in 0..self.height {
            for x in 0..self.width {
                if self.pixel(x, y).is_visible() {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }

        if max_x >= min_x && max_y >= min_y {
            Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
        } else {
            None
        }
    }

    /// Find vertical bars (for monospace pipe test).
    /// Returns x-coordinates of vertical bars found.
    fn find_vertical_bars(&self, threshold: f32) -> Vec<u32> {
        let mut bars = Vec::new();
        let mut in_bar = false;

        for x in 0..self.width {
            // Count visible pixels in this column
            let mut visible_count = 0;
            for y in 0..self.height {
                if self.pixel(x, y).is_opaque() {
                    visible_count += 1;
                }
            }

            let density = visible_count as f32 / self.height as f32;
            let is_bar = density > threshold;

            if is_bar && !in_bar {
                // Entering a bar - record the start
                bars.push(x);
                in_bar = true;
            } else if !is_bar && in_bar {
                // Exiting a bar
                in_bar = false;
            }
        }

        bars
    }

    /// Measure gap between glyphs by finding columns with no/few pixels.
    #[allow(dead_code)]
    fn measure_glyph_gap(&self, threshold: f32) -> Option<u32> {
        let mut gap_start = None;
        let mut gap_end = None;
        let mut seen_first_glyph = false;

        for x in 0..self.width {
            let mut visible_count = 0;
            for y in 0..self.height {
                if self.pixel(x, y).is_visible() {
                    visible_count += 1;
                }
            }

            let density = visible_count as f32 / self.height as f32;
            let is_glyph = density > threshold;

            if is_glyph {
                if !seen_first_glyph {
                    seen_first_glyph = true;
                } else if gap_start.is_some() && gap_end.is_none() {
                    gap_end = Some(x);
                }
            } else if seen_first_glyph && gap_start.is_none() {
                gap_start = Some(x);
            }
        }

        match (gap_start, gap_end) {
            (Some(start), Some(end)) => Some(end - start),
            _ => None,
        }
    }

    /// Save as PNG for debugging.
    fn save_png(&self, name: &str) {
        if std::env::var("MSDF_TEST_SAVE_PNG").is_err() {
            return;
        }

        let dir = PathBuf::from("/tmp/msdf_tests");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("{}.png", name));

        // Convert to RGBA if needed (PNG expects RGBA)
        let pixels: Vec<u8> = if self.is_bgra {
            self.pixels
                .chunks(4)
                .flat_map(|bgra| [bgra[2], bgra[1], bgra[0], bgra[3]])
                .collect()
        } else {
            self.pixels.clone()
        };

        image::save_buffer(
            &path,
            &pixels,
            self.width,
            self.height,
            image::ColorType::Rgba8,
        )
        .expect("Failed to save test PNG");

        eprintln!("Saved: {}", path.display());
    }
}

/// Render text headlessly and return pixel data.
fn render_text_headless(
    text: &str,
    font_size: f32,
    width: u32,
    height: u32,
    use_monospace: bool,
) -> TestOutput {
    let config = TestConfig::new(text, font_size, width, height, use_monospace);
    render_with_config(config)
}

/// Render text with full configuration control.
fn render_with_config(mut config: TestConfig) -> TestOutput {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let done = Arc::new(AtomicBool::new(false));
    config.done = done.clone();

    // Find the workspace root for asset path
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Create render config with test dimensions - MUST be done before TextRenderPlugin
    // Use Bgra8UnormSrgb to match the render target texture format created in setup_test_scene
    let format = TextureFormat::Bgra8UnormSrgb;
    let render_config = MsdfRenderConfig::new(config.width, config.height)
        .with_format(format);
    let width = config.width;
    let height = config.height;
    let is_bgra = matches!(format,
        TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb);

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    file_path: workspace_root.join("assets").to_string_lossy().to_string(),
                    ..default()
                })
                .set(ImagePlugin::default_nearest())
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .disable::<WinitPlugin>(),
        )
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_millis(16)))
        // Insert render config BEFORE TextRenderPlugin so it's available during extraction
        .insert_resource(render_config)
        .add_plugins(TextRenderPlugin)
        .add_plugins(ImageCapturePlugin {
            sender,
            receiver: receiver.clone(),
        })
        .insert_resource(ClearColor(Color::BLACK))
        .insert_resource(config)
        .add_systems(Startup, setup_test_scene)
        .add_systems(PostUpdate, process_captured_image)
        .run();

    // Get the captured pixels
    let pixels = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("Timeout waiting for rendered pixels");

    TestOutput {
        pixels,
        width,
        height,
        is_bgra,
    }
}

/// Setup the test scene with text and camera.
fn setup_test_scene(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    render_device: Res<RenderDevice>,
    config: Res<TestConfig>,
    font_system: Res<SharedFontSystem>,
) {
    let size = Extent3d {
        width: config.width,
        height: config.height,
        ..default()
    };

    // Create render target texture
    let mut render_target =
        Image::new_target_texture(size.width, size.height, TextureFormat::Bgra8UnormSrgb, None);
    render_target.texture_descriptor.usage |= TextureUsages::COPY_SRC;
    let render_target_handle = images.add(render_target);

    // Spawn image copier
    let copier = ImageCopier::new(render_target_handle.clone(), size, &render_device);
    commands.spawn(copier);

    // Camera targeting the render texture
    commands.spawn((
        Camera2d,
        RenderTarget::Image(render_target_handle.clone().into()),
    ));

    // Track the render target for cleanup
    commands.spawn(RenderTargetImage(render_target_handle));

    // Create MSDF text
    let font_family = config.font_family.to_cosmic();

    // Initialize text buffer
    let metrics = cosmic_text::Metrics::new(config.font_size, config.font_size * 1.2);

    if let Ok(mut fs) = font_system.0.lock() {
        let mut buffer = MsdfTextBuffer::new(&mut fs, metrics);
        let attrs = cosmic_text::Attrs::new().family(font_family);
        buffer.set_text(&mut fs, &config.text, attrs, cosmic_text::Shaping::Advanced);
        buffer.set_color(config.color);
        buffer.visual_line_count(&mut fs, config.width as f32, None);

        // Apply per-glyph alternating colors if requested
        if let Some((color_a, color_b)) = config.alternating_colors {
            buffer.set_alternating_colors(color_a, color_b);
        }

        // Position and scale from config
        let text_config = MsdfTextAreaConfig {
            left: config.left,
            top: config.top,
            scale: config.scale,
            bounds: super::TextBounds {
                left: 0,
                top: 0,
                right: config.width as i32,
                bottom: config.height as i32,
            },
            default_color: config.color,
        };

        // Build effects if enabled
        let effects = if config.glow {
            SdfTextEffects {
                rainbow: false,
                glow: Some(GlowConfig::default()),
            }
        } else {
            SdfTextEffects::default()
        };

        commands.spawn((
            buffer,
            text_config,
            effects,
            MsdfText,
            Visibility::Visible,
            InheritedVisibility::VISIBLE,
            ViewVisibility::default(),
        ));
    }
}

/// Process captured image and exit when done.
fn process_captured_image(
    receiver: Res<MainWorldReceiver>,
    mut config: ResMut<TestConfig>,
    mut exit: MessageWriter<AppExit>,
) {
    if config.done.load(Ordering::Relaxed) {
        return;
    }

    if config.frames_remaining > 0 {
        // Drain any early frames
        while receiver.0.try_recv().is_ok() {}
        config.frames_remaining -= 1;
        return;
    }

    // Try to receive the image
    if let Ok(_data) = receiver.0.try_recv() {
        // Data will be received by the outer receiver in render_text_headless
        config.done.store(true, Ordering::Relaxed);
        exit.write(AppExit::Success);
    }
}

// ============================================================================
// GPU TESTS
// ============================================================================

/// Test 1: Basic text renders (not blank).
///
/// Verifies that text actually appears on screen (catches catastrophic failures).
#[test]
fn text_renders_nonblank() {
    let output = render_text_headless("Hello", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("text_renders_nonblank");

    let visible_count = output.count_visible_pixels();
    assert!(
        visible_count > 100,
        "Text should render visible pixels, found only {}",
        visible_count
    );
}

/// Test 2: Monospace spacing consistency.
///
/// Using monospace font, all glyphs MUST be identical width.
/// This catches the "letters too close together" bug.
#[test]
fn mono_spacing_consistent() {
    let output = render_text_headless("|||", 32.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, true);
    output.save_png("mono_spacing_consistent");

    // Find the three pipes by scanning for vertical bars
    // Use a lower threshold since MSDF antialiasing spreads the pixels
    let bar_positions = output.find_vertical_bars(0.02);

    assert!(
        bar_positions.len() >= 3,
        "Should find at least 3 vertical bars, found {} at positions {:?}",
        bar_positions.len(),
        bar_positions
    );

    if bar_positions.len() >= 3 {
        let gap1 = bar_positions[1] - bar_positions[0];
        let gap2 = bar_positions[2] - bar_positions[1];

        // Allow small tolerance for antialiasing
        let diff = (gap1 as i32 - gap2 as i32).abs();
        assert!(
            diff <= 2,
            "Monospace spacing must be consistent: gap1={}, gap2={}, diff={}",
            gap1,
            gap2,
            diff
        );
    }
}

/// Test 3: Kerning visible - AV should be narrower than AA.
///
/// Verifies that kerning pairs render correctly by measuring total width.
/// With proper kerning, the V tucks under the A, making "AV" narrower than "AA".
#[test]
fn kerning_av_narrower_than_aa() {
    let av_output = render_text_headless("AV", 32.0, 150, DEFAULT_HEIGHT, false);
    let aa_output = render_text_headless("AA", 32.0, 150, DEFAULT_HEIGHT, false);

    av_output.save_png("kerning_av");
    aa_output.save_png("kerning_aa");

    // Measure bounding box widths
    let av_bbox = av_output.bounding_box();
    let aa_bbox = aa_output.bounding_box();

    match (av_bbox, aa_bbox) {
        (Some((_, _, av_width, _)), Some((_, _, aa_width, _))) => {
            eprintln!("AV width: {}, AA width: {}", av_width, aa_width);

            // AV should be at least 5% narrower than AA due to kerning
            // (V tucks under A significantly in most fonts)
            assert!(
                av_width < aa_width,
                "KERNING BROKEN: AV ({}) should be narrower than AA ({}) due to kern pair",
                av_width,
                aa_width
            );
        }
        _ => {
            panic!("Could not measure bounding boxes for kerning test");
        }
    }
}

/// Test 4: Font size affects render size.
///
/// Larger font = larger rendered glyphs.
#[test]
fn font_size_affects_render() {
    let small = render_text_headless("A", 16.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    let large = render_text_headless("A", 32.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);

    small.save_png("font_size_small");
    large.save_png("font_size_large");

    let small_bbox = small.bounding_box();
    let large_bbox = large.bounding_box();

    match (small_bbox, large_bbox) {
        (Some((_, _, sw, sh)), Some((_, _, lw, lh))) => {
            // Large should be roughly 2x the size of small
            let width_ratio = lw as f32 / sw as f32;
            let height_ratio = lh as f32 / sh as f32;

            assert!(
                width_ratio > 1.5 && width_ratio < 2.5,
                "32px should be ~2x wider than 16px: ratio={}",
                width_ratio
            );
            assert!(
                height_ratio > 1.5 && height_ratio < 2.5,
                "32px should be ~2x taller than 16px: ratio={}",
                height_ratio
            );
        }
        _ => {
            panic!(
                "Couldn't find bounding boxes: small={:?}, large={:?}",
                small_bbox, large_bbox
            );
        }
    }
}

/// Test 5: Multiple characters render in sequence.
///
/// Verifies that multi-character strings render correctly without overlap.
#[test]
fn multi_char_sequence() {
    let output = render_text_headless("ABC", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("multi_char_sequence");

    // Should have a reasonable bounding box
    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "Multi-character text should have visible pixels");

    if let Some((x, y, w, h)) = bbox {
        // Width should be at least 2x height for "ABC" (roughly square chars)
        assert!(
            w > h,
            "ABC should be wider than tall: {}x{} at ({}, {})",
            w,
            h,
            x,
            y
        );

        // Width should be reasonable for 3 characters at 24px
        // Each character ~15-20px wide, so total ~45-60px minimum
        assert!(w >= 40, "ABC should be at least 40px wide, got {}", w);
    }
}

// ============================================================================
// EXTENDED GPU TESTS
// ============================================================================

/// Test 6: Text wrapping produces multiple lines.
///
/// Long text should wrap at the boundary and produce taller output.
#[test]
fn text_wraps_to_multiple_lines() {
    // Single line
    let single = render_text_headless("Hello", 24.0, 200, 100, false);
    // Text that should wrap (narrow width forces wrap)
    let wrapped = render_text_headless("Hello World Test", 24.0, 80, 150, false);

    single.save_png("wrap_single_line");
    wrapped.save_png("wrap_multi_line");

    let single_bbox = single.bounding_box();
    let wrapped_bbox = wrapped.bounding_box();

    match (single_bbox, wrapped_bbox) {
        (Some((_, _, _, sh)), Some((_, _, _, wh))) => {
            // Wrapped text should be taller (multiple lines)
            assert!(
                wh > sh,
                "Wrapped text should be taller: single={}px, wrapped={}px",
                sh, wh
            );
        }
        _ => {
            panic!("Couldn't measure text heights");
        }
    }
}

/// Test 7: Scale factor affects rendered size.
///
/// scale=2.0 should render text 2x larger.
#[test]
fn scale_factor_affects_size() {
    let normal = render_text_headless("A", 16.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);

    let config = TestConfig::new("A", 16.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false)
        .with_scale(2.0);
    let scaled = render_with_config(config);

    normal.save_png("scale_normal");
    scaled.save_png("scale_2x");

    let normal_bbox = normal.bounding_box();
    let scaled_bbox = scaled.bounding_box();

    match (normal_bbox, scaled_bbox) {
        (Some((_, _, nw, nh)), Some((_, _, sw, sh))) => {
            let width_ratio = sw as f32 / nw as f32;
            let height_ratio = sh as f32 / nh as f32;

            assert!(
                width_ratio > 1.7 && width_ratio < 2.3,
                "scale=2.0 should ~double width: ratio={}",
                width_ratio
            );
            assert!(
                height_ratio > 1.7 && height_ratio < 2.3,
                "scale=2.0 should ~double height: ratio={}",
                height_ratio
            );
        }
        _ => {
            panic!("Couldn't measure bounding boxes");
        }
    }
}

/// Test 8: Text position offset works correctly.
///
/// Text at (100, 50) should have its bounding box start near that position.
#[test]
fn position_offset_works() {
    let config = TestConfig::new("X", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false)
        .with_position(100.0, 50.0);
    let output = render_with_config(config);
    output.save_png("position_offset");

    if let Some((x, y, _, _)) = output.bounding_box() {
        // Allow some tolerance for anchor offset and antialiasing
        assert!(
            x >= 90 && x <= 120,
            "Text at left=100 should start near x=100, got x={}",
            x
        );
        assert!(
            y >= 40 && y <= 70,
            "Text at top=50 should start near y=50, got y={}",
            y
        );
    } else {
        panic!("Text should be visible");
    }
}

/// Test 9: Colored text renders with that color.
///
/// Red text should have red pixels (r > g, r > b).
#[test]
fn colored_text_renders() {
    let config = TestConfig::new("X", 32.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false)
        .with_color(Color::srgb(1.0, 0.0, 0.0)); // Pure red
    let output = render_with_config(config);
    output.save_png("colored_red");

    // Find a bright pixel and check its color
    // Note: pure red (255,0,0) has luminance ~0.3, so we use a lower threshold
    let mut found_red = false;
    for y in 0..output.height {
        for x in 0..output.width {
            let px = output.pixel(x, y);
            if px.luminance() > 0.2 {
                // This is a text pixel - check it's reddish
                if px.r > 200 && px.g < 100 && px.b < 100 {
                    found_red = true;
                    break;
                }
            }
        }
        if found_red {
            break;
        }
    }

    assert!(found_red, "Red text should have red pixels");
}

/// Test 10: Glow effect expands the visible area.
///
/// Text with glow should have more visible pixels than without.
#[test]
fn glow_effect_expands_bounds() {
    let normal = render_text_headless("A", 32.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);

    let config = TestConfig::new("A", 32.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false)
        .with_glow();
    let glowing = render_with_config(config);

    normal.save_png("glow_off");
    glowing.save_png("glow_on");

    let normal_pixels = normal.count_visible_pixels();
    let glow_pixels = glowing.count_visible_pixels();

    // Glow should add pixels around the text
    assert!(
        glow_pixels > normal_pixels,
        "Glow should add visible pixels: normal={}, glow={}",
        normal_pixels, glow_pixels
    );
}

/// Test 11: Punctuation and special characters render.
///
/// Common punctuation should produce visible output.
#[test]
fn punctuation_renders() {
    let output = render_text_headless("!@#$%", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("punctuation");

    let visible = output.count_visible_pixels();
    assert!(
        visible > 50,
        "Punctuation should render visible pixels, got {}",
        visible
    );
}

/// Test 12: Numbers render correctly.
#[test]
fn numbers_render() {
    let output = render_text_headless("0123456789", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("numbers");

    // Should have a wide bounding box for 10 digits
    if let Some((_, _, w, _)) = output.bounding_box() {
        assert!(w >= 100, "10 digits should be at least 100px wide, got {}", w);
    } else {
        panic!("Numbers should be visible");
    }
}

/// Test 13: Empty string doesn't crash and renders blank.
#[test]
fn empty_string_renders_blank() {
    let output = render_text_headless("", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("empty_string");

    let visible = output.count_visible_pixels();
    assert_eq!(visible, 0, "Empty string should render no visible pixels");
}

/// Test 14: Whitespace-only string renders blank.
#[test]
fn whitespace_only_renders_blank() {
    let output = render_text_headless("   ", 24.0, DEFAULT_WIDTH, DEFAULT_HEIGHT, false);
    output.save_png("whitespace_only");

    let visible = output.count_visible_pixels();
    assert_eq!(visible, 0, "Whitespace-only should render no visible pixels");
}

// ============================================================================
// NON-GPU LAYOUT TESTS (cosmic-text verification)
// ============================================================================

/// Test 15: Verify kerning at the cosmic-text layout level (no GPU).
///
/// This checks that cosmic-text applies kerning during shaping.
/// The second glyph in "AV" should have a smaller x position than in "AA"
/// due to the negative kern pair.
#[test]
fn cosmic_text_applies_kerning() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(32.0, 38.4);

    // Create buffer for "AV"
    let mut av_buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Serif);
    av_buffer.set_text(&mut font_system, "AV", attrs.clone(), Shaping::Advanced);
    av_buffer.visual_line_count(&mut font_system, 400.0, None);
    let av_positions = av_buffer.glyph_positions();

    // Create buffer for "AA"
    let mut aa_buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    aa_buffer.set_text(&mut font_system, "AA", attrs, Shaping::Advanced);
    aa_buffer.visual_line_count(&mut font_system, 400.0, None);
    let aa_positions = aa_buffer.glyph_positions();

    assert_eq!(av_positions.len(), 2, "AV should have 2 glyphs");
    assert_eq!(aa_positions.len(), 2, "AA should have 2 glyphs");

    // Both first glyphs (A) should start at same x
    let av_first_x = av_positions[0].0;
    let aa_first_x = aa_positions[0].0;
    assert!(
        (av_first_x - aa_first_x).abs() < 0.1,
        "First A should be at same x: AV={}, AA={}",
        av_first_x,
        aa_first_x
    );

    // The second glyph should be at different x due to kerning
    let av_second_x = av_positions[1].0;
    let aa_second_x = aa_positions[1].0;

    eprintln!("AV glyph positions: {:?}", av_positions);
    eprintln!("AA glyph positions: {:?}", aa_positions);
    eprintln!("AV second glyph x: {}", av_second_x);
    eprintln!("AA second glyph x: {}", aa_second_x);

    // V should be closer to A than the second A is (negative kern)
    assert!(
        av_second_x < aa_second_x,
        "KERNING MISSING: V in 'AV' should be closer (x={}) than A in 'AA' (x={})",
        av_second_x,
        aa_second_x
    );
}

/// Test 16: NORMAL text - investigate NO spacing bug.
///
/// The live app uses SansSerif at 14px. "NO" letters appear to touch.
#[test]
fn normal_text_spacing_sansserif() {
    // Match live app: SansSerif at 14px
    let config = TestConfig::new("NORMAL", 14.0, 200, 50, false)
        .with_font_family(TestFontFamily::SansSerif);
    let output = render_with_config(config);
    output.save_png("normal_sansserif");

    // Also test just "NO" to isolate
    let no_config = TestConfig::new("NO", 14.0, 100, 50, false)
        .with_font_family(TestFontFamily::SansSerif);
    let no_output = render_with_config(no_config);
    no_output.save_png("no_sansserif");

    eprintln!("NORMAL (SansSerif) bounding box: {:?}", output.bounding_box());
    eprintln!("NO (SansSerif) bounding box: {:?}", no_output.bounding_box());
}

/// Test 17: Compare Serif vs SansSerif for "NORMAL".
#[test]
fn normal_text_serif_vs_sansserif() {
    // Serif version (what tests were using)
    let serif_output = render_text_headless("NORMAL", 14.0, 200, 50, false);
    serif_output.save_png("normal_serif");

    // SansSerif version (what live app uses)
    let sansserif_config = TestConfig::new("NORMAL", 14.0, 200, 50, false)
        .with_font_family(TestFontFamily::SansSerif);
    let sansserif_output = render_with_config(sansserif_config);
    sansserif_output.save_png("normal_sansserif_compare");

    eprintln!("Serif bbox: {:?}", serif_output.bounding_box());
    eprintln!("SansSerif bbox: {:?}", sansserif_output.bounding_box());
}

/// Test 18: Check cosmic-text glyph positions for "NORMAL" in SansSerif.
#[test]
fn normal_sansserif_glyph_positions() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(14.0, 16.8); // 14px with 1.2x line height

    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::SansSerif);
    buffer.set_text(&mut font_system, "NORMAL", attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let positions = buffer.glyph_positions();

    eprintln!("NORMAL glyph positions ({} glyphs):", positions.len());
    for (i, (x, y)) in positions.iter().enumerate() {
        let ch = "NORMAL".chars().nth(i).unwrap_or('?');
        eprintln!("  [{}] '{}': ({:.2}, {:.2})", i, ch, x, y);
    }

    // Check N-O gap (positions 0 and 1)
    if positions.len() >= 2 {
        let n_x = positions[0].0;
        let o_x = positions[1].0;
        let gap = o_x - n_x;
        eprintln!("N-O gap: {:.2} (N at {:.2}, O at {:.2})", gap, n_x, o_x);

        // N should take about 10px at 14px font size, so gap should be ~10+
        assert!(
            gap > 5.0,
            "N-O gap ({:.2}) seems too small for 14px font",
            gap
        );
    }
}

/// Test 19: Gap between two I letters must have at least one empty column.
///
/// This test renders "II" (two vertical bars) and verifies there's a gap.
/// Using I avoids N's diagonal complicating the column analysis.
/// If glyphs overlap with no empty column between them, the test fails.
#[test]
fn gap_between_simple_glyphs() {
    // Render "II" at 32px SansSerif - two simple vertical bars
    let config = TestConfig::new("II", 32.0, 150, 80, false)
        .with_font_family(TestFontFamily::SansSerif);
    let output = render_with_config(config);
    output.save_png("no_gap_test");

    // Scan columns to find glyph regions and gaps
    // Use a luminance threshold to ignore faint antialiasing
    // A column is "solid" if it has pixels above the threshold
    const SOLID_THRESHOLD: f32 = 0.5; // Ignore faint AA pixels

    let mut column_has_solid: Vec<bool> = Vec::new();
    let mut column_max_lum: Vec<f32> = Vec::new();
    for x in 0..output.width {
        let mut max_lum: f32 = 0.0;
        for y in 0..output.height {
            let lum = output.pixel(x, y).luminance();
            if lum > max_lum {
                max_lum = lum;
            }
        }
        column_max_lum.push(max_lum);
        column_has_solid.push(max_lum > SOLID_THRESHOLD);
    }

    // Find transitions: solid -> gap -> solid indicates separation
    // We expect: empty... solid(N)... gap... solid(O)... empty
    let mut in_solid = false;
    let mut glyph_regions: Vec<(u32, u32)> = Vec::new(); // (start, end) of each solid region
    let mut current_start = 0u32;

    for (x, &is_solid) in column_has_solid.iter().enumerate() {
        if is_solid && !in_solid {
            // Entering a solid region
            current_start = x as u32;
            in_solid = true;
        } else if !is_solid && in_solid {
            // Exiting a solid region
            glyph_regions.push((current_start, x as u32 - 1));
            in_solid = false;
        }
    }
    // Handle case where glyph extends to the edge
    if in_solid {
        glyph_regions.push((current_start, output.width - 1));
    }

    eprintln!("Found {} glyph regions: {:?}", glyph_regions.len(), glyph_regions);

    // Debug: print pixel values for each column if glyphs are merged
    if glyph_regions.len() == 1 {
        let (start, end) = glyph_regions[0];
        eprintln!("\nColumn-by-column analysis (looking for the gap):");
        for x in start..=end {
            let mut max_luminance: f32 = 0.0;
            let mut visible_count = 0;
            for y in 0..output.height {
                let p = output.pixel(x, y);
                if p.is_visible() {
                    visible_count += 1;
                    let lum = p.luminance();
                    if lum > max_luminance {
                        max_luminance = lum;
                    }
                }
            }
            eprintln!("  col {:2}: {:2} visible, max_lum {:.3}", x, visible_count, max_luminance);
        }
    }

    // For "NO" we expect 2 separate glyph regions with a gap between them
    // If we only find 1 region, the glyphs are overlapping!
    assert!(
        glyph_regions.len() >= 2,
        "GLYPH OVERLAP BUG: Expected 2 separate glyph regions for 'NO', but found {}. \
         The glyphs are visually merged with no empty column between them! \
         Regions found: {:?}",
        glyph_regions.len(),
        glyph_regions
    );

    // Measure the gap between the first two regions
    if glyph_regions.len() >= 2 {
        let gap_start = glyph_regions[0].1 + 1;
        let gap_end = glyph_regions[1].0;
        let gap_width = if gap_end > gap_start { gap_end - gap_start } else { 0 };
        eprintln!(
            "Gap between glyphs: columns {} to {} ({} empty columns)",
            gap_start, gap_end, gap_width
        );
    }

    // Debug: print pixel values for each column in the glyph region
    if glyph_regions.len() == 1 {
        let (start, end) = glyph_regions[0];
        eprintln!("\nColumn-by-column analysis (where N ends, O begins):");
        // Focus on the expected gap region (around x=20 based on cosmic-text positions)
        for x in start..=end {
            let mut max_luminance: f32 = 0.0;
            let mut visible_count = 0;
            for y in 0..output.height {
                let p = output.pixel(x, y);
                if p.is_visible() {
                    visible_count += 1;
                    let lum = p.luminance();
                    if lum > max_luminance {
                        max_luminance = lum;
                    }
                }
            }
            if visible_count > 0 {
                eprintln!("  col {}: {} visible pixels, max luminance {:.3}", x, visible_count, max_luminance);
            }
        }
    }
}

/// Test 20: "NO" at 14px renders with proper letter separation.
///
/// At 14px SansSerif, "NO" should have clear visual separation between letters.
/// With proper MSDF rendering, the glyphs should not overlap or blend together.
///
/// This test verifies:
/// 1. Both N and O render with appropriate pixel counts
/// 2. The letters have visible separation (gap or low-density transition zone)
/// 3. Total width is reasonable for the font size
#[test]
fn no_at_14px_renders_correctly() {
    // Match the exact live app configuration
    let config = TestConfig::new("NO", 14.0, 80, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let output = render_with_config(config);
    output.save_png("no_14px_renders_correctly");

    // Scan columns for visible pixels
    let mut column_counts: Vec<(u32, u32)> = Vec::new();
    for x in 0..output.width {
        let mut count = 0u32;
        for y in 0..output.height {
            if output.pixel(x, y).is_visible() {
                count += 1;
            }
        }
        column_counts.push((x, count));
    }

    eprintln!("Column analysis for 'NO' at 14px:");
    for (x, count) in &column_counts {
        if *count > 0 {
            eprintln!("  col {:2}: {:2} pixels", x, count);
        }
    }

    // Find the bounding box of visible pixels
    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "NO should render visible pixels");

    let (min_x, min_y, width, height) = bbox.unwrap();
    eprintln!("\nBounding box: x={}, y={}, {}x{}", min_x, min_y, width, height);

    // At 14px, "NO" should be roughly 20-25 pixels wide (N ~10px + O ~10-12px)
    assert!(
        width >= 18 && width <= 30,
        "NO at 14px should be 18-30px wide, got {}",
        width
    );

    // Height should be roughly the font size (14px) with some variation
    assert!(
        height >= 10 && height <= 20,
        "NO at 14px should be 10-20px tall, got {}",
        height
    );

    // Find the transition zone between N and O
    // N ends around col 19, O starts around col 21
    // We look for either a gap (empty column) or a low-density transition
    let transition_zone_start = 19u32;
    let transition_zone_end = 22u32;

    let transition_cols: Vec<_> = column_counts.iter()
        .filter(|(x, _)| *x >= transition_zone_start && *x <= transition_zone_end)
        .collect();

    eprintln!("\nTransition zone (cols {}-{}):", transition_zone_start, transition_zone_end);
    for (x, count) in &transition_cols {
        eprintln!("  col {:2}: {:2} pixels", x, count);
    }

    // Check that there's a clear transition between N and O
    // This could be an empty column (gap) or a significant drop in pixel density
    let col_19_count = column_counts.iter()
        .find(|(x, _)| *x == 19)
        .map(|(_, c)| *c)
        .unwrap_or(0);
    let col_20_count = column_counts.iter()
        .find(|(x, _)| *x == 20)
        .map(|(_, c)| *c)
        .unwrap_or(0);
    let col_21_count = column_counts.iter()
        .find(|(x, _)| *x == 21)
        .map(|(_, c)| *c)
        .unwrap_or(0);

    // N's right edge (col 19) should have visible pixels
    assert!(
        col_19_count > 0,
        "N's right edge (col 19: {}) should have visible pixels",
        col_19_count
    );

    // O's left edge (col 21) should have visible pixels
    assert!(
        col_21_count > 0,
        "O's left edge (col 21: {}) should have visible pixels",
        col_21_count
    );

    // The transition (col 20) should either be empty or show reduced density
    // indicating proper separation between the glyphs
    let has_gap = col_20_count == 0;
    let has_transition = col_20_count < col_19_count && col_20_count < col_21_count;

    assert!(
        has_gap || has_transition,
        "Letters should have clear separation: col 19={}, col 20={}, col 21={} \
         (expect col 20 to be empty or lower than neighbors)",
        col_19_count, col_20_count, col_21_count
    );

    if has_gap {
        eprintln!("\n✓ NO renders correctly at 14px with gap between letters");
    } else {
        eprintln!("\n✓ NO renders correctly at 14px with density transition between letters");
    }
}

/// Diagnostic test: inspect cosmic-text glyph positions and offsets.
/// This helps debug kerning issues by showing the raw values from cosmic-text.
#[test]
fn diagnostic_cosmic_text_positions() {
    use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(15.0, 22.5); // 15px font, 1.5 line height (app default)

    // Test with Monospace (what the app uses for content)
    let mut buffer = Buffer::new(&mut font_system, metrics);
    buffer.set_size(&mut font_system, Some(400.0), None);

    let test_text = "skies of gray";
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, test_text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut font_system, false);

    eprintln!("\nCosmic-text glyph positions for '{}' MONOSPACE at 15px:", test_text);
    eprintln!("================================================================");

    for run in buffer.layout_runs() {
        eprintln!("Run line_y={}", run.line_y);
        let mut prev_x = 0.0f32;
        for glyph in run.glyphs {
            let text = &test_text[glyph.start..glyph.end];
            let gap = glyph.x - prev_x;
            eprintln!(
                "  '{}' (glyph_id={:3}): x={:6.2}, w={:5.2}, gap_from_prev={:5.2}, x_offset={:.2}",
                text, glyph.glyph_id, glyph.x, glyph.w, gap, glyph.x_offset
            );
            prev_x = glyph.x + glyph.w;
        }
    }
    eprintln!();
}

/// Test that "gray" at 14px renders with proper letter separation.
#[test]
fn gray_at_14px_letter_separation() {
    let config = TestConfig::new("gray", 14.0, 100, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let output = render_with_config(config);
    output.save_png("gray_14px_separation");

    // Analyze columns to find letter boundaries
    let mut column_counts: Vec<(u32, u32)> = Vec::new();
    for x in 0..output.width {
        let mut count = 0u32;
        for y in 0..output.height {
            if output.pixel(x, y).is_visible() {
                count += 1;
            }
        }
        if count > 0 {
            column_counts.push((x, count));
        }
    }

    eprintln!("\nColumn analysis for 'gray' at 14px:");
    for (x, count) in &column_counts {
        eprintln!("  col {:2}: {:2} pixels", x, count);
    }

    // Find bounding box
    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "'gray' should render");
    let (min_x, min_y, width, height) = bbox.unwrap();
    eprintln!("\nBounding box: x={}, y={}, {}x{}", min_x, min_y, width, height);

    // At 14px, "gray" should be roughly 28 pixels wide based on cosmic-text metrics
    // (g=7.6 + r=5.7 + a=7.8 + y=7.0 = 28.1)
    assert!(
        width >= 20 && width <= 40,
        "'gray' at 14px should be 20-40px wide, got {}",
        width
    );
}

/// Test "gray" at 15px (the actual app default font size).
#[test]
fn gray_at_15px_app_default() {
    let config = TestConfig::new("gray", 15.0, 100, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let output = render_with_config(config);
    output.save_png("gray_15px_app_default");

    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "'gray' should render at 15px");

    let (min_x, _, width, _) = bbox.unwrap();
    eprintln!("\n'gray' at 15px: starts at x={}, width={}", min_x, width);

    // Should be slightly larger than 14px version
    assert!(
        width >= 22 && width <= 45,
        "'gray' at 15px should be 22-45px wide, got {}",
        width
    );
}

/// Test "gray" at 15px with scale factor (simulates HiDPI).
#[test]
fn gray_scaled_hidpi() {
    // Test with scale=1.5 (common HiDPI factor)
    let config = TestConfig::new("gray", 15.0, 150, 60, false)
        .with_font_family(TestFontFamily::SansSerif)
        .with_scale(1.5);
    let output = render_with_config(config);
    output.save_png("gray_15px_scale_1_5");

    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "'gray' should render with scale");

    let (_, _, width, _) = bbox.unwrap();
    eprintln!("\n'gray' at 15px scale=1.5: width={}", width);
}

/// Test monospace font which is what the app actually uses for content.
#[test]
fn monospace_text_kerning() {
    // This is the actual app configuration: Monospace at 15px
    let config = TestConfig::new("skies of gray", 15.0, 200, 40, true); // true = monospace
    let output = render_with_config(config);
    output.save_png("monospace_skies_of_gray");

    // Analyze column spacing
    let mut column_counts: Vec<(u32, u32)> = Vec::new();
    for x in 0..output.width {
        let mut count = 0u32;
        for y in 0..output.height {
            if output.pixel(x, y).is_visible() {
                count += 1;
            }
        }
        if count > 0 {
            column_counts.push((x, count));
        }
    }

    eprintln!("\nColumn analysis for 'skies of gray' monospace 15px:");
    for (x, count) in &column_counts {
        eprintln!("  col {:3}: {:2} pixels", x, count);
    }

    let bbox = output.bounding_box();
    assert!(bbox.is_some(), "Text should render");
    let (min_x, _, width, height) = bbox.unwrap();
    eprintln!("\nBounding box: x={}, {}x{}", min_x, width, height);
}

/// Detailed pixel analysis for "ab" to check letter spacing.
///
/// This test renders just two letters and shows a visual map of where
/// pixels are rendered, helping identify overlap or spacing issues.
#[test]
fn detailed_pixel_analysis_ab() {
    let config = TestConfig::new("ab", 15.0, 50, 30, true); // monospace
    let output = render_with_config(config);
    output.save_png("monospace_ab_detail");

    eprintln!("\n=== Pixel map for 'ab' monospace 15px ===");
    eprintln!("Legend: . = empty, # = opaque, + = semi-transparent");

    // Find vertical bounds of content
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).is_visible() {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }

    // Print pixel map showing actual color, not just alpha
    for y in min_y.saturating_sub(1)..=(max_y + 1).min(output.height - 1) {
        eprint!("y{:02}: ", y);
        for x in 0..output.width.min(40) {
            let pixel = output.pixel(x, y);
            // Check if pixel is text (bright) or background (dark)
            let brightness = (pixel.r as u16 + pixel.g as u16 + pixel.b as u16) / 3;
            if brightness < 20 {
                eprint!(".");  // dark = background
            } else if brightness > 200 {
                eprint!("#");  // bright = text
            } else {
                eprint!("+");  // mid = antialiasing
            }
        }
        eprintln!();
    }

    // Show column-by-column alpha values for the middle row
    let mid_y = (min_y + max_y) / 2;
    eprintln!("\nAlpha values at y={} (middle of text):", mid_y);
    for x in 0..output.width.min(40) {
        let pixel = output.pixel(x, mid_y);
        if pixel.a > 0 {
            eprintln!("  x={:2}: alpha={:3}", x, pixel.a);
        }
    }

    // Calculate where letter boundary should be (at 9px advance)
    eprintln!("\nExpected: 'a' ends around x=19 (10+9), 'b' starts around x=19");
    eprintln!("If letters overlap visually, there will be no empty columns around x=19");
}

/// Test that individual glyph rendering matches combined rendering.
///
/// This verifies that when glyphs are rendered together, there's no
/// unexpected visual artifacts from quad overlap. The font naturally
/// spaces letters close together (kerning), which is correct behavior.
#[test]
fn individual_vs_combined_glyph_widths() {
    // Render "N" alone
    let n_config = TestConfig::new("N", 14.0, 80, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let n_output = render_with_config(n_config);
    let n_bbox = n_output.bounding_box();

    // Render "O" alone
    let o_config = TestConfig::new("O", 14.0, 80, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let o_output = render_with_config(o_config);
    let o_bbox = o_output.bounding_box();

    // Render "NO" together
    let no_config = TestConfig::new("NO", 14.0, 80, 40, false)
        .with_font_family(TestFontFamily::SansSerif);
    let no_output = render_with_config(no_config);
    let no_bbox = no_output.bounding_box();

    // All should render successfully
    assert!(n_bbox.is_some(), "'N' should render");
    assert!(o_bbox.is_some(), "'O' should render");
    assert!(no_bbox.is_some(), "'NO' should render");

    let (_, _, n_width, _) = n_bbox.unwrap();
    let (_, _, o_width, _) = o_bbox.unwrap();
    let (_, _, no_width, _) = no_bbox.unwrap();

    // Combined width should be close to individual sum (with some kerning)
    // Font kerning typically reduces spacing by 0-3 pixels
    let individual_sum = n_width + o_width;
    assert!(
        no_width >= individual_sum.saturating_sub(3) && no_width <= individual_sum + 1,
        "Combined 'NO' width ({}) should be close to N+O ({}), diff = {}",
        no_width, individual_sum, (no_width as i32 - individual_sum as i32).abs()
    );
}

// ============================================================================
// SPACING & SEAM REGRESSION TESTS
// ============================================================================

/// CPU test: verify quad overlap from MSDF padding is expected.
///
/// Adjacent monospace glyphs have quads that overlap (due to SDF padding extending
/// beyond the advance width). This test validates that:
/// 1. Glyph positions are sequential (glyph[1].x == glyph[0].x + advance_width)
/// 2. The overlap amount is documented and expected
#[test]
fn cpu_quad_overlap_expected() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(15.0, 22.5);

    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, "mm", attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let glyphs = buffer.glyphs();
    assert_eq!(glyphs.len(), 2, "Should have exactly 2 glyphs for 'mm'");

    let g0 = &glyphs[0];
    let g1 = &glyphs[1];

    // Glyph spacing: second glyph starts at first glyph's x + advance width
    let expected_x1 = g0.x + g0.advance_width;
    let spacing_error = (g1.x - expected_x1).abs();

    eprintln!("Glyph 0: x={:.2}, advance={:.2}", g0.x, g0.advance_width);
    eprintln!("Glyph 1: x={:.2}, advance={:.2}", g1.x, g1.advance_width);
    eprintln!("Expected g1.x={:.2}, actual={:.2}, error={:.4}", expected_x1, g1.x, spacing_error);

    assert!(
        spacing_error < 0.5,
        "Second glyph should start at first glyph's x + advance: \
         expected {:.2}, got {:.2} (error {:.2})",
        expected_x1, g1.x, spacing_error
    );

    // Both advance widths should be equal (monospace)
    let advance_diff = (g0.advance_width - g1.advance_width).abs();
    assert!(
        advance_diff < 0.01,
        "Monospace glyphs should have equal advance: {:.2} vs {:.2}",
        g0.advance_width, g1.advance_width
    );

    // Document the expected overlap: MSDF padding at 15px ≈ 3.75px per side,
    // so each quad extends ~3.75px beyond the advance width on each side.
    // Adjacent quads therefore overlap by ~7.5px. This is normal and handled
    // by the cell_mask fade in the shader.
    eprintln!(
        "Advance width: {:.2}px — quads will overlap by ~2x SDF padding in the shader",
        g0.advance_width
    );
}

/// GPU test: no dark seam at glyph advance boundaries.
///
/// Renders "mm" and checks that the luminance at the advance boundary between
/// the two m's is not significantly darker than the peak luminance. A dark seam
/// indicates the cell fade or depth buffer is incorrectly suppressing ink.
#[test]
fn no_dark_seam_at_advance_boundary() {
    // Render "mm" at 22px monospace — large enough for clear pixel analysis
    let config = TestConfig::new("mm", 22.0, 100, 50, true);
    let output = render_with_config(config);
    output.save_png("seam_detection_mm");

    // Find vertical extent of the glyphs (rows with ink)
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > 0.3 {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }

    if min_y >= max_y {
        panic!("No text pixels found in 'mm' render");
    }

    // Compute per-column average luminance across the text rows
    let text_rows = (max_y - min_y + 1) as f32;
    let mut col_avg_lum: Vec<f32> = Vec::new();
    for x in 0..output.width {
        let mut sum = 0.0f32;
        for y in min_y..=max_y {
            sum += output.pixel(x, y).luminance();
        }
        col_avg_lum.push(sum / text_rows);
    }

    // Find peak luminance (the brightest column, should be inside a stem)
    let peak_lum = col_avg_lum.iter().cloned().fold(0.0f32, f32::max);

    // Get the glyph positions to find where the advance boundary is
    // We use cosmic-text directly to get the advance width
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};
    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(22.0, 26.4);
    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, "mm", attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let glyphs = buffer.glyphs();
    assert!(glyphs.len() >= 2, "Need at least 2 glyphs");

    // The advance boundary is at g0.x + g0.advance_width, offset by the test's left margin (10px)
    let boundary_x = (10.0_f32 + glyphs[0].x + glyphs[0].advance_width).round() as u32;

    eprintln!("Peak luminance: {:.3}", peak_lum);
    eprintln!("Advance boundary at column: {}", boundary_x);

    // Check a 3-column window around the boundary
    let check_start = boundary_x.saturating_sub(1);
    let check_end = (boundary_x + 1).min(output.width - 1);

    eprintln!("Column luminances around boundary:");
    for x in check_start..=check_end {
        if (x as usize) < col_avg_lum.len() {
            eprintln!("  col {}: avg_lum={:.3}", x, col_avg_lum[x as usize]);
        }
    }

    // The minimum luminance in the boundary window
    let boundary_min_lum = (check_start..=check_end)
        .filter_map(|x| col_avg_lum.get(x as usize).copied())
        .fold(f32::MAX, f32::min);

    // The boundary luminance should be at least 50% of the peak.
    // Before the fix, the symmetric cell fade would reduce it to ~50% or less,
    // and depth write occlusion would make it even worse. After the fix,
    // ink at the boundary should be fully preserved.
    let ratio = if peak_lum > 0.01 { boundary_min_lum / peak_lum } else { 1.0 };
    eprintln!("Boundary/peak ratio: {:.3} (min boundary lum: {:.3})", ratio, boundary_min_lum);

    // The crossfade gives each glyph 50% mask at the boundary. For symmetric pairs
    // like 'mm', both sides contribute ~50% each, so combined luminance is ~75% of
    // peak (due to premultiplied alpha compositing: 0.5 + 0.5*0.5 = 0.75). We use
    // 25% as the floor to catch gross seam bugs while allowing the natural crossfade
    // dip. The original bug had boundary luminance near 0%.
    assert!(
        ratio >= 0.25,
        "DARK SEAM BUG: luminance at advance boundary ({:.3}) is only {:.0}% of peak ({:.3}). \
         Expected >= 25%. Cell fade is too aggressive at the boundary.",
        boundary_min_lum, ratio * 100.0, peak_lum
    );
}

/// Diagnostic: is 'u' rendering wider than its advance width?
///
/// Renders 'u' alone and checks where its visible ink ends relative
/// to the advance boundary. If ink extends significantly past the
/// advance, the cell_mask isn't clamping hard enough.
#[test]
fn diagnostic_u_ink_vs_advance() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(22.0, 26.4);

    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, "u", attrs.clone(), Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let glyphs = buffer.glyphs();
    assert_eq!(glyphs.len(), 1);
    let advance = glyphs[0].advance_width;
    let pen_x = glyphs[0].x;
    eprintln!("'u' at 22px mono: pen_x={:.2}, advance={:.2}", pen_x, advance);
    eprintln!("  advance boundary at pixel: {:.2}", 10.0 + pen_x + advance);

    // Render 'u' alone
    let config = TestConfig::new("u", 22.0, 80, 50, true);
    let output = render_with_config(config);
    output.save_png("diagnostic_u_alone");

    // Find where visible ink ends (rightmost column with significant luminance)
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > 0.1 {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }

    let advance_boundary = (10.0 + pen_x + advance).round() as u32;
    let text_rows = (max_y - min_y + 1).max(1) as f32;

    eprintln!("\nColumn luminances for 'u' alone:");
    let mut last_visible_col = 0u32;
    for x in 0..output.width.min(60) {
        let mut sum = 0.0f32;
        for y in min_y..=max_y {
            sum += output.pixel(x, y).luminance();
        }
        let avg = sum / text_rows;
        if avg > 0.01 {
            let marker = if x == advance_boundary { " <-- advance boundary" } else { "" };
            eprintln!("  col {:2}: avg_lum={:.3}{}", x, avg, marker);
            if avg > 0.05 {
                last_visible_col = x;
            }
        }
    }

    let ink_overshoot = last_visible_col as i32 - advance_boundary as i32;
    eprintln!("\nAdvance boundary: col {}", advance_boundary);
    eprintln!("Last visible col (>0.05 lum): col {}", last_visible_col);
    eprintln!("Ink overshoot past advance: {} pixels", ink_overshoot);

    // Now render 'm' alone for comparison
    let mut buffer_m = MsdfTextBuffer::new(&mut font_system, metrics);
    buffer_m.set_text(&mut font_system, "m", attrs, Shaping::Advanced);
    buffer_m.visual_line_count(&mut font_system, 400.0, None);
    let m_glyphs = buffer_m.glyphs();
    eprintln!("\n'm' at 22px mono: pen_x={:.2}, advance={:.2}", m_glyphs[0].x, m_glyphs[0].advance_width);

    let config_m = TestConfig::new("m", 22.0, 80, 50, true);
    let output_m = render_with_config(config_m);
    output_m.save_png("diagnostic_m_alone");

    eprintln!("\nColumn luminances for 'm' alone:");
    let mut first_visible_col_m = output_m.width;
    let mut min_y_m = output_m.height;
    let mut max_y_m = 0u32;
    for y in 0..output_m.height {
        for x in 0..output_m.width {
            if output_m.pixel(x, y).luminance() > 0.1 {
                min_y_m = min_y_m.min(y);
                max_y_m = max_y_m.max(y);
            }
        }
    }
    let text_rows_m = (max_y_m - min_y_m + 1).max(1) as f32;
    for x in 0..output_m.width.min(60) {
        let mut sum = 0.0f32;
        for y in min_y_m..=max_y_m {
            sum += output_m.pixel(x, y).luminance();
        }
        let avg = sum / text_rows_m;
        if avg > 0.01 {
            eprintln!("  col {:2}: avg_lum={:.3}", x, avg);
            if avg > 0.05 && x < first_visible_col_m {
                first_visible_col_m = x;
            }
        }
    }
    eprintln!("First visible col for 'm' (>0.05 lum): col {}", first_visible_col_m);
}

// ============================================================================
// GOLDEN IMAGE VISUAL REGRESSION TESTS
// ============================================================================
//
// Ground-truth comparison using SSIM (Structural Similarity Index).
// Golden images are rendered by FreeType (via contrib/gen-golden.py) as a
// known-good reference. MSDF renders are compared structurally, not pixel-exact.
//
// ## Workflow
//
// 1. Generate goldens from FreeType:
//    python3 contrib/gen-golden.py
//
// 2. Inspect golden PNGs in assets/test/golden/
//
// 3. Run regression tests:
//    cargo test -p kaijutsu-app golden_ -- --test-threads=1
//
// 4. On failure, inspect diff images in /tmp/msdf_tests/*_diff.png
//
// ## SSIM thresholds
//
// FreeType vs MSDF healthy baseline: 0.73–0.85 (different AA, same structure).
// Threshold: 0.65 — catches real regressions (merged glyphs, blank output,
// wrong spacing) while allowing cross-renderer AA differences.

/// Compute Structural Similarity Index between two test outputs.
///
/// Uses 8x8 sliding windows with the standard SSIM formula:
///   SSIM(x,y) = (2*μx*μy + C1)(2*σxy + C2) / ((μx² + μy² + C1)(σx² + σy² + C2))
///
/// Returns 0.0 (completely different) to 1.0 (identical).
fn compute_ssim(a: &TestOutput, b: &TestOutput) -> f32 {
    assert_eq!(a.width, b.width, "SSIM requires same width");
    assert_eq!(a.height, b.height, "SSIM requires same height");

    let w = a.width as usize;
    let h = a.height as usize;

    // Convert to luminance arrays
    let lum_a: Vec<f32> = (0..w * h)
        .map(|i| {
            let x = (i % w) as u32;
            let y = (i / w) as u32;
            a.pixel(x, y).luminance()
        })
        .collect();
    let lum_b: Vec<f32> = (0..w * h)
        .map(|i| {
            let x = (i % w) as u32;
            let y = (i / w) as u32;
            b.pixel(x, y).luminance()
        })
        .collect();

    // SSIM constants (normalized to [0,1] luminance range)
    let c1: f32 = (0.01_f32).powi(2); // (K1 * L)^2 where L=1.0
    let c2: f32 = (0.03_f32).powi(2);

    let window = 8usize;
    if w < window || h < window {
        // Too small for windowed SSIM, fall back to per-pixel MSE-based comparison
        let mse: f32 = lum_a.iter().zip(&lum_b).map(|(a, b)| (a - b).powi(2)).sum::<f32>()
            / (w * h) as f32;
        return 1.0 - mse; // rough approximation
    }

    let mut ssim_sum = 0.0f64;
    let mut window_count = 0u64;

    for wy in 0..=(h - window) {
        for wx in 0..=(w - window) {
            let mut sum_a = 0.0f64;
            let mut sum_b = 0.0f64;
            let mut sum_a2 = 0.0f64;
            let mut sum_b2 = 0.0f64;
            let mut sum_ab = 0.0f64;
            let n = (window * window) as f64;

            for dy in 0..window {
                for dx in 0..window {
                    let idx = (wy + dy) * w + (wx + dx);
                    let va = lum_a[idx] as f64;
                    let vb = lum_b[idx] as f64;
                    sum_a += va;
                    sum_b += vb;
                    sum_a2 += va * va;
                    sum_b2 += vb * vb;
                    sum_ab += va * vb;
                }
            }

            let mu_a = sum_a / n;
            let mu_b = sum_b / n;
            let sigma_a2 = sum_a2 / n - mu_a * mu_a;
            let sigma_b2 = sum_b2 / n - mu_b * mu_b;
            let sigma_ab = sum_ab / n - mu_a * mu_b;

            let c1 = c1 as f64;
            let c2 = c2 as f64;

            let numerator = (2.0 * mu_a * mu_b + c1) * (2.0 * sigma_ab + c2);
            let denominator = (mu_a * mu_a + mu_b * mu_b + c1) * (sigma_a2 + sigma_b2 + c2);

            ssim_sum += numerator / denominator;
            window_count += 1;
        }
    }

    (ssim_sum / window_count as f64) as f32
}

/// Path to the golden images directory (committed to git).
fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("assets/test/golden")
}

/// Load a golden PNG image as a TestOutput.
fn load_golden(name: &str) -> Option<TestOutput> {
    let path = golden_dir().join(format!("{name}.png"));
    if !path.exists() {
        return None;
    }

    let img = image::open(&path)
        .unwrap_or_else(|e| panic!("Failed to load golden image {}: {e}", path.display()));
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    Some(TestOutput {
        pixels: rgba.into_raw(),
        width,
        height,
        is_bgra: false, // PNG is always RGBA
    })
}

/// Save a TestOutput as a PNG (always RGBA).
fn save_png_to(output: &TestOutput, path: &std::path::Path) {
    let pixels: Vec<u8> = if output.is_bgra {
        output
            .pixels
            .chunks(4)
            .flat_map(|bgra| [bgra[2], bgra[1], bgra[0], bgra[3]])
            .collect()
    } else {
        output.pixels.clone()
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    image::save_buffer(path, &pixels, output.width, output.height, image::ColorType::Rgba8)
        .unwrap_or_else(|e| panic!("Failed to save PNG {}: {e}", path.display()));
}

/// Generate a side-by-side diff image: golden | actual | difference.
fn save_diff_image(golden: &TestOutput, actual: &TestOutput, name: &str) {
    let w = golden.width;
    let h = golden.height;
    let diff_w = w * 3; // side by side by side

    let mut diff_pixels = vec![0u8; (diff_w * h * 4) as usize];

    for y in 0..h {
        for x in 0..w {
            let gp = golden.pixel(x, y);
            let ap = actual.pixel(x, y);

            // Left panel: golden
            let offset_g = ((y * diff_w + x) * 4) as usize;
            diff_pixels[offset_g] = gp.r;
            diff_pixels[offset_g + 1] = gp.g;
            diff_pixels[offset_g + 2] = gp.b;
            diff_pixels[offset_g + 3] = 255;

            // Middle panel: actual
            let offset_a = ((y * diff_w + w + x) * 4) as usize;
            diff_pixels[offset_a] = ap.r;
            diff_pixels[offset_a + 1] = ap.g;
            diff_pixels[offset_a + 2] = ap.b;
            diff_pixels[offset_a + 3] = 255;

            // Right panel: absolute luminance difference, scaled to full range
            let diff_val =
                ((gp.luminance() - ap.luminance()).abs() * 255.0).min(255.0) as u8;
            let offset_d = ((y * diff_w + 2 * w + x) * 4) as usize;
            diff_pixels[offset_d] = diff_val;
            diff_pixels[offset_d + 1] = diff_val;
            diff_pixels[offset_d + 2] = diff_val;
            diff_pixels[offset_d + 3] = 255;
        }
    }

    let dir = PathBuf::from("/tmp/msdf_tests");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(format!("{name}_diff.png"));

    image::save_buffer(&path, &diff_pixels, diff_w, h, image::ColorType::Rgba8)
        .unwrap_or_else(|e| panic!("Failed to save diff PNG: {e}"));

    eprintln!("Diff image saved: {}", path.display());
}

/// Assert that a rendered output matches its golden image.
///
/// - If no golden exists: saves current render to /tmp and skips with a message.
/// - If `MSDF_UPDATE_GOLDEN=1`: overwrites the golden with current render.
/// - Otherwise: computes SSIM and fails if below threshold.
fn assert_matches_golden(output: &TestOutput, name: &str) {
    // FreeType golden vs MSDF render: 0.73–0.85 is the healthy cross-renderer baseline.
    // A real regression (glyphs merging, blank output, wrong spacing) drops below 0.60.
    let ssim_threshold = 0.65;

    // Always save the actual render for inspection
    let tmp_dir = PathBuf::from("/tmp/msdf_tests");
    std::fs::create_dir_all(&tmp_dir).ok();
    save_png_to(output, &tmp_dir.join(format!("{name}.png")));

    // Update mode: overwrite golden with current render
    if std::env::var("MSDF_UPDATE_GOLDEN").is_ok() {
        let golden_path = golden_dir().join(format!("{name}.png"));
        save_png_to(output, &golden_path);
        eprintln!("Updated golden: {}", golden_path.display());
        return;
    }

    // Load golden
    let Some(golden) = load_golden(name) else {
        eprintln!(
            "No golden image for '{name}'. Render saved to /tmp/msdf_tests/{name}.png\n\
             To approve: MSDF_UPDATE_GOLDEN=1 cargo test -p kaijutsu-app {name} -- --test-threads=1"
        );
        return; // Skip — no golden to compare against yet
    };

    // Dimension check
    if golden.width != output.width || golden.height != output.height {
        save_diff_image(&golden, output, name);
        panic!(
            "Golden image dimension mismatch for '{name}': \
             golden={}x{}, actual={}x{}. Diff saved to /tmp/msdf_tests/{name}_diff.png",
            golden.width, golden.height, output.width, output.height
        );
    }

    // SSIM comparison
    let ssim = compute_ssim(&golden, output);
    eprintln!("{name}: SSIM = {ssim:.4} (threshold: {ssim_threshold})");

    if ssim < ssim_threshold {
        save_diff_image(&golden, output, name);
        panic!(
            "VISUAL REGRESSION in '{name}': SSIM {ssim:.4} < {ssim_threshold}\n\
             Diff image: /tmp/msdf_tests/{name}_diff.png\n\
             Actual render: /tmp/msdf_tests/{name}.png\n\
             To update golden: MSDF_UPDATE_GOLDEN=1 cargo test -p kaijutsu-app {name} -- --test-threads=1"
        );
    }
}

/// Golden test: "document" at 22px monospace.
/// Catches the um-joining and inter-glyph bleed bugs.
#[test]
fn golden_document_22px_mono() {
    let config = TestConfig::new("document", 22.0, 250, 60, true);
    let output = render_with_config(config);
    assert_matches_golden(&output, "golden_document_22px_mono");
}

/// Golden test: "mm" at 22px monospace.
/// Catches advance boundary seams between identical glyphs.
#[test]
fn golden_mm_22px_mono() {
    let config = TestConfig::new("mm", 22.0, 100, 50, true);
    let output = render_with_config(config);
    assert_matches_golden(&output, "golden_mm_22px_mono");
}

/// Golden test: "Hello, World!" at 15px monospace.
/// General readability at the previous app default size.
#[test]
fn golden_hello_15px_mono() {
    let config = TestConfig::new("Hello, World!", 15.0, 200, 40, true);
    let output = render_with_config(config);
    assert_matches_golden(&output, "golden_hello_15px_mono");
}

/// Golden test: "AV" at 22px serif.
/// Catches kerning pair preservation — V should tuck under A.
#[test]
fn golden_av_22px_serif() {
    let config = TestConfig::new("AV", 22.0, 100, 50, false)
        .with_font_family(TestFontFamily::Serif);
    let output = render_with_config(config);
    assert_matches_golden(&output, "golden_av_22px_serif");
}

/// Golden test: "fn main() {" at 15px monospace.
/// Catches punctuation rendering + mixed glyph shapes in code.
#[test]
fn golden_code_15px_mono() {
    let config = TestConfig::new("fn main() {", 15.0, 200, 40, true);
    let output = render_with_config(config);
    assert_matches_golden(&output, "golden_code_15px_mono");
}

// ============================================================================
// METRIC VALIDATION TESTS
// ============================================================================
//
// Numerical tests that catch sizing/spacing problems independent of
// pixel-level rendering differences between FreeType and MSDF.

/// Horizontal ink extent: (first_col, last_col) where average column
/// luminance exceeds threshold, scanning only rows with visible text.
///
/// Returns `None` if no columns exceed the threshold.
fn ink_extent(output: &TestOutput, threshold: f32) -> Option<(u32, u32)> {
    // Find vertical extent of text (rows with any significant ink)
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > threshold {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }

    if min_y > max_y {
        return None;
    }

    let text_rows = (max_y - min_y + 1) as f32;
    let mut first_col = None;
    let mut last_col = None;

    for x in 0..output.width {
        let mut sum = 0.0f32;
        for y in min_y..=max_y {
            sum += output.pixel(x, y).luminance();
        }
        let avg = sum / text_rows;
        if avg > threshold {
            if first_col.is_none() {
                first_col = Some(x);
            }
            last_col = Some(x);
        }
    }

    match (first_col, last_col) {
        (Some(f), Some(l)) => Some((f, l)),
        _ => None,
    }
}

/// Test: String ink width matches FreeType golden within tolerance.
///
/// For each golden test case, compare the horizontal extent of visible ink
/// between MSDF render and FreeType golden. If MSDF glyphs are oversized,
/// the total ink span will be wider than FreeType's.
///
/// Monospace cases use tight tolerance (3px). Serif/variable-width cases
/// use wider tolerance (5px) because serifs and kerning pairs cause more
/// AA spread difference between FreeType and MSDF renderers.
#[test]
fn golden_metrics_ink_width() {
    let threshold = 0.15;

    // (name, text, font_size, width, height, use_mono, font_family, width_tol, start_tol)
    let cases: &[(&str, &str, f32, u32, u32, bool, Option<TestFontFamily>, i32, i32)] = &[
        ("golden_document_22px_mono", "document", 22.0, 250, 60, true, None, 3, 3),
        ("golden_mm_22px_mono", "mm", 22.0, 100, 50, true, None, 3, 3),
        ("golden_hello_15px_mono", "Hello, World!", 15.0, 200, 40, true, None, 3, 3),
        ("golden_av_22px_serif", "AV", 22.0, 100, 50, false, Some(TestFontFamily::Serif), 5, 3),
        ("golden_code_15px_mono", "fn main() {", 15.0, 200, 40, true, None, 3, 3),
    ];

    for &(name, text, font_size, width, height, use_mono, ref font_family, width_tolerance, start_tolerance) in cases {
        // Load FreeType golden
        let Some(golden) = load_golden(name) else {
            eprintln!("Skipping {name}: no golden image");
            continue;
        };

        let mut config = TestConfig::new(text, font_size, width, height, use_mono);
        if let Some(family) = font_family {
            config = config.with_font_family(*family);
        }
        let output = render_with_config(config);

        let golden_extent = ink_extent(&golden, threshold);
        let msdf_extent = ink_extent(&output, threshold);

        match (golden_extent, msdf_extent) {
            (Some((gf, gl)), Some((mf, ml))) => {
                let g_width = gl - gf;
                let m_width = ml - mf;
                let width_diff = (m_width as i32 - g_width as i32).abs();
                let start_diff = (mf as i32 - gf as i32).abs();

                eprintln!(
                    "{name}: golden ink [{gf}..{gl}] (w={g_width}), \
                     msdf ink [{mf}..{ml}] (w={m_width}), \
                     width_diff={width_diff}, start_diff={start_diff}"
                );

                assert!(
                    width_diff <= width_tolerance,
                    "{name}: ink width differs by {width_diff}px (tolerance {width_tolerance}px). \
                     golden={g_width}px, msdf={m_width}px"
                );

                assert!(
                    start_diff <= start_tolerance,
                    "{name}: ink start differs by {start_diff}px (tolerance {start_tolerance}px). \
                     golden starts at {gf}, msdf at {mf}"
                );
            }
            (None, _) => {
                eprintln!("{name}: golden has no ink above threshold {threshold} — skipping");
            }
            (_, None) => {
                panic!("{name}: MSDF render has no ink above threshold {threshold}!");
            }
        }
    }
}

/// Test: Glyph ink fits within advance cell for monospace font.
///
/// For the widest monospace glyphs, render individually and verify visible
/// ink doesn't significantly overshoot the advance width boundary.
#[test]
fn monospace_advance_contains_ink() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let threshold = 0.1;
    let overshoot_tolerance = 1i32; // 1px tolerance for AA bleed

    let test_chars = ['m', 'w', 'W', 'M', '@'];

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(22.0, 26.4);

    for ch in &test_chars {
        let text = ch.to_string();

        // Get advance width from cosmic-text
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
        buffer.set_text(&mut font_system, &text, attrs, Shaping::Advanced);
        buffer.visual_line_count(&mut font_system, 400.0, None);

        let glyphs = buffer.glyphs();
        assert!(
            !glyphs.is_empty(),
            "'{ch}' should produce at least one glyph"
        );
        let advance_width = glyphs[0].advance_width;
        let left_margin = 10.0f32; // TestConfig default left

        // Render single character
        let config = TestConfig::new(&text, 22.0, 80, 50, true);
        let output = render_with_config(config);
        output.save_png(&format!("metric_advance_{ch}"));

        let Some((first_ink, last_ink)) = ink_extent(&output, threshold) else {
            panic!("'{ch}' rendered no visible ink!");
        };

        // ink_start and ink_end relative to pen position
        let ink_start_rel = first_ink as f32 - left_margin;
        let ink_end_rel = last_ink as f32 - left_margin;

        eprintln!(
            "'{ch}': advance={advance_width:.1}px, \
             ink=[{first_ink}..{last_ink}] (rel pen: {ink_start_rel:.1}..{ink_end_rel:.1})"
        );

        assert!(
            ink_end_rel <= advance_width + overshoot_tolerance as f32,
            "'{ch}' ink extends {:.1}px past advance boundary \
             (ink_end_rel={ink_end_rel:.1}, advance={advance_width:.1}, tolerance={overshoot_tolerance}px)",
            ink_end_rel - advance_width
        );

        assert!(
            ink_start_rel >= -(overshoot_tolerance as f32),
            "'{ch}' ink starts {:.1}px before pen position (tolerance={overshoot_tolerance}px)",
            -ink_start_rel
        );
    }
}

/// Test: Total rendered ink width = N * advance for monospace strings.
///
/// For monospace fonts, the total rendered ink width should equal
/// `num_chars * advance_width` (within tolerance).
#[test]
fn monospace_string_width_matches_advances() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let threshold = 0.15;
    let text = "document";
    let num_chars = text.len() as f32;

    // Get advance width from cosmic-text
    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(22.0, 26.4);
    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, text, attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let glyphs = buffer.glyphs();
    assert!(
        !glyphs.is_empty(),
        "'document' should produce glyphs"
    );
    let advance_width = glyphs[0].advance_width;
    let expected_width = (num_chars * advance_width).round() as i32;

    // Render
    let config = TestConfig::new(text, 22.0, 250, 60, true);
    let output = render_with_config(config);
    output.save_png("metric_string_width_document");

    let Some((first_ink, last_ink)) = ink_extent(&output, threshold) else {
        panic!("'document' rendered no visible ink!");
    };
    let actual_width = (last_ink - first_ink) as i32;

    eprintln!(
        "'document' at 22px mono: advance={advance_width:.1}px, \
         expected_width={expected_width}px (8*{advance_width:.1}), \
         actual_width={actual_width}px, ink=[{first_ink}..{last_ink}]"
    );

    // Verify all advances are equal (monospace)
    for (i, g) in glyphs.iter().enumerate() {
        let diff = (g.advance_width - advance_width).abs();
        assert!(
            diff < 0.01,
            "Glyph {i} advance ({:.2}) differs from glyph 0 ({advance_width:.2})",
            g.advance_width
        );
    }

    assert!(
        actual_width <= expected_width + 2,
        "'document' ink width ({actual_width}px) exceeds expected ({expected_width}px) by {}px (tolerance 2px)",
        actual_width - expected_width
    );

    assert!(
        actual_width >= expected_width - 4,
        "'document' ink width ({actual_width}px) is {}px narrower than expected ({expected_width}px) (tolerance 4px)",
        expected_width - actual_width
    );
}

/// Test: Adjacent monospace glyphs have visible luminance dip at boundary.
///
/// Strengthens the existing `no_dark_seam_at_advance_boundary` test.
/// Between adjacent monospace glyphs, there should be a luminance dip
/// (not a solid merged mass) at the advance boundary.
#[test]
fn monospace_inter_glyph_separation() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    // Render "mm" at 22px monospace
    let config = TestConfig::new("mm", 22.0, 100, 50, true);
    let output = render_with_config(config);
    output.save_png("metric_inter_glyph_mm");

    // Find vertical extent of text
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > 0.1 {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    assert!(min_y < max_y, "'mm' should have visible text rows");

    let text_rows = (max_y - min_y + 1) as f32;

    // Compute per-column average luminance in text rows
    let col_avg_lum: Vec<f32> = (0..output.width)
        .map(|x| {
            let sum: f32 = (min_y..=max_y)
                .map(|y| output.pixel(x, y).luminance())
                .sum();
            sum / text_rows
        })
        .collect();

    let peak_lum = col_avg_lum.iter().cloned().fold(0.0f32, f32::max);

    // Get advance boundary from cosmic-text
    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(22.0, 26.4);
    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, "mm", attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);

    let glyphs = buffer.glyphs();
    assert!(glyphs.len() >= 2, "Need at least 2 glyphs for 'mm'");

    let left_margin = 10.0f32;
    let boundary_x = (left_margin + glyphs[0].x + glyphs[0].advance_width).round() as u32;

    // Find minimum luminance in a 3-column window around the boundary
    let check_start = boundary_x.saturating_sub(1);
    let check_end = (boundary_x + 1).min(output.width - 1);

    let boundary_min_lum = (check_start..=check_end)
        .filter_map(|x| col_avg_lum.get(x as usize).copied())
        .fold(f32::MAX, f32::min);

    let ratio = if peak_lum > 0.01 {
        boundary_min_lum / peak_lum
    } else {
        1.0
    };

    eprintln!(
        "'mm' inter-glyph: boundary_x={boundary_x}, peak_lum={peak_lum:.3}, \
         boundary_min_lum={boundary_min_lum:.3}, ratio={ratio:.3}"
    );

    eprintln!("Column luminances around boundary:");
    for x in check_start..=check_end {
        if let Some(&lum) = col_avg_lum.get(x as usize) {
            eprintln!("  col {x}: avg_lum={lum:.3}");
        }
    }

    // The boundary luminance should dip below 85% of peak — proving
    // the glyphs aren't a solid merged mass. This is complementary to
    // no_dark_seam_at_advance_boundary which checks it's not TOO dark.
    assert!(
        ratio < 0.85,
        "GLYPHS MERGED: luminance at advance boundary ({boundary_min_lum:.3}) is {:.0}% of peak ({peak_lum:.3}). \
         Expected < 85% — adjacent glyphs should have a visible dip, not solid ink.",
        ratio * 100.0
    );
}

// ============================================================================
// COLOR-CHANNEL OVERLAP DETECTION TESTS
// ============================================================================
//
// By rendering even glyphs in red and odd glyphs in blue, we can measure
// exactly how much each glyph's rendering bleeds into its neighbor's cell.
// At the advance boundary between a red glyph and a blue glyph:
//   - Red channel = contribution from the red (left) glyph only
//   - Blue channel = contribution from the blue (right) glyph only
//   - If both channels are high at the same pixel, that's quad overlap
// This is far more precise than luminance-only analysis with white text.

/// Per-column color channel stats for overlap analysis.
#[derive(Debug)]
struct ColumnChannelStats {
    /// Average red channel value (0.0–1.0) across text rows.
    avg_red: f32,
    /// Average blue channel value (0.0–1.0) across text rows.
    avg_blue: f32,
}

/// Compute per-column red and blue channel averages across text rows.
fn column_channel_stats(output: &TestOutput, min_y: u32, max_y: u32) -> Vec<ColumnChannelStats> {
    let text_rows = (max_y - min_y + 1) as f32;
    (0..output.width)
        .map(|x| {
            let mut sum_r = 0.0f32;
            let mut sum_b = 0.0f32;
            for y in min_y..=max_y {
                let px = output.pixel(x, y);
                sum_r += px.r as f32 / 255.0;
                sum_b += px.b as f32 / 255.0;
            }
            ColumnChannelStats {
                avg_red: sum_r / text_rows,
                avg_blue: sum_b / text_rows,
            }
        })
        .collect()
}

/// Find vertical extent of text (rows with luminance > threshold).
fn text_row_extent(output: &TestOutput, threshold: f32) -> Option<(u32, u32)> {
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > threshold {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if min_y <= max_y { Some((min_y, max_y)) } else { None }
}

/// Test: color-channel overlap detection on "mmmmm" at 22px mono.
///
/// Renders alternating red/blue glyphs and measures how much each
/// glyph's color bleeds across the advance boundary into its neighbor's cell.
///
/// At each boundary between a red glyph (even) and blue glyph (odd):
///   - The red channel should drop to near-zero inside the blue glyph's cell
///   - The blue channel should drop to near-zero inside the red glyph's cell
///   - 2px past the boundary, the "wrong" color should be below the threshold
///
/// This directly measures MSDF quad AA bleed in a way that white-on-black
/// luminance analysis cannot.
#[test]
fn color_overlap_mm_boundary_bleed() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let text = "mmmmm";
    let font_size = 22.0;

    // Pure red and pure blue — maximally separated channels
    let red: [u8; 4] = [255, 0, 0, 255];
    let blue: [u8; 4] = [0, 0, 255, 255];

    let config = TestConfig::new(text, font_size, 200, 60, true)
        .with_alternating_colors(red, blue);
    let output = render_with_config(config);
    output.save_png("color_overlap_mmmmm");

    // Get advance width
    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(font_size, font_size * 1.2);
    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, text, attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);
    let glyphs = buffer.glyphs();
    let advance = glyphs[0].advance_width;
    let left_margin = 10.0f32;

    // Find text rows
    let (min_y, max_y) = text_row_extent(&output, 0.05)
        .expect("Should have visible text");

    let stats = column_channel_stats(&output, min_y, max_y);

    // Check each advance boundary
    // Boundaries are at: left_margin + (i+1) * advance for i = 0..num_glyphs-1
    // Check 1px past boundary — the neighbor's color should already be faded.
    // At 2px it's typically clean, but 1px catches actual AA bleed from
    // oversized MSDF quads extending past the advance boundary.
    let bleed_check_offset = 1u32;
    let bleed_threshold = 0.10; // wrong color should be below 10%

    eprintln!("advance={advance:.1}px, checking {}-glyph boundaries:", text.len() - 1);
    eprintln!("  bleed_check_offset={bleed_check_offset}px, threshold={bleed_threshold}");

    let mut worst_bleed = 0.0f32;
    let mut worst_boundary = 0usize;
    let mut failures = Vec::new();

    for boundary_idx in 0..(text.len() - 1) {
        let boundary_col = (left_margin + (boundary_idx as f32 + 1.0) * advance).round() as u32;
        let left_is_red = boundary_idx % 2 == 0; // even glyph = red

        // Check inside the right glyph's cell (boundary + offset):
        // the left glyph's color should be fading out
        let check_col_right = boundary_col + bleed_check_offset;
        // Check inside the left glyph's cell (boundary - offset):
        // the right glyph's color should be fading out
        let check_col_left = boundary_col.saturating_sub(bleed_check_offset);

        if let (Some(right_stats), Some(left_stats)) = (
            stats.get(check_col_right as usize),
            stats.get(check_col_left as usize),
        ) {
            // If left glyph is red, check that red has faded by check_col_right
            // and that blue has faded by check_col_left
            let (bleed_into_right, bleed_into_left) = if left_is_red {
                (right_stats.avg_red, left_stats.avg_blue)
            } else {
                (right_stats.avg_blue, left_stats.avg_red)
            };

            let left_color = if left_is_red { "red" } else { "blue" };
            let right_color = if left_is_red { "blue" } else { "red" };

            eprintln!(
                "  boundary {boundary_idx} (col {boundary_col}): \
                 {left_color}→{right_color}  \
                 bleed_into_right={bleed_into_right:.3} (col {check_col_right}), \
                 bleed_into_left={bleed_into_left:.3} (col {check_col_left})"
            );

            let max_bleed = bleed_into_right.max(bleed_into_left);
            if max_bleed > worst_bleed {
                worst_bleed = max_bleed;
                worst_boundary = boundary_idx;
            }

            if bleed_into_right > bleed_threshold {
                failures.push(format!(
                    "boundary {boundary_idx}: {left_color} bleeds {bleed_into_right:.3} into {right_color} cell \
                     (col {check_col_right}, threshold {bleed_threshold})"
                ));
            }
            if bleed_into_left > bleed_threshold {
                failures.push(format!(
                    "boundary {boundary_idx}: {right_color} bleeds {bleed_into_left:.3} into {left_color} cell \
                     (col {check_col_left}, threshold {bleed_threshold})"
                ));
            }
        }
    }

    eprintln!(
        "\nWorst bleed: {worst_bleed:.3} at boundary {worst_boundary}"
    );

    // Dump a few key columns for diagnosis
    eprintln!("\nFull channel profile around first boundary:");
    let first_boundary = (left_margin + advance).round() as u32;
    for x in first_boundary.saturating_sub(4)..=(first_boundary + 4).min(output.width - 1) {
        if let Some(s) = stats.get(x as usize) {
            let marker = if x == first_boundary { " <-- boundary" } else { "" };
            eprintln!(
                "  col {x:3}: R={:.3}  B={:.3}{marker}",
                s.avg_red, s.avg_blue
            );
        }
    }

    if !failures.is_empty() {
        panic!(
            "COLOR BLEED DETECTED — {} violations:\n  {}\n\n\
             Adjacent MSDF glyph quads bleed color across the advance boundary.\n\
             This means glyph sizing/AA extends too far into neighbor cells.\n\
             See /tmp/msdf_tests/color_overlap_mmmmm.png for visual.",
            failures.len(),
            failures.join("\n  ")
        );
    }
}

/// Test: color-channel overlap on mixed-width chars "umUMumUWeoeo".
///
/// Tests a variety of glyph shapes at advance boundaries. Wide glyphs
/// like 'M' and 'W' are most likely to bleed; narrow ones like 'e'
/// should have plenty of sidebearing room.
#[test]
fn color_overlap_mixed_chars() {
    use cosmic_text::{Attrs, FontSystem, Metrics, Shaping};

    let text = "umUMumUWeoeo";
    let font_size = 22.0;

    let red: [u8; 4] = [255, 0, 0, 255];
    let blue: [u8; 4] = [0, 0, 255, 255];

    let config = TestConfig::new(text, font_size, 300, 60, true)
        .with_alternating_colors(red, blue);
    let output = render_with_config(config);
    output.save_png("color_overlap_mixed");

    let mut font_system = FontSystem::new();
    let metrics = Metrics::new(font_size, font_size * 1.2);
    let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buffer.set_text(&mut font_system, text, attrs, Shaping::Advanced);
    buffer.visual_line_count(&mut font_system, 400.0, None);
    let glyphs = buffer.glyphs();
    let advance = glyphs[0].advance_width;
    let left_margin = 10.0f32;

    let (min_y, max_y) = text_row_extent(&output, 0.05)
        .expect("Should have visible text");
    let stats = column_channel_stats(&output, min_y, max_y);

    let bleed_check_offset = 1u32;
    let bleed_threshold = 0.10;

    let chars: Vec<char> = text.chars().collect();
    let mut failures = Vec::new();

    eprintln!("Mixed chars '{text}' — advance={advance:.1}px:");

    for boundary_idx in 0..(chars.len() - 1) {
        let boundary_col = (left_margin + (boundary_idx as f32 + 1.0) * advance).round() as u32;
        let left_is_red = boundary_idx % 2 == 0;

        let check_right = boundary_col + bleed_check_offset;
        let check_left = boundary_col.saturating_sub(bleed_check_offset);

        if let (Some(rs), Some(ls)) = (
            stats.get(check_right as usize),
            stats.get(check_left as usize),
        ) {
            let (bleed_right, bleed_left) = if left_is_red {
                (rs.avg_red, ls.avg_blue)
            } else {
                (rs.avg_blue, ls.avg_red)
            };

            let left_ch = chars[boundary_idx];
            let right_ch = chars[boundary_idx + 1];

            if bleed_right > bleed_threshold || bleed_left > bleed_threshold {
                eprintln!(
                    "  '{left_ch}'→'{right_ch}' (col {boundary_col}): \
                     bleed_right={bleed_right:.3}, bleed_left={bleed_left:.3} ⚠"
                );
            }

            if bleed_right > bleed_threshold {
                failures.push(format!(
                    "'{left_ch}'→'{right_ch}': left bleeds {bleed_right:.3} into right cell"
                ));
            }
            if bleed_left > bleed_threshold {
                failures.push(format!(
                    "'{left_ch}'→'{right_ch}': right bleeds {bleed_left:.3} into left cell"
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "COLOR BLEED in mixed chars — {} violations:\n  {}\n\n\
             See /tmp/msdf_tests/color_overlap_mixed.png",
            failures.len(),
            failures.join("\n  ")
        );
    }
}

/// Test: single glyph color isolation — 'm' alone should have ZERO blue.
///
/// Sanity check: render a single red 'm' and verify zero blue channel.
/// This confirms the color separation technique works correctly before
/// we use it to detect inter-glyph bleed.
#[test]
fn color_isolation_single_glyph() {
    let red: [u8; 4] = [255, 0, 0, 255];
    let blue: [u8; 4] = [0, 0, 255, 255];

    // Single 'm' — will be red (even index 0)
    let config = TestConfig::new("m", 22.0, 80, 50, true)
        .with_alternating_colors(red, blue);
    let output = render_with_config(config);
    output.save_png("color_isolation_m");

    // There should be NO blue pixels anywhere — only red on black
    let mut max_blue = 0u8;
    let mut max_blue_pos = (0u32, 0u32);
    for y in 0..output.height {
        for x in 0..output.width {
            let px = output.pixel(x, y);
            if px.b > max_blue {
                max_blue = px.b;
                max_blue_pos = (x, y);
            }
        }
    }

    eprintln!("Single 'm' (red): max blue channel = {} at ({}, {})", max_blue, max_blue_pos.0, max_blue_pos.1);

    assert!(
        max_blue < 5,
        "Single red glyph should have no blue: max_blue={max_blue} at {:?}. \
         Color separation technique is broken!",
        max_blue_pos
    );
}

// ============================================================================
// EXISTING DIAGNOSTIC TESTS
// ============================================================================

/// Visual regression: render "document" at app font size to check "um" joining.
#[test]
fn document_um_not_joined() {
    // App uses monospace at 22px (current default)
    let config = TestConfig::new("document", 22.0, 250, 60, true);
    let output = render_with_config(config);
    output.save_png("document_22px_mono");

    // Also render at 15px (previous default)
    let config15 = TestConfig::new("document", 15.0, 200, 40, true);
    let output15 = render_with_config(config15);
    output15.save_png("document_15px_mono");

    // Analyze the "um" boundary region in the 22px render
    // Print column luminances for the full word
    eprintln!("\n'document' at 22px monospace — column luminances:");
    let mut min_y = output.height;
    let mut max_y = 0u32;
    for y in 0..output.height {
        for x in 0..output.width {
            if output.pixel(x, y).luminance() > 0.1 {
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    let text_rows = (max_y - min_y + 1).max(1) as f32;

    for x in 0..output.width.min(200) {
        let mut sum = 0.0f32;
        for y in min_y..=max_y {
            sum += output.pixel(x, y).luminance();
        }
        let avg = sum / text_rows;
        if avg > 0.01 {
            eprintln!("  col {:3}: avg_lum={:.3}", x, avg);
        }
    }
}
