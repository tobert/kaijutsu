//! Track rays — tracks as beams down the funnel wall of the well.
//!
//! A track is the durable lane (docs/tracks.md); contexts churn around it.
//! Each track renders as one thin [`TrackRayMaterial`] beam lying on the
//! funnel wall — from the vortex throat out through the magic rings to the
//! mouth — at an angle **hashed stably from the track's name** (same FNV
//! recipe as the card accents): the angle never moves across restarts or
//! track churn, so a lane lives at a learnable bearing. While the track's
//! clock rolls, a pulse rides the beam mouth→throat each beat, driven by the
//! same per-track phasors the cards use ([`super::live::WellBeats`]).
//!
//! Data flow mirrors the cluster poll: [`poll_tracks`] ships `listTracks`
//! every few seconds while the well is open; [`apply_tracks`] drains the
//! result into [`WellTracks`] (the roster + derived context→track maps that
//! also feed the cards' border hue / beat keying and the HUD's track line);
//! [`sync_track_rays`] reconciles ray entities; [`animate_track_rays`] pushes
//! the per-frame phasor state into the material uniforms.

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

/// Ray beam width (well units) — a filament, not a wall.
const RAY_WIDTH: f32 = 14.0;

/// Funnel-local endpoints of every ray: from just above the throat floor
/// (the ring deck sits at −850; the beam melts into its glow) out to just
/// past the mouth ring. Radii bracket the ring stack (mouth ring radius 500,
/// vortex core well inside 70).
const RAY_INNER: (f32, f32) = (70.0, -840.0); // (radius, depth) at the throat
const RAY_OUTER: (f32, f32) = (520.0, 10.0); // (radius, depth) past the mouth

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

/// A ray's funnel-local endpoints at `angle`: `(throat_end, mouth_end)`.
pub fn ray_endpoints(angle: f32) -> (Vec3, Vec3) {
    let (r0, z0) = RAY_INNER;
    let (r1, z1) = RAY_OUTER;
    (
        Vec3::new(r0 * angle.cos(), r0 * angle.sin(), z0),
        Vec3::new(r1 * angle.cos(), r1 * angle.sin(), z1),
    )
}

/// The world transform of a ray quad at `angle`: a `Rectangle(len, RAY_WIDTH)`
/// laid along the funnel wall — local +X runs throat→mouth (matching the
/// shader's uv.x), local +Y is the angular (width) direction, local +Z the
/// cone-surface normal (faces up out of the bowl) — then reclined into world
/// space by the shared funnel tilt.
pub fn ray_transform(angle: f32) -> Transform {
    let (p0, p1) = ray_endpoints(angle);
    let dir = (p1 - p0).normalize();
    let angular = Vec3::new(-angle.sin(), angle.cos(), 0.0);
    let normal = dir.cross(angular).normalize();
    // dir × angular has +Z-dominant sign for a bowl opening toward +Z; keep it
    // facing the mouth so the beam reads from the camera side.
    let normal = if normal.z < 0.0 { -normal } else { normal };
    let tilt = super::card::well_tilt_quat();
    Transform {
        translation: tilt * p0.midpoint(p1),
        rotation: tilt * Quat::from_mat3(&Mat3::from_cols(dir, angular, normal)),
        scale: Vec3::ONE,
    }
}

/// Length of every ray beam (all rays share one endpoint recipe → one mesh).
pub fn ray_length() -> f32 {
    let (p0, p1) = ray_endpoints(0.0);
    p0.distance(p1)
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
    /// context → track id, for the border hue + the HUD's track line. Same
    /// coverage as `beat_key_of`.
    pub track_of: HashMap<ContextId, String>,
    /// track id → live ray entity (the rays' own little join).
    ray_entities: HashMap<String, Entity>,
    /// Shared beam mesh, built on first use.
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
) {
    if !state.is_changed() {
        return;
    }

    // Despawn rays whose track is gone.
    let live: std::collections::HashSet<&str> =
        state.tracks.iter().map(|t| t.id.as_str()).collect();
    let dead: Vec<String> = state
        .ray_entities
        .keys()
        .filter(|id| !live.contains(id.as_str()))
        .cloned()
        .collect();
    for id in dead {
        if let Some(e) = state.ray_entities.remove(&id) {
            commands.entity(e).despawn();
        }
    }

    // Spawn rays for new tracks.
    let mesh = state
        .ray_mesh
        .get_or_insert_with(|| meshes.add(Rectangle::new(ray_length(), RAY_WIDTH)))
        .clone();
    let new: Vec<TrackInfo> = state
        .tracks
        .iter()
        .filter(|t| !state.ray_entities.contains_key(&t.id))
        .cloned()
        .collect();
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
            ))
            .id();
        state.ray_entities.insert(t.id, entity);
    }
}

/// Despawn every ray on exit (the well leaves no residue) and drop the join
/// so re-entering respawns from the next poll.
pub fn despawn_track_rays(
    mut commands: Commands,
    mut state: ResMut<WellTracks>,
    rays: Query<Entity, With<TrackRay>>,
) {
    for e in rays.iter() {
        commands.entity(e).despawn();
    }
    state.ray_entities.clear();
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
    fn ray_endpoints_run_throat_to_mouth_at_the_angle() {
        let angle = 1.2f32;
        let (p0, p1) = ray_endpoints(angle);
        assert!(p0.z < p1.z, "throat end is deeper");
        assert!(p0.xy().length() < p1.xy().length(), "throat end is inner");
        // Both ends sit on the ray's bearing.
        assert!((p0.y.atan2(p0.x) - angle).abs() < 1e-4);
        assert!((p1.y.atan2(p1.x) - angle).abs() < 1e-4);
    }

    #[test]
    fn ray_transform_maps_local_x_from_throat_to_mouth() {
        let angle = 0.7f32;
        let (p0, p1) = ray_endpoints(angle);
        let tilt = super::super::card::well_tilt_quat();
        let tf = ray_transform(angle);
        let half = ray_length() * 0.5;
        // The quad's -X end lands on the (tilted) throat endpoint, +X on the mouth.
        let world_p0 = tf.translation + tf.rotation * Vec3::new(-half, 0.0, 0.0);
        let world_p1 = tf.translation + tf.rotation * Vec3::new(half, 0.0, 0.0);
        assert!(world_p0.distance(tilt * p0) < 1e-3, "-X = throat end");
        assert!(world_p1.distance(tilt * p1) < 1e-3, "+X = mouth end");
        // The face normal points out of the bowl (toward the mouth side).
        let n = tf.rotation * Vec3::Z;
        assert!((tilt.inverse() * n).z > 0.0, "faces up out of the funnel");
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
