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
//!
//! **HUD melt** (`docs/timewell.md`): this panel set is being absorbed into
//! in-scene surfaces slice by slice — N/E/W/S all still spawn and update
//! here for now, but East's specs block and West's ancestry chain are, as of
//! slice 3, thin wrappers over [`super::text::specs_text`] /
//! [`super::text::ancestry_text`], the same pure text the reading card now
//! carries on its own face at focus time. Slice 4 retires the panels
//! themselves once every readout has a scene-native home.

use std::f32::consts::FRAC_PI_4;

use bevy::prelude::*;
use kaijutsu_types::{ContextId, Status};

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
    /// Bottom: the selected context's **live tail** (tail -f of its block
    /// stream, [`super::live::ContextTails`]), falling back to the polled
    /// preview when the tail is empty. Re-spawned for the live-state work —
    /// it was hidden to keep the vortex throat clear, and only lights up
    /// while a selection exists; Amy judges the throat-clearance tradeoff.
    South,
    /// Bottom-left: the static keyboard legend (`legend_text`) — visible
    /// whenever the well is up, selection or not (unlike every other slot,
    /// which blanks without a selection). Anchored bottom-**left** so it
    /// doesn't collide with South's bottom-center tail.
    Legend,
}

impl HudSlot {
    /// The spawned slots.
    pub const ALL: [HudSlot; 5] =
        [HudSlot::North, HudSlot::East, HudSlot::West, HudSlot::South, HudSlot::Legend];
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
/// Fallback top-drop (fraction of the frustum half-height) used before UI layout
/// has produced a real dock/window size. ~0.15 matches a 40px dock in a
/// ~540px-tall window; [`dock_top_drop`] derives the live value from the actual
/// dock vs window height so the clearance stays correct at any resolution.
const HUD_TOP_DROP_FALLBACK: f32 = 0.15;
/// Texture-space font size + inner padding for the readout text.
const HUD_FONT_SIZE: f32 = 27.0;
/// North's context-name (first line) is rendered this much larger than the body.
const HUD_TITLE_SCALE: f32 = 1.667;
/// South's tail lines render smaller than the body so more of the stream fits.
const HUD_TAIL_SCALE: f32 = 0.75;
/// Tail lines shown in South (the newest N of the context's tail buffer).
const SOUTH_TAIL_LINES: usize = 5;
/// Chars per South tail line — sized to the South texture width at the tail
/// font so a line never wraps (wrapping would push older lines off the panel).
const SOUTH_LINE_CHARS: usize = 48;
/// Inner padding (texture px) — keeps the text inset from the frame so the panel
/// has breathing room inside its border.
const HUD_PAD: f32 = 30.0;
/// Border strength (the `WellCardMaterial.border` alpha) — no body fill, the
/// panel is a glowing frame tinted by the selection's accent, so unequal panel
/// sizes still read as a set. The HDR gain that makes the outline bloom gently
/// moved onto `ScenePalette::gain_hud_border`.
const HUD_BORDER_STRENGTH: f32 = 1.0;

/// Per-slot texture (logical authoring) size. N/S are wide-short, E/W tall-narrow;
/// the quad aspect tracks this so text never distorts.
fn hud_tex_dims(slot: HudSlot) -> (u32, u32) {
    match slot {
        HudSlot::North => (660, 220),
        // Tall enough for ~4 tail lines at HUD_FONT_SIZE + padding.
        HudSlot::South => (660, 200),
        HudSlot::East | HudSlot::West => (440, 340),
        // Wide enough for the two-column "p promote      d demote" rows.
        HudSlot::Legend => (460, 260),
    }
}

/// Text alignment inside a panel. South is a tail readout — left-aligned like
/// the spec/lineage columns, not centered like North's identity line.
fn hud_align(slot: HudSlot) -> VelloTextAlign {
    match slot {
        HudSlot::North => VelloTextAlign::Middle,
        HudSlot::East | HudSlot::West | HudSlot::South | HudSlot::Legend => VelloTextAlign::Left,
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
        // Modest corner panel — reference material, not live data; kept
        // narrower than North/South so it stays clear of South's tail.
        HudSlot::Legend => {
            let w = half_w * 0.32;
            Vec2::new(w, w / aspect)
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
/// All three top-anchored panels (N center-top, **E/W top corners**) share one
/// dropped top row — `top_drop` (fraction of the half-height, from
/// [`dock_top_drop`]) lowers that row so the panels clear the persistent top dock,
/// whose right side now carries sparklines that reach toward N at narrow widths.
pub fn hud_slot_offset(
    slot: HudSlot,
    fov_y: f32,
    aspect: f32,
    depth: f32,
    margin: f32,
    top_drop: f32,
) -> Vec3 {
    let half_h = depth * (fov_y * 0.5).tan();
    let half_w = half_h * aspect;
    let inset = 1.0 - margin;
    // Top-anchored panels share this row, dropped below the dock band.
    let top = inset - top_drop;
    match slot {
        HudSlot::North => Vec3::new(0.0, half_h * top, -depth),
        // Bottom-anchored panels: no top_drop (there's no dock to clear down
        // here). Legend sits at the bottom-LEFT corner, distinct from South's
        // bottom-center anchor.
        HudSlot::South => Vec3::new(0.0, -half_h * inset, -depth),
        HudSlot::Legend => Vec3::new(-half_w * inset, -half_h * inset, -depth),
        HudSlot::East => Vec3::new(half_w * inset, half_h * top, -depth),
        HudSlot::West => Vec3::new(-half_w * inset, half_h * top, -depth),
    }
}

/// Top-edge inset (fraction of the frustum half-height) needed to clear the
/// persistent top dock. The dock lays out in fixed pixels while the HUD is
/// frustum-proportional, so a fixed fraction would over/under-shoot as the window
/// scales; deriving from the dock's real pixel height keeps the clearance exact at
/// every resolution. The window's full height maps to `2·half_h`, so a `dock_h`-px
/// band is `2·dock_h/window_h` of the half-height. Falls back to
/// [`HUD_TOP_DROP_FALLBACK`] before layout has produced sizes.
fn dock_top_drop(window_h: f32, dock_h: f32) -> f32 {
    if window_h > 0.0 && dock_h > 0.0 {
        2.0 * dock_h / window_h
    } else {
        HUD_TOP_DROP_FALLBACK
    }
}

/// Full child-local transform for a slot: edge offset + a scale that sizes the
/// shared unit quad to [`hud_quad_size`]. Recomputed on spawn and every frame
/// ([`position_well_hud`]) so the HUD stays edge-locked across window resizes.
fn hud_transform(slot: HudSlot, fov_y: f32, aspect: f32, top_drop: f32) -> Transform {
    let half_h = HUD_DEPTH * (fov_y * 0.5).tan();
    let half_w = half_h * aspect;
    let size = hud_quad_size(slot, half_w, half_h);
    let anchor = hud_slot_offset(slot, fov_y, aspect, HUD_DEPTH, HUD_MARGIN, top_drop);
    // Fit the panel fully inside from its edge/corner anchor: pull horizontally
    // toward center by half-width (a center-anchored panel like N stays at x=0 —
    // note `f32::signum(0.0)` is 1.0, so guard the zero case) and vertically
    // toward center by half-height — DOWN from the top-anchored row (N/E/W),
    // UP from the bottom-anchored row (South, Legend — dropping South down
    // like the others pushed it half off-screen; caught live 2026-07-04).
    let cx = if anchor.x == 0.0 {
        0.0
    } else {
        anchor.x - anchor.x.signum() * size.x * 0.5
    };
    let cy = if matches!(slot, HudSlot::South | HudSlot::Legend) {
        anchor.y + size.y * 0.5
    } else {
        anchor.y - size.y * 0.5
    };
    Transform::from_translation(Vec3::new(cx, cy, anchor.z)).with_scale(Vec3::new(size.x, size.y, 1.0))
}

/// Read `(fov_y, aspect)` off a camera projection, falling back to sane defaults
/// for a non-perspective projection (the well always uses perspective).
pub(crate) fn read_perspective(projection: &Projection) -> (f32, f32) {
    match projection {
        Projection::Perspective(p) => (p.fov, p.aspect_ratio),
        _ => (FRAC_PI_4, 16.0 / 9.0),
    }
}

/// Spawn the four edge HUD panels as children of the well camera (empty until
/// a selection drives them). Not a Bevy system — a plain function so
/// `room::enter_room` can call it directly alongside its own furniture spawns
/// (the HUD spawns with the room, camera-parented as always, once per room
/// visit rather than waiting for a dive — `Screen::TimeWell`'s own
/// `OnEnter`/`OnExit` registrations are gone as of Slice D). Takes
/// `cam_entity`/`fov_y`/`aspect` as plain values rather than its own
/// `Query<(Entity, &Projection), …>` — `room::enter_room` already has these
/// off its OWN camera query (merged into one to stay under Bevy's 16-param
/// system-function ceiling), so re-querying here would just be redundant.
pub(crate) fn spawn_well_hud_furniture(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
    cam_entity: Entity,
    fov_y: f32,
    aspect: f32,
    windows: &Query<&Window>,
    dock: &Query<&ComputedNode, With<crate::ui::dock::NorthDock>>,
) {
    let top_drop = top_drop_from(windows, dock);

    // Shared unit quad; per-panel size rides on Transform.scale.
    let quad = meshes.add(Rectangle::new(1.0, 1.0));

    for slot in HudSlot::ALL {
        let (tw, th) = hud_tex_dims(slot);
        let (image, panel) = create_msdf_panel(images, tw, th);
        let material = materials.add(WellCardMaterial {
            texture: image,
            // Black, near-opaque body: the panel interior deliberately blots
            // out the well behind it so HUD text stays readable. (This was
            // accidental before the shader's mask discard was real — accent
            // ZERO used to paint black anyway because nothing discarded.)
            accent: Vec4::new(0.0, 0.0, 0.0, 0.94),
            params: Vec4::ZERO, // plain panel: no selection/lineage/status rings
            shape: hud_shape(slot),
            border: Vec4::ZERO, // outline driven by the selection in update_well_hud
            // dim.x = 1: never dimmed (not a rim Card). y/z are live
            // chatter/beat lanes — 0, or the frame washes cyan+gold.
            dim: Vec4::new(1.0, 0.0, 0.0, 0.0),
        });
        // Room-scale default (Slice C): the panel starts hidden — the same
        // "spawned Hidden, only the zoom shows them" contract patch bay's own
        // LOD layer uses — and `apply_well_hud_lod` corrects it every frame
        // from `RoomState::zoomed`.
        commands
            .spawn((
                WellHud,
                slot,
                HudText::default(),
                Mesh3d(quad.clone()),
                MeshMaterial3d(material),
                hud_transform(slot, fov_y, aspect, top_drop),
                Visibility::Hidden,
                panel,
                Name::new("WellHudPanel"),
            ))
            .insert(ChildOf(cam_entity));
    }
}

/// The HUD's LOD gate (Slice C): show the panels while
/// [`super::scene::well_zoomed`], hide them at room scale — mirrors
/// `patch_bay::apply_patch_lod` exactly (same reasoning: this must run in the
/// AMBIENT tier, not dived-only, so it reacts to a zoom-OUT transition too,
/// not just zoom-in). Change-guarded so a settled panel never re-dirties.
pub fn apply_well_hud_lod(
    room: Res<crate::view::room::RoomState>,
    mut panels: Query<&mut Visibility, With<WellHud>>,
) {
    let want = if super::scene::well_zoomed(&room) { Visibility::Inherited } else { Visibility::Hidden };
    for mut vis in panels.iter_mut() {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Keep each panel edge-locked: re-derive its transform from the live projection
/// (cheap — four `Vec3`s) so the HUD tracks window-aspect/FOV changes.
pub fn position_well_hud(
    camera: Query<&Projection, With<Camera3d>>,
    windows: Query<&Window>,
    dock: Query<&ComputedNode, With<crate::ui::dock::NorthDock>>,
    mut panels: Query<(&HudSlot, &mut Transform), With<WellHud>>,
) {
    let Ok(projection) = camera.single() else {
        return;
    };
    let (fov_y, aspect) = read_perspective(projection);
    let top_drop = top_drop_from(&windows, &dock);
    for (slot, mut tf) in panels.iter_mut() {
        *tf = hud_transform(*slot, fov_y, aspect, top_drop);
    }
}

/// Read the live window + dock heights and turn them into a [`dock_top_drop`]
/// fraction. Shared by spawn and per-frame positioning so both agree.
fn top_drop_from(
    windows: &Query<&Window>,
    dock: &Query<&ComputedNode, With<crate::ui::dock::NorthDock>>,
) -> f32 {
    let window_h = windows.single().map(|w| w.height()).unwrap_or(0.0);
    let dock_h = dock.single().map(|c| c.size().y).unwrap_or(0.0);
    dock_top_drop(window_h, dock_h)
}

/// Refresh the four readouts from the current selection. Recomputes the formatted
/// string every frame (cheap) but only re-lays-out MSDF glyphs — and re-tints the
/// plate — when a panel's string actually changes. Nothing selected → blank.
pub fn update_well_hud(
    state: Res<TimeWellState>,
    tails: Res<super::live::ContextTails>,
    tracks: Res<super::rays::WellTracks>,
    palette: Res<crate::view::scene_palette::ScenePalette>,
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
                c.red * palette.gain_hud_border,
                c.green * palette.gain_hud_border,
                c.blue * palette.gain_hud_border,
                HUD_BORDER_STRENGTH,
            )
        }
        None => Vec4::ZERO,
    };

    for (slot, mut last, mut msdf, mat_node) in panels.iter_mut() {
        let next = match (selected, slot) {
            // Legend is static and selection-independent — checked first so
            // both arms of `selected` fall through to it.
            (_, HudSlot::Legend) => legend_text(),
            (Some(card), HudSlot::North) => hud_north(&card.data, card.status),
            (Some(card), HudSlot::East) => {
                hud_east(&card.data, tracks.track_info_of(&card.context_id))
            }
            (Some(card), HudSlot::West) => hud_west(card.context_id, &state),
            (Some(card), HudSlot::South) => {
                hud_south(&card.data, &tails, card.context_id)
            }
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

    // South is the tail readout: smaller type so ~5 stream lines fit.
    let font_size = if slot == HudSlot::South {
        HUD_FONT_SIZE * HUD_TAIL_SCALE
    } else {
        HUD_FONT_SIZE
    };
    let (glyphs, _) = layout_line(
        text, font, font_size, max_advance, align, (pad, pad), atlas, font_data_map,
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

/// Static keyboard legend content ([`HudSlot::Legend`]) — the verbs are
/// provisional per the design; this listing is their source of truth in-app.
/// Never changes, so `update_well_hud`'s change-guard means this only ever
/// lays out once per well session.
fn legend_text() -> String {
    "CONTROLS\n\
     p promote      d demote\n\
     z pause        a archive\n\
     c conclude     0-9 seat\n\
     \u{23ce} enter        esc back"
        .to_string()
}

fn hud_north(d: &super::card::CardData, status: Option<Status>) -> String {
    let kind = if d.accent.is_empty() { "—" } else { d.accent.as_str() };
    let paused = if d.paused { " · paused" } else { "" };
    format!("{}\n{} · {}{}", d.title, kind, status_label(status), paused)
}

/// Thin wrapper: the specs block's pure composition moved to
/// [`super::text::specs_text`] (HUD-melt slice 3) so the reading card can
/// absorb the same content — see that function's own doc. Kept here so this
/// module's own call site and tests below don't move.
fn hud_east(d: &super::card::CardData, track: Option<&kaijutsu_client::TrackInfo>) -> String {
    super::text::specs_text(d, track)
}

/// Thin wrapper: the ancestry chain's pure composition moved to
/// [`super::text::ancestry_text`] (HUD-melt slice 3) — see that function's
/// own doc.
fn hud_west(selected: ContextId, state: &TimeWellState) -> String {
    super::text::ancestry_text(selected, |id| {
        state.join.get(&id).map(|c| (c.id.display_or(Some(c.label.as_str())), c.forked_from))
    })
}

/// South = the selected context's live tail (tail -f, oldest → newest), each
/// line truncated so it never wraps; falls back to the polled preview when the
/// context hasn't produced a tail line since the app started. Line
/// pick/truncate/join is [`super::live::tail_lines`] (shared with the card
/// face's own live-tail band, slice 2 of the HUD-melt plan) — only the
/// preview fallback stays local here, since the card face already shows that
/// preview as its gist line and would otherwise repeat it.
fn hud_south(
    d: &super::card::CardData,
    tails: &super::live::ContextTails,
    ctx: ContextId,
) -> String {
    match super::live::tail_lines(tails, ctx, SOUTH_TAIL_LINES, SOUTH_LINE_CHARS) {
        Some(joined) => joined,
        None => match &d.preview {
            Some(p) => crate::text::truncate_chars(p, 161),
            None => String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only: `band_label` moved to `super::text` (HUD-melt slice 3), so
    // this module's own use of `Band` no longer reaches past `mod tests` —
    // importing it only here (not at module scope) keeps a plain (non-test)
    // build free of an unused-import warning.
    use kaijutsu_viz::layout::Band;

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
            paused: false,
        }
    }

    #[test]
    fn north_shows_title_type_and_status() {
        let n = hud_north(&card(Band::Active), Some(Status::Running));
        assert!(n.starts_with("alpha"), "title leads");
        assert!(n.contains("coder"), "context-type shown");
        assert!(n.contains("running"), "live status shown");
    }

    #[test]
    fn north_empty_type_falls_back_to_dash() {
        let mut c = card(Band::Active);
        c.accent = String::new();
        let n = hud_north(&c, None);
        assert!(n.contains("— · idle"), "empty type → dash, no status → idle: {n}");
    }

    #[test]
    fn north_shows_paused_when_set() {
        let mut c = card(Band::Active);
        c.paused = true;
        let n = hud_north(&c, Some(Status::Running));
        assert!(n.contains("running · paused"), "paused appends to the status line: {n}");

        c.paused = false;
        let n = hud_north(&c, Some(Status::Running));
        assert!(!n.contains("paused"), "unpaused card carries no paused marker: {n}");
    }

    #[test]
    fn east_lists_specs_with_dash_fallbacks() {
        let mut c = card(Band::Recent);
        c.model_badge = String::new();
        c.fork_badge = None;
        c.keywords = vec![];
        let e = hud_east(&c, None);
        assert!(e.contains("model    —"), "empty model → dash: {e}");
        assert!(e.contains("fork     —"), "no fork → dash");
        assert!(e.contains("keywords —"), "no keywords → dash");
        assert!(e.contains("band     recent"), "band labeled");
        assert!(!e.contains("cluster"), "no cluster line when unclustered");
        assert!(!e.contains("track"), "no track line when unattached");
    }

    #[test]
    fn east_shows_cluster_only_when_present() {
        let mut c = card(Band::Demoted);
        c.cluster_label = Some("storage".into());
        let e = hud_east(&c, None);
        assert!(e.contains("cluster  ◇ storage"), "cluster line present: {e}");
        assert!(e.contains("rings, pulse"), "keywords joined");
    }

    #[test]
    fn east_shows_the_track_line_with_live_transport() {
        let mut t = kaijutsu_client::TrackInfo {
            id: "bass".into(),
            score_context_id: ContextId::from_bytes([9; 16]),
            playing: true,
            playhead_tick: 128,
            period_us: 500_000, // 120 BPM
            beats_per_phrase: 32,
            beat_count: 128,
            last_epoch_ns: 1,
            clock_kind: "system".into(),
            attached: vec![],
        };
        let e = hud_east(&card(Band::Active), Some(&t));
        assert!(e.contains("track    ♪ bass ▶ ♩120 · tick 128"), "playing: {e}");

        t.playing = false;
        let e = hud_east(&card(Band::Active), Some(&t));
        assert!(e.contains("track    ♪ bass ■ stopped"), "stopped: {e}");
    }

    #[test]
    fn south_falls_back_to_truncated_preview_without_a_tail() {
        let tails = super::super::live::ContextTails::default();
        let ctx = ContextId::from_bytes([7; 16]);
        let mut c = card(Band::Active);
        c.preview = Some("x".repeat(300));
        let s = hud_south(&c, &tails, ctx);
        assert!(s.ends_with('…'), "long preview is elided");
        assert!(s.chars().count() <= 161, "bounded length");

        c.preview = Some("short".into());
        assert_eq!(hud_south(&c, &tails, ctx), "short", "short preview passes through");
    }

    #[test]
    fn south_shows_the_newest_tail_lines_over_the_preview() {
        use super::super::live::{ContextTails, TailLine};
        let ctx = ContextId::from_bytes([7; 16]);
        let mut tails = ContextTails::default();
        for i in 0..(SOUTH_TAIL_LINES + 2) {
            tails.push(
                ctx,
                TailLine::new("✦", format!("event {i}")),
                i as f64,
            );
        }
        let mut c = card(Band::Active);
        c.preview = Some("stale preview".into());
        let s = hud_south(&c, &tails, ctx);
        assert!(!s.contains("stale preview"), "live tail wins over the poll preview");
        let shown: Vec<&str> = s.lines().collect();
        assert_eq!(shown.len(), SOUTH_TAIL_LINES, "newest N lines: {s}");
        assert!(shown[0].contains("event 2"), "oldest shown line: {s}");
        assert!(
            shown.last().unwrap().contains(&format!("event {}", SOUTH_TAIL_LINES + 1)),
            "newest line last: {s}"
        );
        // A different context still falls back to its preview.
        let other = ContextId::from_bytes([8; 16]);
        assert_eq!(hud_south(&c, &tails, other), "stale preview");
    }

    // ── hud_slot_offset: frustum-edge placement math ──

    const D: f32 = 100.0;

    #[test]
    fn slots_sit_on_their_edges() {
        let (fov, aspect, m, drop) = (FRAC_PI_4, 16.0 / 9.0, 0.16, 0.15);
        let n = hud_slot_offset(HudSlot::North, fov, aspect, D, m, drop);
        let s = hud_slot_offset(HudSlot::South, fov, aspect, D, m, drop);
        let e = hud_slot_offset(HudSlot::East, fov, aspect, D, m, drop);
        let w = hud_slot_offset(HudSlot::West, fov, aspect, D, m, drop);
        assert!(n.y > 0.0, "north is above center");
        assert!(s.y < 0.0, "south is below center");
        assert!(e.x > 0.0, "east is right of center");
        assert!(w.x < 0.0, "west is left of center");
        // N centered horizontally; E/W anchor to the top corners. All three
        // top-anchored panels share one dock-cleared row.
        assert_eq!(n.x, 0.0);
        assert_eq!(s.x, 0.0);
        assert_eq!(e.y, n.y, "east shares north's dropped top row");
        assert_eq!(e.y, w.y, "east/west share the dropped top row");
        assert!(e.y > 0.0, "but the row is still in the upper half");
        assert_eq!(e.x, -w.x, "east/west are symmetric");
    }

    #[test]
    fn legend_sits_bottom_left_distinct_from_south() {
        let (fov, aspect, m, drop) = (FRAC_PI_4, 16.0 / 9.0, 0.16, 0.15);
        let legend = hud_slot_offset(HudSlot::Legend, fov, aspect, D, m, drop);
        let south = hud_slot_offset(HudSlot::South, fov, aspect, D, m, drop);
        let west = hud_slot_offset(HudSlot::West, fov, aspect, D, m, drop);
        assert!(legend.x < 0.0, "legend is left of center");
        assert!(legend.y < 0.0, "legend is below center");
        assert_eq!(legend.y, south.y, "legend shares south's bottom row (no top_drop)");
        assert_ne!(legend.x, south.x, "legend doesn't sit on top of south's bottom-center anchor");
        assert_eq!(legend.x, west.x, "legend shares west's left column");
        assert_ne!(legend.y, west.y, "legend and west are on different (bottom vs top) rows");
    }

    #[test]
    fn legend_panel_pulls_up_from_its_bottom_anchor_like_south() {
        let (fov, aspect, drop) = (FRAC_PI_4, 16.0 / 9.0, 0.15);
        let legend = hud_transform(HudSlot::Legend, fov, aspect, drop);
        let anchor = hud_slot_offset(HudSlot::Legend, fov, aspect, HUD_DEPTH, HUD_MARGIN, drop);
        assert!(legend.translation.y > anchor.y, "legend sits ABOVE its bottom-edge anchor");
        let half_h = HUD_DEPTH * (fov * 0.5).tan();
        assert!(
            legend.translation.y - legend.scale.y * 0.5 >= -half_h,
            "legend stays fully on-screen"
        );
    }

    #[test]
    fn legend_text_matches_the_documented_controls() {
        let text = legend_text();
        assert!(text.starts_with("CONTROLS"));
        for verb in ["p promote", "d demote", "z pause", "a archive", "c conclude", "0-9 seat", "esc back"] {
            assert!(text.contains(verb), "legend must mention {verb:?}: {text}");
        }
    }

    /// Caught live 2026-07-04: South was dropped DOWN from its bottom-edge
    /// anchor like the top-anchored panels, pushing the tail half off-screen.
    #[test]
    fn south_panel_pulls_up_from_its_bottom_anchor() {
        let (fov, aspect, drop) = (FRAC_PI_4, 16.0 / 9.0, 0.15);
        let s = hud_transform(HudSlot::South, fov, aspect, drop);
        let s_anchor = hud_slot_offset(HudSlot::South, fov, aspect, HUD_DEPTH, HUD_MARGIN, drop);
        assert!(s.translation.y > s_anchor.y, "south sits ABOVE its bottom-edge anchor");
        let n = hud_transform(HudSlot::North, fov, aspect, drop);
        let n_anchor = hud_slot_offset(HudSlot::North, fov, aspect, HUD_DEPTH, HUD_MARGIN, drop);
        assert!(n.translation.y < n_anchor.y, "north drops BELOW its top anchor");
        // South's bottom edge stays inside the frustum.
        let half_h = HUD_DEPTH * (fov * 0.5).tan();
        assert!(s.translation.y - s.scale.y * 0.5 >= -half_h, "south fully on-screen");
    }

    #[test]
    fn all_slots_share_one_plane() {
        let (fov, aspect, m, drop) = (FRAC_PI_4, 1.5, 0.1, 0.1);
        let z = hud_slot_offset(HudSlot::North, fov, aspect, D, m, drop).z;
        assert_eq!(z, -D, "panels sit at -depth in front of the camera");
        for slot in HudSlot::ALL {
            assert_eq!(hud_slot_offset(slot, fov, aspect, D, m, drop).z, z, "shared plane");
        }
    }

    #[test]
    fn wider_aspect_pushes_sides_out_but_not_top() {
        let (fov, m, drop) = (FRAC_PI_4, 0.12, 0.15);
        let narrow = hud_slot_offset(HudSlot::East, fov, 1.0, D, m, drop).x;
        let wide = hud_slot_offset(HudSlot::East, fov, 2.0, D, m, drop).x;
        assert!(wide > narrow, "east moves further right as aspect widens");
        // North's vertical placement is aspect-independent.
        let n1 = hud_slot_offset(HudSlot::North, fov, 1.0, D, m, drop).y;
        let n2 = hud_slot_offset(HudSlot::North, fov, 2.0, D, m, drop).y;
        assert_eq!(n1, n2, "north.y does not depend on aspect");
    }

    #[test]
    fn depth_scales_extents_linearly() {
        let (fov, aspect, m, drop) = (FRAC_PI_4, 1.6, 0.1, 0.1);
        let near = hud_slot_offset(HudSlot::North, fov, aspect, D, m, drop);
        let far = hud_slot_offset(HudSlot::North, fov, aspect, 2.0 * D, m, drop);
        assert!((far.y - 2.0 * near.y).abs() < 1e-3, "doubling depth doubles half-height");
        assert!((far.z - 2.0 * near.z).abs() < 1e-3, "and the in-front distance");
    }

    #[test]
    fn margin_and_drop_zero_lands_on_the_frustum_edge() {
        let (fov, aspect) = (FRAC_PI_4, 1.6);
        let half_h = D * (fov * 0.5).tan();
        let on_edge = hud_slot_offset(HudSlot::North, fov, aspect, D, 0.0, 0.0).y;
        assert!((on_edge - half_h).abs() < 1e-3, "margin 0 + drop 0 → center on the edge");
        // A positive margin pulls it inboard (smaller |y|).
        let inboard = hud_slot_offset(HudSlot::North, fov, aspect, D, 0.2, 0.0).y;
        assert!(inboard < on_edge, "margin pulls the panel inboard");
    }

    #[test]
    fn positive_top_drop_lowers_the_top_row() {
        let (fov, aspect, m) = (FRAC_PI_4, 1.6, 0.02);
        let no_drop = hud_slot_offset(HudSlot::North, fov, aspect, D, m, 0.0).y;
        let dropped = hud_slot_offset(HudSlot::North, fov, aspect, D, m, 0.15).y;
        assert!(dropped < no_drop, "a top-drop lowers north below the frustum top");
    }

    #[test]
    fn dock_top_drop_scales_with_resolution() {
        // A 40px dock is a larger fraction of a short window than a tall one.
        let small = dock_top_drop(540.0, 40.0);
        let large = dock_top_drop(1080.0, 40.0);
        assert!((small - 2.0 * 40.0 / 540.0).abs() < 1e-6, "exact fraction of half-height");
        assert!(large < small, "taller window → smaller fractional drop");
        // Falls back before layout has produced real sizes.
        assert_eq!(dock_top_drop(0.0, 40.0), HUD_TOP_DROP_FALLBACK);
        assert_eq!(dock_top_drop(540.0, 0.0), HUD_TOP_DROP_FALLBACK);
    }
}
