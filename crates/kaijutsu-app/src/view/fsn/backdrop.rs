//! The room's N-window backdrop (lane A4): while `Screen::Room` is live, an
//! off-screen camera renders an impression of the FSN world to a texture,
//! shown through one panel-spanning portal on the N bearing — "you can see
//! it out there before you dive" (`docs/scenes/vfs.md`). Since 2026-07-13
//! this portal is the PRIMARY FSN surface (diving is de-emphasized — see
//! `super::mod`'s own doc), so it now carries the SAME ambient law
//! [`super::scene`] bakes for the dived world: per-cell recency tint (baked
//! at mesh build time, [`super::scene::cell_edge_tints`]) and per-field
//! churn heat (a live gain/hue lift, [`sync_backdrop_heat`]). What it does
//! NOT carry, still: no LOD swap (one fixed tier — wireframe districts,
//! never sparse points or the attended-tier vertex markers), no selection,
//! no vessel silhouette (since 2026-07-13 the room IS the vessel this
//! camera flies) — the backdrop is a glimpse, not the full instrument.
//! Rendered on its own camera/render-layer so it never contends with the
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
//! sees the room itself. The portal quad is the one deliberate exception:
//! it carries no `RenderLayers` (main-camera-visible, showing the rendered
//! texture), while everything else under [`FsnBackdropRoot`] is
//! layer-`FSN_BACKDROP_LAYER`-only.
//!
//! # Content: reused, not duplicated
//!
//! Every mesh/law here is [`super::layout`]/[`super::scene`]'s own pure
//! functions, not a second copy: the seam grid via
//! [`super::scene::line_list_mesh`] (`pub(super)`, plain uncolored — the
//! seam stays cosmetic, no recency bake); the wireframe districts via
//! [`super::layout::field_wireframe`] + [`super::scene::cell_edge_tints`]
//! (the SAME recency-tint law [`super::scene::spawn_field_entities`] bakes,
//! against the SAME [`super::sync::ChildMeta`] listings) +
//! [`super::scene::line_list_mesh_colored`]; the resting gain
//! [`super::scene::WIREFRAME_GAIN_MID`]; and the heat hue-lift's lerp,
//! [`super::scene::lerp_hue`]. One law, two call sites — never a forked
//! copy that could drift out of sync with the dived world's own version.

use std::collections::HashMap;

use bevy::camera::RenderTarget;
use bevy::camera::visibility::RenderLayers;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::prelude::*;
use bevy::render::render_resource::TextureFormat;
use kaijutsu_viz::fsn::{FsnField, layout_field};

use crate::view::room::bearing::{self, Bearing};
use crate::view::scene_palette::{ScenePalette, lin_scaled};

use super::heat::{FsnHeat, HEAT_GAIN_LIFT};
use super::layout;
use super::sync::{ChildMeta, FsnState};

// ── Amy-tunable render constants ────────────────────────────────────────────

/// Render-target texture width (pixels). Height derives from the portal's
/// own aspect ([`WINDOW_W`]/[`WINDOW_H`]) so the camera's frame and the
/// glass agree — a square texture on a non-square quad squeezed the world
/// (and showing it twice on two quads made it read as two copies of a
/// miniature, not one world out there). 2048 since the glass gained
/// fullscreen duty (`room/shot.rs::portal_fullscreen_pose` fills the whole
/// frame with it — 1024 read soft stretched across a real monitor).
/// **Amy-tunable.**
pub const BACKDROP_TEX_W: u32 = 2048;

/// Render-target texture height — [`BACKDROP_TEX_W`] scaled by the portal
/// quad's aspect, so Bevy's projection (which takes its aspect from the
/// render target) matches the glass exactly.
pub const BACKDROP_TEX_H: u32 = (BACKDROP_TEX_W as f32 * (WINDOW_H / WINDOW_W)) as u32;

/// The `RenderLayers` index every backdrop-only entity (and the backdrop
/// camera itself) carries — keeps the backdrop scene invisible to the main
/// camera (layer 0) and vice versa. Layer 1 is left free (unused elsewhere
/// today) in case a future RTT lane wants it.
const FSN_BACKDROP_LAYER: usize = 2;

/// Portal quad size (world units) — ONE panel-spanning window centered on
/// the N face, not the original pair of flanking portholes (both sampled
/// the same texture, so the room saw two copies of the same miniature; Amy
/// called for one expansive portal, 2026-07-13). Sized against the N
/// panel's visible face: full octagon panel width at `WALL_APOTHEM` 1200 is
/// ≈994, minus `room::mod`'s `WALL_PANEL_GAP` reveal (40) ≈954 wide by
/// `WALL_HEIGHT` 560 tall — 880×470 leaves a ~37-unit mullion each side and
/// 45 above/below at [`WINDOW_Y`]. **Amy-tunable.**
pub const WINDOW_W: f32 = 880.0;
pub const WINDOW_H: f32 = 470.0;

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
/// texels (the seam grid, hot districts) cross the bloom threshold and the
/// window reads as lit glass rather than a flat picture. **Amy-tunable.**
pub const WINDOW_LIFT: f32 = 1.15;

/// Hard cap on how many directory fields the backdrop bothers building
/// (root + this many depth-1 children) — the backdrop is a glimpse, not the
/// real world; capping keeps a huge root directory from spawning dozens of
/// wireframe-district meshes for a texture nobody can read detail on
/// anyway. **Amy-tunable.**
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
/// grid, per-field wireframe districts — NOT the portal quad, which
/// is a structural child of this same entity but renders on the main
/// camera's own layer 0; see the module doc's render-layer split). Kept as
/// its OWN root — never `ChildOf(RoomRoot)` — because `RoomRoot`'s own
/// spawn commands are deferred in the same schedule this spawns in
/// (`OnEnter(Screen::Room)`); parenting under an entity ID that may not
/// have landed yet would be a same-frame race.
#[derive(Component)]
pub struct FsnBackdropRoot;

// ── Resources ────────────────────────────────────────────────────────────

/// One built field's spawned wireframe entity plus its own material handle —
/// the handle is what [`sync_backdrop_heat`] needs to write `base_color`
/// every frame without re-querying `MeshMaterial3d` (mirrors [`super::scene::
/// FieldEntities::wireframe_material`]'s identical reason).
struct BackdropField {
    entity: Entity,
    material: Handle<StandardMaterial>,
}

/// The backdrop's live render-target handle plus the per-path rebuild
/// tracking [`sync_backdrop_fields`] needs (mirrors [`super::scene::
/// FsnFields`]'s `built_generation` idiom, but keyed by path directly since
/// the backdrop doesn't need each field's own cell bboxes cached across
/// frames — it recomputes the root field fresh every run, cheap at this
/// field count).
#[derive(Resource, Default)]
pub struct FsnBackdrop {
    /// The render-target texture the portal quad samples — `None` while
    /// `Screen::Room` isn't live (cleared on [`despawn_backdrop`]).
    pub image: Option<Handle<Image>>,
    /// Path -> the listing generation its wireframe district mesh was last
    /// built from.
    built: HashMap<String, u64>,
    /// Path -> that path's own spawned wireframe entity + material handle,
    /// so a generation bump can despawn the stale mesh before spawning its
    /// replacement, and so [`sync_backdrop_heat`] can iterate every built
    /// field's material each frame.
    entities: HashMap<String, BackdropField>,
}

// ── Spawn / despawn ──────────────────────────────────────────────────────

/// Build the render-target image, the off-screen camera pointed at it, and
/// the backdrop scene's cold-start content (the seam grid — drawn
/// UNCONDITIONALLY, so the portal shows *something* the instant the
/// room loads, before any VFS listing has ever arrived). Kicks off the root
/// listing fetch too if [`FsnState`] is empty (a fresh app that never dove
/// into `Screen::Fsn` yet) — [`sync_backdrop_fields`] then populates the
/// wireframe districts once that reply lands, all without the player ever
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
        BACKDROP_TEX_W,
        BACKDROP_TEX_H,
        TextureFormat::Rgba8Unorm,
        Some(TextureFormat::Rgba8UnormSrgb),
    );
    let image_handle = images.add(image);
    backdrop.image = Some(image_handle.clone());
    backdrop.built.clear();
    backdrop.entities.clear();

    // The off-screen camera: NO Hdr, NO Bloom — `scene_palette::
    // apply_scene_post_on_change` queries `(&mut Bloom, &mut Tonemapping)`,
    // so a camera missing the `Bloom` component is never matched, AND that
    // system now carries an explicit `Without<FsnBackdropCamera>` filter as
    // defense-in-depth (its own comment); this camera's tonemapping
    // (explicitly `None` — the render target is an LDR `Rgba8Unorm`
    // surface, not a display needing a display transform) stays untouched
    // either way.
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

    // No vessel silhouette in the backdrop (removed 2026-07-13): the room
    // IS the vessel this camera flies — you can't see yourself out your own
    // window. The dived world keeps the silhouette, riding this same orbit
    // (`scene::orbit_ship`).

    spawn_portal_quad(&mut commands, root, &mut meshes, &mut mats, &image_handle);

    if state.listings.is_empty() {
        state.request("/".into());
    }

    info!("fsn: backdrop spawned (N portal — the world glimpsed from the room)");
}

/// The panel-spanning portal quad centered on the N face — main-camera-
/// visible (no `RenderLayers`, the deliberate exception the module doc
/// calls out), sampling the backdrop's own render target. Transform math
/// mirrors `room::mod::spawn_walls`' own N-panel placement (`WALL_APOTHEM`
/// out, `looking_at` the doubled position so local +Z faces the console)
/// since that fn's own consts (`WALL_HEIGHT`, `WALL_PANEL_GAP`) are private
/// to `room::mod` — this reconstructs just the placement, not the panel
/// mesh itself (the real wall panel is still `room::mod`'s to spawn; the
/// quad sits proud of it, covering the panel's wall threads behind the
/// glass).
fn spawn_portal_quad(
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

    let pos = Vec3::new(north.center[0], WINDOW_Y, north.center[2]);
    let outward = Vec3::new(pos.x * 2.0, pos.y, pos.z * 2.0);
    let panel_tf = Transform::from_translation(pos).looking_at(outward, Vec3::Y);

    let window_mesh = meshes.add(Rectangle::new(WINDOW_W, WINDOW_H));
    let local = Vec3::new(0.0, 0.0, WINDOW_PROUD);
    let mat = mats.add(StandardMaterial {
        base_color_texture: Some(image.clone()),
        base_color: Color::LinearRgba(LinearRgba::rgb(WINDOW_LIFT, WINDOW_LIFT, WINDOW_LIFT)),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(window_mesh),
        MeshMaterial3d(mat),
        Transform::from_translation(panel_tf.transform_point(local))
            .with_rotation(panel_tf.rotation),
        Visibility::Inherited,
        Name::new("FsnBackdropWindow"),
        ChildOf(root),
    ));
}

/// Despawn the backdrop camera + scene root (recursive — takes the portal
/// quad with it too, despite its differing render layer; it's still
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

/// One [`sync_backdrop_fields`] pass's work order, decided by
/// [`plan_backdrop_sync`] — pure data so both halves of the decision are
/// unit-testable without a Bevy world.
#[derive(Debug, Default, PartialEq)]
struct BackdropSyncPlan {
    /// Paths whose wireframe district mesh must be (re)built this pass,
    /// root first, children in name-sorted order.
    rebuild: Vec<String>,
    /// Previously-built paths that are no longer candidates at all (fell
    /// out of the capped window — see [`plan_backdrop_sync`]'s cap-crossing
    /// note) whose entity must be despawned and forgotten, name-sorted.
    remove: Vec<String>,
}

impl BackdropSyncPlan {
    fn is_empty(&self) -> bool {
        self.rebuild.is_empty() && self.remove.is_empty()
    }
}

/// The plan for one [`sync_backdrop_fields`] pass, pure over the two maps.
/// Candidates are root plus up to [`BACKDROP_FIELD_CAP`] depth-1 children,
/// name-sorted (the same candidate selection the rebuild loop uses).
///
/// This is the frame-rate guard: [`sync_backdrop_fields`] runs every Update
/// while `Screen::Room` is live (the app's resting screen), and the
/// relaxed-Voronoi `layout_field` math it feeds is far too expensive to
/// burn per-frame on an unchanged cache. An empty plan means the caller
/// returns before ANY layout math.
///
/// **`rebuild`**: a candidate is stale when `built` doesn't hold its current
/// listing generation — AND, when the ROOT itself is stale, every candidate
/// child is stale with it: a child's world rect is its own cell's bbox in
/// the root field, so a root-layout change moves every child's ground out
/// from under it (`scene::sync_fsn_fields` handles the same case by
/// dropping every descendant on a parent rebuild — this is the backdrop's
/// one-level equivalent).
///
/// **`remove`**: the candidate window is name-sorted and capped, so a NEW
/// alphabetically-earlier listing can push a previously-built path out of
/// the window — nothing would ever revisit it, and its entity would stay
/// spawned at stale coordinates forever. Any `built` key that is no longer
/// a candidate lands here for the caller to despawn and forget.
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
fn plan_backdrop_sync(
    listings: &HashMap<String, super::sync::DirListing>,
    built: &HashMap<String, u64>,
) -> BackdropSyncPlan {
    let Some(root_listing) = listings.get("/") else {
        return BackdropSyncPlan::default();
    };
    let root_stale = built.get("/") != Some(&root_listing.generation);
    let mut rebuild = Vec::new();
    if root_stale {
        rebuild.push("/".to_string());
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
    for path in &child_paths {
        // A root rebuild cascades: even a generation-current child sits on
        // a cell bbox the new root layout just moved.
        if root_stale || built.get(path.as_str()) != Some(&listings[*path].generation) {
            rebuild.push((*path).clone());
        }
    }

    let mut remove: Vec<String> = built
        .keys()
        .filter(|k| k.as_str() != "/" && !child_paths.iter().any(|p| p.as_str() == k.as_str()))
        .cloned()
        .collect();
    remove.sort();

    BackdropSyncPlan { rebuild, remove }
}

/// (Re)build the backdrop's wireframe districts from whatever [`FsnState`]
/// already has cached — root, plus up to [`BACKDROP_FIELD_CAP`] depth-1
/// children, each once its OWN generation changes and all together when the
/// ROOT's does (the one-level descendant-invalidation `scene::
/// sync_fsn_fields` also performs: a root rebuild moves every child's cell
/// bbox, so the children must re-place against the fresh layout).
///
/// Early-outs on an empty [`plan_backdrop_sync`] plan BEFORE any layout
/// math — this system runs every frame `Screen::Room` is live, and the
/// steady state (nothing changed) must cost a few HashMap lookups, not a
/// relaxed-Voronoi solve. When the plan is non-empty, the root field is
/// recomputed fresh for that pass (not cached) so a depth-1 child's own
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
    let plan = plan_backdrop_sync(&state.listings, &backdrop.built);
    if plan.is_empty() {
        return;
    }

    // Sweep first: a previously-built path that fell out of the capped
    // candidate window (see the planner's cap-crossing note) is despawned
    // and forgotten — nothing below will ever visit it again.
    for path in &plan.remove {
        if let Some(old) = backdrop.entities.remove(path) {
            commands.entity(old.entity).despawn();
        }
        backdrop.built.remove(path);
    }

    if plan.rebuild.is_empty() {
        return;
    }

    let Some(root_listing) = state.listings.get("/") else {
        return;
    };

    // Captured ONCE per pass, same reasoning as `scene::sync_fsn_fields`'s
    // own `now_secs` (that fn's doc): a wall-clock read is cheap but every
    // field rebuilding this pass shares the same "now" rather than each
    // computing its own.
    let now_secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    let root_rect = layout::root_world_rect();
    let root_specs: Vec<_> = root_listing
        .children
        .iter()
        .map(|c| layout::child_spec(&c.name, c.kind, c.size, c.child_count))
        .collect();
    let root_field = layout_field(root_rect, &root_specs);

    for path in &plan.rebuild {
        if path == "/" {
            rebuild_one(
                &mut commands,
                root,
                "/",
                &root_field,
                root_listing.generation,
                &root_listing.children,
                now_secs,
                &palette,
                &mut meshes,
                &mut mats,
                &mut backdrop,
            );
            continue;
        }

        // A planned child may be a root-rebuild CASCADE whose own listing
        // generation is unchanged — drop its `built` entry so
        // `rebuild_one`'s generation guard (correct for the ordinary
        // one-path case, blind to "the ground moved") can't skip it.
        backdrop.built.remove(path.as_str());

        let listing = &state.listings[path];
        let placed = layout::split_parent(path)
            .and_then(|(_, name)| root_field.cells.iter().find(|c| c.name == name))
            .and_then(|cell| layout::cell_bbox_inset(&cell.polygon, layout::SUBFIELD_INSET_FRAC));
        let Some(bbox) = placed else {
            // The (possibly fresh) root layout has no home for this child —
            // its name is missing from the root cells, or its cell is too
            // small to host. Any existing entity sits at coordinates from a
            // layout that no longer exists; drop it rather than leave a
            // ghost floating over someone else's cell.
            if let Some(old) = backdrop.entities.remove(path) {
                commands.entity(old.entity).despawn();
            }
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
            &listing.children,
            now_secs,
            &palette,
            &mut meshes,
            &mut mats,
            &mut backdrop,
        );
    }
}

/// Rebuild one path's own wireframe district mesh iff its listing
/// generation moved on from what's tracked in [`FsnBackdrop::built`] —
/// despawning the stale entity first so a rebuild never leaks a duplicate
/// mesh. Bakes the SAME per-cell recency tint [`super::scene::
/// spawn_field_entities`] does, via [`super::scene::cell_edge_tints`]
/// against `children` (that path's own listing — root's or a depth-1
/// child's, `sync_backdrop_fields`' own call sites); the material starts at
/// [`super::scene::WIREFRAME_GAIN_MID`]'s resting gain (no heat yet —
/// [`sync_backdrop_heat`] lifts it on a later frame once [`FsnHeat`] has
/// something to report), and its own handle is retained in
/// [`FsnBackdrop::entities`] so that system can find it without a
/// `MeshMaterial3d` query.
#[allow(clippy::too_many_arguments)]
fn rebuild_one(
    commands: &mut Commands,
    root: Entity,
    path: &str,
    field: &FsnField,
    generation: u64,
    children: &[ChildMeta],
    now_secs: u64,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    backdrop: &mut FsnBackdrop,
) {
    if backdrop.built.get(path).is_some_and(|&g| g == generation) {
        return;
    }
    if let Some(old) = backdrop.entities.remove(path) {
        commands.entity(old.entity).despawn();
    }

    let edge_tints = super::scene::cell_edge_tints(field, children, now_secs, palette.fsn_edge, palette);
    let wireframe_positions = layout::flatten_segments(&layout::field_wireframe(field));
    let wire_vert_counts: Vec<usize> = field.cells.iter().map(|c| 6 * c.polygon.len()).collect();
    let wireframe_colors = layout::per_cell_colors(&wire_vert_counts, &edge_tints);
    let mesh = meshes.add(super::scene::line_list_mesh_colored(&wireframe_positions, &wireframe_colors));
    let material = mats.add(unlit(lin_scaled(palette.fsn_edge, super::scene::WIREFRAME_GAIN_MID)));
    let entity = commands
        .spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material.clone()),
            Transform::default(),
            Visibility::Inherited,
            RenderLayers::layer(FSN_BACKDROP_LAYER),
            Name::new(format!("FsnBackdropWireframe({path})")),
            ChildOf(root),
        ))
        .id();
    backdrop.entities.insert(path.to_string(), BackdropField { entity, material });
    backdrop.built.insert(path.to_string(), generation);
}

/// Lift every built field's wireframe material from [`super::scene::
/// WIREFRAME_GAIN_MID`]'s resting gain toward `palette.gold`'s hue with
/// that path's own churn heat ([`FsnHeat::normalized`]) — the SAME
/// gain/hue law [`super::scene::apply_fsn_lod`] applies to the dived
/// world's wireframe tier (`gain = base * (1.0 + h * HEAT_GAIN_LIFT)`, hue
/// lerped `fsn_edge -> gold` by `h`), applied here to every backdrop field
/// every frame rather than gated by LOD tier (the backdrop has no tiers to
/// gate on). Write-on-change, checked through `Assets::get` BEFORE ever
/// touching `get_mut` — `get_mut` marks the asset Modified (a GPU
/// re-extract) even when the caller then writes nothing, and this system
/// runs every frame on the app's RESTING screen; a settled material must
/// stay settled (the same guarded-write reasoning `room::mod`'s
/// `room_focus_visuals` documents for its plates).
pub fn sync_backdrop_heat(
    heat: Res<FsnHeat>,
    palette: Res<ScenePalette>,
    backdrop: Res<FsnBackdrop>,
    mut mats: ResMut<Assets<StandardMaterial>>,
) {
    for (path, bf) in backdrop.entities.iter() {
        let h = heat.normalized(path);
        let gain = super::scene::WIREFRAME_GAIN_MID * (1.0 + h * HEAT_GAIN_LIFT);
        let hue = super::scene::lerp_hue(palette.fsn_edge, palette.gold, h);
        let want = lin_scaled(hue, gain);
        if mats.get(&bf.material).is_some_and(|m| m.base_color != want) {
            if let Some(mat) = mats.get_mut(&bf.material) {
                mat.base_color = want;
            }
        }
    }
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

    fn plan(
        listings: &HashMap<String, DirListing>,
        built_map: &HashMap<String, u64>,
    ) -> BackdropSyncPlan {
        plan_backdrop_sync(listings, built_map)
    }

    // ── plan_backdrop_sync (the resting-frame guard) ──

    #[test]
    fn no_root_listing_means_no_work_at_all() {
        let mut listings = HashMap::new();
        // A depth-1 listing without the root itself: children can't place
        // without the root field, so the plan must stay empty.
        listings.insert("/a".to_string(), listing(1));
        assert!(plan(&listings, &HashMap::new()).is_empty());
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
        assert!(plan(&listings, &built).is_empty());
    }

    #[test]
    fn an_unbuilt_root_is_the_whole_plan() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        let p = plan(&listings, &HashMap::new());
        assert_eq!(p.rebuild, vec!["/".to_string()]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn a_root_generation_bump_cascades_to_every_built_child() {
        // The root-rebuild cascade: a child's world rect is its own cell's
        // bbox in the ROOT field, so a new root layout moves every child's
        // ground out from under it — generation-current children must
        // rebuild too (scene::sync_fsn_fields drops descendants on a parent
        // rebuild for the same reason).
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(2));
        listings.insert("/a".to_string(), listing(1));
        listings.insert("/b".to_string(), listing(1));
        let built = built(&[("/", 1), ("/a", 1), ("/b", 1)]);
        let p = plan(&listings, &built);
        assert_eq!(
            p.rebuild,
            vec!["/".to_string(), "/a".to_string(), "/b".to_string()],
            "root bump must plan root AND every built child"
        );
        assert!(p.remove.is_empty());
    }

    #[test]
    fn a_child_generation_bump_marks_that_child_only() {
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        listings.insert("/a".to_string(), listing(1));
        listings.insert("/b".to_string(), listing(5));
        let built = built(&[("/", 1), ("/a", 1), ("/b", 4)]);
        let p = plan(&listings, &built);
        assert_eq!(p.rebuild, vec!["/b".to_string()], "root untouched, sibling untouched");
        assert!(p.remove.is_empty());
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
        assert!(plan(&listings, &built).is_empty());
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
            plan(&listings, &built).is_empty(),
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
        let p = plan(&listings, &built);
        assert_eq!(p.rebuild, vec!["/d03".to_string()]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn a_built_child_pushed_out_of_the_cap_window_is_removed() {
        // The cap-crossing sweep: the candidate window is name-sorted and
        // capped, so a NEW alphabetically-earlier listing pushes the
        // sorted-last built path out — without the sweep its entity would
        // stay spawned at stale coordinates forever, never revisited.
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        let mut built_pairs: Vec<(String, u64)> = vec![("/".to_string(), 1)];
        // The OLD window: d01..=dCAP (BACKDROP_FIELD_CAP children), built.
        for i in 1..=BACKDROP_FIELD_CAP {
            let path = format!("/d{i:02}");
            listings.insert(path.clone(), listing(1));
            built_pairs.push((path, 1));
        }
        // A new, alphabetically-FIRST child arrives (unbuilt): the window
        // shifts to d00..d(CAP-1); the old sorted-last child falls out.
        listings.insert("/d00".to_string(), listing(1));
        let built: HashMap<String, u64> = built_pairs.into_iter().collect();
        let p = plan(&listings, &built);
        assert_eq!(p.rebuild, vec!["/d00".to_string()], "the newcomer needs building");
        assert_eq!(
            p.remove,
            vec![format!("/d{BACKDROP_FIELD_CAP:02}")],
            "the displaced sorted-last child must be swept"
        );
    }

    #[test]
    fn the_root_is_never_swept() {
        // `remove` targets displaced CHILDREN only; the root is always a
        // candidate while its listing exists.
        let mut listings = HashMap::new();
        listings.insert("/".to_string(), listing(1));
        let built = built(&[("/", 1)]);
        assert!(plan(&listings, &built).is_empty());
    }
}
