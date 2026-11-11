//! Input action handlers — systems that consume ActionFired messages.
//!
//! Domain input handlers live here; compose's own are in `input/compose/`.
//! They read `ActionFired` messages instead of raw keyboard events.

use bevy::prelude::*;

use super::Action;
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
/// - 3rd press: hard interrupt + clear the compose surface
///
/// Does NOT change focus — that is Escape's job.
pub fn handle_interrupt(
    mut actions: MessageReader<ActionFired>,
    mut interrupt_state: ResMut<crate::input::interrupt::InterruptState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    actor: Option<Res<crate::connection::RpcActor>>,
    mut compose: crate::input::compose::systems::ComposeIo,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::InterruptContext { .. }) {
            continue;
        }

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

        if count >= 3 {
            crate::input::compose::systems::clear_compose(&mut compose);
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
    mut debug_options: ResMut<GlobalUiDebugOptions>,
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

use crate::cell::{CellEditor, ConversationScrollState, EditorEntities, FocusTarget, MainCell};

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
    mut actions: MessageReader<ActionFired>,
    entities: Res<EditorEntities>,
    geometries: Query<&crate::view::geometry::ConversationGeometry, With<MainCell>>,
    mut focus: ResMut<FocusTarget>,
    mut scroll_state: ResMut<ConversationScrollState>,
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

    // Navigate over geometry block rows: every block is navigable whether or
    // not it currently has an entity (the band respawns it as the scroll
    // catches up).
    use crate::view::geometry::RowKey;
    let block_rows: Vec<(kaijutsu_types::BlockId, f32, f32)> = geom
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

    let new_idx = next_focus_index(&direction, current_idx, block_rows.len());

    let (new_id, row_y, row_h) = block_rows[new_idx];

    // Update focus resource — the surface path draws its focus ring from
    // `FocusTarget.block_id` alone (`view::surface::chrome`), so there is no
    // marker component to move.
    focus.focus_block(new_id);

    // Scroll to keep the focused block visible — geometry provides the
    // rect even for entity-less blocks.
    scroll_to_rect_visible(&mut scroll_state, row_y, row_h);

    debug!("Block focus: {:?} (index {})", new_id, new_idx);
}

/// Handle JumpToLatestError — find the most recent non-excluded
/// `Status::Error` block and focus + scroll it into view.
///
/// A failed tool call's `BlockKind::Error` child is inserted immediately
/// after its `ToolResult` (`insert_error_block_as`'s `after: Some(parent_id)`
/// on the kernel side), so "last `Status::Error` block in document order"
/// naturally lands on the diagnostic-carrying Error block rather than the
/// bare ToolResult — the useful one to land on. A stream error's parent
/// (an assistant turn) is never itself `Status::Error`, so the same rule
/// picks its Error child too.
///
/// Follow-up not done here: this focuses the Error block itself, not its
/// parent (the failed ToolResult/turn the provenance line names). Making
/// both visible at once would need either scrolling to a rect spanning
/// both rows or a second "jump to this error's parent" action — noted as
/// follow-up work rather than built speculatively.
pub fn handle_jump_to_latest_error(
    mut actions: MessageReader<ActionFired>,
    entities: Res<EditorEntities>,
    editors: Query<&CellEditor, With<MainCell>>,
    geometries: Query<&crate::view::geometry::ConversationGeometry, With<MainCell>>,
    mut focus: ResMut<FocusTarget>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    let mut fired = false;
    for ActionFired { action, .. } in actions.read() {
        if matches!(action, Action::JumpToLatestError) {
            fired = true;
        }
    }
    if !fired {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = editors.get(main_ent) else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };

    let Some(target_id) = find_latest_error_block(&editor.blocks()) else {
        debug!("JumpToLatestError: no non-excluded error blocks in document");
        return;
    };

    let Some(row) = geom.block_row(&target_id) else {
        return;
    };
    let (row_y, row_h) = (row.y_offset, row.height);

    focus.focus_block(target_id);

    scroll_to_rect_visible(&mut scroll_state, row_y, row_h);

    debug!("Jumped to latest error block: {:?}", target_id);
}

/// Pure lookup: the last non-excluded `Status::Error` block in document
/// order, or `None` if there isn't one. Extracted from
/// `handle_jump_to_latest_error` so the "which block wins" logic is
/// unit-testable without a Bevy `App`.
fn find_latest_error_block(blocks: &[kaijutsu_types::BlockSnapshot]) -> Option<kaijutsu_types::BlockId> {
    blocks
        .iter()
        .rev()
        .find(|b| !b.excluded && b.status == kaijutsu_types::Status::Error)
        .map(|b| b.id)
}

/// Pure index math for block-focus navigation. `len` must be > 0 (the
/// caller returns early on an empty row list). Next/Previous clamp at the
/// document ends — no wraparound — and from an unfocused start, Next
/// enters at the top while Previous enters at the bottom.
fn next_focus_index(
    direction: &NavigationDirection,
    current: Option<usize>,
    len: usize,
) -> usize {
    match direction {
        NavigationDirection::Next => match current {
            Some(i) if i + 1 < len => i + 1,
            Some(i) => i,
            None => 0,
        },
        NavigationDirection::Previous => match current {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => len - 1,
        },
        NavigationDirection::First => 0,
        NavigationDirection::Last => len - 1,
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
        // `reached_bottom()`, not `is_at_bottom()`: sticky-follow semantics
        // (scroll-relief slice 0) require every re-latch site to use the
        // same ~1px "true bottom" threshold as `scroll_by`, or this looser
        // 50px band becomes a backdoor around it — block navigation that
        // lands 40px from bottom would otherwise re-arm follow and the next
        // streamed block yanks the view back down.
        scroll_state.following = scroll_state.reached_bottom();
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

/// Handle the fold toggle (`Action::CollapseToggle`, `c` and `Ctrl+O` in
/// Navigation): fold or unfold the block under the reader, chosen by
/// [`fold_toggle_target`].
///
/// The fold is the viewer's own (`CellEditor::toggle_fold`); nothing is sent
/// to the kernel. The editor is the MainCell's, found through
/// `EditorEntities`.
pub fn handle_collapse_toggle(
    mut actions: MessageReader<ActionFired>,
    entities: Res<EditorEntities>,
    focus: Res<FocusTarget>,
    scroll_state: Res<ConversationScrollState>,
    geometries: Query<&crate::view::geometry::ConversationGeometry, With<MainCell>>,
    mut cells: Query<&mut CellEditor>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::CollapseToggle) {
            continue;
        }
        let Some(main_ent) = entities.main_cell else {
            continue;
        };
        let reader = focus.reader_block(&scroll_state, geometries.get(main_ent).ok());
        let Ok(mut editor) = cells.get_mut(main_ent) else {
            continue;
        };
        let Some(target) = fold_toggle_target(&editor.blocks(), reader) else {
            debug!("fold toggle: no block at or above the reader has a fold");
            continue;
        };
        if let Some(folded) = editor.toggle_fold(&target) {
            debug!("fold toggle: {target:?} {}", if folded { "folded" } else { "opened" });
        }
    }
}

/// The block the fold toggle moves: the nearest block with a fold at or above
/// the reader's block (`kaijutsu_present::reader::target`).
fn fold_toggle_target(
    blocks: &[kaijutsu_types::BlockSnapshot],
    reader: Option<kaijutsu_types::BlockId>,
) -> Option<kaijutsu_types::BlockId> {
    kaijutsu_present::reader::target(blocks, reader, |b| {
        kaijutsu_present::fold::has_fold(b, b.collapsed)
    })
}

/// Handle `Action::YankBlock` (`y` in Navigation): yank the reader's block.
pub fn handle_yank_block(
    mut actions: MessageReader<ActionFired>,
    entities: Res<EditorEntities>,
    focus: Res<FocusTarget>,
    scroll_state: Res<ConversationScrollState>,
    geometries: Query<&crate::view::geometry::ConversationGeometry, With<MainCell>>,
    cells: Query<&CellEditor>,
    mut compose: ResMut<crate::input::compose::ComposeState>,
    clipboard: Option<Res<crate::input::ClipboardWriter>>,
    mut errors: ResMut<crate::view::components::GlobalErrorQueue>,
    time: Res<Time>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::YankBlock) {
            continue;
        }
        let Some(main_ent) = entities.main_cell else {
            continue;
        };
        let reader = focus.reader_block(&scroll_state, geometries.get(main_ent).ok());
        let Ok(editor) = cells.get(main_ent) else {
            continue;
        };
        let Some(text) = yank_target(&editor.blocks(), reader).and_then(|id| editor.block_snapshot(&id)).map(|b| b.content) else {
            errors.push("yank", "nothing to yank here", time.elapsed_secs_f64());
            continue;
        };
        compose.yank(&kaijutsu_editor::Yank::lines(&text));
        let lines = text.lines().count();
        if let Some(writer) = clipboard.as_ref() {
            writer.write(text);
        }
        let noun = if lines == 1 { "line" } else { "lines" };
        errors.push("yank", format!("yanked {lines} {noun} \u{2014} p or Ctrl+A ] puts"), time.elapsed_secs_f64());
    }
}

/// The block `y` yanks: the nearest block with text at or above the reader's
/// block (`kaijutsu_present::reader::target`).
fn yank_target(
    blocks: &[kaijutsu_types::BlockSnapshot],
    reader: Option<kaijutsu_types::BlockId>,
) -> Option<kaijutsu_types::BlockId> {
    kaijutsu_present::reader::target(blocks, reader, |b| !b.content.is_empty())
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
// CLIPBOARD
// ============================================================================

/// The PRIMARY selection on Linux (X11/Wayland); other platforms have no
/// primary selection, so middle-click falls back to the ordinary clipboard.
pub(crate) fn primary_selection_text(clip: &mut arboard::Clipboard) -> Result<String, arboard::Error> {
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

#[cfg(test)]
mod tests {
    use super::{
        NavigationDirection, find_latest_error_block, handle_navigate_blocks, next_focus_index,
        scroll_to_rect_visible,
    };
    use crate::cell::ConversationScrollState;
    use kaijutsu_types::{BlockId, BlockKind, BlockSnapshotBuilder, ContextId, PrincipalId, Status};

    /// A headless app running only `handle_collapse_toggle` over a main cell
    /// holding one block of each kind given, in order.
    fn fold_toggle_app(kinds: &[BlockKind]) -> (bevy::prelude::App, Vec<BlockId>) {
        let blocks: Vec<(BlockKind, String)> =
            kinds.iter().map(|kind| (*kind, "line".to_string())).collect();
        fold_toggle_app_with(&blocks)
    }

    /// Like [`fold_toggle_app`], with each block's content given. The view is
    /// scrolled off the tail, so the focused block is the reader's.
    fn fold_toggle_app_with(blocks: &[(BlockKind, String)]) -> (bevy::prelude::App, Vec<BlockId>) {
        use bevy::prelude::*;

        use super::handle_collapse_toggle;
        use crate::input::events::ActionFired;
        use crate::view::{CellEditor, EditorEntities, FocusTarget, MainCell};
        use kaijutsu_types::{ContentType, Role};

        let mut app = App::new();
        app.add_message::<ActionFired>();
        app.init_resource::<EditorEntities>();
        app.init_resource::<FocusTarget>();
        app.insert_resource(ConversationScrollState { following: false, ..Default::default() });
        app.add_systems(Update, handle_collapse_toggle);

        let mut editor = CellEditor::new();
        let mut ids: Vec<BlockId> = Vec::new();
        for (kind, content) in blocks {
            let id = editor
                .store
                .insert_block(
                    None,
                    ids.last(),
                    Role::Model,
                    *kind,
                    content.as_str(),
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert block");
            ids.push(id);
        }

        let main_ent = app.world_mut().spawn((editor, MainCell)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(main_ent);
        (app, ids)
    }

    fn press_fold_toggle(app: &mut bevy::prelude::App) {
        use crate::input::events::ActionFired;
        use crate::input::{Action, context::InputContext};

        app.world_mut().write_message(ActionFired::new(
            Action::CollapseToggle,
            InputContext::Navigation,
        ));
        app.update();
    }

    fn folded(app: &bevy::prelude::App, id: BlockId) -> bool {
        use crate::view::{CellEditor, EditorEntities};

        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        let editor = app.world().get::<CellEditor>(main_ent).unwrap();
        editor.store.get_block_snapshot(&id).unwrap().collapsed
    }

    /// The yank handler over the same main-cell harness as the fold toggle,
    /// with compose, a clipboard writer and the notice queue.
    fn yank_app(
        blocks: &[(BlockKind, &str)],
    ) -> (bevy::prelude::App, Vec<BlockId>, crossbeam_channel::Receiver<String>) {
        use bevy::prelude::*;

        let owned: Vec<(BlockKind, String)> =
            blocks.iter().map(|(k, c)| (*k, c.to_string())).collect();
        let (mut app, ids) = fold_toggle_app_with(&owned);
        let (writer, written) = crate::input::ClipboardWriter::for_test();
        app.insert_resource(writer);
        app.init_resource::<crate::input::compose::ComposeState>();
        app.init_resource::<crate::view::components::GlobalErrorQueue>();
        app.init_resource::<Time>();
        app.add_systems(Update, super::handle_yank_block);
        (app, ids, written)
    }

    fn press_yank(app: &mut bevy::prelude::App) {
        use crate::input::events::ActionFired;
        use crate::input::{Action, context::InputContext};

        app.world_mut().write_message(ActionFired::new(Action::YankBlock, InputContext::Navigation));
        app.update();
    }

    fn notices(app: &bevy::prelude::App) -> Vec<String> {
        app.world()
            .resource::<crate::view::components::GlobalErrorQueue>()
            .entries
            .iter()
            .map(|e| e.message.clone())
            .collect()
    }

    /// What `p` on the shell surface would put from the shared registers.
    fn registers_put(app: &mut bevy::prelude::App) -> String {
        let mut compose = app.world_mut().resource_mut::<crate::input::compose::ComposeState>();
        compose.put_shell();
        compose.shell.text()
    }

    /// `y` on the focused block puts its plain content, as whole lines, in the
    /// vi registers and the clipboard, and says how many lines.
    #[test]
    fn yank_block_takes_the_focused_blocks_content() {
        use crate::view::FocusTarget;

        let (mut app, ids, written) =
            yank_app(&[(BlockKind::Text, "one\ntwo"), (BlockKind::Text, "other")]);
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[0]);
        press_yank(&mut app);
        assert_eq!(written.try_iter().collect::<Vec<_>>(), vec!["one\ntwo".to_string()]);
        assert_eq!(registers_put(&mut app), "one\ntwo");
        assert_eq!(notices(&app), vec!["yanked 2 lines \u{2014} p or Ctrl+A ] puts".to_string()]);
    }

    /// A block with no text is passed over for the nearest one above with some.
    #[test]
    fn yank_block_passes_over_an_empty_block() {
        use crate::view::FocusTarget;

        let (mut app, ids, written) =
            yank_app(&[(BlockKind::Text, "kept"), (BlockKind::Text, "")]);
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[1]);
        press_yank(&mut app);
        assert_eq!(written.try_iter().collect::<Vec<_>>(), vec!["kept".to_string()]);
        assert_eq!(notices(&app), vec!["yanked 1 line \u{2014} p or Ctrl+A ] puts".to_string()]);
    }

    /// With no clipboard the registers still fill and nothing complains.
    #[test]
    fn yank_block_without_a_clipboard_still_fills_the_registers() {
        let (mut app, _ids, _written) = yank_app(&[(BlockKind::Text, "text")]);
        app.world_mut().remove_resource::<crate::input::ClipboardWriter>();
        press_yank(&mut app);
        assert_eq!(registers_put(&mut app), "text");
    }

    /// No block with text: a notice, and the clipboard and registers stay put.
    #[test]
    fn yank_block_with_nothing_to_yank_says_so() {
        let (mut app, _ids, written) = yank_app(&[(BlockKind::Text, "")]);
        press_yank(&mut app);
        assert_eq!(written.try_iter().count(), 0);
        assert_eq!(registers_put(&mut app), "");
        assert_eq!(notices(&app), vec!["nothing to yank here".to_string()]);
    }

    /// The fold toggle must work with nothing but `EditorEntities.main_cell`
    /// and no focused block: the editor is found through the main cell.
    #[test]
    fn the_fold_toggle_works_from_editor_entities_alone() {
        let (mut app, ids) = fold_toggle_app(&[BlockKind::Thinking]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[0]), "the toggle must reach the MainCell editor");
    }

    #[test]
    fn the_fold_toggle_moves_the_focused_foldable_block() {
        use crate::view::FocusTarget;

        let (mut app, ids) =
            fold_toggle_app(&[BlockKind::Thinking, BlockKind::Text, BlockKind::Thinking]);
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[0]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[0]), "the focused block folds");
        assert!(!folded(&app, ids[2]), "no other block moves, of any kind");

        press_fold_toggle(&mut app);
        assert!(!folded(&app, ids[0]), "a second press opens it again");
    }

    /// A text block between two reasoning blocks: the toggle takes the
    /// reasoning above the focus and never jumps to the newest below it.
    #[test]
    fn the_fold_toggle_takes_the_nearest_fold_above_the_focus() {
        use crate::view::FocusTarget;

        let (mut app, ids) = fold_toggle_app(&[
            BlockKind::Thinking,
            BlockKind::Text,
            BlockKind::Thinking,
        ]);
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[1]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[0]), "the reasoning above the focus folds");
        assert!(!folded(&app, ids[2]), "the newest, below the focus, does not move");
    }

    #[test]
    fn the_fold_toggle_without_focus_takes_the_newest_block_with_a_fold() {
        let (mut app, ids) = fold_toggle_app(&[
            BlockKind::Thinking,
            BlockKind::Thinking,
            BlockKind::Text,
        ]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[1]), "the newest reasoning folds");
        assert!(!folded(&app, ids[0]));
        assert!(!folded(&app, ids[2]));
    }

    #[test]
    fn the_fold_toggle_passes_over_a_short_open_result() {
        use crate::view::FocusTarget;

        let (mut app, ids) = fold_toggle_app(&[
            BlockKind::Thinking,
            BlockKind::ToolResult,
            BlockKind::Text,
        ]);
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[2]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[0]), "the short result has nothing worth folding");
        assert!(!folded(&app, ids[1]));
    }

    #[test]
    fn the_fold_toggle_takes_a_long_result_and_reopens_a_folded_short_one() {
        let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let (mut app, ids) = fold_toggle_app_with(&[
            (BlockKind::ToolResult, "ok".to_string()),
            (BlockKind::ToolResult, long),
        ]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[1]), "the long result folds");
        assert!(!folded(&app, ids[0]));

        // Fold the short one by hand: it has a fold now, so the toggle
        // reaches it from a focus on it.
        {
            use crate::view::{CellEditor, EditorEntities, FocusTarget};
            let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
            app.world_mut().get_mut::<CellEditor>(main_ent).unwrap().toggle_fold(&ids[0]);
            app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[0]);
        }
        assert!(folded(&app, ids[0]));
        press_fold_toggle(&mut app);
        assert!(!folded(&app, ids[0]), "a folded block can always be opened");
    }

    /// A focus the view has scrolled away from while following the tail is
    /// stale: the toggle reads the reader as on the tail.
    #[test]
    fn the_fold_toggle_ignores_a_stale_focus_while_following() {
        use crate::view::FocusTarget;

        let (mut app, ids) =
            fold_toggle_app(&[BlockKind::Thinking, BlockKind::Text, BlockKind::Thinking]);
        app.world_mut().resource_mut::<ConversationScrollState>().following = true;
        app.world_mut().resource_mut::<FocusTarget>().focus_block(ids[0]);
        press_fold_toggle(&mut app);
        assert!(folded(&app, ids[2]), "no geometry row on screen: the tail's newest folds");
        assert!(!folded(&app, ids[0]));
    }

    #[test]
    fn the_fold_toggle_does_nothing_without_a_foldable_block() {
        let (mut app, ids) = fold_toggle_app(&[BlockKind::Text]);
        press_fold_toggle(&mut app);
        assert!(!folded(&app, ids[0]));
    }

    /// j/k must move focus reading only `ConversationGeometry` — no per-block
    /// entities exist to query. `handle_navigate_blocks` used to also require
    /// a `BlockCellContainer` (for the now-deleted `FocusedBlockCell` marker
    /// bookkeeping); a hard requirement on it would have meant navigation
    /// silently doing nothing whenever that container was absent.
    #[test]
    fn block_navigation_works_without_the_legacy_cell_container() {
        use bevy::prelude::*;

        use crate::input::events::ActionFired;
        use crate::input::{Action, context::InputContext};
        use crate::view::geometry::{ConversationGeometry, EstimateParams, RowSeed};
        use crate::view::{EditorEntities, FocusTarget, MainCell};
        use kaijutsu_types::Role;

        let mut app = App::new();
        app.add_message::<ActionFired>();
        app.init_resource::<EditorEntities>();
        app.init_resource::<FocusTarget>();
        app.init_resource::<ConversationScrollState>();
        app.add_systems(Update, handle_navigate_blocks);

        let ctx = ContextId::new();
        let principal = PrincipalId::new();
        let ids: Vec<BlockId> = (1..=3).map(|seq| BlockId::new(ctx, principal, seq)).collect();
        let mut geom = ConversationGeometry::default();
        geom.reconcile(
            &ids,
            |_| {
                Some(RowSeed {
                    text_len: 10,
                    newline_count: 0,
                    role: Role::User,
                    kind: BlockKind::Text,
                    collapsed: false,
                    parent_id: None,
                })
            },
            &EstimateParams::default(),
            1,
        );
        geom.recompute_offsets();

        let main_ent = app.world_mut().spawn((geom, MainCell)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(main_ent);

        app.world_mut().write_message(ActionFired::new(
            Action::FocusFirstBlock,
            InputContext::Navigation,
        ));
        app.update();

        assert_eq!(
            app.world().resource::<FocusTarget>().block_id,
            Some(ids[0]),
            "j/k must reach FocusTarget without any block-cell entities",
        );
    }

    fn block(seq: u64, kind: BlockKind, status: Status, excluded: bool) -> kaijutsu_types::BlockSnapshot {
        let id = BlockId::new(ContextId::new(), PrincipalId::new(), seq);
        BlockSnapshotBuilder::new(id, kind)
            .status(status)
            .excluded(excluded)
            .build()
    }

    fn scroll_state(content_height: f32, visible_height: f32, offset: f32) -> ConversationScrollState {
        ConversationScrollState {
            offset,
            target_offset: offset,
            content_height,
            visible_height,
            following: false,
            new_blocks_added: false,
            pending_scroll_anchor: None,
        }
    }

    /// Sticky-follow regression (found by kaibo/qwen review, 2026-08-16):
    /// `scroll_to_rect_visible` (j/k block navigation revealing an
    /// off-screen block) used to re-latch follow off `is_at_bottom()`'s 50px
    /// band — the same band `scroll_by` was fixed to stop using. A reveal
    /// landing 40px from the true bottom must NOT re-arm follow; only
    /// landing at the true bottom (~1px, `reached_bottom`) may.
    #[test]
    fn scroll_to_rect_visible_does_not_relatch_follow_from_the_50px_band() {
        // content=1000, visible=400 -> max_offset=600. A block whose bottom
        // sits at 640 pulls the view down to target=640-400+20(margin)=260...
        // simpler: pick a block bottom that lands the clamped target at
        // max_offset - 40 (40px shy of the true bottom, inside the old
        // 50px band but outside reached_bottom's ~1px one).
        let mut state = scroll_state(1000.0, 400.0, 0.0);
        // view_bottom = 0 + 400 = 400; MARGIN = 20, so block_bottom must
        // exceed 380 to trigger the "reveal from below" branch. Choose a
        // block bottom of 940: target = 940 - 400 + 20 = 560, clamped to
        // max_offset (600) unchanged since 560 < 600. 600 - 560 = 40px shy.
        scroll_to_rect_visible(&mut state, /* y_offset */ 900.0, /* height */ 40.0);
        assert_eq!(state.target_offset, 560.0, "landed 40px shy of true bottom");
        assert!(
            !state.following,
            "landing merely inside the 50px band must not re-latch follow"
        );
    }

    /// The companion positive case: a reveal that lands AT the true bottom
    /// (clamped to max_offset) does re-latch follow, same as `scroll_by`.
    #[test]
    fn scroll_to_rect_visible_relatches_follow_at_the_true_bottom() {
        let mut state = scroll_state(1000.0, 400.0, 0.0);
        // block_bottom = 1000 (the very end of the content) -> target =
        // 1000 - 400 + 20 = 620, clamped to max_offset = 600 (true bottom).
        scroll_to_rect_visible(&mut state, /* y_offset */ 960.0, /* height */ 40.0);
        assert_eq!(state.target_offset, 600.0, "clamped to the true bottom");
        assert!(state.following, "landing at the true bottom re-latches follow");
    }

    /// The end-clamp rules j/k depend on: no wraparound at either edge,
    /// and an unfocused start enters the document at the near end for the
    /// direction pressed.
    #[test]
    fn next_focus_index_clamps_at_ends_and_enters_from_either_side() {
        use NavigationDirection::*;

        // Interior moves
        assert_eq!(next_focus_index(&Next, Some(0), 3), 1);
        assert_eq!(next_focus_index(&Previous, Some(2), 3), 1);

        // Clamp at the ends — j at the bottom / k at the top stay put
        assert_eq!(next_focus_index(&Next, Some(2), 3), 2);
        assert_eq!(next_focus_index(&Previous, Some(0), 3), 0);

        // Unfocused entry: Next from the top, Previous from the bottom
        assert_eq!(next_focus_index(&Next, None, 3), 0);
        assert_eq!(next_focus_index(&Previous, None, 3), 2);

        // First/Last ignore the current position
        assert_eq!(next_focus_index(&First, Some(2), 3), 0);
        assert_eq!(next_focus_index(&Last, Some(0), 3), 2);
        assert_eq!(next_focus_index(&First, None, 3), 0);
        assert_eq!(next_focus_index(&Last, None, 3), 2);

        // Single-block document: every direction lands on it
        assert_eq!(next_focus_index(&Next, Some(0), 1), 0);
        assert_eq!(next_focus_index(&Previous, Some(0), 1), 0);
        assert_eq!(next_focus_index(&Next, None, 1), 0);
        assert_eq!(next_focus_index(&Previous, None, 1), 0);
    }

    #[test]
    fn find_latest_error_block_picks_the_last_one_in_document_order() {
        let earlier = block(0, BlockKind::Error, Status::Error, false);
        let later = block(1, BlockKind::Error, Status::Error, false);
        let blocks = vec![earlier, later.clone()];
        assert_eq!(find_latest_error_block(&blocks), Some(later.id));
    }

    #[test]
    fn find_latest_error_block_skips_excluded_errors() {
        let excluded_error = block(0, BlockKind::Error, Status::Error, true);
        let live_error = block(1, BlockKind::Error, Status::Error, false);
        let blocks = vec![live_error.clone(), excluded_error];
        assert_eq!(find_latest_error_block(&blocks), Some(live_error.id));
    }

    #[test]
    fn find_latest_error_block_none_when_no_errors() {
        let blocks = vec![
            block(0, BlockKind::Text, Status::Done, false),
            block(1, BlockKind::ToolCall, Status::Running, false),
        ];
        assert_eq!(find_latest_error_block(&blocks), None);
    }

    #[test]
    fn find_latest_error_block_none_when_only_error_is_excluded() {
        let blocks = vec![block(0, BlockKind::Error, Status::Error, true)];
        assert_eq!(find_latest_error_block(&blocks), None);
    }

    #[test]
    fn find_latest_error_block_matches_status_regardless_of_kind() {
        // The instruction is "last Status::Error block", not "last
        // BlockKind::Error" — a ToolResult with no error child yet (still
        // mid-insert) should still be findable.
        let tool_result = block(0, BlockKind::ToolResult, Status::Error, false);
        let blocks = vec![tool_result.clone()];
        assert_eq!(find_latest_error_block(&blocks), Some(tool_result.id));
    }
}
