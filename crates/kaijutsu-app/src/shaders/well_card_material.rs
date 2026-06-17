//! Well card material — the 3D card foundation for the time well.
//!
//! `WellCardMaterial` is a 3D `Material` (unlike `BlockFxMaterial`, which is a 2D
//! `UiMaterial`) for the billboarded cards + focus card in `view/time_well/`. It
//! samples the card's RTT texture (the text/content layer) and is the home for
//! the in-shader SDF glow / rings / status FX (slice 2) — the same "texture =
//! content, shader = FX" split `block_fx` uses, lifted onto a 3D quad. Glow is
//! fragment-shader SDF falloff (no HDR/bloom), matching the rest of the app.
//!
//! Slice 1 (this): parity with the old unlit `StandardMaterial` — sample the
//! texture, Mask alpha. The `params` uniform is wired now so slice 2 can drive
//! FX without a layout change.

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for a single time-well card (rim card or focus card).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WellCardMaterial {
    /// The card's RTT content texture (accent bg + text, rasterized per card).
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    /// Reserved for slice-2 SDF FX: `[selected, in_lineage, status, time]`.
    /// Unused in slice 1 (sampled trivially so the binding isn't stripped).
    #[uniform(2)]
    pub params: Vec4,
}

impl Material for WellCardMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/well_card.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Masked alpha is order-independent (the bg is opaque; only the rounded
        // corners fall below the cutoff), matching the old StandardMaterial.
        AlphaMode::Mask(0.5)
    }
}
