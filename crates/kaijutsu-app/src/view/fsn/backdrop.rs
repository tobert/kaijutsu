//! The room's N-window backdrop (lane A4): while `Screen::Room` is live, an
//! off-screen camera renders a sparse-tier impression of the FSN world to a
//! texture, shown through two wall panels flanking the N bearing — "you can
//! see it out there before you dive" (`docs/scenes/vfs.md`). Distinct from
//! [`super::scene`]'s own world (`Screen::Fsn`): this is a much cheaper,
//! always-uncolored glimpse (no recency bake, no heat, no LOD swap — just a
//! seam grid, up to a dozen seed-point clusters, and the ship silhouette),
//! rendered on its own camera/render-layer so it never contends with the
//! main camera's own scene.
//!
//! # Two-camera isolation (the prerequisite bug this lane's own fix note calls out)
//!
//! Adding a second [`Camera3d`] breaks every `Query<.., With<Camera3d>>
//! .single()` call that can run while `Screen::Room` is live — Bevy's
//! `single()` fails once more than one entity matches. `super::mod`'s own
//! camera-query fixes (`time_well::legend`, `room::mod`'s `enter_room`, and
//! `super::scene`'s `enter_fsn`, all now `Without<FsnBackdropCamera>`) are
//! the other half of this; this module is careful never to insert
//! [`super::scene::FsnCamera`] or `room::RoomCamera` on its own camera
//! entity, and never to query `With<Camera3d>` without excluding itself.
//!
//! # Render-layer split
//!
//! The backdrop camera and every entity it should see carry
//! `RenderLayers::layer(`[`FSN_BACKDROP_LAYER`]`)`; the main camera carries
//! no `RenderLayers` at all (Bevy's implicit layer 0), so it never sees this
//! layer's content — and the backdrop camera, seeing ONLY that layer, never
//! sees the room itself. The two window quads are the one deliberate
//! exception: they carry no `RenderLayers` (main-camera-visible, showing the
//! rendered texture), while everything else under [`FsnBackdropRoot`] is
//! layer-`FSN_BACKDROP_LAYER`-only.
//!
//! # Content: reused, not duplicated
//!
//! Every mesh here is built from [`super::layout`]'s own pure functions (the
//! same seam grid / ship silhouette / seed-point builders [`super::scene`]
//! uses) via [`super::scene::line_list_mesh`]/[`super::scene::
//! triangle_list_mesh`] (`pub(super)`, reused as-is — no recency bake, no
//! per-cell tint, this is the plain uncolored path those two helpers already
//! supported before lane A1 added their colored siblings).

use std::collections::HashMap;

use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use kaijutsu_viz::fsn::{FsnField, layout_field};

use crate::view::room::bearing::{self, Bearing};
use crate::view::scene_palette::{ScenePalette, lin_scaled};

use super::layout;
use super::sync::FsnState;

// ── Amy-tunable render constants ────────────────────────────────────────────

/// Render-target texture side length (square, pixels). **Amy-tunable.**
pub const BACKDROP_TEX: u32 = 512;

/// The `RenderLayers` index every backdrop-only entity (and the backdrop
/// camera itself) carries — keeps the backdrop scene invisible to the main
/// camera (layer 0) and vice versa. Layer 1 is left free (unused elsewhere
/// today) in case a future RTT lane wants it.
const FSN_BACKDROP_LAYER: usize = 2;

/// Window quad size (world units) — sized to sit comfortably inside the N
/// wall panel next to its nameplate (`room::mod`'s `PLATE_QUAD_W/H`, 210×62,
/// is the nearby reference scale; these read as narrower-and-taller, a
/// window rather than a plate). **Amy-tunable, first guess.**
pub const WINDOW_W: f32 = 150.0;
pub const WINDOW_H: f32 = 220.0;

/// Fraction of the N panel's own half-width each window sits out from
/// center (so the two windows flank the panel's midline, not overlap it).
/// **Amy-tunable.**
pub const WINDOW_OFFSET_FRAC: f32 = 0.28;

/// How far proud of the wall panel's own face the window quads sit — same
/// z-fighting-avoidance idiom as `room::mod`'s `WALL_TRIM_PROUD`/
/// `WALL_THREAD_PROUD`, its own constant here since that one is private to
/// `room::mod`. **Amy-tunable.**
pub const WINDOW_PROUD: f32 = 3.0;

/// World-Y the window quads (and the N-panel transform they're built from)
/// center at. Mirrors `room::mod`'s `WALL_HEIGHT * 0.5` (560.0 × 0.5 =
/// 280.0) — kept as this module's own constant rather than importing the
/// private `WALL_HEIGHT` (`room::mod` doesn't expose it). **Amy-tunable —
/// re-tune together if `WALL_HEIGHT` ever moves.**
pub const WINDOW_Y: f32 = 280.0;

/// Brightness multiplier baked into the window material's `base_color` on
/// top of the sampled texture — >1.0 so the rendered scene's own bright
/// texels (the ship, the seam grid) cross the bloom threshold and the
/// window reads as lit glass rather than a flat picture. **Amy-tunable.**
pub const WINDOW_LIFT: f32 = 1.15;

/// Point-marker half-extent for the backdrop's own seed-point clusters —
/// matches `scene::SPARSE_POINT_SIZE`'s value (that constant is private to
/// `scene`, so this is its own copy, not a re-export). **Amy-tunable.**
const BACKDROP_POINT_SIZE: f32 = 9.0;

/// Hard cap on how many directory fields the backdrop bothers building
/// (root + this many depth-1 children) — the backdrop is a glimpse, not the
/// real world; capping keeps a huge root directory from spawning dozens of
/// seed-point meshes for a texture nobody can read detail on anyway.
/// **Amy-tunable.**
const BACKDROP_FIELD_CAP: usize = 11;

// ── Components ───────────────────────────────────────────────────────────

/// Marks the backdrop's own off-screen camera — never carries
/// [`super::scene::FsnCamera`] or `room::RoomCamera`; this is a THIRD
/// camera role, always rendering to a texture, never the shared on-screen
/// camera. `pub`: every `Query<.., With<Camera3d>>` elsewhere in the app
/// that can run while `Screen::Room` is live needs `Without<Self>` to keep
/// `.single()` working once this camera exists alongside the main one.
#[derive(Component)]
pub struct FsnBackdropCamera;

/// Root of every backdrop-scene entity (camera-visible content — the seam
/// grid, seed-point clusters, ship silhouette — NOT the window quads, which
/// are structural children of this same entity but render on the main
/// camera's own layer 0; see the module doc's render-layer split). Kept as
/// its OWN root — never `ChildOf(RoomRoot)` — because `RoomRoot`'s own
/// spawn commands are deferred in the same schedule this spawns in
/// (`OnEnter(Screen::Room)`); parenting under an entity ID that may not
/// have landed yet would be a same-frame race.
#[derive(Component)]
pub struct FsnBackdropRoot;

// ── Resources ────────────────────────────────────────────────────────────

/// The backdrop's live render-target handle plus the per-path rebuild
/// tracking [`sync_backdrop_fields`] needs (mirrors [`super::scene::
/// FsnFields`]'s `built_generation` idiom, but keyed by path directly since
/// the backdrop doesn't need each field's own cell bboxes cached across
/// frames — it recomputes the root field fresh every run, cheap at this
/// field count).
#[derive(Resource, Default)]
pub struct FsnBackdrop {
    /// The render-target texture the window quads sample — `None` while
    /// `Screen::Room` isn't live (cleared on [`despawn_backdrop`]).
    pub image: Option<Handle<Image>>,
    /// Path -> the listing generation its seed-point mesh was last built
    /// from.
    built: HashMap<String, u64>,
    /// Path -> that path's own spawned seed-point entity, so a generation
    /// bump can despawn the stale mesh before spawning its replacement.
    entities: HashMap<String, Entity>,
}

// ── Spawn / despawn ──────────────────────────────────────────────────────

/// Build the render-target image, the off-screen camera pointed at it, and
/// the backdrop scene's cold-start content (seam grid + ship silhouette —
/// drawn UNCONDITIONALLY, so the windows show *something* the instant the
/// room loads, before any VFS listing has ever arrived). Kicks off the root
/// listing fetch too if [`FsnState`] is empty (a fresh app that never dove
/// into `Screen::Fsn` yet) — [`sync_backdrop_fields`] then populates the
/// seed-point clusters once that reply lands, all without the player ever
/// diving through the N door.
pub fn spawn_backdrop(
    mut commands: Commands,
    mut backdrop: ResMut<FsnBackdrop>,
    mut state: ResMut<FsnState>,
    palette: Res<ScenePalette>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    existing: Query<Entity, With<FsnBackdropRoot>>,
) {
    if !existing.is_empty() {
        return;
    }

    let image = Image::new_target_texture(
        BACKDROP_TEX,
        BACKDROP_TEX,
        TextureFormat::Rgba8Unorm,
        Some(TextureFormat::Rgba8UnormSrgb),
    );
    let image_handle = images.add(image);
    backdrop.image = Some(image_handle.clone());
    backdrop.built.clear();
    backdrop.entities.clear();

    // The off-screen camera: NO Hdr, NO Bloom — `scene_palette::
    // apply_scene_post_on_change` queries `(&mut Bloom, &mut Tonemapping)`,
    // so a camera missing the `Bloom` component is simply never matched;
    // this camera's tonemapping (explicitly `None` — the render target is
    // an LDR `Rgba8Unorm` surface, not a display needing a display
    // transform) is never touched by that system.
    commands.spawn((
        Camera3d::default(),
        Camera {
            order: -1,
            clear_color: ClearColorConfig::Custom(Color::LinearRgba(palette.bg)),
            ..default()
        },
        RenderTarget::Image(image_handle.clone().into()),
        Tonemapping::None,
        RenderLayers::layer(FSN_BACKDROP_LAYER),
        FsnBackdropCamera,
        Transform::default(),
        Name::new("FsnBackdropCamera"),
    ));

    let root = commands
        .spawn((
            FsnBackdropRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("FsnBackdropRoot"),
        ))
        .id();

    let seam_mesh = meshes.add(super::scene::line_list_mesh(&layout::flatten_segments(
        &layout::seam_grid(layout::root_world_rect()),
    )));
    let seam_mat = mats.add(unlit(lin_scaled(palette.fsn_seam, 1.0)));
    commands.spawn((
        Mesh3d(seam_mesh),
        MeshMaterial3d(seam_mat),
        Transform::default(),
        Visibility::Inherited,
        RenderLayers::layer(FSN_BACKDROP_LAYER),
        Name::new("FsnBackdropSeam"),
        ChildOf(root),
    ));

    let ship_mesh = meshes.add(super::scene::line_list_mesh(&layout::flatten_segments(
        &layout::ship_silhouette_segments(),
    )));
    let ship_mat = mats.add(unlit(lin_scaled(palette.gold, palette.trim)));
    commands.spawn((
        Mesh3d(ship_mesh),
        MeshMaterial3d(ship_mat),
        Transform::default(),
        Visibility::Inherited,
        RenderLayers::layer(FSN_BACKDROP_LAYER),
        Name::new("FsnBackdropShip"),
        ChildOf(root),
    ));

    spawn_window_quads(&mut commands, root, &mut meshes, &mut mats, &image_handle);

    if state.listings.is_empty() {
        state.request("/".into());
    }

    info!("fsn: backdrop spawned (N windows — the world glimpsed from the room)");
}

/// The two window quads flanking the N marker — main-camera-visible (no
/// `RenderLayers`, the deliberate exception the module doc calls out),
/// sampling the backdrop's own render target. Transform math mirrors
/// `room::mod::spawn_walls`' own N-panel placement (`WALL_APOTHEM` out,
/// `looking_at` the doubled position so local +Z faces the console) since
/// that fn's own consts (`WALL_HEIGHT`, `WALL_PANEL_GAP`) are private to
/// `room::mod` — this reconstructs just the placement, not the panel mesh
/// itself (the real wall panel is still `room::mod`'s to spawn; these quads
/// sit proud of it).
fn spawn_window_quads(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    image: &Handle<Image>,
) {
    let wall_apothem = crate::view::palette::WALL_APOTHEM;
    let panels = bearing::octagon_panels(wall_apothem);
    let north = panels
        .iter()
        .find(|p| p.bearing == Some(Bearing::North))
        .expect("octagon_panels always includes a North face");
    let panel_width = bearing::octagon_panel_width(wall_apothem);

    let pos = Vec3::new(north.center[0], WINDOW_Y, north.center[2]);
    let outward = Vec3::new(pos.x * 2.0, pos.y, pos.z * 2.0);
    let panel_tf = Transform::from_translation(pos).looking_at(outward, Vec3::Y);

    let window_mesh = meshes.add(Rectangle::new(WINDOW_W, WINDOW_H));
    for (label, side) in [("L", -1.0_f32), ("R", 1.0_f32)] {
        let local = Vec3::new(side * panel_width * WINDOW_OFFSET_FRAC, 0.0, WINDOW_PROUD);
        let mat = mats.add(StandardMaterial {
            base_color_texture: Some(image.clone()),
            base_color: Color::LinearRgba(LinearRgba::rgb(WINDOW_LIFT, WINDOW_LIFT, WINDOW_LIFT)),
            unlit: true,
            ..default()
        });
        commands.spawn((
            Mesh3d(window_mesh.clone()),
            MeshMaterial3d(mat),
            Transform::from_translation(panel_tf.transform_point(local))
                .with_rotation(panel_tf.rotation),
            Visibility::Inherited,
            Name::new(format!("FsnBackdropWindow{label}")),
            ChildOf(root),
        ));
    }
}

/// Despawn the backdrop camera + scene root (recursive — takes the window
/// quads with it too, despite their differing render layer; they're still
/// `ChildOf` the same root) and drop the resource's live handles, so a later
/// `OnEnter(Screen::Room)` rebuilds cleanly rather than finding stale state.
pub fn despawn_backdrop(
    mut commands: Commands,
    roots: Query<Entity, With<FsnBackdropRoot>>,
    cameras: Query<Entity, With<FsnBackdropCamera>>,
    mut backdrop: ResMut<FsnBackdrop>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    for e in cameras.iter() {
        commands.entity(e).despawn();
    }
    backdrop.image = None;
    backdrop.built.clear();
    backdrop.entities.clear();
}

// ── Ambient sync (Screen::Room only) ────────────────────────────────────

/// Slow circular drift for the backdrop's own camera — [`layout::
/// orbit_pose`]'s pure math turned into a `Transform`.
pub fn orbit_backdrop_camera(
    time: Res<Time>,
    mut camera: Query<&mut Transform, With<FsnBackdropCamera>>,
) {
    let Ok(mut tf) = camera.single_mut() else {
        return;
    };
    let (pos, look_at) = layout::orbit_pose(time.elapsed_secs());
    *tf = Transform::from_translation(Vec3::from_array(pos))
        .looking_at(Vec3::from_array(look_at), Vec3::Y);
}

/// The rebuild plan for one [`sync_backdrop_fields`] pass: which of the
/// backdrop's candidate paths — root plus up to [`BACKDROP_FIELD_CAP`]
/// depth-1 children, name-sorted (the same candidate selection the rebuild
/// loop uses) — are stale against `built` (missing, or built at a different
/// generation). Pure over the two maps, so the "does this frame owe any
/// work at all?" decision is unit-testable without a Bevy world.
///
/// This is the frame-rate guard: [`sync_backdrop_fields`] runs every Update
/// while `Screen::Room` is live (the app's resting screen), and the
/// relaxed-Voronoi `layout_field` math it feeds is far too expensive to
/// burn per-frame on an unchanged cache. An empty plan means the caller
/// returns before ANY layout math — `rebuild_one`'s own per-path generation
/// guard still stands behind this, but only as defense in depth; this plan
/// is what keeps the resting frame free.
///
/// No root listing yet means no work at all (children can't place without
/// the root field), matching the rebuild loop's own early return.
///
/// One accepted corner: a stale child the rebuild loop then SKIPS (its name
/// missing from the root field's cells, or its cell too small to host —
/// `cell_bbox_inset` = `None`) never lands in `built`, so it stays in the
/// plan and the layout math re-runs each frame until its own (or the
/// root's) listing changes. That degrades exactly to the old always-run
/// behavior for a rare geometry corner — recording "skipped" as "built"
/// would instead wedge the child out of ever retrying after a root-layout
/// change, a worse trade.
fn paths_needing_rebuild(
    listings: &HashMap<String, super::sync::DirListing>,
    built: &HashMap<String, u64>,
) -> Vec<String> {
    let Some(root_listing) = listings.get("/") else {
        return Vec::new();
    };
    let mut stale = Vec::new();
    if built.get("/") != Some(&root_listing.generation) {
        stale.push("/".to_string());
    }
    // Candidates are capped BEFORE stale-filtering — a stale child beyond
    // the cap is not a candidate and must not trigger work (it would never
    // be rebuilt anyway; treating it as stale would re-run the root layout
    // every frame forever).
    let mut child_paths: Vec<&String> = listings
        .keys()
        .filter(|p| layout::split_parent(p).is_some_and(|(parent, _)| parent == "/"))
        .collect();
    child_paths.sort();
    child_paths.truncate(BACKDROP_FIELD_CAP);
    for path in child_paths {
        if built.get(path.as_str()) != Some(&listings[path].generation) {
            stale.push(path.clone());
        }
    }
    stale
}

/// (Re)build the backdrop's seed-point clusters from whatever [`FsnState`]
/// already has cached — root, plus up to [`BACKDROP_FIELD_CAP`] depth-1
/// children, each only once its OWN generation changes (mirrors `scene::
/// sync_fsn_fields`'s rebuild-on-generation-bump idiom, simplified: no
/// descendant-invalidation chain, since the backdrop never nests past
/// depth 1 in the first place).
///
/// Early-outs on an empty [`paths_needing_rebuild`] plan BEFORE any layout
/// math — this system runs every frame `Screen::Room` is live, and the
/// steady state (nothing changed) must cost a few HashMap lookups, not a
/// relaxed-Voronoi solve. When at least one path IS stale, the root field
/// is recomputed fresh for that pass (not cached) so a depth-1 child's own
/// placement (`layout::cell_bbox_inset` against the root field's own cell)
/// is always available even when only the child changed — once per actual
/// change, and simpler than persisting `scene::FsnFields`-style per-cell
/// bbox caches for a view that never does LOD or selection.
pub fn sync_backdrop_fields(
    state: Res<FsnState>,
    mut backdrop: ResMut<FsnBackdrop>,
    mut commands: Commands,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    roots: Query<Entity, With<FsnBackdropRoot>>,
) {
    let Ok(root) = roots.single() else { return };

    // The resting-frame guard: cheap map lookups only, no layout math,
    // no `ResMut` deref-mut (reading `backdrop.built` through `Deref`
    // keeps change detection quiet too).
    let stale = paths_needing_rebuild(&state.listings, &backdrop.built);
    if stale.is_empty() {
        return;
    }

    let Some(root_listing) = state.listings.get("/") else {
        return;
    };

    let root_rect = layout::root_world_rect();
    let root_specs: Vec<_> = root_listing
        .children
        .iter()
        .map(|c| layout::child_spec(&c.name, c.kind, c.size, c.child_count))
        .collect();
    let root_field = layout_field(root_rect, &root_specs);

    for path in &stale {
        if path == "/" {
            rebuild_one(
                &mut commands,
                root,
                "/",
                &root_field,
                root_listing.generation,
                &palette,
                &mut meshes,
                &mut mats,
                &mut backdrop,
            );
            continue;
        }

        let listing = &state.listings[path];
        let Some((_, name)) = layout::split_parent(path) else {
            continue;
        };
        let Some(cell) = root_field.cells.iter().find(|c| c.name == name) else {
            continue;
        };
        let Some(bbox) = layout::cell_bbox_inset(&cell.polygon, layout::SUBFIELD_INSET_FRAC) else {
            continue;
        };
        let specs: Vec<_> = listing
            .children
            .iter()
            .map(|c| layout::child_spec(&c.name, c.kind, c.size, c.child_count))
            .collect();
        let field = layout_field(bbox, &specs);

        rebuild_one(
            &mut commands,
            root,
            path,
            &field,
            listing.generation,
            &palette,
            &mut meshes,
            &mut mats,
            &mut backdrop,
        );
    }
}

/// Rebuild one path's own seed-point mesh iff its listing generation moved
/// on from what's tracked in [`FsnBackdrop::built`] — despawning the stale
/// entity first so a rebuild never leaks a duplicate mesh.
#[allow(clippy::too_many_arguments)]
fn rebuild_one(
    commands: &mut Commands,
    root: Entity,
    path: &str,
    field: &FsnField,
    generation: u64,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    backdrop: &mut FsnBackdrop,
) {
    if backdrop.built.get(path).is_some_and(|&g| g == generation) {
        return;
    }
    if let Some(old) = backdrop.entities.remove(path) {
        commands.entity(old).despawn();
    }

    let positions = layout::field_seed_points(field);
    let (verts, indices) = layout::point_marker_mesh_data(&positions, BACKDROP_POINT_SIZE);
    let mesh = meshes.add(super::scene::triangle_list_mesh(verts, indices));
    let mat = mats.add(unlit(lin_scaled(palette.fsn_vertex, 0.55)));
    let entity = commands
        .spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::default(),
            Visibility::Inherited,
            RenderLayers::layer(FSN_BACKDROP_LAYER),
            Name::new(format!("FsnBackdropSeeds({path})")),
            ChildOf(root),
        ))
        .id();
    backdrop.entities.insert(path.to_string(), entity);
    backdrop.built.insert(path.to_string(), generation);
}

fn unlit(color: Color) -> StandardMaterial {
    StandardMaterial {
        base_color: color,
        unlit: true,
        ..default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::fsn::sync::DirListing;

    fn listing(generation: u64) -> DirListing {
        DirListing { generation, truncated_here: false, children: vec![] }
    }

    fn built(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(p, g)| (p.to_string(), *g)).collect()
    }

    // ── paths_needing_rebuild (the resting-frame guard) ──

    #[test]
    fn no_root_listing_means_no_work_at_all() {
        let mut listings = HashMap::new();
        // A depth-1 listing without the root itself: children can't place
        // without the root field, so the plan must stay empty.
        listings.insert("/a".to_string(), listing(1));
        assert!(paths_needing_rebuild(&listings, &HashMap::new()).is_empty());
    }

    #[test]
    fn everything_built_at_current_generation_is_an_empty_plan() {
        // THE steady state — every Update frame while the room rests. An
        // empty plan is what lets sync_backdrop_fields return before any
        // Voronoi math.
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(3));
        listings.insert("/a".to_string(), listing(1));
        listings.insert("/b".to_string(), listing(7));
        let built = built(&[("/", 3), ("/a", 1), ("/b", 7)]);
        assert!(paths_needing_rebuild(&listings, &built).is_empty());
    }

    #[test]
    fn an_unbuilt_root_is_the_whole_plan() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        assert_eq!(paths_needing_rebuild(&listings, &HashMap::new()), vec!["/".to_string()]);
    }

    #[test]
    fn a_root_generation_bump_marks_root_only() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(2));
        listings.insert("/a".to_string(), listing(1));
        let built = built(&[("/", 1), ("/a", 1)]);
        assert_eq!(paths_needing_rebuild(&listings, &built), vec!["/".to_string()]);
    }

    #[test]
    fn a_child_generation_bump_marks_that_child_only() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        listings.insert("/a".to_string(), listing(1));
        listings.insert("/b".to_string(), listing(5));
        let built = built(&[("/", 1), ("/a", 1), ("/b", 4)]);
        assert_eq!(paths_needing_rebuild(&listings, &built), vec!["/b".to_string()]);
    }

    #[test]
    fn deeper_paths_are_never_candidates() {
        // Only root + depth-1 children are backdrop candidates; a cached
        // depth-2 listing (from the world's own deeper dives) must not
        // drag the plan non-empty.
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        listings.insert("/a".to_string(), listing(1));
        listings.insert("/a/b".to_string(), listing(9));
        let built = built(&[("/", 1), ("/a", 1)]);
        assert!(paths_needing_rebuild(&listings, &built).is_empty());
    }

    #[test]
    fn a_stale_child_beyond_the_cap_is_not_a_candidate() {
        // Candidates are name-sorted then truncated to BACKDROP_FIELD_CAP
        // BEFORE stale-filtering — a stale child past the cap would never
        // be rebuilt, so treating it as stale would re-run the root layout
        // every frame forever.
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        let mut built_pairs: Vec<(String, u64)> = vec![("/".to_string(), 1)];
        // BACKDROP_FIELD_CAP children within the cap, all built current...
        for i in 0..BACKDROP_FIELD_CAP {
            let path = format!("/d{i:02}");
            listings.insert(path.clone(), listing(1));
            built_pairs.push((path, 1));
        }
        // ...plus one sorted-last child ("/z…") that is stale (never built).
        listings.insert("/z-overflow".to_string(), listing(1));
        let built: HashMap<String, u64> = built_pairs.into_iter().collect();
        assert!(
            paths_needing_rebuild(&listings, &built).is_empty(),
            "a stale child beyond the cap must not owe work"
        );
    }

    #[test]
    fn a_stale_child_within_the_cap_is_planned() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        let mut built_pairs: Vec<(String, u64)> = vec![("/".to_string(), 1)];
        for i in 0..BACKDROP_FIELD_CAP {
            let path = format!("/d{i:02}");
            listings.insert(path.clone(), listing(1));
            built_pairs.push((path, 1));
        }
        // Bump one child WITHIN the sorted cap window.
        listings.insert("/d03".to_string(), listing(2));
        let built: HashMap<String, u64> = built_pairs.into_iter().collect();
        assert_eq!(paths_needing_rebuild(&listings, &built), vec!["/d03".to_string()]);
    }
}
