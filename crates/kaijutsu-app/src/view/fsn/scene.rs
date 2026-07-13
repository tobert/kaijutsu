//! FSN-world Bevy glue: spawn/despawn, the fly camera, per-directory-field
//! mesh entities, LOD-tier visibility gating, and selection. Every actual
//! shape/threshold this module reaches for lives in [`super::layout`]
//! (pure, unit-tested) — this file's own job is turning those numbers into
//! `Entity`/`Mesh`/`Visibility`.
//!
//! # Entity/mesh count strategy
//!
//! **Four entities per known directory field** (not per cell — the task's
//! own budget discipline): a seam-grid `LineList`, a wireframe-prism
//! `LineList` (every cell's prism in ONE mesh, `layout::field_wireframe`),
//! a sparse-seed-points `TriangleList` (one octahedron per child, `layout::
//! point_marker_mesh_data`), and a vertex-points `TriangleList` (one
//! octahedron per prism-top vertex). All four exist for a known field
//! regardless of tier — [`apply_fsn_lod`] swaps `Visibility`, never rebuilds
//! — plus ONE shared selection-ring entity for the whole world (not one per
//! field). A field is only built once its directory's listing is known
//! (`sync::FsnState`); an unenumerated directory has no entities of its own
//! at all — it renders only as its cell (a point or prism, whatever tier its
//! PARENT field is drawing at) inside the parent's already-built meshes.
//!
//! # Material choice: reused, not new
//!
//! Every mesh here is unlit [`StandardMaterial`] with brightness carried in
//! `base_color` (values > 1.0 blooming through the app's existing bloom
//! pass) — the SAME idiom `room::mod`'s own module doc describes for the
//! floor/dome/walls, applied to `LineList`/point meshes instead of
//! `TriangleList` surfaces. `Material`'s fragment stage doesn't care about
//! primitive topology, so nothing about line-list rendering needed a new
//! shader; [`crate::shaders::TraceGlowMaterial`] (the other candidate) was
//! skipped because slice 0 has no moving-glow requirement to justify its
//! per-vertex UV/time uniform — a flat brightness swap on tier change is
//! all three of the "wireframe/seam/vertex" lanes need.

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_viz::fsn::{FsnField, NodeKind, Rect, layout_field};

use crate::ui::screen::Screen;
use crate::view::scene_palette::{ScenePalette, lin_scaled, warmth_tint};

use super::heat::{FsnHeat, HEAT_GAIN_LIFT};
use super::layout;
use super::sync::{ChildMeta, FsnState};

// ── Amy-tunable render constants ────────────────────────────────────────────

/// Point-marker half-extent (world units) for the sparse-tier seed dots.
const SPARSE_POINT_SIZE: f32 = 9.0;
/// Point-marker half-extent for the attended-tier prism-top vertex points —
/// a hair larger, since they read as the "you are here" detail layer.
const VERTEX_POINT_SIZE: f32 = 13.0;

/// Wireframe brightness at the Wireframe (mid) tier — LDR, a calm presence.
const WIREFRAME_GAIN_MID: f32 = 0.85;
/// Wireframe brightness at the Attended (near) tier — HDR via
/// `ScenePalette::crest`, "full-brightness edges" per the design doc.
const VERTEX_GAIN: f32 = 1.0;

/// Fly speed (world units/second) and altitude-adjust speed.
const CAM_FLY_SPEED: f32 = 900.0;
const CAM_ALTITUDE_SPEED: f32 = 700.0;

/// Starting camera pose on N-dive: high and pulled back over the world's
/// near edge, pitched down onto it — "arrows/WASD translate over the
/// plane... camera pitched down ~35-50°." **First guess, live-tune over
/// BRP.**
const START_EYE: Vec3 = Vec3::new(0.0, 1400.0, 1400.0);
const START_LOOK: Vec3 = Vec3::new(0.0, 0.0, -100.0);

/// Selection-ring geometry (world units) — a thin annulus at the selected
/// cell's seed point, ground level.
const SELECTION_RING_OUTER: f32 = 46.0;
const SELECTION_RING_INNER: f32 = 34.0;
const SELECTION_RING_Y: f32 = 1.2;

// ── Components ────────────────────────────────────────────────────────────

/// Root of every FSN-world entity — despawn is recursive
/// (`OnExit(Screen::Fsn)`), same discipline as `room::RoomRoot`.
#[derive(Component)]
pub struct FsnRoot;

/// Marks the shared app camera while the FSN world owns it (the room-camera
/// claim pattern: insert on entry, remove + restore clear colour on exit —
/// `room::enter_room`/`teardown_room`'s own idiom, reused rather than
/// spawning a second camera).
#[derive(Component)]
pub struct FsnCamera;

/// The one shared selection-ring entity. `pub(crate)`: it appears in
/// [`apply_fsn_lod`]'s own `Query` type parameters, and a `pub fn`'s
/// signature can't leak a more-private type.
#[derive(Component)]
pub(crate) struct FsnSelectionRing;

// ── Resources ────────────────────────────────────────────────────────────

/// One known directory field's spawned entities + the per-cell data
/// [`fsn_select`] and [`sync_fsn_fields`] need (name, seed XZ, kind, and the
/// inset cell bbox, all in `field.cells`' own name-sorted order so the four
/// stay zipped).
struct FieldEntities {
    seam: Entity,
    sparse_points: Entity,
    wireframe: Entity,
    wireframe_material: Handle<StandardMaterial>,
    vertex_points: Entity,
    /// World-space center of this field's own rect — [`apply_fsn_lod`]'s
    /// camera-distance anchor.
    rect_center: Vec3,
    /// The listing generation this field was built from — [`sync_fsn_fields`]'s
    /// rebuild-detection key.
    built_generation: u64,
    cell_names: Vec<String>,
    cell_seed_xz: Vec<[f32; 2]>,
    cell_kind: Vec<NodeKind>,
    /// Each cell's [`layout::cell_bbox_inset`] rect — where that cell's OWN
    /// subdirectory field goes if/when its listing arrives (`None` = the
    /// cell is too small to host one). Bboxes, not whole polygons: this is
    /// all the child-placement chain needs, and it's smaller to retain.
    cell_bbox: Vec<Option<Rect>>,
}

/// Every known directory's spawned field entities, keyed by its own absolute
/// VFS path. Cleared on `OnExit(Screen::Fsn)` alongside the entities
/// themselves (the entities die with `FsnRoot`'s recursive despawn; this map
/// must not out-live them — a stale `Entity` here would be a dangling
/// reference on the next visit).
#[derive(Resource, Default)]
pub struct FsnFields {
    fields: HashMap<String, FieldEntities>,
}

/// The nearest cell to the camera's forward-ray ground intersection, if any —
/// [`fsn_select`]'s output, read by [`apply_fsn_lod`] for the highlight ring.
#[derive(Resource, Default)]
pub struct FsnSelection {
    pub selected: Option<String>,
}

// ── Enter / exit ─────────────────────────────────────────────────────────

/// Claim the shared camera, spawn the (empty) world root + selection ring,
/// and kick off the root listing fetch if nothing is cached yet (a re-dive
/// reuses whatever `FsnState` already cached — see that resource's own doc).
pub fn enter_fsn(
    mut commands: Commands,
    palette: Res<ScenePalette>,
    mut state: ResMut<FsnState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    // `Without<FsnBackdropCamera>`: defensive — the backdrop's off-screen
    // RTT camera (`super::backdrop`) is a `Camera3d` too. It's despawned on
    // `OnExit(Screen::Room)`, which Bevy runs before `OnEnter(Screen::Fsn)`
    // for the same transition, so it shouldn't be alive when this runs —
    // but the exclusion costs nothing and removes the ordering assumption.
    mut app_camera: Query<
        (Entity, &mut Camera, &mut Transform),
        (With<Camera3d>, Without<super::backdrop::FsnBackdropCamera>),
    >,
    existing: Query<Entity, With<FsnRoot>>,
) {
    if !existing.is_empty() {
        return;
    }

    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(FsnCamera);
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.bg));
        *tf = Transform::from_translation(START_EYE).looking_at(START_LOOK, Vec3::Y);
    }

    let root = commands
        .spawn((FsnRoot, Transform::default(), Visibility::Inherited, Name::new("FsnRoot")))
        .id();

    let ring_mesh = meshes.add(Annulus::new(SELECTION_RING_INNER, SELECTION_RING_OUTER));
    let ring_mat = mats.add(unlit(lin_scaled(palette.fsn_vertex, palette.crest)));
    commands.spawn((
        FsnSelectionRing,
        Mesh3d(ring_mesh),
        MeshMaterial3d(ring_mat),
        Transform::from_xyz(0.0, SELECTION_RING_Y, 0.0)
            .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Hidden,
        Name::new("FsnSelectionRing"),
        ChildOf(root),
    ));

    if state.listings.is_empty() {
        state.request("/".into());
    }

    info!("fsn: entered (DATA HORIZON — the VFS landscape)");
}

/// Despawn the world root (recursive — every field's meshes, the selection
/// ring), release the camera claim, and drop the now-dangling entity map.
/// `FsnState`'s own cache (`listings`) survives — see its doc — so a later
/// re-dive doesn't re-fetch the root from scratch.
pub fn exit_fsn(
    mut commands: Commands,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<FsnRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<FsnCamera>>,
    mut fields: ResMut<FsnFields>,
    mut selection: ResMut<FsnSelection>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        commands.entity(cam_entity).remove::<FsnCamera>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
    fields.fields.clear();
    selection.selected = None;
    info!("fsn: exited");
}

// ── Field mesh sync ──────────────────────────────────────────────────────

/// (Re)build a directory field's four entities whenever its listing's
/// generation changes (including "doesn't exist yet"). Runs unconditionally
/// every frame in `Screen::Fsn` rather than gating on `FsnState::is_changed`
/// — the poll/apply systems touch `FsnState` (queue bookkeeping) far more
/// often than `listings` actually grows, so a precise change-detection gate
/// would either over-fire anyway or risk missing a real update; the
/// per-path generation check below is cheap enough (a HashMap walk over a
/// few dozen directories) that ambient re-evaluation costs nothing worth
/// guarding against (the time-well freeze-fix lesson: prefer "recompute,
/// cheaply" over a latch that can go stale).
///
/// # Field placement: parent-cell bboxes, built root-first
///
/// A non-root path's rect comes from its PARENT's already-built
/// [`FieldEntities::cell_bbox`] (see `layout`'s module doc for why this
/// replaced quadtree-quadrant descent). That makes build order a real
/// dependency chain: if a listing's parent field isn't built yet (HashMap
/// iteration order is arbitrary), the path is simply skipped this frame and
/// builds on a later one — listings arrive root-first anyway (the depth-2
/// front-load ingests a parent before its expanded children), so the chain
/// settles within a frame or two of data landing. Skips are also the
/// fail-soft for a child name missing from the parent's cells (stale or
/// truncated parent listing) and for a cell too small to host a field
/// (`cell_bbox` = `None`).
pub fn sync_fsn_fields(
    mut commands: Commands,
    state: Res<FsnState>,
    mut fields: ResMut<FsnFields>,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    roots: Query<Entity, With<FsnRoot>>,
) {
    let Ok(root) = roots.single() else { return };

    // Captured ONCE per run (not per field/per cell) — a wall-clock read is
    // cheap but there's no reason to take it more than once for however many
    // fields rebuild this frame. Every field built this frame shares the same
    // "now"; recency otherwise only refreshes on the field's own next
    // rebuild (a listing re-fetch bumping `built_generation` — see the
    // `needs_build` check below), never ambiently between fetches. Accepted:
    // a cell's tint holds steady between dives rather than visibly aging in
    // place while you watch it, which the design doc doesn't ask for at
    // slice 1 and ambient per-frame re-tinting would cost a HashMap rebuild
    // per field per frame for no visible gain.
    let now_secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    for (path, listing) in state.listings.iter() {
        let needs_build = fields
            .fields
            .get(path)
            .is_none_or(|fe| fe.built_generation != listing.generation);
        if !needs_build {
            continue;
        }

        // Resolve the field's world rect BEFORE tearing anything down, so a
        // skip (parent not built yet, cell missing, cell too small) leaves
        // any existing field standing rather than despawned-and-not-rebuilt.
        let rect = if path == "/" {
            layout::root_world_rect()
        } else {
            let Some((parent, name)) = layout::split_parent(path) else { continue };
            let Some(parent_fe) = fields.fields.get(parent) else { continue };
            let Some(i) = parent_fe.cell_names.iter().position(|n| n == name) else { continue };
            let Some(bbox) = parent_fe.cell_bbox[i] else { continue };
            bbox
        };

        if let Some(old) = fields.fields.remove(path) {
            despawn_field(&mut commands, &old);
            // A rebuild re-runs this field's Voronoi layout, which moves its
            // cells' bboxes — every DESCENDANT field's rect derived from the
            // old layout is now stale geometry. Drop them too; they rebuild
            // over the following frames from the new bboxes (their own
            // `needs_build` sees them missing).
            let prefix = if path == "/" { "/".to_string() } else { format!("{path}/") };
            let stale: Vec<String> =
                fields.fields.keys().filter(|p| p.starts_with(&prefix)).cloned().collect();
            for p in stale {
                if let Some(fe) = fields.fields.remove(&p) {
                    despawn_field(&mut commands, &fe);
                }
            }
        }

        let child_specs: Vec<_> = listing
            .children
            .iter()
            .map(|c: &ChildMeta| layout::child_spec(&c.name, c.kind, c.size, c.child_count))
            .collect();
        let field = layout_field(rect, &child_specs);

        let entities = spawn_field_entities(
            &mut commands,
            root,
            path,
            rect,
            &field,
            listing.generation,
            &listing.children,
            now_secs,
            &palette,
            &mut meshes,
            &mut mats,
        );
        fields.fields.insert(path.clone(), entities);
    }
}

/// Despawn one field's four entities — the teardown half of a rebuild.
fn despawn_field(commands: &mut Commands, fe: &FieldEntities) {
    commands.entity(fe.seam).despawn();
    commands.entity(fe.sparse_points).despawn();
    commands.entity(fe.wireframe).despawn();
    commands.entity(fe.vertex_points).despawn();
}

/// Build and spawn one field's four entities (all `Visibility::Hidden` at
/// spawn — [`apply_fsn_lod`] sets the real tier the very next frame; nothing
/// flashes fully visible for one frame before the LOD gate catches up).
///
/// # Recency glow (lane A1): the vertex-color half of the composition law
///
/// Each cell's own child (`children`, this directory's listing — matched by
/// name, since `field.cells` is built name-sorted from the same listing)
/// contributes a mtime age against `now_secs`, [`layout::recency_weight`]
/// turns that into a 0..1 "how gold" weight, and [`warmth_tint`] bakes it
/// into a per-channel vertex-color tint against `palette.fsn_edge`
/// (wireframe) / `palette.fsn_vertex` (both point meshes) — see this
/// module's doc for why the material's own `base_color` (hue × gain × heat,
/// `apply_fsn_lod`'s sole write) stays untouched by this: `tint × base_color
/// == lerp(base_color, gold, w)` exactly, so the two channels compose
/// without either one clobbering the other. A child missing from `children`
/// (shouldn't happen — `field.cells` is built FROM this same listing) reads
/// as infinitely aged (no tint) rather than defaulting to full gold, the
/// safer failure direction.
#[allow(clippy::too_many_arguments)]
fn spawn_field_entities(
    commands: &mut Commands,
    root: Entity,
    path: &str,
    rect: Rect,
    field: &FsnField,
    generation: u64,
    children: &[ChildMeta],
    now_secs: u64,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) -> FieldEntities {
    let mtimes: HashMap<&str, u64> = children.iter().map(|c| (c.name.as_str(), c.mtime_secs)).collect();
    let recency_of = |name: &str| -> f32 {
        let age_secs = match mtimes.get(name) {
            Some(&mtime) => (now_secs as i64 - mtime as i64) as f64,
            None => f64::INFINITY,
        };
        layout::recency_weight(age_secs)
    };
    let edge_tints: Vec<[f32; 4]> =
        field.cells.iter().map(|c| warmth_tint(palette.fsn_edge, palette.gold, recency_of(&c.name))).collect();
    let vertex_tints: Vec<[f32; 4]> =
        field.cells.iter().map(|c| warmth_tint(palette.fsn_vertex, palette.gold, recency_of(&c.name))).collect();

    let seam_mesh = meshes.add(line_list_mesh(&layout::flatten_segments(&layout::seam_grid(rect))));
    let seam_mat = mats.add(unlit(lin_scaled(palette.fsn_seam, 1.0)));
    let seam = commands
        .spawn((
            Mesh3d(seam_mesh),
            MeshMaterial3d(seam_mat),
            Transform::default(),
            Visibility::Hidden,
            Name::new(format!("FsnSeam({path})")),
            ChildOf(root),
        ))
        .id();

    // Sparse points: one seed per cell — a straight one-color-per-marker
    // expansion via `per_cell_colors`.
    let sparse_positions = layout::field_seed_points(field);
    let (sparse_verts, sparse_indices) =
        layout::point_marker_mesh_data(&sparse_positions, SPARSE_POINT_SIZE);
    let sparse_marker_counts = vec![layout::MARKER_VERT_COUNT; field.cells.len()];
    let sparse_colors = layout::per_cell_colors(&sparse_marker_counts, &vertex_tints);
    let sparse_mesh = meshes.add(triangle_list_mesh_colored(sparse_verts, sparse_indices, sparse_colors));
    let sparse_mat = mats.add(unlit(lin_scaled(palette.fsn_vertex, 0.55)));
    let sparse_points = commands
        .spawn((
            Mesh3d(sparse_mesh),
            MeshMaterial3d(sparse_mat),
            Transform::default(),
            Visibility::Hidden,
            Name::new(format!("FsnSparsePoints({path})")),
            ChildOf(root),
        ))
        .id();

    // Wireframe: `field_wireframe` concatenates every cell's own
    // `prism_wireframe` in `field.cells`' own order (its own doc), so each
    // cell's `6 × polygon.len()` position count (2 positions per segment ×
    // `3 × polygon.len()` segments) computed straight from `field.cells`
    // lines up 1:1 with that same concatenation — the "vertex counts line
    // up" invariant `per_cell_colors` depends on, without re-deriving the
    // segment geometry a second time.
    let wireframe_positions = layout::flatten_segments(&layout::field_wireframe(field));
    let wire_vert_counts: Vec<usize> = field.cells.iter().map(|c| 6 * c.polygon.len()).collect();
    let wireframe_colors = layout::per_cell_colors(&wire_vert_counts, &edge_tints);
    let wireframe_mesh = meshes.add(line_list_mesh_colored(&wireframe_positions, &wireframe_colors));
    let wireframe_material = mats.add(unlit(lin_scaled(palette.fsn_edge, WIREFRAME_GAIN_MID)));
    let wireframe = commands
        .spawn((
            Mesh3d(wireframe_mesh),
            MeshMaterial3d(wireframe_material.clone()),
            Transform::default(),
            Visibility::Hidden,
            Name::new(format!("FsnWireframe({path})")),
            ChildOf(root),
        ))
        .id();

    // Vertex points: one octahedron per prism-TOP vertex — one level of
    // `per_cell_colors` expands each cell's tint across its own polygon
    // vertices, a second level expands each of THOSE across the marker
    // template's own vertex count.
    let top_positions = layout::field_top_vertices(field);
    let (top_verts, top_indices) = layout::point_marker_mesh_data(&top_positions, VERTEX_POINT_SIZE);
    let per_vertex_counts: Vec<usize> = field.cells.iter().map(|c| c.polygon.len()).collect();
    let per_top_vertex_tints = layout::per_cell_colors(&per_vertex_counts, &vertex_tints);
    let top_marker_counts = vec![layout::MARKER_VERT_COUNT; top_positions.len()];
    let top_colors = layout::per_cell_colors(&top_marker_counts, &per_top_vertex_tints);
    let vertex_mesh = meshes.add(triangle_list_mesh_colored(top_verts, top_indices, top_colors));
    let vertex_mat = mats.add(unlit(lin_scaled(palette.fsn_vertex, palette.crest * VERTEX_GAIN)));
    let vertex_points = commands
        .spawn((
            Mesh3d(vertex_mesh),
            MeshMaterial3d(vertex_mat),
            Transform::default(),
            Visibility::Hidden,
            Name::new(format!("FsnVertexPoints({path})")),
            ChildOf(root),
        ))
        .id();

    let rect_center = Vec3::new(
        ((rect.x0 + rect.x1) * 0.5) as f32,
        0.0,
        ((rect.y0 + rect.y1) * 0.5) as f32,
    );
    let cell_names: Vec<String> = field.cells.iter().map(|c| c.name.clone()).collect();
    let cell_seed_xz: Vec<[f32; 2]> =
        field.cells.iter().map(|c| [c.seed.x as f32, c.seed.y as f32]).collect();
    let cell_kind: Vec<NodeKind> = field.cells.iter().map(|c| c.kind).collect();
    let cell_bbox: Vec<Option<Rect>> = field
        .cells
        .iter()
        .map(|c| layout::cell_bbox_inset(&c.polygon, layout::SUBFIELD_INSET_FRAC))
        .collect();

    FieldEntities {
        seam,
        sparse_points,
        wireframe,
        wireframe_material,
        vertex_points,
        rect_center,
        built_generation: generation,
        cell_names,
        cell_seed_xz,
        cell_kind,
        cell_bbox,
    }
}

fn unlit(color: Color) -> StandardMaterial {
    StandardMaterial { base_color: color, unlit: true, ..default() }
}

/// `pub(super)`: reused as-is (uncolored — no recency bake) by
/// [`super::backdrop`]'s cold-start seam grid / ship silhouette, which don't
/// carry per-cell tint data of their own.
pub(super) fn line_list_mesh(positions: &[[f32; 3]]) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::PrimitiveTopology;

    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions.to_vec())
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
}

/// [`line_list_mesh`] plus a baked per-vertex `Mesh::ATTRIBUTE_COLOR` — the
/// wireframe's recency tint (lane A1). `colors.len()` must equal
/// `positions.len()` (the caller's own [`layout::per_cell_colors`] expansion
/// guarantees this — see [`spawn_field_entities`]'s doc).
fn line_list_mesh_colored(positions: &[[f32; 3]], colors: &[[f32; 4]]) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::PrimitiveTopology;

    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions.to_vec())
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors.to_vec())
}

/// `pub(super)`: reused as-is (uncolored) by [`super::backdrop`]'s seed-point
/// meshes.
pub(super) fn triangle_list_mesh(positions: Vec<[f32; 3]>, indices: Vec<u32>) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_indices(Indices::U32(indices))
}

/// [`triangle_list_mesh`] plus a baked per-vertex `Mesh::ATTRIBUTE_COLOR` —
/// the two point meshes' recency tint (lane A1). `colors.len()` must equal
/// `positions.len()`.
fn triangle_list_mesh_colored(positions: Vec<[f32; 3]>, indices: Vec<u32>, colors: Vec<[f32; 4]>) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

// ── Camera fly ───────────────────────────────────────────────────────────

/// Keyboard-first fly: arrows/WASD translate over the world's XZ plane
/// (world-axis-relative, not camera-relative — the camera holds a fixed
/// pitch, never yaws, so "forward" always means the same world direction);
/// PgUp/PgDn adjust altitude. Both axes clamp via `layout::clamp_camera_xz`/
/// `clamp_altitude`.
pub fn fsn_camera_fly(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut camera: Query<&mut Transform, With<FsnCamera>>,
) {
    let Ok(mut tf) = camera.single_mut() else { return };
    let dt = time.delta_secs();
    let mut dx: f32 = 0.0;
    let mut dz: f32 = 0.0;
    if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
        dz -= 1.0;
    }
    if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
        dz += 1.0;
    }
    if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        dx -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        dx += 1.0;
    }
    if dx != 0.0 || dz != 0.0 {
        let len = (dx * dx + dz * dz).sqrt();
        let (x, z) = layout::clamp_camera_xz(
            tf.translation.x + dx / len * CAM_FLY_SPEED * dt,
            tf.translation.z + dz / len * CAM_FLY_SPEED * dt,
        );
        tf.translation.x = x;
        tf.translation.z = z;
    }

    let mut dy = 0.0;
    if keys.pressed(KeyCode::PageUp) || keys.pressed(KeyCode::Equal) {
        dy += 1.0;
    }
    if keys.pressed(KeyCode::PageDown) || keys.pressed(KeyCode::Minus) {
        dy -= 1.0;
    }
    if dy != 0.0 {
        tf.translation.y = layout::clamp_altitude(tf.translation.y + dy * CAM_ALTITUDE_SPEED * dt);
    }
}

// ── Selection ────────────────────────────────────────────────────────────

/// The cell nearest the camera's forward-ray ground intersection selects:
/// updates [`FsnSelection`] and, **on the frame the selection flips** to a
/// directory cell, requests its listing ("selection approach on a truncated
/// cell triggers the deeper snapshot"). The request is edge-triggered, not
/// per-frame: `FsnState::request` deliberately stays re-queueable for a
/// truncated listing (that's its retry path), so calling it every frame the
/// same cell remains selected would refetch a genuinely-truncated directory
/// (more entries than the kernel cap, e.g. a huge `target/`) forever — one
/// RPC after another for as long as you look at it. Re-selecting the cell
/// later (look away, look back) is the retry gesture for both truncation
/// and a failed fetch.
pub fn fsn_select(
    camera: Query<&Transform, With<FsnCamera>>,
    fields: Res<FsnFields>,
    mut state: ResMut<FsnState>,
    mut selection: ResMut<FsnSelection>,
) {
    let Ok(cam_tf) = camera.single() else { return };
    let forward = cam_tf.forward().as_vec3();
    let ground = layout::ray_ground_intersection(cam_tf.translation.to_array(), forward.to_array());
    let Some(ground) = ground else {
        selection.selected = None;
        return;
    };

    let mut candidate_paths: Vec<String> = Vec::new();
    let mut candidate_xz: Vec<[f32; 2]> = Vec::new();
    let mut candidate_kind: Vec<NodeKind> = Vec::new();
    for (base_path, fe) in fields.fields.iter() {
        for ((name, xz), kind) in fe.cell_names.iter().zip(&fe.cell_seed_xz).zip(&fe.cell_kind) {
            candidate_paths.push(layout::join_path(base_path, name));
            candidate_xz.push(*xz);
            candidate_kind.push(*kind);
        }
    }

    let Some(idx) = layout::nearest_index(ground, &candidate_xz) else {
        selection.selected = None;
        return;
    };
    let path = candidate_paths[idx].clone();
    if selection.selected.as_deref() != Some(path.as_str()) {
        // Edge: the selection actually flipped to this cell. Only now does a
        // directory cell request its listing — see this system's doc for why
        // per-frame requesting would hot-loop on a truncated directory. As a
        // side benefit, `FsnState` is only ever `DerefMut`-touched on the
        // flip, so its change detection stays quiet while the selection
        // rests.
        if candidate_kind[idx] == NodeKind::Dir {
            state.request(path.clone());
        }
        selection.selected = Some(path);
    }
}

/// Move/hide the shared selection ring to sit on whatever [`FsnSelection`]
/// currently names, and lift the SELECTED field's wireframe brightness a
/// further notch beyond its LOD tier's own gain — the "edge brightness
/// lift... or a highlight ring" the design doc offers as either/or; slice 0
/// ships both, since the ring alone can be easy to lose against a bright
/// Attended-tier field and the edge lift alone gives no readout at
/// Wireframe/Sparse distance.
pub fn apply_fsn_lod(
    camera: Query<&Transform, With<FsnCamera>>,
    fields: Res<FsnFields>,
    selection: Res<FsnSelection>,
    state: Res<FsnState>,
    palette: Res<ScenePalette>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    // `Without<FsnSelectionRing>` keeps this disjoint from `rings`' own
    // `&mut Visibility` access below — the ring entity carries `Visibility`
    // too, so an unfiltered query here would alias it and Bevy would refuse
    // to schedule the system.
    mut visibilities: Query<&mut Visibility, Without<FsnSelectionRing>>,
    mut rings: Query<(&mut Transform, &mut Visibility), (With<FsnSelectionRing>, Without<FsnCamera>)>,
) {
    let Ok(cam_tf) = camera.single() else { return };

    // Which field (if any) owns the current selection, and where that
    // cell's seed sits — resolved once, reused by both the per-field
    // brightness lift below and the ring placement at the end.
    let selected_hit: Option<(String, [f32; 2])> = selection.selected.as_ref().and_then(|sel| {
        fields.fields.iter().find_map(|(base, fe)| {
            fe.cell_names
                .iter()
                .position(|n| layout::join_path(base, n) == *sel)
                .map(|i| (base.clone(), fe.cell_seed_xz[i]))
        })
    });

    for (path, fe) in fields.fields.iter() {
        let dist = cam_tf.translation.distance(fe.rect_center);
        // Always true today (a field only exists once its own listing is
        // known — see the module doc), but reading it from `FsnState`
        // rather than hardcoding `true` means a future re-fetch that
        // invalidates a listing degrades this field's tier for free.
        let tier = layout::lod_tier(state.is_enumerated(path), dist);
        let is_selected_field = selected_hit.as_ref().is_some_and(|(base, _)| base == path);

        set_visibility(&mut visibilities, fe.seam, Visibility::Inherited);
        set_visibility(&mut visibilities, fe.sparse_points, vis(matches!(tier, layout::LodTier::Sparse)));
        set_visibility(&mut visibilities, fe.wireframe, vis(!matches!(tier, layout::LodTier::Sparse)));
        set_visibility(&mut visibilities, fe.vertex_points, vis(matches!(tier, layout::LodTier::Attended)));

        let base_gain = match tier {
            layout::LodTier::Sparse | layout::LodTier::Wireframe => WIREFRAME_GAIN_MID,
            layout::LodTier::Attended => palette.crest,
        };
        let gain = if is_selected_field { base_gain * SELECTION_GAIN_LIFT } else { base_gain };
        if let Some(mat) = mats.get_mut(&fe.wireframe_material) {
            let want = lin_scaled(palette.fsn_edge, gain);
            if mat.base_color != want {
                mat.base_color = want;
            }
        }
    }

    if let Ok((mut ring_tf, mut ring_vis)) = rings.single_mut() {
        match selected_hit {
            Some((_, xz)) => {
                ring_tf.translation.x = xz[0];
                ring_tf.translation.z = xz[1];
                *ring_vis = Visibility::Inherited;
            }
            None => *ring_vis = Visibility::Hidden,
        }
    }
}

/// Extra brightness multiplier on top of a field's own tier gain when it
/// holds the current selection — the ring alone can be lost against a
/// bright field; this makes the selected field's whole edge set read
/// hotter, not just the one cell's ring.
const SELECTION_GAIN_LIFT: f32 = 1.3;

fn vis(want: bool) -> Visibility {
    if want { Visibility::Inherited } else { Visibility::Hidden }
}

fn set_visibility(
    visibilities: &mut Query<&mut Visibility, Without<FsnSelectionRing>>,
    entity: Entity,
    want: Visibility,
) {
    if let Ok(mut cur) = visibilities.get_mut(entity)
        && *cur != want
    {
        *cur = want;
    }
}

// ── Keyboard ─────────────────────────────────────────────────────────────

/// Esc surfaces back to the room — `Screen::Fsn` was entered FROM
/// `Screen::Room` (the N-dive), so that's the level directly above, same as
/// every other dive's Esc discipline (`docs/scenes/shell.md`).
pub fn fsn_keyboard(keys: Res<ButtonInput<KeyCode>>, mut next: ResMut<NextState<Screen>>) {
    if keys.just_pressed(KeyCode::Escape) {
        next.set(Screen::Room);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::fsn::sync::DirListing;
    use kaijutsu_client::VfsFileType;

    fn child(name: &str, kind: VfsFileType, size: u64, child_count: u32) -> ChildMeta {
        ChildMeta { name: name.into(), kind, size, child_count, ignored: false, mtime_secs: 0 }
    }

    fn listing(generation: u64, children: Vec<ChildMeta>) -> DirListing {
        DirListing { generation, truncated_here: false, children }
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(Assets::<Mesh>::default())
            .insert_resource(Assets::<StandardMaterial>::default())
            .init_resource::<ScenePalette>()
            .init_resource::<FsnState>()
            .init_resource::<FsnFields>()
            .add_systems(Update, sync_fsn_fields);
        app.world_mut().spawn(FsnRoot);
        app
    }

    /// The fresh-dive shape: a root listing and one expanded subdirectory
    /// listing (the depth-2 front-load) must BOTH get fields — the subdir's
    /// placed inside its own cell's bbox in the root field, within the root
    /// world square. Two updates cover the arbitrary HashMap iteration
    /// order (a child skips the frame its parent isn't built yet — the
    /// documented root-first dependency chain).
    #[test]
    fn root_and_subdir_fields_build_from_listings() {
        let mut app = test_app();
        {
            let mut state = app.world_mut().resource_mut::<FsnState>();
            state.listings.insert(
                "/".into(),
                listing(
                    1,
                    vec![
                        child("src", VfsFileType::Directory, 0, 3),
                        child("Cargo.toml", VfsFileType::File, 512, 0),
                        child("docs", VfsFileType::Directory, 0, 8),
                    ],
                ),
            );
            state
                .listings
                .insert("/src".into(), listing(1, vec![child("main.rs", VfsFileType::File, 4096, 0)]));
        }
        app.update();
        app.update();

        let fields = app.world().resource::<FsnFields>();
        assert!(fields.fields.contains_key("/"), "root field builds");
        let src = fields.fields.get("/src").expect("subdir field builds inside its parent cell");

        // The subdir field sits inside the root world square, not stacked at
        // some quadrant-hash address.
        let root_rect = layout::root_world_rect();
        assert!(
            (root_rect.x0..=root_rect.x1).contains(&(src.rect_center.x as f64))
                && (root_rect.y0..=root_rect.y1).contains(&(src.rect_center.z as f64)),
            "subdir field center {:?} must lie within the root world rect",
            src.rect_center
        );
    }

    /// A parent rebuild (listing generation bump) re-runs its Voronoi layout
    /// and moves its cells — descendant fields built from the OLD layout are
    /// stale geometry and must be dropped and rebuilt, not left standing at
    /// their old rects.
    #[test]
    fn a_parent_rebuild_drops_and_rebuilds_descendant_fields() {
        let mut app = test_app();
        {
            let mut state = app.world_mut().resource_mut::<FsnState>();
            state.listings.insert(
                "/".into(),
                listing(1, vec![child("src", VfsFileType::Directory, 0, 1)]),
            );
            state
                .listings
                .insert("/src".into(), listing(1, vec![child("lib.rs", VfsFileType::File, 64, 0)]));
        }
        app.update();
        app.update();
        let old_src_seam = app.world().resource::<FsnFields>().fields["/src"].seam;

        // The root listing re-arrives at a new generation (e.g. a truncated
        // re-fetch): its field AND the /src descendant must rebuild.
        {
            let mut state = app.world_mut().resource_mut::<FsnState>();
            state.listings.insert(
                "/".into(),
                listing(
                    2,
                    vec![
                        child("src", VfsFileType::Directory, 0, 1),
                        child("new_dir", VfsFileType::Directory, 0, 2),
                    ],
                ),
            );
        }
        app.update();
        app.update();

        let fields = app.world().resource::<FsnFields>();
        assert!(fields.fields.contains_key("/"), "root rebuilt");
        let new_src = fields.fields.get("/src").expect("descendant rebuilt from the new layout");
        assert_ne!(new_src.seam, old_src_seam, "descendant entities must be fresh, not stale");
    }

    /// The recency bake (lane A1): a built wireframe mesh must carry
    /// `Mesh::ATTRIBUTE_COLOR` with exactly one entry per position — the
    /// per-cell tint expansion's own invariant (`layout::per_cell_colors`'
    /// vertex counts must line up with the mesh's own position buffer, or
    /// the render pipeline silently drops/misreads the attribute).
    #[test]
    fn wireframe_mesh_carries_a_matching_length_vertex_color_attribute() {
        let mut app = test_app();
        {
            let mut state = app.world_mut().resource_mut::<FsnState>();
            state.listings.insert(
                "/".into(),
                listing(
                    1,
                    vec![
                        child("recent.rs", VfsFileType::File, 128, 0),
                        child("ancient.log", VfsFileType::File, 4096, 0),
                        child("sub", VfsFileType::Directory, 0, 2),
                    ],
                ),
            );
        }
        app.update();

        let wireframe_entity = app.world().resource::<FsnFields>().fields["/"].wireframe;
        let mesh_handle =
            app.world().get::<Mesh3d>(wireframe_entity).expect("wireframe entity has a mesh").0.clone();
        let meshes = app.world().resource::<Assets<Mesh>>();
        let mesh = meshes.get(&mesh_handle).expect("mesh asset must exist");
        let positions = mesh.attribute(Mesh::ATTRIBUTE_POSITION).expect("positions attribute");
        let colors = mesh.attribute(Mesh::ATTRIBUTE_COLOR).expect("baked recency color attribute");
        assert_eq!(colors.len(), positions.len(), "one baked color per vertex position");
    }
}

