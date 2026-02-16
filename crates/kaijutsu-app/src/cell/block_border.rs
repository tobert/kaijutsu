//! Shader-rendered block borders.
//!
//! Single `MaterialNode<BlockBorderMaterial>` per bordered block — no 9-slice overhead.
//! Supports full rectangle, top-accent, and dashed borders with chase/pulse/breathe animations.

use bevy::prelude::*;

use super::components::{
    BlockCell, BlockCellLayout, BlockKind, BlockSnapshot, CellEditor, DriftKind, MainCell, Role,
    BlockCellContainer,
};
use super::systems::EditorEntities;
use crate::shaders::block_border_material::BlockBorderMaterial;
use crate::ui::theme::Theme;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Visual style for a block's shader border.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct BlockBorderStyle {
    pub kind: BorderKind,
    pub color: Color,
    pub thickness: f32,
    pub corner_radius: f32,
    /// Padding inside the border (clearance for text).
    pub padding: BorderPadding,
    pub animation: BorderAnimation,
}

/// Simplified padding (top, bottom, left, right in pixels).
#[derive(Debug, Clone, Copy, Reflect)]
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

impl BorderPadding {
    pub fn vertical(&self) -> f32 {
        self.top + self.bottom
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

/// Links a BlockCell to its border MaterialNode entity.
#[derive(Component)]
pub struct BlockBorderEntity(pub Entity);

// ============================================================================
// SYSTEMS
// ============================================================================

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
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();

    for entity in &container.block_cells {
        let Ok((ent, block_cell, existing_style)) = block_cells.get(*entity) else {
            continue;
        };

        let Some(block) = blocks.get(&block_cell.block_id) else {
            // Block removed — strip border if present
            if existing_style.is_some() {
                commands.entity(ent).remove::<BlockBorderStyle>();
            }
            continue;
        };

        let new_style = compute_border_style(block, &theme);

        match (new_style, existing_style) {
            (Some(style), _) => {
                commands.entity(ent).insert(style);
            }
            (None, Some(_)) => {
                commands.entity(ent).remove::<BlockBorderStyle>();
            }
            (None, None) => {} // no border needed
        }
    }
}

/// Decide border style for a block based on kind, status, and content.
fn compute_border_style(block: &BlockSnapshot, theme: &Theme) -> Option<BlockBorderStyle> {
    use kaijutsu_crdt::Status;

    let padding = BorderPadding {
        top: theme.block_border_padding,
        bottom: theme.block_border_padding * 0.75,
        left: theme.block_border_padding * 1.5,
        right: theme.block_border_padding * 1.5,
    };

    match block.kind {
        BlockKind::ToolCall => {
            let (animation, color) = match block.status {
                Status::Running => (BorderAnimation::Chase, theme.block_border_tool_call),
                Status::Pending => (BorderAnimation::Chase, theme.block_border_tool_call),
                _ => (BorderAnimation::None, theme.block_border_tool_call.with_alpha(0.5)),
            };
            Some(BlockBorderStyle {
                kind: BorderKind::Full,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding,
                animation,
            })
        }
        BlockKind::ToolResult => {
            let content = block.content.trim();
            if content.is_empty() && !block.is_error {
                return None; // empty success — no border
            }
            if block.is_error {
                Some(BlockBorderStyle {
                    kind: BorderKind::Full,
                    color: theme.block_border_error,
                    thickness: theme.block_border_thickness,
                    corner_radius: theme.block_border_corner_radius,
                    padding,
                    animation: BorderAnimation::Pulse,
                })
            } else {
                let line_count = content.lines().count();
                let kind = if line_count <= 3 {
                    BorderKind::TopAccent
                } else {
                    BorderKind::Full
                };
                Some(BlockBorderStyle {
                    kind,
                    color: theme.block_border_tool_result,
                    thickness: theme.block_border_thickness,
                    corner_radius: theme.block_border_corner_radius,
                    padding,
                    animation: BorderAnimation::None,
                })
            }
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
                })
            }
        }
        BlockKind::Drift => match block.drift_kind {
            Some(DriftKind::Pull) | Some(DriftKind::Distill) | Some(DriftKind::Merge) => {
                Some(BlockBorderStyle {
                    kind: BorderKind::Full,
                    color: theme.block_border_drift,
                    thickness: theme.block_border_thickness,
                    corner_radius: theme.block_border_corner_radius,
                    padding,
                    animation: BorderAnimation::None,
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
            })
        }
        // ShellCommand, ShellOutput, Drift Push/Commit — no border
        _ => None,
    }
}

/// Spawn MaterialNode<BlockBorderMaterial> for BlockCells that have a style but no entity.
pub fn spawn_block_borders(
    mut commands: Commands,
    block_cells: Query<(Entity, &BlockBorderStyle), Without<BlockBorderEntity>>,
    mut materials: ResMut<Assets<BlockBorderMaterial>>,
    theme: Res<Theme>,
) {
    for (entity, style) in block_cells.iter() {
        let material = BlockBorderMaterial::from_style(style, &theme);
        let handle = materials.add(material);

        let border_entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(-style.padding.left),
                    top: Val::Px(-style.padding.top),
                    width: Val::Px(100.0),
                    height: Val::Px(50.0),
                    ..default()
                },
                MaterialNode(handle),
                ZIndex(-1), // Render behind text
            ))
            .id();

        // Make border a child of the BlockCell so it scrolls/clips with it
        commands.entity(entity).insert(BlockBorderEntity(border_entity));
        commands.entity(entity).add_child(border_entity);
    }
}

/// Position border nodes to match their associated BlockCell bounds.
#[allow(dead_code)] // Legacy — kept for rollback reference during flex migration
pub fn layout_block_borders(
    block_cells: Query<(&BlockCellLayout, &BlockBorderStyle, &BlockBorderEntity)>,
    mut border_nodes: Query<(&mut Node, &mut Visibility)>,
    mut materials: ResMut<Assets<BlockBorderMaterial>>,
    border_material_query: Query<&MaterialNode<BlockBorderMaterial>>,
    layout: Res<crate::cell::components::WorkspaceLayout>,
    scroll_state: Res<crate::cell::components::ConversationScrollState>,
    dag_view: Query<
        (&ComputedNode, &UiGlobalTransform),
        (
            With<super::components::ConversationContainer>,
            With<crate::ui::tiling::PaneFocus>,
        ),
    >,
) {
    let Ok((node, transform)) = dag_view.single() else {
        return;
    };
    let (_, _, translation) = transform.to_scale_angle_translation();
    let content = node.content_box();
    let visible_top = translation.y + content.min.y;
    let visible_bottom = translation.y + content.max.y;
    let base_width = content.width();
    let margin = layout.workspace_margin_left;

    for (block_layout, style, border_ent) in block_cells.iter() {
        let Ok((mut node, mut visibility)) = border_nodes.get_mut(border_ent.0) else {
            continue;
        };

        let indent = block_layout.indent_level as f32 * super::systems::INDENT_WIDTH;
        let left = margin + indent;
        let width = base_width - indent;
        let content_top = visible_top + block_layout.y_offset - scroll_state.offset;

        // Full border bounds (unclamped)
        let border_top = content_top - style.padding.top;
        let border_bottom = content_top + block_layout.height + style.padding.bottom;

        // Clamp to visible conversation area
        let clamped_top = border_top.max(visible_top);
        let clamped_bottom = border_bottom.min(visible_bottom);

        // Hide borders entirely outside the visible area
        if clamped_top >= clamped_bottom {
            *visibility = Visibility::Hidden;
            continue;
        }
        *visibility = Visibility::Inherited;

        let w = width + style.padding.left + style.padding.right;
        let h = clamped_bottom - clamped_top;

        // Position and size the border node (clamped to visible area)
        node.left = Val::Px(left - style.padding.left);
        node.top = Val::Px(clamped_top);
        node.width = Val::Px(w);
        node.height = Val::Px(h);

        // Update dimensions uniform for aspect-correct rendering
        if let Ok(mat_node) = border_material_query.get(border_ent.0) {
            if let Some(mat) = materials.get_mut(mat_node.0.id()) {
                mat.dimensions = Vec4::new(w, h, 0.0, 0.0);
            }
        }
    }
}

/// Position border nodes from parent BlockCell's ComputedNode (flex layout).
///
/// Since borders are now children of BlockCells, they just need to be sized
/// relative to their parent. No manual scroll/visibility clamping needed.
pub fn layout_block_borders_from_flex(
    block_cells: Query<(&ComputedNode, &BlockBorderStyle, &BlockBorderEntity)>,
    mut border_nodes: Query<&mut Node>,
    mut materials: ResMut<Assets<BlockBorderMaterial>>,
    border_material_query: Query<&MaterialNode<BlockBorderMaterial>>,
) {
    for (computed, style, border_ent) in block_cells.iter() {
        let size = computed.size();
        let w = size.x + style.padding.left + style.padding.right;
        let h = size.y + style.padding.top + style.padding.bottom;

        if let Ok(mut node) = border_nodes.get_mut(border_ent.0) {
            node.left = Val::Px(-style.padding.left);
            node.top = Val::Px(-style.padding.top);
            node.width = Val::Px(w);
            node.height = Val::Px(h);
        }

        // Update dimensions uniform for aspect-correct rendering
        if let Ok(mat_node) = border_material_query.get(border_ent.0) {
            if let Some(mat) = materials.get_mut(mat_node.0.id()) {
                mat.dimensions = Vec4::new(w, h, 0.0, 0.0);
            }
        }
    }
}

/// Update material properties when BlockBorderStyle changes (e.g. Running → Done).
pub fn update_block_border_state(
    block_cells: Query<(&BlockBorderStyle, &BlockBorderEntity), Changed<BlockBorderStyle>>,
    border_material_query: Query<&MaterialNode<BlockBorderMaterial>>,
    mut materials: ResMut<Assets<BlockBorderMaterial>>,
    theme: Res<Theme>,
) {
    for (style, border_ent) in block_cells.iter() {
        let Ok(mat_node) = border_material_query.get(border_ent.0) else {
            continue;
        };
        let Some(mat) = materials.get_mut(mat_node.0.id()) else {
            continue;
        };

        // Update material from new style
        *mat = BlockBorderMaterial::from_style_with_dimensions(style, &theme, mat.dimensions);
    }
}

/// Clean up border entities when BlockCell loses its style or is despawned.
pub fn cleanup_block_borders(
    mut commands: Commands,
    // BlockCells that have a border entity but no style
    removed_style: Query<(Entity, &BlockBorderEntity), Without<BlockBorderStyle>>,
    // Orphaned border entities (their parent BlockCell was despawned)
    all_border_refs: Query<&BlockBorderEntity>,
) {
    // Case 1: Style removed but entity still has BlockBorderEntity
    for (entity, border_ent) in removed_style.iter() {
        commands.entity(border_ent.0).try_despawn();
        commands.entity(entity).remove::<BlockBorderEntity>();
    }

    // Case 2: Check for orphaned border entities
    // (Handled by Bevy's despawn cascading if we parent correctly,
    // but we don't parent — border nodes are top-level for absolute positioning)
    // This is covered by the RemovedComponents approach, but for simplicity
    // we rely on case 1 + the fact that spawn_block_cells despawns the BlockCell
    // entity which triggers Without<BlockBorderStyle> on the next frame.
    let _ = all_border_refs; // suppress unused warning — presence in query is the point
}
