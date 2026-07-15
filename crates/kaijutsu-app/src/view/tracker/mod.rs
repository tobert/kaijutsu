//! Tracker station — the pattern-grid face at East (`docs/tracks.md`; the
//! approved plan is `snazzy-jumping-hejlsberg.md`).
//!
//! Slice 0: **track state only** (no score-cell/note content — that's
//! slice 1, after score-context sync plumbing is decided) and **read-only**
//! (patch-bay precedent; no transport RPCs exist client-side anyway — only
//! `list_tracks`, already polled into [`WellTracks`] by the room's own
//! system group).
//!
//! A classic-tracker homage honest to kaijutsu's track model: one vertical
//! column per track, rows scrolling downward at each column's **own
//! tempo** (independent clock domains, `docs/tracks.md`), a fixed playhead
//! row. [`grid`] is the pure math (column layout, tempo label, row count,
//! the scrolling/freeze row-offset, phrase marking); this module is the
//! Bevy glue — placement, the static face, and the systems that spawn/
//! animate/pulse/text/LOD the per-track columns.
//!
//! **The rendering split** (a hybrid, `snazzy-jumping-hejlsberg.md`'s
//! rendering decision): row quads move by per-frame-free `Transform`
//! writes (no asset extraction), phrase-row emphasis and the row/backing
//! surfaces are `StandardMaterial` (unlit, brightness in `base_color`, the
//! room's family-wide discipline), the playhead pulse is a quantized
//! change-guarded `base_color` write per column (`room::sync_room_glow`'s
//! own discipline — this is where the East marker's old beat-breathe is
//! re-homed, per-column instead of one shared pylon), text is MSDF panels
//! (`time_well::panel`), and the transport glyph (▶ playing / ■ stopped) is
//! geometry, not a font glyph.
//!
//! Like the patch-bay wheel at W, the whole subtree rides ONE placement
//! transform ([`STATION_E_PLACEMENT`]) that mounts it on the room's E wall
//! panel; [`spawn_furniture`] is called once from `room::enter_room` and
//! the subtree lives as long as the room. No keyboard system of its own —
//! `room::plain_zoom_keyboard` already covers Up/Esc for a zoomed panel
//! with no interactive content.

pub mod grid;

use std::time::Instant;

use bevy::prelude::*;
use kaijutsu_client::TrackInfo;
use kaijutsu_types::ContextId;

use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs};
use crate::text::shaping::VelloFont;
use crate::ui::screen::Screen;
use crate::view::palette;
use crate::view::room::nav::Station;
use crate::view::room::{PLATE_TEX_H, PLATE_TEX_W, RoomState, layout_plate_text};
use crate::view::scene_palette::{ScenePalette, lin, lin_scaled};
use crate::view::time_well::live::WellBeats;
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use crate::view::time_well::rays::WellTracks;
use crate::view::time_well::scene::accent_color;

// ── Station placement seam (Amy-tunable — the ONE knob) ──────────────────────
//
// Mirrors `patch_bay::StationPlacement`/`placement_transform` (~30 lines;
// duplication is the convention until a third consumer wants a shared
// helper). The tracker face is authored differently from the patch-bay
// wheel, though: the wheel was a horizontal table re-oriented face-out
// (pitch AND yaw, plus a roll tie-break — `patch_bay::STATION_W_PLACEMENT`'s
// doc has the full derivation); the tracker face is authored VERTICALLY
// from the start — local XY is the face plane, local +Z the outward
// normal, local +Y already "up" — so mounting it on the wall is a single
// yaw, no pitch, no roll tie-break needed.

/// A rigid-plus-uniform-scale placement of a scene into room space. Same
/// shape as `patch_bay::StationPlacement`; see that type's doc for the
/// general pitch-then-yaw composition ([`placement_rotation`]).
struct StationPlacement {
    translation: Vec3,
    scale: f32,
    pitch: f32,
    yaw: f32,
}

/// The pure rotation half of a placement: pitch about local +X, then yaw
/// about world +Y (`Ry(yaw) * Rx(pitch)`) — identical composition to
/// `patch_bay::placement_rotation`.
fn placement_rotation(p: &StationPlacement) -> Quat {
    Quat::from_rotation_y(p.yaw) * Quat::from_rotation_x(p.pitch)
}

/// The `Transform` for the placement entity that re-roots the tracker
/// subtree into room space.
fn placement_transform(p: &StationPlacement) -> Transform {
    Transform::from_translation(p.translation)
        .with_rotation(placement_rotation(p))
        .with_scale(Vec3::splat(p.scale))
}

/// Map a tracker-LOCAL point to room space through a placement — test-only,
/// the same `placement_to_room` shape `patch_bay` uses to lock its rotation
/// derivation against concrete points instead of raw quaternion algebra.
#[cfg(test)]
fn placement_to_room(p: &StationPlacement, local: Vec3) -> Vec3 {
    p.translation + placement_rotation(p) * (local * p.scale)
}

/// The tracker face's placement at E — mounted ON the wall panel, the same
/// "the surface gets taken over by its content" call the W wheel made
/// (`palette.rs`'s "Station E contract" banner has the shared numbers).
///
/// **The rotation.** The face is authored with local +Z as its outward
/// normal (a plain vertical billboard: local XY is the drawing plane, no
/// re-orientation needed the way the wheel's horizontal table needed one).
/// Mounted at E, that normal must point OUT of the panel and INTO the room:
/// the E panel sits at world +X and faces −X, so local +Z must land on
/// world −X̂. With `pitch = 0` (`placement_rotation`'s `Rx(pitch)` is the
/// identity), the rotation is a bare yaw: `Ry(θ)` sends local +Z ↦
/// `(sin θ, 0, cos θ)`, which is `(-1, 0, 0)` at `θ = -π/2`. The same `Ry`
/// sends local +X ↦ `(cos θ, 0, -sin θ) = (0, 0, 1)` — local +X (screen-
/// right, since text/columns run left-to-right in local X) lands on world
/// +Z, and local +Y (already "up") is untouched by a pure Y-rotation, so it
/// stays world +Y. No roll tie-break needed: unlike the wheel's horizontal
/// table, there was never a second candidate to choose between — the face
/// was never lying down to begin with. Locked ±1e-5 by the `placement_*`
/// tests below.
///
/// **The placement.** `translation.x` sets the face plane
/// [`palette::STATION_E_PROUD`] world-units proud of the E panel (inward,
/// toward world −X, off `palette::WALL_APOTHEM`); `translation.y` centers
/// it on the panel ([`palette::STATION_E_MOUNT_Y`], the panel's own
/// vertical center — the same number [`patch_bay`](super::patch_bay) uses
/// at W, since both walls share `room::WALL_HEIGHT`).
const STATION_E_PLACEMENT: StationPlacement = StationPlacement {
    translation: Vec3::new(
        palette::WALL_APOTHEM - palette::STATION_E_PROUD,
        palette::STATION_E_MOUNT_Y,
        0.0,
    ),
    scale: palette::STATION_E_SCALE,
    pitch: 0.0,
    yaw: -std::f32::consts::FRAC_PI_2,
};

// ── Face geometry (Amy-tunable) ─────────────────────────────────────────────

/// The face's authored size — near the E panel's own full width/height
/// (`bearing::octagon_panel_width(palette::WALL_APOTHEM)` ≈ 994,
/// `room::WALL_HEIGHT` = 560), so [`palette::STATION_E_SCALE`] starts at
/// `1.0` (`palette.rs`'s "Station E contract" doc has the reasoning).
const FACE_W: f32 = 994.0;
const FACE_H: f32 = 560.0;

/// Vertical spacing between adjacent rows, world units at face scale.
const ROW_SPACING: f32 = 18.0;

/// Where the fixed playhead sits within the scrollable row window, as a
/// fraction from the top (`0.0`) to the bottom (`1.0`) — feeds BOTH
/// [`grid::row_offset`]'s `below` argument (`R · (1 - PLAYHEAD_FRAC)`) and
/// this face's own physical playhead Y (below), so one knob controls where
/// the playhead reads on the wall and how the row math is windowed around
/// it. `0.62` reads as "just past center": a little more headroom above the
/// playhead for approaching beats than below for ones just past.
const PLAYHEAD_FRAC: f32 = 0.62;

/// How many rows fill the visible grid band at [`ROW_SPACING`] — the
/// `window_rows` argument to [`grid::row_count_for`].
const WINDOW_ROWS: usize = (GRID_TOP - GRID_BOTTOM) as usize / ROW_SPACING as usize;

/// Top margin reserved for the header/dot/glyph band at the top of each
/// column, above the scrolling grid.
const GRID_TOP_MARGIN: f32 = 96.0;
/// Bottom margin reserved for the frame.
const GRID_BOTTOM_MARGIN: f32 = 24.0;
const GRID_TOP: f32 = FACE_H / 2.0 - GRID_TOP_MARGIN;
const GRID_BOTTOM: f32 = -(FACE_H / 2.0) + GRID_BOTTOM_MARGIN;
/// The playhead's fixed world Y on the face — [`PLAYHEAD_FRAC`] of the way
/// down the grid band.
const PLAYHEAD_Y: f32 = GRID_TOP - PLAYHEAD_FRAC * (GRID_TOP - GRID_BOTTOM);

/// Column width ceiling — a handful of tracks doesn't turn into billboards
/// ([`grid::column_layout`]'s clamp).
const COL_W_MAX: f32 = 150.0;
/// Gap between adjacent columns.
const COL_GAP: f32 = 16.0;
/// Margin trimmed off each side of a column's own width for its row/accent
/// geometry, so adjacent columns never touch.
const COL_INSET: f32 = 6.0;

/// Row quad height — leaves a visible gap between rows at [`ROW_SPACING`].
const ROW_HEIGHT: f32 = ROW_SPACING * 0.72;
/// Playhead quad height — a little taller than a row so it reads as the
/// fixed reference line, not just another row.
const PLAYHEAD_HEIGHT: f32 = ROW_SPACING * 0.9;

/// Attached-context dot radius, gap between dots, and the row they float on
/// (room-scale ambient — the roster count at a glance without diving).
const DOT_RADIUS: f32 = 4.0;
const DOT_GAP: f32 = 12.0;
const DOT_Y: f32 = FACE_H / 2.0 - 46.0;
/// Cap on how many attached-context dots a column shows (`TrackInfo.attached`
/// is uncapped; a wall of dots past this reads as noise, not signal).
const DOT_CAP: usize = 8;

/// Transport glyph (▶ playing / ■ stopped) half-size and vertical seat,
/// just below the dots.
const GLYPH_SIZE: f32 = 16.0;
const GLYPH_Y: f32 = FACE_H / 2.0 - 72.0;

/// Header plate seat, just under the top edge (dived-only text —
/// [`TrackerLod`]).
const HEADER_Y: f32 = FACE_H / 2.0 - 20.0;

/// Frame ribbon width (the faint edge around the face).
const FRAME_WIDTH: f32 = 3.0;

/// Whether a row quad centered at local `y` sits fully inside the grid band.
/// There is no clip/mask geometry on the face, and a column's row set is
/// deliberately TALLER than the visible band ([`grid::row_count_for`] rounds
/// up to a phrase multiple plus a wrap margin — for a long phrase, much
/// taller) — without this cull, the margin rows draw over the glyph/dot band
/// above the grid and past the frame below it. Rows outside the band go
/// `Visibility::Hidden`, which also makes the wrap teleport invisible: a row
/// only ever jumps edges while hidden.
fn row_in_band(y: f32) -> bool {
    (GRID_BOTTOM + ROW_HEIGHT / 2.0..=GRID_TOP - ROW_HEIGHT / 2.0).contains(&y)
}

// ── State ───────────────────────────────────────────────────────────────────

/// Per-track column bookkeeping + the two row materials shared across every
/// column on the face (the "two shared handles per face, not per row"
/// discipline).
#[derive(Resource)]
pub struct TrackerState {
    /// track id → its column root entity.
    column_entities: std::collections::HashMap<String, Entity>,
    /// Re-lay-out the title/idle/header text on the next dived frame (the
    /// patch-bay `text_dirty` shape).
    text_dirty: bool,
    /// track id → last known beat position — the DURABLE half of the freeze
    /// contract (kaibo review finding): the entity-side carry in
    /// [`sync_tracker_columns`] dies with the columns at `teardown_room`, so
    /// without this map a room exit/re-entry would seat a frozen track's
    /// rows back at beat 0. Written by [`animate_tracker_scroll`] while a
    /// phasor is live, pruned to the roster on rebuild, and deliberately
    /// NOT cleared by [`Self::arm`].
    freeze_carry: std::collections::HashMap<String, f64>,
    row_material: Option<Handle<StandardMaterial>>,
    phrase_row_material: Option<Handle<StandardMaterial>>,
}

impl Default for TrackerState {
    fn default() -> Self {
        Self {
            column_entities: std::collections::HashMap::new(),
            text_dirty: true,
            freeze_carry: std::collections::HashMap::new(),
            row_material: None,
            phrase_row_material: None,
        }
    }
}

impl TrackerState {
    /// Clear the id→entity map alone (the `WellTracks::clear_ray_entities`
    /// stale-id lesson: without this, `sync_tracker_columns`'s re-entry
    /// count-fallback could match the roster by COUNT ALONE against dead
    /// entity ids left over from the previous room visit). Called from
    /// `room::enter_room`, same frame `spawn_furniture` runs.
    pub(crate) fn arm(&mut self) {
        // `freeze_carry` deliberately survives: it is the only copy of a
        // frozen track's position once teardown has taken the entities.
        self.column_entities.clear();
        self.text_dirty = true;
    }
}

// ── Components ────────────────────────────────────────────────────────────

/// The placement entity carrying [`STATION_E_PLACEMENT`] (child of
/// `RoomRoot`); [`TrackerRoot`] is its child, so the whole face moves as one.
#[derive(Component)]
struct StationEPlacement;

#[derive(Component)]
pub struct TrackerRoot;

/// The zoomed view's LOD layer — title/idle/header text: unreadable pixels
/// at room scale (`text::build_card_scenes`'s own doc has the same
/// reasoning every text-LOD gate in this app shares). Room-scale ambient is
/// the columns, grid, transport glyph, dots, and pulsing playheads.
#[derive(Component)]
pub struct TrackerLod;

#[derive(Component)]
struct TrackerTitlePlate;

/// The "NO TRACKS" plate — also [`TrackerLod`], but with its OWN extra
/// visibility condition (only while the roster is empty); see
/// [`apply_tracker_lod`]'s doc for why that has to be a second condition on
/// the SAME system rather than a second writer touching the same
/// `Visibility`.
#[derive(Component)]
struct TrackerIdlePlate;

/// One track's column: its identity, the beat key `WellBeats` phasors are
/// keyed by, its cached transport metadata (drives a respawn when it
/// changes — see [`sync_tracker_columns`]), and the frozen beat position the
/// scroll holds exactly while `WellBeats::beat_position` returns `None`
/// (`grid.rs`'s freeze-anchor contract).
#[derive(Component)]
pub struct TrackerColumn {
    pub track_id: String,
    pub score_ctx: ContextId,
    pub beats_per_phrase: u64,
    /// `TrackInfo.playing` at the last (re)spawn. Not read this slice —
    /// `sync_tracker_columns` fully rebuilds on any roster change rather
    /// than diffing per-track (see that fn's own doc), so nothing compares
    /// against this cached copy yet; kept because it's the natural seam a
    /// future incremental-diff version would compare against instead of
    /// re-querying `WellTracks`.
    #[allow(dead_code)]
    pub playing: bool,
    pub last_position: f64,
}

/// A scrolling row quad; `index` is its row number within its column's `R`.
#[derive(Component)]
struct TrackerRow {
    index: usize,
}

/// The fixed playhead quad — a child of its [`TrackerColumn`];
/// [`pulse_tracker_playheads`] reads the track identity off the parent via
/// `ChildOf` rather than duplicating it here.
#[derive(Component)]
struct TrackerPlayhead;

/// A column's header plate; holds the label text `fill_tracker_text` commits
/// once the font loads.
#[derive(Component)]
struct TrackerHeaderPlate(String);

// ── Plugin ──────────────────────────────────────────────────────────────────

pub struct TrackerPlugin;

impl Plugin for TrackerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TrackerState>().add_systems(
            Update,
            (
                sync_tracker_columns,
                animate_tracker_scroll,
                pulse_tracker_playheads,
                fill_tracker_text.run_if(tracker_zoomed),
                apply_tracker_lod,
            )
                .chain()
                .run_if(in_state(Screen::Room)),
        );
    }
}

/// Pure predicate: is `station` the room's current zoom target? Mirrors
/// `patch_bay::is_zoomed_into`.
fn is_zoomed_into(zoomed: Option<Station>, station: Station) -> bool {
    zoomed == Some(station)
}

fn tracker_zoomed(room: Res<RoomState>) -> bool {
    is_zoomed_into(room.zoomed, Station::Tracks)
}

// ── Scene lifecycle ──────────────────────────────────────────────────────────

/// Spawn the tracker face as **the east station itself** — the static half
/// (the placement anchor, the backing panel, the faint frame, and the
/// title/idle plates). The per-track columns are built by
/// [`sync_tracker_columns`] from the polled roster. Called once from
/// `room::enter_room`; the whole subtree lives as long as `RoomRoot`.
pub(crate) fn spawn_furniture(
    commands: &mut Commands,
    room_root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    std_materials: &mut Assets<StandardMaterial>,
    card_materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
) {
    let placement = commands
        .spawn((
            StationEPlacement,
            placement_transform(&STATION_E_PLACEMENT),
            Visibility::Inherited,
            Name::new("StationEPlacement"),
            ChildOf(room_root),
        ))
        .id();
    let root = commands
        .spawn((
            TrackerRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("TrackerRoot"),
            ChildOf(placement),
        ))
        .id();

    // Dark backing quad — the face's own panel surface, seated behind
    // everything else drawn on it.
    let backing_mesh = meshes.add(Rectangle::new(FACE_W, FACE_H));
    let backing_material = std_materials.add(StandardMaterial {
        base_color: lin(palette.dark_surface),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(backing_mesh),
        MeshMaterial3d(backing_material),
        Transform::from_xyz(0.0, 0.0, -2.0),
        Visibility::Inherited,
        Name::new("TrackerBacking"),
        ChildOf(root),
    ));

    // A faint gold frame around the face edge — the etch tier, same
    // brightness family the patch-bay wheel's guide rings use.
    let frame_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette.gold, palette.etch),
        unlit: true,
        ..default()
    });
    for (w, h, x, y) in frame_bars(FACE_W, FACE_H, FRAME_WIDTH) {
        commands.spawn((
            Mesh3d(meshes.add(Rectangle::new(w, h))),
            MeshMaterial3d(frame_material.clone()),
            Transform::from_xyz(x, y, -0.5),
            Visibility::Inherited,
            Name::new("TrackerFrame"),
            ChildOf(root),
        ));
    }

    // Title / idle plates (MSDF; filled by `fill_tracker_text` only while
    // dived). Both are the dive LOD — spawned `Visibility::Hidden` like
    // every other plate in the app, corrected by `apply_tracker_lod`.
    let title = header_plate_bundle(
        meshes,
        card_materials,
        images,
        Vec3::new(0.0, FACE_H / 2.0 + 44.0, 0.5),
        320.0,
    );
    commands.spawn((TrackerTitlePlate, TrackerLod, title, Name::new("TrackerTitle"), ChildOf(root)));

    let idle = header_plate_bundle(meshes, card_materials, images, Vec3::new(0.0, 0.0, 0.5), 260.0);
    commands.spawn((TrackerIdlePlate, TrackerLod, idle, Name::new("TrackerIdle"), ChildOf(root)));

    info!("tracker: stationed as the east station");
}

/// Four `(width, height, x, y)` bars framing a `w`×`h` rectangle, `t` thick.
fn frame_bars(w: f32, h: f32, t: f32) -> [(f32, f32, f32, f32); 4] {
    [
        (w, t, 0.0, h / 2.0),  // top
        (w, t, 0.0, -h / 2.0), // bottom
        (t, h, -w / 2.0, 0.0), // left
        (t, h, w / 2.0, 0.0),  // right
    ]
}

/// A floating MSDF text plate, `w` wide (height follows the shared plate
/// texture's aspect so the glyphs don't stretch — the patch-bay
/// `PORT_LABEL_H` recipe). No `upright_wall_facing` needed the way the
/// patch-bay's radially-laid-out plates need one: the tracker face's local
/// quads already face local +Z by default, which [`STATION_E_PLACEMENT`]
/// maps square onto the room.
fn header_plate_bundle(
    meshes: &mut Assets<Mesh>,
    card_materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
    pos: Vec3,
    w: f32,
) -> impl Bundle {
    let h = w * PLATE_TEX_H / PLATE_TEX_W;
    let mesh = meshes.add(Rectangle::new(w, h));
    let (image, panel) = create_msdf_panel(images, PLATE_TEX_W as u32, PLATE_TEX_H as u32);
    let material = card_materials.add(WellCardMaterial {
        texture: image,
        accent: Vec4::ZERO,
        params: Vec4::ZERO,
        shape: Vec4::new(PLATE_TEX_W / PLATE_TEX_H, 0.0, 0.0, 0.0),
        border: Vec4::ZERO,
        dim: Vec4::new(0.85, 0.0, 0.0, 0.0),
    });
    (
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(pos),
        Visibility::Hidden,
        panel,
    )
}

// ── Systems ─────────────────────────────────────────────────────────────────

/// Reconcile columns against the polled roster ([`WellTracks`]). Gated like
/// `rays::sync_track_rays` — the count fallback covers room re-entry (exit
/// despawned the columns but the roster survived), and the change check
/// dodges the same "`ResMut`'s `DerefMut` always dirties" footgun.
///
/// Unlike `sync_track_rays`, this does a full rebuild on any change rather
/// than a pure spawn-new/despawn-dead diff: a track's OWN metadata changing
/// (tempo, phrase length, play state) needs its column respawned (the row
/// count and playhead glyph both depend on it), and the roster's SIZE
/// changing needs every surviving column's x position recomputed anyway
/// (`grid::column_layout` re-centers around the new count) — between those
/// two, almost every poll where anything changed needs to touch almost
/// every column, so the extra churn of a full rebuild is cheap against the
/// ~5s poll cadence this only runs on (not a per-frame system). Recorded as
/// a deliberate deviation from `sync_track_rays`'s shape in the slice-0
/// report.
pub(crate) fn sync_tracker_columns(
    mut commands: Commands,
    well_tracks: Res<WellTracks>,
    mut state: ResMut<TrackerState>,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    mut card_materials: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    roots: Query<Entity, With<TrackerRoot>>,
    columns: Query<&TrackerColumn>,
) {
    if !well_tracks.is_changed() && state.column_entities.len() == well_tracks.tracks.len() {
        return;
    }
    let Ok(root) = roots.single() else {
        return;
    };

    // Carry each surviving track's frozen scroll position through the
    // rebuild: a stopped track's poll flips `playing` within ~5s of the
    // freeze, which lands here as a full rebuild — without this carry the
    // rebuilt rows would snap back to beat 0, breaking the exact-freeze
    // contract (`grid.rs`'s freeze test; the phasor is already gone, so
    // `animate_tracker_scroll` would never correct the snap). The entity
    // read is the fresh copy; `state.freeze_carry` is the durable fallback
    // for the rebuild the entity carry can't cover (room re-entry, where
    // teardown took the entities — kaibo review finding).
    let carried: std::collections::HashMap<String, f64> = state
        .column_entities
        .iter()
        .filter_map(|(id, e)| columns.get(*e).ok().map(|c| (id.clone(), c.last_position)))
        .collect();

    for (_, e) in state.column_entities.drain() {
        commands.entity(e).despawn();
    }
    // Prune the durable carry to the live roster so vanished track names
    // don't accumulate forever (a returning track restarts from 0 like any
    // new one).
    let roster: std::collections::HashSet<&str> =
        well_tracks.tracks.iter().map(|t| t.id.as_str()).collect();
    state.freeze_carry.retain(|id, _| roster.contains(id.as_str()));
    state.text_dirty = true;
    if well_tracks.tracks.is_empty() {
        return;
    }

    // Ordinary rows sit on the quiet `etch` tier (0.28 — engraved detail);
    // phrase rows get the louder `trough_subtle` tier (0.75). Live-verify
    // finding: the first cut had these swapped, which both inverted the
    // phrase emphasis (boundaries read as gaps) and made the whole grid a
    // wall of bright bars — the grid should be the quiet texture the
    // accent-hued playhead pulses against.
    let row_material = state
        .row_material
        .get_or_insert_with(|| {
            std_materials.add(StandardMaterial {
                base_color: lin_scaled(palette.gold, palette.etch),
                unlit: true,
                ..default()
            })
        })
        .clone();
    let phrase_material = state
        .phrase_row_material
        .get_or_insert_with(|| {
            std_materials.add(StandardMaterial {
                base_color: lin_scaled(palette.gold, palette.trough_subtle),
                unlit: true,
                ..default()
            })
        })
        .clone();

    let slots = grid::column_layout(well_tracks.tracks.len(), FACE_W, COL_W_MAX, COL_GAP);
    for (t, slot) in well_tracks.tracks.iter().zip(slots.iter()) {
        let p0 = carried
            .get(&t.id)
            .or_else(|| state.freeze_carry.get(&t.id))
            .copied()
            .unwrap_or(0.0);
        let entity = spawn_column(
            &mut commands,
            root,
            t,
            slot,
            p0,
            &palette,
            &mut meshes,
            &mut std_materials,
            &mut card_materials,
            &mut images,
            row_material.clone(),
            phrase_material.clone(),
        );
        state.column_entities.insert(t.id.clone(), entity);
    }
}

/// One track's column subtree: the accent strip, the transport glyph, up to
/// [`DOT_CAP`] attached-context dots, `R` row-line quads (phrase rows on
/// `phrase_material`, the rest on `row_material`), the playhead quad, and
/// the (hidden, dive-LOD) header plate.
#[allow(clippy::too_many_arguments)]
fn spawn_column(
    commands: &mut Commands,
    root: Entity,
    t: &TrackInfo,
    slot: &grid::ColumnSlot,
    // The scroll position to seat the rows at: the previous column's frozen
    // `last_position` when this spawn is a rebuild, `0.0` for a track seen
    // for the first time. A live phasor overrides it the same frame
    // (`animate_tracker_scroll` is chained right after the sync); a frozen
    // one never does, which is exactly the point.
    p0: f64,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    std_materials: &mut Assets<StandardMaterial>,
    card_materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
    row_material: Handle<StandardMaterial>,
    phrase_material: Handle<StandardMaterial>,
) -> Entity {
    let accent = accent_color(&t.id).to_linear();
    let accent_v = Vec3::new(accent.red, accent.green, accent.blue);
    let inner_w = (slot.width - COL_INSET * 2.0).max(1.0);

    let column = commands
        .spawn((
            TrackerColumn {
                track_id: t.id.clone(),
                score_ctx: t.score_context_id,
                beats_per_phrase: t.beats_per_phrase,
                playing: t.playing,
                last_position: p0,
            },
            Transform::from_xyz(slot.x_center, 0.0, 0.0),
            Visibility::Inherited,
            Name::new(format!("TrackerColumn-{}", t.id)),
            ChildOf(root),
        ))
        .id();

    // Accent strip: a thin vertical bar the column's own hue, spanning the
    // grid band — the column's spine, readable at room scale.
    let strip_h = GRID_TOP - GRID_BOTTOM;
    let strip_material = std_materials.add(StandardMaterial {
        base_color: lin(LinearRgba::rgb(accent_v.x, accent_v.y, accent_v.z)),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Rectangle::new(2.0, strip_h))),
        MeshMaterial3d(strip_material),
        Transform::from_xyz(-inner_w / 2.0, (GRID_TOP + GRID_BOTTOM) / 2.0, 0.05),
        Visibility::Inherited,
        Name::new("TrackerAccentStrip"),
        ChildOf(column),
    ));

    // Transport glyph: geometry, not a font glyph — ▶ playing, ■ stopped.
    let glyph_mesh = if t.playing {
        meshes.add(Triangle2d::new(
            Vec2::new(-GLYPH_SIZE * 0.5, GLYPH_SIZE * 0.6),
            Vec2::new(-GLYPH_SIZE * 0.5, -GLYPH_SIZE * 0.6),
            Vec2::new(GLYPH_SIZE * 0.6, 0.0),
        ))
    } else {
        meshes.add(Rectangle::new(GLYPH_SIZE * 0.85, GLYPH_SIZE * 0.85))
    };
    let glyph_material = std_materials.add(StandardMaterial {
        base_color: lin(LinearRgba::rgb(
            accent_v.x * palette.trim,
            accent_v.y * palette.trim,
            accent_v.z * palette.trim,
        )),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(glyph_mesh),
        MeshMaterial3d(glyph_material),
        Transform::from_xyz(0.0, GLYPH_Y, 0.2),
        Visibility::Inherited,
        Name::new("TrackerTransportGlyph"),
        ChildOf(column),
    ));

    // Attached-context dots: the roster count at a glance, room-scale
    // ambient. Centered as a row under the header band.
    let shown = t.attached.len().min(DOT_CAP);
    if shown > 0 {
        let dot_mesh = meshes.add(Circle::new(DOT_RADIUS));
        let dot_material = std_materials.add(StandardMaterial {
            base_color: lin(LinearRgba::rgb(accent_v.x, accent_v.y, accent_v.z)),
            unlit: true,
            ..default()
        });
        let span = (shown as f32 - 1.0) * DOT_GAP;
        for i in 0..shown {
            let x = -span / 2.0 + i as f32 * DOT_GAP;
            commands.spawn((
                Mesh3d(dot_mesh.clone()),
                MeshMaterial3d(dot_material.clone()),
                Transform::from_xyz(x, DOT_Y, 0.2),
                Visibility::Inherited,
                Name::new("TrackerAttachedDot"),
                ChildOf(column),
            ));
        }
    }

    // Row-line quads: R rows, phrase rows on the brighter shared material.
    // Seated at `p0` (the carried freeze position, or 0.0 for a new track) —
    // `animate_tracker_scroll` (chained right after this system) overrides
    // with the live phasor position before the frame renders when one is
    // rolling; a frozen column keeps this pose. Rows outside the band spawn
    // hidden ([`row_in_band`]).
    let total = grid::row_count_for(t.beats_per_phrase, WINDOW_ROWS);
    let below = total as f64 * (1.0 - PLAYHEAD_FRAC as f64);
    let row_mesh = meshes.add(Rectangle::new(inner_w, ROW_HEIGHT));
    for j in 0..total {
        let material =
            if grid::is_phrase_row(j, t.beats_per_phrase) { phrase_material.clone() } else { row_material.clone() };
        let y = PLAYHEAD_Y + grid::row_offset(j, p0, total, below) as f32 * ROW_SPACING;
        let vis = if row_in_band(y) { Visibility::Inherited } else { Visibility::Hidden };
        commands.spawn((
            TrackerRow { index: j },
            Mesh3d(row_mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_xyz(0.0, y, 0.0),
            vis,
            Name::new("TrackerRow"),
            ChildOf(column),
        ));
    }

    // Playhead: fixed Y, own material per column so `pulse_tracker_playheads`
    // can pulse it independently.
    let playhead_material = std_materials.add(StandardMaterial {
        base_color: lin(LinearRgba::rgb(
            accent_v.x * palette.marker,
            accent_v.y * palette.marker,
            accent_v.z * palette.marker,
        )),
        unlit: true,
        ..default()
    });
    commands.spawn((
        TrackerPlayhead,
        Mesh3d(meshes.add(Rectangle::new(inner_w, PLAYHEAD_HEIGHT))),
        MeshMaterial3d(playhead_material),
        Transform::from_xyz(0.0, PLAYHEAD_Y, 0.15),
        Visibility::Inherited,
        Name::new("TrackerPlayhead"),
        ChildOf(column),
    ));

    // Header plate: id / tempo · phrase length. Dive LOD, filled once the
    // font loads (`fill_tracker_text`).
    // "/PHR" not "/PHRASE": `layout_plate_text`'s plate is single-line-sized
    // (340×100 tex, ~18 chars/line at the plate font) — "120 BPM · 32/PHRASE"
    // is 19 chars, which parley wraps onto a third line the texture then
    // clips (live-verify finding: the header read "120 BPM · 32/" with a
    // half-clipped "PHRASE" under it).
    let header_text = format!("{}\n{} · {}/PHR", t.id, grid::tempo_label(t.period_us), t.beats_per_phrase);
    let header = header_plate_bundle(
        meshes,
        card_materials,
        images,
        Vec3::new(0.0, HEADER_Y, 0.5),
        inner_w.min(200.0),
    );
    commands.spawn((
        TrackerHeaderPlate(header_text),
        TrackerLod,
        header,
        Name::new(format!("TrackerHeader-{}", t.id)),
        ChildOf(column),
    ));

    column
}

/// Per-column scroll: `Some(p)` caches it and writes every row child's
/// `translation.y`; `None` (no live phasor — a transport flush, or a track
/// that has never rolled) skips the column entirely — the exact freeze
/// [`TrackerColumn::last_position`] holds.
fn animate_tracker_scroll(
    beats: Res<WellBeats>,
    mut state: ResMut<TrackerState>,
    mut columns: Query<(&mut TrackerColumn, &Children)>,
    mut rows: Query<(&TrackerRow, &mut Transform, &mut Visibility)>,
) {
    let now = Instant::now();
    for (mut col, children) in columns.iter_mut() {
        let Some(p) = beats.beat_position(&col.score_ctx, now) else {
            continue;
        };
        col.last_position = p;
        // The durable copy ([`TrackerState::freeze_carry`]): survives the
        // teardown that takes this component with it.
        state.freeze_carry.insert(col.track_id.clone(), p);
        let total = grid::row_count_for(col.beats_per_phrase, WINDOW_ROWS);
        let below = total as f64 * (1.0 - PLAYHEAD_FRAC as f64);
        for child in children.iter() {
            let Ok((row, mut tf, mut vis)) = rows.get_mut(child) else {
                continue;
            };
            let y = PLAYHEAD_Y + grid::row_offset(row.index, p, total, below) as f32 * ROW_SPACING;
            if (tf.translation.y - y).abs() > f32::EPSILON {
                tf.translation.y = y;
            }
            // Band cull ([`row_in_band`]): margin rows hide instead of
            // drawing over the header band / past the frame, and the wrap
            // teleport only ever happens while hidden. Change-guarded like
            // the transform write.
            let want = if row_in_band(y) { Visibility::Inherited } else { Visibility::Hidden };
            if *vis != want {
                *vis = want;
            }
        }
    }
}

/// Quantization step for the playhead glow — the `room::sync_room_glow`/
/// `GLOW_STEP` discipline, private to this module (small duplication, same
/// stance `StationPlacement`'s doc takes).
const GLOW_STEP: f32 = 1.0 / 64.0;

fn quantize(v: f32) -> f32 {
    (v / GLOW_STEP).round() * GLOW_STEP
}

/// The East beat-breathe, re-homed: each column's playhead pulses on ITS
/// OWN track's beat envelope (`WellBeats::envelope`), accent hue lifted by
/// [`ScenePalette::gain_beat`] — per-column, per-tempo, where the room's old
/// shared East marker used to breathe on the well's single loudest-track
/// envelope. Quantized + change-guarded so a quiet playhead never touches
/// `Assets<StandardMaterial>` (`room::set_glow`'s own discipline).
fn pulse_tracker_playheads(
    beats: Res<WellBeats>,
    palette: Res<ScenePalette>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    columns: Query<&TrackerColumn>,
    playheads: Query<(&ChildOf, &MeshMaterial3d<StandardMaterial>), With<TrackerPlayhead>>,
) {
    let now = Instant::now();
    for (child_of, handle) in playheads.iter() {
        let Ok(col) = columns.get(child_of.parent()) else {
            continue;
        };
        let env = beats.envelope(&col.score_ctx, now);
        let c = accent_color(&col.track_id).to_linear();
        let brightness = quantize(palette.marker + env * palette.gain_beat);
        let target = Vec3::new(c.red, c.green, c.blue) * brightness;
        let Some(cur) = materials.get(&handle.0).map(|m| m.base_color.to_linear()) else {
            continue;
        };
        let changed = (cur.red - target.x).abs() > 1e-4
            || (cur.green - target.y).abs() > 1e-4
            || (cur.blue - target.z).abs() > 1e-4;
        if changed
            && let Some(m) = materials.get_mut(&handle.0)
        {
            m.base_color = Color::LinearRgba(LinearRgba::rgb(target.x, target.y, target.z));
        }
    }
}

/// Fill/refresh the title, idle, and every header plate when dirty — the
/// same async-font gate `patch_bay::fill_patch_text` uses.
fn fill_tracker_text(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut state: ResMut<TrackerState>,
    // The three queries are pairwise disjoint via crossed `Without`s — the
    // same shape `patch_bay::fill_patch_text` uses. `With<A>` vs `With<B>`
    // alone does NOT prove disjointness to Bevy's conflict checker (nothing
    // stops one entity carrying both markers), and three `&mut
    // MsdfBlockGlyphs` queries without that proof are a B0001 panic the
    // first time the Update schedule initializes this system.
    mut titles: Query<
        &mut MsdfBlockGlyphs,
        (With<TrackerTitlePlate>, Without<TrackerIdlePlate>, Without<TrackerHeaderPlate>),
    >,
    mut idle: Query<
        &mut MsdfBlockGlyphs,
        (With<TrackerIdlePlate>, Without<TrackerTitlePlate>, Without<TrackerHeaderPlate>),
    >,
    mut headers: Query<(&TrackerHeaderPlate, &mut MsdfBlockGlyphs)>,
) {
    if !state.text_dirty {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    if let Ok(mut msdf) = titles.single_mut() {
        let glyphs = layout_plate_text("TRACKER", font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    if let Ok(mut msdf) = idle.single_mut() {
        let glyphs = layout_plate_text("NO TRACKS", font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    for (header, mut msdf) in headers.iter_mut() {
        let glyphs = layout_plate_text(&header.0, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    state.text_dirty = false;
}

/// The zoom LOD: show title/idle/header text only while
/// [`Station::Tracks`] is zoomed (`patch_bay::apply_patch_lod`'s shape).
/// [`TrackerIdlePlate`] carries a SECOND condition on top of the shared
/// zoom gate (only while the roster is empty) rather than a second system
/// writing the same `Visibility` component — two writers racing to set the
/// same field is its own footgun class (whichever runs later in the chain
/// wins, silently), so the roster check lives here, inline, as the one
/// place this entity's visibility is ever decided.
fn apply_tracker_lod(
    room: Res<RoomState>,
    well_tracks: Res<WellTracks>,
    mut lod: Query<(&mut Visibility, Has<TrackerIdlePlate>), With<TrackerLod>>,
) {
    let zoomed = is_zoomed_into(room.zoomed, Station::Tracks);
    for (mut vis, is_idle) in lod.iter_mut() {
        let want = if is_idle {
            if zoomed && well_tracks.tracks.is_empty() { Visibility::Inherited } else { Visibility::Hidden }
        } else if zoomed {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *vis != want {
            *vis = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── placement ──

    fn e_rotation_only() -> StationPlacement {
        StationPlacement {
            translation: Vec3::ZERO,
            scale: 1.0,
            pitch: STATION_E_PLACEMENT.pitch,
            yaw: STATION_E_PLACEMENT.yaw,
        }
    }

    #[test]
    fn placement_rotation_sends_local_normal_into_the_room_along_world_west() {
        // Local +Z (the face's outward normal) must land on world −X̂ — off
        // the E wall, into the room.
        let mapped = placement_to_room(&e_rotation_only(), Vec3::Z);
        assert!((mapped - Vec3::NEG_X).length() < 1e-5, "{mapped:?}");
    }

    #[test]
    fn placement_rotation_leaves_local_up_as_world_up() {
        // No roll tie-break needed: the face is already right-side up.
        let mapped = placement_to_room(&e_rotation_only(), Vec3::Y);
        assert!((mapped - Vec3::Y).length() < 1e-5, "{mapped:?}");
    }

    #[test]
    fn placement_rotation_sends_local_right_to_world_z() {
        let mapped = placement_to_room(&e_rotation_only(), Vec3::X);
        assert!((mapped - Vec3::Z).length() < 1e-5, "{mapped:?}");
    }

    #[test]
    fn placement_translation_sits_on_the_e_wall_line() {
        let at = placement_to_room(&STATION_E_PLACEMENT, Vec3::ZERO);
        assert_eq!(at, STATION_E_PLACEMENT.translation);
        assert!(at.x > 0.0, "E wall is at world +X: {at:?}");
        assert!(
            (palette::WALL_APOTHEM - at.x - palette::STATION_E_PROUD).abs() < 1e-3,
            "proud of the wall by STATION_E_PROUD, not deep inside it: {at:?}"
        );
    }

    // ── freeze carry across room visits ──

    #[test]
    fn arm_clears_entities_but_preserves_the_freeze_carry() {
        // The kaibo-review regression: room exit tears the column entities
        // down, so re-entry's `arm()` must NOT also drop the durable
        // freeze positions — they are the only copy left, and clearing
        // them would seat a frozen track's rows back at beat 0.
        let mut s = TrackerState::default();
        s.column_entities.insert("bass".to_string(), Entity::PLACEHOLDER);
        s.freeze_carry.insert("bass".to_string(), 42.5);
        s.arm();
        assert!(s.column_entities.is_empty(), "stale entity ids must go");
        assert_eq!(s.freeze_carry.get("bass"), Some(&42.5), "freeze position must survive");
    }

    // ── band cull ──

    #[test]
    fn row_in_band_accepts_the_playhead_and_rejects_the_margins() {
        // The playhead's own Y is always in-band.
        assert!(row_in_band(PLAYHEAD_Y));
        // Fully inside the band edges: in.
        assert!(row_in_band(GRID_TOP - ROW_HEIGHT));
        assert!(row_in_band(GRID_BOTTOM + ROW_HEIGHT));
        // A quad that would poke past an edge: out.
        assert!(!row_in_band(GRID_TOP));
        assert!(!row_in_band(GRID_BOTTOM));
        // Deep in the header band / below the face: out.
        assert!(!row_in_band(GLYPH_Y));
        assert!(!row_in_band(-(FACE_H / 2.0) - 10.0));
    }

    #[test]
    fn tallest_phrase_wraps_only_outside_the_band() {
        // For a long phrase (total >> window), every row the math can place
        // at the wrap seam (the extremes of `row_offset`'s range) must be
        // out of band — the teleport is never visible.
        let p_len = 64u64;
        let total = grid::row_count_for(p_len, WINDOW_ROWS);
        let below = total as f64 * (1.0 - PLAYHEAD_FRAC as f64);
        let lo = PLAYHEAD_Y + (-below) as f32 * ROW_SPACING;
        let hi = PLAYHEAD_Y + (total as f64 - below - 1.0) as f32 * ROW_SPACING;
        assert!(!row_in_band(lo), "bottom wrap seam visible: y={lo}");
        assert!(!row_in_band(hi), "top wrap seam visible: y={hi}");
    }

    // ── LOD gate ──

    #[test]
    fn is_zoomed_into_matches_only_the_named_station() {
        assert!(is_zoomed_into(Some(Station::Tracks), Station::Tracks));
        assert!(!is_zoomed_into(Some(Station::PatchBay), Station::Tracks));
        assert!(!is_zoomed_into(None, Station::Tracks));
    }
}
