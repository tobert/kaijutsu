//! Card layout computation.
//!
//! Positions cards in a 3D cascade based on the focused index.
//! Assigns LOD levels. Updates StandardMaterial alpha for distance fade.

use bevy::prelude::*;

use super::sync::StackCard;
use super::material::{StackCardMaterial, StackCardUniforms};

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
    // ── Reading mode ──
    /// Z-position of the expanded reading card.
    pub reading_card_z: f32,
    /// Y-offset to raise the reading card above center (room for strip below).
    pub reading_card_y: f32,
    /// Viewport fill fraction for the reading card (0.0–1.0, default 0.92).
    pub reading_card_fill: f32,
    /// Scroll step per j/k press in reading mode (world units).
    pub reading_scroll_step: f32,
    /// Y-position of the compressed strip.
    pub strip_y: f32,
    /// Z-position of the compressed strip.
    pub strip_z: f32,
    /// X-spacing between cards in the strip.
    pub strip_spacing: f32,
    /// Scale of strip cards (relative to card_width).
    pub strip_scale: f32,
    /// Forward tilt of strip cards in radians.
    pub strip_tilt: f32,
    /// Transition animation speed (units/sec).
    pub reading_speed: f32,
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
            // Reading mode
            reading_card_z: 30.0,
            reading_card_y: 10.0,
            reading_card_fill: 0.92,
            reading_scroll_step: 15.0,
            strip_y: -50.0,
            strip_z: -10.0,
            strip_spacing: 1.2,
            strip_scale: 0.18,
            strip_tilt: 0.7,
            reading_speed: 5.0,
        }
    }
}

/// Animation phase for the stack view entry transition.
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Resource)]
pub enum StackAnimPhase {
    /// Cards are spreading out from a collapsed point.
    Entering { progress: f32 },
    /// Normal operation.
    Active,
}

impl Default for StackAnimPhase {
    fn default() -> Self {
        Self::Active
    }
}

/// View mode for the card stack — Browse (cascade) or Reading (expanded card).
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource)]
pub enum StackViewMode {
    /// Normal cascade browsing.
    Browse,
    /// Reading a specific card — expanded, with compressed stack strip below.
    Reading {
        /// Index of the card being read (its position in the strip gap).
        source_index: usize,
    },
}

impl Default for StackViewMode {
    fn default() -> Self {
        Self::Browse
    }
}

/// Transition state for the Browse ↔ Reading animation.
/// `progress`: 0.0 = fully browse, 1.0 = fully reading.
#[derive(Resource, Reflect, Debug)]
#[reflect(Resource)]
pub struct ReadingTransition {
    pub progress: f32,
    pub target: f32,
    /// Which card index is (or was) pulled out for reading.
    /// Retained during exit animation so the gap marker animates out.
    pub source_index: usize,
    /// Vertical scroll offset in world units (positive = scrolled down).
    pub scroll_offset: f32,
}

impl Default for ReadingTransition {
    fn default() -> Self {
        Self {
            progress: 0.0,
            target: 0.0,
            source_index: 0,
            scroll_offset: 0.0,
        }
    }
}

/// Marker on the sparkly gap placeholder entity in the compressed strip.
#[derive(Component, Reflect, Debug)]
#[reflect(Component)]
pub struct GapMarker;

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn smoothstep_f32(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// System: advance the entry animation timer.
pub fn tick_stack_anim(
    mut phase: ResMut<StackAnimPhase>,
    time: Res<Time>,
) {
    let dt = time.delta_secs();
    let speed = 3.3; // ~0.3s for full transition
    if let StackAnimPhase::Entering { ref mut progress } = *phase {
        *progress = (*progress + dt * speed).min(1.0);
        if *progress >= 1.0 {
            *phase = StackAnimPhase::Active;
        }
    }
}

/// System: advance the browse ↔ reading transition.
pub fn tick_reading_transition(
    view_mode: Res<StackViewMode>,
    mut transition: ResMut<ReadingTransition>,
    state: Res<CardStackState>,
    params: Res<CardStackLayout>,
    time: Res<Time>,
) {
    match *view_mode {
        StackViewMode::Browse => {
            transition.target = 0.0;
            transition.scroll_offset = 0.0;
        }
        StackViewMode::Reading { source_index } => {
            transition.target = 1.0;
            transition.source_index = source_index;
            // Safety: if card count shrank past our source, exit reading
            if source_index >= state.card_count && state.card_count > 0 {
                transition.target = 0.0;
            }
        }
    }

    let dt = time.delta_secs();
    let diff = transition.target - transition.progress;
    if diff.abs() > 0.001 {
        let t = (params.reading_speed * dt).min(1.0);
        transition.progress += diff * t;
    } else {
        transition.progress = transition.target;
    }
}

/// System: spawn/despawn the gap sparkle marker entity.
pub fn manage_gap_marker(
    mut commands: Commands,
    transition: Res<ReadingTransition>,
    gap_markers: Query<Entity, With<GapMarker>>,
    cards: Query<&StackCard>,
    root_q: Query<Entity, With<super::sync::CardStackRoot>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StackCardMaterial>>,
) {
    let should_exist = transition.progress > 0.001 || transition.target > 0.0;
    let exists = !gap_markers.is_empty();

    if should_exist && !exists {
        // Find role color of the source card
        let glow_color = cards
            .iter()
            .find(|c| c.card_index as usize == transition.source_index)
            .map(|c| super::sync::role_glow_linear(c.role))
            .unwrap_or(Color::srgb(0.5, 0.5, 0.5));

        let quad = meshes.add(Plane3d::new(Vec3::Z, Vec2::new(0.5, 0.5)));
        let mat = materials.add(StackCardMaterial {
            texture: Handle::default(),
            uniforms: StackCardUniforms {
                // z = 1.0 → gap sparkle render mode
                card_params: Vec4::new(0.0, 0.0, 1.0, 0.0),
                glow_color: glow_color.to_linear().to_vec4(),
                glow_params: Vec4::new(0.8, 0.0, 0.0, 0.0),
            },
        });

        let entity = commands
            .spawn((
                GapMarker,
                Mesh3d(quad),
                MeshMaterial3d(mat),
                Transform::from_xyz(0.0, 0.0, 0.0).with_scale(Vec3::splat(0.01)),
            ))
            .id();

        if let Ok(root) = root_q.single() {
            commands.entity(root).add_child(entity);
        }
    } else if !should_exist && exists {
        for entity in gap_markers.iter() {
            commands.entity(entity).despawn();
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
///
/// In Browse mode: existing cascade layout.
/// In Reading mode (or transitioning): lerps each card between its browse
/// position and its reading position (expanded card or compressed strip).
pub fn compute_card_layout(
    state: Res<CardStackState>,
    params: Res<CardStackLayout>,
    anim_phase: Res<StackAnimPhase>,
    transition: Res<ReadingTransition>,
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
    mut gap_q: Query<
        (&mut Transform, &MeshMaterial3d<StackCardMaterial>),
        (With<GapMarker>, Without<StackCard>),
    >,
    camera_q: Query<&GlobalTransform, With<super::camera::StackCameraTag>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
) {
    let focus = state.current_focus;
    let anim_t = match *anim_phase {
        StackAnimPhase::Entering { progress } => ease_out_cubic(progress),
        StackAnimPhase::Active => 1.0,
    };
    let rt = transition.progress;
    let source_idx = transition.source_index;

    // Compute viewport-fit reading scale and clip plane from camera geometry
    let camera_z = camera_q.iter().next().map(|t| t.translation().z).unwrap_or(180.0);
    let camera_y = camera_q.iter().next().map(|t| t.translation().y).unwrap_or(20.0);
    let aspect = windows.iter().next().map(|w| w.width() / w.height()).unwrap_or(1.6);
    let fov = std::f32::consts::FRAC_PI_4;
    let dist_card = camera_z - params.reading_card_z;
    let half_h = dist_card * (fov / 2.0).tan();
    let reading_scale = 2.0 * half_h * aspect * params.reading_card_fill;

    // Clip plane: project strip top from strip_z to reading_card_z plane
    let strip_top_world = params.strip_y + params.card_width * params.strip_scale * 0.5;
    let dist_strip = camera_z - params.strip_z;
    let reading_clip_y = camera_y + (strip_top_world - camera_y) * (dist_card / dist_strip);

    for (card_entity, card, mut transform, mut lod, mut vis) in cards.iter_mut() {
        let idx = card.card_index as f32;
        let card_idx = card.card_index as usize;
        let distance = idx - focus;
        let abs_dist = distance.abs();

        // ── Browse LOD ──
        let browse_lod = if abs_dist < 0.5 {
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

        // Cull cards not visible in current mode
        let strip_dist = (card_idx as f32 - source_idx as f32).abs();
        let strip_max = params.max_visible_behind.max(params.max_visible_ahead) as f32;
        if rt < 0.001 && browse_lod == CardLod::Culled {
            *lod = CardLod::Culled;
            *vis = Visibility::Hidden;
            continue;
        }
        // In reading mode, cull strip cards too far from source
        if rt > 0.5 && card_idx != source_idx && strip_dist > strip_max {
            *lod = CardLod::Culled;
            *vis = Visibility::Hidden;
            continue;
        }

        *vis = Visibility::Inherited;
        *lod = if rt > 0.5 {
            if card_idx == source_idx {
                CardLod::Focused
            } else {
                CardLod::Far
            }
        } else {
            browse_lod
        };

        // ── Browse position (existing cascade logic) ──
        let step = if distance <= 0.0 {
            distance
        } else {
            distance * 0.4
        };
        let focus_blend = (1.0 - abs_dist * 2.0).clamp(0.0, 1.0);

        let browse_pos = Vec3::new(
            params.cascade_offset.x * step,
            params.cascade_offset.y * step,
            params.focused_z + params.cascade_offset.z * step + focus_blend * 5.0,
        );

        let velocity_lean = state.scroll_velocity * 0.02;
        let distance_tilt = distance * 0.05;
        let browse_rot =
            Quat::from_rotation_y(params.cascade_rotation * step + velocity_lean)
                * Quat::from_rotation_z(velocity_lean * 0.5)
                * Quat::from_rotation_x(distance_tilt.min(0.2));

        let scale_factor = params.cascade_scale.powf(abs_dist);
        let browse_scale =
            params.card_width * scale_factor * (1.0 + focus_blend * 0.03) * anim_t.max(0.01);

        // ── Browse material values ──
        let browse_opacity =
            if abs_dist < 1.0 { 1.0 } else { browse_lod.opacity().max(0.1) } * anim_t;
        let browse_glow = 0.5 + focus_blend * 0.4;
        let browse_lod_f = smoothstep_f32(0.5, 8.0, abs_dist);

        if rt < 0.001 {
            // ── Pure browse mode ──
            transform.translation = browse_pos;
            transform.rotation = browse_rot;
            transform.scale = Vec3::splat(browse_scale);

            if anim_t < 1.0 {
                let collapse = Vec3::new(0.0, 0.0, params.focused_z);
                transform.translation = collapse.lerp(transform.translation, anim_t);
            }

            update_card_materials(
                card_entity,
                browse_opacity,
                browse_lod_f,
                browse_glow,
                f32::MIN, // no clip in browse mode
                &children_q,
                &mat_handle_q,
                &mut materials,
            );
        } else {
            // ── Blend toward reading layout ──
            let (read_pos, read_rot, read_scale_val, read_opacity, read_glow, read_lod_f) =
                if card_idx == source_idx {
                    // This card rises to expanded reading position
                    (
                        Vec3::new(0.0, params.reading_card_y + transition.scroll_offset, params.reading_card_z),
                        Quat::IDENTITY,
                        reading_scale,
                        1.0_f32,
                        0.9_f32,
                        0.0_f32,
                    )
                } else {
                    // This card drops to the compressed strip
                    // Centered on source_index so the gap is always mid-strip
                    let relative = card_idx as f32 - source_idx as f32;

                    (
                        Vec3::new(
                            relative * params.strip_spacing,
                            params.strip_y,
                            params.strip_z + relative.abs() * -0.5,
                        ),
                        Quat::from_rotation_x(params.strip_tilt),
                        params.card_width * params.strip_scale,
                        0.7_f32,
                        0.3_f32,
                        0.85_f32,
                    )
                };

            let eased = ease_out_cubic(rt);
            transform.translation = browse_pos.lerp(read_pos, eased);
            transform.rotation = browse_rot.slerp(read_rot, eased);
            transform.scale =
                Vec3::splat(browse_scale + (read_scale_val - browse_scale) * eased);

            let opacity = browse_opacity + (read_opacity - browse_opacity) * eased;
            let glow = browse_glow + (read_glow - browse_glow) * eased;
            let lod_f = browse_lod_f + (read_lod_f - browse_lod_f) * eased;

            // Clip reading card below strip; no clip for strip cards
            let clip = if card_idx == source_idx {
                reading_clip_y
            } else {
                f32::MIN
            };

            update_card_materials(
                card_entity,
                opacity,
                lod_f,
                glow,
                clip,
                &children_q,
                &mat_handle_q,
                &mut materials,
            );
        }
    }

    // ── Position gap marker in the strip ──
    if rt > 0.001 {
        // Gap is at relative=0 (center of strip, since strip is centered on source)
        let gap_strip_pos = Vec3::new(
            0.0,
            params.strip_y,
            params.strip_z + 0.1,
        );
        let gap_strip_rot = Quat::from_rotation_x(params.strip_tilt);
        let gap_strip_scale = params.card_width * params.strip_scale;
        let eased = ease_out_cubic(rt);

        for (mut gap_tf, gap_mat_h) in gap_q.iter_mut() {
            // Animate from the focused card's browse position into the strip
            let gap_start = Vec3::new(0.0, 0.0, params.focused_z);
            gap_tf.translation = gap_start.lerp(gap_strip_pos, eased);
            gap_tf.rotation = Quat::IDENTITY.slerp(gap_strip_rot, eased);
            gap_tf.scale = Vec3::splat(gap_strip_scale * eased.max(0.01));

            if let Some(mat) = materials.get_mut(&gap_mat_h.0) {
                mat.uniforms.card_params.x = rt * 0.8;
            }
        }
    }
}

/// Helper: update child quad StackCardMaterial uniforms.
/// `clip_y`: world-space Y below which fragments are discarded (f32::MIN = no clip).
fn update_card_materials(
    card_entity: Entity,
    opacity: f32,
    lod_factor: f32,
    glow_intensity: f32,
    clip_y: f32,
    children_q: &Query<&Children>,
    mat_handle_q: &Query<&MeshMaterial3d<StackCardMaterial>>,
    materials: &mut Assets<StackCardMaterial>,
) {
    if let Ok(children) = children_q.get(card_entity) {
        for child in children.iter() {
            if let Ok(mat_handle) = mat_handle_q.get(child) {
                if let Some(mat) = materials.get_mut(&mat_handle.0) {
                    mat.uniforms.card_params.x = opacity;
                    mat.uniforms.card_params.y = lod_factor;
                    mat.uniforms.card_params.w = clip_y;
                    mat.uniforms.glow_params.x = glow_intensity;
                }
            }
        }
    }
}
