//! Card layout computation.
//!
//! Positions cards in a 3D cascade based on the focused index.
//! Assigns LOD levels. Updates StandardMaterial alpha for distance fade.

use bevy::prelude::*;

use super::sync::StackCard;

/// LOD level for a card — drives opacity and visibility.
#[derive(Component, Reflect, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[reflect(Component)]
pub enum CardLod {
    Focused,
    Near,
    Mid,
    Far,
    #[default]
    Culled,
}

impl CardLod {
    pub fn opacity(self) -> f32 {
        match self {
            Self::Focused => 1.0,
            Self::Near => 0.85,
            Self::Mid => 0.6,
            Self::Far => 0.3,
            Self::Culled => 0.0,
        }
    }
}

/// Global state for the stack view.
#[derive(Resource, Reflect, Debug)]
#[reflect(Resource)]
pub struct CardStackState {
    pub focused_index: usize,
    pub card_count: usize,
    pub scroll_velocity: f32,
}

impl Default for CardStackState {
    fn default() -> Self {
        Self {
            focused_index: 0,
            card_count: 0,
            scroll_velocity: 0.0,
        }
    }
}

impl CardStackState {
    pub fn focus_next(&mut self) {
        if self.card_count > 0 && self.focused_index < self.card_count - 1 {
            self.focused_index += 1;
        }
    }

    pub fn focus_prev(&mut self) {
        if self.focused_index > 0 {
            self.focused_index -= 1;
        }
    }

    pub fn focus_last(&mut self) {
        if self.card_count > 0 {
            self.focused_index = self.card_count - 1;
        }
    }

    pub fn focus_first(&mut self) {
        self.focused_index = 0;
    }
}

/// Tuning knobs for the cascade layout. BRP-reflectable for live tweaking.
#[derive(Resource, Reflect, Debug)]
#[reflect(Resource)]
pub struct CardStackLayout {
    pub cascade_offset: Vec3,
    pub cascade_rotation: f32,
    pub cascade_scale: f32,
    pub max_visible_behind: usize,
    pub max_visible_ahead: usize,
    pub focused_z: f32,
    pub card_width: f32,
}

impl Default for CardStackLayout {
    fn default() -> Self {
        Self {
            cascade_offset: Vec3::new(10.0, -5.0, -40.0),
            cascade_rotation: 0.02,
            cascade_scale: 0.97,
            max_visible_behind: 12,
            max_visible_ahead: 3,
            focused_z: 0.0,
            card_width: 200.0,
        }
    }
}

/// System: compute positions, LOD, and visibility for all cards.
/// Updates child StandardMaterial alpha for LOD fade.
pub fn compute_card_layout(
    state: Res<CardStackState>,
    params: Res<CardStackLayout>,
    mut cards: Query<(
        Entity,
        &StackCard,
        &mut Transform,
        &mut CardLod,
        &mut Visibility,
    )>,
    children_q: Query<&Children>,
    mat_handle_q: Query<&MeshMaterial3d<StandardMaterial>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let focus = state.focused_index as i32;

    for (card_entity, card, mut transform, mut lod, mut vis) in cards.iter_mut() {
        let idx = card.card_index as i32;
        let distance = idx - focus;

        let new_lod = if distance == 0 {
            CardLod::Focused
        } else if distance.unsigned_abs() <= 2 {
            CardLod::Near
        } else if distance.unsigned_abs() <= 5 {
            CardLod::Mid
        } else if (distance < 0 && distance.unsigned_abs() <= params.max_visible_behind as u32)
            || (distance > 0 && distance.unsigned_abs() <= params.max_visible_ahead as u32)
        {
            CardLod::Far
        } else {
            CardLod::Culled
        };

        *lod = new_lod;

        if new_lod == CardLod::Culled {
            *vis = Visibility::Hidden;
            continue;
        }
        *vis = Visibility::Inherited;

        // Cascade position
        let step = if distance <= 0 {
            distance as f32
        } else {
            distance as f32 * 0.4
        };

        transform.translation = Vec3::new(
            params.cascade_offset.x * step,
            params.cascade_offset.y * step,
            params.focused_z + params.cascade_offset.z * step,
        );
        transform.rotation = Quat::from_rotation_y(params.cascade_rotation * step);

        let scale_factor = params.cascade_scale.powi(distance.unsigned_abs() as i32);
        transform.scale = Vec3::splat(params.card_width * scale_factor);

        // Update child quad material alpha for LOD fade
        let opacity = new_lod.opacity();
        if let Ok(children) = children_q.get(card_entity) {
            for child in children.iter() {
                if let Ok(mat_handle) = mat_handle_q.get(child) {
                    if let Some(mat) = materials.get_mut(&mat_handle.0) {
                        mat.base_color.set_alpha(opacity);
                    }
                }
            }
        }
    }
}
