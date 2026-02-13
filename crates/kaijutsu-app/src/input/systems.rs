//! Input action handlers — systems that consume ActionFired messages.
//!
//! All domain input handlers live here. They read `ActionFired` or
//! `TextInputReceived` messages instead of raw keyboard events.

use bevy::prelude::*;

use super::action::Action;
use super::events::ActionFired;
use super::focus::FocusArea;
use crate::ui::constellation::ConstellationVisible;
use crate::ui::state::AppScreen;

// ============================================================================
// FOCUS SYNC — keep FocusArea consistent with app state
// ============================================================================

/// Sync FocusArea when AppScreen state changes.
///
/// When switching to Dashboard, focus goes to Dashboard.
/// When switching to Conversation, focus goes to Conversation (block navigation).
pub fn sync_focus_from_screen(
    screen: Res<State<AppScreen>>,
    mut focus: ResMut<FocusArea>,
) {
    if !screen.is_changed() {
        return;
    }

    match screen.get() {
        AppScreen::Dashboard => {
            *focus = FocusArea::Dashboard;
        }
        AppScreen::Conversation => {
            // Default to Conversation (block navigation) when entering conversation
            // Only change if we're coming from Dashboard — preserve existing focus
            // if already in conversation (e.g. switching between compose/nav)
            if *focus == FocusArea::Dashboard {
                *focus = FocusArea::Conversation;
            }
        }
    }
}

// ============================================================================
// FOCUS CYCLING — Tab/Shift+Tab
// ============================================================================

/// Handle CycleFocusForward and CycleFocusBackward actions.
///
/// Tab cycle order: Compose → Conversation → (Constellation if visible) → wrap.
pub fn handle_focus_cycle(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    constellation_visible: Option<Res<ConstellationVisible>>,
) {
    for ActionFired(action) in actions.read() {
        let constellation_on = constellation_visible
            .as_ref()
            .map(|v| v.0)
            .unwrap_or(false);

        match action {
            Action::CycleFocusForward => {
                *focus = match focus.as_ref() {
                    FocusArea::Compose => FocusArea::Conversation,
                    FocusArea::Conversation => {
                        if constellation_on {
                            FocusArea::Constellation
                        } else {
                            FocusArea::Compose
                        }
                    }
                    FocusArea::Constellation => FocusArea::Compose,
                    // Don't cycle out of dialog/dashboard/editing
                    other => other.clone(),
                };
            }
            Action::CycleFocusBackward => {
                *focus = match focus.as_ref() {
                    FocusArea::Compose => {
                        if constellation_on {
                            FocusArea::Constellation
                        } else {
                            FocusArea::Conversation
                        }
                    }
                    FocusArea::Conversation => FocusArea::Compose,
                    FocusArea::Constellation => FocusArea::Conversation,
                    other => other.clone(),
                };
            }
            _ => {}
        }
    }
}

// ============================================================================
// FOCUS COMPOSE / UNFOCUS — direct focus management
// ============================================================================

/// Handle FocusCompose action (i/Space in Navigation → jump to compose).
pub fn handle_focus_compose(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
) {
    for ActionFired(action) in actions.read() {
        if matches!(action, Action::FocusCompose) {
            *focus = FocusArea::Compose;
        }
    }
}

/// Handle Unfocus action (Escape — context-dependent "go up").
///
/// - EditingBlock → Conversation (stop editing)
/// - Compose → Conversation (keep draft)
/// - Constellation → Conversation + close constellation
/// - Dialog → previous focus (handled by dialog system)
pub fn handle_unfocus(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut constellation_visible: Option<ResMut<ConstellationVisible>>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::Unfocus) {
            continue;
        }

        match focus.as_ref() {
            FocusArea::EditingBlock { .. } | FocusArea::Compose => {
                *focus = FocusArea::Conversation;
            }
            FocusArea::Constellation => {
                if let Some(ref mut vis) = constellation_visible {
                    vis.0 = false;
                }
                *focus = FocusArea::Conversation;
            }
            _ => {}
        }
    }
}

// ============================================================================
// DEBUG HANDLERS — migrated from ui/debug.rs to consume ActionFired
// ============================================================================

/// Handle Quit action.
pub fn handle_quit(
    mut actions: MessageReader<ActionFired>,
    mut exit: MessageWriter<AppExit>,
) {
    for ActionFired(action) in actions.read() {
        if matches!(action, Action::Quit) {
            info!("Quitting...");
            exit.write(AppExit::Success);
        }
    }
}

/// Handle DebugToggle action.
pub fn handle_debug_toggle(
    mut actions: MessageReader<ActionFired>,
    mut debug_options: ResMut<UiDebugOptions>,
) {
    for ActionFired(action) in actions.read() {
        if matches!(action, Action::DebugToggle) {
            debug_options.toggle();
            info!(
                "UI debug overlay: {}",
                if debug_options.enabled { "ON" } else { "OFF" }
            );
        }
    }
}

/// Handle Screenshot action.
pub fn handle_screenshot(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
) {
    use bevy::render::view::screenshot::{save_to_disk, Screenshot};

    for ActionFired(action) in actions.read() {
        if matches!(action, Action::Screenshot) {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let path = format!("design/screenshots/screenshot-{}.png", timestamp);

            let _ = std::fs::create_dir_all("design/screenshots");

            info!("Saving screenshot to {}", path);
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path));
        }
    }
}

/// Handle ToggleConstellation action.
pub fn handle_toggle_constellation(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut constellation_visible: Option<ResMut<ConstellationVisible>>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::ToggleConstellation) {
            continue;
        }

        if let Some(ref mut vis) = constellation_visible {
            vis.0 = !vis.0;
            if vis.0 {
                *focus = FocusArea::Constellation;
            } else {
                *focus = FocusArea::Conversation;
            }
        }
    }
}

// ============================================================================
// BLOCK NAVIGATION
// ============================================================================

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, CellEditor, CellState,
    ConversationScrollState, EditorEntities, FocusTarget, FocusedBlockCell, MainCell,
};

/// Navigation direction for block focus.
enum NavigationDirection {
    Next,
    Previous,
    First,
    Last,
}

/// Handle block navigation actions (j/k, Home/End, G).
///
/// Replaces the old `navigate_blocks` system — reads ActionFired instead of raw keys.
pub fn handle_navigate_blocks(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    block_cells: Query<(Entity, &BlockCell, &BlockCellLayout)>,
    mut focus: ResMut<FocusTarget>,
    mut scroll_state: ResMut<ConversationScrollState>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    let mut direction: Option<NavigationDirection> = None;

    for ActionFired(action) in actions.read() {
        match action {
            Action::FocusNextBlock => direction = Some(NavigationDirection::Next),
            Action::FocusPrevBlock => direction = Some(NavigationDirection::Previous),
            Action::FocusFirstBlock => direction = Some(NavigationDirection::First),
            Action::FocusLastBlock => direction = Some(NavigationDirection::Last),
            _ => {}
        }
    }

    let Some(direction) = direction else {
        return;
    };

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let blocks = editor.blocks();
    if blocks.is_empty() {
        return;
    }

    // Find current focus index
    let current_idx = focus
        .block_id
        .as_ref()
        .and_then(|id| blocks.iter().position(|b| &b.id == id));

    // Calculate new index
    let new_idx = match direction {
        NavigationDirection::Next => match current_idx {
            Some(i) if i + 1 < blocks.len() => i + 1,
            Some(i) => i,
            None => 0,
        },
        NavigationDirection::Previous => match current_idx {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => blocks.len() - 1,
        },
        NavigationDirection::First => 0,
        NavigationDirection::Last => blocks.len() - 1,
    };

    let new_block = &blocks[new_idx];

    // Update focus resource
    focus.focus_block(new_block.id.clone());

    // Remove old FocusedBlockCell markers
    for entity in focused_markers.iter() {
        commands.entity(entity).remove::<FocusedBlockCell>();
    }

    // Add FocusedBlockCell marker to the new focused entity
    if let Some(entity) = container.get_entity(&new_block.id) {
        commands.entity(entity).insert(FocusedBlockCell);

        // Scroll to keep focused block visible
        if let Ok((_, _, layout)) = block_cells.get(entity) {
            scroll_to_block_visible(&mut scroll_state, layout);
        }
    }

    debug!("Block focus: {:?} (index {})", new_block.id, new_idx);
}

/// Scroll to keep a block visible in the viewport.
fn scroll_to_block_visible(
    scroll_state: &mut ConversationScrollState,
    layout: &BlockCellLayout,
) {
    let block_top = layout.y_offset;
    let block_bottom = layout.y_offset + layout.height;
    let view_top = scroll_state.offset;
    let view_bottom = scroll_state.offset + scroll_state.visible_height;

    const MARGIN: f32 = 20.0;

    if block_top < view_top + MARGIN {
        scroll_state.target_offset = (block_top - MARGIN).max(0.0);
        scroll_state.offset = scroll_state.target_offset;
        scroll_state.following = false;
    } else if block_bottom > view_bottom - MARGIN {
        let target = block_bottom - scroll_state.visible_height + MARGIN;
        scroll_state.target_offset = target.min(scroll_state.max_offset());
        scroll_state.offset = scroll_state.target_offset;
        scroll_state.following = scroll_state.is_at_bottom();
    }
}

// ============================================================================
// SCROLLING
// ============================================================================

/// Handle scroll actions (ScrollDelta, HalfPageUp/Down, ScrollToEnd/Top).
///
/// Replaces the old `handle_scroll_input` system.
pub fn handle_scroll(
    mut actions: MessageReader<ActionFired>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    for ActionFired(action) in actions.read() {
        match action {
            Action::ScrollDelta(delta) => {
                scroll_state.scroll_by(*delta);
            }
            Action::HalfPageDown => {
                let half_page = scroll_state.visible_height * 0.5;
                scroll_state.scroll_by(half_page);
            }
            Action::HalfPageUp => {
                let half_page = scroll_state.visible_height * 0.5;
                scroll_state.scroll_by(-half_page);
            }
            Action::ScrollToEnd => {
                scroll_state.scroll_to_end();
            }
            Action::ScrollToTop => {
                scroll_state.target_offset = 0.0;
                scroll_state.offset = 0.0;
                scroll_state.following = false;
            }
            _ => {}
        }
    }
}

// ============================================================================
// EXPAND / COLLAPSE / VIEW POP
// ============================================================================

/// Handle ExpandBlock action (f key on focused block → full-screen reader).
pub fn handle_expand_block(
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusTarget>,
    mut view_stack: ResMut<crate::ui::state::ViewStack>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::ExpandBlock) {
            continue;
        }

        if let Some(ref block_id) = focus.block_id {
            view_stack.push(crate::ui::state::View::ExpandedBlock {
                block_id: block_id.clone(),
            });
            info!("Expanded block: {:?}", block_id);
        }
    }
}

/// Handle CollapseToggle action (toggle thinking block collapse).
pub fn handle_collapse_toggle(
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusTarget>,
    mut cells: Query<(&mut CellEditor, &mut CellState)>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::CollapseToggle) {
            continue;
        }

        let Some(focused_entity) = focus.entity else {
            continue;
        };

        let Ok((mut editor, _cell_state)) = cells.get_mut(focused_entity) else {
            continue;
        };

        // Find thinking blocks to toggle
        let thinking_blocks: Vec<_> = editor
            .blocks()
            .iter()
            .filter(|b| matches!(b.kind, kaijutsu_crdt::BlockKind::Thinking))
            .map(|b| b.id.clone())
            .collect();

        if !thinking_blocks.is_empty() {
            for block_id in &thinking_blocks {
                editor.toggle_block_collapse(block_id);
            }
            let collapsed = editor
                .blocks()
                .iter()
                .find(|b| matches!(b.kind, kaijutsu_crdt::BlockKind::Thinking))
                .map(|b| b.collapsed)
                .unwrap_or(false);
            info!(
                "Thinking blocks: {}",
                if collapsed { "collapsed" } else { "expanded" }
            );
        } else {
            // Toggle collapse on entire cell
            let Ok((_editor, mut cell_state)) = cells.get_mut(focused_entity) else {
                continue;
            };
            cell_state.collapsed = !cell_state.collapsed;
        }
    }
}

/// Handle PopView action (Esc in expanded view → pop back).
pub fn handle_view_pop(
    mut actions: MessageReader<ActionFired>,
    mut view_stack: ResMut<crate::ui::state::ViewStack>,
) {
    for ActionFired(action) in actions.read() {
        if matches!(action, Action::PopView) && !view_stack.is_at_root() {
            view_stack.pop();
        }
    }
}

// ============================================================================
// TILING PANE MANAGEMENT
// ============================================================================

use crate::ui::tiling::{FocusDirection, SplitDirection, TilingTree};

/// Handle tiling pane actions (Alt+hjkl, split, close, resize).
///
/// Replaces the old `handle_tiling_keys` system.
pub fn handle_tiling(
    mut actions: MessageReader<ActionFired>,
    mut tree: ResMut<TilingTree>,
) {
    for ActionFired(action) in actions.read() {
        match action {
            Action::FocusPaneLeft => {
                if tree.focus_direction(FocusDirection::Left) {
                    info!("Tiling: focus left → {}", tree.focused);
                }
            }
            Action::FocusPaneDown => {
                if tree.focus_direction(FocusDirection::Down) {
                    info!("Tiling: focus down → {}", tree.focused);
                }
            }
            Action::FocusPaneUp => {
                if tree.focus_direction(FocusDirection::Up) {
                    info!("Tiling: focus up → {}", tree.focused);
                }
            }
            Action::FocusPaneRight => {
                if tree.focus_direction(FocusDirection::Right) {
                    info!("Tiling: focus right → {}", tree.focused);
                }
            }
            Action::SplitVertical => {
                let target = tree.focused;
                if let Some(new_pane) = tree.split(target, SplitDirection::Row) {
                    info!("Tiling: split vertical → new {}", new_pane);
                }
            }
            Action::SplitHorizontal => {
                let target = tree.focused;
                if let Some(new_pane) = tree.split(target, SplitDirection::Column) {
                    info!("Tiling: split horizontal → new {}", new_pane);
                }
            }
            Action::ClosePane => {
                let target = tree.focused;
                if tree.close(target) {
                    info!("Tiling: closed pane, now focused {}", tree.focused);
                }
            }
            Action::GrowPane => {
                let target = tree.focused;
                tree.resize(target, 0.05);
            }
            Action::ShrinkPane => {
                let target = tree.focused;
                tree.resize(target, -0.05);
            }
            Action::TogglePreviousPaneFocus => {
                tree.toggle_focus();
                info!("Tiling: toggle focus → {}", tree.focused);
            }
            _ => {}
        }
    }
}

// ============================================================================
// CONSTELLATION NAVIGATION
// ============================================================================

use crate::cell::{
    BlockEditCursor, ComposeBlock, EditingBlockCell, PromptSubmitted,
};
use crate::ui::constellation::{
    Constellation, ConstellationCamera, DialogMode, OpenContextDialog,
    find_nearest_in_direction, model_picker::OpenModelPicker,
};

/// Handle constellation navigation actions (spatial nav, pan, zoom, fork, model picker).
///
/// Replaces the old `handle_focus_navigation` system — much simpler since
/// the dispatcher handles mode guards, modifiers, and sequences.
pub fn handle_constellation_nav(
    mut actions: MessageReader<ActionFired>,
    mut constellation: ResMut<Constellation>,
    mut camera: ResMut<ConstellationCamera>,
    visible: Res<ConstellationVisible>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    mut dialog_writer: MessageWriter<OpenContextDialog>,
    mut model_writer: MessageWriter<OpenModelPicker>,
    doc_cache: Res<crate::cell::DocumentCache>,
) {
    if !visible.0 {
        return;
    }

    for ActionFired(action) in actions.read() {
        match action {
            Action::SpatialNav(direction) => {
                // Find nearest node in the given direction
                if let Some(target_id) = find_nearest_in_direction(&constellation, *direction) {
                    constellation.focus(&target_id);
                    // Auto-center camera on focused node
                    if let Some(node) = constellation.node_by_id(&target_id) {
                        camera.target_offset = -node.position * camera.zoom;
                    }
                }
            }
            Action::Pan(direction) => {
                let pan_speed = 50.0;
                camera.target_offset += *direction * pan_speed;
            }
            Action::ZoomIn => {
                camera.target_zoom = (camera.target_zoom * 1.25).min(4.0);
            }
            Action::ZoomOut => {
                camera.target_zoom = (camera.target_zoom / 1.25).max(0.25);
            }
            Action::ZoomReset => {
                camera.reset();
            }
            Action::Activate => {
                // Enter on focused node → switch context
                if let Some(ref focus_id) = constellation.focus_id {
                    info!("Constellation: switching to {}", focus_id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: focus_id.clone(),
                    });
                }
            }
            Action::NextContext => {
                if let Some(id) = constellation.next_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: id,
                    });
                }
            }
            Action::PrevContext => {
                if let Some(id) = constellation.prev_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: id,
                    });
                }
            }
            Action::ToggleAlternate => {
                if let Some(alt_id) = constellation.alternate_id.clone() {
                    constellation.focus(&alt_id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: alt_id,
                    });
                }
            }
            Action::ConstellationFork => {
                if let Some(ref focus_id) = constellation.focus_id {
                    if let Some(doc_id) = doc_cache.document_id_for_context(focus_id) {
                        dialog_writer.write(OpenContextDialog(DialogMode::ForkContext {
                            source_context: focus_id.clone(),
                            source_document_id: doc_id.to_string(),
                        }));
                    } else {
                        warn!("Cannot fork '{}': not in document cache", focus_id);
                    }
                }
            }
            Action::ConstellationModelPicker => {
                if let Some(ref focus_id) = constellation.focus_id {
                    model_writer.write(OpenModelPicker {
                        context_name: focus_id.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

// ============================================================================
// TEXT INPUT (COMPOSE + INLINE BLOCK EDITING)
// ============================================================================

/// Handle text input in Compose area.
///
/// Consumes TextInputReceived for character insertion and ActionFired for
/// editing actions (Submit, Backspace, Delete, cursor movement).
/// Auto-detects shell prefix (: or `) for routing on Submit.
pub fn handle_compose_input(
    mut text_events: MessageReader<super::events::TextInputReceived>,
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusArea>,
    mut compose_blocks: Query<&mut ComposeBlock, With<crate::ui::tiling::PaneFocus>>,
    mut submit_writer: MessageWriter<PromptSubmitted>,
) {
    if !matches!(*focus, FocusArea::Compose) {
        return;
    }

    let Ok(mut compose) = compose_blocks.single_mut() else {
        return;
    };

    // Handle text insertion
    for super::events::TextInputReceived(text) in text_events.read() {
        compose.insert(text);
    }

    // Handle editing actions
    for ActionFired(action) in actions.read() {
        match action {
            Action::Submit => {
                if !compose.is_empty() {
                    let text = compose.take();
                    info!("ComposeBlock submitted: {} chars", text.len());
                    submit_writer.write(PromptSubmitted { text });
                }
            }
            Action::InsertNewline => {
                compose.insert("\n");
            }
            Action::Backspace => {
                compose.backspace();
            }
            Action::Delete => {
                compose.delete();
            }
            Action::CursorLeft => {
                compose.move_left();
            }
            Action::CursorRight => {
                compose.move_right();
            }
            // Phase 4+: CursorUp/Down (multi-line compose), word movement, clipboard
            _ => {}
        }
    }
}

/// Handle text input in inline block editing mode.
///
/// Consumes TextInputReceived for character insertion and ActionFired for
/// editing actions. Operates on CRDT via BlockDocument.
pub fn handle_block_edit_input(
    mut text_events: MessageReader<super::events::TextInputReceived>,
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusArea>,
    entities: Res<EditorEntities>,
    mut main_cells: Query<&mut CellEditor, With<MainCell>>,
    mut editing_cells: Query<(&BlockCell, &mut BlockEditCursor), With<EditingBlockCell>>,
) {
    if !matches!(*focus, FocusArea::EditingBlock { .. }) {
        return;
    }

    let Ok((block_cell, mut cursor)) = editing_cells.single_mut() else {
        return;
    };
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(mut editor) = main_cells.get_mut(main_ent) else {
        return;
    };

    // Handle text insertion
    for super::events::TextInputReceived(text) in text_events.read() {
        for c in text.chars() {
            if c.is_control() {
                continue;
            }
            let s = c.to_string();
            if editor
                .doc
                .edit_text(&block_cell.block_id, cursor.offset, &s, 0)
                .is_ok()
            {
                cursor.offset += s.len();
            }
        }
    }

    // Handle editing actions
    for ActionFired(action) in actions.read() {
        match action {
            Action::InsertNewline => {
                if editor
                    .doc
                    .edit_text(&block_cell.block_id, cursor.offset, "\n", 0)
                    .is_ok()
                {
                    cursor.offset += 1;
                }
            }
            Action::Backspace => {
                if cursor.offset > 0
                    && let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id)
                {
                    let text = &block.content;
                    let mut new_offset = cursor.offset.saturating_sub(1);
                    while new_offset > 0 && !text.is_char_boundary(new_offset) {
                        new_offset -= 1;
                    }
                    let delete_len = cursor.offset - new_offset;
                    if editor
                        .doc
                        .edit_text(&block_cell.block_id, new_offset, "", delete_len)
                        .is_ok()
                    {
                        cursor.offset = new_offset;
                    }
                }
            }
            Action::Delete => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    if cursor.offset < text.len() {
                        let mut end = cursor.offset + 1;
                        while end < text.len() && !text.is_char_boundary(end) {
                            end += 1;
                        }
                        let delete_len = end - cursor.offset;
                        let _ = editor
                            .doc
                            .edit_text(&block_cell.block_id, cursor.offset, "", delete_len);
                    }
                }
            }
            Action::CursorLeft => {
                if cursor.offset > 0
                    && let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id)
                {
                    let text = &block.content;
                    let mut new_offset = cursor.offset - 1;
                    while new_offset > 0 && !text.is_char_boundary(new_offset) {
                        new_offset -= 1;
                    }
                    cursor.offset = new_offset;
                }
            }
            Action::CursorRight => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    if cursor.offset < text.len() {
                        let mut new_offset = cursor.offset + 1;
                        while new_offset < text.len() && !text.is_char_boundary(new_offset) {
                            new_offset += 1;
                        }
                        cursor.offset = new_offset;
                    }
                }
            }
            Action::CursorHome => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let before_cursor = &text[..cursor.offset];
                    cursor.offset = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
                }
            }
            Action::CursorEnd => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let after_cursor = &text[cursor.offset..];
                    cursor.offset += after_cursor.find('\n').unwrap_or(after_cursor.len());
                }
            }
            _ => {}
        }
    }
}
