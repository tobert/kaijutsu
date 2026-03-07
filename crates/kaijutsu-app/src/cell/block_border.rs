//! Vello-rendered block borders (fieldset/legend style).
//!
//! Each bordered block gets a `UiVelloScene` on the same entity, drawing a fieldset
//! border. The scene is NOT a child entity — child entities would cause Taffy to
//! ignore ContentSize from UiVelloText, collapsing block height to just padding.

use bevy::prelude::*;
use bevy_vello::prelude::UiVelloScene;

use kaijutsu_types::ToolKind;

use crate::view::{
    BlockCell, BlockKind, BlockSnapshot, CellEditor, DriftKind, MainCell,
    BlockCellContainer, EditorEntities, Role,
};
use crate::view::fieldset;
use crate::text::{FontHandles, TextMetrics};
use crate::ui::theme::Theme;
use crate::connection::RpcConnectionState;
use crate::ui::drift::DriftState;
use crate::view::document::DocumentCache;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Visual style for a block's border.
#[derive(Component, Debug, Clone, PartialEq, Reflect)]
#[reflect(Component)]
pub struct BlockBorderStyle {
    pub kind: BorderKind,
    pub color: Color,
    pub thickness: f32,
    pub corner_radius: f32,
    /// Padding inside the border (clearance for text).
    pub padding: BorderPadding,
    pub animation: BorderAnimation,
    /// Top label text (e.g. "tool call: grep", "thinking", "drift: push").
    #[reflect(ignore)]
    pub top_label: Option<String>,
    /// Bottom label text (e.g. "running", "done", "error").
    #[reflect(ignore)]
    pub bottom_label: Option<String>,
}

/// Simplified padding (top, bottom, left, right in pixels).
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct BorderPadding {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

impl Default for BorderPadding {
    fn default() -> Self {
        Self {
            top: 8.0,
            bottom: 6.0,
            left: 12.0,
            right: 12.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect, Default)]
pub enum BorderKind {
    /// Complete rectangle border.
    #[default]
    Full,
    /// Top line only (short results).
    TopAccent,
    /// Dashed rectangle (thinking).
    Dashed,
    /// Top + left + right edges, no bottom (tool call with result below).
    OpenBottom,
    /// Left + right + bottom edges, horizontal divider at top (tool result connected to call above).
    OpenTop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect, Default)]
pub enum BorderAnimation {
    /// Static border.
    #[default]
    None,
    /// Traveling light (running tool call).
    Chase,
    /// Breathing pulse (error).
    Pulse,
    /// Subtle breathing (thinking expanded).
    Breathe,
}

/// Marker that the block cell has a `UiVelloScene` border drawn on it.
///
/// The border is rendered directly on the BlockCell entity (not a child),
/// so that Taffy continues to use `ContentSize` from `UiVelloText` for sizing.
/// (Child entities turn the parent into a container, causing Taffy to ignore ContentSize.)
#[derive(Component)]
pub struct BlockBorderActive;

// ============================================================================
// SYSTEMS
// ============================================================================

/// Context for computing border labels (username, model name).
struct BorderContext {
    username: String,
    model: String,
}

/// Examine each BlockCell's snapshot and add/update/remove `BlockBorderStyle`.
///
/// Runs in CellPhase::Buffer (after sync_block_cell_buffers).
pub fn determine_block_border_style(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    block_cells: Query<(Entity, &BlockCell, Option<&BlockBorderStyle>)>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
    layout_gen: Res<super::components::LayoutGeneration>,
    conn_state: Res<RpcConnectionState>,
    drift_state: Res<DriftState>,
    doc_cache: Res<DocumentCache>,
    mut last_gen: Local<u64>,
) {
    // Border styles only change when blocks change (add/remove/line count/status)
    if layout_gen.0 == *last_gen {
        return;
    }
    *last_gen = layout_gen.0;

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let blocks_vec = editor.blocks();
    let blocks: std::collections::HashMap<_, _> = blocks_vec
        .iter()
        .map(|b| (b.id, b))
        .collect();

    // Build set of tool_call_ids that have a ToolResult
    let has_result: std::collections::HashSet<_> = blocks_vec
        .iter()
        .filter(|b| b.kind == BlockKind::ToolResult)
        .filter_map(|b| b.tool_call_id)
        .collect();

    // Build context for labels
    let ctx = BorderContext {
        username: conn_state.identity.as_ref().map(|i| i.username.clone()).unwrap_or_default(),
        model: doc_cache.active_id()
            .and_then(|ctx_id| drift_state.contexts.iter().find(|c| c.id == ctx_id))
            .map(|c| c.model.clone())
            .unwrap_or_default(),
    };

    for &entity in container.block_cells.values() {
        let Ok((ent, block_cell, existing_style)) = block_cells.get(entity) else {
            continue;
        };

        let Some(block) = blocks.get(&block_cell.block_id) else {
            // Block removed — strip border if present
            if existing_style.is_some() {
                commands.entity(ent).remove::<BlockBorderStyle>();
            }
            continue;
        };

        let has_result_below = has_result.contains(&block.id);
        let new_style = compute_border_style(block, &theme, &ctx, has_result_below, text_metrics.cell_font_size);

        match (&new_style, existing_style) {
            (Some(style), Some(existing)) if style == existing => {
                // Style unchanged — skip insert to avoid triggering change detection
            }
            (Some(style), _) => {
                commands.entity(ent).insert(style.clone());
            }
            (None, Some(_)) => {
                commands.entity(ent).remove::<BlockBorderStyle>();
            }
            (None, None) => {} // no border needed
        }
    }
}

/// Decide border style for a block based on kind, status, and content.
///
/// `has_result`: true if this ToolCall block has a paired ToolResult below.
fn compute_border_style(
    block: &BlockSnapshot,
    theme: &Theme,
    ctx: &BorderContext,
    has_result: bool,
    font_size: f32,
) -> Option<BlockBorderStyle> {
    use kaijutsu_crdt::Status;

    // Padding scales with font size: block_border_padding is a multiplier
    let base = theme.block_border_padding * font_size;
    let padding = BorderPadding {
        top: base * 0.5,
        bottom: base * 0.375,
        left: base * 0.75,
        right: base * 0.75,
    };

    match block.kind {
        BlockKind::ToolCall => {
            let (animation, color) = match block.status {
                Status::Running | Status::Pending => {
                    (BorderAnimation::Chase, theme.block_border_tool_call)
                }
                _ => {
                    // Unified boxes (with result below) keep higher opacity for visible sides
                    let alpha = if has_result { 0.85 } else { 0.7 };
                    (BorderAnimation::None, theme.block_border_tool_call.with_alpha(alpha))
                }
            };

            // Top label: "COMMAND @username" for shell, "TOOL CALL model" for others
            let top_label = match block.tool_kind {
                Some(ToolKind::Shell) => {
                    if ctx.username.is_empty() {
                        "COMMAND".to_string()
                    } else {
                        format!("COMMAND @{}", ctx.username)
                    }
                }
                _ => {
                    if ctx.model.is_empty() {
                        "TOOL CALL".to_string()
                    } else {
                        let model = ctx.model.rsplit('/').next().unwrap_or(&ctx.model);
                        format!("TOOL CALL {}", model)
                    }
                }
            };

            // Status label on the call only when there's no result block yet
            let status_label = if has_result {
                None // status moves to the result's bottom label
            } else {
                match block.status {
                    Status::Running => Some("running".to_string()),
                    Status::Pending => Some("pending".to_string()),
                    Status::Done => None,
                    Status::Error => Some("error".to_string()),
                }
            };

            // Use OpenBottom when paired with a result block
            let kind = if has_result {
                BorderKind::OpenBottom
            } else {
                BorderKind::Full
            };

            Some(BlockBorderStyle {
                kind,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding,
                animation,
                top_label: Some(top_label),
                bottom_label: status_label,
            })
        }
        BlockKind::ToolResult => {
            let content = block.content.trim();
            let has_output = block.output.is_some();
            if content.is_empty() && !has_output && !block.is_error {
                return None; // empty success — no border
            }

            let has_paired_call = block.tool_call_id.is_some();
            let color = if block.is_error {
                theme.block_border_error
            } else if has_paired_call {
                // Match the ToolCall's alpha for a unified box look
                theme.block_border_tool_call.with_alpha(0.85)
            } else {
                theme.block_border_tool_call
            };
            let animation = if block.is_error {
                BorderAnimation::Pulse
            } else {
                BorderAnimation::None
            };

            // Connected to call above → OpenTop, standalone → Full
            let kind = if has_paired_call {
                BorderKind::OpenTop
            } else {
                BorderKind::Full
            };

            // Status label on the bottom
            let status_label = match block.status {
                Status::Running => Some("running".to_string()),
                Status::Pending => Some("pending".to_string()),
                Status::Done => None,
                Status::Error => Some("error".to_string()),
            };

            Some(BlockBorderStyle {
                kind,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding: BorderPadding {
                    top: if has_paired_call { base * 0.25 } else { padding.top },
                    ..padding
                },
                animation,
                top_label: None, // divider line, no label
                bottom_label: status_label,
            })
        }
        BlockKind::Thinking => {
            if block.collapsed {
                None // collapsed thinking — no border
            } else {
                Some(BlockBorderStyle {
                    kind: BorderKind::Dashed,
                    color: theme.block_border_thinking,
                    thickness: theme.block_border_thickness,
                    corner_radius: theme.block_border_corner_radius,
                    padding,
                    animation: BorderAnimation::Breathe,
                    top_label: Some("thinking".to_string()),
                    bottom_label: None,
                })
            }
        }
        BlockKind::Drift => match block.drift_kind {
            Some(DriftKind::Pull) | Some(DriftKind::Distill) | Some(DriftKind::Merge) => {
                let drift_label = match block.drift_kind {
                    Some(DriftKind::Pull) => "drift: pull",
                    Some(DriftKind::Distill) => "drift: distill",
                    Some(DriftKind::Merge) => "drift: merge",
                    _ => "drift",
                };
                Some(BlockBorderStyle {
                    kind: BorderKind::Full,
                    color: theme.block_border_drift,
                    thickness: theme.block_border_thickness,
                    corner_radius: theme.block_border_corner_radius,
                    padding,
                    animation: BorderAnimation::None,
                    top_label: Some(drift_label.to_string()),
                    bottom_label: None,
                })
            }
            _ => None,
        },
        BlockKind::Text => {
            let color = match block.role {
                Role::User => theme.block_border_user,
                _ => theme.block_border_assistant,
            };
            // Skip if fully transparent (default)
            if color.alpha() < 0.01 {
                return None;
            }
            Some(BlockBorderStyle {
                kind: BorderKind::TopAccent,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding,
                animation: BorderAnimation::None,
                top_label: None,
                bottom_label: None,
            })
        }
        // File, Drift Push/Commit — no border
        _ => None,
    }
}

/// Add `UiVelloScene` directly to BlockCells that have a border style.
///
/// The scene is rendered on the same entity as `UiVelloText` so Taffy continues
/// to use `ContentSize` (from bevy_vello's text measurement) for height sizing.
/// Using a child entity would make Taffy treat the block as a container and
/// ignore ContentSize, collapsing the height to just the padding (3-4px).
///
/// Runs in PostUpdate (after UiSystems::Layout so ComputedNode is available).
pub fn spawn_vello_borders(
    mut commands: Commands,
    block_cells: Query<Entity, (With<BlockBorderStyle>, Without<BlockBorderActive>)>,
) {
    for entity in block_cells.iter() {
        commands.entity(entity).insert((
            UiVelloScene::default(),
            BlockBorderActive,
        ));
    }
}

/// Rebuild border scenes when style or size changes.
///
/// Runs in PostUpdate (after UiSystems::Layout).
pub fn update_vello_borders(
    mut block_cells: Query<
        (&BlockBorderStyle, &mut UiVelloScene, &ComputedNode),
        Or<(Changed<BlockBorderStyle>, Changed<ComputedNode>)>,
    >,
    fonts: Res<Assets<bevy_vello::prelude::VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
) {
    let font = fonts.get(&font_handles.mono);

    for (style, mut scene_component, computed) in block_cells.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let mut scene = bevy_vello::vello::Scene::new();
        fieldset::build_fieldset_border(
            &mut scene,
            size.x as f64,
            size.y as f64,
            style,
            style.top_label.as_deref(),
            style.bottom_label.as_deref(),
            font,
            0.0, // initial time — animate_vello_borders handles ongoing animation
            theme.bg,
        );

        *scene_component = UiVelloScene::from(scene);
    }
}

/// Animate borders every frame for blocks with active animations.
///
/// Runs in PostUpdate (after update_vello_borders).
pub fn animate_vello_borders(
    time: Res<Time>,
    mut block_cells: Query<(&BlockBorderStyle, &mut UiVelloScene, &ComputedNode)>,
    fonts: Res<Assets<bevy_vello::prelude::VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
) {
    let t = time.elapsed_secs();
    let font = fonts.get(&font_handles.mono);

    for (style, mut scene_component, computed) in block_cells.iter_mut() {
        // Only animate blocks with active animations
        if style.animation == BorderAnimation::None {
            continue;
        }

        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let mut scene = bevy_vello::vello::Scene::new();
        fieldset::build_fieldset_border(
            &mut scene,
            size.x as f64,
            size.y as f64,
            style,
            style.top_label.as_deref(),
            style.bottom_label.as_deref(),
            font,
            t,
            theme.bg,
        );

        *scene_component = UiVelloScene::from(scene);
    }
}

/// Clean up border scenes when BlockCell loses its style.
pub fn cleanup_block_borders(
    mut commands: Commands,
    // BlockCells that have BlockBorderActive but no BlockBorderStyle
    removed_style: Query<Entity, (With<BlockBorderActive>, Without<BlockBorderStyle>)>,
) {
    for entity in removed_style.iter() {
        commands.entity(entity).remove::<(UiVelloScene, BlockBorderActive)>();
    }
}
