//! Track rays — tracks as vortex-spiral ribbons down the funnel wall of the
//! well.
//!
//! A track is the durable lane (docs/tracks.md); contexts churn around it.
//! Each track renders as one thin [`TrackRayMaterial`] ribbon spiraling down
//! the funnel wall — from the vortex throat out through the magic rings to
//! the mouth — at a bearing **hashed stably from the track's name** (same FNV
//! recipe as the card accents): the bearing never moves across restarts or
//! track churn, so a lane lives at a learnable address. Every ray winds the
//! SAME rotational direction (drain-swirl), which is why this is a spiral and
//! not a straight beam: a room-scale view looking down into the mouth once
//! read several straight chords as an inscribed pentagram, and same-direction
//! spirals can never cross into a star chord the way independent straight
//! beams could (placeholder aesthetic — geometry swap only, "we'll glow up
//! later"). While the track's clock rolls, a pulse rides the ribbon
//! mouth→throat each beat, driven by the same per-track phasors the cards use
//! ([`super::live::WellBeats`]).
//!
//! Data flow mirrors the cluster poll: [`poll_tracks`] ships `listTracks`
//! every few seconds while the well is open; [`apply_tracks`] drains the
//! result into [`WellTracks`] (the roster + derived context→track maps that
//! also feed the cards' border hue / beat keying and the reading card's
//! track line); [`sync_track_rays`] reconciles ray entities;
//! [`animate_track_rays`] pushes the per-frame phasor state into the
//! material uniforms.

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::time::Instant;

use bevy::prelude::*;
use kaijutsu_client::TrackInfo;
use kaijutsu_types::ContextId;

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};
use crate::shaders::TrackRayMaterial;

/// How often to poll `listTracks` while the well is open (seconds). Tracks
/// are few and churn slowly; the *beat* rides `BeatSync` push, not this poll.
const TRACK_POLL_INTERVAL: f64 = 5.0;

/// Ray ribbon width (well units) — a filament, not a wall.
const RAY_WIDTH: f32 = 14.0;

/// Funnel-local stations every ray's spiral interpolates between: from just
/// above the throat floor (the ring deck sits at −850; the ribbon melts into
/// its glow) out to just past the mouth ring. Radii bracket the ring stack
/// (mouth ring radius 500, vortex core well inside 70).
const RAY_INNER: (f32, f32) = (70.0, -840.0); // (radius, depth) at the throat
const RAY_OUTER: (f32, f32) = (520.0, 10.0); // (radius, depth) past the mouth

/// How far (radians) the spiral ribbon winds from the mouth back to the
/// throat. `0.0` would be a straight radial beam (the retired pentagram
/// look); every ray winds this SAME amount, in this SAME direction — the sign
/// is picked once, here, and never varies per-ray — so no two rays' spirals
/// can ever cross into a straight-chord star the way independent straight
/// beams did. ~2.0 (a bit over a third of a turn) reads as a drain-swirl
/// without disguising which bearing the ray addresses. **Amy-tunable, first
/// guess — glow-up later.**
const SPIRAL_SWEEP: f32 = 2.0;

/// Segment count for the baked spiral ribbon mesh — enough to read as a
/// smooth curl rather than a faceted polygon.
const SPIRAL_SEGMENTS: usize = 28;

/// Stable angle for a track's ray: FNV-1a over the name → [0, TAU). The same
/// recipe as [`super::scene::accent_color`] so a track's bearing is as stable
/// as its hue; collisions just co-locate two rays (acceptable at lane counts,
/// and both keep their own hue). Deliberately NOT evenly-spaced-by-sort:
/// even spacing re-bearings every existing lane when a track is created,
/// and the whole point of a ray is a bearing you learn.
pub fn ray_angle(track_id: &str) -> f32 {
    let mut h: u32 = 2166136261;
    for b in track_id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h % 4096) as f32 / 4096.0 * TAU
}

/// A point on the spiral ribbon's centerline at `t` ∈ `[0, 1]` (0 = throat, 1
/// = mouth), baked at bearing **0**: the mesh's mouth end (`t = 1`) sits on
/// the +X axis — the address [`ray_angle`] names, since that's the track's
/// visible bearing — and the throat end (`t = 0`) trails around behind it by
/// [`SPIRAL_SWEEP`]. Radius and depth interpolate linearly between
/// [`RAY_INNER`]/[`RAY_OUTER`]; the angle interpolates linearly from
/// `SPIRAL_SWEEP` down to `0`, so the curve winds one direction the whole
/// way, never reversing.
fn spiral_point(t: f32) -> Vec3 {
    let (r0, z0) = RAY_INNER;
    let (r1, z1) = RAY_OUTER;
    let r = r0 + (r1 - r0) * t;
    let z = z0 + (z1 - z0) * t;
    let a = SPIRAL_SWEEP * (1.0 - t);
    Vec3::new(r * a.cos(), r * a.sin(), z)
}

/// Exact analytic tangent (d/dt) of [`spiral_point`] at `t` — unnormalized,
/// callers normalize. Used instead of a finite difference so the ribbon's
/// width direction (perpendicular to the path, lying in the cone surface) is
/// exact at every sample.
fn spiral_tangent_raw(t: f32) -> Vec3 {
    let (r0, z0) = RAY_INNER;
    let (r1, z1) = RAY_OUTER;
    let dr = r1 - r0;
    let dz = z1 - z0;
    let r = r0 + dr * t;
    let a = SPIRAL_SWEEP * (1.0 - t);
    let da_dt = -SPIRAL_SWEEP;
    Vec3::new(dr * a.cos() - r * da_dt * a.sin(), dr * a.sin() + r * da_dt * a.cos(), dz)
}

/// The funnel wall's own surface normal at `t`, oriented up-out-of-the-bowl:
/// `cross(radial_direction, angular_direction)` at the point's own angle —
/// this is a property of the CONE surface the ribbon rides (independent of
/// which curve is traced across it), not the ribbon curve's own tangent.
/// Always faces up at today's dims (`RAY_OUTER.0 > RAY_INNER.0` keeps the
/// cross product's z-component `= RAY_OUTER.0 - RAY_INNER.0` positive for
/// every angle, by construction); the flip guard mirrors the old
/// `ray_transform`'s same dead-code caution for the day the funnel's slope
/// gets re-tuned (Gemini review, 2026-07-04, carried forward).
fn spiral_normal(t: f32) -> Vec3 {
    let (r0, z0) = RAY_INNER;
    let (r1, z1) = RAY_OUTER;
    let dr = r1 - r0;
    let dz = z1 - z0;
    let a = SPIRAL_SWEEP * (1.0 - t);
    let radial = Vec3::new(dr * a.cos(), dr * a.sin(), dz);
    let angular = Vec3::new(-a.sin(), a.cos(), 0.0);
    let mut n = radial.cross(angular).normalize();
    if n.z < 0.0 {
        n = -n;
    }
    n
}

/// Build the shared spiral ribbon's raw vertex buffers, baked at bearing
/// angle 0 (see [`spiral_point`]): `(positions, normals, uvs, indices)`.
/// `uv.x` is cumulative 3D arclength, 0 at the throat end → 1 at the mouth
/// end — `track_ray.wgsl`'s contract, so the beat pulse rides at uniform
/// speed along the curve, not sample density; `uv.y` is 0..1 across the
/// ribbon's [`RAY_WIDTH`]. Every ray shares this ONE mesh; per-ray placement
/// is a pure rotation ([`ray_transform`]).
fn spiral_ribbon_vertices() -> (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>) {
    let n = SPIRAL_SEGMENTS;
    let half = RAY_WIDTH * 0.5;

    let points: Vec<Vec3> = (0..=n).map(|i| spiral_point(i as f32 / n as f32)).collect();

    // Cumulative 3D arclength (the curve leaves the throat/mouth radii via
    // depth AND angle at once), normalized to [0, 1].
    let mut cum = vec![0.0f32; n + 1];
    for i in 1..=n {
        cum[i] = cum[i - 1] + points[i].distance(points[i - 1]);
    }
    let total = cum[n].max(1e-6);

    let mut positions = Vec::with_capacity((n + 1) * 2);
    let mut normals = Vec::with_capacity((n + 1) * 2);
    let mut uvs = Vec::with_capacity((n + 1) * 2);
    for (i, &p) in points.iter().enumerate() {
        let t = i as f32 / n as f32;
        let tangent = spiral_tangent_raw(t).normalize();
        let normal = spiral_normal(t);
        // Perpendicular to the path, lying IN the cone surface (⊥ normal) —
        // the ribbon's width direction. `tangent × normal` (not the reverse)
        // is the order that keeps the [l0, l1, r0, r0, l1, r1] index winding
        // below front-facing toward `normal`, the same recipe
        // `bearing::ribbon_vertices` proved for its own flat ribbon.
        let width = tangent.cross(normal).normalize();
        let u = cum[i] / total;
        positions.push((p + width * half).to_array());
        positions.push((p - width * half).to_array());
        normals.push(normal.to_array());
        normals.push(normal.to_array());
        uvs.push([u, 0.0]);
        uvs.push([u, 1.0]);
    }

    let mut indices = Vec::with_capacity(n * 6);
    for i in 0..n {
        let a = (i * 2) as u32;
        let (l0, r0, l1, r1) = (a, a + 1, a + 2, a + 3);
        indices.extend_from_slice(&[l0, l1, r0, r0, l1, r1]);
    }
    (positions, normals, uvs, indices)
}

/// Wrap [`spiral_ribbon_vertices`] into a `Mesh` — the shared shape every
/// ray's entity points at ([`sync_track_rays`]).
fn spiral_ribbon_mesh() -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let (positions, normals, uvs, indices) = spiral_ribbon_vertices();
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

/// The world transform of a ray's ribbon at `angle`: a pure rotation about
/// the funnel's Z axis (placing the mesh's baked bearing-0 mouth end at this
/// bearing), composed with the shared funnel recline (`well_tilt_quat`). The
/// cone-surface orientation now lives in the baked mesh itself
/// ([`spiral_ribbon_vertices`]), so placement no longer needs a per-ray
/// basis/translation — one quaternion, not a `Mat3` + midpoint.
pub fn ray_transform(angle: f32) -> Transform {
    Transform::from_rotation(super::card::well_tilt_quat() * Quat::from_rotation_z(angle))
}

/// One track's beam in the scene. Carries the track's id + its beat key (the
/// score context — what [`super::live::WellBeats`] phasors are keyed by).
#[derive(Component)]
pub struct TrackRay {
    pub track_id: String,
    pub score_context: ContextId,
}

/// The polled track roster + the derived maps the rest of the well reads.
#[derive(Resource, Default)]
pub struct WellTracks {
    /// The latest `listTracks` answer (sorted by id — the wire order).
    pub tracks: Vec<TrackInfo>,
    /// context → the track's **score context**: the key under which
    /// [`super::live::WellBeats`] holds that track's phasor. Covers every
    /// attached context AND the score context itself, so any card on the
    /// lane can find its beat.
    pub beat_key_of: HashMap<ContextId, ContextId>,
    /// context → track id, for the border hue + the reading card's track
    /// line. Same coverage as `beat_key_of`.
    pub track_of: HashMap<ContextId, String>,
    /// track id → live ray entity (the rays' own little join).
    ray_entities: HashMap<String, Entity>,
    /// Shared spiral ribbon mesh, built on first use.
    ray_mesh: Option<Handle<Mesh>>,
}

impl WellTracks {
    /// Rebuild the roster + derived maps from a fresh poll result.
    pub fn set_tracks(&mut self, tracks: Vec<TrackInfo>) {
        self.beat_key_of.clear();
        self.track_of.clear();
        for t in &tracks {
            self.beat_key_of.insert(t.score_context_id, t.score_context_id);
            self.track_of.insert(t.score_context_id, t.id.clone());
            for ctx in &t.attached {
                self.beat_key_of.insert(*ctx, t.score_context_id);
                self.track_of.insert(*ctx, t.id.clone());
            }
        }
        self.tracks = tracks;
    }

    /// The track a context rides (attached or score), if any.
    pub fn track_info_of(&self, ctx: &ContextId) -> Option<&TrackInfo> {
        let id = self.track_of.get(ctx)?;
        self.tracks.iter().find(|t| &t.id == id)
    }

    /// Reset the id→entity map alone (the roster itself is untouched). Called
    /// by `scene::arm_well` (`room::enter_room`'s re-arm, so
    /// `sync_track_rays`'s re-entry count fallback doesn't compare against a
    /// stale roster of dead entity ids left over from the previous room
    /// visit — a design-review correction to the time-well/room integration
    /// plan, since without this the fallback can match by COUNT ALONE and
    /// silently never respawn a single ray). [`despawn_track_rays`] also
    /// calls this directly; its own registration on `Screen::TimeWell`'s
    /// `OnExit` is gone as of Slice D (`lovely-swimming-prism.md`), but a
    /// test still exercises it standalone (see its own doc).
    pub fn clear_ray_entities(&mut self) {
        self.ray_entities.clear();
    }

    #[cfg(test)]
    fn ray_entity_count(&self) -> usize {
        self.ray_entities.len()
    }
}

/// Poll `listTracks` while the well is open (mirrors `sync::poll_clusters`).
pub fn poll_tracks(
    actor: Option<Res<RpcActor>>,
    time: Res<Time>,
    mut last_poll: Local<f64>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };
    let elapsed = time.elapsed_secs_f64();
    if elapsed - *last_poll < TRACK_POLL_INTERVAL {
        return;
    }
    *last_poll = elapsed;

    let handle = actor.handle.clone();
    let tx = result_channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.list_tracks().await {
                Ok(tracks) => {
                    let _ = tx.send(RpcResultMessage::TracksReceived { tracks });
                }
                Err(e) => log::debug!("time-well: list_tracks failed: {e}"),
            }
        })
        .detach();
}

/// Drain `TracksReceived` into [`WellTracks`]. Change-guarded: an unchanged
/// roster doesn't dirty the resource (so `sync_track_rays` stays idle).
pub fn apply_tracks(
    mut state: ResMut<WellTracks>,
    mut events: MessageReader<RpcResultMessage>,
) {
    for ev in events.read() {
        if let RpcResultMessage::TracksReceived { tracks } = ev
            && state.tracks != *tracks
        {
            debug!("time-well tracks: {} tracks", tracks.len());
            state.set_tracks(tracks.clone());
        }
    }
}

/// Reconcile ray entities against the polled roster: spawn a beam per new
/// track, despawn beams whose track vanished. Angle/hue derive purely from
/// the track name, so surviving rays never need repositioning.
pub fn sync_track_rays(
    mut commands: Commands,
    mut state: ResMut<WellTracks>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TrackRayMaterial>>,
    roots: Query<Entity, With<super::scene::TimeWellRoot>>,
) {
    // Run when the roster changed OR when the entity count disagrees with it —
    // the count fallback covers well re-entry (exit despawned the rays but the
    // roster survived, and an unchanged re-poll won't dirty the resource) and
    // dodges the screen-gated change-detection footgun, same as
    // `sync::sync_time_well` (found by the DeepSeek review, 2026-07-04).
    if !state.is_changed() && state.ray_entities.len() == state.tracks.len() {
        return;
    }

    // Compute the diff through `&state` reads only, and bail before ANY
    // mutable touch when there is nothing to reconcile: `ResMut`'s `DerefMut`
    // flags the resource changed even for a no-op write, so an unconditional
    // `get_or_insert_with` here would re-dirty `WellTracks` every run and the
    // system would never sleep (found by the Gemini review, 2026-07-04).
    let live: std::collections::HashSet<&str> =
        state.tracks.iter().map(|t| t.id.as_str()).collect();
    let dead: Vec<String> = state
        .ray_entities
        .keys()
        .filter(|id| !live.contains(id.as_str()))
        .cloned()
        .collect();
    let new: Vec<TrackInfo> = state
        .tracks
        .iter()
        .filter(|t| !state.ray_entities.contains_key(&t.id))
        .cloned()
        .collect();
    if dead.is_empty() && new.is_empty() {
        return;
    }

    // Despawn rays whose track is gone.
    for id in dead {
        if let Some(e) = state.ray_entities.remove(&id) {
            commands.entity(e).despawn();
        }
    }

    if new.is_empty() {
        return;
    }
    // The placement/root (Slice B): every ray becomes its `ChildOf` descendant
    // below. Unlike `sync_time_well`'s equivalent lookup, a missing root here
    // is NOT a broken invariant worth panicking over — nothing above this
    // point committed `new` anywhere durable (it's re-derived fresh from
    // `state.ray_entities` every call), so skipping the spawn just leaves
    // these tracks in `new` again next frame; self-healing once the well
    // actually enters.
    let Ok(root) = roots.single() else {
        return;
    };
    // Spawn rays for new tracks (shared spiral ribbon mesh, built on first use).
    let mesh = state.ray_mesh.get_or_insert_with(|| meshes.add(spiral_ribbon_mesh())).clone();
    for t in new {
        let angle = ray_angle(&t.id);
        let c = super::scene::accent_color(&t.id).to_linear();
        let material = materials.add(TrackRayMaterial::new(Vec4::new(
            c.red, c.green, c.blue, 0.85,
        )));
        let entity = commands
            .spawn((
                TrackRay {
                    track_id: t.id.clone(),
                    score_context: t.score_context_id,
                },
                Mesh3d(mesh.clone()),
                MeshMaterial3d(material),
                ray_transform(angle),
                Visibility::Inherited,
                Name::new(format!("TrackRay({})", t.id)),
                ChildOf(root),
            ))
            .id();
        state.ray_entities.insert(t.id, entity);
    }
}

/// Reset the ray roster's id→entity map on exit. The `TrackRay` entities
/// themselves die with the placement root's own recursive despawn (Slice B
/// reparenting — every ray is its `ChildOf` descendant; the room's own
/// `RoomRoot` teardown cascades to it as of Slice C); despawning them again
/// here would just be racing/duplicating that command. This only clears the
/// stale map, so re-entry's count fallback (`sync_track_rays`'s
/// `state.ray_entities.len() == state.tracks.len()` guard) doesn't compare
/// against dangling entity ids and skip the respawn.
///
/// No production caller left as of Slice D (`Screen::TimeWell`'s `OnExit`
/// registration, its only caller, is gone — `arm_well` covers the same
/// clearing duty on room re-entry via `clear_ray_entities` directly); kept
/// for the `rays_respawn_after_exit_even_with_an_unchanged_roster` test
/// below, which calls it directly to simulate the old exit path.
#[allow(dead_code)] // only reachable from the test below (see doc above)
pub fn despawn_track_rays(mut state: ResMut<WellTracks>) {
    state.clear_ray_entities();
}

/// Quantization step for ray uniforms — same discipline as the card lanes:
/// coarse enough that a settled (stopped, quiet) ray stops re-extracting.
const RAY_LANE_STEP: f32 = 1.0 / 64.0;

fn quantize(v: f32) -> f32 {
    (v / RAY_LANE_STEP).round() * RAY_LANE_STEP
}

/// Push each ray's live state into its material: beat envelope + fractional
/// beat position from the track's phasor, transport gate from the roster,
/// and the attached contexts' chatter. A playing ray animates every frame
/// (the pulse rides `beat_frac`); a stopped, quiet ray settles and stops
/// writing.
pub fn animate_track_rays(
    state: Res<WellTracks>,
    beats: Res<super::live::WellBeats>,
    activity: Res<super::activity::RingActivity>,
    mut materials: ResMut<Assets<TrackRayMaterial>>,
    rays: Query<(&TrackRay, &MeshMaterial3d<TrackRayMaterial>)>,
) {
    let now = Instant::now();
    for (ray, handle) in rays.iter() {
        let info = state.tracks.iter().find(|t| t.id == ray.track_id);
        let playing = info.is_some_and(|t| t.playing);
        let (env, frac) = beats.envelope_and_frac(&ray.score_context, now);
        // The lane's chatter: the loudest attached context (score included).
        let chatter = info
            .map(|t| {
                t.attached
                    .iter()
                    .chain(std::iter::once(&t.score_context_id))
                    .map(|c| activity.context_energy(c) / super::activity::CONTEXT_MAX)
                    .fold(0.0f32, f32::max)
                    .clamp(0.0, 1.0)
            })
            .unwrap_or(0.0);

        let next = Vec4::new(
            quantize(env),
            if playing { 1.0 } else { 0.0 },
            // beat_frac is the pulse's position — quantizing it would stutter
            // the packet, so it stays continuous; it only varies while a
            // phasor exists (0.0 otherwise, which settles).
            frac,
            quantize(chatter),
        );
        let Some(cur) = materials.get(&handle.0).map(|m| m.params) else {
            continue;
        };
        if cur != next
            && let Some(mat) = materials.get_mut(&handle.0)
        {
            mat.params = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ray_angle_is_stable_in_range_and_distinct_across_names() {
        let a = ray_angle("bass");
        assert_eq!(a, ray_angle("bass"), "same name → same bearing, always");
        for name in ["bass", "drums", "keys", "eurorack", "practice-machine"] {
            let angle = ray_angle(name);
            assert!((0.0..TAU).contains(&angle), "{name}: {angle}");
        }
        assert_ne!(ray_angle("bass"), ray_angle("drums"));
    }

    #[test]
    fn spiral_point_endpoints_land_on_ray_inner_and_outer() {
        let throat = spiral_point(0.0);
        let mouth = spiral_point(1.0);
        let (r0, z0) = RAY_INNER;
        let (r1, z1) = RAY_OUTER;
        assert!((throat.xy().length() - r0).abs() < 1e-3, "throat radius: {throat:?}");
        assert!((throat.z - z0).abs() < 1e-3, "throat depth: {throat:?}");
        assert!((mouth.xy().length() - r1).abs() < 1e-3, "mouth radius: {mouth:?}");
        assert!((mouth.z - z1).abs() < 1e-3, "mouth depth: {mouth:?}");
    }

    #[test]
    fn spiral_point_depth_rises_monotonically_from_throat_to_mouth() {
        let depths: Vec<f32> = (0..=20).map(|i| spiral_point(i as f32 / 20.0).z).collect();
        for w in depths.windows(2) {
            assert!(w[1] > w[0], "depth should climb toward the mouth: {depths:?}");
        }
    }

    #[test]
    fn spiral_point_swirls_one_direction_the_whole_way() {
        // Bearing (atan2) at each sample should keep moving the same way —
        // the drain-swirl invariant that keeps two rays from ever crossing
        // into a straight-chord star.
        let bearings: Vec<f32> = (0..=20)
            .map(|i| {
                let p = spiral_point(i as f32 / 20.0);
                p.y.atan2(p.x)
            })
            .collect();
        let mut sign = None;
        for w in bearings.windows(2) {
            let d = w[1] - w[0];
            assert!(d.abs() > 1e-6, "bearing should keep moving: {bearings:?}");
            match sign {
                None => sign = Some(d.signum()),
                Some(s) => assert_eq!(d.signum(), s, "swirl reversed direction: {bearings:?}"),
            }
        }
    }

    #[test]
    fn spiral_mouth_end_sits_at_bearing_zero_in_the_baked_mesh() {
        // `ray_angle` addresses a track by where its ray meets the MOUTH, so
        // the baked (bearing-0) mesh's mouth end must sit exactly on the +X
        // axis for `ray_transform`'s single Z-rotation to place it correctly.
        let mouth = spiral_point(1.0);
        assert!(mouth.y.atan2(mouth.x).abs() < 1e-4, "mouth end bakes at angle 0: {mouth:?}");
        assert!(mouth.x > 0.0, "…specifically on +X: {mouth:?}");
    }

    #[test]
    fn spiral_ribbon_uvs_are_monotonic_arclength_ending_at_the_mouth() {
        let (_, _, uvs, _) = spiral_ribbon_vertices();
        let u_at = |i: usize| uvs[i * 2][0];
        assert!(u_at(0).abs() < 1e-6, "throat end starts at uv.x = 0: {}", u_at(0));
        assert!(
            (u_at(SPIRAL_SEGMENTS) - 1.0).abs() < 1e-6,
            "mouth end ends at uv.x = 1: {}",
            u_at(SPIRAL_SEGMENTS)
        );
        for i in 0..SPIRAL_SEGMENTS {
            assert!(u_at(i) <= u_at(i + 1) + 1e-6, "uv.x must be monotonic along the ribbon");
        }
    }

    #[test]
    fn spiral_ribbon_vertices_are_two_per_sample_and_six_indices_per_segment() {
        let (positions, normals, uvs, indices) = spiral_ribbon_vertices();
        assert_eq!(positions.len(), (SPIRAL_SEGMENTS + 1) * 2, "2 verts per sample");
        assert_eq!(normals.len(), positions.len(), "one normal per vertex");
        assert_eq!(uvs.len(), positions.len(), "one uv per vertex");
        assert_eq!(indices.len(), SPIRAL_SEGMENTS * 6, "6 indices per segment");
        assert!(indices.iter().all(|&i| (i as usize) < positions.len()), "every index in range");
    }

    #[test]
    fn spiral_ribbon_width_matches_ray_width_at_every_sample() {
        let (positions, _, _, _) = spiral_ribbon_vertices();
        for pair in positions.chunks(2) {
            let l = Vec3::from_array(pair[0]);
            let r = Vec3::from_array(pair[1]);
            let width = l.distance(r);
            assert!((width - RAY_WIDTH).abs() < 1e-2, "ribbon width drifted: {width}");
        }
    }

    #[test]
    fn ray_transform_places_the_baked_mouth_and_throat_at_the_ray_bearing() {
        let angle = 0.7f32;
        let tilt = super::super::card::well_tilt_quat();
        let tf = ray_transform(angle);

        let mouth = tf.translation + tf.rotation * spiral_point(1.0);
        let throat = tf.translation + tf.rotation * spiral_point(0.0);

        let (r0, z0) = RAY_INNER;
        let (r1, z1) = RAY_OUTER;
        let expect_mouth = tilt * Vec3::new(r1 * angle.cos(), r1 * angle.sin(), z1);
        // The throat trails the mouth's bearing by SPIRAL_SWEEP (the baked
        // mesh's own throat angle), rotated by the same `angle`.
        let throat_bearing = angle + SPIRAL_SWEEP;
        let expect_throat =
            tilt * Vec3::new(r0 * throat_bearing.cos(), r0 * throat_bearing.sin(), z0);

        assert!(mouth.distance(expect_mouth) < 1e-3, "mouth end addresses the ray's bearing: {mouth:?}");
        assert!(throat.distance(expect_throat) < 1e-3, "throat trails by SPIRAL_SWEEP: {throat:?}");
    }

    fn track(id: &str, score: u8, attached: &[u8]) -> TrackInfo {
        TrackInfo {
            id: id.into(),
            score_context_id: ContextId::from_bytes([score; 16]),
            playing: false,
            playhead_tick: 0,
            period_us: 500_000,
            beats_per_phrase: 32,
            beat_count: 0,
            last_epoch_ns: 0,
            clock_kind: "system".into(),
            attached: attached
                .iter()
                .map(|n| ContextId::from_bytes([*n; 16]))
                .collect(),
        }
    }

    /// Regression (DeepSeek review, 2026-07-04): exit despawns the rays but
    /// the roster survives in `WellTracks`; if the next poll returns the SAME
    /// roster, nothing dirties the resource — the count fallback must still
    /// respawn the beams on re-entry.
    ///
    /// Updated for Slice B (`ChildOf` reparenting): `sync_track_rays` now
    /// requires a `TimeWellRoot` placement entity to spawn rays under (a real
    /// room visit always has one, from `spawn_well_furniture`), and the real
    /// exit path despawns rays via the ROOT's own recursive despawn
    /// (`RoomRoot`'s teardown, Slice C), not `despawn_track_rays` directly
    /// anymore — this test stands in both: spawns a bare placement entity,
    /// then despawns it to simulate the real exit.
    #[test]
    fn rays_respawn_after_exit_even_with_an_unchanged_roster() {
        use bevy::ecs::system::RunSystemOnce;

        fn ray_count(app: &mut App) -> usize {
            app.world_mut().query::<&TrackRay>().iter(app.world()).count()
        }

        let mut app = App::new();
        app.insert_resource(Assets::<Mesh>::default())
            .insert_resource(Assets::<TrackRayMaterial>::default())
            .init_resource::<WellTracks>()
            .add_systems(Update, sync_track_rays);

        let root = app.world_mut().spawn(super::super::scene::TimeWellRoot).id();

        app.world_mut()
            .resource_mut::<WellTracks>()
            .set_tracks(vec![track("bass", 9, &[1])]);
        app.update();
        assert_eq!(ray_count(&mut app), 1, "ray spawned for the track");
        app.update();
        assert_eq!(ray_count(&mut app), 1, "settled roster spawns nothing new");

        // Exit the room: the placement root's recursive despawn removes every
        // ray (the real exit path, `RoomRoot`'s own teardown); `despawn_track_rays`
        // only resets the roster's id→entity map now (see its doc comment) so
        // the count fallback below doesn't compare against dangling ids.
        app.world_mut().despawn(root);
        app.world_mut().run_system_once(despawn_track_rays).unwrap();
        assert_eq!(ray_count(&mut app), 0, "exit leaves no rays");

        // Re-enter: a fresh placement root, no roster change (an unchanged
        // re-poll never calls set_tracks) — the count fallback alone must
        // respawn the beam.
        app.world_mut().spawn(super::super::scene::TimeWellRoot);
        app.update();
        assert_eq!(ray_count(&mut app), 1, "re-entry respawns from the surviving roster");
    }

    #[test]
    fn clear_ray_entities_empties_the_map_without_touching_the_roster() {
        let mut wt = WellTracks::default();
        wt.set_tracks(vec![track("bass", 9, &[1])]);
        wt.ray_entities.insert("bass".into(), bevy::prelude::Entity::PLACEHOLDER);
        assert_eq!(wt.ray_entity_count(), 1);

        wt.clear_ray_entities();

        assert_eq!(wt.ray_entity_count(), 0, "the stale id->entity map clears");
        assert_eq!(wt.tracks.len(), 1, "the roster itself is untouched");
    }

    #[test]
    fn set_tracks_maps_attached_and_score_contexts_to_the_lane() {
        let mut wt = WellTracks::default();
        wt.set_tracks(vec![track("bass", 9, &[1, 2]), track("drums", 8, &[3])]);

        let score = ContextId::from_bytes([9; 16]);
        let player = ContextId::from_bytes([1; 16]);
        assert_eq!(wt.beat_key_of[&player], score, "attached → its track's score ctx");
        assert_eq!(wt.beat_key_of[&score], score, "score ctx keys itself");
        assert_eq!(wt.track_of[&player], "bass");
        assert_eq!(wt.track_of[&ContextId::from_bytes([3; 16])], "drums");
        assert_eq!(wt.track_info_of(&player).unwrap().id, "bass");
        assert!(wt.track_info_of(&ContextId::from_bytes([7; 16])).is_none());

        // A re-poll fully replaces the maps (no stale bindings linger).
        wt.set_tracks(vec![track("bass", 9, &[2])]);
        assert!(!wt.track_of.contains_key(&player), "detached context unmapped");
        assert!(!wt.track_of.contains_key(&ContextId::from_bytes([3; 16])));
    }
}
