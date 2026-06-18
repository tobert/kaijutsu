//! The well's edge HUD: the selected card's data fanned out to the four edges
//! (N/S/E/W) instead of a panel pulled into the center, so the glowing core +
//! rings stay the open browser space. As you browse (arrow/Tab/band-hop) the HUD
//! tracks the selection:
//!
//! - **N** (top): identity — title · context-type · live status.
//! - **E** (right): specs — model, fork kind, band, keywords, cluster.
//! - **W** (left): lineage — the fork-ancestry chain (this ◂ parent ◂ …).
//! - **S** (bottom): preview — a snippet of the most representative block.
//!
//! Each readout is an **in-scene MSDF panel** (the shared [`super::panel`]
//! primitive): a 3D quad **parented to the well camera**, so it rides the frame
//! (screen-stable) yet lives in the scene — HDR/bloom, depth, and the
//! `WellCardMaterial` accent plate, same visual language as the cards. Being a
//! camera child it always faces the camera (no billboard system). Panels are
//! positioned at the camera's frustum edges via [`hud_slot_offset`], derived
//! from the live `Projection` so they adapt to window aspect.
//!
//! Spawned on enter, despawned on exit, repositioned each frame
//! ([`position_well_hud`]), and re-laid-out only when the formatted text actually
//! changes ([`update_well_hud`] — no per-frame relayout).

use std::f32::consts::FRAC_PI_4;

use bevy::prelude::*;
use kaijutsu_types::{ContextId, Status};
use kaijutsu_viz::layout::Band;

use super::panel::{commit_panel_glyphs, create_msdf_panel};
use super::scene::{Card, TimeWellState, accent_color};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

/// Which edge a HUD panel lives on. Shared despawn happens via [`WellHud`].
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudSlot {
    North,
    East,
    West,
    /// Bottom preview — formatter + tests kept, but currently **not spawned**
    /// (hidden); add back to [`HudSlot::ALL`] to show it again.
    #[allow(dead_code)]
    South,
}

impl HudSlot {
    /// The spawned slots. South is intentionally omitted (hidden) — the open
    /// bottom keeps the vortex throat clear; re-add it here to bring it back.
    pub const ALL: [HudSlot; 3] = [HudSlot::North, HudSlot::East, HudSlot::West];
}

/// Container marker for the whole HUD (despawned together on exit).
#[derive(Component)]
pub struct WellHud;

/// The last formatted string a panel rendered — change-guards the MSDF relayout
/// so an unchanged edge never rebuilds its glyphs.
#[derive(Component, Default)]
pub struct HudText(String);

// ── Layout tuning (all derived from the frustum; px values are texture-space). ──

/// Local distance in front of the camera the HUD plane sits at. The panels are
/// camera children, so this is constant in screen space and always renders in
/// front of every card (which live hundreds of units further down the funnel).
const HUD_DEPTH: f32 = 100.0;
/// Gap from the frustum edge, as a fraction of the half-extent — the size-aware
/// fit in `hud_transform` keeps each panel's outer edge exactly this far in, so
/// a small value hugs the screen edge.
const HUD_MARGIN: f32 = 0.02;
/// Extra downward drop (fraction of half-height) for the E/W corner panels so
/// their top edge clears the app's persistent top status bar. N is center-top
/// and clears the bar's left/right labels on its own.
const HUD_EW_TOP_DROP: f32 = 0.15;
/// Texture-space font size + inner padding for the readout text.
const HUD_FONT_SIZE: f32 = 27.0;
/// North's context-name (first line) is rendered this much larger than the body.
const HUD_TITLE_SCALE: f32 = 1.667;
/// Inner padding (texture px) — keeps the text inset from the frame so the panel
/// has breathing room inside its border.
const HUD_PAD: f32 = 30.0;
/// Border strength (the `WellCardMaterial.border` alpha) and an HDR gain so the
/// outline blooms gently. No body fill — the panel is a glowing frame tinted by
/// the selection's accent, so unequal panel sizes still read as a set.
const HUD_BORDER_STRENGTH: f32 = 1.0;
const HUD_BORDER_GAIN: f32 = 1.8;

/// Per-slot texture (logical authoring) size. N/S are wide-short, E/W tall-narrow;
/// the quad aspect tracks this so text never distorts.
fn hud_tex_dims(slot: HudSlot) -> (u32, u32) {
    match slot {
        HudSlot::North => (660, 220),
        HudSlot::South => (560, 150),
        HudSlot::East | HudSlot::West => (440, 340),
    }
}

/// Text alignment inside a panel.
fn hud_align(slot: HudSlot) -> VelloTextAlign {
    match slot {
        HudSlot::North | HudSlot::South => VelloTextAlign::Middle,
        HudSlot::East | HudSlot::West => VelloTextAlign::Left,
    }
}

/// World quad size (well units) for a slot, derived from the frustum half-extents
/// so panels stay screen-proportional across aspect/FOV. Width/height keep the
/// slot's texture aspect (no text distortion).
fn hud_quad_size(slot: HudSlot, half_w: f32, half_h: f32) -> Vec2 {
    let (tw, th) = hud_tex_dims(slot);
    let aspect = tw as f32 / th as f32;
    match slot {
        HudSlot::North | HudSlot::South => {
            let w = half_w * 0.60;
            Vec2::new(w, w / aspect)
        }
        HudSlot::East | HudSlot::West => {
            let h = half_h * 0.46;
            Vec2::new(h * aspect, h)
        }
    }
}

/// `WellCardMaterial.shape` for a HUD panel: `[aspect, corner_radius, ring_width,
/// inset]` — same rounded-rect knobs as a card but the slot's own aspect.
fn hud_shape(slot: HudSlot) -> Vec4 {
    let (tw, th) = hud_tex_dims(slot);
    // [aspect, corner_radius, ring_width (thin frame), inset].
    Vec4::new(tw as f32 / th as f32, 0.05, 0.018, 0.012)
}

/// The frustum **edge/corner anchor** for a slot, in camera-local space. At local
/// depth `depth` the frustum half-extents are `half_h = depth·tan(fov_y/2)` and
/// `half_w = half_h·aspect`; the anchor sits `margin` (fraction of the half-extent)
/// inboard of the edge, on the plane `z = -depth` (camera looks down local -Z).
/// N anchors top-center; **E/W anchor to the top corners** (they share N's top
/// row and grow downward — `hud_transform` fits the panel in from the anchor).
pub fn hud_slot_offset(slot: HudSlot, fov_y: f32, aspect: f32, depth: f32, margin: f32) -> Vec3 {
    let half_h = depth * (fov_y * 0.5).tan();
    let half_w = half_h * aspect;
    let inset = 1.0 - margin;
    match slot {
        HudSlot::North => Vec3::new(0.0, half_h * inset, -depth),
        HudSlot::South => Vec3::new(0.0, -half_h * inset, -depth),
        HudSlot::East => Vec3::new(half_w * inset, half_h * (inset - HUD_EW_TOP_DROP), -depth),
        HudSlot::West => Vec3::new(-half_w * inset, half_h * (inset - HUD_EW_TOP_DROP), -depth),
    }
}

/// Full child-local transform for a slot: edge offset + a scale that sizes the
/// shared unit quad to [`hud_quad_size`]. Recomputed on spawn and every frame
/// ([`position_well_hud`]) so the HUD stays edge-locked across window resizes.
fn hud_transform(slot: HudSlot, fov_y: f32, aspect: f32) -> Transform {
    let half_h = HUD_DEPTH * (fov_y * 0.5).tan();
    let half_w = half_h * aspect;
    let size = hud_quad_size(slot, half_w, half_h);
    let anchor = hud_slot_offset(slot, fov_y, aspect, HUD_DEPTH, HUD_MARGIN);
    // Fit the panel fully inside from its edge/corner anchor: pull horizontally
    // toward center by half-width (a center-anchored panel like N stays at x=0 —
    // note `f32::signum(0.0)` is 1.0, so guard the zero case) and always down
    // from the top anchor by half-height.
    let cx = if anchor.x == 0.0 {
        0.0
    } else {
        anchor.x - anchor.x.signum() * size.x * 0.5
    };
    let cy = anchor.y - size.y * 0.5;
    Transform::from_translation(Vec3::new(cx, cy, anchor.z)).with_scale(Vec3::new(size.x, size.y, 1.0))
}

/// Read `(fov_y, aspect)` off a camera projection, falling back to sane defaults
/// for a non-perspective projection (the well always uses perspective).
fn read_perspective(projection: &Projection) -> (f32, f32) {
    match projection {
        Projection::Perspective(p) => (p.fov, p.aspect_ratio),
        _ => (FRAC_PI_4, 16.0 / 9.0),
    }
}

/// Spawn the four edge HUD panels as children of the well camera (empty until a
/// selection drives them). The app camera always exists, so this queries
/// `With<Camera3d>` directly — no ordering dependency on `enter_time_well`.
pub fn spawn_well_hud(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    camera: Query<(Entity, &Projection), With<Camera3d>>,
) {
    let Ok((cam_entity, projection)) = camera.single() else {
        warn!("well HUD: no Camera3d to parent panels to");
        return;
    };
    let (fov_y, aspect) = read_perspective(projection);

    // Shared unit quad; per-panel size rides on Transform.scale.
    let quad = meshes.add(Rectangle::new(1.0, 1.0));

    for slot in HudSlot::ALL {
        let (tw, th) = hud_tex_dims(slot);
        let (image, panel) = create_msdf_panel(&mut images, tw, th);
        let material = materials.add(WellCardMaterial {
            texture: image,
            accent: Vec4::ZERO, // no body fill — the panel is a glowing frame
            params: Vec4::ZERO, // plain panel: no selection/lineage/status rings
            shape: hud_shape(slot),
            border: Vec4::ZERO, // outline driven by the selection in update_well_hud
        });
        commands
            .spawn((
                WellHud,
                slot,
                HudText::default(),
                Mesh3d(quad.clone()),
                MeshMaterial3d(material),
                hud_transform(slot, fov_y, aspect),
                Visibility::Inherited,
                panel,
                Name::new("WellHudPanel"),
            ))
            .insert(ChildOf(cam_entity));
    }
}

/// Despawn the whole HUD on exit (the camera, their parent, survives).
pub fn despawn_well_hud(mut commands: Commands, hud: Query<Entity, With<WellHud>>) {
    for e in hud.iter() {
        commands.entity(e).despawn();
    }
}

/// Keep each panel edge-locked: re-derive its transform from the live projection
/// (cheap — four `Vec3`s) so the HUD tracks window-aspect/FOV changes.
pub fn position_well_hud(
    camera: Query<&Projection, With<Camera3d>>,
    mut panels: Query<(&HudSlot, &mut Transform), With<WellHud>>,
) {
    let Ok(projection) = camera.single() else {
        return;
    };
    let (fov_y, aspect) = read_perspective(projection);
    for (slot, mut tf) in panels.iter_mut() {
        *tf = hud_transform(*slot, fov_y, aspect);
    }
}

/// Refresh the four readouts from the current selection. Recomputes the formatted
/// string every frame (cheap) but only re-lays-out MSDF glyphs — and re-tints the
/// plate — when a panel's string actually changes. Nothing selected → blank.
pub fn update_well_hud(
    state: Res<TimeWellState>,
    cards: Query<&Card>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut panels: Query<(&HudSlot, &mut HudText, &mut MsdfBlockGlyphs, &MeshMaterial3d<WellCardMaterial>)>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading
    };
    let selected = state.selected.and_then(|sel| cards.iter().find(|c| c.context_id == sel));

    // Border outline echoes the selection's accent (HDR so it blooms); none → off.
    let border = match selected {
        Some(card) => {
            let c = accent_color(&card.data.accent).to_linear();
            Vec4::new(
                c.red * HUD_BORDER_GAIN,
                c.green * HUD_BORDER_GAIN,
                c.blue * HUD_BORDER_GAIN,
                HUD_BORDER_STRENGTH,
            )
        }
        None => Vec4::ZERO,
    };

    for (slot, mut last, mut msdf, mat_node) in panels.iter_mut() {
        let next = match (selected, slot) {
            (Some(card), HudSlot::North) => hud_north(&card.data, card.status),
            (Some(card), HudSlot::East) => hud_east(&card.data),
            (Some(card), HudSlot::West) => hud_west(card.context_id, &state),
            (Some(card), HudSlot::South) => hud_south(&card.data),
            (None, _) => String::new(),
        };
        // Skip unchanged text — but always do the first build (version 0) even
        // for empty text, so the freshly-allocated texture is cleared to
        // transparent (otherwise an always-empty panel shows raw GPU pixels).
        if last.0 == next && msdf.version != 0 {
            continue;
        }
        last.0 = next.clone();

        if let Some(mat) = materials.get_mut(&mat_node.0) {
            mat.border = border;
        }

        let glyphs = match (next.is_empty(), atlas.as_deref_mut()) {
            (false, Some(atlas)) => layout_hud_text(&next, font, *slot, atlas, &mut font_data_map),
            _ => Vec::new(),
        };
        commit_panel_glyphs(&mut msdf, glyphs);
    }
}

/// Lay out one string at `font_size` into MSDF glyphs at `offset`, returning the
/// glyphs and the laid-out block height (for stacking).
fn layout_line(
    text: &str,
    font: &VelloFont,
    font_size: f32,
    max_advance: Option<f32>,
    align: VelloTextAlign,
    offset: (f64, f64),
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> (Vec<PositionedGlyph>, f32) {
    let layout = font.layout(
        text,
        &VelloTextStyle { font_size, line_height: 1.2, ..default() },
        align,
        max_advance,
    );
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    let brush = bevy_color_to_brush(Color::srgb(0.95, 0.97, 1.0));
    let glyphs = collect_msdf_glyphs(&layout, &[], &brush, offset, atlas);
    (glyphs, layout.height())
}

/// Lay out a (possibly multi-line) readout string into MSDF glyphs for a slot,
/// wrapped to the slot's texture width and aligned per [`hud_align`]. North gets
/// special treatment: its first line is the **context name**, rendered
/// [`HUD_TITLE_SCALE`]× larger than the rest, then the status line below it.
fn layout_hud_text(
    text: &str,
    font: &VelloFont,
    slot: HudSlot,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    let (tw, _th) = hud_tex_dims(slot);
    let max_advance = Some(tw as f32 - 2.0 * HUD_PAD);
    let align = hud_align(slot);
    let pad = HUD_PAD as f64;

    if slot == HudSlot::North {
        // Title (context name) big; remainder (type · status) normal, below it.
        let mut parts = text.splitn(2, '\n');
        let title = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let (mut out, title_h) = layout_line(
            title, font, HUD_FONT_SIZE * HUD_TITLE_SCALE, max_advance, align, (pad, pad),
            atlas, font_data_map,
        );
        if !rest.is_empty() {
            let y = pad + title_h as f64 + 6.0;
            let (sub, _) = layout_line(
                rest, font, HUD_FONT_SIZE, max_advance, align, (pad, y), atlas, font_data_map,
            );
            out.extend(sub);
        }
        return out;
    }

    let (glyphs, _) = layout_line(
        text, font, HUD_FONT_SIZE, max_advance, align, (pad, pad), atlas, font_data_map,
    );
    glyphs
}

fn status_label(status: Option<Status>) -> &'static str {
    match status {
        Some(Status::Running) => "● running",
        Some(Status::Error) => "✕ error",
        Some(Status::Done) => "✓ done",
        _ => "idle",
    }
}

fn band_label(band: Band) -> &'static str {
    match band {
        Band::Hot => "hot",
        Band::RecentConcluded => "recent",
        Band::Haystack => "haystack",
    }
}

fn hud_north(d: &super::card::CardData, status: Option<Status>) -> String {
    let kind = if d.accent.is_empty() { "—" } else { d.accent.as_str() };
    format!("{}\n{} · {}", d.title, kind, status_label(status))
}

fn hud_east(d: &super::card::CardData) -> String {
    let keys = if d.keywords.is_empty() {
        "—".to_string()
    } else {
        d.keywords.join(", ")
    };
    let mut lines = vec![
        format!("model    {}", if d.model_badge.is_empty() { "—" } else { &d.model_badge }),
        format!("fork     {}", d.fork_badge.as_deref().unwrap_or("—")),
        format!("band     {}", band_label(d.band)),
        format!("keywords {keys}"),
    ];
    if let Some(c) = &d.cluster_label {
        lines.push(format!("cluster  ◇ {c}"));
    }
    // Longest line at the top, tapering to shortest — the block nests into the
    // top corner (label columns stay aligned; only the row order changes).
    lines.sort_by_key(|l| std::cmp::Reverse(l.chars().count()));
    format!("SPECS\n{}", lines.join("\n"))
}

fn hud_west(selected: ContextId, state: &TimeWellState) -> String {
    // Walk the fork-ancestry chain up (this ◂ parent ◂ …), titles from the join.
    let mut out = String::from("LINEAGE\n");
    let mut cur = Some(selected);
    let mut depth = 0;
    while let Some(id) = cur {
        let title = state
            .join
            .get(&id)
            .map(|c| c.id.display_or(Some(c.label.as_str())))
            .unwrap_or_else(|| id.short());
        if depth == 0 {
            out.push_str(&title);
        } else {
            out.push_str(&format!("\n◂ {title}"));
        }
        // Stop after a handful of generations; guard against cycles.
        depth += 1;
        if depth >= 6 {
            out.push_str("\n◂ …");
            break;
        }
        cur = state.join.get(&id).and_then(|c| c.forked_from);
        if cur == Some(id) {
            break;
        }
    }
    if depth == 1 {
        out.push_str("\n(root)");
    }
    out
}

fn hud_south(d: &super::card::CardData) -> String {
    match &d.preview {
        Some(p) => {
            let snippet: String = p.chars().take(160).collect();
            if p.chars().count() > 160 {
                format!("{snippet}…")
            } else {
                snippet
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(band: Band) -> super::super::card::CardData {
        super::super::card::CardData {
            title: "alpha".into(),
            accent: "coder".into(),
            model_badge: "anthropic/claude-opus-4-8".into(),
            fork_badge: Some("full".into()),
            keywords: vec!["rings".into(), "pulse".into()],
            preview: None,
            band,
            forked_from: None,
            cluster_label: None,
        }
    }

    #[test]
    fn north_shows_title_type_and_status() {
        let n = hud_north(&card(Band::Hot), Some(Status::Running));
        assert!(n.starts_with("alpha"), "title leads");
        assert!(n.contains("coder"), "context-type shown");
        assert!(n.contains("running"), "live status shown");
    }

    #[test]
    fn north_empty_type_falls_back_to_dash() {
        let mut c = card(Band::Hot);
        c.accent = String::new();
        let n = hud_north(&c, None);
        assert!(n.contains("— · idle"), "empty type → dash, no status → idle: {n}");
    }

    #[test]
    fn east_lists_specs_with_dash_fallbacks() {
        let mut c = card(Band::RecentConcluded);
        c.model_badge = String::new();
        c.fork_badge = None;
        c.keywords = vec![];
        let e = hud_east(&c);
        assert!(e.contains("model    —"), "empty model → dash: {e}");
        assert!(e.contains("fork     —"), "no fork → dash");
        assert!(e.contains("keywords —"), "no keywords → dash");
        assert!(e.contains("band     recent"), "band labeled");
        assert!(!e.contains("cluster"), "no cluster line when unclustered");
    }

    #[test]
    fn east_shows_cluster_only_when_present() {
        let mut c = card(Band::Haystack);
        c.cluster_label = Some("storage".into());
        let e = hud_east(&c);
        assert!(e.contains("cluster  ◇ storage"), "cluster line present: {e}");
        assert!(e.contains("rings, pulse"), "keywords joined");
    }

    #[test]
    fn south_truncates_long_preview() {
        let mut c = card(Band::Hot);
        c.preview = Some("x".repeat(300));
        let s = hud_south(&c);
        assert!(s.ends_with('…'), "long preview is elided");
        assert!(s.chars().count() <= 161, "bounded length");

        c.preview = Some("short".into());
        assert_eq!(hud_south(&c), "short", "short preview passes through");
    }

    // ── hud_slot_offset: frustum-edge placement math ──

    const D: f32 = 100.0;

    #[test]
    fn slots_sit_on_their_edges() {
        let (fov, aspect, m) = (FRAC_PI_4, 16.0 / 9.0, 0.16);
        let n = hud_slot_offset(HudSlot::North, fov, aspect, D, m);
        let s = hud_slot_offset(HudSlot::South, fov, aspect, D, m);
        let e = hud_slot_offset(HudSlot::East, fov, aspect, D, m);
        let w = hud_slot_offset(HudSlot::West, fov, aspect, D, m);
        assert!(n.y > 0.0, "north is above center");
        assert!(s.y < 0.0, "south is below center");
        assert!(e.x > 0.0, "east is right of center");
        assert!(w.x < 0.0, "west is left of center");
        // N centered horizontally; E/W anchor to the top corners but drop a bit
        // below N's row to clear the top status bar.
        assert_eq!(n.x, 0.0);
        assert_eq!(s.x, 0.0);
        assert!(e.y < n.y, "east drops below the top row to clear the bar");
        assert!(e.y > 0.0, "but is still in the upper half");
        assert_eq!(e.y, w.y, "east/west share the dropped top row");
        assert_eq!(e.x, -w.x, "east/west are symmetric");
    }

    #[test]
    fn all_slots_share_one_plane() {
        let (fov, aspect, m) = (FRAC_PI_4, 1.5, 0.1);
        let z = hud_slot_offset(HudSlot::North, fov, aspect, D, m).z;
        assert_eq!(z, -D, "panels sit at -depth in front of the camera");
        for slot in HudSlot::ALL {
            assert_eq!(hud_slot_offset(slot, fov, aspect, D, m).z, z, "shared plane");
        }
    }

    #[test]
    fn wider_aspect_pushes_sides_out_but_not_top() {
        let (fov, m) = (FRAC_PI_4, 0.12);
        let narrow = hud_slot_offset(HudSlot::East, fov, 1.0, D, m).x;
        let wide = hud_slot_offset(HudSlot::East, fov, 2.0, D, m).x;
        assert!(wide > narrow, "east moves further right as aspect widens");
        // North's vertical placement is aspect-independent.
        let n1 = hud_slot_offset(HudSlot::North, fov, 1.0, D, m).y;
        let n2 = hud_slot_offset(HudSlot::North, fov, 2.0, D, m).y;
        assert_eq!(n1, n2, "north.y does not depend on aspect");
    }

    #[test]
    fn depth_scales_extents_linearly() {
        let (fov, aspect, m) = (FRAC_PI_4, 1.6, 0.1);
        let near = hud_slot_offset(HudSlot::North, fov, aspect, D, m);
        let far = hud_slot_offset(HudSlot::North, fov, aspect, 2.0 * D, m);
        assert!((far.y - 2.0 * near.y).abs() < 1e-3, "doubling depth doubles half-height");
        assert!((far.z - 2.0 * near.z).abs() < 1e-3, "and the in-front distance");
    }

    #[test]
    fn margin_zero_lands_on_the_frustum_edge() {
        let (fov, aspect) = (FRAC_PI_4, 1.6);
        let half_h = D * (fov * 0.5).tan();
        let on_edge = hud_slot_offset(HudSlot::North, fov, aspect, D, 0.0).y;
        assert!((on_edge - half_h).abs() < 1e-3, "margin 0 → center on the edge");
        // A positive margin pulls it inboard (smaller |y|).
        let inboard = hud_slot_offset(HudSlot::North, fov, aspect, D, 0.2).y;
        assert!(inboard < on_edge, "margin pulls the panel inboard");
    }
}
