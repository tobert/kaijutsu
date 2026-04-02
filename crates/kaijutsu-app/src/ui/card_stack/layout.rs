//! Card layout computation.
//!
//! Positions cards in a 3D cascade based on the focused index.
//! Assigns LOD levels. Updates StandardMaterial alpha for distance fade.

use bevy::prelude::*;

use super::sync::StackCard;
use super::material::StackCardMaterial;

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
    /// Target focus index (integer)
    pub focused_index: usize,
    /// Smoothly interpolated current focus (fractional)
    pub current_focus: f32,
    /// Previous focus for velocity calculation
    pub last_focus: f32,
    pub card_count: usize,
    /// Rate of change of current_focus
    pub scroll_velocity: f32,
}

impl Default for CardStackState {
    fn default() -> Self {
        Self {
            focused_index: 0,
            current_focus: 0.0,
            last_focus: 0.0,
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
    pub smooth_speed: f32,
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
            smooth_speed: 10.0,
        }
    }
}

/// System: interpolate current_focus toward focused_index.
pub fn interpolate_stack_focus(
    mut state: ResMut<CardStackState>,
    params: Res<CardStackLayout>,
    time: Res<Time>,
) {
    let dt = time.delta_secs();
    if dt < 0.0001 { return; }

    state.last_focus = state.current_focus;
    let target = state.focused_index as f32;
    let diff = target - state.current_focus;
    
    if diff.abs() > 0.001 {
        let t = (params.smooth_speed * dt).min(1.0);
        state.current_focus += diff * t;
    } else {
        state.current_focus = target;
    }

    state.scroll_velocity = (state.current_focus - state.last_focus) / dt;
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
    mat_handle_q: Query<&MeshMaterial3d<StackCardMaterial>>,
    mut materials: ResMut<Assets<StackCardMaterial>>,
) {
    let focus = state.current_focus;

    for (card_entity, card, mut transform, mut lod, mut vis) in cards.iter_mut() {
        let idx = card.card_index as f32;
        let distance = idx - focus;
        let abs_dist = distance.abs();

        let new_lod = if abs_dist < 0.5 {
            CardLod::Focused
        } else if abs_dist <= 2.5 {
            CardLod::Near
        } else if abs_dist <= 5.5 {
            CardLod::Mid
        } else if (distance < 0.0 && abs_dist <= params.max_visible_behind as f32)
            || (distance > 0.0 && abs_dist <= params.max_visible_ahead as f32)
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
        let step = if distance <= 0.0 {
            distance
        } else {
            distance * 0.4
        };

        transform.translation = Vec3::new(
            params.cascade_offset.x * step,
            params.cascade_offset.y * step,
            params.focused_z + params.cascade_offset.z * step,
        );

        // Add a "lean" based on scroll velocity and a "tilt" based on distance
        let velocity_lean = state.scroll_velocity * 0.02;
        let distance_tilt = distance * 0.05;
        
        transform.rotation = Quat::from_rotation_y(params.cascade_rotation * step + velocity_lean)
            * Quat::from_rotation_z(velocity_lean * 0.5)
            * Quat::from_rotation_x(distance_tilt.min(0.2));

        let scale_factor = params.cascade_scale.powf(abs_dist);
        transform.scale = Vec3::splat(params.card_width * scale_factor);

        // Update child quad material alpha for LOD fade
        // For smooth focus, we calculate opacity from fractional distance
        let opacity = if abs_dist < 1.0 {
            1.0
        } else {
            new_lod.opacity().max(0.1)
        };

        if let Ok(children) = children_q.get(card_entity) {
            for child in children.iter() {
                if let Ok(mat_handle) = mat_handle_q.get(child) {
                    if let Some(mat) = materials.get_mut(&mat_handle.0) {
                        mat.uniforms.card_params.x = opacity;
                    }
                }
            }
        }
    }
}
