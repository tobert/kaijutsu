//! Vello-drawn dock bars (North + South).
//!
//! Each dock is a single Bevy entity with `UiVelloScene`. All text is drawn
//! directly into the Vello scene — no child entities, no flex layout for widgets.
//!
//! `DockState` resource holds all widget data. Data-gathering systems write to
//! `DockState` fields; render systems read `DockState` + `ComputedNode` and
//! rebuild the Vello scene each frame the data changes.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_vello::prelude::{UiVelloScene, VelloFont, VelloTextAlign, VelloTextStyle};
use bevy_vello::vello;
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
use crate::text::{FontHandles, bevy_color_to_brush};
use crate::ui::constellation::{ActivityState, Constellation};
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
    pub agent_activity: DockText,
    pub block_activity: DockText,
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
            let bevy_vello::parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
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
            UiVelloScene::default(),
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
            UiVelloScene::default(),
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
    font_handles: Res<FontHandles>,
    mut query: Query<(&mut UiVelloScene, &ComputedNode), With<NorthDock>>,
) {
    if !dock_state.is_changed() && !theme.is_changed() {
        return;
    }

    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    let Ok((mut scene_comp, computed)) = query.single_mut() else {
        return;
    };

    let mut scene = vello::Scene::new();
    let width = computed.size().x as f64;

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

    *scene_comp = UiVelloScene::from(scene);
}

/// Render the South dock scene.
///
/// Layout: `[mode] [model] ... [activity] [block_activity] ... [contexts] ... [hints]`
pub fn render_south_dock(
    dock_state: Res<DockState>,
    theme: Res<Theme>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    mut query: Query<(&mut UiVelloScene, &ComputedNode), With<SouthDock>>,
    mut hit_regions: ResMut<DockHitRegions>,
) {
    if !dock_state.is_changed() && !theme.is_changed() {
        return;
    }

    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };

    let Ok((mut scene_comp, computed)) = query.single_mut() else {
        return;
    };

    let mut scene = vello::Scene::new();
    let width = computed.size().x as f64;
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

    // === Right group: hints (right-aligned) ===
    let hints_brush = bevy_color_to_brush(theme.fg_dim);
    let hints_w = measure_text(&dock_state.hints.text, dock_state.hints.font_size, font);
    let hints_x = (width - pad_h - hints_w).max(x + gap);

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

    *scene_comp = UiVelloScene::from(scene);
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

    // Convert cursor to dock-local coordinates
    let dock_global = global_transform.translation();
    let dock_size = computed.size();
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

/// Update mode widget text when FocusArea or Screen changes.
pub fn update_mode(
    focus_area: Res<FocusArea>,
    screen: Res<State<crate::ui::screen::Screen>>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !focus_area.is_changed() && !screen.is_changed() {
        return;
    }

    use crate::ui::screen::Screen;
    let (color, name) = match screen.get() {
        Screen::Constellation => (theme.mode_visual, "CONSTELLATION"),
        Screen::Conversation => match focus_area.as_ref() {
            FocusArea::Compose => (theme.mode_chat, "INPUT"),
            FocusArea::Conversation => (theme.mode_normal, focus_area.name()),
            FocusArea::Dialog => (theme.mode_shell, focus_area.name()),
        },
    };

    dock.mode.text = name.to_string();
    dock.mode.color = color;
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
    } else if conn_state.reconnect_attempt > 0 {
        (
            format!(
                "\u{27f3} Reconnecting ({})...",
                conn_state.reconnect_attempt
            ),
            theme.warning,
        )
    } else {
        ("\u{26a1} Disconnected".to_string(), theme.error)
    };

    dock.connection.text = text;
    dock.connection.color = color;
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
pub fn update_hints(
    focus_area: Res<FocusArea>,
    screen: Res<State<crate::ui::screen::Screen>>,
    mut dock: ResMut<DockState>,
) {
    if !focus_area.is_changed() && !screen.is_changed() {
        return;
    }

    use crate::ui::screen::Screen;
    let hints = match screen.get() {
        Screen::Constellation => {
            "Enter: switch \u{2502} m: model \u{2502} n: new \u{2502} Tab: compose \u{2502} Esc: back"
        }
        Screen::Conversation => match focus_area.as_ref() {
            FocusArea::Compose => {
                "Enter: submit \u{2502} Shift+Enter: newline \u{2502} Tab: mode ring \u{2502} Esc: dismiss"
            }
            FocusArea::Conversation => {
                "i: chat \u{2502} :: shell \u{2502} j/k: navigate \u{2502} f: expand \u{2502} `: constellation \u{2502} Alt+hjkl: pane"
            }
            FocusArea::Dialog => "Enter: confirm \u{2502} Esc: cancel \u{2502} j/k: navigate",
        },
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

/// Update agent activity — summarizes non-idle activity across constellation nodes.
pub fn update_agent_activity(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    if !constellation.is_changed() {
        return;
    }

    let mut streaming = 0u32;
    let mut waiting = 0u32;
    let mut active = 0u32;

    for node in &constellation.nodes {
        match node.activity {
            ActivityState::Streaming => streaming += 1,
            ActivityState::Waiting => waiting += 1,
            ActivityState::Active => active += 1,
            _ => {}
        }
    }

    let total_active = streaming + waiting + active;
    let (text, color) = if streaming == 1 && total_active == 1 {
        ("streaming".to_string(), theme.accent)
    } else if total_active > 0 {
        (format!("{} active", total_active), theme.accent)
    } else {
        (String::new(), theme.fg_dim)
    };

    if dock.agent_activity.text != text {
        dock.agent_activity.text = text;
        dock.agent_activity.color = color;
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
                    update_agent_activity,
                    update_block_activity,
                    handle_dock_click,
                ),
            )
            .add_systems(
                PostUpdate,
                (render_north_dock, render_south_dock).after(bevy::ui::UiSystems::Layout),
            );
    }
}
