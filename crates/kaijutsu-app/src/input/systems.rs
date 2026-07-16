//! Input action handlers — systems that consume ActionFired messages.
//!
//! All domain input handlers live here. They read `ActionFired` or
//! `TextInputReceived` messages instead of raw keyboard events.

use bevy::prelude::*;

use super::action::Action;
use super::events::ActionFired;
use super::focus::FocusArea;

// ============================================================================
// FOCUS CYCLING — Tab/Shift+Tab
// ============================================================================

/// Handle CycleFocusForward and CycleFocusBackward actions.
///
/// Within-conversation Tab cycle: Compose → Conversation → Compose.
pub fn handle_focus_cycle(mut actions: MessageReader<ActionFired>, mut focus: ResMut<FocusArea>) {
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::CycleFocusForward => {
                *focus = match focus.as_ref() {
                    FocusArea::Compose => FocusArea::Conversation,
                    FocusArea::Conversation => FocusArea::Compose,
                    // Don't cycle out of dialog/editing
                    other => other.clone(),
                };
            }
            Action::CycleFocusBackward => {
                *focus = match focus.as_ref() {
                    FocusArea::Compose => FocusArea::Conversation,
                    FocusArea::Conversation => FocusArea::Compose,
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
///
/// Only handles chat surface summoning. Shell surface is activated via
/// `handle_toggle_surface` (Ctrl+Z).
pub fn handle_focus_compose(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<super::focus::ActiveSurface>,
    mut overlay: Query<&mut crate::cell::InputOverlay, With<crate::cell::InputOverlayMarker>>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut vim: ResMut<crate::input::vim::VimMachineResource>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::FocusCompose | Action::SummonChat) {
            continue;
        }
        *focus = FocusArea::Compose;
        *surface = super::focus::ActiveSurface::Chat;

        // Set the overlay mode and restore text from CRDT if available
        if let Ok(mut overlay) = overlay.single_mut() {
            overlay.mode = crate::cell::InputMode::Chat;
            // Restore draft from CRDT InputDocEntry if overlay is empty
            // and no clear is pending (submit/escape×3 in flight).
            if overlay.text.is_empty()
                && let Some(ctx_id) = doc_cache.active_id()
                && let Some(cached) = doc_cache.get(ctx_id)
                && !cached.input_pending_clear
                && let Some(ref input) = cached.input
            {
                let crdt_text = input.text();
                if !crdt_text.is_empty() {
                    overlay.text = crdt_text;
                    overlay.cursor = overlay.text.len();
                }
            }

            // Always reset vim state to Normal first — clears any stale
            // operator-pending or visual state from a previous focus session.
            use modalkit::keybindings::BindingMachine;
            vim.machine.reset_mode();
            // Drain actions queued by mode transition (enter-hook fires
            // CursorClose + motion-back + Checkpoint when leaving Insert).
            while vim.machine.pop().is_some() {}

            // Vim mode transition:
            // - Empty overlay: enter Insert mode (ready to type immediately)
            // - Draft text: stay in Normal mode (user reviews before editing)
            if overlay.text.is_empty() {
                // Synthesize 'i' keypress to enter Insert mode
                use modalkit::crossterm::event::KeyCode;
                vim.machine.input_key(KeyCode::Char('i').into());
                while vim.machine.pop().is_some() {}
            }
            overlay.vim_mode = vim.machine.show_mode();
        }
    }
}

/// Handle ToggleSurface action (Ctrl+Z — symmetric chat ↔ shell toggle).
///
/// Toggles ActiveSurface and sets FocusArea::Compose to summon the
/// appropriate surface. Each surface maintains its own draft text.
pub fn handle_toggle_surface(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<super::focus::ActiveSurface>,
    mut vim: ResMut<crate::input::vim::VimMachineResource>,
    mut chat_overlay: Query<
        &mut crate::cell::InputOverlay,
        With<crate::cell::InputOverlayMarker>,
    >,
    mut shell_overlay: Query<
        &mut crate::cell::InputOverlay,
        (
            With<crate::view::shell_dock::ShellDockMarker>,
            Without<crate::cell::InputOverlayMarker>,
        ),
    >,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::ToggleSurface) {
            continue;
        }

        surface.toggle();
        *focus = FocusArea::Compose;

        // Reset vim state for the newly active surface.
        use modalkit::keybindings::BindingMachine;
        vim.machine.reset_mode();
        while vim.machine.pop().is_some() {}

        // Get the overlay for the newly active surface
        let overlay = if surface.is_shell() {
            shell_overlay.single_mut().ok()
        } else {
            chat_overlay.single_mut().ok()
        };

        if let Some(mut overlay) = overlay {
            // Enter Insert mode if overlay is empty
            if overlay.text.is_empty() {
                use modalkit::crossterm::event::KeyCode;
                vim.machine.input_key(KeyCode::Char('i').into());
                while vim.machine.pop().is_some() {}
            }
            overlay.vim_mode = vim.machine.show_mode();
        }
    }
}

/// Handle PopLevel action for the conversation screen (Escape — "go up").
///
/// Scene consumers (room, well, patch bay, fsn) handle PopLevel for their
/// own levels; this system owns the conversation-side transitions:
/// 1. FocusArea::Dialog → ignored (dialog systems own their Escape)
/// 2. FocusArea::Compose → FocusArea::Conversation
pub fn handle_pop_level(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<super::focus::ActiveSurface>,
    mut vim: ResMut<crate::input::vim::VimMachineResource>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::PopLevel) {
            continue;
        }

        // 1. Dialogs handle their own Escape/PopLevel.
        if matches!(*focus, FocusArea::Dialog) {
            continue;
        }

        // 2. Normal focus transitions
        if matches!(focus.as_ref(), FocusArea::Compose) {
            // Reset vim to Normal mode when leaving compose
            use modalkit::keybindings::BindingMachine;
            vim.machine.reset_mode();
            *focus = FocusArea::Conversation;

            // If escaping from shell surface, return to chat as default
            if surface.is_shell() {
                *surface = super::focus::ActiveSurface::Chat;
            }
        }
    }
}

/// The prefilled-`kj` prompt pattern (docs/input.md; Amy 2026-07-16: "pop a
/// kj so the user can type and hit enter — we might use that pattern
/// elsewhere"): summon the SHELL surface with a command line already typed,
/// cursor at the end, in Insert mode. Enter runs it; Esc-Esc abandons it.
/// The shell surface is local-only (no CRDT input sync), so the prefill is
/// a plain overlay write.
pub fn handle_prompt_prefill(
    mut actions: MessageReader<ActionFired>,
    mut focus: ResMut<FocusArea>,
    mut surface: ResMut<super::focus::ActiveSurface>,
    mut vim: ResMut<crate::input::vim::VimMachineResource>,
    mut shell_overlay: Query<
        &mut crate::cell::InputOverlay,
        With<crate::view::shell_dock::ShellDockMarker>,
    >,
) {
    for ActionFired { action, .. } in actions.read() {
        let prefill = match action {
            Action::PromptContextRename => "kj context rename ",
            Action::PromptContextSwitch => "kj context switch ",
            _ => continue,
        };

        *surface = super::focus::ActiveSurface::Shell;
        *focus = FocusArea::Compose;

        let Ok(mut overlay) = shell_overlay.single_mut() else {
            continue;
        };
        overlay.text = prefill.to_string();
        overlay.cursor = overlay.text.len();
        overlay.selection_anchor = None;

        // Fresh vim state, straight into Insert at the end of the line.
        use modalkit::keybindings::BindingMachine;
        vim.machine.reset_mode();
        while vim.machine.pop().is_some() {}
        vim.machine
            .input_key(modalkit::crossterm::event::KeyCode::Char('i').into());
        while vim.machine.pop().is_some() {}
        overlay.vim_mode = vim.machine.show_mode();
    }
}

/// Handle DetachToConversation (`Ctrl+A d`) — back to the Conversation view
/// from any scene or the editor. The editor session stays alive kernel-side
/// (same suspend semantics as its Ctrl+Z intercept); scene teardown rides
/// the screens' own OnExit schedules.
pub fn handle_detach(
    mut actions: MessageReader<ActionFired>,
    screen: Res<State<crate::ui::screen::Screen>>,
    mut next: ResMut<NextState<crate::ui::screen::Screen>>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::DetachToConversation) {
            continue;
        }
        if *screen.get() != crate::ui::screen::Screen::Conversation {
            next.set(crate::ui::screen::Screen::Conversation);
        }
    }
}

/// Handle InterruptContext action — multi-press Ctrl+C interrupt.
///
/// Press count determines escalation:
/// - 1st press: soft interrupt (stop agentic loop after current tool turn)
/// - 2nd press: hard interrupt (abort LLM stream + kill kaish jobs)
/// - 3rd press: hard interrupt + clear compose buffer
///
/// Does NOT change focus — that's handle_unfocus's job (Escape key).
pub fn handle_interrupt(
    mut actions: MessageReader<ActionFired>,
    mut interrupt_state: ResMut<crate::input::interrupt::InterruptState>,
    mut chat_overlay: Query<
        &mut crate::cell::InputOverlay,
        With<crate::cell::InputOverlayMarker>,
    >,
    mut shell_overlay: Query<
        &mut crate::cell::InputOverlay,
        (
            With<crate::view::shell_dock::ShellDockMarker>,
            Without<crate::cell::InputOverlayMarker>,
        ),
    >,
    surface: Res<super::focus::ActiveSurface>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    actor: Option<Res<crate::connection::RpcActor>>,
) {
    for ActionFired { action, .. } in actions.read() {
        let _immediate = match action {
            Action::InterruptContext { immediate } => *immediate,
            _ => continue,
        };

        let count = interrupt_state.press();

        // Escalate based on press count
        let effective_immediate = count >= 2;

        // Fire RPC interrupt (fire-and-forget)
        if let Some(ref actor) = actor
            && let Some(ctx_id) = doc_cache.active_id()
        {
            let handle = actor.handle.clone();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle.interrupt_context(ctx_id, effective_immediate).await {
                        Ok(success) => {
                            log::debug!(
                                "interrupt_context: ctx={}, immediate={}, success={}",
                                ctx_id,
                                effective_immediate,
                                success
                            );
                        }
                        Err(e) => log::warn!("interrupt_context failed: {e}"),
                    }
                })
                .detach();
        }

        // 3rd press: clear compose buffer + tell kernel to clear input doc
        if count >= 3 {
            let ov = if surface.is_shell() {
                shell_overlay.single_mut().ok()
            } else {
                chat_overlay.single_mut().ok()
            };
            if let Some(mut ov) = ov {
                ov.text.clear();
                ov.cursor = 0;
                ov.selection_anchor = None;
            }
            if let Some(ctx_id) = doc_cache.active_id() {
                // Suppress late TextOps until InputCleared re-fetch
                if let Some(cached) = doc_cache.get_mut(ctx_id) {
                    cached.input_pending_clear = true;
                }
                // Tell the kernel to clear — emits InputCleared
                if let Some(ref actor) = actor {
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.clear_input(ctx_id).await {
                                log::warn!("clear_input failed: {e}");
                            }
                        })
                        .detach();
                }
            }
            interrupt_state.reset();
        }
    }
}

// ============================================================================
// DEBUG HANDLERS — migrated from ui/debug.rs to consume ActionFired
// ============================================================================

/// Handle Quit action.
pub fn handle_quit(mut actions: MessageReader<ActionFired>, mut exit: MessageWriter<AppExit>) {
    for ActionFired { action, .. } in actions.read() {
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
    for ActionFired { action, .. } in actions.read() {
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
pub fn handle_screenshot(mut commands: Commands, mut actions: MessageReader<ActionFired>) {
    use bevy::render::view::screenshot::{Screenshot, save_to_disk};

    for ActionFired { action, .. } in actions.read() {
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

// ============================================================================
// BLOCK NAVIGATION
// ============================================================================

use crate::cell::{
    BlockCell, BlockCellContainer, CellEditor, ConversationScrollState,
    EditorEntities, FocusTarget, FocusedBlockCell, MainCell,
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
    geometries: Query<&crate::view::geometry::ConversationGeometry, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut focus: ResMut<FocusTarget>,
    mut scroll_state: ResMut<ConversationScrollState>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    let mut direction: Option<NavigationDirection> = None;

    for ActionFired { action, .. } in actions.read() {
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
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    // Navigate over geometry block rows: every block is navigable whether or
    // not it currently has an entity (the band respawns it as the scroll
    // catches up).
    use crate::view::geometry::RowKey;
    let block_rows: Vec<(kaijutsu_crdt::BlockId, f32, f32)> = geom
        .rows()
        .iter()
        .filter_map(|row| match row.key {
            RowKey::Block(id) => Some((id, row.y_offset, row.height)),
            RowKey::Header(_) => None,
        })
        .collect();
    if block_rows.is_empty() {
        return;
    }

    // Find current focus index
    let current_idx = focus
        .block_id
        .as_ref()
        .and_then(|id| block_rows.iter().position(|(bid, _, _)| bid == id));

    // Calculate new index
    let new_idx = match direction {
        NavigationDirection::Next => match current_idx {
            Some(i) if i + 1 < block_rows.len() => i + 1,
            Some(i) => i,
            None => 0,
        },
        NavigationDirection::Previous => match current_idx {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => block_rows.len() - 1,
        },
        NavigationDirection::First => 0,
        NavigationDirection::Last => block_rows.len() - 1,
    };

    let (new_id, row_y, row_h) = block_rows[new_idx];

    // Update focus resource
    focus.focus_block(new_id);

    // Remove old FocusedBlockCell markers
    for entity in focused_markers.iter() {
        commands.entity(entity).remove::<FocusedBlockCell>();
    }

    // Add FocusedBlockCell marker when the entity exists this frame;
    // otherwise apply_focused_block_marker picks it up once the band
    // spawns it (the scroll below moves the band there).
    if let Some(entity) = container.get_entity(&new_id) {
        commands.entity(entity).insert(FocusedBlockCell);
    }

    // Scroll to keep the focused block visible — geometry provides the
    // rect even for entity-less blocks.
    scroll_to_rect_visible(&mut scroll_state, row_y, row_h);

    debug!("Block focus: {:?} (index {})", new_id, new_idx);
}

/// Apply the `FocusedBlockCell` marker once the focused block's entity
/// exists. Focus nav can land on a block that is outside the entity band
/// (despawned); the nav scrolls toward it, the band spawns it a frame or
/// two later, and this system attaches the marker then. Also strips stale
/// markers when focus moved on while an entity was despawned.
pub fn apply_focused_block_marker(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    focus: Res<FocusTarget>,
    marked: Query<(Entity, &BlockCell), With<FocusedBlockCell>>,
) {
    let Some(focused_id) = focus.block_id else {
        return;
    };
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let mut already_marked = false;
    for (entity, cell) in marked.iter() {
        if cell.block_id == focused_id {
            already_marked = true;
        } else {
            commands.entity(entity).remove::<FocusedBlockCell>();
        }
    }

    if !already_marked
        && let Some(entity) = container.get_entity(&focused_id)
    {
        commands.entity(entity).insert(FocusedBlockCell);
    }
}

/// Scroll to keep a block's rect visible in the viewport.
fn scroll_to_rect_visible(scroll_state: &mut ConversationScrollState, y_offset: f32, height: f32) {
    let block_top = y_offset;
    let block_bottom = y_offset + height;
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
/// Prevents gamepad scroll leaking into dialogs.
pub fn handle_scroll(
    mut actions: MessageReader<ActionFired>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    for ActionFired { action, .. } in actions.read() {
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
// COLLAPSE
// ============================================================================

/// Handle CollapseToggle action (toggle thinking block collapse).
///
/// Guarded to Conversation focus — CollapseToggle is Navigation-only.
pub fn handle_collapse_toggle(
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusTarget>,
    mut cells: Query<&mut CellEditor>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::CollapseToggle) {
            continue;
        }

        let Some(focused_entity) = focus.entity else {
            continue;
        };

        let Ok(mut editor) = cells.get_mut(focused_entity) else {
            continue;
        };

        // Find thinking blocks to toggle
        let thinking_blocks: Vec<_> = editor
            .blocks()
            .iter()
            .filter(|b| matches!(b.kind, kaijutsu_crdt::BlockKind::Thinking))
            .map(|b| b.id)
            .collect();

        if thinking_blocks.is_empty() {
            continue;
        }
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
    }
}

/// Handle ToggleBlockExcluded action — x in Navigation.
///
/// Toggles the excluded flag on the focused block via set_block_excluded RPC.
pub fn handle_toggle_block_excluded(
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusTarget>,
    cells: Query<&CellEditor>,
    entities: Res<EditorEntities>,
    actor: Option<Res<crate::connection::RpcActor>>,
    doc_cache: Res<crate::cell::DocumentCache>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::ToggleBlockExcluded) {
            continue;
        }

        let Some(ref block_id) = focus.block_id else {
            continue;
        };

        let Some(main_ent) = entities.main_cell else {
            continue;
        };

        let Ok(editor) = cells.get(main_ent) else {
            continue;
        };

        // Find the block's current excluded state — one snapshot, not a
        // whole-document clone.
        let Some(block) = editor.block_snapshot(block_id) else {
            continue;
        };
        let new_excluded = !block.excluded;
        let bid = *block_id;

        // Fire RPC to toggle exclusion
        if let (Some(actor), Some(ctx_id)) = (&actor, doc_cache.active_id()) {
            let handle = actor.handle.clone();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle.set_block_excluded(ctx_id, &bid, new_excluded).await {
                        Ok(_) => {
                            log::info!(
                                "set_block_excluded: block={:?} excluded={}",
                                bid,
                                new_excluded
                            );
                        }
                        Err(e) => log::warn!("set_block_excluded failed: {e}"),
                    }
                })
                .detach();
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
pub fn handle_tiling(mut actions: MessageReader<ActionFired>, mut tree: ResMut<TilingTree>) {
    for ActionFired { action, .. } in actions.read() {
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
// TEXT INPUT (COMPOSE + INLINE BLOCK EDITING)
// ============================================================================

/// Handle text input in the input overlay (Compose area).
///
/// Consumes TextInputReceived for character insertion and ActionFired for
/// editing actions (Submit, Backspace, Delete, cursor movement).
/// Uses `ActiveSurface` to determine shell vs chat routing on Submit.
///
/// Dual-writes to CRDT input document via `edit_input` RPC for persistence.
/// Submit uses `submit_input` RPC — overlay cleared only on `InputCleared`.
pub fn handle_compose_input(
    mut text_events: MessageReader<super::events::TextInputReceived>,
    mut actions: MessageReader<ActionFired>,
    mut chat_overlay: Query<
        &mut crate::cell::InputOverlay,
        With<crate::cell::InputOverlayMarker>,
    >,
    mut shell_overlay: Query<
        &mut crate::cell::InputOverlay,
        (
            With<crate::view::shell_dock::ShellDockMarker>,
            Without<crate::cell::InputOverlayMarker>,
        ),
    >,
    mut clipboard: Option<ResMut<super::SystemClipboard>>,
    actor: Option<Res<crate::connection::RpcActor>>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
    mut focus: ResMut<FocusArea>,
    surface: Res<super::focus::ActiveSurface>,
) {
    let mut overlay = if surface.is_shell() {
        match shell_overlay.single_mut() {
            Ok(o) => o,
            Err(_) => return,
        }
    } else {
        match chat_overlay.single_mut() {
            Ok(o) => o,
            Err(_) => return,
        }
    };

    let ctx_id = doc_cache.active_id();

    let is_shell = surface.is_shell();

    // Handle text insertion
    for super::events::TextInputReceived(text) in text_events.read() {
        let pos_before = overlay.cursor;
        overlay.insert(text);

        // Only dual-write to CRDT for chat input. Shell input is local-only.
        if !is_shell {
            if let (Some(actor), Some(ctx)) = (&actor, ctx_id) {
                let handle = actor.handle.clone();
                let insert_text = text.clone();
                let pos = pos_before as u64;
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        if let Err(e) = handle.edit_input(ctx, pos, &insert_text, 0).await {
                            log::warn!("edit_input (insert) failed: {e}");
                        }
                    })
                    .detach();
            }
        }
    }

    // Handle editing actions
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::Submit => {
                if !overlay.is_empty()
                    && let (Some(actor), Some(ctx)) = (&actor, ctx_id)
                {
                    let handle = actor.handle.clone();

                    if is_shell {
                        // Shell: call shell_execute directly with local text
                        let code = overlay.text.clone();
                        bevy::tasks::IoTaskPool::get()
                            .spawn(async move {
                                match handle.shell_execute(&code, ctx, true).await {
                                    Ok(block_id) => {
                                        log::info!("shell_execute ok: {:?}", block_id)
                                    }
                                    Err(e) => log::error!("shell_execute failed: {e}"),
                                }
                            })
                            .detach();

                        // Clear shell overlay locally (no CRDT involvement)
                        overlay.text.clear();
                        overlay.cursor = 0;
                        overlay.selection_anchor = None;
                    } else {
                        // Chat: submit via CRDT → submit_input RPC
                        bevy::tasks::IoTaskPool::get()
                            .spawn(async move {
                                match handle.submit_input(ctx, false).await {
                                    Ok(result) => {
                                        log::info!("submit_input ok: {:?}", result.block_id)
                                    }
                                    Err(e) => log::error!("submit_input failed: {e}"),
                                }
                            })
                            .detach();

                        // Clear overlay optimistically. The server's InputCleared
                        // confirms via re-fetch (see handle_input_doc_events).
                        overlay.text.clear();
                        overlay.cursor = 0;
                        overlay.selection_anchor = None;

                        // Suppress late TextOps until InputCleared re-fetch
                        if let Some(cached) = doc_cache.get_mut(ctx) {
                            cached.input_pending_clear = true;
                        }
                    }

                    // Dismiss overlay by transitioning focus
                    *focus = FocusArea::Conversation;
                }
                // No else — if not connected, do nothing (no offline fallback)
            }
            Action::InsertNewline => {
                let pos_before = overlay.cursor;
                overlay.insert("\n");

                if !is_shell && let (Some(actor), Some(ctx)) = (&actor, ctx_id) {
                    let handle = actor.handle.clone();
                    let pos = pos_before as u64;
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.edit_input(ctx, pos, "\n", 0).await {
                                log::warn!("edit_input (newline) failed: {e}");
                            }
                        })
                        .detach();
                }
            }
            Action::Backspace => {
                let had_selection = overlay.selection_range().is_some();
                let (del_pos, del_len) = if had_selection {
                    let range = overlay.selection_range().unwrap();
                    (range.start, range.end - range.start)
                } else if overlay.cursor > 0 {
                    let prev = overlay.text[..overlay.cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    (prev, overlay.cursor - prev)
                } else {
                    (0, 0)
                };

                overlay.backspace();

                if !is_shell
                    && del_len > 0
                    && let (Some(actor), Some(ctx)) = (&actor, ctx_id)
                {
                    let handle = actor.handle.clone();
                    let pos = del_pos as u64;
                    let delete = del_len as u64;
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.edit_input(ctx, pos, "", delete).await {
                                log::warn!("edit_input (backspace) failed: {e}");
                            }
                        })
                        .detach();
                }
            }
            Action::Delete => {
                let had_selection = overlay.selection_range().is_some();
                let (del_pos, del_len) = if had_selection {
                    let range = overlay.selection_range().unwrap();
                    (range.start, range.end - range.start)
                } else if overlay.cursor < overlay.text.len() {
                    let next = overlay.text[overlay.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| overlay.cursor + i)
                        .unwrap_or(overlay.text.len());
                    (overlay.cursor, next - overlay.cursor)
                } else {
                    (0, 0)
                };

                overlay.delete();

                if !is_shell
                    && del_len > 0
                    && let (Some(actor), Some(ctx)) = (&actor, ctx_id)
                {
                    let handle = actor.handle.clone();
                    let pos = del_pos as u64;
                    let delete = del_len as u64;
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.edit_input(ctx, pos, "", delete).await {
                                log::warn!("edit_input (delete) failed: {e}");
                            }
                        })
                        .detach();
                }
            }
            Action::CursorLeft => overlay.move_left(),
            Action::CursorRight => overlay.move_right(),
            // Xterm-style paste (docs/input.md): Ctrl+V reads CLIPBOARD;
            // middle-click reads PRIMARY (Linux), falling back to CLIPBOARD
            // where no primary selection exists.
            Action::Paste | Action::PastePrimary => {
                if let Some(ref mut clip) = clipboard {
                    let fetched = if matches!(action, Action::PastePrimary) {
                        primary_selection_text(&mut clip.0)
                    } else {
                        clip.0.get_text()
                    };
                    match fetched {
                        Ok(text) => {
                            let sel_range = overlay.selection_range();
                            let pos_before = if let Some(ref range) = sel_range {
                                range.start
                            } else {
                                overlay.cursor
                            };
                            let del_len = sel_range.as_ref().map(|r| r.end - r.start).unwrap_or(0);

                            overlay.insert(&text);

                            if !is_shell {
                                if let (Some(actor), Some(ctx)) = (&actor, ctx_id) {
                                    let handle = actor.handle.clone();
                                    let pos = pos_before as u64;
                                    let delete = del_len as u64;
                                    let insert_text = text.clone();
                                    bevy::tasks::IoTaskPool::get()
                                        .spawn(async move {
                                            if let Err(e) =
                                                handle
                                                    .edit_input(ctx, pos, &insert_text, delete)
                                                    .await
                                            {
                                                log::warn!("edit_input (paste) failed: {e}");
                                            }
                                        })
                                        .detach();
                                }
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

/// The PRIMARY selection on Linux (X11/Wayland); other platforms have no
/// primary selection, so middle-click falls back to the ordinary clipboard.
fn primary_selection_text(clip: &mut arboard::Clipboard) -> Result<String, arboard::Error> {
    #[cfg(target_os = "linux")]
    {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        clip.get().clipboard(LinuxClipboardKind::Primary).text()
    }
    #[cfg(not(target_os = "linux"))]
    {
        clip.get_text()
    }
}

// handle_block_edit_input removed with FocusArea::EditingBlock.
// To restore inline block editing: add EditingBlock to FocusArea enum,
// re-add the system body, and wire it back in mod.rs.

// ============================================================================
// DEFENSIVE CLEANUP
// ============================================================================

/// Safety net: remove FocusedBlockCell if FocusArea is not Conversation.
///
/// Prevents ghost highlights when focus switches to Compose or Dialog.
pub fn cleanup_stale_focused_markers(
    mut commands: Commands,
    focus_area: Res<FocusArea>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    if matches!(*focus_area, FocusArea::Conversation) {
        return;
    }
    for entity in focused_markers.iter() {
        commands.entity(entity).remove::<FocusedBlockCell>();
        debug!("Cleaned up stale FocusedBlockCell on {:?}", entity);
    }
}
