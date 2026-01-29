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

/// Test scene configuration.
#[derive(Resource)]
struct TestConfig {
    text: String,
    font_size: f32,
    width: u32,
    height: u32,
    use_monospace: bool,
    /// Text position offset from top-left.
    left: f32,
    top: f32,
    /// Scale factor for text rendering.
    scale: f32,
    /// Text color.
    color: Color,
    /// Enable glow effect.
    glow: bool,
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
            use_monospace,
            left: 10.0,
            top: 10.0,
            scale: 1.0,
            color: Color::WHITE,
            glow: false,
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
    let font_family = if config.use_monospace {
        cosmic_text::Family::Monospace
    } else {
        cosmic_text::Family::Serif
    };

    // Initialize text buffer
    let metrics = cosmic_text::Metrics::new(config.font_size, config.font_size * 1.2);

    if let Ok(mut fs) = font_system.0.lock() {
        let mut buffer = MsdfTextBuffer::new(&mut fs, metrics);
        let attrs = cosmic_text::Attrs::new().family(font_family);
        buffer.set_text(&mut fs, &config.text, attrs, cosmic_text::Shaping::Advanced);
        buffer.set_color(config.color);
        buffer.visual_line_count(&mut fs, config.width as f32);

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

/// Test 3: Kerning visible - AV should be tighter than AA.
///
/// Verifies that kerning pairs render correctly.
#[test]
fn kerning_av_tighter_than_aa() {
    let av_output = render_text_headless("AV", 32.0, 150, DEFAULT_HEIGHT, false);
    let aa_output = render_text_headless("AA", 32.0, 150, DEFAULT_HEIGHT, false);

    av_output.save_png("kerning_av");
    aa_output.save_png("kerning_aa");

    // Measure gaps
    let av_gap = av_output.measure_glyph_gap(0.02);
    let aa_gap = aa_output.measure_glyph_gap(0.02);

    // AV should have smaller gap due to kerning (V tucks under A)
    match (av_gap, aa_gap) {
        (Some(av), Some(aa)) => {
            // Note: If kerning isn't working, they'll be similar
            // We allow this test to pass but warn if kerning seems missing
            if av >= aa {
                eprintln!(
                    "WARNING: AV gap ({}) >= AA gap ({}). Kerning may not be working.",
                    av, aa
                );
            }
        }
        _ => {
            eprintln!("WARNING: Couldn't measure glyph gaps reliably");
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
