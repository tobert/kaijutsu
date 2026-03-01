//! Vello-rendered block borders (fieldset/legend style).
//!
//! Each bordered block gets a `UiVelloScene` child that draws a fieldset
//! border with label gaps. Replaces the previous `BlockBorderMaterial` shader.

use bevy::prelude::*;
use bevy_vello::prelude::UiVelloScene;

use crate::view::{
    BlockCell, BlockKind, BlockSnapshot, CellEditor, DriftKind, MainCell,
    BlockCellContainer, EditorEntities, Role,
};
use crate::view::fieldset;
use crate::text::FontHandles;
use crate::ui::theme::Theme;

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

impl BorderPadding {
    #[allow(dead_code)]
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

/// Links a BlockCell to its border child entity (UiVelloScene).
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
    layout_gen: Res<super::components::LayoutGeneration>,
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
            let tool_name = if block.content.is_empty() {
                "tool call".to_string()
            } else {
                // First line often has tool name
                let first_line = block.content.lines().next().unwrap_or("tool call");
                format!("tool call: {}", first_line.chars().take(30).collect::<String>())
            };
            let status_label = match block.status {
                Status::Running => Some("running".to_string()),
                Status::Pending => Some("pending".to_string()),
                Status::Done => None, // done tools don't need a status label
                Status::Error => Some("error".to_string()),
            };
            Some(BlockBorderStyle {
                kind: BorderKind::Full,
                color,
                thickness: theme.block_border_thickness,
                corner_radius: theme.block_border_corner_radius,
                padding,
                animation,
                top_label: Some(tool_name),
                bottom_label: status_label,
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
                    top_label: Some("result".to_string()),
                    bottom_label: Some("error".to_string()),
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
                    top_label: Some("result".to_string()),
                    bottom_label: None,
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

/// Spawn UiVelloScene border entity for BlockCells that have a style but no border entity.
///
/// Runs in PostUpdate (after UiSystems::Layout so ComputedNode is available).
pub fn spawn_vello_borders(
    mut commands: Commands,
    block_cells: Query<(Entity, &BlockBorderStyle), Without<BlockBorderEntity>>,
) {
    for (entity, _style) in block_cells.iter() {
        let border_entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                UiVelloScene::default(),
                ZIndex(-1), // Render behind text
            ))
            .id();

        commands.entity(entity).insert(BlockBorderEntity(border_entity));
        commands.entity(entity).add_child(border_entity);
    }
}

/// Rebuild border scenes when style or size changes.
///
/// Runs in PostUpdate (after UiSystems::Layout).
pub fn update_vello_borders(
    block_cells: Query<
        (&BlockBorderStyle, &BlockBorderEntity, &ComputedNode),
        Or<(Changed<BlockBorderStyle>, Changed<ComputedNode>)>,
    >,
    mut scenes: Query<&mut UiVelloScene>,
    fonts: Res<Assets<bevy_vello::prelude::VelloFont>>,
    font_handles: Res<FontHandles>,
) {
    let font = fonts.get(&font_handles.mono);

    for (style, border_ent, computed) in block_cells.iter() {
        let Ok(mut scene_component) = scenes.get_mut(border_ent.0) else {
            continue;
        };

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
        );

        *scene_component = UiVelloScene::from(scene);
    }
}

/// Animate borders every frame for blocks with active animations.
///
/// Runs in PostUpdate (after update_vello_borders).
pub fn animate_vello_borders(
    time: Res<Time>,
    block_cells: Query<(&BlockBorderStyle, &BlockBorderEntity, &ComputedNode)>,
    mut scenes: Query<&mut UiVelloScene>,
    fonts: Res<Assets<bevy_vello::prelude::VelloFont>>,
    font_handles: Res<FontHandles>,
) {
    let t = time.elapsed_secs();
    let font = fonts.get(&font_handles.mono);

    for (style, border_ent, computed) in block_cells.iter() {
        // Only animate blocks with active animations
        if style.animation == BorderAnimation::None {
            continue;
        }

        let Ok(mut scene_component) = scenes.get_mut(border_ent.0) else {
            continue;
        };

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
        );

        *scene_component = UiVelloScene::from(scene);
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

    let _ = all_border_refs; // suppress unused warning — presence in query is the point
}
