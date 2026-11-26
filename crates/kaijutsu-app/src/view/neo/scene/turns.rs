use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::ui::UiGlobalTransform;

use super::{SceneAssets, TurnHeading};

fn bands(starts: &[f32]) -> Vec<(f32, f32, f32)> {
    let mut result = Vec::with_capacity(starts.len() + 1);
    if starts.is_empty() {
        return vec![(0.0, 1.0, 0.0)];
    }
    result.push((0.0, starts[0], 0.0));
    for (index, &top) in starts.iter().enumerate() {
        let bottom = starts.get(index + 1).copied().unwrap_or(1.0);
        let age = (starts.len() - index).min(3) as f32;
        result.push((top, bottom, age * 0.15));
    }
    result
}

fn vertices(top: f32, bottom: f32, depth: f32) -> [([f32; 3], [f32; 2]); 4] {
    [
        ([-0.5, 0.5 - bottom, -depth], [0.0, bottom]),
        ([0.5, 0.5 - bottom, 0.0], [1.0, bottom]),
        ([0.5, 0.5 - top, 0.0], [1.0, top]),
        ([-0.5, 0.5 - top, -depth], [0.0, top]),
    ]
}

fn mesh(starts: &[f32]) -> Mesh {
    let mut positions = Vec::new();
    let mut uvs = Vec::new();
    let mut indices = Vec::new();
    for (top, bottom, depth) in bands(starts) {
        let base = positions.len() as u32;
        for (position, uv) in vertices(top, bottom, depth) {
            positions.push(position);
            uvs.push(uv);
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

pub(in crate::view::neo) fn sync(
    assets: Res<SceneAssets>,
    nodes: Query<(&ComputedNode, &UiGlobalTransform)>,
    headings: Query<(&ComputedNode, &UiGlobalTransform), With<TurnHeading>>,
    surfaces: Query<(&Mesh3d, &Visibility)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut previous: Local<Option<Vec<f32>>>,
) {
    let Some(card) = assets.cards.iter().find(|card| card.group == 0) else { return };
    let Ok((surface, visibility)) = surfaces.get(card.surface) else { return };
    if *visibility == Visibility::Hidden { return; }
    let Ok((root, transform)) = nodes.get(card.ui) else { return };
    if root.is_empty() { return; }
    let height = root.size().y;
    let root_top = transform.translation.y - height * 0.5;
    let mut starts: Vec<f32> = headings.iter().filter_map(|(node, transform)| {
        if node.is_empty() { return None; }
        let top = (transform.translation.y - node.size().y * 0.5 - root_top) / height;
        (top > 0.0 && top < 1.0).then_some(top)
    }).collect();
    starts.sort_by(f32::total_cmp);
    starts.dedup_by(|a, b| (*a - *b).abs() < 0.0001);
    if previous.as_ref() == Some(&starts) { return; }
    if let Some(mut target) = meshes.get_mut(&surface.0) {
        *target = mesh(&starts);
        *previous = Some(starts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_keep_uvs_and_right_edge_while_older_turns_recede() {
        let bands = bands(&[0.2, 0.5, 0.8]);
        assert_eq!(bands.len(), 4);
        assert_eq!(bands[0], (0.0, 0.2, 0.0));
        assert!(bands[1].2 > bands[2].2 && bands[2].2 > bands[3].2);
        assert!((bands[3].2 - 0.15).abs() < 0.001);
        for (top, bottom, depth) in bands {
            let vertices = vertices(top, bottom, depth);
            assert_eq!(vertices[1].0, [0.5, 0.5 - bottom, 0.0]);
            assert_eq!(vertices[2].0, [0.5, 0.5 - top, 0.0]);
            assert_eq!(vertices[0].1, [0.0, bottom]);
            assert_eq!(vertices[2].1, [1.0, top]);
            assert_eq!(vertices[0].0[2], -depth);
        }
    }

    #[test]
    fn no_turns_keeps_a_flat_complete_surface() {
        assert_eq!(bands(&[]), vec![(0.0, 1.0, 0.0)]);
    }
}
