//! Input action handlers — systems that consume ActionFired messages.
//!
//! All domain input handlers live here. They read `ActionFired` or
//! `TextInputReceived` messages instead of raw keyboard events.

use bevy::prelude::*;

use super::action::Action;
use super::events::ActionFired;
use super::focus::FocusArea;
use crate::ui::constellation::ConstellationVisible; // Used by handle_focus_cycle (reads visibility)

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
        // ConstellationVisible synced by enforce_constellation_focus_sync
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

use crate::ui::state::ViewStack;

/// Handle Unfocus action (Escape — context-dependent "go up").
///
/// Escape precedence:
/// 1. FocusArea::Dialog → ignored (handled by dialog systems via FocusStack)
/// 2. ViewStack overlay active → pop view, keep FocusArea
/// 3. FocusArea::EditingBlock → clean up markers, FocusArea::Conversation
/// 4. FocusArea::Compose → FocusArea::Conversation
/// 5. FocusArea::Constellation → FocusArea::Conversation (visibility synced by render)
pub fn handle_unfocus(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut view_stack: ResMut<ViewStack>,
    editing_cells: Query<Entity, With<EditingBlockCell>>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::Unfocus) {
            continue;
        }

        // 1. Dialogs handle their own Escape/Unfocus via FocusStack.
        if matches!(*focus, FocusArea::Dialog) {
            continue;
        }

        // 2. Pop ViewStack if an overlay view is active (ExpandedBlock, etc)
        if !view_stack.is_at_root() {
            info!("Escape: popping view stack");
            view_stack.pop();
            continue;
        }

        // 3. Normal focus transitions
        match focus.as_ref() {
            FocusArea::EditingBlock => {
                // ECS Cleanup
                for entity in editing_cells.iter() {
                    commands.entity(entity).remove::<EditingBlockCell>();
                    commands.entity(entity).remove::<BlockEditCursor>();
                }
                *focus = FocusArea::Conversation;
            }
            FocusArea::Compose => {
                *focus = FocusArea::Conversation;
            }
            FocusArea::Constellation => {
                // ConstellationVisible synced by enforce_constellation_focus_sync
                *focus = FocusArea::Conversation;
            }
            _ => {}
        }
    }
}

/// Handle Activate action in Navigation context (Enter on focused block → edit).
pub fn handle_activate_navigation(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    focus_target: Res<FocusTarget>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::Activate) {
            continue;
        }

        let Some(ref block_id) = focus_target.block_id else {
            continue;
        };

        let Some(main_ent) = entities.main_cell else {
            continue;
        };
        let Ok(editor) = main_cells.get(main_ent) else {
            continue;
        };
        let Ok(container) = containers.get(main_ent) else {
            continue;
        };

        // Only allow editing User Text blocks
        let Some(block) = editor.doc.get_block_snapshot(block_id) else {
            continue;
        };

        if block.role != kaijutsu_crdt::Role::User || block.kind != kaijutsu_crdt::BlockKind::Text {
            warn!("Cannot edit block {:?}: only User Text blocks are editable", block_id);
            continue;
        }

        if let Some(entity) = container.get_entity(block_id) {
            info!("Entering edit mode for block {:?}", block_id);
            
            commands.entity(entity).insert((
                EditingBlockCell,
                BlockEditCursor { offset: block.content.len(), selection_anchor: None },
            ));

            *focus = FocusArea::EditingBlock;
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
) {
    for ActionFired(action) in actions.read() {
        if !matches!(action, Action::ToggleConstellation) {
            continue;
        }

        // Just toggle focus — enforce_constellation_focus_sync handles visibility
        *focus = if matches!(*focus, FocusArea::Constellation) {
            FocusArea::Conversation
        } else {
            FocusArea::Constellation
        };
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
/// Only active when Conversation has focus. Without this guard,
/// FocusNextBlock/FocusPrevBlock (shared with Dialog j/k) would
/// move block focus in the background while a dialog is open.
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
/// Only active in Conversation or Compose focus (scrolling the conversation).
/// Prevents gamepad scroll leaking into dialogs or constellation.
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
///
/// Guarded to Conversation focus — ExpandBlock is Navigation-only.
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
///
/// Guarded to Conversation focus — CollapseToggle is Navigation-only.
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
    Constellation, ConstellationCamera, DialogMode, NewContextConfig, OpenContextDialog,
    create_or_fork_context, find_nearest_in_direction, model_picker::OpenModelPicker,
};

/// Handle constellation navigation actions (spatial nav, pan, zoom, fork, model picker).
///
/// Guarded to FocusArea::Constellation — prevents Activate/Pan/etc from
/// leaking when a Dialog overlays the (still-visible) constellation.
pub fn handle_constellation_nav(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut constellation: ResMut<Constellation>,
    mut camera: ResMut<ConstellationCamera>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    mut dialog_writer: MessageWriter<OpenContextDialog>,
    mut model_writer: MessageWriter<OpenModelPicker>,
    doc_cache: Res<crate::cell::DocumentCache>,
    bootstrap: Res<crate::connection::BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
    new_ctx_config: Res<NewContextConfig>,
    actor: Option<Res<crate::connection::RpcActor>>,
) {
    for ActionFired(action) in actions.read() {
        match action {
            Action::SpatialNav(direction) => {
                if let Some(target_id) = find_nearest_in_direction(&constellation, *direction) {
                    constellation.focus(&target_id);
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
                // Enter → switch context and dismiss constellation
                if let Some(ref focus_id) = constellation.focus_id {
                    info!("Constellation: switching to {}", focus_id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: focus_id.clone(),
                    });
                    // enforce_constellation_focus_sync handles visibility
                    *focus = FocusArea::Compose;
                }
            }
            Action::NextContext => {
                if let Some(id) = constellation.next_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: id,
                    });
                    *focus = FocusArea::Compose;
                }
            }
            Action::PrevContext => {
                if let Some(id) = constellation.prev_context_id().map(|s| s.to_string()) {
                    constellation.focus(&id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: id,
                    });
                    *focus = FocusArea::Compose;
                }
            }
            Action::ToggleAlternate => {
                if let Some(alt_id) = constellation.alternate_id.clone() {
                    constellation.focus(&alt_id);
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: alt_id,
                    });
                    *focus = FocusArea::Compose;
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
            Action::ConstellationCreate => {
                info!("Constellation: creating new context");
                create_or_fork_context(
                    &new_ctx_config,
                    &bootstrap,
                    &conn_state,
                    actor.as_deref(),
                    &doc_cache,
                );
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
// TIMELINE NAVIGATION
// ============================================================================

use crate::ui::timeline::{ForkRequest, TimelineState};

/// Handle timeline navigation actions.
///
/// Guarded to Conversation focus — timeline keys are Navigation-only.
pub fn handle_timeline(
    mut actions: MessageReader<ActionFired>,
    mut timeline: ResMut<TimelineState>,
    mut fork_writer: MessageWriter<ForkRequest>,
) {
    for ActionFired(action) in actions.read() {
        match action {
            Action::TimelineStepBack => {
                let step = 1.0 / (timeline.snapshot_count.max(1) as f32);
                let new_pos = (timeline.target_position - step).max(0.0);
                timeline.begin_scrub(new_pos);
                timeline.end_scrub();
            }
            Action::TimelineStepForward => {
                let step = 1.0 / (timeline.snapshot_count.max(1) as f32);
                let new_pos = (timeline.target_position + step).min(1.0);
                timeline.begin_scrub(new_pos);
                timeline.end_scrub();
            }
            Action::TimelineJumpToLive => {
                timeline.jump_to_live();
            }
            Action::TimelineFork => {
                if timeline.is_historical() {
                    fork_writer.write(ForkRequest {
                        from_version: timeline.viewing_version,
                        name: None,
                    });
                }
            }
            Action::TimelineToggle => {
                timeline.expanded = !timeline.expanded;
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
    mut compose_blocks: Query<&mut ComposeBlock, With<crate::ui::tiling::PaneFocus>>,
    mut submit_writer: MessageWriter<PromptSubmitted>,
    mut clipboard: Option<ResMut<super::SystemClipboard>>,
) {
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
            Action::SelectAll => {
                compose.select_all();
            }
            Action::Copy => {
                if let Some(ref mut clip) = clipboard {
                    if let Some(text) = compose.selected_text() {
                        if let Err(e) = clip.0.set_text(text) {
                            warn!("Copy failed: {e}");
                        }
                    }
                }
            }
            Action::Cut => {
                if let Some(ref mut clip) = clipboard {
                    if let Some(text) = compose.selected_text() {
                        if let Err(e) = clip.0.set_text(text) {
                            warn!("Cut failed: {e}");
                        } else {
                            compose.delete_selection();
                        }
                    }
                }
            }
            Action::Paste => {
                if let Some(ref mut clip) = clipboard {
                    match clip.0.get_text() {
                        Ok(text) => compose.insert(&text),
                        Err(e) => warn!("Paste failed: {e}"),
                    }
                }
            }
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
    entities: Res<EditorEntities>,
    mut main_cells: Query<&mut CellEditor, With<MainCell>>,
    mut editing_cells: Query<(&BlockCell, &mut BlockEditCursor), With<EditingBlockCell>>,
    mut clipboard: Option<ResMut<super::SystemClipboard>>,
) {
    let Ok((block_cell, mut cursor)) = editing_cells.single_mut() else {
        return;
    };
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(mut editor) = main_cells.get_mut(main_ent) else {
        return;
    };

    // Handle text insertion — clears selection, replaces if active
    for super::events::TextInputReceived(text) in text_events.read() {
        // Delete selection before inserting
        if let Some(anchor) = cursor.selection_anchor {
            let start = anchor.min(cursor.offset);
            let end = anchor.max(cursor.offset);
            let _ = editor.doc.edit_text(&block_cell.block_id, start, "", end - start);
            cursor.offset = start;
            cursor.selection_anchor = None;
        }
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
                // Delete selection first if active
                if let Some(anchor) = cursor.selection_anchor {
                    let start = anchor.min(cursor.offset);
                    let end = anchor.max(cursor.offset);
                    let _ = editor.doc.edit_text(&block_cell.block_id, start, "", end - start);
                    cursor.offset = start;
                    cursor.selection_anchor = None;
                }
                if editor
                    .doc
                    .edit_text(&block_cell.block_id, cursor.offset, "\n", 0)
                    .is_ok()
                {
                    cursor.offset += 1;
                }
            }
            Action::Backspace => {
                // Delete selection if active
                if let Some(anchor) = cursor.selection_anchor {
                    let start = anchor.min(cursor.offset);
                    let end = anchor.max(cursor.offset);
                    if editor.doc.edit_text(&block_cell.block_id, start, "", end - start).is_ok() {
                        cursor.offset = start;
                    }
                    cursor.selection_anchor = None;
                } else if cursor.offset > 0
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
                // Delete selection if active
                if let Some(anchor) = cursor.selection_anchor {
                    let start = anchor.min(cursor.offset);
                    let end = anchor.max(cursor.offset);
                    if editor.doc.edit_text(&block_cell.block_id, start, "", end - start).is_ok() {
                        cursor.offset = start;
                    }
                    cursor.selection_anchor = None;
                } else if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
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
                cursor.selection_anchor = None;
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
                cursor.selection_anchor = None;
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
                cursor.selection_anchor = None;
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let before_cursor = &text[..cursor.offset];
                    cursor.offset = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
                }
            }
            Action::CursorEnd => {
                cursor.selection_anchor = None;
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let after_cursor = &text[cursor.offset..];
                    cursor.offset += after_cursor.find('\n').unwrap_or(after_cursor.len());
                }
            }
            Action::SelectAll => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    if !block.content.is_empty() {
                        cursor.selection_anchor = Some(0);
                        cursor.offset = block.content.len();
                    }
                }
            }
            Action::Copy => {
                if let Some(ref mut clip) = clipboard {
                    if let Some(anchor) = cursor.selection_anchor {
                        if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                            let start = anchor.min(cursor.offset);
                            let end = anchor.max(cursor.offset);
                            if end <= block.content.len() {
                                let selected = &block.content[start..end];
                                if let Err(e) = clip.0.set_text(selected) {
                                    warn!("Copy failed: {e}");
                                }
                            }
                        }
                    }
                }
            }
            Action::Cut => {
                if let Some(ref mut clip) = clipboard {
                    if let Some(anchor) = cursor.selection_anchor {
                        if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                            let start = anchor.min(cursor.offset);
                            let end = anchor.max(cursor.offset);
                            if end <= block.content.len() {
                                let selected = block.content[start..end].to_string();
                                if let Err(e) = clip.0.set_text(&selected) {
                                    warn!("Cut failed: {e}");
                                } else {
                                    let _ = editor.doc.edit_text(
                                        &block_cell.block_id, start, "", end - start,
                                    );
                                    cursor.offset = start;
                                    cursor.selection_anchor = None;
                                }
                            }
                        }
                    }
                }
            }
            Action::Paste => {
                if let Some(ref mut clip) = clipboard {
                    match clip.0.get_text() {
                        Ok(pasted) => {
                            // Delete selection first if active
                            if let Some(anchor) = cursor.selection_anchor {
                                let start = anchor.min(cursor.offset);
                                let end = anchor.max(cursor.offset);
                                let _ = editor.doc.edit_text(
                                    &block_cell.block_id, start, "", end - start,
                                );
                                cursor.offset = start;
                                cursor.selection_anchor = None;
                            }
                            if editor
                                .doc
                                .edit_text(&block_cell.block_id, cursor.offset, &pasted, 0)
                                .is_ok()
                            {
                                cursor.offset += pasted.len();
                            }
                        }
                        Err(e) => warn!("Paste failed: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
}

// ============================================================================
// DEFENSIVE CLEANUP
// ============================================================================

/// Safety net: remove EditingBlockCell/BlockEditCursor if FocusArea is not EditingBlock.
/// 
/// This prevents stale markers from accumulating due to logic bugs in transition handlers.
/// Runs at the end of InputPhase::Handle so it catches anything the handlers missed.
pub fn cleanup_stale_editing_markers(
    mut commands: Commands,
    focus_area: Res<FocusArea>,
    editing_cells: Query<Entity, With<EditingBlockCell>>,
) {
    if matches!(*focus_area, FocusArea::EditingBlock) {
        return; // Markers are valid
    }
    for entity in editing_cells.iter() {
        commands.entity(entity).remove::<EditingBlockCell>();
        commands.entity(entity).remove::<BlockEditCursor>();
        trace!("Cleaned up stale EditingBlockCell on {:?}", entity);
    }
}

/// Safety net: remove FocusedBlockCell if FocusArea is not suitable for navigation.
/// 
/// Prevents ghost highlights when focus switches to areas where j/k navigation
/// is not active (Compose, Constellation, etc).
pub fn cleanup_stale_focused_markers(
    mut commands: Commands,
    focus_area: Res<FocusArea>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    // Only valid in Conversation or EditingBlock (where it serves as an edit anchor)
    if matches!(*focus_area, FocusArea::Conversation | FocusArea::EditingBlock) {
        return;
    }
    for entity in focused_markers.iter() {
        commands.entity(entity).remove::<FocusedBlockCell>();
        debug!("Cleaned up stale FocusedBlockCell on {:?}", entity);
    }
}
