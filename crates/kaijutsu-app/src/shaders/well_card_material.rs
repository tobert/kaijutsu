//! Well card material — the 3D card for the time well (rim cards + focus card).
//!
//! `WellCardMaterial` is a 3D `Material` that draws the *entire* card on the GPU:
//! the accent rounded-rect background, the selection/lineage rings (SDF, from
//! `params`), and the MSDF text composited on top (the `texture`, which the MSDF
//! pass renders text-on-transparent into). This is the vello-free well — vello no
//! longer touches card textures (it stays for SVG/ABC elsewhere). The bling (rings
//! + status pulse) is SDF in the fragment shader emitting **HDR** (>1.0) color, so
//! the app's single HDR `Camera3d` blooms it into a glow halo (see
//! `main::setup_camera`); animation reads `globals.time` directly.

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for a single time-well card (rim card or focus card).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WellCardMaterial {
    /// MSDF text texture (text on transparent — the MSDF pass clears + renders it).
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    /// Accent background color (linear rgba). Fills the rounded-rect body.
    #[uniform(2)]
    pub accent: Vec4,

    /// `[selected, in_lineage, status, drifting]` — drives the rings + status
    /// pulse + drift sheen. `status`: pending/none 0, running 1, done 2,
    /// error 3; `drifting` (0/1) gates the LDR sheen sweep. Animation reads
    /// `globals.time` in the shader.
    #[uniform(3)]
    pub params: Vec4,

    /// `[aspect (w/h), corner_radius, ring_width, inset]` in the shader's
    /// aspect-corrected UV space.
    #[uniform(4)]
    pub shape: Vec4,

    /// `[r, g, b, strength]` — a steady outline drawn from the SDF ring band,
    /// independent of the `params` selection/lineage/status rings. A
    /// deliberate **union of two purposes** on distinct material instances:
    /// HUD panels carry the selection accent here (HDR gain, strength 1.0 — a
    /// blooming frame around an empty interior; `hud::update_well_hud`),
    /// while rim cards carry their **track hue** (LDR, modest strength —
    /// passive lane identity; `live::sync_card_live_uniforms`). Unattached
    /// cards leave it `Vec4::ZERO` (strength 0 → no border). Split into two
    /// fields if the two ever need to animate differently.
    #[uniform(5)]
    pub border: Vec4,

    /// `.x` = focus dimming (1.0 = full/focused, `<1.0` recedes the card so the
    /// *focused* ring's cards pop). Applied as a **color multiply** in
    /// `well_card.wgsl` — this material is `AlphaMode::Mask(0.5)`, so dimming
    /// the alpha instead would push the body under the cutoff and clip the whole
    /// card rather than fade it. Written per-frame by `scene::dim_nonfocused_rings`
    /// (rim cards only; the focus card + HUD panels stay 1.0).
    ///
    /// `.y` = live **chatter** (0..1) and `.z` = live **beat** envelope (0..1)
    /// — the event-driven card lanes written by
    /// `view::time_well::live::sync_card_live_uniforms` (cyan rim lift / gold
    /// beat thump in the shader, no texture rebuild). `.w` unused.
    #[uniform(6)]
    pub dim: Vec4,
}

impl Material for WellCardMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/well_card.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Masked alpha is order-independent (the rounded-rect body is opaque; only
        // outside the corners falls below the cutoff and is discarded).
        AlphaMode::Mask(0.5)
    }
}
