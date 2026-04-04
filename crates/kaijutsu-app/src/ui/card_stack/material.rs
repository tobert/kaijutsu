//! StackCardMaterial — 3D material for conversation stack cards.
//!
//! Samples the block's existing RTT texture and applies:
//! - Opacity control (fade distant cards)
//! - Edge glow (holographic shimmer, role-colored)
//! - LOD degradation (blur → abstract → outline as distance increases)

use bevy::prelude::*;
use bevy::pbr::{MaterialPipeline, MaterialPipelineKey};
use bevy_mesh::MeshVertexBufferLayoutRef;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderRef;

/// Packed uniform data for the card shader.
/// All parameters in a single struct at binding 2.
#[derive(Clone, Copy, Debug, Default, bevy::render::render_resource::ShaderType, Reflect)]
pub struct StackCardUniforms {
    pub card_params: Vec4,
    pub glow_color: Vec4,
    pub glow_params: Vec4,
}

/// 3D material for stack card quads.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct StackCardMaterial {
    /// The block's RTT texture.
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    /// Packed uniforms: card_params + glow_color + glow_params.
    #[uniform(2)]
    pub uniforms: StackCardUniforms,
}

impl Default for StackCardMaterial {
    fn default() -> Self {
        Self {
            texture: Handle::default(),
            uniforms: StackCardUniforms {
                card_params: Vec4::new(1.0, 0.0, 0.0, 0.0),
                glow_color: Vec4::ZERO,
                glow_params: Vec4::ZERO,
            },
        }
    }
}

impl Material for StackCardMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/stack_card.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // Disable face culling so back faces render with their own pattern.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}
