//! The surface's render target and how it reaches the screen.
//!
//! Each [`ConversationSurface`] draws into **one viewport-sized** RTT — pane
//! size, never document size — which a plain `ImageNode` composites back into
//! the pane. That is the editor/diff-surface precedent
//! (`view/editor/render.rs`'s `msdf_surface_bundle`) minus the
//! `BlockFxMaterial`: slice 1 has no borders, glow or label decoration to
//! post-process, so the extra material would be a second full-screen draw of
//! the same pixels for nothing. Slice 2 adds chrome as instanced SDF quads
//! *inside* this texture, not as a material on top of it.
//!
//! # Why the texture is opaque
//!
//! `msdf_surface_bundle` sets its `ImageNode` to `Color::NONE` because a
//! material — not Bevy's UI image pipeline — draws the block textures, and
//! letting UI draw them too double-composites premultiplied content through a
//! straight-alpha path (dark halos around every glyph). Here Bevy's UI image
//! pipeline *is* the compositor, so that hazard is real and has to be closed
//! from the other end: the surface pass **clears to the theme background**
//! (`Theme::bg`, the same color the camera clears to, which is what shows
//! behind conversation panes today — the `ConversationContainer` node has no
//! background of its own). Every pixel of the finished texture therefore has
//! alpha 1, where premultiplied and straight encodings are identical, and the
//! composite cannot halo no matter which the UI pipeline assumes.
//!
//! The clear value is the color's **linear** components. A clear on a
//! non-sRGB `Rgba8Unorm` target is stored verbatim, and the UI pipeline
//! samples it and passes it through as a linear value — so `to_srgba()` here
//! would paint each pane a lighter shade than the camera clear behind it and
//! outline every pane in its own background. Glyph colors keep the encoding
//! `color_to_rgba8` has always given them (`to_srgba`), which is what every
//! other MSDF surface in the app already renders, so text on the surface path
//! matches text on the legacy path exactly.
//!
//! # Sizing
//!
//! The composite node is absolutely positioned with all four insets at zero,
//! so it fills the pane's containing block, and the RTT is sized from **its
//! own** `ComputedNode` (the dock pattern, `ui::dock::resize_dock_textures`)
//! rather than the pane's. Sizing from the node that actually gets drawn is
//! what keeps the doc→NDC mapping exact; taking the pane's content box
//! instead would disagree by the pane's padding and shear the text
//! sub-pixel. The consequence is that the surface's drawing origin is the
//! pane's *padding* box, so document x=0 lands ~4px left of where the legacy
//! per-block cells start (they are content-box children). Cosmetic, flagged,
//! and cheapest to fix later by zeroing the pane padding on the surface path
//! — not by re-deriving a second size here.

use bevy::prelude::*;

use crate::cell::ConversationContainer;
use crate::text::TextMetrics;
use crate::view::block_render::{GpuTextureLimits, round_to_physical_px};
use crate::view::ui_rtt::{UiRttTexture, logical_size, resize_rtt_texture};

use super::ConversationSurface;

/// Marker for the UI node that composites a surface's finished texture.
///
/// Deliberately **not** `crate::view::block_render::MsdfSurface`, and the
/// entity deliberately carries no `MsdfBlockGlyphs`: those two are what the
/// legacy MSDF pass matches on (`resize_block_textures` filters
/// `With<MsdfSurface>`, `extract_msdf_blocks` requires `&MsdfBlockGlyphs`),
/// and a surface that answered either query would be resized against a
/// `built_width` it doesn't own or extracted a second time into the per-block
/// pass. The surface path owns its own resize ([`resize_surface_targets`])
/// and its own extraction (`super::extract`), so it must stay invisible to
/// both — see this module's tests.
#[derive(Component, Debug, Default)]
pub struct ConversationSurfaceTarget;

/// Give every [`ConversationSurface`] its RTT + composite node, parented to
/// the pane it draws for.
///
/// Runs after `window::ensure_conversation_surfaces` (which spawns the bare
/// surface entities); a surface whose pane vanished is left alone here and
/// reaped there.
pub fn attach_surface_targets(
    mut commands: Commands,
    surfaces: Query<(Entity, &ConversationSurface), Without<ConversationSurfaceTarget>>,
    panes: Query<Entity, With<ConversationContainer>>,
) {
    for (entity, surface) in &surfaces {
        if panes.get(surface.pane).is_err() {
            continue;
        }
        commands.entity(entity).insert((
            ConversationSurfaceTarget,
            UiRttTexture::default(),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                top: Val::Px(0.0),
                bottom: Val::Px(0.0),
                ..default()
            },
            // Transparent until the first resize allocates a real texture:
            // the default handle points at nothing, and drawing it would
            // flash a white rect for a frame. `resize_surface_targets` turns
            // the tint on at the same moment it hands over a real image.
            ImageNode::default().with_color(Color::NONE),
            Name::new("ConversationSurfaceTarget"),
        ));
        commands.entity(surface.pane).add_child(entity);
    }
}

/// Keep each surface texture matched to its composite node's physical size.
///
/// Every frame, from live layout — the dock's approach, and for the same
/// reason: there is no separate "build" pass whose cached size could be
/// trusted instead. `round_to_physical_px` keeps `ceil()` inside
/// `ui_rtt_texture_dims` a no-op, so the physical texture is exactly
/// `logical * scale` and the extractor's `width / built_width` recovers the
/// scale factor exactly.
pub fn resize_surface_targets(
    mut targets: Query<
        (&ComputedNode, &mut UiRttTexture, &mut ImageNode),
        With<ConversationSurfaceTarget>,
    >,
    text_metrics: Res<TextMetrics>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
) {
    let scale = text_metrics.scale_factor;
    let max_dim = gpu_limits.max_texture_dim;

    for (computed, mut texture, mut image_node) in targets.iter_mut() {
        // ComputedNode is physical px; resize_rtt_texture takes logical.
        let size = logical_size(computed);
        let logical_w = round_to_physical_px(size.x, scale);
        let logical_h = round_to_physical_px(size.y, scale);
        if logical_w <= 0.0 || logical_h <= 0.0 {
            continue;
        }
        texture.built_width = logical_w;
        texture.built_height = logical_h;
        if resize_rtt_texture(
            &mut texture,
            &mut image_node,
            logical_w,
            logical_h,
            scale,
            max_dim,
            &mut images,
        ) {
            // A real texture exists now — let the UI actually draw it.
            image_node.color = Color::WHITE;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::msdf::MsdfBlockGlyphs;
    use crate::view::block_render::MsdfSurface;

    fn app() -> App {
        let mut app = App::new();
        app.add_systems(Update, attach_surface_targets);
        app
    }

    #[test]
    fn a_surface_gets_a_target_parented_to_its_pane() {
        let mut app = app();
        let pane = app.world_mut().spawn(ConversationContainer).id();
        let surface = app.world_mut().spawn(ConversationSurface::new(pane)).id();
        app.update();

        assert!(app.world().get::<ConversationSurfaceTarget>(surface).is_some());
        assert!(app.world().get::<UiRttTexture>(surface).is_some());
        assert!(app.world().get::<ImageNode>(surface).is_some());
        assert_eq!(
            app.world().get::<ChildOf>(surface).map(|c| c.parent()),
            Some(pane),
        );
    }

    #[test]
    fn attaching_is_idempotent() {
        let mut app = app();
        let pane = app.world_mut().spawn(ConversationContainer).id();
        let surface = app.world_mut().spawn(ConversationSurface::new(pane)).id();
        app.update();
        let handle = app.world().get::<UiRttTexture>(surface).unwrap().image.clone();
        app.update();
        // Re-inserting the bundle would reset the texture handle the resize
        // system just allocated — and re-parent every frame.
        assert_eq!(app.world().get::<UiRttTexture>(surface).unwrap().image, handle);
        assert_eq!(app.world().entity(pane).get::<Children>().map(|c| c.len()), Some(1));
    }

    /// The double-extraction guard, asserted against the *actual* filters the
    /// legacy pass uses rather than against a comment: a surface target must
    /// match neither `extract_msdf_blocks`'s query nor
    /// `resize_block_textures`'s marker.
    #[test]
    fn a_surface_target_is_invisible_to_the_per_block_msdf_pass() {
        let mut app = app();
        let pane = app.world_mut().spawn(ConversationContainer).id();
        app.world_mut().spawn(ConversationSurface::new(pane));
        app.update();

        let world = app.world_mut();
        let mut extract_query = world.query::<(&MsdfBlockGlyphs, &UiRttTexture)>();
        assert_eq!(
            extract_query.iter(world).count(),
            0,
            "the surface target would be extracted a second time by extract_msdf_blocks",
        );
        let mut resize_query = world.query_filtered::<&UiRttTexture, With<MsdfSurface>>();
        assert_eq!(
            resize_query.iter(world).count(),
            0,
            "the surface target would be resized by resize_block_textures",
        );
    }

    #[test]
    fn a_surface_whose_pane_is_not_a_container_gets_no_target() {
        let mut app = app();
        let stray = app.world_mut().spawn_empty().id();
        let surface = app.world_mut().spawn(ConversationSurface::new(stray)).id();
        app.update();
        assert!(app.world().get::<ConversationSurfaceTarget>(surface).is_none());
    }
}
