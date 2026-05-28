//! Golden-image regression tests for ABC engraving.
//!
//! Rasterizes the same `vello::Scene` the app builds via a headless
//! `vello::Renderer` and compares against PNG goldens in
//! `src/text/abc_goldens/`. Set `UPDATE_GOLDENS=1` to regenerate.
//!
//! Snippets are pure-staff ABC (no T:/C: text fields) so the test
//! never needs a `VelloFont`. If we later add lyric/title goldens
//! we'll need to plumb a bevy_vello font here.

use bevy_vello::vello;
use bevy_vello::vello::wgpu;
use image::{ImageBuffer, Rgba, RgbaImage};
use kaijutsu_abc::engrave::{layout, EngravingOptions};
use kaijutsu_abc::parse;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use vello::kurbo::Affine;
use vello::peniko::{Brush, Color};

use crate::text::TextMetrics;

/// Render each engraving element at this multiplier of the intrinsic
/// IR coordinates. Larger goldens catch finer geometry bugs at the
/// cost of git size; 4x feels right for ABC at the default 10.0
/// staff_spacing.
const RENDER_SCALE: f64 = 4.0;

/// Maximum allowed RMSE between the rendered image and the golden,
/// expressed as a fraction of 255 (so 0.005 ≈ 0.5%).
const MAX_RMSE: f64 = 0.005;

/// Maximum per-channel delta any single pixel may exceed.
const MAX_CHANNEL_DELTA: u8 = 2;

struct Harness {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Mutex<vello::Renderer>,
}

static HARNESS: OnceLock<Option<Harness>> = OnceLock::new();

fn harness() -> Option<&'static Harness> {
    HARNESS
        .get_or_init(|| {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY),
                flags: wgpu::InstanceFlags::from_build_config().with_env(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                backend_options: wgpu::BackendOptions::from_env_or_default(),
            });
            let adapter_fut =
                wgpu::util::initialize_adapter_from_env_or_default(&instance, None);
            let adapter = pollster::block_on(adapter_fut).ok()?;
            let device_fut = adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("abc-golden"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            });
            let (device, queue) = pollster::block_on(device_fut).ok()?;
            let renderer = vello::Renderer::new(
                &device,
                vello::RendererOptions {
                    use_cpu: false,
                    num_init_threads: NonZeroUsize::new(1),
                    antialiasing_support: vello::AaSupport::area_only(),
                    pipeline_cache: None,
                },
            )
            .ok()?;
            Some(Harness {
                device,
                queue,
                renderer: Mutex::new(renderer),
            })
        })
        .as_ref()
}

fn rasterize(scene: &vello::Scene, width: u32, height: u32) -> Option<RgbaImage> {
    let h = harness()?;
    let texture = h.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("abc-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    h.renderer
        .lock()
        .unwrap()
        .render_to_texture(
            &h.device,
            &h.queue,
            scene,
            &view,
            &vello::RenderParams {
                base_color: vello::peniko::Color::BLACK,
                width,
                height,
                antialiasing_method: vello::AaConfig::Area,
            },
        )
        .expect("vello render_to_texture");

    // Texture → buffer copy. bytes_per_row must be a multiple of 256.
    let bytes_per_pixel = 4u32;
    let unpadded_bpr = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bpr = unpadded_bpr.div_ceil(align) * align;
    let buffer_size = (padded_bpr as u64) * height as u64;
    let buffer = h.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("abc-readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = h
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("abc-copy"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bpr),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    h.queue.submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
    h.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    rx.recv().unwrap().expect("buffer map");

    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded_bpr * height) as usize);
    for row in 0..height {
        let start = (row * padded_bpr) as usize;
        let end = start + unpadded_bpr as usize;
        pixels.extend_from_slice(&mapped[start..end]);
    }
    drop(mapped);
    buffer.unmap();

    ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, pixels)
}

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/text/abc_goldens")
}

fn engrave_abc(source: &str) -> (vello::Scene, u32, u32) {
    let parsed = parse(source);
    assert!(
        !parsed.has_errors(),
        "ABC parse errors:\n{:#?}",
        parsed.errors().collect::<Vec<_>>()
    );
    let tune = parsed
        .value
        .first()
        .expect("ABC source produced no tunes");

    let opts = EngravingOptions::default();
    let elements = layout::engrave(tune, &opts);

    let brush = Brush::Solid(Color::WHITE);
    let metrics = TextMetrics::default();
    let (inner_scene, w, h) = super::render_engraving_to_scene(
        &elements,
        opts.margin,
        &brush,
        None,
        &metrics,
    );

    // Scale up into the golden canvas so subpixel bugs are visible.
    let mut scaled = vello::Scene::new();
    scaled.append(&inner_scene, Some(Affine::scale(RENDER_SCALE)));
    let canvas_w = (w * RENDER_SCALE).ceil() as u32;
    let canvas_h = (h * RENDER_SCALE).ceil() as u32;
    (scaled, canvas_w, canvas_h)
}

/// Compare `actual` against the golden at `goldens_dir()/<name>.png`.
///
/// Behaviour:
/// - `UPDATE_GOLDENS=1` set → overwrite the golden, succeed.
/// - golden missing → write it, fail with a clear message (so CI can't
///   silently accept a new golden).
/// - dimensions differ → fail.
/// - RMSE > MAX_RMSE OR any channel delta > MAX_CHANNEL_DELTA → fail
///   and dump an `<name>.actual.png` next to the golden for inspection.
fn assert_matches_golden(name: &str, actual: &RgbaImage) {
    let path = goldens_dir().join(format!("{name}.png"));
    let update = std::env::var_os("UPDATE_GOLDENS").is_some();

    if update {
        actual.save(&path).expect("write updated golden");
        eprintln!("UPDATE_GOLDENS: wrote {}", path.display());
        return;
    }

    if !path.exists() {
        actual.save(&path).expect("write initial golden");
        panic!(
            "golden missing — wrote initial {}. Inspect it, then re-run.",
            path.display()
        );
    }

    let golden = image::open(&path)
        .unwrap_or_else(|e| panic!("load golden {}: {e}", path.display()))
        .to_rgba8();

    if golden.dimensions() != actual.dimensions() {
        let actual_path = goldens_dir().join(format!("{name}.actual.png"));
        actual.save(&actual_path).ok();
        panic!(
            "dimension mismatch for {name}: golden {:?}, actual {:?} (saved {})",
            golden.dimensions(),
            actual.dimensions(),
            actual_path.display(),
        );
    }

    let mut sq_sum = 0.0f64;
    let mut worst_delta = 0u8;
    let mut worst_pixel = (0u32, 0u32);
    for (x, y, p) in actual.enumerate_pixels() {
        let g = golden.get_pixel(x, y);
        for c in 0..4 {
            let d = (p.0[c] as i32 - g.0[c] as i32).unsigned_abs() as u8;
            if d > worst_delta {
                worst_delta = d;
                worst_pixel = (x, y);
            }
            sq_sum += (d as f64) * (d as f64);
        }
    }
    let n = (actual.width() as f64) * (actual.height() as f64) * 4.0;
    let rmse = (sq_sum / n).sqrt() / 255.0;

    let fail = rmse > MAX_RMSE || worst_delta > MAX_CHANNEL_DELTA;
    if fail {
        let actual_path = goldens_dir().join(format!("{name}.actual.png"));
        actual.save(&actual_path).ok();
        panic!(
            "golden mismatch for {name}: rmse={:.4} (max {:.4}), worst channel delta={} at {:?} (max {}) — saved {}",
            rmse,
            MAX_RMSE,
            worst_delta,
            worst_pixel,
            MAX_CHANNEL_DELTA,
            actual_path.display(),
        );
    }
}

fn run_case(name: &str) {
    let source = std::fs::read_to_string(goldens_dir().join(format!("{name}.abc")))
        .unwrap_or_else(|e| panic!("read {name}.abc: {e}"));
    let (scene, w, h) = engrave_abc(&source);
    let Some(img) = rasterize(&scene, w, h) else {
        eprintln!("SKIP {name}: no wgpu adapter (headless GPU unavailable)");
        return;
    };
    assert_matches_golden(name, &img);
}

#[test]
fn single_bar_c_major_quarter_notes() {
    run_case("single_bar");
}

#[test]
fn chord_with_accidentals() {
    run_case("chord_accidentals");
}

#[test]
fn beamed_eighths_sixteenths() {
    run_case("beamed_eighths");
}

/// Pure pitch→Y regression: chromatic quarter notes from C4 up to C6 and
/// back (sharps ascending, flats descending), single treble staff, no
/// beams or slurs. Spans one ledger below the staff to two above, so a
/// notehead height or ledger-count shift — the octave bug that put every
/// note an octave too low — shows immediately. Beaming/slurs are covered
/// by other goldens; this one isolates the mapping.
#[test]
fn chromatic_run_octave_regression() {
    run_case("chromatic_run");
}
