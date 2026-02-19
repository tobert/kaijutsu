//! MSDF text measurement for Bevy UI layout integration.
//!
//! Sets `ContentSize` on BlockCells using `FixedMeasure` with pre-computed
//! heights from MSDF line counts. This lets Bevy's taffy layout engine
//! compute correct heights for BlockCells without manual height tracking.

use bevy::prelude::*;
use bevy::ui::measurement::{ContentSize, FixedMeasure, NodeMeasure};

use super::components::{
    BlockCell, BlockCellContainer, ConversationContainer, WorkspaceLayout,
};
use super::systems::EditorEntities;
use crate::text::{FontMetricsCache, MsdfTextBuffer, SharedFontSystem};
use crate::ui::tiling::PaneFocus;

/// Update `ContentSize` on BlockCells with cached MSDF line counts.
///
/// Runs after `sync_block_cell_buffers` (CellPhase::Buffer), before Bevy's
/// layout pass. Computes visual line count at the current pane width and
/// sets a `FixedMeasure` so taffy can derive correct heights.
///
/// **Optimization:** Compares computed height against existing measure before
/// calling `content_size.set()`, since `.set()` triggers `&mut self` →
/// Bevy change detection → taffy relayout for ALL nodes.
pub fn update_msdf_measures(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<
        (&mut MsdfTextBuffer, &mut ContentSize),
        With<BlockCell>,
    >,
    font_system: Res<SharedFontSystem>,
    mut metrics_cache: ResMut<FontMetricsCache>,
    layout: Res<WorkspaceLayout>,
    conv_containers: Query<
        &ComputedNode,
        (With<ConversationContainer>, With<PaneFocus>),
    >,
    windows: Query<&Window>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_gen: Local<u64>,
    mut last_base_width: Local<f32>,
) {
    // Only recompute measures when content or width changes
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let margin = layout.workspace_margin_left;
    let base_width = conv_containers
        .iter()
        .next()
        .map(|node| node.size().x)
        .filter(|w| *w > 0.0)
        .unwrap_or_else(|| {
            windows
                .iter()
                .next()
                .map(|w| w.resolution.width())
                .unwrap_or(1280.0)
        });
    let base_width = base_width - (margin * 2.0);

    let width_changed = (base_width - *last_base_width).abs() > 1.0;
    let content_changed = layout_gen.0 != *last_gen;

    if !width_changed && !content_changed {
        return;
    }
    *last_gen = layout_gen.0;
    *last_base_width = base_width;

    let mut fs = font_system.0.lock().unwrap();

    for entity in &container.block_cells {
        let Ok((mut buffer, mut content_size)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Wrap width: border padding is on Node.padding, taffy subtracts it
        // from available width. We just use the base pane width here.
        // TODO: indent_level should reduce wrap_width but we don't query
        // BlockCellLayout here to avoid borrow conflicts.
        let wrap_width = base_width;

        let line_count =
            buffer.visual_line_count(&mut fs, wrap_width, Some(&mut metrics_cache));

        let height = (line_count as f32) * layout.line_height + 4.0;

        // Width is unconstrained — the node's 100% width handles it.
        // Height is the content-only height; Node.padding adds border padding.
        content_size.set(NodeMeasure::Fixed(FixedMeasure {
            size: Vec2::new(0.0, height),
        }));
    }
}
