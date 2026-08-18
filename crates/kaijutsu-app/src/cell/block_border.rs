//! Shader-drawn block borders (fieldset/legend style).
//!
//! `BlockBorderStyle` is pure data — an SDF border description consumed by
//! `shaders::sync_block_fx` and rasterized by `assets/shaders/block_fx.wgsl`
//! against the block's MSDF-rendered content texture. No vello scene, no
//! child entities: the border lives entirely in `MaterialNode<BlockFxMaterial>`
//! uniforms on the surface's own entity.
//!
//! # Entity-free core
//!
//! [`compute_border_style`] and [`apply_focus_style`] are **entity-free** and
//! take [`BorderInputs`] — the handful of snapshot fields the decision
//! actually reads — rather than a `BlockSnapshot` or an ECS query. The
//! conversation surface (`view::surface::chrome`) carries the same struct in
//! its content cache and calls these directly, so there is exactly one place
//! the border rules live.

use bevy::prelude::*;

use kaijutsu_types::{BlockId, ErrorCategory, ErrorSeverity, Status, ToolKind};

use crate::ui::theme::Theme;
use crate::view::{BlockKind, BlockSnapshot, DriftKind, Role};

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
    /// A single full-width horizontal rule through the node's vertical
    /// center — no box, no insets. Used only by the role-group divider
    /// (`view::block_render::sync_role_group_headers`); the label straddles
    /// a gap in the line via the same `label_gaps` mechanism as fieldset
    /// labels.
    CenterLine,
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
#[derive(Debug, Default, Clone)]
pub struct BorderContext {
    pub username: String,
    pub model: String,
}

/// Everything [`compute_border_style`] reads out of a block.
///
/// A `Copy` projection of `BlockSnapshot`, deliberately narrow: it is what
/// lets the border rules run somewhere that has no snapshot in hand (the
/// surface's `BlockContentCache` stores one of these per block and never
/// touches the block store again). `content` collapses to the one question the
/// rules ask of it — is it blank — so the struct stays allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderInputs {
    pub id: BlockId,
    pub kind: BlockKind,
    pub role: Role,
    pub status: Status,
    pub tool_kind: Option<ToolKind>,
    pub tool_call_id: Option<BlockId>,
    /// `content.trim().is_empty()` — an empty successful tool result draws no
    /// border at all.
    pub content_empty: bool,
    pub has_output: bool,
    pub is_error: bool,
    pub collapsed: bool,
    pub excluded: bool,
    pub drift_kind: Option<DriftKind>,
    /// `(category, severity)` from a structured error payload.
    pub error: Option<(ErrorCategory, ErrorSeverity)>,
}

impl BorderInputs {
    pub fn from_snapshot(block: &BlockSnapshot) -> Self {
        Self {
            id: block.id,
            kind: block.kind,
            role: block.role,
            status: block.status,
            tool_kind: block.tool_kind,
            tool_call_id: block.tool_call_id,
            content_empty: block.content.trim().is_empty(),
            has_output: block.output.is_some(),
            is_error: block.is_error,
            collapsed: block.collapsed,
            excluded: block.excluded,
            drift_kind: block.drift_kind,
            error: block.error.as_ref().map(|e| (e.category, e.severity)),
        }
    }

    /// Whether this block is a *visible* tool result — the predicate that
    /// decides whether the call above it joins to it (`OpenBottom`) or closes
    /// itself off (`Full`).
    pub fn is_visible_tool_result(&self) -> bool {
        self.kind == BlockKind::ToolResult
            && (!self.content_empty || self.has_output || self.is_error)
    }
}

/// Decide border style for a block based on kind, status, and content.
///
/// Pure and entity-free — see the module docs. `has_result`: true if this
/// ToolCall block has a paired ToolResult below.
///
/// `font_size` scales the padding, and that padding is **layout**: the
/// surface path subtracts it from the wrap width (`view::surface::chrome`).
/// Changing it changes where lines break.
pub fn compute_border_style(
    block: &BorderInputs,
    theme: &Theme,
    ctx: &BorderContext,
    has_result: bool,
    font_size: f32,
) -> Option<BlockBorderStyle> {
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
                    // A draft is what you are typing; labelling it "draft"
                    // would narrate the compose box back at you.
                    Status::Draft => None,
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
            if !block.is_visible_tool_result() {
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
                // As above: a draft is the thing being typed, not a state to
                // report back.
                Status::Draft => None,
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
            let (color, animation, border_kind) = match block.error.map(|(_, severity)| severity) {
                Some(ErrorSeverity::Warning) => (
                    theme.block_border_error_warning,
                    BorderAnimation::None,
                    BorderKind::Dashed,
                ),
                Some(ErrorSeverity::Fatal) => (
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
                .map(|(_, severity)| severity.as_str())
                .unwrap_or("error");
            let category_label = block
                .error
                .map(|(category, _)| category.as_str())
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
    if block.excluded
        && let Some(ref mut style) = result
    {
        // Dim the border color to indicate exclusion
        let dimmed = style.color.with_alpha(style.color.alpha() * 0.35);
        style.color = dimmed;
        // Override animation — excluded blocks shouldn't animate
        style.animation = BorderAnimation::None;
    }

    result
}

/// Overlay keyboard-focus feedback (`FocusTarget.block_id`, moved by j/k)
/// onto a block's computed border style.
///
/// Focus lives on the border because the border is the only per-block
/// visual that can exist for every block kind: the previous indicator — a
/// 1.15× brighten of the plain-text color — changed zero pixels on
/// markdown blocks, whose spans carry their own theme brushes, so focus
/// moved with nothing on screen showing it (2026-08-04 live debug). Two
/// rules keep this overlay layout-neutral, so focusing a block can never
/// reflow its text:
///
/// - A block that already has a border keeps its kind, labels, animation,
///   and — critically — its padding; only the stroke color changes.
///   Padding feeds text layout in `build_block_scenes`.
/// - A borderless block gains a minimal ring with ZERO padding — the same
///   clearance as no border at all, drawn purely by the `block_fx` shader.
pub fn apply_focus_style(
    style: Option<BlockBorderStyle>,
    focused: bool,
    theme: &Theme,
) -> Option<BlockBorderStyle> {
    if !focused {
        return style;
    }
    match style {
        Some(mut style) => {
            style.color = theme.block_border_focus;
            Some(style)
        }
        None => Some(BlockBorderStyle {
            kind: BorderKind::Full,
            color: theme.block_border_focus,
            thickness: theme.block_border_thickness,
            corner_radius: theme.block_border_corner_radius,
            padding: BorderPadding {
                top: 0.0,
                bottom: 0.0,
                left: 0.0,
                right: 0.0,
            },
            animation: BorderAnimation::None,
            top_label: None,
            bottom_label: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Focus over an existing border recolors the stroke and nothing else:
    /// kind, labels, animation, and padding all survive, so focusing a
    /// bordered block can never reflow its text or drop its labels.
    #[test]
    fn apply_focus_style_recolors_without_touching_layout() {
        let theme = Theme::default();
        let styled = BlockBorderStyle {
            kind: BorderKind::OpenBottom,
            color: theme.block_border_tool_call,
            thickness: 2.0,
            corner_radius: 3.0,
            padding: BorderPadding::default(),
            animation: BorderAnimation::Chase,
            top_label: Some("TOOL CALL".into()),
            bottom_label: Some("running".into()),
        };

        let focused = apply_focus_style(Some(styled.clone()), true, &theme)
            .expect("style must survive focus");
        assert_eq!(focused.color, theme.block_border_focus);
        assert_eq!(
            BlockBorderStyle { color: styled.color, ..focused },
            styled,
            "focus must change the stroke color and nothing else"
        );

        // Unfocused passes through untouched, Some or None.
        assert_eq!(apply_focus_style(Some(styled.clone()), false, &theme), Some(styled));
        assert_eq!(apply_focus_style(None, false, &theme), None);
    }
}

