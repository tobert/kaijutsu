use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::{NoFrustumCulling, RenderLayers};
use bevy::color::Mix;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::ui::UiGlobalTransform;
use kaijutsu_types::BlockId;

use super::{SceneAssets, STAGE_LAYER, ThreadNotch};
use super::super::{NeoMode, SceneContent};
use super::super::cards::ThreadLink;
use super::super::model::{Section, Workspace};
use crate::ui::{screen::Screen, theme::Theme};
use crate::view::scene_palette::HeroicPalette;

use super::layout::CAMERA_DISTANCE;

const GLINT_DURATION: f32 = 0.55;

const STROKE_DEPTH: f32 = 0.16;
const PORT_GAP: f32 = 0.040;
const TARGET_ROW: f32 = 0.22;

fn port(card: &Transform, x: f32, y: f32) -> Vec2 {
    let point = card.transform_point(Vec3::new(x, y, 0.0));
    let scale = CAMERA_DISTANCE / (CAMERA_DISTANCE - point.z);
    point.truncate() * scale + Vec2::new(x.signum() * PORT_GAP, 0.0)
}

fn route(dialogue: &Transform, notch: f32, event: &Transform, patch: Option<&Transform>) -> Vec<Vec3> {
    let start = port(dialogue, 0.5, notch);
    let receipt = port(event, -0.5, TARGET_ROW);
    let mut points = Vec::with_capacity(if patch.is_some() { 25 } else { 13 });
    points.push(start);
    let bend = (receipt.x - start.x) * 0.62;
    let approach = receipt - Vec2::X * bend;
    curve(&mut points, start, start + Vec2::X * bend, approach, receipt);
    if let Some(patch) = patch {
        let end = port(patch, -0.5, TARGET_ROW);
        let x = receipt.x.min(end.x) - PORT_GAP;
        curve(&mut points, receipt, receipt + Vec2::Y * ((end.y - receipt.y) * 0.44), Vec2::new(x, end.y), end);
    }
    // Unproject the path onto one plane so the stroke stays in the visible gap.
    let scale = (CAMERA_DISTANCE - STROKE_DEPTH) / CAMERA_DISTANCE;
    points.into_iter().map(|point| (point * scale).extend(STROKE_DEPTH)).collect()
}

fn curve(points: &mut Vec<Vec2>, start: Vec2, a: Vec2, b: Vec2, end: Vec2) {
    for step in 1..=12 {
        let t = step as f32 / 12.0;
        let left = start.lerp(a, t).lerp(a.lerp(b, t), t);
        let right = a.lerp(b, t).lerp(b.lerp(end, t), t);
        points.push(left.lerp(right, t));
    }
}

fn along(points: &[Vec3], elapsed: f32) -> Option<Vec3> {
    if elapsed >= GLINT_DURATION {
        return None;
    }
    let length: f32 = points.windows(2).map(|span| span[0].distance(span[1])).sum();
    let mut distance = length * elapsed / GLINT_DURATION;
    for span in points.windows(2) {
        let segment = span[0].distance(span[1]);
        if distance <= segment && segment > 0.0 {
            return Some(span[0].lerp(span[1], distance / segment));
        }
        distance -= segment;
    }
    points.last().copied()
}

#[derive(Resource)]
pub(in crate::view::neo) struct Filament {
    stroke: Entity,
    glint: Entity,
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

pub(in crate::view::neo) fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mesh = meshes.add(stroke_mesh(&[Vec3::ZERO, Vec3::X]));
    let material = materials.add(StandardMaterial {
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        cull_mode: None,
        ..default()
    });
    let stroke = commands.spawn((
        Name::new("Neo Provenance Filament"),
        StrokePart,
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material.clone()),
        Transform::default(),
        Visibility::Hidden,
        NoFrustumCulling,
        RenderLayers::layer(STAGE_LAYER),
    )).id();
    let glint = commands.spawn((
        Name::new("Neo Provenance Glint"),
        StrokePart,
        Mesh3d(meshes.add(Circle::new(0.045))),
        MeshMaterial3d(materials.add(StandardMaterial { unlit: true, ..default() })),
        Transform::default(),
        Visibility::Hidden,
        RenderLayers::layer(STAGE_LAYER),
    )).id();
    commands.insert_resource(Filament { stroke, glint, mesh, material });
}

fn stroke_mesh(points: &[Vec3]) -> Mesh {
    let mut positions = Vec::new();
    let mut colors: Vec<[f32; 4]> = Vec::new();
    let mut indices = Vec::new();
    for (i, point) in points.iter().enumerate() {
        let before = points[i.saturating_sub(1)];
        let after = points[(i + 1).min(points.len() - 1)];
        let direction = (after - before).truncate().normalize_or_zero();
        let normal = direction.perp().extend(0.0);
        for (offset, alpha) in [(-0.030, 0.0), (-0.007, 1.0), (0.007, 1.0), (0.030, 0.0)] {
            positions.push((*point + normal * offset).to_array());
            colors.push([1.0, 1.0, 1.0, alpha]);
        }
        if i > 0 {
            for band in 0..3 {
                let a = ((i - 1) * 4 + band) as u32;
                indices.extend([a, a + 4, a + 5, a, a + 5, a + 1]);
            }
        }
    }
    let normals = vec![[0.0_f32, 0.0, 1.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

type Selection = (ThreadLink, Section, Option<BlockId>);

#[derive(Default)]
pub(in crate::view::neo) struct Playback {
    selection: Option<Selection>,
    start: Option<f64>,
    skip_glint: bool,
    geometry: Option<(Transform, f32, Transform, Option<Transform>)>,
    points: Vec<Vec3>,
}

pub(in crate::view::neo) fn sync(
    assets: Res<SceneAssets>,
    filament: Res<Filament>,
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    workspace: Res<Workspace>,
    content: Res<SceneContent>,
    theme: Res<Theme>,
    palette: Res<HeroicPalette>,
    time: Res<Time<Real>>,
    notches: Query<(&ComputedNode, &UiGlobalTransform), With<ThreadNotch>>,
    nodes: Query<(&ComputedNode, &UiGlobalTransform)>,
    cards: Query<(&Visibility, &Transform), Without<StrokePart>>,
    mut parts: Query<(&mut Visibility, &mut Transform), With<StrokePart>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut playback: Local<Playback>,
) {
    let linked = mode.enabled && *screen.get() == Screen::Conversation;
    let selection = content.thread.filter(|_| linked)
        .map(|link| (link, workspace.section, workspace.selection(workspace.section)));
    let geometry = selection.and_then(|(link, _, _)| {
        let card = |group| {
            let card = assets.cards.iter().find(|card| card.group == group && card.rank == 0)?;
            let (visibility, pose) = cards.get(card.surface).ok()?;
            (*visibility != Visibility::Hidden).then_some((card, *pose))
        };
        let (dialogue, pose) = card(0)?;
        let (_, event) = card(2)?;
        let patch = if link.patch.is_some() { Some(card(1)?.1) } else { None };
        let (bounds, root) = nodes.get(dialogue.ui).ok()?;
        let (marker, notch) = notches.single().ok()?;
        if bounds.is_empty() || marker.is_empty() {
            return None;
        }
        let row = (root.translation.y - notch.translation.y) / bounds.size().y;
        ((-0.5..=0.5).contains(&row)).then_some((pose, row, event, patch))
    });

    let now = time.elapsed_secs_f64();
    if playback.selection != selection {
        playback.selection = selection;
        playback.start = None;
        playback.skip_glint = false;
    }
    playback.skip_glint |= palette.reduced_motion;
    if let Some((dialogue, row, event, patch)) = geometry {
        if geometry != playback.geometry {
            playback.geometry = geometry;
            playback.points = route(&dialogue, row, &event, patch.as_ref());
            if let Some(mut mesh) = meshes.get_mut(&filament.mesh) {
                *mesh = stroke_mesh(&playback.points);
            }
        }
    }
    if (theme.is_changed() || filament.is_added()) && let Some(mut material) = materials.get_mut(&filament.material) {
        material.base_color = theme.accent.mix(&Color::WHITE, 0.18);
    }
    let glint = geometry.filter(|_| !playback.skip_glint).and_then(|_| {
        let start = *playback.start.get_or_insert(now);
        along(&playback.points, (now - start) as f32)
    });
    if let Ok((mut visible, _)) = parts.get_mut(filament.stroke) {
        visible.set_if_neq(if geometry.is_some() { Visibility::Visible } else { Visibility::Hidden });
    }
    if let Ok((mut visible, mut transform)) = parts.get_mut(filament.glint) {
        visible.set_if_neq(if glint.is_some() { Visibility::Visible } else { Visibility::Hidden });
        if let Some(point) = glint {
            transform.translation = point + Vec3::Z * 0.015;
        }
    }
}

#[derive(Component)]
pub(in crate::view::neo) struct StrokePart;

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::layout::pose;

    fn project(point: Vec3) -> Vec2 {
        point.truncate() * (CAMERA_DISTANCE / (CAMERA_DISTANCE - point.z))
    }

    #[test]
    fn connector_leaves_the_measured_dialogue_row_and_runs_in_the_card_gap() {
        for aspect in [0.75, 640.0 / 534.0, 16.0 / 9.0, 2.75] {
            let dialogue = pose(0, 0, aspect);
            let patch = pose(1, 0, aspect);
            let event = pose(2, 0, aspect);
            let notch = 0.23;
            let points = route(&dialogue, notch, &event, Some(&patch));
            let dialogue_port = project(dialogue.transform_point(Vec3::new(0.5, notch, 0.0)));
            let start = project(points[0]);
            assert!(start.x > dialogue_port.x && (start.y - dialogue_port.y).abs() < 0.001);
            assert!(points.iter().all(|point| project(*point).x > dialogue_port.x));
            let event_port = project(event.transform_point(Vec3::new(-0.5, 0.22, 0.0)));
            let patch_port = project(patch.transform_point(Vec3::new(-0.5, 0.22, 0.0)));
            let receipt = project(*route(&dialogue, notch, &event, None).last().unwrap());
            let end = project(*points.last().unwrap());
            assert!(receipt.x < event_port.x && (receipt.y - event_port.y).abs() < 0.001);
            assert!(end.x < patch_port.x && (end.y - patch_port.y).abs() < 0.001);
            assert!(points.iter().any(|point| project(*point).distance(receipt) < 0.001));
            assert!(points.iter().all(|point| project(*point).x < event_port.x.max(patch_port.x)));
        }
    }

    #[test]
    fn reduced_motion_keeps_the_filament_and_skips_the_glint() {
        use kaijutsu_types::{ContextId, PrincipalId};

        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        world.init_resource::<Theme>();
        world.init_resource::<Workspace>();
        world.init_resource::<Time<Real>>();
        world.insert_resource(NeoMode { enabled: true, ..default() });
        world.insert_resource(State::new(Screen::Conversation));
        world.insert_resource(HeroicPalette { reduced_motion: true, ..default() });
        let id = |seq| BlockId::new(ContextId::from_bytes([1; 16]), PrincipalId::from_bytes([2; 16]), seq);
        world.insert_resource(SceneContent {
            thread: Some(ThreadLink { call: id(3), receipt: None, patch: None }),
            ..default()
        });

        let cards = [0, 2].into_iter().map(|group| {
            let surface = world.spawn((Visibility::Visible, pose(group, 0, 16.0 / 9.0))).id();
            let ui = world.spawn((
                ComputedNode { size: Vec2::splat(400.0), ..default() },
                UiGlobalTransform::from_xy(200.0, 200.0),
            )).id();
            super::super::Card {
                surface, camera: Entity::PLACEHOLDER, ui, image: default(), material: default(), group, rank: 0,
            }
        }).collect();
        world.spawn((
            ThreadNotch,
            ComputedNode { size: Vec2::new(400.0, 20.0), ..default() },
            UiGlobalTransform::from_xy(200.0, 180.0),
        ));
        world.insert_resource(SceneAssets {
            camera: Entity::PLACEHOLDER, image: default(), mono: default(), serif: default(), cards,
        });

        let setup = world.register_system(setup);
        let sync = world.register_system(sync);
        world.run_system(setup).unwrap();
        world.run_system(sync).unwrap();
        let Filament { stroke, glint, .. } = *world.resource::<Filament>();
        assert_eq!(world.get::<Visibility>(stroke), Some(&Visibility::Visible));
        assert_eq!(world.get::<Visibility>(glint), Some(&Visibility::Hidden));

        world.resource_mut::<HeroicPalette>().reduced_motion = false;
        world.run_system(sync).unwrap();
        assert_eq!(world.get::<Visibility>(glint), Some(&Visibility::Hidden));

        world.resource_mut::<SceneContent>().thread.as_mut().unwrap().receipt = Some(id(4));
        world.run_system(sync).unwrap();
        assert_eq!(world.get::<Visibility>(glint), Some(&Visibility::Visible));

        world.resource_mut::<HeroicPalette>().reduced_motion = true;
        world.run_system(sync).unwrap();
        assert_eq!(world.get::<Visibility>(stroke), Some(&Visibility::Visible));
        assert_eq!(world.get::<Visibility>(glint), Some(&Visibility::Hidden));
    }

    #[test]
    fn glint_travels_at_constant_speed_and_disappears_at_the_end() {
        let points = [Vec3::ZERO, Vec3::X, Vec3::new(1.0, 3.0, 0.0)];
        assert_eq!(along(&points, 0.0), Some(Vec3::ZERO));
        assert_eq!(along(&points, GLINT_DURATION * 0.5), Some(Vec3::new(1.0, 1.0, 0.0)));
        assert_eq!(along(&points, GLINT_DURATION), None);
        assert_eq!(along(&points, GLINT_DURATION * 2.0), None);
    }
}
