//! Custom materials for the heroic view (`docs/newui.md`, "Shaders").
//!
//! - [`HeroicSunMaterial`]: striped synthwave sun backdrop (`assets/shaders/heroic/sun.wesl`)
//! - [`HeroicFireMaterial`]: additive beat-locked edge fire (`assets/shaders/heroic/fire.wesl`)
//! - [`HeroicHillsMaterial`]: spectrogram flank hills with vertex displacement (`assets/shaders/heroic/hills.wesl`)

use bevy::material::AlphaMode;
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Striped sun disc material (`assets/shaders/heroic/sun.wesl`).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct HeroicSunMaterial {
    #[uniform(0)]
    pub sun_top: Vec4,
    #[uniform(1)]
    pub sun_mid: Vec4,
    #[uniform(2)]
    pub sun_bot: Vec4,
    /// `[intensity, beat_pulse, reduced_motion, _]`
    #[uniform(3)]
    pub params: Vec4,
}

impl Material for HeroicSunMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/heroic/sun.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
}

/// Additive beat-locked roadside fire material (`assets/shaders/heroic/fire.wesl`).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct HeroicFireMaterial {
    /// `[beat_len, beat_scroll_z, intensity, reduced_motion]`
    #[uniform(0)]
    pub params: Vec4,
    /// `[rainbow_lightness, rainbow_chroma, road_length, _]`
    #[uniform(1)]
    pub ramp: Vec4,
    /// `[fog_color.rgb, fog_start]`
    #[uniform(2)]
    pub fog_color_start: Vec4,
    /// `[fog_end, _, _, _]`
    #[uniform(3)]
    pub fog_end_pad: Vec4,
    /// 16-band energy packed into 4 vec4s.
    #[uniform(4)]
    pub bands: [Vec4; 4],
}

impl Material for HeroicFireMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/heroic/fire.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Add
    }

    fn specialize(
        _pipeline: &bevy::pbr::MaterialPipeline,
        descriptor: &mut bevy::render::render_resource::RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        _key: bevy::pbr::MaterialPipelineKey<Self>,
    ) -> Result<(), bevy::render::render_resource::SpecializedMeshPipelineError> {
        // Double-sided and no depth write (`docs/newui.md`, "Edge fire").
        descriptor.primitive.cull_mode = None;
        if let Some(depth) = &mut descriptor.depth_stencil {
            depth.depth_write_enabled = Some(false);
        }
        Ok(())
    }
}

/// Spectrogram hills material (`assets/shaders/heroic/hills.wesl`).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct HeroicHillsMaterial {
    /// `[gain, phase, intensity, reduced_motion]`
    #[uniform(0)]
    pub params: Vec4,
    /// `[rainbow_lightness, rainbow_chroma, grid_cols, grid_rows]`
    #[uniform(1)]
    pub grid: Vec4,
    /// `[hill_fill.rgb, fog_start]`
    #[uniform(2)]
    pub fill_fog_start: Vec4,
    /// `[fog_color.rgb, fog_end]`
    #[uniform(3)]
    pub fog_color_end: Vec4,
    /// 32 history rows x 16 bands = 128 vec4s (4 vec4s per row).
    #[uniform(4)]
    pub history: [Vec4; 128],
}

impl Default for HeroicHillsMaterial {
    fn default() -> Self {
        Self {
            params: Vec4::new(0.6, 0.0, 0.6, 0.0),
            grid: Vec4::new(0.78, 0.14, 12.0, 32.0),
            fill_fog_start: Vec4::new(0.01, 0.002, 0.02, 18.0),
            fog_color_end: Vec4::new(0.03, 0.005, 0.07, 55.0),
            history: [Vec4::ZERO; 128],
        }
    }
}

impl Material for HeroicHillsMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/heroic/hills.wesl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/heroic/hills.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Opaque
    }
}
