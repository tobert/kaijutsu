//! Card ↔ Block synchronization.
//!
//! Reads blocks from CellEditor, groups consecutive blocks by role into
//! "cards", and spawns/despawns 3D card entities. Each card is a parent
//! entity with child quad meshes — one per block — sharing the block's
//! existing RTT texture handle via StandardMaterial.

use bevy::prelude::*;

use kaijutsu_crdt::Role;

use crate::cell::{BlockCell, BlockCellContainer, BlockId, CellEditor, MainCell};
use crate::view::block_render::{BlockScene, BlockTexture};
use crate::ui::card_stack::layout::{CardLod, CardStackState};
use crate::ui::card_stack::material::{StackCardMaterial, StackCardUniforms};

/// Marker on a card parent entity (one per role-group).
#[derive(Component, Reflect, Debug)]
#[reflect(Component)]
pub struct StackCard {
    #[reflect(ignore)]
    pub block_ids: Vec<BlockId>,
    #[reflect(ignore)]
    #[allow(dead_code)]
    pub role: Role,
    pub card_index: u32,
}

/// Marker on a child quad within a card (one per block).
#[derive(Component, Debug)]
#[allow(dead_code)]
pub struct CardBlockQuad {
    pub block_id: BlockId,
}

/// Marker for the root entity that parents all card entities.
#[derive(Component, Reflect, Debug)]
#[reflect(Component)]
pub struct CardStackRoot;

struct CardGroup {
    role: Role,
    block_ids: Vec<BlockId>,
}

fn group_blocks_into_cards(editor: &CellEditor) -> Vec<CardGroup> {
    let mut groups: Vec<CardGroup> = Vec::new();
    for snap in editor.store.blocks_ordered() {
        let role = snap.role;
        match groups.last_mut() {
            Some(g) if g.role == role => {
                g.block_ids.push(snap.id);
            }
            _ => {
                groups.push(CardGroup {
                    role,
                    block_ids: vec![snap.id],
                });
            }
        }
    }
    groups
}

/// System: sync card entities to match current conversation blocks.
pub fn sync_stack_cards(
    mut commands: Commands,
    editor_q: Query<&CellEditor, With<MainCell>>,
    container_q: Query<&BlockCellContainer>,
    block_q: Query<(&BlockCell, &BlockTexture, &BlockScene)>,
    existing_cards: Query<(Entity, &StackCard)>,
    root_q: Query<Entity, With<CardStackRoot>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StackCardMaterial>>,
    mut stack_state: ResMut<CardStackState>,
) {
    let Ok(editor) = editor_q.single() else {
        return;
    };

    let groups = group_blocks_into_cards(editor);
    let new_count = groups.len();

    // Find or spawn root
    let root = if let Ok(root) = root_q.single() {
        root
    } else {
        commands
            .spawn((CardStackRoot, Transform::default(), Visibility::Inherited))
            .id()
    };

    // Skip rebuild if card structure hasn't changed
    let existing: Vec<(Entity, &StackCard)> = existing_cards.iter().collect();
    let needs_rebuild = existing.len() != new_count
        || existing.iter().zip(groups.iter()).any(|((_, card), group)| {
            card.block_ids.first() != group.block_ids.first()
                || card.block_ids.last() != group.block_ids.last()
                || card.block_ids.len() != group.block_ids.len()
        });

    if !needs_rebuild {
        stack_state.card_count = new_count;
        return;
    }

    // Despawn existing
    for (entity, _) in &existing {
        commands.entity(*entity).despawn();
    }

    let container = container_q.iter().next();
    let card_quad = meshes.add(Plane3d::new(Vec3::Z, Vec2::new(0.5, 0.5)));

    for (idx, group) in groups.iter().enumerate() {
        let glow_color = role_glow_linear(group.role);

        let card_entity = commands
            .spawn((
                StackCard {
                    block_ids: group.block_ids.clone(),
                    role: group.role,
                    card_index: idx as u32,
                },
                CardLod::Culled,
                Transform::from_xyz(0.0, 0.0, -1000.0),
                Visibility::Inherited,
            ))
            .id();
        commands.entity(root).add_child(card_entity);

        // Spawn a child quad for each block
        // Track bottom edge of last placed block for tight stacking
        let mut y_cursor = 0.0_f32;

        for (block_idx, block_id) in group.block_ids.iter().enumerate() {
            let texture_data = container
                .and_then(|c| c.get_entity(block_id))
                .and_then(|ent| block_q.get(ent).ok());

            let Some((_, tex, scene)) = texture_data else {
                continue; // Skip blocks without textures (they might be still rendering)
            };

            let texture_handle = tex.image.clone();
            let (built_w, built_h) = (scene.built_width, scene.built_height);

            let mat = materials.add(StackCardMaterial {
                texture: texture_handle,
                uniforms: StackCardUniforms {
                    card_params: Vec4::new(1.0, 0.0, 0.0, 0.0),
                    glow_color: glow_color.to_linear().to_vec4(),
                    glow_params: Vec4::new(0.5, 0.0, 0.0, 0.0), // 0.5 glow intensity
                },
            });

            let aspect = if built_w > 0.0 { built_h / built_w } else { 0.1 };
            let quad_width = 1.0;
            let quad_height = quad_width * aspect;

            // Stack tightly: first block at y=0, each subsequent touches the one above
            let y_offset = if block_idx == 0 {
                y_cursor = -quad_height / 2.0;
                0.0
            } else {
                let center = y_cursor - quad_height / 2.0;
                y_cursor = center - quad_height / 2.0;
                center
            };

            let quad_entity = commands
                .spawn((
                    CardBlockQuad { block_id: *block_id },
                    Mesh3d(card_quad.clone()),
                    MeshMaterial3d(mat),
                    Transform::from_xyz(0.0, y_offset, 0.0)
                        .with_scale(Vec3::new(quad_width, quad_height, 1.0)),
                ))
                .id();
            commands.entity(card_entity).add_child(quad_entity);
        }
    }

    stack_state.card_count = new_count;
    if stack_state.focused_index >= new_count && new_count > 0 {
        stack_state.focused_index = new_count - 1;
    }
}

/// Despawn all card entities.
pub fn despawn_all_cards(
    mut commands: Commands,
    root_q: Query<Entity, With<CardStackRoot>>,
) {
    for root in root_q.iter() {
        commands.entity(root).despawn();
    }
}

/// Map role → Color (linear, for StandardMaterial base_color).
pub(crate) fn role_glow_linear(role: Role) -> Color {
    match role {
        Role::User => Color::srgb(0.2, 0.85, 0.95),
        Role::Model => Color::srgb(0.65, 0.35, 1.0),
        Role::Tool => Color::srgb(1.0, 0.7, 0.15),
        Role::System => Color::srgb(0.4, 0.45, 0.55),
        Role::Asset => Color::srgb(0.3, 0.7, 0.65),
    }
}
