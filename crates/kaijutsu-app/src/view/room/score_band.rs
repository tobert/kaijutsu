//! The rehearsal score on eight flat faces around the Time Well.

use std::time::Instant;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, futures_lite::future};

use crate::text::SvgFontDb;
use crate::ui::theme::Theme;
use crate::view::heroic::live::RoomScore;
use crate::view::scene_palette::ScenePalette;
use crate::view::time_well::live::WellBeatsRes;
use crate::view::time_well::rays::WellTracks;
use super::{RoomState, Station, score_ink};

use super::{bearing, scene_geometry};

const FACES: usize = 8;
const RADIUS: f32 = scene_geometry::WALL_APOTHEM * 1.06;
const CENTER_Y: f32 = super::WALL_HEIGHT * 0.56;

fn face_pose(index: usize) -> Transform {
    let panel = bearing::octagon_panels(RADIUS)[index];
    let center = Vec3::new(panel.center[0], CENTER_Y, panel.center[2]);
    Transform::from_translation(center).looking_to(center - Vec3::Y * CENTER_Y, Vec3::Y)
}

fn window_index(beat: f64, phrase_start: i64, bar_beats: f64) -> usize {
    ((beat - phrase_start as f64).max(0.0) / (bar_beats * FACES as f64)).floor() as usize
}

#[derive(Component)]
pub(super) struct WallShell;

#[derive(Component)]
pub(super) struct Playhead;

#[derive(Clone, Copy, PartialEq)]
struct PageKey {
    revision: u32,
    window: usize,
    ink: Color,
    fonts: Option<u32>,
}

#[derive(Component)]
pub(super) struct ScoreBand {
    faces: Vec<Handle<StandardMaterial>>,
    meter: f64,
    requested: Option<PageKey>,
    task: Option<Task<score_ink::EngravedBand>>,
    spans: Vec<(f64, f64)>,
}

pub(super) fn spawn(
    commands: &mut Commands,
    room: Entity,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    palette: &ScenePalette,
) {
    let width = bearing::octagon_panel_width(RADIUS);
    let height = width * 0.5;
    let mesh = meshes.add(Rectangle::new(width, height));
    let root = commands.spawn((
        Transform::default(), Visibility::Hidden, ChildOf(room), Name::new("Rehearsal score band"),
    )).id();
    let mut faces = Vec::with_capacity(FACES);
    for index in 0..FACES {
        let material = materials.add(StandardMaterial {
            unlit: true,
            alpha_mode: bevy::material::AlphaMode::Blend,
            base_color: Color::NONE,
            ..default()
        });
        commands.spawn((
            Mesh3d(mesh.clone()), MeshMaterial3d(material.clone()), face_pose(index),
            Name::new(format!("Score facet {}", index + 1)), ChildOf(root),
        ));
        faces.push(material);
    }
    commands.spawn((
        Playhead, Mesh3d(meshes.add(Rectangle::new(5.0, height * 0.86))),
        MeshMaterial3d(materials.add(super::unlit(Color::LinearRgba(palette.gold)))),
        Transform::default(), Visibility::Hidden, ChildOf(root), Name::new("Score playhead"),
    ));
    commands.entity(root).insert(ScoreBand {
        faces, meter: 4.0, requested: None, task: None, spans: Vec::new(),
    });
}

/// A roster snapshot locates the open page until the track's own clock arrives.
fn score_position(score: &RoomScore, beats: &WellBeatsRes, tracks: &WellTracks) -> (f64, bool) {
    let start = score.start as f64;
    let Some(context) = score.score_context else { return (start, false) };
    if let Some(beat) = beats.beat_position(&context, Instant::now()) {
        return (beat.max(start), beat >= start);
    }
    let snapshot = tracks.track_info_of(&context).map_or(start, |t| t.playhead_tick as f64);
    (snapshot.max(start), false)
}

/// Keep the current eight bars. Engraving runs off the frame loop, one job at a time.
pub(super) fn engrave_score(
    score: Res<RoomScore>,
    beats: Res<WellBeatsRes>,
    tracks: Res<WellTracks>,
    theme: Res<Theme>,
    fonts: Option<Res<SvgFontDb>>,
    mut band: Query<&mut ScoreBand>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Ok(mut band) = band.single_mut() else { return };
    let revision = score.last_changed().get();
    if score.parts.is_empty() {
        band.spans.clear();
        band.requested = None;
        band.task = None;
        return;
    }
    if band.requested.is_none_or(|key| key.revision != revision) {
        band.meter = score.parts.iter().find_map(|p| score_ink::beats_per_bar(&p.abc)).unwrap_or(4.0);
    }
    let (beat, _) = score_position(&score, &beats, &tracks);
    let window = window_index(beat, score.start, band.meter);
    let wanted = PageKey { revision, window, ink: theme.fg, fonts: fonts.as_ref().map(|f| f.last_changed().get()) };
    if let Some(task) = band.task.as_mut() {
        let Some(pages) = block_on(future::poll_once(task)) else {
            // A later phrase or track may become current while this job is running.
            if band.requested != Some(wanted) { band.spans.clear(); }
            return;
        };
        band.task = None;
        if band.requested == Some(wanted) {
            debug!("score band: beat {beat:.1}, start {}, window {window}, engraved spans {:?}", score.start, pages.spans);
            band.spans = pages.spans;
            for (face, image) in band.faces.iter().zip(pages.pages) {
                if let Some(mut m) = materials.get_mut(face) {
                    m.base_color_texture = Some(images.add(image));
                    m.base_color = Color::WHITE;
                }
            }
        }
    }
    if band.requested == Some(wanted) { return; }
    let meter = band.meter;
    let origin = score.start as f64 + window as f64 * FACES as f64 * meter;
    let parts: Vec<_> = score.parts.iter().map(|p| (p.abc.clone(), p.start as f64 - origin)).collect();
    band.spans.clear();
    band.requested = Some(wanted);
    let fonts = fonts.as_deref().cloned();
    band.task = Some(AsyncComputeTaskPool::get().spawn(async move {
        let parts: Vec<_> = parts.iter().map(|(abc, offset)| (abc.as_str(), *offset)).collect();
        score_ink::engrave_facets(&parts, meter, FACES, wanted.ink, fonts.as_ref())
    }));
}

pub(super) fn show_score(
    room: Res<RoomState>,
    score: Res<RoomScore>,
    beats: Res<WellBeatsRes>,
    tracks: Res<WellTracks>,
    mut band: Query<(&ScoreBand, &mut Transform, &mut Visibility), (Without<WallShell>, Without<Playhead>)>,
    mut wall: Query<&mut Visibility, (With<WallShell>, Without<Playhead>)>,
    mut cursor: Query<(&mut Transform, &mut Visibility), With<Playhead>>,
) {
    let (Ok((band, mut ring, mut visible)), Ok(mut shell), Ok((mut line, mut line_visible))) =
        (band.single_mut(), wall.single_mut(), cursor.single_mut()) else { return };
    let music_view = room.carousel.focused_station() != Station::Switchboard;
    let show = music_view && !band.spans.is_empty();
    visible.set_if_neq(if show { Visibility::Inherited } else { Visibility::Hidden });
    shell.set_if_neq(if music_view && score.score_context.is_some() {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    });
    let (beat, rolling) = score_position(&score, &beats, &tracks);
    let window_start = band.requested.map_or(0.0, |key| key.window as f64 * band.meter * FACES as f64);
    let phase = beat - score.start as f64 - window_start;
    let on_staff = rolling && band.spans.iter().any(|&(start, end)| start <= phase && phase < end);
    line_visible.set_if_neq(if show && on_staff { Visibility::Inherited } else { Visibility::Hidden });
    if !show { return; }

    let page = (beat - score.start as f64) / band.meter;
    let index = page.floor() as usize % FACES;
    let fraction = page.fract() as f32;
    // Bring the current measure across the north face at an even angular speed.
    let yaw = (index as f32 + fraction - 0.5) * std::f32::consts::TAU / FACES as f32;
    ring.rotation = Quat::from_rotation_y(yaw);
    let u = score_ink::BEAT_LEFT + fraction * (score_ink::BEAT_RIGHT - score_ink::BEAT_LEFT);
    let x = (u - 0.5) * bearing::octagon_panel_width(RADIUS);
    *line = face_pose(index) * Transform::from_xyz(x, 0.0, 1.5);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_staff_edges_meet_and_face_the_well() {
        let half_width = bearing::octagon_panel_width(RADIUS) / 2.0;
        for index in 0..FACES {
            let face = face_pose(index);
            let next = face_pose((index + 1) % FACES);
            let right = face.transform_point(Vec3::X * half_width);
            let left = next.transform_point(Vec3::NEG_X * half_width);
            assert!(right.distance(left) < 0.01, "gap at face {index}: {right:?}, {left:?}");
            let inward = Vec3::Y * CENTER_Y - face.translation;
            assert!(face.rotation.mul_vec3(Vec3::Z).dot(inward) > 0.99 * RADIUS);
        }
    }

    #[test]
    fn a_roster_snapshot_opens_the_current_page_before_the_clock_arrives() {
        let context = kaijutsu_types::ContextId::new();
        let mut tracks = WellTracks::default();
        tracks.set_tracks(vec![kaijutsu_client::TrackInfo {
            id: "trio".into(), score_context_id: context, playing: true,
            playhead_tick: 140, period_us: 500_000, beats_per_phrase: 32,
            beat_count: 140, last_epoch_ns: 0, clock_kind: "system".into(), attached: Vec::new(),
        }]);
        let score = RoomScore { start: 100, score_context: Some(context), ..default() };
        let (beat, rolling) = score_position(&score, &WellBeatsRes::default(), &tracks);
        assert_eq!(window_index(beat, score.start, 4.0), 1);
        assert!(!rolling);
    }

    #[test]
    fn a_long_phrase_advances_to_its_later_pages() {
        assert_eq!(window_index(121.9, 90, 4.0), 0);
        assert_eq!(window_index(122.0, 90, 4.0), 1);
        assert_eq!(window_index(120.0, 90, 3.0), 1);
    }
}
