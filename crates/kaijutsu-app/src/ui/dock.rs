//! Vello-drawn dock bars (North + South).
//!
//! Each dock is a single Bevy entity with `UiVectorScene` + `UiRttTexture` +
//! `ImageNode` (the kaijutsu-owned vello→texture primitive). All text is drawn
//! directly into the Vello scene — no child entities, no flex layout for widgets.
//!
//! `DockState` resource holds all widget data. Data-gathering systems write to
//! `DockState` fields; render systems read `DockState` + `ComputedNode` and
//! rebuild the Vello scene each frame the data changes.

use std::collections::VecDeque;

use bevy::prelude::*;
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::view::block_render::GpuTextureLimits;
use crate::view::ui_rtt::{
    UiVectorScene, UiRttTexture, create_ui_rtt_texture, logical_size, ui_rtt_texture_dims,
};
use vello::kurbo::Affine;
use vello::peniko::Fill;

use crate::cell::ContextSwitchRequested;
use crate::connection::RpcConnectionState;
use crate::connection::actor_plugin::ServerEventMessage;
use crate::input::FocusArea;
use crate::text::sparkline::{SparklineColors, SparklineData, build_sparkline_paths};

/// A dock sparkline — ring-buffer time series with fixed capacity.
#[derive(Clone, Debug)]
pub struct DockSparkline {
    pub data: SparklineData,
    capacity: usize,
}

impl DockSparkline {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: SparklineData {
                values: Vec::with_capacity(capacity),
                label: None,
            },
            capacity,
        }
    }

    /// Push a new sample, evicting the oldest if at capacity.
    pub fn push(&mut self, value: f64) {
        if self.data.values.len() >= self.capacity {
            self.data.values.remove(0);
        }
        self.data.values.push(value);
    }
}
use crate::text::{ShapingFonts, bevy_color_to_brush};
use crate::ui::drift::DriftState;
use crate::ui::theme::Theme;

// ============================================================================
// TYPES
// ============================================================================

/// A single text item to draw in a dock.
#[derive(Debug, Clone)]
pub struct DockText {
    pub text: String,
    pub color: Color,
    pub font_size: f32,
}

/// Badge data for a context in the context strip.
#[derive(Debug, Clone)]
pub struct ContextBadgeData {
    pub context_id: kaijutsu_types::ContextId,
    pub label: String,
    pub is_active: bool,
}

/// Context strip state for the South dock.
#[derive(Debug, Clone, Default)]
pub struct ContextsState {
    pub badges: Vec<ContextBadgeData>,
    pub overflow_count: usize,
    pub staged_count: usize,
    pub notification: Option<(String, String)>, // (source_ctx, preview)
}

/// All dock widget data — the single resource driving both dock renders.
#[derive(Resource)]
pub struct DockState {
    // North dock
    pub title: DockText,
    pub event_pulse: DockText,
    pub connection: DockText,

    // North dock sparklines
    pub event_spark: DockSparkline,
    pub activity_spark: DockSparkline,

    // South dock
    pub mode: DockText,
    pub model_badge: DockText,
    pub context_usage: DockText,
    pub agent_activity: DockText,
    pub block_activity: DockText,
    /// Ambient visibility into `background_exec.rs`'s host-process registry
    /// for the active context — "is anything backgrounded still running,
    /// and how did the last one end" (docs/issues.md "Background shell +
    /// process management"). Empty text = nothing running and nothing ever
    /// finished, same "hidden when idle" convention as `block_activity`.
    pub background_jobs: DockText,
    pub hints: DockText,
    pub contexts: ContextsState,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            title: DockText {
                text: "会術 Kaijutsu".into(),
                color: Color::WHITE, // overridden by theme in render
                font_size: 26.0,
            },
            event_pulse: DockText {
                text: "quiet".into(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            connection: DockText {
                text: "Connecting...".into(),
                color: Color::WHITE,
                font_size: 16.0,
            },
            event_spark: DockSparkline::new(40),
            activity_spark: DockSparkline::new(40),
            mode: DockText {
                text: "INPUT".into(),
                color: Color::WHITE,
                font_size: 16.0,
            },
            model_badge: DockText {
                text: "—".into(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            context_usage: DockText {
                text: "—".into(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            agent_activity: DockText {
                text: String::new(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            block_activity: DockText {
                text: String::new(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            background_jobs: DockText {
                text: String::new(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            hints: DockText {
                text: "Enter: submit │ Shift+Enter: newline │ Esc: normal".into(),
                color: Color::WHITE,
                font_size: 13.0,
            },
            contexts: ContextsState::default(),
        }
    }
}

/// Click hit regions for the South dock (context badges).
#[derive(Resource, Default)]
pub struct DockHitRegions {
    /// (x_min, x_max, context_id) in dock-local coordinates.
    pub south_regions: Vec<(f32, f32, kaijutsu_types::ContextId)>,
}

/// Marker for the North dock entity.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct NorthDock;

/// Marker for the South dock entity.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct SouthDock;

// ============================================================================
// TEXT DRAWING HELPERS
// ============================================================================

/// Draw text into a Vello scene at (x, y) and return the advance width.
///
/// `y` is the top of the text area — baseline is offset by font metrics.
fn draw_dock_text(
    scene: &mut vello::Scene,
    text: &str,
    x: f64,
    y: f64,
    font_size: f32,
    font: &VelloFont,
    brush: &vello::peniko::Brush,
) -> f64 {
    if text.is_empty() {
        return 0.0;
    }

    let style = VelloTextStyle {
        font_size,
        ..default()
    };

    let layout = font.layout(text, &style, VelloTextAlign::Left, None);
    let transform = Affine::translate((x, y));

    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let mut gx = glyph_run.offset();
            let gy = glyph_run.baseline();
            let run = glyph_run.run();
            let run_font = run.font();
            let run_font_size = run.font_size();

            scene
                .draw_glyphs(run_font)
                .brush(brush)
                .hint(true)
                .transform(transform)
                .font_size(run_font_size)
                .normalized_coords(run.normalized_coords())
                .draw(
                    Fill::NonZero,
                    glyph_run.glyphs().map(|glyph| {
                        let px = gx + glyph.x;
                        let py = gy - glyph.y;
                        gx += glyph.advance;
                        vello::Glyph {
                            id: glyph.id as _,
                            x: px,
                            y: py,
                        }
                    }),
                );
        }
    }

    layout.width() as f64
}

/// Measure text width without drawing.
fn measure_text(text: &str, font_size: f32, font: &VelloFont) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let style = VelloTextStyle {
        font_size,
        ..default()
    };
    let layout = font.layout(text, &style, VelloTextAlign::Left, None);
    layout.width() as f64
}

/// Measure text width, falling back to a heuristic if no font is available.
#[allow(dead_code)] // Available for use when font hasn't loaded yet
fn measure_text_or_heuristic(text: &str, font_size: f32, font: Option<&VelloFont>) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    if let Some(f) = font {
        measure_text(text, font_size, f)
    } else {
        // Heuristic: monospace at given size ≈ 0.6 * font_size per char
        text.len() as f64 * font_size as f64 * 0.6
    }
}

/// Draw a sparkline at (x, y) in a Vello scene.
///
/// Builds paths from `data` and strokes/fills with `line_color` and a fill at `fill_alpha`.
fn draw_sparkline_at(
    scene: &mut vello::Scene,
    data: &SparklineData,
    width: f64,
    height: f64,
    x: f64,
    y: f64,
    line_color: Color,
    fill_alpha: f32,
) {
    use vello::kurbo::{Cap, Join, Stroke};

    let colors = SparklineColors {
        line: line_color,
        fill: Some(line_color.with_alpha(fill_alpha)),
    };
    let paths = build_sparkline_paths(data, width, height, 2.0);
    let transform = Affine::translate((x, y));

    let line_brush = bevy_color_to_brush(colors.line);
    let stroke = Stroke {
        width: 1.5,
        join: Join::Round,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        ..Default::default()
    };

    if let (Some(fill_path), Some(fill_color)) = (&paths.fill, &colors.fill) {
        let fill_brush = bevy_color_to_brush(*fill_color);
        scene.fill(Fill::NonZero, transform, &fill_brush, None, fill_path);
    }
    scene.stroke(&stroke, transform, &line_brush, None, &paths.line);
}

// ============================================================================
// STARTUP SYSTEM
// ============================================================================

/// Spawn the two dock entities as children of TilingRoot.
pub fn spawn_docks(
    mut commands: Commands,
    theme: Res<Theme>,
    tiling_root: Query<Entity, With<super::tiling_reconciler::TilingRoot>>,
) {
    let Ok(root) = tiling_root.single() else {
        return;
    };

    // North dock — inserted at index 0 (before ContentArea)
    let north = commands
        .spawn((
            NorthDock,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(40.0),
                ..default()
            },
            BorderColor::all(theme.border),
            ImageNode::default(),
            UiVectorScene::default(),
            UiRttTexture::default(),
            GlobalZIndex(crate::constants::ZLayer::HUD),
        ))
        .id();
    commands.entity(root).insert_children(0, &[north]);

    // South dock — appended (after ContentArea)
    let south = commands
        .spawn((
            SouthDock,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(32.0),
                border: UiRect::top(Val::Px(1.0)),
                ..default()
            },
            BorderColor::all(theme.border),
            ImageNode::default(),
            UiVectorScene::default(),
            UiRttTexture::default(),
            GlobalZIndex(crate::constants::ZLayer::HUD),
        ))
        .id();
    commands.entity(root).add_child(south);
}

// ============================================================================
// RENDER SYSTEMS (PostUpdate, after Layout)
// ============================================================================

/// Render the North dock scene: title (left), pulse + connection (right).
pub fn render_north_dock(
    dock_state: Res<DockState>,
    theme: Res<Theme>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut query: Query<(&mut UiVectorScene, &mut UiRttTexture, &ComputedNode), With<NorthDock>>,
) {
    let Ok((mut scene_comp, mut rtt, computed)) = query.single_mut() else {
        return;
    };

    // Rebuild on data/theme change or when the dock changed width (right-aligned
    // groups must reflow; a stale-width scene would otherwise stretch onto the
    // resized texture).
    // ComputedNode is physical px; the dock scene builds in logical.
    let logical_width = logical_size(computed).x;
    let width_changed = (rtt.built_width - logical_width).abs() > 0.5;
    if !dock_state.is_changed() && !theme.is_changed() && !width_changed {
        return;
    }

    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    let mut scene = vello::Scene::new();
    let width = logical_width as f64;

    // Insets: 16px horizontal, 6px vertical
    let pad_h = 16.0_f64;
    let pad_v = 6.0_f64;

    // Left group: title (CJK font for kanji, falls back to mono)
    let title_font = fonts.get(&font_handles.cjk).unwrap_or(font);
    let title_brush = bevy_color_to_brush(theme.accent);
    draw_dock_text(
        &mut scene,
        &dock_state.title.text,
        pad_h,
        pad_v,
        dock_state.title.font_size,
        title_font,
        &title_brush,
    );

    // Right group: sparklines + pulse + gap + connection (right-aligned)
    let gap = 12.0_f64;
    let conn_brush = bevy_color_to_brush(dock_state.connection.color);
    let conn_w = measure_text(
        &dock_state.connection.text,
        dock_state.connection.font_size,
        font,
    );

    let pulse_brush = bevy_color_to_brush(dock_state.event_pulse.color);
    let pulse_w = measure_text(
        &dock_state.event_pulse.text,
        dock_state.event_pulse.font_size,
        font,
    );

    // Sparkline dimensions
    let spark_w = 80.0_f64;
    let spark_h = 20.0_f64;
    let spark_gap = 8.0_f64;
    let sparks_total = spark_w + spark_gap + spark_w + gap;

    let right_total = sparks_total + pulse_w + gap + conn_w;
    let right_x = (width - pad_h - right_total).max(pad_h);

    // Draw sparklines
    let spark_y = (36.0 - spark_h) / 2.0; // vertically center in 36px dock
    draw_sparkline_at(
        &mut scene,
        &dock_state.event_spark.data,
        spark_w,
        spark_h,
        right_x,
        spark_y,
        theme.accent,
        0.15,
    );
    draw_sparkline_at(
        &mut scene,
        &dock_state.activity_spark.data,
        spark_w,
        spark_h,
        right_x + spark_w + spark_gap,
        spark_y,
        theme.fg_dim,
        0.10,
    );

    let text_right_x = right_x + sparks_total;

    if !dock_state.event_pulse.text.is_empty() {
        draw_dock_text(
            &mut scene,
            &dock_state.event_pulse.text,
            text_right_x,
            pad_v + 4.0, // slightly lower for smaller text
            dock_state.event_pulse.font_size,
            font,
            &pulse_brush,
        );
    }

    draw_dock_text(
        &mut scene,
        &dock_state.connection.text,
        text_right_x + pulse_w + gap,
        pad_v,
        dock_state.connection.font_size,
        font,
        &conn_brush,
    );

    scene_comp.scene = scene;
    let logical = logical_size(computed);
    rtt.built_width = logical.x;
    rtt.built_height = logical.y;
    scene_comp.version = scene_comp.version.wrapping_add(1).max(1);
}

/// Render the South dock scene.
///
/// Layout: `[mode] [model] ... [activity] [block_activity] ... [contexts] ... [context_usage] [hints]`
pub fn render_south_dock(
    dock_state: Res<DockState>,
    theme: Res<Theme>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut query: Query<(&mut UiVectorScene, &mut UiRttTexture, &ComputedNode), With<SouthDock>>,
    mut hit_regions: ResMut<DockHitRegions>,
) {
    let Ok((mut scene_comp, mut rtt, computed)) = query.single_mut() else {
        return;
    };

    // ComputedNode is physical px; the dock scene builds in logical.
    let logical_width = logical_size(computed).x;
    let width_changed = (rtt.built_width - logical_width).abs() > 0.5;
    if !dock_state.is_changed() && !theme.is_changed() && !width_changed {
        return;
    }

    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    let mut scene = vello::Scene::new();
    let width = logical_width as f64;
    hit_regions.south_regions.clear();

    // Insets: 12px horizontal, 4px vertical
    let pad_h = 12.0_f64;
    let pad_v = 4.0_f64;
    let gap = 12.0_f64;

    // === Left group: mode + model ===
    let mut x = pad_h;

    let mode_brush = bevy_color_to_brush(dock_state.mode.color);
    let mode_w = draw_dock_text(
        &mut scene,
        &dock_state.mode.text,
        x,
        pad_v,
        dock_state.mode.font_size,
        font,
        &mode_brush,
    );
    x += mode_w + gap;

    if !dock_state.model_badge.text.is_empty() {
        let model_brush = bevy_color_to_brush(dock_state.model_badge.color);
        let model_w = draw_dock_text(
            &mut scene,
            &dock_state.model_badge.text,
            x,
            pad_v,
            dock_state.model_badge.font_size,
            font,
            &model_brush,
        );
        x += model_w + gap;
    }

    // === Right group: context_usage + hints (right-aligned) ===
    let hints_brush = bevy_color_to_brush(theme.fg_dim);
    let hints_w = measure_text(&dock_state.hints.text, dock_state.hints.font_size, font);
    let hints_x = (width - pad_h - hints_w).max(x + gap);

    // context_usage sits immediately left of hints, in the same right-aligned group.
    let usage_brush = bevy_color_to_brush(dock_state.context_usage.color);
    let usage_w = measure_text(
        &dock_state.context_usage.text,
        dock_state.context_usage.font_size,
        font,
    );
    let usage_x = (hints_x - gap - usage_w).max(x + gap);

    draw_dock_text(
        &mut scene,
        &dock_state.context_usage.text,
        usage_x,
        pad_v,
        dock_state.context_usage.font_size,
        font,
        &usage_brush,
    );

    draw_dock_text(
        &mut scene,
        &dock_state.hints.text,
        hints_x,
        pad_v,
        dock_state.hints.font_size,
        font,
        &hints_brush,
    );

    // === Middle area: activity + block_activity + contexts ===
    // Activity items go left-to-right from current x
    if !dock_state.agent_activity.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.agent_activity.color);
        let w = draw_dock_text(
            &mut scene,
            &dock_state.agent_activity.text,
            x,
            pad_v,
            dock_state.agent_activity.font_size,
            font,
            &brush,
        );
        x += w + gap;
    }

    if !dock_state.block_activity.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.block_activity.color);
        let w = draw_dock_text(
            &mut scene,
            &dock_state.block_activity.text,
            x,
            pad_v,
            dock_state.block_activity.font_size,
            font,
            &brush,
        );
        x += w + gap;
    }

    if !dock_state.background_jobs.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.background_jobs.color);
        let w = draw_dock_text(
            &mut scene,
            &dock_state.background_jobs.text,
            x,
            pad_v,
            dock_state.background_jobs.font_size,
            font,
            &brush,
        );
        x += w + gap;
    }

    // Context badges — between activity and hints
    let ctx = &dock_state.contexts;
    if let Some((ref source, ref preview)) = ctx.notification {
        // Notification mode: single text
        let notif_text = format!("\u{2190} @{}: \"{}\"", source, preview);
        let brush = bevy_color_to_brush(theme.accent);
        let w = draw_dock_text(&mut scene, &notif_text, x, pad_v, 11.0, font, &brush);
        let _ = w; // advance x not needed — notification is a single item
    } else if !ctx.badges.is_empty() {
        let badge_gap = 8.0_f64;
        for badge in &ctx.badges {
            let label = if badge.is_active {
                format!("[{}]", badge.label)
            } else {
                badge.label.clone()
            };
            let color = if badge.is_active {
                theme.accent
            } else {
                theme.fg_dim
            };
            let brush = bevy_color_to_brush(color);

            let x_start = x as f32;
            let w = draw_dock_text(&mut scene, &label, x, pad_v, 11.0, font, &brush);
            let x_end = (x + w) as f32;
            hit_regions
                .south_regions
                .push((x_start, x_end, badge.context_id));
            x += w + badge_gap;
        }

        if ctx.overflow_count > 0 {
            let overflow_text = format!("+{}", ctx.overflow_count);
            let brush = bevy_color_to_brush(theme.fg_dim);
            let w = draw_dock_text(&mut scene, &overflow_text, x, pad_v, 11.0, font, &brush);
            x += w + badge_gap;
        }

        if ctx.staged_count > 0 {
            let staged_text = format!("\u{00b7}{} staged", ctx.staged_count);
            let brush = bevy_color_to_brush(theme.fg_dim);
            draw_dock_text(&mut scene, &staged_text, x, pad_v, 11.0, font, &brush);
        }
    }

    scene_comp.scene = scene;
    let logical = logical_size(computed);
    rtt.built_width = logical.x;
    rtt.built_height = logical.y;
    scene_comp.version = scene_comp.version.wrapping_add(1).max(1);
}

/// Size each dock's render texture to its laid-out node (physical pixels) and
/// repoint the `ImageNode` when it changes. Mirrors `block_render`'s resize but
/// sizes from `ComputedNode` (full-width bar) rather than measured content.
pub fn resize_dock_textures(
    mut query: Query<
        (&ComputedNode, &mut UiRttTexture, &mut ImageNode),
        Or<(With<NorthDock>, With<SouthDock>)>,
    >,
    text_metrics: Res<crate::text::TextMetrics>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
) {
    let scale = text_metrics.scale_factor;
    let max_dim = gpu_limits.max_texture_dim;

    for (computed, mut texture, mut image_node) in query.iter_mut() {
        // ComputedNode is physical px; ui_rtt_texture_dims expects logical.
        let size = logical_size(computed);
        if size.x <= 0.0 || size.y <= 0.0 {
            continue;
        }

        let (target_w, target_h) = ui_rtt_texture_dims(size.x, size.y, scale, max_dim);
        if texture.width != target_w || texture.height != target_h {
            let new_handle = create_ui_rtt_texture(&mut images, target_w, target_h);
            image_node.image = new_handle.clone();
            texture.image = new_handle;
            texture.width = target_w;
            texture.height = target_h;
        }
    }
}

// ============================================================================
// CLICK HANDLER
// ============================================================================

/// Handle clicks on context badges in the South dock.
pub fn handle_dock_click(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    south_dock: Query<(&ComputedNode, &GlobalTransform), With<SouthDock>>,
    hit_regions: Res<DockHitRegions>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }

    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };
    let Ok((computed, global_transform)) = south_dock.single() else {
        return;
    };

    // Convert cursor to dock-local coordinates. The UI transform and
    // ComputedNode are physical px; cursor_position() and the hit regions
    // (built in the logical-space scene) are logical — convert to logical.
    let inv = computed.inverse_scale_factor();
    let dock_global = global_transform.translation() * inv;
    let dock_size = logical_size(computed);
    // UI node origin is at the center of the node in global transform
    let local_x = cursor_pos.x - (dock_global.x - dock_size.x / 2.0);
    let local_y = cursor_pos.y - (dock_global.y - dock_size.y / 2.0);

    // Check if within dock bounds
    if local_x < 0.0 || local_x > dock_size.x || local_y < 0.0 || local_y > dock_size.y {
        return;
    }

    // Check hit regions
    for &(x_min, x_max, context_id) in &hit_regions.south_regions {
        if local_x >= x_min && local_x <= x_max {
            info!("Context badge clicked: {}", context_id.short());
            switch_writer.write(ContextSwitchRequested { context_id });
            return;
        }
    }
}

// ============================================================================
// DATA-GATHERING SYSTEMS (write to DockState)
// ============================================================================

/// Update mode widget text from vim state + focus area + screen.
///
/// When the user is in a text-editing surface (Compose/Dialog), shows the vim
/// editing mode (NORMAL/INSERT/VISUAL). Otherwise shows the app-level mode.
/// All labels come from the `mode_label_*` fields of `theme.toml` (CRDT-owned,
/// fetched over RPC from the kernel).
pub fn update_mode(
    focus_area: Res<FocusArea>,
    screen: Res<State<crate::ui::screen::Screen>>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
    overlay_q: Query<&crate::view::components::InputOverlay>,
) {
    use crate::ui::screen::Screen;

    // Resolve vim mode from the active overlay (if any).
    let vim_mode = overlay_q.iter().next().and_then(|o| o.vim_mode.clone());

    let (color, label) = match screen.get() {
        Screen::Conversation => match focus_area.as_ref() {
            FocusArea::Compose | FocusArea::Dialog => {
                vim_mode_to_dock(&vim_mode, &theme)
            }
            FocusArea::Conversation => (theme.mode_normal, &theme.mode_label_normal),
        },
        // The editor / room / fsn own the viewport; the conversation dock is
        // hidden, but keep the mode indicator coherent rather than panicking.
        // (The editor's own vim mode renders on its panel — docs/vi.md steps
        // 4–5. `Room` covers a station zoom too now — including the well,
        // which has no second screen left of its own since Slice D. `Fsn`
        // reads raw keys like the room, same reasoning.)
        Screen::Editor | Screen::Room | Screen::Fsn => (theme.mode_normal, &theme.mode_label_normal),
    };

    if dock.mode.text != *label || dock.mode.color != color {
        dock.mode.text = label.clone();
        dock.mode.color = color;
    }
}

/// Map a vim mode string from modalkit to a dock (color, label) pair.
fn vim_mode_to_dock<'a>(vim_mode: &Option<String>, theme: &'a Theme) -> (Color, &'a String) {
    match vim_mode.as_deref() {
        Some(s) if s.contains("INSERT") => (theme.mode_insert, &theme.mode_label_insert),
        Some(s) if s.contains("VISUAL") => (theme.mode_visual, &theme.mode_label_visual),
        Some(s) if s.contains("REPLACE") => (theme.mode_insert, &theme.mode_label_insert),
        _ => (theme.mode_normal, &theme.mode_label_normal),
    }
}

/// Map a connection error string to a short, user-facing label.
///
/// We considered putting a structured kind on `ConnectionStatus::Error`
/// instead of substring-matching, but the error already flows through
/// thiserror Display impls we own (kaijutsu-client SshError) so the strings
/// are stable. Substring detection here keeps the change localized; if more
/// callers need this classification later, promote to an enum on the broadcast.
pub(crate) fn classify_connection_error(msg: &str) -> Option<&'static str> {
    if msg.contains("SSH_AUTH_SOCK") {
        Some("\u{26a0} no SSH agent (set SSH_AUTH_SOCK)")
    } else if msg.contains("No SSH keys available") {
        Some("\u{26a0} SSH agent has no keys")
    } else if msg.contains("Key rejected") || msg.contains("No keys accepted") {
        Some("\u{26a0} SSH key rejected by server")
    } else if msg.contains("HOST KEY CHANGED") || msg.contains("Host key verification failed") {
        Some("\u{26a0} SSH host key mismatch")
    } else if msg.contains("Failed to load key") {
        Some("\u{26a0} SSH key load failed")
    } else {
        None
    }
}

/// Update connection widget when RpcConnectionState changes.
pub fn update_connection(
    conn_state: Res<RpcConnectionState>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !conn_state.is_changed() {
        return;
    }

    let (text, color) = if conn_state.connected {
        let status = conn_state
            .identity
            .as_ref()
            .map(|i| format!("\u{2713} @{}", i.username))
            .unwrap_or_else(|| "\u{2713} Connected".to_string());
        (status, theme.success)
    } else if let Some(label) = conn_state
        .last_error
        .as_deref()
        .and_then(classify_connection_error)
    {
        (label.to_string(), theme.error)
    } else if conn_state.reconnect_attempt > 0 {
        (
            format!(
                "\u{27f3} Reconnecting ({})...",
                conn_state.reconnect_attempt
            ),
            theme.warning,
        )
    } else {
        ("\u{26a0} Disconnected".to_string(), theme.error)
    };

    dock.connection.text = text;
    dock.connection.color = color;
}

#[cfg(test)]
mod tests {
    use super::{
        classify_connection_error, format_background_activity, format_context_usage,
        format_elapsed_ms, format_token_count, BackgroundActivityLevel,
    };

    #[test]
    fn format_token_count_below_thousand_is_exact() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
    }

    #[test]
    fn format_token_count_thousands_abbreviate() {
        assert_eq!(format_token_count(234_000), "234k");
        assert_eq!(format_token_count(1_500), "1.5k");
    }

    #[test]
    fn format_token_count_millions_abbreviate() {
        assert_eq!(format_token_count(1_000_000), "1M");
        assert_eq!(format_token_count(1_200_000), "1.2M");
    }

    /// No usage yet (never completed an LLM call) shows the SAME em-dash
    /// placeholder as the model badge — never a fabricated "0/0" or "0".
    #[test]
    fn format_context_usage_no_usage_is_em_dash() {
        assert_eq!(format_context_usage(None, None), "\u{2014}");
        // Even a stray window with no usage must not claim a fraction —
        // there is no observed fill to show.
        assert_eq!(format_context_usage(None, Some(200_000)), "\u{2014}");
    }

    /// Usage exists but the model's window isn't configured — show the raw
    /// count alone, never guess a denominator to build a fraction/percentage.
    #[test]
    fn format_context_usage_unknown_window_shows_raw_count_only() {
        assert_eq!(format_context_usage(Some(45_231), None), "45.2k");
    }

    /// Both known — the fraction Amy asked for ("234k/1M").
    #[test]
    fn format_context_usage_known_window_shows_fraction() {
        assert_eq!(format_context_usage(Some(234_000), Some(1_000_000)), "234k/1M");
    }

    /// A genuinely fresh, known-window context (0 used) must render `0/window`,
    /// not fall back to the unknown em-dash — this is the exact distinction
    /// the wire's `-1.0` percentage sentinel exists to protect (0% used is
    /// real data, not an absence).
    #[test]
    fn format_context_usage_zero_used_known_window_is_not_em_dash() {
        assert_eq!(format_context_usage(Some(0), Some(200_000)), "0/200k");
    }

    #[test]
    fn agent_missing_classified() {
        let msg = "SSH: SSH error: SSH agent error: Environment variable \
                   `SSH_AUTH_SOCK` not found";
        assert_eq!(
            classify_connection_error(msg),
            Some("\u{26a0} no SSH agent (set SSH_AUTH_SOCK)")
        );
    }

    #[test]
    fn no_keys_classified() {
        let msg = "SSH: SSH error: No SSH keys available in agent";
        assert_eq!(
            classify_connection_error(msg),
            Some("\u{26a0} SSH agent has no keys")
        );
    }

    #[test]
    fn key_rejected_classified() {
        let msg = "SSH: SSH error: Auth failed: Key rejected by server";
        assert_eq!(
            classify_connection_error(msg),
            Some("\u{26a0} SSH key rejected by server")
        );
    }

    #[test]
    fn host_key_mismatch_classified() {
        let msg = "SSH: SSH error: HOST KEY CHANGED for localhost:2222! ...";
        assert_eq!(
            classify_connection_error(msg),
            Some("\u{26a0} SSH host key mismatch")
        );
    }

    #[test]
    fn key_load_failed_classified() {
        let msg = "SSH: SSH error: Failed to load key: /home/x/.ssh/id_rsa: bad passphrase";
        assert_eq!(
            classify_connection_error(msg),
            Some("\u{26a0} SSH key load failed")
        );
    }

    #[test]
    fn transient_unclassified() {
        // Network-style errors stay unclassified — they fall back to the
        // generic Reconnecting label which is appropriate.
        assert_eq!(
            classify_connection_error("SSH: SSH error: Connection failed: connection refused"),
            None
        );
        assert_eq!(
            classify_connection_error("connect timeout (10s)"),
            None
        );
        assert_eq!(
            classify_connection_error("reconnect backoff (attempt 3, 4.0s remaining)"),
            None
        );
    }

    #[test]
    fn format_elapsed_ms_seconds_only_under_a_minute() {
        assert_eq!(format_elapsed_ms(0), "0s");
        assert_eq!(format_elapsed_ms(59_000), "59s");
    }

    #[test]
    fn format_elapsed_ms_minutes_and_seconds_under_an_hour() {
        assert_eq!(format_elapsed_ms(60_000), "1m00s");
        assert_eq!(format_elapsed_ms(192_000), "3m12s");
    }

    #[test]
    fn format_elapsed_ms_hours_and_minutes_past_an_hour() {
        assert_eq!(format_elapsed_ms(3_600_000), "1h00m");
        assert_eq!(format_elapsed_ms(3_600_000 + 5 * 60_000), "1h05m");
    }

    /// Nothing running, nothing ever finished — the "hidden when idle"
    /// state, same convention `block_activity` uses for its own empty text.
    #[test]
    fn background_activity_idle_when_nothing_running_or_finished() {
        let (text, level) = format_background_activity(0, None, None, None, None, 10_000);
        assert_eq!(text, "");
        assert_eq!(level, BackgroundActivityLevel::Idle);
    }

    /// One process running: singular phrasing, elapsed anchored on its own
    /// start time.
    #[test]
    fn background_activity_one_running_shows_elapsed_since_start() {
        let (text, level) =
            format_background_activity(1, Some(1_000), None, None, None, 1_000 + 5_000);
        assert_eq!(text, "\u{2699} running 5s");
        assert_eq!(level, BackgroundActivityLevel::Running);
    }

    /// Several running: count shown, elapsed anchored on the OLDEST one
    /// (the wire's `oldest_running_started_at`), not an average or the
    /// newest.
    #[test]
    fn background_activity_several_running_shows_count_and_oldest_elapsed() {
        let (text, level) =
            format_background_activity(3, Some(0), None, None, None, 192_000);
        assert_eq!(text, "\u{2699} 3 running (3m12s)");
        assert_eq!(level, BackgroundActivityLevel::Running);
    }

    /// A running process always wins over a stale finished outcome — there
    /// IS something to watch right now, so the last-finished text must not
    /// leak through while something is still going.
    #[test]
    fn background_activity_running_takes_precedence_over_last_finished() {
        let (text, level) = format_background_activity(
            1,
            Some(9_000),
            Some(1_000),
            Some("exited"),
            Some(1),
            10_000,
        );
        assert!(text.starts_with("\u{2699} running"), "got: {text:?}");
        assert_eq!(level, BackgroundActivityLevel::Running);
    }

    /// A clean exit(0) after everything else finished — the "just
    /// succeeded" case, rendered calmly (Finished, not Failed).
    #[test]
    fn background_activity_clean_exit_is_finished_not_failed() {
        let (text, level) =
            format_background_activity(0, None, Some(5_000), Some("exited"), Some(0), 17_000);
        assert_eq!(text, "\u{2699} exited 12s ago");
        assert_eq!(level, BackgroundActivityLevel::Finished);
    }

    /// A nonzero exit is the "just failed" case the dock must call out —
    /// Failed level, exit code visible.
    #[test]
    fn background_activity_nonzero_exit_is_failed_with_code() {
        let (text, level) =
            format_background_activity(0, None, Some(5_000), Some("exited"), Some(1), 17_000);
        assert_eq!(text, "\u{2699} exited(1) 12s ago");
        assert_eq!(level, BackgroundActivityLevel::Failed);
    }

    /// A killed process has no exit code — must read "killed", never a
    /// fabricated exit code.
    #[test]
    fn background_activity_killed_is_failed_with_no_fabricated_code() {
        let (text, level) =
            format_background_activity(0, None, Some(5_000), Some("killed"), None, 17_000);
        assert_eq!(text, "\u{2699} killed 12s ago");
        assert_eq!(level, BackgroundActivityLevel::Failed);
    }

    /// The running -> terminal transition: once `running_count` drops to 0
    /// and a finished outcome is reported, the badge must switch from the
    /// "running" phrasing to the "how did it end" phrasing — never keep
    /// showing "running" past the process's actual completion.
    #[test]
    fn background_activity_transitions_from_running_to_finished() {
        let (running_text, running_level) =
            format_background_activity(1, Some(1_000), None, None, None, 3_000);
        assert_eq!(running_level, BackgroundActivityLevel::Running);
        assert!(running_text.contains("running"));

        let (done_text, done_level) =
            format_background_activity(0, None, Some(4_000), Some("exited"), Some(0), 6_000);
        assert_eq!(done_level, BackgroundActivityLevel::Finished);
        assert!(!done_text.contains("running"), "must not still say 'running' after it finished");
    }
}

/// Update contexts widget when DriftState or DocumentCache changes.
pub fn update_contexts(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    _theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    let ctx = &mut dock.contexts;

    // Notification takes precedence
    if let Some(ref notif) = drift_state.notification {
        ctx.notification = Some((notif.source_ctx.clone(), notif.preview.clone()));
        ctx.badges.clear();
        ctx.overflow_count = 0;
        ctx.staged_count = 0;
        return;
    }
    ctx.notification = None;

    let mru_ids = doc_cache.mru_ids();
    let active_id = doc_cache.active_id();
    let max_display = 5;

    if !mru_ids.is_empty() {
        ctx.badges = mru_ids
            .iter()
            .take(max_display)
            .map(|doc_id| {
                let ctx_name = doc_cache
                    .get(*doc_id)
                    .map(|c| c.context_name.clone())
                    .unwrap_or_else(|| "?".to_string());
                let is_active = active_id == Some(*doc_id);
                let short = if ctx_name.len() > 12 {
                    ctx_name[..12].to_string()
                } else {
                    ctx_name.clone()
                };
                ContextBadgeData {
                    context_id: *doc_id,
                    label: short,
                    is_active,
                }
            })
            .collect();

        ctx.overflow_count = mru_ids.len().saturating_sub(max_display);
        ctx.staged_count = drift_state.staged_count();
    } else {
        // Fall back to drift state contexts as text-based badges
        ctx.badges.clear();
        ctx.overflow_count = 0;
        ctx.staged_count = drift_state.staged_count();

        if !drift_state.contexts.is_empty() {
            for (i, drift_ctx) in drift_state.contexts.iter().enumerate() {
                if i >= max_display {
                    ctx.overflow_count = drift_state.contexts.len() - max_display;
                    break;
                }
                ctx.badges.push(ContextBadgeData {
                    context_id: drift_ctx.id,
                    label: format!("@{}", drift_ctx.id.short()),
                    is_active: drift_state.local_context_id == Some(drift_ctx.id),
                });
            }
        }
    }
}

/// Update hints widget based on FocusArea and Screen.
///
/// `Screen::Room` now shows one of two hint lines depending on
/// `RoomState::zoomed` (2026-07-10 evening, the fullscreen-panel pivot: the
/// old `Screen::PatchBay` hint line moved here, since diving no longer
/// changes `Screen` at all).
pub fn update_hints(
    focus_area: Res<FocusArea>,
    screen: Res<State<crate::ui::screen::Screen>>,
    room: Res<crate::view::room::RoomState>,
    prefix: Res<crate::input::prefix::PrefixState>,
    mut dock: ResMut<DockState>,
) {
    if !focus_area.is_changed()
        && !screen.is_changed()
        && !room.is_changed()
        && !prefix.is_changed()
    {
        return;
    }

    // A pending Ctrl+A owns the footer while armed — this IS the prefix
    // legend (docs/input.md): it appears exactly when you need it and
    // vanishes with the pending state, so no separate `?` overlay. The
    // working set only — sized to sit clear of the left badge cluster at
    // normal window widths (n/p and `a` live in docs/input.md); a reminder,
    // not the reference.
    if prefix.armed() {
        let hints = "^A: 0-9 seat \u{2502} ^A last \u{2502} q close \u{2502} \
                     w well \u{2502} ' switch \u{2502} A rename \u{2502} d detach";
        if dock.hints.text != hints {
            dock.hints.text = hints.to_string();
        }
        return;
    }

    use crate::ui::screen::Screen;
    let hints = match screen.get() {
        Screen::Conversation => match focus_area.as_ref() {
            FocusArea::Compose => {
                "Enter: submit \u{2502} Shift+Enter: newline \u{2502} Ctrl+Z: shell \u{2502} Esc Esc: dismiss"
            }
            FocusArea::Conversation => {
                "i: chat \u{2502} Ctrl+Z: shell \u{2502} j/k: navigate \u{2502} Alt+hjkl: pane"
            }
            FocusArea::Dialog => "Enter: confirm \u{2502} Esc: cancel \u{2502} j/k: navigate",
        },
        Screen::Editor => "Esc: back to conversation \u{2502} (editor)",
        Screen::Room => match room.zoomed {
            None => "\u{2190}\u{2192}: station \u{2502} Enter/\u{2193}: zoom \u{2502} Esc: conversation",
            Some(crate::view::room::nav::Station::PatchBay) => {
                "\u{2190}\u{2192}: wire \u{2502} \u{2191}/Esc: room \u{2502} r: rescan"
            }
            Some(crate::view::room::nav::Station::TimeWell) => {
                "0-9/\u{2190}\u{2192}\u{2191}\u{2193}: seat & ring \u{2502} Enter: focus/commit \u{2502} c/p/d/z/a: act \u{2502} Esc: room"
            }
            // Every wall station zooms now (`station_is_zoomable`,
            // 2026-07-13); the plain panels (Tracks/Vfs/Radiators) share
            // `plain_zoom_keyboard`'s surface-only keys.
            Some(_) => "\u{2191}/Esc: room",
        },
        Screen::Fsn => {
            "WASD/\u{2190}\u{2192}\u{2191}\u{2193}: fly \u{2502} PgUp/PgDn: altitude \u{2502} Esc: room"
        }
    };

    if dock.hints.text != hints {
        dock.hints.text = hints.to_string();
    }
}

/// Rolling event counter state for the EventPulse widget.
#[derive(Default)]
pub(crate) struct EventPulseState {
    timestamps: VecDeque<f64>,
    last_spark_sample: f64,
}

/// Update event pulse — shows server event rate in a rolling 5s window.
pub fn update_event_pulse(
    mut state: Local<EventPulseState>,
    time: Res<Time>,
    mut events: MessageReader<ServerEventMessage>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    let now = time.elapsed_secs_f64();
    let window = 5.0;

    let count = events.read().count();
    for _ in 0..count {
        state.timestamps.push_back(now);
    }

    while let Some(&front) = state.timestamps.front() {
        if now - front > window {
            state.timestamps.pop_front();
        } else {
            break;
        }
    }

    let total = state.timestamps.len();
    let (text, color) = if total > 0 {
        (format!("~{} ops", total), theme.accent)
    } else {
        ("quiet".to_string(), theme.fg_dim)
    };

    if dock.event_pulse.text != text {
        dock.event_pulse.text = text;
        dock.event_pulse.color = color;
    }

    // Sample event rate for sparkline every 250ms
    if now - state.last_spark_sample >= 0.25 {
        state.last_spark_sample = now;
        dock.event_spark.push(total as f64);
    }
}

/// Update model badge — shows active context's model name.
pub fn update_model_badge(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    let model_text = if let Some(active_id) = doc_cache.active_id() {
        drift_state
            .contexts
            .iter()
            .find(|ctx| ctx.id == active_id)
            .map(|ctx| {
                if ctx.model.is_empty() {
                    "\u{2014}".to_string()
                } else {
                    shorten_model_name(&ctx.model)
                }
            })
            .unwrap_or_else(|| "\u{2014}".to_string())
    } else {
        "\u{2014}".to_string()
    };

    if dock.model_badge.text != model_text {
        dock.model_badge.text = model_text;
        dock.model_badge.color = theme.fg_dim;
    }
}

/// Shorten a model name for display (e.g. "claude-opus-4-6" -> "opus-4.6").
fn shorten_model_name(model: &str) -> String {
    let m = model.strip_prefix("claude-").unwrap_or(model);
    if let Some(pos) = m.rfind('-')
        && pos > 0
        && m[pos + 1..].chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}.{}", &m[..pos], &m[pos + 1..]);
    }
    m.to_string()
}

/// Update the context-usage badge — "how full is the active context",
/// Amy's bottom-dock gauge ask. Reads the SAME kernel-derived numbers `kj
/// context info --json` reports (`ContextInfo::context_window` /
/// `context_used_tokens` / `context_used_pct`, decoded from the wire's
/// honest sentinels by `kaijutsu-client::parse_context_info`) — display
/// only, no interaction.
pub fn update_context_usage_badge(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    let text = if let Some(active_id) = doc_cache.active_id() {
        drift_state
            .contexts
            .iter()
            .find(|ctx| ctx.id == active_id)
            .map(|ctx| format_context_usage(ctx.context_used_tokens, ctx.context_window))
            .unwrap_or_else(|| "\u{2014}".to_string())
    } else {
        "\u{2014}".to_string()
    };

    if dock.context_usage.text != text {
        dock.context_usage.text = text;
        dock.context_usage.color = theme.fg_dim;
    }
}

/// Render text for the context-usage badge — never claims knowledge it
/// doesn't have:
/// - no usage yet (context never completed an LLM call) -> em-dash, same
///   placeholder the model badge uses;
/// - usage but no configured window for the model -> the raw token count
///   alone, no fraction/percentage (there's nothing honest to divide by);
/// - both known -> `used/window` in abbreviated form (e.g. `234k/1M`).
fn format_context_usage(used_tokens: Option<u64>, window: Option<u64>) -> String {
    match (used_tokens, window) {
        (Some(used), Some(w)) => format!("{}/{}", format_token_count(used), format_token_count(w)),
        (Some(used), None) => format_token_count(used),
        (None, _) => "\u{2014}".to_string(),
    }
}

/// Abbreviate a token count for the dock's tight horizontal budget:
/// `999` -> `"999"`, `234_000` -> `"234k"`, `1_000_000` -> `"1M"`.
/// Not locale-aware, not exact for display — a legible-at-a-glance
/// approximation, same spirit as `shorten_model_name`.
fn format_token_count(n: u64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    if n as f64 >= M {
        format_abbreviated(n as f64 / M, "M")
    } else if n as f64 >= K {
        format_abbreviated(n as f64 / K, "k")
    } else {
        n.to_string()
    }
}

/// One decimal place, dropped when it would just be `.0` (`1.0M` -> `1M`,
/// `1.5M` stays `1.5M`).
fn format_abbreviated(value: f64, suffix: &str) -> String {
    if (value.round() - value).abs() < 0.05 {
        format!("{:.0}{suffix}", value.round())
    } else {
        format!("{value:.1}{suffix}")
    }
}

/// Which of the dock's semantic theme colors the background-jobs badge
/// should render with. Kept as a small enum (not `bevy::Color` itself) so
/// `format_background_activity`'s state -> text mapping is unit-testable
/// with no `Theme` resource involved — `update_background_jobs` maps this
/// to a color at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundActivityLevel {
    /// Nothing running, nothing ever finished (or it aged out of the
    /// kernel's own retention and was reaped) — badge renders as empty text.
    Idle,
    /// At least one background process is still running.
    Running,
    /// The most-recently-finished process exited cleanly (code 0).
    Finished,
    /// The most-recently-finished process was killed or exited nonzero.
    Failed,
}

/// Format the background-jobs dock badge from a context's background-process
/// ambient-state wire fields (`ContextInfo::background_*`, decoded from
/// `ContextHandleInfo` @23-@27 by `kaijutsu_client::parse_context_info`) —
/// display only, no interaction (killing a job stays a model-driven MCP
/// tool call, not a dock click), same spirit as `format_context_usage`.
///
/// Precedence: a still-running process always wins over a past one's
/// outcome — there IS something to watch right now. Only once nothing is
/// running does the last-finished outcome surface. Nothing ever
/// running/finished renders as empty text (`BackgroundActivityLevel::Idle`)
/// — never a fabricated "0 running".
pub(crate) fn format_background_activity(
    running_count: u32,
    oldest_running_started_at_unix_ms: Option<u64>,
    last_finished_at_unix_ms: Option<u64>,
    last_finished_status: Option<&str>,
    last_exit_code: Option<i32>,
    now_unix_ms: u64,
) -> (String, BackgroundActivityLevel) {
    if running_count > 0 {
        let elapsed = oldest_running_started_at_unix_ms
            .map(|started| now_unix_ms.saturating_sub(started))
            .unwrap_or(0);
        let text = if running_count == 1 {
            format!("\u{2699} running {}", format_elapsed_ms(elapsed))
        } else {
            format!("\u{2699} {running_count} running ({})", format_elapsed_ms(elapsed))
        };
        return (text, BackgroundActivityLevel::Running);
    }

    match (last_finished_at_unix_ms, last_finished_status) {
        (None, None) => (String::new(), BackgroundActivityLevel::Idle),
        (Some(finished_at), Some(status)) => {
            let ago = format_elapsed_ms(now_unix_ms.saturating_sub(finished_at));
            match status {
                "exited" => match last_exit_code {
                    Some(0) => (
                        format!("\u{2699} exited {ago} ago"),
                        BackgroundActivityLevel::Finished,
                    ),
                    Some(code) => (
                        format!("\u{2699} exited({code}) {ago} ago"),
                        BackgroundActivityLevel::Failed,
                    ),
                    // Shouldn't happen (an "exited" status always carries a
                    // code) — shown rather than silently hidden, since a
                    // display gap here would mask a real wire/decode bug.
                    None => (
                        format!("\u{2699} exited(?) {ago} ago"),
                        BackgroundActivityLevel::Failed,
                    ),
                },
                "killed" => (
                    format!("\u{2699} killed {ago} ago"),
                    BackgroundActivityLevel::Failed,
                ),
                // Unrecognized status string — an honest "something's off"
                // marker rather than silently falling back to empty (a
                // real state we CAN observe is inconsistent).
                other => (format!("\u{2699} {other} {ago} ago"), BackgroundActivityLevel::Idle),
            }
        }
        // Half-set wire pair (one of the two set, the other not) — the
        // server always sets both or neither. Rendered rather than hidden,
        // for the same "don't silently mask an inconsistent state" reason.
        _ => ("\u{2699} ?".to_string(), BackgroundActivityLevel::Idle),
    }
}

/// Bucket a millisecond duration for the dock's tight horizontal budget:
/// `< 1min` -> seconds, `< 1hr` -> minutes+seconds, else hours+minutes.
/// Not locale-aware — a legible-at-a-glance approximation, same spirit as
/// `format_token_count`.
fn format_elapsed_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Update the background-jobs badge — "is anything backgrounded still
/// running, and how did the last one end" for the active context, Amy's ask
/// for app visibility into `background_exec.rs` (docs/issues.md
/// "Background shell + process management"). Reads the SAME kernel-derived
/// `ContextInfo.background_*` fields the context-usage badge reads its own
/// fields from — display only, no interaction. Refreshes on the same
/// ~5s `DriftState` poll cadence the model badge and context-usage badge
/// already use (`ui/drift.rs::DRIFT_POLL_INTERVAL`) — the elapsed-time text
/// is therefore accurate to within one poll interval, not per-frame.
pub fn update_background_jobs(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let (text, level) = doc_cache
        .active_id()
        .and_then(|active_id| drift_state.contexts.iter().find(|ctx| ctx.id == active_id))
        .map(|ctx| {
            format_background_activity(
                ctx.background_running_count,
                ctx.background_oldest_running_started_at,
                ctx.background_last_finished_at,
                ctx.background_last_finished_status.as_deref(),
                ctx.background_last_exit_code,
                now_unix_ms,
            )
        })
        .unwrap_or((String::new(), BackgroundActivityLevel::Idle));

    let color = match level {
        BackgroundActivityLevel::Idle => theme.fg_dim,
        BackgroundActivityLevel::Running => theme.accent,
        BackgroundActivityLevel::Finished => theme.fg_dim,
        BackgroundActivityLevel::Failed => theme.error,
    };

    if dock.background_jobs.text != text {
        dock.background_jobs.text = text;
        dock.background_jobs.color = color;
    }
}

/// Tracks running block counts for the BlockActivity widget.
#[derive(Default)]
pub(crate) struct BlockActivityCounts {
    running: u32,
    last_active_doc: Option<String>,
    last_spark_sample: f64,
}

/// Update block activity — shows running block count for active document.
pub fn update_block_activity(
    mut state: Local<BlockActivityCounts>,
    time: Res<Time>,
    mut events: MessageReader<ServerEventMessage>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    let active_doc = doc_cache.active_id().map(|s| s.to_string());

    if active_doc != state.last_active_doc {
        state.running = 0;
        state.last_active_doc = active_doc.clone();
    }

    for event in events.read() {
        if let kaijutsu_client::ServerEvent::BlockStatusChanged {
            context_id, status, ..
        } = &event.0
            && active_doc.as_deref() == Some(&context_id.to_string())
        {
            match status {
                kaijutsu_crdt::Status::Running => {
                    state.running = state.running.saturating_add(1);
                }
                kaijutsu_crdt::Status::Done | kaijutsu_crdt::Status::Error => {
                    state.running = state.running.saturating_sub(1);
                }
                _ => {}
            }
        }
    }

    let text = if state.running > 0 {
        format!("{} running", state.running)
    } else {
        String::new()
    };

    if dock.block_activity.text != text {
        dock.block_activity.text = text;
        dock.block_activity.color = theme.accent;
    }

    // Sample running block count for sparkline every 250ms
    let now = time.elapsed_secs_f64();
    if now - state.last_spark_sample >= 0.25 {
        state.last_spark_sample = now;
        dock.activity_spark.push(state.running as f64);
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for Vello-drawn dock bars.
pub struct DockPlugin;

impl Plugin for DockPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DockState>()
            .init_resource::<DockHitRegions>()
            .register_type::<NorthDock>()
            .register_type::<SouthDock>()
            // spawn_docks runs in PostStartup so TilingRoot exists (spawned in Startup)
            .add_systems(PostStartup, spawn_docks)
            .add_systems(
                Update,
                (
                    update_mode,
                    update_connection,
                    update_contexts,
                    update_hints,
                    update_event_pulse,
                    update_model_badge,
                    update_context_usage_badge,
                    update_block_activity,
                    update_background_jobs,
                    // Conversation-only: the dock's ComputedNode/GlobalTransform
                    // survive Visibility::Hidden, so without the gate a click
                    // in the dock's footprint would switch contexts UNDER a
                    // fullscreen scene (input-rework audit, 2026-07-16).
                    handle_dock_click
                        .run_if(in_state(crate::ui::screen::Screen::Conversation)),
                ),
            )
            .add_systems(
                PostUpdate,
                (
                    (render_north_dock, render_south_dock).after(bevy::ui::UiSystems::Layout),
                    resize_dock_textures
                        .after(render_north_dock)
                        .after(render_south_dock),
                ),
            );
    }
}
