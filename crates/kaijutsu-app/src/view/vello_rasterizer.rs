//! Offscreen vello rasterizer owned by kaijutsu.
//!
//! A `vello::Renderer` lives in the render world and rasterizes a built
//! `vello::Scene` to a texture view via `render_to_texture`. This is the one
//! piece of the vello integration we actually need — vello as an offscreen
//! rasterizer presented on an `ImageNode`/mesh. Owning the renderer means that
//! when Bevy bumps the render API we migrate ~50 lines we control. Init pattern:
//! settings in `build()`, renderer in `finish()` once `RenderDevice` exists,
//! with a CPU safe-mode fallback — trimmed to the single `render_to_texture`
//! use case, no UI compositing pass, no canvas.
//!
//! Consumers reach it through `ui_rtt::render_vello_scenes` (block
//! cells, role borders, docks, MSDF text surfaces).

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy::render::{RenderApp, renderer::RenderDevice};
use vello::{AaConfig, AaSupport};

/// The render-world `vello::Renderer`, shared behind a lock.
///
/// `std::sync::Mutex` (not parking_lot) so `lock()` yields a `LockResult` the
/// caller pattern-matches with `let Ok(..) = renderer.lock()`.
#[derive(Resource, Deref, DerefMut)]
pub struct VelloRasterizer(pub Arc<Mutex<vello::Renderer>>);

impl VelloRasterizer {
    pub fn try_new(
        device: &vello::wgpu::Device,
        settings: &VelloRasterizerSettings,
    ) -> Result<Self, vello::Error> {
        vello::Renderer::new(
            device,
            vello::RendererOptions {
                use_cpu: settings.use_cpu,
                // Vello can't add AA support after init, so request all modes
                // up front and pick per-frame via `RenderParams.antialiasing_method`.
                antialiasing_support: AaSupport::all(),
                num_init_threads: None,
                pipeline_cache: None,
            },
        )
        .map(Mutex::new)
        .map(Arc::new)
        .map(VelloRasterizer)
    }
}

impl FromWorld for VelloRasterizer {
    fn from_world(world: &mut World) -> Self {
        let device = world.resource::<RenderDevice>().wgpu_device().clone();
        match VelloRasterizer::try_new(&device, world.resource::<VelloRasterizerSettings>()) {
            Ok(r) => r,
            Err(e) => {
                // Safe-mode fallback: retry on the CPU shaders before giving up.
                error!("vello GPU renderer init failed ({e}); retrying in CPU safe mode");
                {
                    let mut settings = world.resource_mut::<VelloRasterizerSettings>();
                    settings.use_cpu = true;
                    settings.antialiasing = AaConfig::Area;
                }
                match VelloRasterizer::try_new(&device, world.resource::<VelloRasterizerSettings>())
                {
                    Ok(r) => r,
                    // Crashing beats a silent fallback that paints nothing.
                    Err(e) => panic!("failed to start vello rasterizer: {e}"),
                }
            }
        }
    }
}

/// Render settings for the offscreen rasterizer.
#[derive(Resource, Clone)]
pub struct VelloRasterizerSettings {
    /// Run vello's CPU shaders instead of the GPU pipeline.
    pub use_cpu: bool,
    /// Antialiasing strategy passed to each `render_to_texture` call.
    pub antialiasing: AaConfig,
}

impl Default for VelloRasterizerSettings {
    fn default() -> Self {
        Self {
            use_cpu: false,
            antialiasing: AaConfig::Area,
        }
    }
}

/// Installs the rasterizer resources into the render world.
///
/// Settings go in during `build()`; the renderer itself is created in
/// `finish()`, the only point at which `RenderDevice` is present in the
/// render app.
pub struct VelloRasterizerPlugin;

impl Plugin for VelloRasterizerPlugin {
    fn build(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.init_resource::<VelloRasterizerSettings>();
    }

    fn finish(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.init_resource::<VelloRasterizer>();
    }
}
