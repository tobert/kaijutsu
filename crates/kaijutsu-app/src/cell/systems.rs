//! Cell systems for input handling and rendering.

use bevy::prelude::*;
use bevy::ui::CalculatedClip;
use bevy::ui::measurement::ContentSize;

use super::components::{
    BlockEditCursor, BlockKind, BlockSnapshot, Cell, CellEditor, CellPosition,
    CellState, ComposeBlock, ConversationScrollState, DriftKind, EditingBlockCell,
    FocusTarget, MainCell, PromptSubmitted, RoleHeader, RoleHeaderLayout,
    ViewingConversation, WorkspaceLayout,
};
use crate::input::FocusArea;
use crate::conversation::CurrentConversation;
use crate::text::{
    bevy_to_cosmic_color, FontMetricsCache, MsdfBufferInfo, MsdfText, SharedFontSystem,
    MsdfTextAreaConfig, MsdfTextBuffer, TextMetrics,
    msdf::SdfTextEffects,
};
use crate::text::markdown::{self, MarkdownColors};
use crate::ui::format::format_for_display;
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;

// ============================================================================
// LAYOUT CONSTANTS
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
pub(crate) const INDENT_WIDTH: f32 = 24.0;

/// Vertical spacing between blocks.
const BLOCK_SPACING: f32 = 8.0;

/// Height reserved for role transition headers (e.g., "User", "Assistant").
const ROLE_HEADER_HEIGHT: f32 = 20.0;

/// Spacing between role header and block content.
const ROLE_HEADER_SPACING: f32 = 4.0;


// ============================================================================
// BLOCK COLOR MAPPING
// ============================================================================

/// Map a block to its semantic text color based on BlockKind and Role.
///
/// This enables visual distinction between different block types:
/// - User messages: soft white
/// - Assistant messages: light blue
/// - Thinking: dim gray (de-emphasized)
/// - Tool calls: amber
/// - Tool results: green (error: red)
/// - Shell: cyan for commands, gray for output
pub fn block_color(block: &BlockSnapshot, theme: &Theme) -> bevy::prelude::Color {
    use kaijutsu_crdt::Role;

    match block.kind {
        BlockKind::Text => {
            // Text blocks colored by role
            match block.role {
                Role::User => theme.block_user,
                Role::Model => theme.block_assistant,  // Model/AI responses
                Role::System => theme.fg_dim,          // System messages are dim
                Role::Tool => theme.block_tool_result, // Tool context
            }
        }
        BlockKind::Thinking => theme.block_thinking,
        BlockKind::ToolCall => {
            if block.status == kaijutsu_crdt::Status::Done {
                theme.fg  // Completed tool calls: plain foreground
            } else {
                theme.block_tool_call  // Running/pending: amber
            }
        }
        BlockKind::ToolResult => {
            if block.is_error {
                theme.block_tool_error
            } else {
                theme.block_tool_result
            }
        }
        BlockKind::ShellCommand => theme.block_shell_cmd,
        BlockKind::ShellOutput => theme.block_shell_output,
        BlockKind::Drift => match block.drift_kind {
            Some(DriftKind::Push) => theme.block_drift_push,
            Some(DriftKind::Pull) | Some(DriftKind::Distill) => theme.block_drift_pull,
            Some(DriftKind::Merge) => theme.block_drift_merge,
            Some(DriftKind::Commit) => theme.block_drift_commit,
            None => theme.fg_dim,
        },
    }
}

// Input handling moved to input/ module (focus-based dispatch).

/// Initialize MsdfTextBuffer for cells that don't have one yet.
pub fn init_cell_buffers(
    mut commands: Commands,
    cells_without_buffer: Query<(Entity, &CellEditor), (With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, editor) in cells_without_buffer.iter() {
        // Create a new buffer with DPI-aware metrics
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        buffer.set_snap_x(true); // monospace: snap to pixel grid
        buffer.set_letter_spacing(text_metrics.letter_spacing);

        // Initialize with current editor text
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(
            &mut font_system,
            &editor.text(),
            attrs,
            cosmic_text::Shaping::Advanced,
        );

        // Use try_insert to gracefully handle entity despawns between query and command application
        commands.entity(entity).try_insert((buffer, MsdfBufferInfo::default()));
        info!("Initialized MsdfTextBuffer for entity {:?}", entity);
    }
}

/// Format content blocks for display.
///
/// Delegates to `format_single_block` for each block, joining with blank lines.
fn format_blocks_for_display(blocks: &[BlockSnapshot]) -> String {
    if blocks.is_empty() {
        return String::new();
    }

    blocks
        .iter()
        .map(|b| format_single_block(b, None))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Strip provider prefix from model name for compact display.
///
/// `"anthropic/claude-sonnet-4-5"` → `"claude-sonnet-4-5"`
/// `"claude-opus-4-6"` → `"claude-opus-4-6"`
fn truncate_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Draw a box-drawing frame around content.
///
/// ```text
/// ┌─ header ──────────────────┐
/// │ content line 1             │
/// │ content line 2             │
/// └────────────────────────────┘
/// ```
#[allow(dead_code)] // Kept for non-GPU display paths (MCP text output, fallback)
fn draw_box(header: &str, content: &str, width: usize) -> String {
    let inner = width.saturating_sub(4); // account for "│ " and " │"
    let header_pad = inner.saturating_sub(header.chars().count() + 2);
    let mut out = String::new();

    // Top border
    out.push_str("┌─ ");
    out.push_str(header);
    out.push(' ');
    for _ in 0..header_pad {
        out.push('─');
    }
    out.push_str("┐\n");

    // Content lines
    for line in content.lines() {
        out.push_str("│ ");
        let line_chars: usize = line.chars().count();
        out.push_str(line);
        let pad = inner.saturating_sub(line_chars);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push_str(" │\n");
    }
    // Handle empty content
    if content.is_empty() {
        out.push_str("│ ");
        for _ in 0..inner {
            out.push(' ');
        }
        out.push_str(" │\n");
    }

    // Bottom border
    out.push('└');
    for _ in 0..inner + 2 {
        out.push('─');
    }
    out.push_str("┘\n");

    out
}

/// Format a drift block with variant-specific visual treatment.
///
/// `local_ctx`: if provided, determines push direction arrow (→ outgoing, ← incoming).
fn format_drift_block(block: &BlockSnapshot, local_ctx: Option<&str>) -> String {
    let ctx = block.source_context.as_deref().unwrap_or("?");
    let model = block.source_model.as_deref().map(truncate_model).unwrap_or("unknown");
    let ctx_label = format!("@{}", ctx);

    // Determine direction arrow: → if we sent it, ← if we received it
    let arrow = match local_ctx {
        Some(local) if ctx == local => "\u{2192}",
        _ => "\u{2190}",
    };

    match block.drift_kind {
        Some(DriftKind::Push) => {
            let preview = block.content.lines().next().unwrap_or("");
            format!("{} {} ({})  {}", arrow, ctx_label, model, preview)
        }
        Some(DriftKind::Pull) | Some(DriftKind::Distill) => {
            // Shader border handles visual framing; emit plain header + content
            format!("pulled from {} ({})\n{}", ctx_label, model, block.content)
        }
        Some(DriftKind::Merge) => {
            format!("\u{21c4} merged from {} ({})\n{}", ctx_label, model, block.content)
        }
        Some(DriftKind::Commit) => {
            format!("# {}  {}", ctx_label, block.content.lines().next().unwrap_or(""))
        }
        None => {
            format!("~ {} ({})  {}", ctx_label, model, block.content.lines().next().unwrap_or(""))
        }
    }
}

/// Update MsdfTextBuffer from CellEditor when dirty.
///
/// For cells with content blocks, formats them with visual markers.
/// For plain text cells, uses the text directly.
pub fn sync_cell_buffers(
    mut cells: Query<(&CellEditor, &mut MsdfTextBuffer, Option<&BlockCellContainer>), Changed<CellEditor>>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (editor, mut buffer, container) in cells.iter_mut() {
        // Skip cells that have BlockCells — they render per-block via sync_block_cell_buffers.
        // Clear the MainCell buffer so it doesn't render an overlapping text wall.
        if container.is_some_and(|c| !c.block_cells.is_empty()) {
            buffer.set_text(
                &mut font_system,
                "",
                cosmic_text::Attrs::new(),
                cosmic_text::Shaping::Advanced,
            );
            continue;
        }

        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));

        // Use block-formatted text if we have blocks, otherwise use raw text
        let display_text = if editor.has_blocks() {
            format_blocks_for_display(&editor.blocks())
        } else {
            editor.text()
        };

        buffer.set_text(
            &mut font_system,
            &display_text,
            attrs,
            cosmic_text::Shaping::Advanced,
        );
    }
}

/// Compute cell heights based on content.
///
/// For cells with blocks, counts lines from the formatted display text.
/// Collapsed thinking blocks contribute minimal lines.
///
/// For MainCell, also updates ConversationScrollState with content height.
pub fn compute_cell_heights(
    mut cells: Query<(&CellEditor, &mut CellState, Option<&MainCell>), Changed<CellEditor>>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    for (editor, mut state, main_cell) in cells.iter_mut() {
        let display_text = if editor.has_blocks() {
            format_blocks_for_display(&editor.blocks())
        } else {
            editor.text()
        };
        let line_count = display_text.lines().count().max(1);

        // For MainCell, don't cap height - we need full content height for scrolling
        let content_height = if main_cell.is_some() {
            // Full height without max cap, tight padding
            // TODO(dedup): inline height formula duplicates WorkspaceLayout::height_for_lines
            (line_count as f32) * layout.line_height + 4.0
        } else {
            // Other cells use capped height
            layout.height_for_lines(line_count)
        };

        state.computed_height = content_height;

        // For MainCell, update scroll state with content height
        if main_cell.is_some() {
            scroll_state.content_height = content_height;
        }
    }
}

/// Visual indication for focused cell.
pub fn highlight_focused_cell(
    focus: Res<FocusTarget>,
    mut cells: Query<(Entity, &mut MsdfTextAreaConfig), With<Cell>>,
    theme: Option<Res<crate::ui::theme::Theme>>,
) {
    let Some(ref theme) = theme else {
        warn_once!("Theme resource unavailable for cell highlighting");
        return;
    };

    for (entity, mut config) in cells.iter_mut() {
        let color = if Some(entity) == focus.entity {
            theme.accent // Brighter for focused
        } else {
            theme.fg_dim
        };
        config.default_color = color;
    }
}

/// Click to focus a cell.
///
/// FUTURE: This system will evolve to support threaded conversations:
/// - Clicking a BlockCell could focus it for reply (input follows focus)
/// - Input area might "attach" to the focused block or move on-screen
/// - Consider: FocusTarget.entity vs ActiveThread vs ReplyTarget as separate concepts
///
/// For now: Any cell with MsdfTextAreaConfig can receive focus.
/// The cursor renders at the focused cell's position.
pub fn click_to_focus(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cells: Query<(Entity, &MsdfTextAreaConfig), With<Cell>>,
    mut focus: ResMut<FocusTarget>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }

    // Get cursor position
    let Some(cursor_pos) = windows.iter().next().and_then(|w| w.cursor_position()) else {
        return;
    };

    // Check if cursor is inside any cell bounds
    for (entity, config) in cells.iter() {
        let bounds = &config.bounds;
        if cursor_pos.x >= bounds.left as f32
            && cursor_pos.x <= bounds.right as f32
            && cursor_pos.y >= bounds.top as f32
            && cursor_pos.y <= bounds.bottom as f32
        {
            focus.entity = Some(entity);
            return;
        }
    }

    // Clicked outside any cell - clear entity focus
    focus.entity = None;
}

/// Toggle collapse state of focused cell or thinking blocks (Tab in Normal mode).
///
/// Behavior:
/// - If the cell has thinking blocks, Tab toggles the first thinking block's collapse state
/// - Otherwise, Tab toggles the whole cell's collapse state
// handle_collapse_toggle — DELETED (Phase 5)
// Migrated to input::systems::handle_collapse_toggle

use bevy::math::Vec4;

// ============================================================================
// CURSOR SYSTEMS (Shader Cursor)
// ============================================================================

use crate::shaders::{CursorBeamMaterial, CursorMode};

/// Marker component for the cursor UI entity.
#[derive(Component)]
pub struct CursorMarker;

/// Consolidated resource tracking editor-related singleton entities.
///
/// Replaces the previous separate resources:
/// - `CursorEntity` → `entities.cursor`
/// - `MainCellEntity` → `entities.main_cell`
/// - `ExpandedBlockEntity` → `entities.expanded_view`
#[derive(Resource, Default)]
pub struct EditorEntities {
    /// The cursor UI entity (shader-based).
    pub cursor: Option<Entity>,
    /// The main conversation cell entity.
    pub main_cell: Option<Entity>,
    /// The expanded block overlay view entity.
    pub expanded_view: Option<Entity>,
    /// The ConversationContainer entity (flex parent for BlockCells).
    pub conversation_container: Option<Entity>,
}

// Font metrics are now read from TextMetrics resource instead of hardcoded constants.
// This ensures cursor positioning matches actual text rendering.
// Monospace char width is approximately 0.6x the font size.
const MONOSPACE_WIDTH_RATIO: f32 = 0.6;

/// Spawn the cursor entity if it doesn't exist.
pub fn spawn_cursor(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<crate::ui::theme::Theme>,
    text_metrics: Res<TextMetrics>,
) {
    if entities.cursor.is_some() {
        return;
    }

    // Use theme cursor color (defaults to cursor_normal)
    let color = theme.cursor_normal;

    let material = cursor_materials.add(CursorBeamMaterial {
        color,
        // params: x=orb_size, y=intensity, z=wander_speed, w=blink_rate
        params: Vec4::new(0.25, 1.2, 2.0, 0.0),
        time: Vec4::new(0.0, CursorMode::Block as u8 as f32, 0.0, 0.0),
    });

    // Derive cursor size from TextMetrics for consistency with text rendering
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;

    let entity = commands
        .spawn((
            CursorMarker,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(char_width + 8.0),  // Slightly larger for bloom
                height: Val::Px(line_height + 4.0),
                ..default()
            },
            BackgroundColor(Color::NONE),  // Explicit transparent - let shader handle all rendering
            MaterialNode(material),
            ZIndex(crate::constants::ZLayer::CURSOR),
            Visibility::Hidden, // Start hidden until we have a focused cell
        ))
        .id();

    entities.cursor = Some(entity);
    info!("Spawned cursor entity");
}

/// Update cursor position and visibility based on focused cell and focus area.
pub fn update_cursor(
    focus: Res<FocusTarget>,
    focus_area: Res<FocusArea>,
    entities: Res<EditorEntities>,
    mut cells: Query<(&mut CellEditor, &MsdfTextAreaConfig)>,
    mut cursor_query: Query<(&mut Node, &mut Visibility, &MaterialNode<CursorBeamMaterial>), With<CursorMarker>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<crate::ui::theme::Theme>,
    text_metrics: Res<TextMetrics>,
) {
    let Some(cursor_ent) = entities.cursor else {
        return;
    };

    let Ok((mut node, mut visibility, material_node)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    // Hide cursor if no cell is focused
    let Some(focused_entity) = focus.entity else {
        *visibility = Visibility::Hidden;
        return;
    };

    let Ok((mut editor, config)) = cells.get_mut(focused_entity) else {
        *visibility = Visibility::Hidden;
        return;
    };

    // Show cursor
    *visibility = Visibility::Inherited;

    // Calculate cursor position (uses cache to avoid O(N) scan every frame)
    let (row, col) = cursor_position(&mut editor);

    // Position relative to cell bounds using TextMetrics for consistency
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;
    let x = config.left + (col as f32 * char_width);
    let y = config.top + (row as f32 * line_height);

    node.left = Val::Px(x - 2.0); // Slight offset for beam alignment
    node.top = Val::Px(y);

    // Update cursor mode and wandering orb params based on focus area
    if let Some(material) = cursor_materials.get_mut(&material_node.0) {
        let (cursor_mode, color, params) = if focus_area.is_text_input() {
            // Text input (compose or block editing): beam cursor
            (CursorMode::Beam, theme.cursor_insert, Vec4::new(0.25, 1.2, 2.0, 0.0))
        } else {
            // Navigation: block cursor
            (CursorMode::Block, theme.cursor_normal, Vec4::new(0.2, 1.0, 1.5, 0.6))
        };
        material.time.y = cursor_mode as u8 as f32;
        material.color = color;
        material.params = params;
    }
}

/// Calculate cursor row and column by walking blocks directly.
///
/// Uses a cache to avoid O(N) string scans every frame. The cache is invalidated
/// when the document version changes (on any edit operation).
fn cursor_position(editor: &mut CellEditor) -> (usize, usize) {
    let current_version = editor.version();

    // Return cached position if still valid
    if editor.cursor_cache.version == current_version {
        return (editor.cursor_cache.row, editor.cursor_cache.col);
    }

    // Compute position and cache it
    let (row, col) = compute_cursor_position(editor);
    editor.cursor_cache.row = row;
    editor.cursor_cache.col = col;
    editor.cursor_cache.version = current_version;

    (row, col)
}

/// Internal: compute cursor position by walking blocks (O(N) string scan).
fn compute_cursor_position(editor: &CellEditor) -> (usize, usize) {
    let Some(ref cursor_block_id) = editor.cursor.block_id else {
        return (0, 0);
    };

    let blocks = editor.doc.blocks_ordered();
    let mut row = 0;

    for (i, block) in blocks.iter().enumerate() {
        let text = &block.content;

        if &block.id == cursor_block_id {
            // Found the cursor's block - count rows within it and compute col
            let offset = editor.cursor.offset.min(text.len());
            let before_cursor = &text[..offset];
            row += before_cursor.matches('\n').count();
            let col = before_cursor
                .rfind('\n')
                .map(|pos| offset - pos - 1)
                .unwrap_or(offset);
            return (row, col);
        }

        // Count rows in this block
        row += text.matches('\n').count();

        // Add block separator (2 newlines between blocks)
        if i < blocks.len() - 1 {
            row += 2;
        }
    }

    // Cursor block not found - return end position
    (row, 0)
}

// ============================================================================
// PROMPT SUBMISSION (from ComposeBlock)
// ============================================================================

/// Send submitted prompts to the server.
///
/// Routes to either:
/// - LLM prompt (Chat mode): Adds user message + streams LLM response
/// - Shell execute (Shell mode): Executes kaish command + streams output
///
/// The server handles:
/// 1. Adding the appropriate block(s) to the conversation
/// 2. Streaming the response as blocks
/// 3. Broadcasting block changes to all connected clients
///
/// Note: We don't add messages locally - the server adds them and broadcasts
/// back to us. This avoids duplicate messages.
/// Detect whether submitted text is a shell command based on prefix.
///
/// Text starting with `:` or `` ` `` is routed to kaish (shell execution).
/// Everything else is routed to the LLM (chat prompt).
fn is_shell_command(text: &str) -> bool {
    text.starts_with(':') || text.starts_with('`')
}

/// Strip the shell prefix from a command before execution.
fn strip_shell_prefix(text: &str) -> &str {
    if text.starts_with(':') || text.starts_with('`') {
        &text[1..]
    } else {
        text
    }
}

pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    current_conv: Res<CurrentConversation>,
    sync_state: Res<super::components::DocumentSyncState>,
    mut scroll_state: ResMut<ConversationScrollState>,
    actor: Option<Res<crate::connection::RpcActor>>,
    channel: Res<crate::connection::RpcResultChannel>,
) {
    // Early exit: don't do any work unless there are actually events to process
    if submit_events.is_empty() {
        return;
    }

    // Get the current conversation ID
    let Some(conv_id) = current_conv.id() else {
        warn!("No current conversation to add message to");
        return;
    };

    // Get the document cell_id from sync state
    // This ensures we use the same ID the server uses for BlockInserted events
    let Some(ref doc) = sync_state.doc else {
        warn!("No document in sync state");
        return;
    };
    let doc_cell_id = doc.document_id().to_string();

    for event in submit_events.read() {
        if let Some(ref actor) = actor {
            let handle = actor.handle.clone();
            let cell_id = doc_cell_id.clone();
            let conv = conv_id.to_string();
            let tx = channel.sender();

            // Auto-detect shell vs chat from text prefix
            if is_shell_command(&event.text) {
                let text = strip_shell_prefix(&event.text).to_string();
                // Shell: fire-and-forget, results via ServerEvent broadcast
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        if let Err(e) = handle.shell_execute(&text, &cell_id).await {
                            log::error!("shell_execute failed: {e}");
                            let _ = tx.send(crate::connection::RpcResultMessage::RpcError {
                                operation: "shell_execute".into(),
                                error: e.to_string(),
                            });
                        }
                    })
                    .detach();
                info!("Sent shell command to server (conv={}, cell_id={})", conv, doc_cell_id);
            } else {
                let text = event.text.clone();
                // Chat: fire-and-forget, results via ServerEvent broadcast
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        if let Err(e) = handle.prompt(&text, None, &cell_id).await {
                            log::error!("prompt failed: {e}");
                            let _ = tx.send(crate::connection::RpcResultMessage::RpcError {
                                operation: "prompt".into(),
                                error: e.to_string(),
                            });
                        }
                    })
                    .detach();
                info!("Sent prompt to server (conv={}, cell_id={})", conv, doc_cell_id);
            }
            // Enable follow mode to smoothly track streaming response
            scroll_state.start_following();
        } else {
            warn!("No connection available, prompt not sent to server");
        }
    }
}

/// Smooth scroll interpolation system.
///
/// Runs every frame to smoothly interpolate scroll position toward target.
///
/// Key insight: In follow mode, we lock directly to bottom (no interpolation).
/// Interpolation is only used for manual scrolling (wheel, Page Up/Down, etc.).
/// This prevents the "chasing a moving target" stutter during streaming.
pub fn smooth_scroll(
    mut scroll_state: ResMut<ConversationScrollState>,
    time: Res<Time>,
    entities: Res<EditorEntities>,
    mut scroll_positions: Query<(&mut ScrollPosition, &ComputedNode), With<super::components::ConversationContainer>>,
) {
    // Clear the user_scrolled flag at end of frame
    // (This system runs late in the frame after block events)
    scroll_state.user_scrolled_this_frame = false;

    // Clamp target in case content shrank (context switch, block collapse)
    scroll_state.clamp_target();

    let max = scroll_state.max_offset();

    // In follow mode, lock directly to bottom — no interpolation needed.
    // This is how terminals work: content grows, viewport stays anchored.
    if scroll_state.following {
        // Jitter prevention: only update if max changed by at least 1 pixel
        if (max - scroll_state.offset).abs() >= 1.0 {
            scroll_state.offset = max;
            scroll_state.target_offset = max;
        }
    } else {
        // Manual scroll: lerp toward target for smooth motion
        const SCROLL_SPEED: f32 = 12.0;
        let target = scroll_state.target_offset;
        let offset = scroll_state.offset;
        let t = (time.delta_secs() * SCROLL_SPEED).min(1.0);
        let new_offset = offset + (target - offset) * t;

        // Snap when close enough to prevent sub-pixel jitter
        scroll_state.offset = if (new_offset - target).abs() < 0.5 {
            target
        } else {
            new_offset
        };
    }

    // Write scroll offset to Bevy's ScrollPosition and read visible/content heights
    if let Some(conv) = entities.conversation_container {
        if let Ok((mut scroll_pos, computed)) = scroll_positions.get_mut(conv) {
            **scroll_pos = Vec2::new(scroll_pos.x, scroll_state.offset);
            // Use content box height (inside border+padding) as the visible viewport
            let content_box = computed.content_box();
            let visible_h = content_box.height();
            if visible_h > 0.0 {
                scroll_state.visible_height = visible_h;
            }
            // Use Bevy's computed content size as authoritative scroll extent
            let bevy_content_h = computed.content_size().y;
            if bevy_content_h > 0.0 {
                scroll_state.content_height = bevy_content_h;
            }
        }
    }
}

// handle_scroll_input — DELETED (Phase 5)
// Mouse wheel handled by input::dispatch, Ctrl+D/U by input::systems::handle_scroll

// navigate_blocks + NavigationDirection + scroll_to_block_visible — DELETED (Phase 5)
// Migrated to input::systems::handle_navigate_blocks

use super::components::FocusedBlockCell;

/// Highlight the focused block cell with a visual indicator.
///
/// Applies a background tint or border highlight to the BlockCell
/// that has the FocusedBlockCell marker.
pub fn highlight_focused_block(
    mut focused_configs: Query<(&BlockCell, &mut MsdfTextAreaConfig), With<FocusedBlockCell>>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    theme: Res<Theme>,
) {
    // Early return if no focused blocks - skip HashMap allocation entirely
    if focused_configs.is_empty() {
        return;
    }

    // For now, we indicate focus by slightly brightening the text color
    // Future: could add a background highlight or border via a separate UI element

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    // Build block lookup
    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();

    // Focused blocks get a slightly brighter color
    for (block_cell, mut config) in focused_configs.iter_mut() {
        if let Some(block) = blocks.get(&block_cell.block_id) {
            let base_color = block_color(block, &theme);
            // Brighten the color slightly to indicate focus
            let srgba = base_color.to_srgba();
            let focused_color = Color::srgba(
                (srgba.red * 1.15).min(1.0),
                (srgba.green * 1.15).min(1.0),
                (srgba.blue * 1.15).min(1.0),
                srgba.alpha,
            );
            config.default_color = focused_color;
        }
    }

    // Unfocused blocks return to base color (handled by sync_block_cell_buffers)
    // Note: This system runs after sync_block_cell_buffers so the focused block
    // override takes precedence.
}

// handle_expand_block + handle_view_pop — DELETED (Phase 5)
// Migrated to input::systems::handle_expand_block + handle_view_pop

// ============================================================================
// EXPANDED BLOCK VIEW (Phase 4)
// ============================================================================

use crate::ui::state::{ExpandedBlockView, ViewStack};

/// Spawn the ExpandedBlockView when ViewStack enters ExpandedBlock state.
pub fn spawn_expanded_block_view(
    mut commands: Commands,
    view_stack: Res<ViewStack>,
    mut entities: ResMut<EditorEntities>,
    existing_views: Query<Entity, With<ExpandedBlockView>>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
) {
    // Check if we need to spawn or despawn
    let should_show = view_stack.has_expanded_block();

    if should_show && entities.expanded_view.is_none() {
        // Spawn the expanded block view
        let Some(block_id) = view_stack.expanded_block_id() else {
            return;
        };

        // Get the block content from MainCell
        let Some(main_ent) = entities.main_cell else {
            return;
        };
        let Ok(editor) = main_cells.get(main_ent) else {
            return;
        };

        let blocks = editor.blocks();
        let Some(block) = blocks.iter().find(|b| &b.id == block_id) else {
            warn!("Expanded block not found: {:?}", block_id);
            return;
        };

        // Create the text buffer
        let mut fs = font_system.0.lock().unwrap();
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = MsdfTextBuffer::new(&mut fs, metrics);
        buffer.set_snap_x(true); // monospace: snap to pixel grid
        buffer.set_letter_spacing(text_metrics.letter_spacing);

        let color = block_color(block, &theme);
        buffer.set_color(color);
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut fs, &block.content, attrs, cosmic_text::Shaping::Advanced);
        drop(fs);

        // Spawn the view entity
        let entity = commands
            .spawn((
                ExpandedBlockView,
                MsdfText,
                buffer,
                MsdfBufferInfo::default(),
                MsdfTextAreaConfig {
                    left: 40.0,
                    top: 60.0,  // Leave room for header
                    scale: 1.0,
                    bounds: crate::text::TextBounds {
                        left: 0,
                        top: 0,
                        right: 1200,
                        bottom: 800,
                    },
                    default_color: color,
                },
                Visibility::Inherited,
                // Store block info for updates
                ExpandedBlockInfo {
                    block_id: block_id.clone(),
                    block_kind: block.kind,
                },
            ))
            .id();

        entities.expanded_view = Some(entity);
        info!("Spawned ExpandedBlockView for {:?}", block_id);
    } else if !should_show && entities.expanded_view.is_some() {
        // Despawn when leaving ExpandedBlock view
        for entity in existing_views.iter() {
            if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
        }
        entities.expanded_view = None;
        info!("Despawned ExpandedBlockView");
    }
}

/// Stores info about the currently expanded block.
#[derive(Component)]
pub struct ExpandedBlockInfo {
    pub block_id: kaijutsu_crdt::BlockId,
    #[allow(dead_code)]
    pub block_kind: BlockKind,
}

/// Update the ExpandedBlockView content when the block changes.
pub fn sync_expanded_block_content(
    view_stack: Res<ViewStack>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut expanded_views: Query<
        (&ExpandedBlockInfo, &mut MsdfTextBuffer, &mut MsdfTextAreaConfig),
        With<ExpandedBlockView>,
    >,
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    windows: Query<&Window>,
) {
    if !view_stack.has_expanded_block() {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    // Get window size for bounds
    let (width, height) = windows
        .iter()
        .next()
        .map(|w| (w.width(), w.height()))
        .unwrap_or((1280.0, 800.0));

    let blocks = editor.blocks();

    for (info, mut buffer, mut config) in expanded_views.iter_mut() {
        let Some(block) = blocks.iter().find(|b| b.id == info.block_id) else {
            continue;
        };

        // Update text if changed
        let mut fs = font_system.0.lock().unwrap();
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut fs, &block.content, attrs, cosmic_text::Shaping::Advanced);
        drop(fs);

        // Update color
        let color = block_color(block, &theme);
        buffer.set_color(color);
        config.default_color = color;

        // Update bounds to fill screen (with padding)
        config.bounds = crate::text::TextBounds {
            left: 40,
            top: 60,
            right: (width - 40.0) as i32,
            bottom: (height - 40.0) as i32,
        };
    }
}

// ============================================================================
// MAIN CELL SYSTEMS
// ============================================================================

/// Spawn the main kernel cell on startup.
///
/// This is the primary workspace cell that displays kernel output, shell interactions,
/// and agent conversations. It fills the space between the header and prompt.
pub fn spawn_main_cell(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    conversation_container: Query<Entity, Added<super::components::ConversationContainer>>,
) {
    // Only spawn once when we see the ConversationContainer
    if entities.main_cell.is_some() {
        return;
    }

    // Wait for conversation container to exist
    let Ok(conv_entity) = conversation_container.single() else {
        return;
    };

    entities.conversation_container = Some(conv_entity);

    // Create the main kernel cell
    let cell = Cell::new();
    let cell_id = cell.id.clone();

    // Initial welcome message
    let welcome_text = "Welcome to 会術 Kaijutsu\n\nPress 'i' to start typing...";

    // NOTE: MainCell does NOT get MsdfText/MsdfTextAreaConfig.
    // The BlockCell system handles per-block rendering.
    // MainCell only holds the CellEditor (source of truth for content).
    let entity = commands
        .spawn((
            cell,
            CellEditor::default().with_text(welcome_text),
            CellState {
                computed_height: 400.0, // Will be updated by layout
                collapsed: false,
            },
            CellPosition::new(0),
            MainCell,
        ))
        .id();

    entities.main_cell = Some(entity);
    info!("Spawned main kernel cell with id {:?}", cell_id.0);
}

/// Track the focused ConversationContainer and re-parent block cells when it changes.
///
/// After a pane split, the reconciler despawns and rebuilds all PaneMarker entities.
/// This orphans block cells from the old container. This system detects when the
/// focused ConversationContainer changes (new entity with PaneFocus) and:
/// 1. Updates `EditorEntities.conversation_container`
/// 2. Re-parents existing block cells + role headers to the new container
pub fn track_conversation_container(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    focused_containers: Query<Entity, (With<super::components::ConversationContainer>, With<crate::ui::tiling::PaneFocus>)>,
    containers: Query<&BlockCellContainer>,
) {
    let Ok(focused) = focused_containers.single() else {
        return;
    };

    if entities.conversation_container == Some(focused) {
        return;
    }

    let old = entities.conversation_container;
    entities.conversation_container = Some(focused);

    // Re-parent block cells and role headers from old container to new one
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    info!(
        "Conversation container changed: {:?} -> {:?}, re-parenting {} block cells + {} role headers",
        old, focused, container.block_cells.len(), container.role_headers.len()
    );

    for &entity in &container.block_cells {
        if let Ok(mut ec) = commands.get_entity(entity) {
            ec.set_parent_in_place(focused);
        }
    }
    for &entity in &container.role_headers {
        if let Ok(mut ec) = commands.get_entity(entity) {
            ec.set_parent_in_place(focused);
        }
    }
}

/// Sync the MainCell's content with DocumentSyncState.
///
/// This system:
/// 1. Checks if there's a current conversation
/// 2. Checks if the sync state's version has changed
/// 3. If changed, rebuilds the MainCell's BlockDocument from sync state
///
/// DocumentSyncState owns the authoritative BlockDocument. This system
/// copies that document to the MainCell's CellEditor for rendering.
pub fn sync_main_cell_to_conversation(
    current_conv: Res<CurrentConversation>,
    sync_state: Res<super::components::DocumentSyncState>,
    entities: Res<EditorEntities>,
    mut main_cell: Query<(&mut CellEditor, Option<&mut ViewingConversation>), With<MainCell>>,
    mut commands: Commands,
) {
    // Need both a current conversation and sync state document
    let Some(conv_id) = current_conv.id() else {
        debug!("sync_main_cell: no current conversation");
        return;
    };
    let Some(entity) = entities.main_cell else {
        trace!("sync_main_cell: no main cell entity");
        return;
    };
    let Some(ref source_doc) = sync_state.doc else {
        trace!("sync_main_cell: no document in sync state");
        return;
    };

    // Verify document ID matches
    if source_doc.document_id() != conv_id {
        debug!("sync_main_cell: document ID mismatch ({} vs {})", source_doc.document_id(), conv_id);
        return;
    }

    // Get the main cell's editor and viewing component
    let Ok((mut editor, viewing_opt)) = main_cell.get_mut(entity) else {
        debug!("sync_main_cell: couldn't get main cell editor");
        return;
    };

    // Check if we need to sync (version changed)
    let sync_version = sync_state.version();
    let needs_sync = match viewing_opt {
        Some(ref viewing) => {
            viewing.conversation_id != conv_id || viewing.last_sync_version != sync_version
        }
        None => true,
    };

    if !needs_sync {
        return;
    }

    // Copy from authoritative source
    let agent_id = editor.doc.agent_id().to_string();
    let snapshot = source_doc.snapshot();
    let new_doc = kaijutsu_crdt::BlockDocument::from_snapshot(snapshot, &agent_id);
    editor.doc = new_doc;

    // Update cursor to end of document
    if let Some(last_block) = editor.blocks().last() {
        let len = last_block.content.len();
        editor.cursor = super::components::BlockCursor::at(last_block.id.clone(), len);
    }

    // Update or insert the ViewingConversation component
    match viewing_opt {
        Some(mut viewing) => {
            viewing.conversation_id = conv_id.to_string();
            viewing.last_sync_version = sync_version;
        }
        None => {
            commands.entity(entity).insert(ViewingConversation {
                conversation_id: conv_id.to_string(),
                last_sync_version: sync_version,
            });
        }
    }

    trace!(
        "Synced MainCell to conversation {} (version {})",
        conv_id, sync_version
    );
}

// ============================================================================
// BLOCK EVENT HANDLING (Server → Client Block Sync)
// ============================================================================

use crate::connection::{RpcResultMessage, ServerEventMessage};

/// Handle block events from the server, routing through DocumentCache.
///
/// This system processes:
/// - `ServerEventMessage` — streamed block events (inserted, edited, deleted, etc.)
/// - `RpcResultMessage::ContextJoined` — initial document state after joining a context
///
/// **Multi-context routing:**
/// All events are routed by `document_id` to the appropriate `CachedDocument` in
/// `DocumentCache`. For the active document, changes are also mirrored to
/// `DocumentSyncState` for backward compatibility with `sync_main_cell_to_conversation`.
///
/// **Sync protocol (per document):**
/// - Initial state (ContextJoined) → full sync via `from_oplog()`
/// - Subsequent BlockInserted → incremental merge via `merge_ops_owned()`
/// - BlockTextOps → always incremental merge
///
/// Implements terminal-like auto-scroll: if the user is at the bottom when
/// new content arrives, we stay at the bottom.
pub fn handle_block_events(
    mut server_events: MessageReader<ServerEventMessage>,
    mut result_events: MessageReader<RpcResultMessage>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut sync_state: ResMut<super::components::DocumentSyncState>,
    mut doc_cache: ResMut<super::components::DocumentCache>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut current_conv: ResMut<CurrentConversation>,
    mut pending_switch: ResMut<super::components::PendingContextSwitch>,
    mut switch_writer: MessageWriter<super::components::ContextSwitchRequested>,
    mut sync_gen: ResMut<crate::connection::actor_plugin::SyncGeneration>,
) {
    use kaijutsu_client::ServerEvent;
    use super::components::CachedDocument;

    // Check if we're at the bottom before processing events (for auto-scroll)
    let was_at_bottom = scroll_state.is_at_bottom();

    // Get agent ID for creating documents
    let agent_id = format!("user:{}", whoami::username());

    // Handle initial document state from ContextJoined
    for result in result_events.read() {
        if let RpcResultMessage::ContextJoined { membership, document_id, initial_state } = result {
            let context_name = membership.context_name.clone();

            // Create or update cache entry
            if !doc_cache.contains(document_id) {
                let mut synced = kaijutsu_client::SyncedDocument::new(document_id, &agent_id);

                // Apply initial state if provided
                if let Some(state) = initial_state {
                    match synced.apply_document_state(state) {
                        Ok(effect) => {
                            info!("Cache: initial sync for '{}' ({}) effect: {:?}", context_name, document_id, effect);
                        }
                        Err(e) => {
                            error!("Cache: initial sync error for '{}' ({}): {}", context_name, document_id, e);
                        }
                    }
                }

                let cached = CachedDocument {
                    synced,
                    context_name: context_name.clone(),
                    synced_at_generation: sync_gen.0,
                    last_accessed: std::time::Instant::now(),
                    scroll_offset: 0.0,
                };
                doc_cache.insert(document_id.clone(), cached);
            } else if let Some(state) = initial_state {
                // Reconnect case: cache entry exists but server has authoritative state
                if let Some(cached) = doc_cache.get_mut(document_id) {
                    match cached.synced.apply_document_state(state) {
                        Ok(effect) => {
                            info!("Cache: reconnect refresh for '{}' ({}) effect: {:?}", context_name, document_id, effect);
                            cached.synced_at_generation = sync_gen.0;
                        }
                        Err(e) => {
                            error!("Cache: reconnect refresh error for '{}' ({}): {}", context_name, document_id, e);
                        }
                    }
                }
            }

            // If this is the first context or no active doc yet, make it active
            let is_first = doc_cache.active_id().is_none();
            if is_first {
                doc_cache.set_active(document_id);
            }

            // Check if this ContextJoined satisfies a pending context switch
            if let Some(ref pending_ctx) = pending_switch.0 {
                if pending_ctx == &context_name {
                    info!("Pending context switch satisfied: '{}' joined, auto-switching", context_name);
                    pending_switch.0 = None;
                    switch_writer.write(super::components::ContextSwitchRequested {
                        context_name: context_name.clone(),
                    });
                }
            }

            // Mirror to DocumentSyncState for the active document
            let is_active = doc_cache.active_id() == Some(document_id.as_str());
            if is_active {
                if current_conv.id() != Some(document_id) {
                    info!(
                        "Updating current_conv to server's document_id: {} (was {:?})",
                        document_id,
                        current_conv.id()
                    );
                    current_conv.0 = Some(document_id.clone());
                }

                if let Some(state) = initial_state {
                    match sync_state.apply_initial_state(&state.document_id, &agent_id, &state.ops) {
                        Ok(result) => {
                            info!("Initial state sync result: {:?}", result);
                        }
                        Err(e) => {
                            error!("Initial state sync error: {}", e);
                        }
                    }
                }
            }
        }
    }

    let active_doc_id = doc_cache.active_id().map(|s| s.to_string());

    // Handle streamed block events — route by document_id through DocumentCache
    for ServerEventMessage(event) in server_events.read() {
        // Extract document_id from event for routing
        let event_doc_id = match event {
            ServerEvent::BlockInserted { document_id, .. }
            | ServerEvent::BlockTextOps { document_id, .. }
            | ServerEvent::BlockStatusChanged { document_id, .. }
            | ServerEvent::BlockDeleted { document_id, .. }
            | ServerEvent::BlockCollapsedChanged { document_id, .. }
            | ServerEvent::BlockMoved { document_id, .. }
            | ServerEvent::SyncReset { document_id, .. } => Some(document_id.as_str()),
            _ => None,
        };

        // Route to cache entry via SyncedDocument.apply_event
        if let Some(doc_id) = event_doc_id {
            if let Some(cached) = doc_cache.get_mut(doc_id) {
                let effect = cached.synced.apply_event(event);
                match &effect {
                    kaijutsu_client::SyncEffect::Updated { .. }
                    | kaijutsu_client::SyncEffect::FullSync { .. } => {
                        cached.synced_at_generation = sync_gen.0;
                    }
                    kaijutsu_client::SyncEffect::NeedsResync => {
                        cached.synced_at_generation = 0;
                        sync_gen.0 = sync_gen.0.wrapping_add(1);
                    }
                    kaijutsu_client::SyncEffect::Ignored => {}
                }
                trace!("Cache: event for {}: {:?}", doc_id, effect);
            }
        }

        // Mirror to DocumentSyncState if active (legacy path — will be removed
        // when DocumentSyncState is fully replaced by DocumentCache)
        match event {
            ServerEvent::BlockInserted { document_id, block, ops } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    match sync_state.apply_block_inserted(document_id, &agent_id, block, ops) {
                        Ok(result) => trace!("Block insert sync: {:?}", result),
                        Err(e) => trace!("Block insert sync error: {}", e),
                    }
                }
            }
            ServerEvent::BlockTextOps { document_id, ops, .. } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    match sync_state.apply_text_ops(document_id, &agent_id, ops) {
                        Ok(result) => trace!("Text ops sync: {:?}", result),
                        Err(e) => trace!("Text ops sync error: {}", e),
                    }
                }
            }
            ServerEvent::BlockStatusChanged { document_id, block_id, status } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    if let Some(ref mut doc) = sync_state.doc {
                        if document_id == doc.document_id() {
                            let _ = doc.set_status(block_id, *status);
                        }
                    }
                }
            }
            ServerEvent::BlockDeleted { document_id, block_id } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    if let Some(ref mut doc) = sync_state.doc {
                        if document_id == doc.document_id() {
                            let _ = doc.delete_block(block_id);
                        }
                    }
                }
            }
            ServerEvent::BlockCollapsedChanged { document_id, block_id, collapsed } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    if let Some(ref mut doc) = sync_state.doc {
                        if document_id == doc.document_id() {
                            let _ = doc.set_collapsed(block_id, *collapsed);
                        }
                    }
                }
            }
            ServerEvent::BlockMoved { document_id, block_id, after_id } => {
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    if let Some(ref mut doc) = sync_state.doc {
                        if document_id == doc.document_id() {
                            let _ = doc.move_block(block_id, after_id.as_ref());
                        }
                    }
                }
            }
            // SyncReset and resource events handled by cache path above
            _ => {}
        }
    }

    // Terminal-like auto-scroll: if we were at the bottom before processing
    // events and content changed, enable follow mode to smoothly track new content.
    if was_at_bottom && layout_gen.0 > scroll_state.last_content_gen && !scroll_state.user_scrolled_this_frame {
        scroll_state.start_following();
        scroll_state.last_content_gen = layout_gen.0;
    }
}

/// Handle context switch requests.
///
/// When `ContextSwitchRequested` is received (from constellation gt/gT/Ctrl-^/click):
/// 1. Save current scroll state to active CachedDocument
/// 2. Look up target document_id from context_name
/// 3. **Cache hit**: swap active_id, mirror cached doc to DocumentSyncState, restore scroll
/// 4. **Cache miss**: log warning (future: trigger join_context RPC)
pub fn handle_context_switch(
    mut switch_events: MessageReader<super::components::ContextSwitchRequested>,
    mut doc_cache: ResMut<super::components::DocumentCache>,
    mut sync_state: ResMut<super::components::DocumentSyncState>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut current_conv: ResMut<CurrentConversation>,
    mut pending_switch: ResMut<super::components::PendingContextSwitch>,
    bootstrap: Res<crate::connection::BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
) {
    for event in switch_events.read() {
        let context_name = &event.context_name;

        // Look up document_id for this context
        let target_doc_id = match doc_cache.document_id_for_context(context_name) {
            Some(id) => id.to_string(),
            None => {
                // Cache miss — spawn a new actor to join the context, then auto-switch
                info!(
                    "Context switch: cache miss for '{}', spawning actor to join",
                    context_name
                );
                pending_switch.0 = Some(context_name.clone());

                let kernel_id = conn_state
                    .current_kernel
                    .as_ref()
                    .map(|k| k.id.clone())
                    .unwrap_or_else(|| crate::constants::DEFAULT_KERNEL_ID.to_string());

                let instance = uuid::Uuid::new_v4().to_string();
                let _ = bootstrap.tx.send(crate::connection::BootstrapCommand::SpawnActor {
                    config: conn_state.ssh_config.clone(),
                    kernel_id,
                    context_name: context_name.clone(),
                    instance,
                });
                continue;
            }
        };

        // Skip if already active
        if doc_cache.active_id() == Some(target_doc_id.as_str()) {
            continue;
        }

        // Save current scroll offset to outgoing cache entry
        if let Some(active_id) = doc_cache.active_id().map(|s| s.to_string())
            && let Some(cached) = doc_cache.get_mut(&active_id)
        {
            cached.scroll_offset = scroll_state.offset;
        }

        // Switch active document
        doc_cache.set_active(&target_doc_id);

        // Mirror cached document to DocumentSyncState
        if let Some(cached) = doc_cache.get(&target_doc_id) {
            let agent_id = format!("user:{}", whoami::username());

            // Rebuild DocumentSyncState from cached document's oplog
            let oplog_bytes = cached.synced.doc().oplog_bytes().unwrap_or_default();
            sync_state.reset();
            match sync_state.apply_initial_state(&target_doc_id, &agent_id, &oplog_bytes) {
                Ok(_) => {
                    info!("Context switch: mirrored '{}' ({}) to DocumentSyncState", context_name, target_doc_id);
                }
                Err(e) => {
                    error!("Context switch: failed to mirror '{}' to DocumentSyncState: {}", context_name, e);
                }
            }

            // Update CurrentConversation
            current_conv.0 = Some(target_doc_id.clone());

            // Restore scroll offset
            scroll_state.offset = cached.scroll_offset;
            scroll_state.target_offset = cached.scroll_offset;
            scroll_state.following = false; // Don't auto-scroll on switch

            info!(
                "Context switch complete: '{}' → document '{}' (scroll: {:.0})",
                context_name, target_doc_id, cached.scroll_offset
            );
        }
    }
}

/// Check if the active document is stale (missed events while inactive).
///
/// Compares the active document's `synced_at_generation` against the current
/// `SyncGeneration`. If stale, triggers a `get_document_state()` re-fetch
/// via IoTaskPool to resync the document.
pub fn check_cache_staleness(
    doc_cache: Res<super::components::DocumentCache>,
    sync_gen: Res<crate::connection::actor_plugin::SyncGeneration>,
    actor: Option<Res<crate::connection::RpcActor>>,
    mut checked_gen: Local<u64>,
) {
    // Only check when SyncGeneration actually changed
    if sync_gen.0 == *checked_gen {
        return;
    }
    *checked_gen = sync_gen.0;

    let Some(active_id) = doc_cache.active_id().map(|s| s.to_string()) else {
        return;
    };

    let Some(cached) = doc_cache.get(&active_id) else {
        return;
    };

    // If the active document hasn't synced since the last generation bump, it's stale
    if cached.synced_at_generation < sync_gen.0 {
        let Some(ref actor) = actor else {
            return;
        };

        info!(
            "Staleness detected: active doc '{}' synced_at={} < current={}",
            active_id, cached.synced_at_generation, sync_gen.0
        );

        let handle = actor.handle.clone();
        let doc_id = active_id.clone();

        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.get_document_state(&doc_id).await {
                    Ok(state) => {
                        info!(
                            "Staleness re-fetch complete for {}: {} bytes oplog",
                            doc_id,
                            state.ops.len()
                        );
                        // The re-fetched state will arrive as a ContextJoined-like event
                        // through the normal sync path. The actor's subscription system
                        // handles the replay.
                    }
                    Err(e) => {
                        warn!("Staleness re-fetch failed for {}: {}", doc_id, e);
                    }
                }
            })
            .detach();

        // Mark as checked to avoid re-triggering until next generation bump
        // The actual resync will update synced_at_generation when events arrive
    }
}

// ============================================================================
// BLOCK CELL SYSTEMS (Per-Block UI Rendering)
// ============================================================================

use super::components::{BlockCell, BlockCellContainer, BlockCellLayout};

/// Display a JSON value with real newlines in string values.
///
/// `serde_json::to_string_pretty` re-escapes newlines in string values as `\n`,
/// which renders as literal backslash-n in the UI. This walks the JSON tree and
/// outputs string content directly so embedded newlines display correctly.
fn display_json_value(value: &serde_json::Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            // Output string content directly — preserves real \n as newlines
            // Indent continuation lines to align with the opening quote
            let continuation_pad = "  ".repeat(indent + 1);
            let indented = s.replace('\n', &format!("\n{continuation_pad}"));
            format!("\"{indented}\"")
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            let inner_pad = "  ".repeat(indent + 1);
            let items: Vec<String> = arr
                .iter()
                .map(|v| format!("{inner_pad}{}", display_json_value(v, indent + 1)))
                .collect();
            format!("[\n{}\n{pad}]", items.join(",\n"))
        }
        serde_json::Value::Object(obj) => {
            if obj.is_empty() {
                return "{}".to_string();
            }
            let inner_pad = "  ".repeat(indent + 1);
            let entries: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{inner_pad}\"{k}\": {}",
                        display_json_value(v, indent + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{pad}}}", entries.join(",\n"))
        }
    }
}

/// Format tool call arguments as compact key: value lines.
///
/// Flat JSON objects render as `key: value` per line (unquoted strings).
/// Multiline string values show first line + `(N lines)` suffix.
/// Values > 60 chars are truncated. Total > 5 lines shows first 4 + count.
/// Non-object inputs fall through to `display_json_value`.
fn format_tool_args(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return display_json_value(value, 0),
    };

    let max_lines = 5;
    let max_value_len = 60;
    let mut lines: Vec<String> = Vec::new();

    for (key, val) in obj {
        let formatted = match val {
            serde_json::Value::String(s) => {
                let first_line = s.lines().next().unwrap_or("");
                let line_count = s.lines().count();
                if line_count > 1 {
                    let truncated = if first_line.len() > max_value_len {
                        format!("{}...", &first_line[..max_value_len])
                    } else {
                        first_line.to_string()
                    };
                    format!("{} ({} lines)", truncated, line_count)
                } else if s.len() > max_value_len {
                    format!("{}...", &s[..max_value_len])
                } else {
                    s.clone()
                }
            }
            serde_json::Value::Null => "null".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            // Nested objects/arrays: compact single-line JSON
            other => {
                let json = serde_json::to_string(other).unwrap_or_default();
                if json.len() > max_value_len {
                    format!("{}...", &json[..max_value_len])
                } else {
                    json
                }
            }
        };
        lines.push(format!("{}: {}", key, formatted));
    }

    if lines.len() > max_lines {
        let remaining = lines.len() - (max_lines - 1);
        lines.truncate(max_lines - 1);
        lines.push(format!("... ({} more)", remaining));
    }

    lines.join("\n")
}

/// Format a single block for display.
///
/// Returns the formatted text for one block, including visual markers.
/// `local_ctx`: optional local context ID for drift push direction.
pub fn format_single_block(block: &BlockSnapshot, local_ctx: Option<&str>) -> String {
    match block.kind {
        BlockKind::Thinking => {
            if block.collapsed {
                "Thinking [collapsed]".to_string()
            } else {
                format!("Thinking\n{}", block.content)
            }
        }
        BlockKind::Text => block.content.clone(),
        BlockKind::ToolCall => {
            let name = block.tool_name.as_deref().unwrap_or("unknown");
            let status_tag = match block.status {
                kaijutsu_crdt::Status::Running => " [running]",
                kaijutsu_crdt::Status::Pending => " [pending]",
                _ => "",
            };
            let mut output = format!("{}{}", name, status_tag);
            if let Some(ref input) = block.tool_input {
                if !input.is_null() {
                    let args = format_tool_args(input);
                    if !args.is_empty() {
                        output.push('\n');
                        output.push_str(&args);
                    }
                }
            }
            output
        }
        BlockKind::ToolResult => {
            let content = block.content.trim();
            if block.is_error {
                if content.is_empty() {
                    "error".to_string()
                } else {
                    format!("error \u{2717}\n{}", content)
                }
            } else if content.is_empty() {
                "done".to_string()
            } else {
                let line_count = content.lines().count();
                if line_count <= 3 {
                    format!("done\n{}", content)
                } else {
                    format!("result\n{}", content)
                }
            }
        }
        BlockKind::ShellCommand => {
            format!("$ {}", block.content)
        }
        BlockKind::ShellOutput => {
            // Use display hint for richer formatting (tables, trees)
            let formatted = format_for_display(&block.content, block.display_hint.as_deref());
            formatted.text
        }
        BlockKind::Drift => format_drift_block(block, local_ctx),
    }
}

/// Spawn or update BlockCell entities to match the MainCell's BlockDocument.
///
/// This system diffs the current block IDs against existing BlockCell entities:
/// - Spawns new BlockCells for added blocks
/// - Despawns BlockCells for removed blocks
/// - Maintains order in BlockCellContainer
pub fn spawn_block_cells(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    _block_cells: Query<(Entity, &BlockCell)>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    // Get or create the BlockCellContainer
    let mut container = if let Ok(c) = containers.get_mut(main_ent) {
        c
    } else {
        // Add container to the main cell
        commands.entity(main_ent).insert(BlockCellContainer::default());
        return; // Will run again next frame with the container
    };

    // Get current block IDs from the document
    let current_blocks: Vec<_> = editor.blocks().iter().map(|b| b.id.clone()).collect();
    let current_ids: std::collections::HashSet<_> = current_blocks.iter().collect();

    // Find blocks to remove (in container but not in document)
    let to_remove: Vec<_> = container
        .block_to_entity
        .iter()
        .filter(|(id, _)| !current_ids.contains(id))
        .map(|(_, e)| *e)
        .collect();

    for entity in to_remove {
        commands.entity(entity).try_despawn();
        container.remove(entity);
    }

    // Get the current document version for timeline visibility
    let current_version = editor.version();

    // Find blocks to add (in document but not in container)
    let conv_entity = entities.conversation_container;
    for block_id in &current_blocks {
        if !container.contains(block_id) {
            // Spawn new BlockCell as flex child of ConversationContainer
            let entity = commands
                .spawn((
                    BlockCell::new(block_id.clone()),
                    BlockCellLayout::default(),
                    MsdfText,
                    MsdfTextAreaConfig::default(),
                    SdfTextEffects::default(),
                    ContentSize::default(),
                    Node {
                        width: Val::Percent(100.0),
                        ..default()
                    },
                    TimelineVisibility {
                        created_at_version: current_version,
                        opacity: 1.0,
                        is_past: false,
                    },
                ))
                .id();
            if let Some(conv) = conv_entity {
                if let Ok(mut ec) = commands.get_entity(conv) { ec.add_child(entity); }
            }
            container.add(block_id.clone(), entity);
        }
    }

    // Reorder container.block_cells to match document order
    let mut new_order = Vec::with_capacity(current_blocks.len());
    for block_id in &current_blocks {
        if let Some(entity) = container.get_entity(block_id) {
            new_order.push(entity);
        }
    }
    container.block_cells = new_order;
}

/// Sync RoleHeader entities for role transitions.
///
/// Spawns role header entities using the same pattern as BlockCells:
/// MsdfText + MsdfTextAreaConfig for consistent rendering.
pub fn sync_role_headers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    theme: Res<Theme>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(mut container) = containers.get_mut(main_ent) else {
        return;
    };

    // Despawn existing role headers (will rebuild each time blocks change)
    for entity in container.role_headers.drain(..) {
        commands.entity(entity).try_despawn();
    }

    // Detect role transitions and spawn headers
    let blocks = editor.blocks();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;

    for block in &blocks {
        let is_transition = prev_role != Some(block.role);
        if is_transition {
            // Get color for this role
            let color = match block.role {
                kaijutsu_crdt::Role::User => theme.block_user,
                kaijutsu_crdt::Role::Model => theme.block_assistant,
                kaijutsu_crdt::Role::System => theme.fg_dim,
                kaijutsu_crdt::Role::Tool => theme.block_tool_call,
            };

            // Use same rendering pattern as BlockCells
            let mut config = MsdfTextAreaConfig::default();
            config.default_color = color;

            let entity = commands
                .spawn((
                    RoleHeader {
                        role: block.role,
                        block_id: block.id.clone(),
                    },
                    RoleHeaderLayout::default(),
                    MsdfText,
                    config,
                    Node {
                        width: Val::Percent(100.0),
                        min_height: Val::Px(ROLE_HEADER_HEIGHT),
                        margin: UiRect::bottom(Val::Px(ROLE_HEADER_SPACING)),
                        ..default()
                    },
                ))
                .id();
            if let Some(conv) = entities.conversation_container {
                if let Ok(mut ec) = commands.get_entity(conv) { ec.add_child(entity); }
            }

            container.role_headers.push(entity);
        }
        prev_role = Some(block.role);
    }
}

/// Initialize MsdfTextBuffers for RoleHeaders that don't have one.
pub fn init_role_header_buffers(
    mut commands: Commands,
    role_headers: Query<(Entity, &RoleHeader, &MsdfTextAreaConfig), (With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, header, config) in role_headers.iter() {
        // Use UI metrics for headers (slightly smaller than content)
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        buffer.set_snap_x(true); // monospace: snap to pixel grid
        buffer.set_letter_spacing(text_metrics.letter_spacing);

        // Apply the role color from MsdfTextAreaConfig (set during sync_role_headers)
        buffer.set_color(config.default_color);

        // Set header text based on role
        let text = match header.role {
            kaijutsu_crdt::Role::User => "── USER ──────────────────────",
            kaijutsu_crdt::Role::Model => "── ASSISTANT ─────────────────",
            kaijutsu_crdt::Role::System => "── SYSTEM ────────────────────",
            kaijutsu_crdt::Role::Tool => "── TOOL ──────────────────────",
        };
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut font_system, text, attrs, cosmic_text::Shaping::Advanced);

        // Use try_insert to gracefully handle entity despawns between query and command application
        commands.entity(entity).try_insert((buffer, MsdfBufferInfo::default()));
    }
}

/// Initialize MsdfTextBuffers for BlockCells that don't have one.
pub fn init_block_cell_buffers(
    mut commands: Commands,
    block_cells: Query<Entity, (With<BlockCell>, With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in block_cells.iter() {
        let metrics = text_metrics.scaled_cell_metrics();
        let buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        // Use try_insert to gracefully handle entity despawns between query and command application
        commands.entity(entity).try_insert((buffer, MsdfBufferInfo::default()));
    }
}

/// Sync BlockCell MsdfTextBuffers with their corresponding block content.
///
/// Only updates cells whose content has changed (tracked via version).
/// When any buffer is updated, bumps LayoutGeneration to trigger re-layout.
/// Also applies block-specific text colors based on BlockKind and Role.
///
/// Note: We don't use Changed<CellEditor> because sync_main_cell_to_conversation
/// mutates editor.doc directly which doesn't trigger Bevy change detection.
/// Instead, we rely on block_cell.last_render_version for dirty tracking.
pub fn sync_block_cell_buffers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&mut BlockCell, &mut MsdfTextBuffer, &mut MsdfTextAreaConfig, Option<&TimelineVisibility>)>,
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    drift_state: Res<crate::ui::drift::DriftState>,
    mut layout_gen: ResMut<super::components::LayoutGeneration>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    // Only run when the main cell editor changes
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let doc_version = editor.version();

    // Check if ANY blocks need updating before allocating HashMaps
    // This is an O(N) check but avoids HashMap allocation when nothing changed
    let needs_update = container.block_cells.iter().any(|e| {
        block_cells
            .get(*e)
            .map(|(bc, _, _, _)| bc.last_render_version < doc_version)
            .unwrap_or(false)
    });

    if !needs_update {
        return;
    }

    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    // Get ordered blocks for role transition detection
    let blocks_ordered = editor.blocks();

    // Build lookup map and track role transitions
    let mut blocks: std::collections::HashMap<_, _> = std::collections::HashMap::new();
    let mut is_role_transition: std::collections::HashMap<kaijutsu_crdt::BlockId, (bool, kaijutsu_crdt::Role)> =
        std::collections::HashMap::new();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;

    for block in &blocks_ordered {
        let transition = prev_role != Some(block.role);
        is_role_transition.insert(block.id.clone(), (transition, block.role));
        blocks.insert(block.id.clone(), block.clone());
        prev_role = Some(block.role);
    }

    let mut layout_changed = false;
    for entity in &container.block_cells {
        let Ok((mut block_cell, mut buffer, mut config, timeline_vis)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Check if this block needs updating
        if block_cell.last_render_version >= doc_version {
            continue;
        }

        let Some(block) = blocks.get(&block_cell.block_id) else {
            continue;
        };

        // Format and update the buffer
        // Note: Role headers are now rendered as separate RoleHeader entities,
        // no longer prepended inline. See layout_block_cells for space reservation.
        let local_ctx = drift_state.local_context_id.as_deref();
        let text = format_single_block(block, local_ctx);

        // Apply block-specific color based on BlockKind and Role
        let base_color = block_color(block, &theme);
        buffer.set_color(base_color);

        // Set per-vertex effects: rainbow for user text only
        let effects = if block.kind == BlockKind::Text && block.role == kaijutsu_crdt::Role::User {
            SdfTextEffects { rainbow: true }
        } else {
            SdfTextEffects::default()
        };
        commands.entity(*entity).insert(effects);

        // Use rich text (markdown) for Text blocks, plain text for everything else
        let base_attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        if block.kind == BlockKind::Text {
            // Build markdown colors from theme, with base block color as default
            let md_colors = MarkdownColors {
                heading: bevy_to_cosmic_color(theme.md_heading_color),
                code: bevy_to_cosmic_color(theme.md_code_fg),
                strong: theme.md_strong_color.map(bevy_to_cosmic_color),
                code_block: bevy_to_cosmic_color(theme.md_code_block_fg),
            };
            let rich_spans = markdown::parse_to_rich_spans(&text);
            let cosmic_spans = markdown::to_cosmic_spans(&rich_spans, &base_attrs, &md_colors);
            buffer.set_rich_text(
                &mut font_system,
                cosmic_spans.into_iter(),
                &base_attrs,
                cosmic_text::Shaping::Advanced,
            );
        } else {
            buffer.set_text(&mut font_system, &text, base_attrs, cosmic_text::Shaping::Advanced);
        }

        // Apply timeline visibility opacity (dimmed when viewing historical states)
        let color = if let Some(vis) = timeline_vis {
            base_color.with_alpha(base_color.alpha() * vis.opacity)
        } else {
            base_color
        };
        config.default_color = color;

        // Track line count for layout dirty detection
        // Use newline count as a fast proxy - layout only needs to recompute
        // when vertical extent changes, not on every character
        let line_count = text.chars().filter(|c| *c == '\n').count() + 1;
        if block_cell.last_line_count != line_count {
            block_cell.last_line_count = line_count;
            layout_changed = true;
        }

        block_cell.last_render_version = doc_version;
    }

    // Only bump generation when layout actually needs to change
    // (new blocks, removed blocks, or line count changes)
    if layout_changed {
        layout_gen.bump();
    }
}

/// Layout BlockCells vertically within the conversation area.
///
/// Computes heights and positions for each block, accounting for:
/// - Block content height (using visual line count after wrapping)
/// - Spacing between blocks
/// - Indentation for nested tool results
/// - Space for turn headers before first block of each turn
///
/// **Performance optimization:** This system tracks the last layout generation
/// and window size. It skips expensive recomputation when neither content nor
/// window has changed. This makes scrolling feel instant regardless of block count.
pub fn layout_block_cells(
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCell, &mut BlockCellLayout, &mut MsdfTextBuffer)>,
    mut role_headers: Query<(&RoleHeader, &mut RoleHeaderLayout)>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    // NOTE: InputShadowHeight removed - legacy input layer is gone, ComposeBlock is inline
    font_system: Res<SharedFontSystem>,
    mut metrics_cache: ResMut<FontMetricsCache>,
    conv_containers: Query<&ComputedNode, (With<super::components::ConversationContainer>, With<crate::ui::tiling::PaneFocus>)>,
    windows: Query<&Window>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_layout_gen: Local<u64>,
    mut last_base_width: Local<f32>,
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

    let mut y_offset = 0.0;

    let margin = layout.workspace_margin_left;

    // Get pane width from the focused ConversationContainer's ComputedNode.
    // Falls back to window width on first frame before layout runs.
    let base_width = conv_containers.iter().next()
        .map(|node| node.size().x)
        .filter(|w| *w > 0.0)
        .unwrap_or_else(|| {
            windows.iter().next()
                .map(|w| w.resolution.width())
                .unwrap_or(1280.0)
        });
    let base_width = base_width - (margin * 2.0);

    // === Performance optimization: skip if nothing changed ===
    let width_changed = (base_width - *last_base_width).abs() > 1.0;
    let content_changed = layout_gen.0 != *last_layout_gen;

    if !width_changed && !content_changed {
        return;
    }

    *last_base_width = base_width;
    *last_layout_gen = layout_gen.0;

    // Note: visible_height is updated in smooth_scroll from ConversationContainer's ComputedNode

    // Lock font system for shaping
    let mut font_system = font_system.0.lock().unwrap();

    // Get blocks for lookup
    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();

    // Get blocks in order for role transition detection
    let blocks_ordered = editor.blocks();

    // Track role transitions for inline headers
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;
    let mut block_is_role_transition: std::collections::HashMap<kaijutsu_crdt::BlockId, bool> =
        std::collections::HashMap::new();
    for block in &blocks_ordered {
        let is_transition = prev_role != Some(block.role);
        block_is_role_transition.insert(block.id.clone(), is_transition);
        prev_role = Some(block.role); // Role is Copy, no clone needed
    }

    // Determine indentation: ToolResult blocks with a tool_call_id are nested

    for entity in &container.block_cells {
        let Ok((block_cell, mut block_layout, mut buffer)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Check if this is a role transition - if so, position the role header and add space
        if block_is_role_transition.get(&block_cell.block_id) == Some(&true) {
            // Find and position the role header for this block
            for (header, mut header_layout) in role_headers.iter_mut() {
                if header.block_id == block_cell.block_id {
                    header_layout.y_offset = y_offset;
                    break;
                }
            }
            y_offset += ROLE_HEADER_HEIGHT + ROLE_HEADER_SPACING;
        }

        // Determine indentation level based on parent_id (DAG nesting)
        let indent_level = if let Some(block) = blocks.get(&block_cell.block_id) {
            // ToolResult blocks with a tool_call_id are nested under the tool call
            if block.kind == BlockKind::ToolResult && block.tool_call_id.is_some() {
                1
            } else if block.parent_id.is_some() {
                // Any block with a parent_id gets indented
                1
            } else {
                0
            }
        } else {
            0
        };

        // Calculate wrap width accounting for indentation.
        // Border padding is on Node.padding — taffy subtracts it from available
        // width automatically, so we don't account for it here.
        let indent = indent_level as f32 * INDENT_WIDTH;
        let wrap_width = base_width - indent;

        // Compute height from visual line count (after text wrapping)
        // This shapes the buffer if needed and returns accurate wrapped line count
        // Pixel alignment via metrics_cache helps small text render crisply
        let line_count = buffer.visual_line_count(&mut font_system, wrap_width, Some(&mut metrics_cache));
        // Tight height: just the lines, minimal padding for future chrome.
        // Border vertical padding is on Node.padding — taffy adds it to the
        // border box automatically, so min_height is content-only.
        // TODO(dedup): inline height formula duplicates WorkspaceLayout::height_for_lines
        let height = (line_count as f32) * layout.line_height + 4.0;

        block_layout.y_offset = y_offset;
        block_layout.height = height;
        block_layout.indent_level = indent_level;

        y_offset += height + BLOCK_SPACING;
    }

    // Update scroll state with total content height
    scroll_state.content_height = y_offset;
}

/// Sync BlockCellLayout heights/indentation to Bevy Node for flex layout.
///
/// After `layout_block_cells` computes heights, this system writes them to the
/// Node component so Bevy's flex layout knows how tall each block should be.
pub fn update_block_cell_nodes(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut Node, Option<&super::block_border::BlockBorderStyle>), With<BlockCell>>,
    mut role_header_nodes: Query<&mut Node, (With<RoleHeader>, Without<BlockCell>)>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    for entity in &container.block_cells {
        let Ok((layout, mut node, border_style)) = block_cells.get_mut(*entity) else {
            continue;
        };
        node.min_height = Val::Px(layout.height);
        node.margin = UiRect {
            left: Val::Px(layout.indent_level as f32 * INDENT_WIDTH),
            bottom: Val::Px(BLOCK_SPACING),
            ..default()
        };
        // Set padding from border style so text sits inside the content box
        // while the border (absolute child at 100%×100%) fills the border box.
        if let Some(style) = border_style {
            node.padding = UiRect {
                left: Val::Px(style.padding.left),
                right: Val::Px(style.padding.right),
                top: Val::Px(style.padding.top),
                bottom: Val::Px(style.padding.bottom),
            };
        } else {
            node.padding = UiRect::ZERO;
        }
    }

    // Role header nodes already have fixed height from spawn, but update if needed
    for entity in &container.role_headers {
        if let Ok(mut node) = role_header_nodes.get_mut(*entity) {
            node.min_height = Val::Px(ROLE_HEADER_HEIGHT);
            node.margin = UiRect::bottom(Val::Px(ROLE_HEADER_SPACING));
        }
    }
}

/// Reorder ConversationContainer children to match document order.
///
/// Interleaves role headers before their associated blocks so flex layout
/// renders them in the correct visual order.
pub fn reorder_conversation_children(
    entities: Res<EditorEntities>,
    mut commands: Commands,
    containers: Query<&BlockCellContainer>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    role_headers: Query<&RoleHeader>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Some(conv_entity) = entities.conversation_container else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    // Build ordered child list: role headers interleaved with blocks
    let blocks = editor.blocks();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;
    let mut ordered_children = Vec::new();

    // Build block_id → role header entity map
    let mut header_map: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    for &header_ent in &container.role_headers {
        if let Ok(header) = role_headers.get(header_ent) {
            header_map.insert(header.block_id.clone(), header_ent);
        }
    }

    for block in &blocks {
        let is_transition = prev_role != Some(block.role);
        if is_transition {
            if let Some(&header_ent) = header_map.get(&block.id) {
                ordered_children.push(header_ent);
            }
        }
        if let Some(block_ent) = container.get_entity(&block.id) {
            ordered_children.push(block_ent);
        }
        prev_role = Some(block.role);
    }

    if let Ok(mut ec) = commands.get_entity(conv_entity) { ec.replace_children(&ordered_children); }
}

/// Position BlockCell text areas from ComputedNode (flex layout result).
///
/// Follows the same pattern as `position_compose_block`: reads ComputedNode +
/// UiGlobalTransform to determine screen position, then writes MsdfTextAreaConfig.
/// This replaces the manual positioning in `apply_block_cell_positions`.
pub fn position_block_cells_from_flex(
    mut block_cells: Query<
        (&ComputedNode, &UiGlobalTransform, &mut MsdfTextAreaConfig, Option<&CalculatedClip>),
        With<BlockCell>,
    >,
) {
    for (computed, transform, mut config, clip) in block_cells.iter_mut() {
        let (_, _, translation) = transform.to_scale_angle_translation();
        let size = computed.size();
        let content = computed.content_box();

        // Content box origin relative to the node's top-left corner.
        // Translation is center-based, so convert to top-left first.
        let node_left = translation.x - size.x / 2.0;
        let node_top = translation.y - size.y / 2.0;
        let left = node_left + content.min.x;
        let top = node_top + content.min.y;

        config.left = left;
        config.top = top;
        config.scale = 1.0;

        let raw_left = left as i32;
        let raw_top = top as i32;
        let raw_right = (node_left + content.max.x) as i32;
        let raw_bottom = (node_top + content.max.y) as i32;

        config.bounds = if let Some(clip) = clip {
            crate::text::TextBounds {
                left: raw_left.max(clip.clip.min.x as i32),
                top: raw_top.max(clip.clip.min.y as i32),
                right: raw_right.min(clip.clip.max.x as i32),
                bottom: raw_bottom.min(clip.clip.max.y as i32),
            }
        } else {
            crate::text::TextBounds { left: raw_left, top: raw_top, right: raw_right, bottom: raw_bottom }
        };
    }
}

/// Position RoleHeader text areas from ComputedNode (flex layout result).
pub fn position_role_headers_from_flex(
    mut role_headers: Query<
        (&ComputedNode, &UiGlobalTransform, &mut MsdfTextAreaConfig, Option<&CalculatedClip>),
        (With<RoleHeader>, Without<BlockCell>),
    >,
) {
    for (computed, transform, mut config, clip) in role_headers.iter_mut() {
        let (_, _, translation) = transform.to_scale_angle_translation();
        let size = computed.size();

        let left = translation.x - size.x / 2.0;
        let top = translation.y - size.y / 2.0;

        config.left = left;
        config.top = top;
        config.scale = 1.0;

        let raw_left = left as i32;
        let raw_top = top as i32;
        let raw_right = (left + size.x) as i32;
        let raw_bottom = (top + size.y) as i32;

        config.bounds = if let Some(clip) = clip {
            crate::text::TextBounds {
                left: raw_left.max(clip.clip.min.x as i32),
                top: raw_top.max(clip.clip.min.y as i32),
                right: raw_right.min(clip.clip.max.x as i32),
                bottom: raw_bottom.min(clip.clip.max.y as i32),
            }
        } else {
            crate::text::TextBounds { left: raw_left, top: raw_top, right: raw_right, bottom: raw_bottom }
        };
    }
}


// ============================================================================
// TURN/ROLE HEADER SYSTEMS (Removed in DAG migration)
// ============================================================================
//
// Turn headers are now rendered inline based on role transitions.
// The layout_block_cells system handles role transition detection and
// reserves space for inline role headers.
//
// TODO: Implement inline role header rendering in BlockCell format_single_block
// or as a separate UI element spawned alongside BlockCells.

// NOTE: Legacy input area position systems have been removed.
// The InputLayer, InputFrame, and floating prompt are replaced by ComposeBlock.

// ============================================================================
// BLOCK CELL EDITING SYSTEMS (Unified Edit Model)
// ============================================================================
//
// These systems enable editing any BlockCell. ComposeBlock handles new input
// while existing blocks can be edited inline.
// The core insight: "mode emerges from focus + edit state, not a global switch"
//
// Flow:
// 1. User navigates with j/k to focus a BlockCell (existing navigate_blocks system)
// 2. User presses 'i' to enter edit mode on the focused block
// 3. Keyboard input goes to that block via CRDT operations
// 4. User presses Escape to exit edit mode

// handle_block_edit_mode + handle_block_cell_input — DELETED (Phase 5)
// Block editing enter/exit now handled by input::systems::handle_unfocus + Activate action
// Block cell text input migrated to input::systems::handle_block_edit_input

/// Update cursor rendering to show cursor in editing BlockCell.
///
/// When a BlockCell is being edited, the cursor should render at that block's
/// position, not in the PromptCell.
pub fn update_block_edit_cursor(
    editing_cells: Query<(&BlockCell, &BlockEditCursor, &BlockCellLayout, &MsdfTextAreaConfig), With<EditingBlockCell>>,
    entities: Res<EditorEntities>,
    focus_area: Res<FocusArea>,
    mut cursor_query: Query<(&mut Node, &mut Visibility), With<CursorMarker>>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    text_metrics: Res<TextMetrics>,
) {
    // Only when editing a block
    let Ok((block_cell, cursor, _layout, config)) = editing_cells.single() else {
        return;
    };

    let Some(cursor_ent) = entities.cursor else {
        return;
    };

    let Ok((mut node, mut visibility)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    // Get block content for cursor position calculation
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) else {
        return;
    };

    // Calculate cursor row/col within block content
    let text = &block.content;
    let offset = cursor.offset.min(text.len());
    let before_cursor = &text[..offset];
    let row = before_cursor.matches('\n').count();
    let col = before_cursor
        .rfind('\n')
        .map(|pos| offset - pos - 1)
        .unwrap_or(offset);

    // Show cursor when editing a block
    *visibility = if focus_area.is_text_input() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    // Position cursor relative to block cell's text area
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;
    let x = config.left + (col as f32 * char_width);
    let y = config.top + (row as f32 * line_height);

    node.left = Val::Px(x - 2.0);
    node.top = Val::Px(y);
}

// ============================================================================
// COMPOSE BLOCK SYSTEMS
// ============================================================================

/// Initialize MsdfTextBuffer for ComposeBlock entities.
pub fn init_compose_block_buffer(
    mut commands: Commands,
    compose_blocks: Query<Entity, (With<ComposeBlock>, With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in compose_blocks.iter() {
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        buffer.set_snap_x(true); // monospace: snap to pixel grid
        buffer.set_letter_spacing(text_metrics.letter_spacing);

        // Initialize with placeholder text
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut font_system, "Type here...", attrs, cosmic_text::Shaping::Advanced);
        // Shape the text so glyphs are populated for MSDF rendering
        buffer.visual_line_count(&mut font_system, 800.0, None);

        commands.entity(entity).insert((buffer, MsdfBufferInfo::default()));
    }
}

// handle_compose_block_input — DELETED (Phase 5)
// Migrated to input::systems::handle_compose_input

/// Sync ComposeBlock text to its MsdfTextBuffer.
///
/// In shell mode, runs kaish syntax validation on each keystroke and tints
/// text red for invalid syntax (but not for incomplete input like `if`).
pub fn sync_compose_block_buffer(
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    mut compose_blocks: Query<(&ComposeBlock, &mut MsdfTextBuffer, &mut MsdfTextAreaConfig), Changed<ComposeBlock>>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (compose, mut buffer, mut config) in compose_blocks.iter_mut() {
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));

        // Show placeholder when empty
        let display_text = if compose.is_empty() {
            "Type here..."
        } else {
            &compose.text
        };

        // Set glyph color from theme before shaping (color bakes into glyphs)
        // Auto-detect shell mode from text prefix (: or `)
        let color = if compose.is_empty() {
            theme.fg_dim // Placeholder is dimmed
        } else if is_shell_command(&compose.text) {
            let validation = crate::kaish::validate(strip_shell_prefix(&compose.text));
            if !validation.valid && !validation.incomplete {
                theme.block_tool_error // Red tint for syntax errors
            } else {
                theme.block_user
            }
        } else {
            theme.block_user // User input color
        };
        buffer.set_color(color);
        config.default_color = color;

        buffer.set_text(&mut font_system, display_text, attrs, cosmic_text::Shaping::Advanced);

        // Shape the text so glyphs are populated for MSDF rendering
        let wrap_width = config.bounds.width().max(100) as f32;
        buffer.visual_line_count(&mut font_system, wrap_width, None);
    }
}

/// Position the ComposeBlock text area based on its computed UI layout.
///
/// ComposeBlock uses Bevy's flex layout system (unlike BlockCells which use manual positioning).
/// This system reads the computed position from Bevy's UI layout and updates the
/// MsdfTextAreaConfig bounds so the MSDF text renders in the correct location.
pub fn position_compose_block(
    mut compose_blocks: Query<
        (&bevy::ui::ComputedNode, &bevy::ui::UiGlobalTransform, &mut MsdfTextAreaConfig),
        With<ComposeBlock>,
    >,
) {
    for (computed, global_transform, mut config) in compose_blocks.iter_mut() {
        // UiGlobalTransform gives us the center position in screen space
        // (origin at top-left, Y increases downward).
        // Convert to top-left corner for rendering.
        let (_, _, translation) = global_transform.to_scale_angle_translation();
        let size = computed.size();

        // Translation is the center of the node, convert to top-left corner
        let left = translation.x - size.x / 2.0;
        let top = translation.y - size.y / 2.0;

        // Update MsdfTextAreaConfig position
        config.left = left;
        config.top = top;

        // Update bounds for clipping
        config.bounds = crate::text::TextBounds {
            left: left as i32,
            top: top as i32,
            right: (left + size.x) as i32,
            bottom: (top + size.y) as i32,
        };
    }
}

/// Position the cursor in the focused ComposeBlock during Input mode.
///
/// When in Input mode with no active EditingBlockCell, the cursor beam
/// should appear at the ComposeBlock's cursor offset. This replaces the
/// old bubble cursor system.
pub fn update_compose_cursor(
    focus_area: Res<FocusArea>,
    entities: Res<EditorEntities>,
    compose_blocks: Query<(&ComposeBlock, &MsdfTextAreaConfig), With<crate::ui::tiling::PaneFocus>>,
    editing_blocks: Query<Entity, With<EditingBlockCell>>,
    mut cursor_query: Query<(&mut Node, &mut Visibility, &MaterialNode<CursorBeamMaterial>), With<CursorMarker>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
) {
    // Only show compose cursor when compose area is focused
    if !matches!(*focus_area, FocusArea::Compose) {
        return;
    }
    // Don't override if editing a block cell inline
    if !editing_blocks.is_empty() {
        return;
    }

    let Some(cursor_ent) = entities.cursor else { return };
    let Ok((compose, config)) = compose_blocks.single() else { return };
    let Ok((mut node, mut visibility, material_node)) = cursor_query.get_mut(cursor_ent) else { return };

    *visibility = Visibility::Inherited;

    // Calculate position from ComposeBlock.cursor offset
    let text = &compose.text;
    let offset = compose.cursor.min(text.len());
    let before = &text[..offset];
    let row = before.matches('\n').count();
    let col = before.rfind('\n').map(|p| offset - p - 1).unwrap_or(offset);

    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;

    node.left = Val::Px(config.left + (col as f32 * char_width) - 2.0);
    node.top = Val::Px(config.top + (row as f32 * line_height));

    // Update material for Input mode appearance
    if let Some(material) = cursor_materials.get_mut(&material_node.0) {
        material.time.y = CursorMode::Beam as u8 as f32;
        material.color = theme.cursor_insert;
        material.params = Vec4::new(0.25, 1.2, 2.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::{BlockId, Role, Status};

    fn test_block_id() -> BlockId {
        BlockId {
            document_id: "test-doc".to_string(),
            agent_id: "test-agent".to_string(),
            seq: 0,
        }
    }

    #[test]
    fn test_format_tool_args_flat_object() {
        let input = serde_json::json!({
            "path": "/etc/hosts",
            "limit": 10
        });
        let result = format_tool_args(&input);
        assert!(result.contains("path: /etc/hosts"));
        assert!(result.contains("limit: 10"));
    }

    #[test]
    fn test_format_tool_args_truncates_long_values() {
        let long_val = "x".repeat(80);
        let input = serde_json::json!({ "data": long_val });
        let result = format_tool_args(&input);
        assert!(result.contains("..."));
        assert!(result.len() < 80);
    }

    #[test]
    fn test_format_tool_args_multiline_string() {
        let input = serde_json::json!({
            "content": "line1\nline2\nline3\nline4"
        });
        let result = format_tool_args(&input);
        assert!(result.contains("line1"));
        assert!(result.contains("(4 lines)"));
    }

    #[test]
    fn test_format_tool_args_many_keys_truncated() {
        let input = serde_json::json!({
            "a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6
        });
        let result = format_tool_args(&input);
        assert!(result.contains("... ("));
        assert!(result.contains("more)"));
    }

    #[test]
    fn test_format_tool_args_empty_object() {
        let input = serde_json::json!({});
        let result = format_tool_args(&input);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_format_tool_args_non_object() {
        let input = serde_json::json!("just a string");
        let result = format_tool_args(&input);
        assert!(result.contains("just a string"));
    }

    #[test]
    fn test_tool_call_plain_text() {
        let block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
            "test",
        );
        let result = format_single_block(&block, None);
        // Shader borders handle visual framing — text is plain
        assert!(!result.contains('┌'));
        assert!(!result.contains('└'));
        assert!(result.contains("read_file"));
        assert!(result.contains("path: /etc/hosts"));
        // Running status tag (tool_call constructor sets Running)
        assert!(result.contains("[running]"));
    }

    #[test]
    fn test_tool_call_empty_args() {
        let mut block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            "list_all",
            serde_json::json!(null),
            "test",
        );
        block.status = Status::Done;
        let result = format_single_block(&block, None);
        assert_eq!(result, "list_all");
    }

    #[test]
    fn test_tool_result_success_empty() {
        let result_block = BlockSnapshot::tool_result(
            test_block_id(),
            test_block_id(),
            "",
            false,
            Some(0),
            "test",
        );
        let result = format_single_block(&result_block, None);
        assert_eq!(result, "done");
    }

    #[test]
    fn test_tool_result_success_short() {
        let result_block = BlockSnapshot::tool_result(
            test_block_id(),
            test_block_id(),
            "file contents here",
            false,
            Some(0),
            "test",
        );
        let result = format_single_block(&result_block, None);
        assert!(result.starts_with("done\n"));
        assert!(result.contains("file contents here"));
    }

    #[test]
    fn test_tool_result_success_long() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result_block = BlockSnapshot::tool_result(
            test_block_id(),
            test_block_id(),
            content,
            false,
            Some(0),
            "test",
        );
        let result = format_single_block(&result_block, None);
        assert!(!result.contains('┌'));
        assert!(result.starts_with("result\n"));
    }

    #[test]
    fn test_tool_result_error() {
        let result_block = BlockSnapshot::tool_result(
            test_block_id(),
            test_block_id(),
            "permission denied",
            true,
            Some(1),
            "test",
        );
        let result = format_single_block(&result_block, None);
        assert!(result.contains("error \u{2717}"));
        assert!(result.contains("permission denied"));
    }

    #[test]
    fn test_tool_result_error_empty() {
        let result_block = BlockSnapshot::tool_result(
            test_block_id(),
            test_block_id(),
            "",
            true,
            Some(1),
            "test",
        );
        let result = format_single_block(&result_block, None);
        assert_eq!(result, "error");
    }

    #[test]
    fn test_thinking_no_emoji() {
        let block = BlockSnapshot::thinking(test_block_id(), None, "reasoning here", "test");
        let result = format_single_block(&block, None);
        assert!(!result.contains('💭'));
        assert!(result.starts_with("Thinking\n"));
        assert!(result.contains("reasoning here"));
    }

    #[test]
    fn test_thinking_collapsed_no_emoji() {
        let mut block = BlockSnapshot::thinking(test_block_id(), None, "reasoning", "test");
        block.collapsed = true;
        let result = format_single_block(&block, None);
        assert!(!result.contains('💭'));
        assert_eq!(result, "Thinking [collapsed]");
    }

    #[test]
    fn test_format_blocks_delegates_to_format_single_block() {
        let blocks = vec![
            BlockSnapshot::text(test_block_id(), None, Role::User, "hello", "test"),
            BlockSnapshot::text(test_block_id(), None, Role::Model, "world", "test"),
        ];
        let result = format_blocks_for_display(&blocks);
        assert_eq!(result, "hello\n\nworld");
    }

    #[test]
    fn test_format_blocks_empty() {
        assert_eq!(format_blocks_for_display(&[]), "");
    }

    #[test]
    fn test_drift_commit_no_emoji() {
        let block = BlockSnapshot::drift(
            test_block_id(),
            None,
            "checkpoint summary",
            "test",
            "ctx-abc",
            None,
            DriftKind::Commit,
        );
        let result = format_single_block(&block, None);
        assert!(!result.contains('📝'));
        assert!(result.starts_with("# @ctx-abc"));
    }

    #[test]
    fn test_drift_none_no_emoji() {
        let mut block = BlockSnapshot::drift(
            test_block_id(),
            None,
            "some drift content",
            "test",
            "ctx-xyz",
            Some("claude".to_string()),
            DriftKind::Push, // We'll override drift_kind
        );
        block.drift_kind = None;
        let result = format_single_block(&block, None);
        assert!(!result.contains('🌊'));
        assert!(result.starts_with("~ @ctx-xyz"));
    }

    #[test]
    fn test_draw_box_basic() {
        let result = draw_box("header", "content", 40);
        assert!(result.contains("┌─ header"));
        assert!(result.contains("│ content"));
        assert!(result.contains('└'));
        assert!(result.contains('┐'));
        assert!(result.contains('┘'));
    }
}
