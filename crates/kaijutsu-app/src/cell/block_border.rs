//! Vello-rendered block borders (fieldset/legend style).
//!
//! Each bordered block gets a `UiVelloScene` on the same entity, drawing a fieldset
//! border. The scene is NOT a child entity — child entities would cause Taffy to
//! ignore ContentSize from UiVelloText, collapsing block height to just padding.

use bevy::prelude::*;

use kaijutsu_types::ToolKind;

use crate::connection::RpcConnectionState;
use crate::text::TextMetrics;
use crate::ui::drift::DriftState;
use crate::ui::theme::Theme;
use crate::view::document::DocumentCache;
use crate::view::{
    BlockCell, BlockCellContainer, BlockKind, BlockSnapshot, CellEditor, DriftKind, EditorEntities,
    MainCell, Role,
};

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

/// Measured label gap positions (pixel coordinates within the block texture).
///
/// Computed during `build_block_scenes()` where font metrics are available.
/// Read by `sync_block_fx()` to populate the `label_gaps` shader uniform.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Reflect)]
#[reflect(Component)]
pub struct BorderLabelMetrics {
    /// Top label gap: horizontal start (px from left edge of node).
    pub top_gap_x0: f32,
    /// Top label gap: horizontal end (px from left edge of node).
    pub top_gap_x1: f32,
    /// Bottom label gap: horizontal start (px from left edge of node).
    pub bottom_gap_x0: f32,
    /// Bottom label gap: horizontal end (px from left edge of node).
    pub bottom_gap_x1: f32,
    /// Border inset from top edge (px). Moves border stroke inward so
    /// the label can straddle it fieldset/legend-style. 0 = default (1px AA inset).
    pub border_inset_top: f32,
    /// Border inset from bottom edge (px). 0 = default (1px AA inset).
    pub border_inset_bottom: f32,
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

/// Per-block excluded state, propagated to ECS for shader-driven gutter indicator.
///
/// The `block_fx.wgsl` shader draws a small SDF circle in the right gutter zone:
/// filled dot when included, hollow ring + strikethrough when excluded.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Reflect, Default)]
#[reflect(Component)]
pub struct BlockExcludedState(pub bool);

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
    block_cells: Query<(Entity, &BlockCell, Option<&BlockBorderStyle>, Option<&BlockExcludedState>)>,
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
    let blocks: std::collections::HashMap<_, _> = blocks_vec.iter().map(|b| (b.id, b)).collect();

    // Build set of tool_call_ids that have a *visible* ToolResult.
    // Empty success results render no border, so the call should not
    // use OpenBottom to connect to an invisible block.
    let has_result: std::collections::HashSet<_> = blocks_vec
        .iter()
        .filter(|b| b.kind == BlockKind::ToolResult)
        .filter(|b| !b.content.trim().is_empty() || b.output.is_some() || b.is_error)
        .filter_map(|b| b.tool_call_id)
        .collect();

    // Build context for labels
    let ctx = BorderContext {
        username: conn_state
            .identity
            .as_ref()
            .map(|i| i.username.clone())
            .unwrap_or_default(),
        model: doc_cache
            .active_id()
            .and_then(|ctx_id| drift_state.contexts.iter().find(|c| c.id == ctx_id))
            .map(|c| c.model.clone())
            .unwrap_or_default(),
    };

    for &entity in container.block_cells.values() {
        let Ok((ent, block_cell, existing_style, existing_excluded)) = block_cells.get(entity)
        else {
            continue;
        };

        let Some(block) = blocks.get(&block_cell.block_id) else {
            // Block removed — strip border if present
            if existing_style.is_some() {
                commands.entity(ent).remove::<BlockBorderStyle>();
            }
            continue;
        };

        // Propagate excluded state to ECS for the gutter indicator shader
        let new_excluded = BlockExcludedState(block.excluded);
        if existing_excluded != Some(&new_excluded) {
            commands.entity(ent).insert(new_excluded);
        }

        let has_result_below = has_result.contains(&block.id);
        let new_style = compute_border_style(
            block,
            &theme,
            &ctx,
            has_result_below,
            text_metrics.cell_font_size,
        );

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

    // Padding scales with font size: block_border_padding is a multiplier.
    // This defines the clearance between the border stroke and text content.
    let base = theme.block_border_padding * font_size;
    let padding = BorderPadding {
        top: base * 0.75,
        bottom: base * 0.6,
        left: base,
        // Extra right padding reserves space for the gutter inclusion indicator
        right: base * 1.5,
    };

    let mut result = match block.kind {
        BlockKind::ToolCall => {
            let (animation, color) = match block.status {
                Status::Running | Status::Pending => {
                    (BorderAnimation::Chase, theme.block_border_tool_call)
                }
                _ => (
                    BorderAnimation::None,
                    theme.block_border_tool_call.with_alpha(0.85),
                ),
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
                    Status::Done => Some("done".to_string()),
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
                    top: if has_paired_call {
                        base * 0.5
                    } else {
                        padding.top
                    },
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
            Some(DriftKind::Pull)
            | Some(DriftKind::Distill)
            | Some(DriftKind::Merge)
            | Some(DriftKind::Fork) => {
                let drift_label = match block.drift_kind {
                    Some(DriftKind::Pull) => "drift: pull",
                    Some(DriftKind::Distill) => "drift: distill",
                    Some(DriftKind::Merge) => "drift: merge",
                    Some(DriftKind::Fork) => "fork",
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
        BlockKind::Error => {
            let (color, animation, border_kind) =
                match block.error.as_ref().map(|e| e.severity) {
                    Some(kaijutsu_types::ErrorSeverity::Warning) => (
                        theme.block_border_error_warning,
                        BorderAnimation::None,
                        BorderKind::Dashed,
                    ),
                    Some(kaijutsu_types::ErrorSeverity::Fatal) => (
                        theme.block_border_error_fatal,
                        BorderAnimation::Pulse,
                        BorderKind::Full,
                    ),
                    _ => (
                        theme.block_border_error,
                        BorderAnimation::Pulse,
                        BorderKind::Full,
                    ),
                };
            let severity_label = block
                .error
                .as_ref()
                .map(|e| e.severity.as_str())
                .unwrap_or("error");
            let category_label = block
                .error
                .as_ref()
                .map(|e| e.category.as_str())
                .unwrap_or("error");
            Some(BlockBorderStyle {
                kind: border_kind,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding,
                animation,
                top_label: Some(format!("{} {}", category_label, severity_label)),
                bottom_label: None,
            })
        }
        // File, Drift Push/Commit — no border
        _ => None,
    };

    // Post-process: dim excluded blocks (gutter indicator is shader-driven)
    if block.excluded {
        if let Some(ref mut style) = result {
            // Dim the border color to indicate exclusion
            let dimmed = style.color.with_alpha(style.color.alpha() * 0.35);
            style.color = dimmed;
            // Override animation — excluded blocks shouldn't animate
            style.animation = BorderAnimation::None;
        }
    }

    result
}

