//! Input overlay — animated floating command palette.
//!
//! The InputOverlay is the sole compose surface, rendered as a floating
//! command palette (rofi/dmenu style) with:
//! - Centered positioning (~60% width, near top-third)
//! - UiVelloScene-rendered background (avoids BackgroundColor/Vello conflict)
//! - Animated borders (Chase/Breathe effects)
//! - Summon animation (fade-in + subtle scale)
//! - Optional glow effect

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy_vello::prelude::{UiVelloScene, UiVelloText, VelloTextAnchor};
use bevy_vello::vello;
use vello::kurbo::{Affine, RoundedRect, Shape, Stroke};
use vello::peniko::Fill;

use crate::cell::{InputOverlay, InputOverlayMarker};
use crate::input::FocusArea;
use crate::text::{bevy_color_to_brush, FontHandles, KjText, KjTextEffects, TextMetrics};
use crate::ui::theme::Theme;
use crate::view::cursor::cursor_row_col;
use crate::view::fieldset::apply_alpha;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Style for the command palette overlay.
#[derive(Component, Reflect, Clone)]
#[reflect(Component)]
pub struct OverlayStyle {
    pub bg_color: Color,
    pub border_color: Color,
    pub border_thickness: f32,
    pub corner_radius: f32,
    pub glow_radius: f32,
    pub glow_intensity: f32,
    pub animation: OverlayAnimation,
}

impl Default for OverlayStyle {
    fn default() -> Self {
        Self {
            bg_color: Color::srgba(0.102, 0.106, 0.149, 0.95),
            border_color: Color::srgb(0.478, 0.635, 0.969),
            border_thickness: 2.0,
            corner_radius: 8.0,
            glow_radius: 6.0,
            glow_intensity: 0.25,
            animation: OverlayAnimation::Breathe,
        }
    }
}

/// Animation style for the overlay border.
#[derive(Default, Clone, Copy, PartialEq, Eq, Reflect, Debug)]
pub enum OverlayAnimation {
    #[default]
    None,
    /// Subtle alpha oscillation
    Breathe,
    /// Running light around border
    Chase,
}

/// Summon animation state.
#[derive(Resource, Default)]
pub struct OverlaySummonState {
    /// Animation progress: 0.0 = hidden, 1.0 = fully visible
    pub progress: f32,
    /// Target visibility state
    pub target_visible: bool,
    /// Whether animation is in progress
    pub animating: bool,
}

// ============================================================================
// SPAWN SYSTEM
// ============================================================================

/// Spawn the singleton InputOverlay entity (root-level, absolute positioned).
///
/// Starts hidden (Visibility::Hidden). Shown/hidden by `sync_overlay_visibility`
/// based on FocusArea::Compose. Uses UiVelloScene for background rendering
/// to avoid the BackgroundColor/Vello compositing conflict.
pub fn spawn_input_overlay(
    mut commands: Commands,
    existing: Query<Entity, With<InputOverlayMarker>>,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    text_metrics: Res<TextMetrics>,
) {
    if !existing.is_empty() {
        return;
    }

    let style = OverlayStyle {
        bg_color: theme.compose_bg.with_alpha(0.95),
        border_color: theme.compose_palette_border,
        border_thickness: 2.0,
        corner_radius: 8.0,
        glow_radius: theme.compose_palette_glow_radius,
        glow_intensity: theme.compose_palette_glow_intensity,
        animation: OverlayAnimation::Breathe,
    };

    commands.spawn((
        InputOverlayMarker,
        InputOverlay::default(),
        style,
        KjText,
        KjTextEffects { rainbow: true },
        UiVelloText {
            value: "[chat] shell | ".to_string(),
            style: bevy_vello::prelude::VelloTextStyle {
                font: font_handles.mono.clone(),
                brush: bevy_color_to_brush(theme.fg_dim),
                font_size: text_metrics.cell_font_size,
                font_axes: bevy_vello::prelude::VelloFontAxes {
                    weight: Some(200.0),
                    ..default()
                },
                ..default()
            },
            ..default()
        },
        // Override default Center anchor — text starts at content-box top-left
        VelloTextAnchor::TopLeft,
        // Scene for background/border rendering (avoids BackgroundColor conflict)
        UiVelloScene::default(),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(15.0),
            left: Val::Percent(20.0),
            right: Val::Percent(20.0),
            min_height: Val::Px(48.0),
            max_width: Val::Px(800.0),
            padding: UiRect::all(Val::Px(16.0)),
            border: UiRect::all(Val::Px(2.0)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..default()
        },
        BorderColor::all(Color::NONE), // Visual border via Vello scene, not Bevy BorderColor
        Visibility::Hidden,
        ZIndex(crate::constants::ZLayer::MODAL),
    ));
    info!("Spawned InputOverlay entity");
}

// ============================================================================
// ANIMATION SYSTEMS
// ============================================================================

/// Update summon animation state when focus changes.
pub fn update_summon_animation(focus: Res<FocusArea>, mut summon: ResMut<OverlaySummonState>) {
    let should_show = matches!(*focus, FocusArea::Compose);
    if summon.target_visible != should_show {
        summon.target_visible = should_show;
        summon.animating = true;
    }
}

/// Interpolate summon progress (fade in/out).
pub fn animate_summon(time: Res<Time>, mut summon: ResMut<OverlaySummonState>) {
    if !summon.animating {
        return;
    }

    let target = if summon.target_visible { 1.0 } else { 0.0 };
    let speed = 8.0; // ~125ms animation
    let delta = time.delta_secs() * speed;

    if summon.progress < target {
        summon.progress = (summon.progress + delta).min(target);
    } else {
        summon.progress = (summon.progress - delta).max(target);
    }

    if (summon.progress - target).abs() < 0.001 {
        summon.progress = target;
        summon.animating = false;
    }
}

/// Show/hide the InputOverlay entity based on summon progress.
///
/// Uses summon.progress to gate visibility so the overlay remains visible
/// during the dismiss animation.
pub fn sync_overlay_visibility(
    summon: Res<OverlaySummonState>,
    mut overlay_query: Query<&mut Visibility, With<InputOverlayMarker>>,
) {
    // Only update when summon state changes
    if !summon.is_changed() {
        return;
    }
    for mut vis in overlay_query.iter_mut() {
        *vis = if summon.progress > 0.0 {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

// ============================================================================
// BUFFER SYNC SYSTEMS
// ============================================================================

/// Sync InputOverlay text to its UiVelloText.
pub fn sync_input_overlay_buffer(
    theme: Res<Theme>,
    mut overlay_query: Query<(&InputOverlay, &mut UiVelloText), Changed<InputOverlay>>,
) {
    for (overlay, mut vello_text) in overlay_query.iter_mut() {
        let display = overlay.display_text();
        let new_brush = if overlay.is_empty() {
            bevy_color_to_brush(theme.fg_dim)
        } else if overlay.is_shell() {
            let validation = crate::kaish::validate(&overlay.text);
            if !validation.valid && !validation.incomplete {
                bevy_color_to_brush(theme.block_tool_error)
            } else {
                bevy_color_to_brush(theme.block_user)
            }
        } else {
            bevy_color_to_brush(theme.block_user)
        };

        if vello_text.style.brush != new_brush {
            vello_text.style.brush = new_brush;
        }
        if vello_text.value != display {
            vello_text.value = display;
        }
    }
}

/// Keep InputOverlay's `max_advance` in sync with its `ComputedNode` width.
///
/// This prevents long compose text from overflowing or wrapping unexpectedly.
/// Runs on `Changed<ComputedNode>` to handle window resizes.
pub fn sync_overlay_max_advance(
    mut overlay_query: Query<
        (&mut UiVelloText, &ComputedNode),
        (With<InputOverlayMarker>, Changed<ComputedNode>),
    >,
) {
    for (mut vello_text, computed_node) in overlay_query.iter_mut() {
        let width = computed_node.size().x;
        if width > 0.0 {
            let new_advance = Some(width);
            if vello_text.max_advance != new_advance {
                vello_text.max_advance = new_advance;
            }
        }
    }
}

/// Sync OverlayStyle from theme when theme changes.
pub fn sync_overlay_style_to_theme(
    theme: Res<Theme>,
    mut overlay_query: Query<&mut OverlayStyle, With<InputOverlayMarker>>,
) {
    if !theme.is_changed() {
        return;
    }
    for mut style in overlay_query.iter_mut() {
        style.bg_color = theme.compose_bg.with_alpha(0.95);
        style.border_color = theme.compose_palette_border;
        style.glow_radius = theme.compose_palette_glow_radius;
        style.glow_intensity = theme.compose_palette_glow_intensity;
    }
}

// ============================================================================
// SCENE RENDERING (PostUpdate)
// ============================================================================

/// Rebuild overlay scene in PostUpdate (after Layout).
///
/// This renders the background, glow, animated border, and cursor using Vello,
/// avoiding the BackgroundColor/Vello compositing conflict.
pub fn update_overlay_scene(
    summon: Res<OverlaySummonState>,
    focus_area: Res<FocusArea>,
    time: Res<Time>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
    mut query: Query<
        (&OverlayStyle, &InputOverlay, &ComputedNode, &mut UiVelloScene),
        With<InputOverlayMarker>,
    >,
) {
    // Only show cursor when in compose mode
    let show_cursor = matches!(*focus_area, FocusArea::Compose);

    for (style, overlay, computed, mut vello_scene) in query.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        // Calculate cursor position (row, col in text)
        let display = overlay.display_text();
        let (row, col) = cursor_row_col(&display, overlay.display_cursor_offset());

        let mut scene = vello::Scene::new();
        build_overlay_panel(
            &mut scene,
            size.x as f64,
            size.y as f64,
            style,
            time.elapsed_secs(),
            summon.progress,
        );

        // Draw cursor if visible
        if show_cursor && summon.progress > 0.0 {
            let char_width = text_metrics.cell_char_width as f64;
            let line_height = text_metrics.cell_line_height as f64;

            // Scene origin appears to be at content-box, not border-box.
            // No padding offsets needed.
            let cursor_x = col as f64 * char_width;
            let cursor_y = row as f64 * line_height;

            draw_cursor_beam(
                &mut scene,
                cursor_x,
                cursor_y,
                line_height,
                theme.cursor_insert,
                summon.progress,
                time.elapsed_secs(),
            );
        }

        *vello_scene = UiVelloScene::from(scene);
    }
}

// ============================================================================
// SCENE BUILDING
// ============================================================================

/// Build the command palette panel scene.
///
/// Draws:
/// 1. Multi-pass glow (if enabled)
/// 2. Filled background (semi-transparent)
/// 3. Border stroke
/// 4. Animation overlay (chase/breathe)
fn build_overlay_panel(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    style: &OverlayStyle,
    time: f32,
    summon_progress: f32,
) {
    let alpha = summon_progress * animation_alpha(&style.animation, time);

    // 1. Draw glow (multi-pass blur approximation)
    if style.glow_radius > 0.0 && style.glow_intensity > 0.0 {
        draw_glow(
            scene,
            width,
            height,
            style.corner_radius as f64,
            style.border_color,
            style.glow_radius,
            style.glow_intensity * alpha,
        );
    }

    // 2. Draw filled background (semi-transparent)
    let bg_brush = apply_alpha(&bevy_color_to_brush(style.bg_color), alpha * 0.95);
    let rect = RoundedRect::new(0.0, 0.0, width, height, style.corner_radius as f64);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &bg_brush, None, &rect);

    // 3. Draw border stroke
    let border_brush = apply_alpha(&bevy_color_to_brush(style.border_color), alpha);
    let half_t = style.border_thickness as f64 / 2.0;
    let border_rect = RoundedRect::new(
        half_t,
        half_t,
        width - half_t,
        height - half_t,
        style.corner_radius as f64,
    );
    let stroke = Stroke::new(style.border_thickness as f64);
    scene.stroke(&stroke, Affine::IDENTITY, &border_brush, None, &border_rect);

    // 4. Chase animation overlay (if active)
    if matches!(style.animation, OverlayAnimation::Chase) {
        draw_chase_overlay(scene, width, height, style, time, alpha);
    }
}

/// Draw glow effect using multi-pass blur approximation.
fn draw_glow(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    corner_radius: f64,
    color: Color,
    radius: f32,
    intensity: f32,
) {
    let passes = 4;
    for i in 0..passes {
        let offset = (i + 1) as f64 * (radius as f64 / passes as f64);
        let a = intensity * (1.0 - i as f32 / passes as f32);
        let brush = apply_alpha(&bevy_color_to_brush(color), a);
        let rect = RoundedRect::new(
            -offset,
            -offset,
            width + offset,
            height + offset,
            corner_radius + offset * 0.5,
        );
        let stroke = Stroke::new(2.0);
        scene.stroke(&stroke, Affine::IDENTITY, &brush, None, &rect);
    }
}

/// Draw chase animation overlay (bright segment traveling along border).
fn draw_chase_overlay(
    scene: &mut vello::Scene,
    width: f64,
    height: f64,
    style: &OverlayStyle,
    time: f32,
    alpha: f32,
) {
    let half_t = style.border_thickness as f64 / 2.0;
    let rect = RoundedRect::new(
        half_t,
        half_t,
        width - half_t,
        height - half_t,
        style.corner_radius as f64,
    );
    let perimeter = rect.perimeter(0.1);

    // Chase segment: ~15% of perimeter, traveling at constant speed
    let chase_len = perimeter * 0.15;
    let position = (time as f64 * 2.0) % perimeter;

    // Bright version of the border color
    let srgba = style.border_color.to_srgba();
    let bright = Color::srgba(
        (srgba.red * 1.5).min(1.0),
        (srgba.green * 1.5).min(1.0),
        (srgba.blue * 1.5).min(1.0),
        (srgba.alpha * 1.8).min(1.0) * alpha,
    );
    let brush = bevy_color_to_brush(bright);

    // Use dashed stroke to create a single bright segment
    let stroke = Stroke::new(style.border_thickness as f64 * 1.5)
        .with_caps(vello::kurbo::Cap::Round)
        .with_dashes(position, &[chase_len, perimeter - chase_len]);

    scene.stroke(&stroke, Affine::IDENTITY, &brush, None, &rect);
}

/// Draw a simple cursor beam (vertical line).
fn draw_cursor_beam(
    scene: &mut vello::Scene,
    x: f64,
    y: f64,
    height: f64,
    color: bevy::math::Vec4,
    alpha: f32,
    _time: f32, // Reserved for future blink animation
) {
    let beam_width = 2.0;
    let beam_color = Color::srgba(color.x, color.y, color.z, color.w * alpha);
    let brush = bevy_color_to_brush(beam_color);

    let rect = vello::kurbo::Rect::new(x, y, x + beam_width, y + height);
    scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect);
}

/// Compute animation alpha multiplier.
fn animation_alpha(animation: &OverlayAnimation, time: f32) -> f32 {
    match animation {
        OverlayAnimation::None => 1.0,
        OverlayAnimation::Chase => 1.0, // chase uses overlay, base alpha stays 1.0
        OverlayAnimation::Breathe => {
            let base = 0.85;
            let amplitude = 0.15;
            base + amplitude * (time * 1.5).sin()
        }
    }
}
