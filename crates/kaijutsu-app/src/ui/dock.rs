//! MSDF-drawn dock bars (North + South) — no vello.
//!
//! Each dock is a single Bevy entity with `MsdfBlockGlyphs` + `UiRttTexture` +
//! `ImageNode` (the same generic MSDF extract/render pass block cells and
//! role headers use — `view::block_render::{extract_msdf_blocks,
//! render_msdf_block_textures}` — since neither is gated on `BlockCell`).
//! Text collects glyphs into `MsdfBlockGlyphs`; the North dock's two
//! sparklines are flat-colored triangles in `MsdfBlockGeometry`
//! (`text::sparkline::build_sparkline_vertices`), drawn into the same dock
//! texture — they used to be per-piece UI-node children, which respawned
//! after layout on every data tick and blanked for a frame (the HUD
//! flicker).
//!
//! `DockState` resource holds all widget data. Data-gathering systems write to
//! `DockState` fields; render systems read `DockState` + `ComputedNode` and
//! rebuild the glyph + geometry buffers each frame the data changes — both
//! ride `MsdfBlockGlyphs.version` (fill both, bump once; see
//! `MsdfBlockGeometry`'s doc comment).

use std::collections::VecDeque;

use bevy::prelude::*;
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::text::color_to_rgba8;
use crate::text::msdf::{
    FontDataMap, MsdfAtlas, MsdfBlockGeometry, MsdfBlockGlyphs, PositionedGlyph,
    collect_msdf_glyphs, geometry::GeometryVertex,
};
use crate::shaders::BlockFxMaterial;
use crate::view::block_render::GpuTextureLimits;
use crate::view::ui_rtt::{UiRttTexture, logical_size};

use crate::cell::ContextSwitchRequested;
use crate::connection::RpcConnectionState;
use crate::connection::actor_plugin::ServerEventMessage;
use crate::input::FocusArea;
use crate::text::sparkline::{SparklineData, build_sparkline_vertices};

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
use crate::connection::drift::DriftState;
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
    /// Ambient HUD signal for `GlobalErrorQueue` — context-free failures
    /// (bad `theme.toml`, failed context creation, other RPC errors with no
    /// conversation to attach to). Empty when the queue is empty; see
    /// `update_global_errors_badge`.
    pub global_errors: DockText,
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
    /// True while the active screen is a fleet/world view (`Room`, `Fsn`) —
    /// the HUD *detaches from the active context*: renders skip every
    /// context-bound widget (model badge, block/agent activity, background
    /// jobs, context badges, context usage, the activity sparkline) so the
    /// footer carries only mode/hints and the genuinely ambient app signals.
    /// The room's own furniture (switchboard embers, radiators, seats) is
    /// the fleet-level display; a one-context cockpit readout under it was
    /// contradictory (Amy, 2026-08-10). Editor/Diff stay attached — they are
    /// surfaces *of* the active context. Data-gathering systems keep
    /// running; only rendering is gated, so reattaching is instant.
    pub detached: bool,
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
            global_errors: DockText {
                text: String::new(),
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
            detached: false,
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
// TEXT + SPARKLINE DRAWING HELPERS
// ============================================================================

/// Collect MSDF glyphs for one run of dock text at `(x, y)`, appending them
/// into `glyphs`, and return the advance width — same signature/contract as
/// the vello-drawing function this replaces, so every call site below reads
/// unchanged.
///
/// `y` is the top of the text area; `collect_msdf_glyphs`'s offset does the
/// same job the old `Affine::translate((x, y))` transform did, since a glyph
/// run's own `offset()`/`baseline()` are already relative to that top-left
/// origin. Single-color text — no span brushes, `brush` is the fallback for
/// every glyph — same as a block cell's checkbox glyphs
/// (`view::block_render::build_block_scenes`).
fn collect_dock_text_glyphs(
    glyphs: &mut Vec<PositionedGlyph>,
    text: &str,
    x: f64,
    y: f64,
    font_size: f32,
    font: &VelloFont,
    brush: &peniko::Brush,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> f64 {
    if text.is_empty() {
        return 0.0;
    }

    let style = VelloTextStyle {
        font_size,
        ..default()
    };
    let layout = font.layout(text, &style, VelloTextAlign::Left, None);

    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }

    glyphs.extend(collect_msdf_glyphs(&layout, &[], brush, (x, y), atlas));

    layout.width() as f64
}

/// Measure text width without drawing. Pure Parley shaping — no vello, no
/// MSDF; unchanged by the dock's move off vello.
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

/// Append a dock sparkline at `(x, y)` as flat-colored triangles — drawn into
/// the dock's own MSDF texture via `MsdfBlockGeometry`, in the same rebuild
/// that collects the dock's glyphs, so the sparkline updates atomically with
/// the text around it (no child entities, no layout round-trip, no
/// one-frame blank).
fn append_dock_sparkline(
    vertices: &mut Vec<GeometryVertex>,
    data: &SparklineData,
    width: f64,
    height: f64,
    x: f64,
    y: f64,
    line_color: Color,
    fill_alpha: f32,
) {
    vertices.extend(build_sparkline_vertices(
        data,
        width as f32,
        height as f32,
        2.0,
        (x as f32, y as f32),
        color_to_rgba8(line_color),
        Some(color_to_rgba8(line_color.with_alpha(fill_alpha))),
    ));
}

// ============================================================================
// STARTUP SYSTEM
// ============================================================================

/// Spawn the two dock entities as children of TilingRoot.
pub fn spawn_docks(
    mut commands: Commands,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    tiling_root: Query<Entity, With<super::tiling_reconciler::TilingRoot>>,
) {
    let Ok(root) = tiling_root.single() else {
        return;
    };

    // Both docks composite their RTT texture through a default
    // BlockFxMaterial (no border, no glow — a pure texture draw) rather than
    // the ImageNode: the texture content is premultiplied, and only the
    // material's pipeline declares that blend state. The ImageNode stays as
    // the handle slot resize_rtt_texture repoints, tinted fully transparent
    // so the UI image pipeline (straight-alpha) never draws it.

    // North dock — inserted at index 0 (before ContentArea). Carries
    // `MsdfBlockGeometry` for its two sparklines (flat triangles in the same
    // texture as the glyphs); the South dock is text-only and doesn't.
    let north = commands
        .spawn((
            NorthDock,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(40.0),
                ..default()
            },
            BorderColor::all(theme.border),
            ImageNode::default().with_color(Color::NONE),
            MaterialNode(fx_materials.add(BlockFxMaterial::default())),
            MsdfBlockGlyphs::default(),
            MsdfBlockGeometry::default(),
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
            ImageNode::default().with_color(Color::NONE),
            MaterialNode(fx_materials.add(BlockFxMaterial::default())),
            MsdfBlockGlyphs::default(),
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
    mut query: Query<
        (
            &mut MsdfBlockGlyphs,
            &mut MsdfBlockGeometry,
            &mut UiRttTexture,
            &ComputedNode,
        ),
        With<NorthDock>,
    >,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Ok((mut msdf_glyphs, mut msdf_geometry, mut rtt, computed)) = query.single_mut() else {
        return;
    };

    // Rebuild on data/theme change or when the dock changed width (right-aligned
    // groups must reflow; a stale-width glyph layout would otherwise stretch
    // onto the resized texture).
    // ComputedNode is physical px; the dock scene builds in logical.
    let logical_width = logical_size(computed).x;
    let width_changed = (rtt.built_width - logical_width).abs() > 0.5;
    if !dock_state.is_changed() && !theme.is_changed() && !width_changed {
        return;
    }

    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(ref mut atlas) = atlas else {
        return;
    };

    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
    let mut geometry: Vec<GeometryVertex> = Vec::new();
    let width = logical_width as f64;

    // Insets: 16px horizontal, 6px vertical
    let pad_h = 16.0_f64;
    let pad_v = 6.0_f64;

    // Left group: title (CJK font for kanji, falls back to mono)
    let title_font = fonts.get(&font_handles.cjk).unwrap_or(font);
    let title_brush = bevy_color_to_brush(theme.accent);
    collect_dock_text_glyphs(
        &mut glyphs,
        &dock_state.title.text,
        pad_h,
        pad_v,
        dock_state.title.font_size,
        title_font,
        &title_brush,
        atlas,
        &mut font_data_map,
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

    // Ambient GlobalErrorQueue signal — sits between the pulse and the
    // connection status; empty (width 0) when the queue has nothing to show,
    // so it costs no space when there's nothing wrong.
    let errors_brush = bevy_color_to_brush(dock_state.global_errors.color);
    let errors_w = measure_text(
        &dock_state.global_errors.text,
        dock_state.global_errors.font_size,
        font,
    );

    // Sparkline dimensions. The activity sparkline samples the ACTIVE
    // context's running blocks, so it drops out while the HUD is detached
    // (`DockState::detached`); the event sparkline is kernel-wide and stays.
    let spark_w = 80.0_f64;
    let spark_h = 20.0_f64;
    let spark_gap = 8.0_f64;
    let sparks_total = if dock_state.detached {
        spark_w + gap
    } else {
        spark_w + spark_gap + spark_w + gap
    };

    let right_total = sparks_total + pulse_w + gap + errors_w + gap + conn_w;
    let right_x = (width - pad_h - right_total).max(pad_h);

    // Draw sparklines
    let spark_y = (36.0 - spark_h) / 2.0; // vertically center in 36px dock
    append_dock_sparkline(
        &mut geometry,
        &dock_state.event_spark.data,
        spark_w,
        spark_h,
        right_x,
        spark_y,
        theme.accent,
        0.15,
    );
    if !dock_state.detached {
        append_dock_sparkline(
            &mut geometry,
            &dock_state.activity_spark.data,
            spark_w,
            spark_h,
            right_x + spark_w + spark_gap,
            spark_y,
            theme.fg_dim,
            0.10,
        );
    }

    let text_right_x = right_x + sparks_total;

    if !dock_state.event_pulse.text.is_empty() {
        collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.event_pulse.text,
            text_right_x,
            pad_v + 4.0, // slightly lower for smaller text
            dock_state.event_pulse.font_size,
            font,
            &pulse_brush,
            atlas,
            &mut font_data_map,
        );
    }

    if !dock_state.global_errors.text.is_empty() {
        collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.global_errors.text,
            text_right_x + pulse_w + gap,
            pad_v + 4.0, // same size class as the pulse text, same offset
            dock_state.global_errors.font_size,
            font,
            &errors_brush,
            atlas,
            &mut font_data_map,
        );
    }

    collect_dock_text_glyphs(
        &mut glyphs,
        &dock_state.connection.text,
        text_right_x + pulse_w + gap + errors_w + gap,
        pad_v,
        dock_state.connection.font_size,
        font,
        &conn_brush,
        atlas,
        &mut font_data_map,
    );

    // Fill both, bump once: sparkline triangles ride the glyph version (see
    // `MsdfBlockGeometry`'s doc comment for why it has no counter of its own).
    msdf_geometry.vertices = geometry;
    msdf_glyphs.glyphs = glyphs;
    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1).max(1);
    let logical = logical_size(computed);
    rtt.built_width = logical.x;
    rtt.built_height = logical.y;
}

/// Render the South dock scene.
///
/// Layout: `[mode] [model] ... [activity] [block_activity] ... [contexts] ... [context_usage] [hints]`
pub fn render_south_dock(
    dock_state: Res<DockState>,
    theme: Res<Theme>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut query: Query<(&mut MsdfBlockGlyphs, &mut UiRttTexture, &ComputedNode), With<SouthDock>>,
    mut hit_regions: ResMut<DockHitRegions>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Ok((mut msdf_glyphs, mut rtt, computed)) = query.single_mut() else {
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
    let Some(ref mut atlas) = atlas else {
        return;
    };

    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
    let width = logical_width as f64;
    hit_regions.south_regions.clear();

    // Insets: 12px horizontal, 4px vertical
    let pad_h = 12.0_f64;
    let pad_v = 4.0_f64;
    let gap = 12.0_f64;

    // === Left group: mode + model ===
    let mut x = pad_h;

    let mode_brush = bevy_color_to_brush(dock_state.mode.color);
    let mode_w = collect_dock_text_glyphs(
        &mut glyphs,
        &dock_state.mode.text,
        x,
        pad_v,
        dock_state.mode.font_size,
        font,
        &mode_brush,
        atlas,
        &mut font_data_map,
    );
    x += mode_w + gap;

    // Context-bound widgets (model badge, activity counts, background jobs,
    // context badges, context usage) all skip rendering while the HUD is
    // detached (`DockState::detached`) — the footer carries only the mode
    // slot and hints on the fleet/world screens.
    if !dock_state.detached && !dock_state.model_badge.text.is_empty() {
        let model_brush = bevy_color_to_brush(dock_state.model_badge.color);
        let model_w = collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.model_badge.text,
            x,
            pad_v,
            dock_state.model_badge.font_size,
            font,
            &model_brush,
            atlas,
            &mut font_data_map,
        );
        x += model_w + gap;
    }

    // === Right group: context_usage + hints (right-aligned) ===
    let hints_brush = bevy_color_to_brush(theme.fg_dim);
    let hints_w = measure_text(&dock_state.hints.text, dock_state.hints.font_size, font);
    let hints_x = (width - pad_h - hints_w).max(x + gap);

    // context_usage sits immediately left of hints, in the same right-aligned group.
    if !dock_state.detached {
        let usage_brush = bevy_color_to_brush(dock_state.context_usage.color);
        let usage_w = measure_text(
            &dock_state.context_usage.text,
            dock_state.context_usage.font_size,
            font,
        );
        let usage_x = (hints_x - gap - usage_w).max(x + gap);

        collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.context_usage.text,
            usage_x,
            pad_v,
            dock_state.context_usage.font_size,
            font,
            &usage_brush,
            atlas,
            &mut font_data_map,
        );
    }

    collect_dock_text_glyphs(
        &mut glyphs,
        &dock_state.hints.text,
        hints_x,
        pad_v,
        dock_state.hints.font_size,
        font,
        &hints_brush,
        atlas,
        &mut font_data_map,
    );

    // === Middle area: activity + block_activity + contexts ===
    // Activity items go left-to-right from current x
    if !dock_state.detached && !dock_state.agent_activity.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.agent_activity.color);
        let w = collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.agent_activity.text,
            x,
            pad_v,
            dock_state.agent_activity.font_size,
            font,
            &brush,
            atlas,
            &mut font_data_map,
        );
        x += w + gap;
    }

    if !dock_state.detached && !dock_state.block_activity.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.block_activity.color);
        let w = collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.block_activity.text,
            x,
            pad_v,
            dock_state.block_activity.font_size,
            font,
            &brush,
            atlas,
            &mut font_data_map,
        );
        x += w + gap;
    }

    if !dock_state.detached && !dock_state.background_jobs.text.is_empty() {
        let brush = bevy_color_to_brush(dock_state.background_jobs.color);
        let w = collect_dock_text_glyphs(
            &mut glyphs,
            &dock_state.background_jobs.text,
            x,
            pad_v,
            dock_state.background_jobs.font_size,
            font,
            &brush,
            atlas,
            &mut font_data_map,
        );
        x += w + gap;
    }

    // Context badges — between activity and hints
    let ctx = &dock_state.contexts;
    if dock_state.detached {
        // No badges while detached — and `south_regions` stays cleared, so
        // even if the click handler's Conversation gate ever loosened,
        // there'd be nothing stale to hit.
    } else if let Some((ref source, ref preview)) = ctx.notification {
        // Notification mode: single text
        let notif_text = format!("\u{2190} @{}: \"{}\"", source, preview);
        let brush = bevy_color_to_brush(theme.accent);
        let w = collect_dock_text_glyphs(
            &mut glyphs, &notif_text, x, pad_v, 11.0, font, &brush, atlas, &mut font_data_map,
        );
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
            let w = collect_dock_text_glyphs(
                &mut glyphs, &label, x, pad_v, 11.0, font, &brush, atlas, &mut font_data_map,
            );
            let x_end = (x + w) as f32;
            hit_regions
                .south_regions
                .push((x_start, x_end, badge.context_id));
            x += w + badge_gap;
        }

        if ctx.overflow_count > 0 {
            let overflow_text = format!("+{}", ctx.overflow_count);
            let brush = bevy_color_to_brush(theme.fg_dim);
            let w = collect_dock_text_glyphs(
                &mut glyphs, &overflow_text, x, pad_v, 11.0, font, &brush, atlas,
                &mut font_data_map,
            );
            x += w + badge_gap;
        }

        if ctx.staged_count > 0 {
            let staged_text = format!("\u{00b7}{} staged", ctx.staged_count);
            let brush = bevy_color_to_brush(theme.fg_dim);
            collect_dock_text_glyphs(
                &mut glyphs, &staged_text, x, pad_v, 11.0, font, &brush, atlas,
                &mut font_data_map,
            );
        }
    }

    msdf_glyphs.glyphs = glyphs;
    msdf_glyphs.version = msdf_glyphs.version.wrapping_add(1).max(1);
    let logical = logical_size(computed);
    rtt.built_width = logical.x;
    rtt.built_height = logical.y;
}

/// Size each dock's render texture to its laid-out node (physical pixels) and
/// repoint the `ImageNode` when it changes. Mirrors `block_render`'s resize but
/// sizes from `ComputedNode` (full-width bar) rather than measured content.
pub fn resize_dock_textures(
    mut query: Query<
        (
            &ComputedNode,
            &mut UiRttTexture,
            &mut ImageNode,
            &MaterialNode<BlockFxMaterial>,
        ),
        Or<(With<NorthDock>, With<SouthDock>)>,
    >,
    text_metrics: Res<crate::text::TextMetrics>,
    gpu_limits: Res<GpuTextureLimits>,
    mut images: ResMut<Assets<Image>>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let scale = text_metrics.scale_factor;
    let max_dim = gpu_limits.max_texture_dim;

    for (computed, mut texture, mut image_node, mat_node) in query.iter_mut() {
        // ComputedNode is physical px; resize_rtt_texture expects logical.
        let size = logical_size(computed);
        let resized = crate::view::ui_rtt::resize_rtt_texture(
            &mut texture,
            &mut image_node,
            size.x,
            size.y,
            scale,
            max_dim,
            &mut images,
        );
        // The material draws the texture on screen (the ImageNode is a
        // non-drawing handle slot) — repoint it at the fresh allocation.
        if resized && let Some(mut mat) = fx_materials.get_mut(&mat_node.0) {
            mat.texture = texture.image.clone();
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

/// Whether a screen detaches the HUD from the active context (see
/// [`DockState::detached`]). `Room` and `Fsn` are fleet/world views — one
/// context's cockpit readout contradicts their stance. `Editor` and `Diff`
/// stay attached: both render content *of* the active context.
pub(crate) fn hud_detached(screen: crate::ui::screen::Screen) -> bool {
    use crate::ui::screen::Screen;
    matches!(screen, Screen::Room | Screen::Fsn)
}

/// The mode slot's label while the room owns the viewport: where you are,
/// not a vim mode — the zoomed station's engraved-nameplate label, or "ROOM"
/// at the carousel level.
pub(crate) fn room_slot_label(zoomed: Option<crate::view::room::nav::Station>) -> &'static str {
    match zoomed {
        Some(station) => station.label(),
        None => "ROOM",
    }
}

/// Keep [`DockState::detached`] in sync with the active screen. Writes only
/// on a real transition so an idle frame never dirties `DockState` (both
/// dock renders rebuild on its change detection).
pub fn sync_hud_detach(
    screen: Res<State<crate::ui::screen::Screen>>,
    mut dock: ResMut<DockState>,
) {
    let want = hud_detached(*screen.get());
    if dock.detached != want {
        dock.detached = want;
    }
}

/// Update mode widget text from vim state + focus area + screen.
///
/// When the user is in a text-editing surface (Compose/Dialog), shows the vim
/// editing mode (NORMAL/INSERT/VISUAL). Otherwise shows the app-level mode.
/// All labels come from the `mode_label_*` fields of `theme.toml` (CRDT-owned,
/// fetched over RPC from the kernel) — except the detached screens (Room/Fsn),
/// whose slot shows *where you are* ([`room_slot_label`]) instead of a vim
/// mode that has no surface behind it there.
pub fn update_mode(
    focus_area: Res<FocusArea>,
    screen: Res<State<crate::ui::screen::Screen>>,
    room: Res<crate::view::room::RoomState>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
    overlay_q: Query<&crate::view::components::InputOverlay>,
) {
    use crate::ui::screen::Screen;

    // Resolve vim mode from the active overlay (if any).
    let vim_mode = overlay_q.iter().next().and_then(|o| o.vim_mode.clone());

    let (color, label): (Color, &str) = match screen.get() {
        Screen::Conversation => match focus_area.as_ref() {
            FocusArea::Compose | FocusArea::Dialog => {
                vim_mode_to_dock(&vim_mode, &theme)
            }
            FocusArea::Conversation => (theme.mode_normal, &theme.mode_label_normal),
        },
        // The editor / diff viewer own the viewport and draw their own vim
        // mode on their own panels (docs/vi.md steps 4–5; the diff viewer's
        // status strip) — the dock just stays coherent instead of guessing.
        Screen::Editor | Screen::Diff => (theme.mode_normal, &theme.mode_label_normal),
        // Detached screens: the slot names the place, not a mode.
        Screen::Room => (theme.accent, room_slot_label(room.zoomed)),
        Screen::Fsn => (theme.accent, crate::view::room::nav::Station::Vfs.label()),
    };

    if dock.mode.text != label || dock.mode.color != color {
        dock.mode.text = label.to_string();
        dock.mode.color = color;
    }
}

/// Map a vim mode string from modalkit to a dock (color, label) pair.
fn vim_mode_to_dock<'a>(vim_mode: &Option<String>, theme: &'a Theme) -> (Color, &'a str) {
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

/// Render text for the global-error badge — the ambient HUD signal for
/// context-free failures (bad `theme.toml`, failed context creation, other
/// RPC errors with no conversation to attach to). These never become
/// conversation blocks because there's no context to attach them to, so
/// without this badge they're log-only and invisible in the UI.
///
/// `GlobalErrorQueue` already auto-dismisses entries after 10s
/// (`GlobalErrorQueue::gc`, driven from `update_connection_state`); this is
/// pure display logic over whatever currently survives that GC — it never
/// claims more than the queue holds, and returns `None` (nothing to show)
/// once the queue is empty.
///
/// - empty queue -> `None`
/// - one entry -> the operation + message, truncated to fit the dock
/// - several -> a count plus the most recent message, so a burst doesn't
///   just show a stale first error while newer ones silently pile up
/// - `last_repeat_count > 1` -> a `(×N)` suffix, since `GlobalErrorQueue`
///   collapses exact repeats into one entry rather than consuming new slots
///   (a reconnect storm would otherwise read as a single, misleadingly
///   unremarkable error).
fn format_global_error_badge(
    count: usize,
    last_operation: &str,
    last_message: &str,
    last_repeat_count: u32,
) -> Option<String> {
    if count == 0 {
        return None;
    }
    let msg = crate::text::truncate_chars(last_message, 40);
    let suffix = if last_repeat_count > 1 {
        format!(" (\u{d7}{last_repeat_count})")
    } else {
        String::new()
    };
    if count == 1 {
        Some(format!("\u{26a0} {last_operation}: {msg}{suffix}"))
    } else {
        Some(format!("\u{26a0} {count} errors (last: {msg}{suffix})"))
    }
}

/// Update the global-error badge from `GlobalErrorQueue`.
///
/// No `is_changed()` gate: `GlobalErrorQueue::gc` (called every frame from
/// `update_connection_state`) mutates the resource — and so bumps its
/// change tick — whether or not it actually removed anything, so gating on
/// `queue.is_changed()` would not skip any real work. Instead this only
/// touches `DockState` when the *displayed* text actually differs, which is
/// what gates the glyph rebuild in `render_north_dock`.
pub fn update_global_errors_badge(
    queue: Res<crate::view::components::GlobalErrorQueue>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    let text = match queue.entries.back() {
        Some(last) => format_global_error_badge(
            queue.entries.len(),
            &last.operation,
            &last.message,
            last.repeat_count,
        )
        .unwrap_or_default(),
        None => String::new(),
    };

    if dock.global_errors.text != text {
        dock.global_errors.text = text;
        dock.global_errors.color = theme.error;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_connection_error, count_block_activity, format_background_activity,
        format_block_activity, format_context_usage, format_elapsed_ms, format_global_error_badge,
        format_token_count, hud_detached, room_slot_label, BackgroundActivityLevel,
    };
    use crate::ui::theme::Theme;

    /// Room and Fsn are the fleet/world views — the HUD detaches from the
    /// active context there. Everything else (Conversation, and the two
    /// surfaces that render the active context's own content: Editor, Diff)
    /// stays attached. Exhaustive over `Screen` so a future screen variant
    /// forces a deliberate choice here.
    #[test]
    fn hud_detaches_on_fleet_views_only() {
        use crate::ui::screen::Screen;
        for screen in [
            Screen::Conversation,
            Screen::Editor,
            Screen::Room,
            Screen::Diff,
            Screen::Fsn,
        ] {
            let expect = matches!(screen, Screen::Room | Screen::Fsn);
            assert_eq!(hud_detached(screen), expect, "{screen:?}");
        }
    }

    /// The detached mode slot names the place: the zoomed station's
    /// engraved-nameplate label, or "ROOM" at the carousel level.
    #[test]
    fn room_slot_label_names_the_place() {
        use crate::view::room::nav::Station;
        assert_eq!(room_slot_label(None), "ROOM");
        assert_eq!(room_slot_label(Some(Station::TimeWell)), "TIME WELL");
        assert_eq!(room_slot_label(Some(Station::Switchboard)), "SWITCHBOARD");
    }

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

    // ------------------------------------------------------------------
    // format_global_error_badge — GlobalErrorQueue HUD rendering
    // ------------------------------------------------------------------

    #[test]
    fn global_error_badge_empty_queue_shows_nothing() {
        // No errors -> no badge, never a fabricated "0 errors".
        assert_eq!(format_global_error_badge(0, "op", "msg", 1), None);
    }

    #[test]
    fn global_error_badge_single_entry_shows_operation_and_message() {
        assert_eq!(
            format_global_error_badge(1, "config", "theme.toml: bad TOML", 1),
            Some("\u{26a0} config: theme.toml: bad TOML".to_string())
        );
    }

    #[test]
    fn global_error_badge_several_entries_shows_count_and_last_message() {
        assert_eq!(
            format_global_error_badge(3, "context", "create failed", 1),
            Some("\u{26a0} 3 errors (last: create failed)".to_string())
        );
    }

    #[test]
    fn global_error_badge_repeat_count_shows_multiplier_suffix() {
        // GlobalErrorQueue::push collapses exact repeats into one entry with
        // a growing repeat_count rather than consuming new slots — the
        // badge must surface that count, or a reconnect storm just looks
        // like one unremarkable error forever.
        assert_eq!(
            format_global_error_badge(1, "ssh", "no SSH agent", 7),
            Some("\u{26a0} ssh: no SSH agent (\u{d7}7)".to_string())
        );
    }

    #[test]
    fn global_error_badge_single_occurrence_has_no_multiplier_suffix() {
        assert_eq!(
            format_global_error_badge(1, "ssh", "no SSH agent", 1),
            Some("\u{26a0} ssh: no SSH agent".to_string())
        );
    }

    #[test]
    fn global_error_badge_truncates_long_messages() {
        let long = "x".repeat(80);
        let badge = format_global_error_badge(1, "op", &long, 1).unwrap();
        // "⚠ op: " prefix + 40-char truncated message (39 chars + ellipsis).
        let expected_msg = crate::text::truncate_chars(&long, 40);
        assert_eq!(badge, format!("\u{26a0} op: {expected_msg}"));
        assert!(badge.ends_with('…'));
    }

    // ------------------------------------------------------------------
    // count_block_activity — pure counting logic over document state
    // ------------------------------------------------------------------

    use kaijutsu_client::{ContextChange, ContextDelivery, ContextMirror};
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, PrincipalId, Status};

    fn test_block_id() -> kaijutsu_types::BlockId {
        kaijutsu_types::BlockId::new(
            kaijutsu_types::ContextId::new(),
            PrincipalId::new(),
            0,
        )
    }

    #[test]
    fn count_block_activity_counts_running_and_error_only() {
        let blocks = vec![
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Running)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Running)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Error)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Done)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Pending)
                .build(),
        ];
        assert_eq!(count_block_activity(&blocks), (2, 1));
    }

    #[test]
    fn count_block_activity_skips_excluded_blocks_for_both_counters() {
        // Defect 2 (exclusion half): a user-excluded block is the "I've
        // dealt with this" gesture and must not keep nagging the HUD, even
        // though it's still visible (dimmed) in the transcript.
        let blocks = vec![
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Error)
                .excluded(true)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Running)
                .excluded(true)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Error)
                .build(),
        ];
        assert_eq!(count_block_activity(&blocks), (0, 1));
    }

    // ------------------------------------------------------------------
    // State-derivation proofs — these exercise the REAL join/recovery code
    // path (docs/change-feed.md): `ContextMirror::apply_snapshot` for a
    // hydrate, `ContextMirror::receive` for a steady-state delivery. No
    // `ServerEvent::BlockStatusChanged`, and — post-migration — no CRDT of
    // any kind, is ever constructed; the count is derived fresh from
    // `mirror.blocks()` every time, proving it cannot drift from an
    // accumulator that was never built.
    // ------------------------------------------------------------------

    fn mirror_delivery(
        context_id: kaijutsu_types::ContextId,
        version: u64,
        events: Vec<ContextChange>,
    ) -> ContextDelivery {
        ContextDelivery {
            context_id,
            events: events
                .into_iter()
                .map(|change| kaijutsu_client::VersionedChange { version, change })
                .collect(),
            version,
        }
    }

    #[test]
    fn joining_a_context_with_preexisting_error_blocks_shows_nonzero_failed_count() {
        // Defect 1: a document that already had Running/Error blocks BEFORE
        // this client ever attached — the exact shape `ContextHydration::
        // Joined`'s `apply_snapshot` installs on a fresh join — must show a
        // correct count from the first frame, with zero feed deliveries in
        // its history.
        let ctx = kaijutsu_types::ContextId::new();
        let blocks = vec![
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Done)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Error)
                .build(),
            BlockSnapshotBuilder::new(test_block_id(), BlockKind::Text)
                .status(Status::Running)
                .build(),
        ];

        let mut mirror = ContextMirror::new(ctx);
        mirror.apply_snapshot(blocks, 1).expect("apply_snapshot");

        assert_eq!(count_block_activity(mirror.blocks()), (1, 1));
    }

    #[test]
    fn a_delivery_that_deletes_an_error_block_clears_it_from_the_count() {
        // Defect 2 (deletion half): the failed block is deleted server-side
        // and the change feed delivers that fact directly — no
        // `BlockStatusChanged` fabricated, and (unlike the pre-migration
        // CRDT resync this test used to drive) no full resnapshot needed
        // either: an ordinary `BlockDeleted` delivery must drop it from the
        // count on its own.
        let ctx = kaijutsu_types::ContextId::new();
        let failed_id = test_block_id();
        let mut mirror = ContextMirror::new(ctx);
        mirror
            .apply_snapshot(
                vec![BlockSnapshotBuilder::new(failed_id, BlockKind::Text)
                    .status(Status::Error)
                    .build()],
                1,
            )
            .unwrap();
        assert_eq!(count_block_activity(mirror.blocks()), (0, 1), "sanity: starts failed");

        mirror
            .receive(mirror_delivery(
                ctx,
                2,
                vec![ContextChange::BlockDeleted { block_id: failed_id }],
            ))
            .expect("apply delivery");

        assert_eq!(count_block_activity(mirror.blocks()), (0, 0));
    }

    #[test]
    fn a_delivery_that_excludes_an_error_block_clears_it_from_the_count() {
        // Defect 2 (exclusion half): the block is not deleted, just
        // excluded — same "must clear from the count without ever seeing a
        // fabricated status change" shape as the deletion case above.
        let ctx = kaijutsu_types::ContextId::new();
        let failed_id = test_block_id();
        let mut mirror = ContextMirror::new(ctx);
        mirror
            .apply_snapshot(
                vec![BlockSnapshotBuilder::new(failed_id, BlockKind::Text)
                    .status(Status::Error)
                    .build()],
                1,
            )
            .unwrap();
        assert_eq!(count_block_activity(mirror.blocks()), (0, 1), "sanity: starts failed");

        mirror
            .receive(mirror_delivery(
                ctx,
                2,
                vec![ContextChange::ExcludedChanged {
                    block_id: failed_id,
                    excluded: true,
                }],
            ))
            .expect("apply delivery");

        assert_eq!(count_block_activity(mirror.blocks()), (0, 0));
    }

    #[test]
    fn format_block_activity_idle_is_empty() {
        let theme = Theme::default();
        let (text, color) = format_block_activity(0, 0, &theme);
        assert_eq!(text, "");
        assert_eq!(color, theme.accent);
    }

    #[test]
    fn format_block_activity_running_only_is_accent_colored() {
        let theme = Theme::default();
        let (text, color) = format_block_activity(2, 0, &theme);
        assert_eq!(text, "2 running");
        assert_eq!(color, theme.accent);
    }

    #[test]
    fn format_block_activity_failed_only_is_error_colored() {
        let theme = Theme::default();
        let (text, color) = format_block_activity(0, 1, &theme);
        assert_eq!(text, "1 failed");
        assert_eq!(color, theme.error);
    }

    #[test]
    fn format_block_activity_running_and_failed_shows_both_error_colored() {
        // A failure tints the WHOLE widget theme.error, even while blocks
        // are still running — the failure is the more urgent signal.
        let theme = Theme::default();
        let (text, color) = format_block_activity(2, 1, &theme);
        assert_eq!(text, "2 running, 1 failed");
        assert_eq!(color, theme.error);
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
        Screen::Diff => {
            "j/k: line \u{2502} ]c/[c: hunk \u{2502} V: select \u{2502} y: yank \u{2502} q: close"
        }
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
        (format!("~{} ev", total), theme.accent)
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

/// Cached running/failed counts for the BlockActivity widget, derived from
/// document STATE rather than accumulated from the event stream.
///
/// An earlier version of this counted `ServerEvent::BlockStatusChanged`
/// transitions incrementally (`running += 1` / `-= 1` as events arrived). A
/// kaibo review found that accumulator drifts from what's actually on
/// screen in three ways:
///
/// 1. `ContextJoined`'s initial sync and a post-lag `ContextResynced` both
///    replace document state directly (`view/sync.rs` -> `DocumentCache::
///    apply_sync`) and emit NO events — so joining a context that already
///    has Running/Error blocks left the badge blank. Worse for `failed`
///    than `running`: a stuck Running block eventually gets a correcting
///    Done/Error event, but a terminal Error never emits again, so the
///    badge would disagree with the screen for the rest of the session.
/// 2. `BlockDeleted` / `BlockExcludedChanged` were never matched by the old
///    event loop — excluding or deleting a failed block left the count
///    stuck.
/// 3. A broadcast-lag drop (`connection/actor_plugin.rs`) loses whatever
///    transitions were in flight, and per (1) the resync that follows
///    doesn't regenerate them — the counters never recover.
///
/// Deriving the counts fresh from `ContextMirror::blocks()` whenever the
/// mirror's version changes makes all three impossible by construction:
/// there is no accumulator to drift out of sync with the document, because
/// there is no accumulator.
#[derive(Default)]
pub(crate) struct BlockActivityState {
    /// `(active context, that document's sync version)` at the last
    /// recompute — the cheap gate so the O(blocks) walk in
    /// `count_block_activity` only runs when the document actually changed,
    /// not on every frame.
    last_seen: Option<(kaijutsu_types::ContextId, u64)>,
    /// Cached from the last recompute; read every frame for the sparkline
    /// sample even on frames the gate above skips recomputation.
    running: u32,
    failed: u32,
    last_spark_sample: f64,
}

/// Count live Running/Error blocks in a document snapshot.
///
/// Excluded blocks are skipped for both counts: user exclusion (`docs`'
/// remediate-a-poisoned-conversation flow — exclude, then fork) is the
/// user's "I've dealt with this" gesture, so an excluded Error block should
/// stop nagging the HUD even though it's still visible (dimmed) in the
/// transcript. Deleted blocks never appear here at all — `ContextMirror`
/// removes a deleted block from its list outright (`ContextChange::
/// BlockDeleted`, docs/change-feed.md), so no explicit check is needed for
/// those.
fn count_block_activity(blocks: &[kaijutsu_types::BlockSnapshot]) -> (u32, u32) {
    let mut running = 0u32;
    let mut failed = 0u32;
    for b in blocks {
        if b.excluded {
            continue;
        }
        match b.status {
            kaijutsu_types::Status::Running => running += 1,
            kaijutsu_types::Status::Error => failed += 1,
            _ => {}
        }
    }
    (running, failed)
}

/// Render the BlockActivity widget's (text, color) from running/failed
/// counts. Any failures tint the whole widget `theme.error` — a red count
/// alongside an otherwise-normal running count is more legible at a glance
/// than a single color trying to average two different signals.
fn format_block_activity(running: u32, failed: u32, theme: &Theme) -> (String, Color) {
    if failed > 0 {
        let text = if running > 0 {
            format!("{running} running, {failed} failed")
        } else {
            format!("{failed} failed")
        };
        (text, theme.error)
    } else if running > 0 {
        (format!("{running} running"), theme.accent)
    } else {
        (String::new(), theme.accent)
    }
}

/// Update block activity — shows running + failed block counts for the
/// active document, re-derived from document state whenever its sync
/// version changes (see `BlockActivityState`), never from the event stream.
pub fn update_block_activity(
    mut state: Local<BlockActivityState>,
    time: Res<Time>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut dock: ResMut<DockState>,
) {
    let active = doc_cache
        .active_id()
        .and_then(|id| doc_cache.get(id).map(|entry| (id, entry)));
    let seen = active.as_ref().map(|(id, entry)| (*id, entry.mirror.version()));

    if seen != state.last_seen {
        state.last_seen = seen;
        let (running, failed) = active
            .map(|(_, entry)| count_block_activity(entry.mirror.blocks()))
            .unwrap_or_default();
        state.running = running;
        state.failed = failed;

        let (text, color) = format_block_activity(state.running, state.failed, &theme);
        if dock.block_activity.text != text || dock.block_activity.color != color {
            dock.block_activity.text = text;
            dock.block_activity.color = color;
        }
    }

    // Sample running block count for sparkline every 250ms. Reuses the
    // cached count above rather than re-deriving — the gate already
    // guarantees it's current as of the last real document change.
    let now = time.elapsed_secs_f64();
    if now - state.last_spark_sample >= 0.25 {
        state.last_spark_sample = now;
        dock.activity_spark.push(state.running as f64);
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the MSDF-drawn dock bars.
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
                    sync_hud_detach,
                    update_mode,
                    update_connection,
                    update_global_errors_badge,
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
