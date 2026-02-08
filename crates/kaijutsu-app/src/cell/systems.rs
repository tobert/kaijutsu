//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::components::{
    BlockDocument, BlockEditCursor, BlockKind, BlockSnapshot, Cell, CellEditor, CellPosition,
    CellState, ComposeBlock, ConversationScrollState, CurrentMode, DriftKind, EditingBlockCell,
    EditorMode, FocusTarget, InputKind, MainCell, PromptSubmitted, RoleHeader, RoleHeaderLayout,
    ViewingConversation, WorkspaceLayout,
};
use crate::conversation::CurrentConversation;
use crate::text::{
    bevy_to_cosmic_color, FontMetricsCache, MsdfText, SharedFontSystem, MsdfTextAreaConfig,
    MsdfTextBuffer, TextMetrics,
};
use crate::text::markdown::{self, MarkdownColors};
use crate::ui::format::format_for_display;
use crate::ui::state::AppScreen;
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;

// ============================================================================
// LAYOUT CONSTANTS
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
const INDENT_WIDTH: f32 = 24.0;

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
        BlockKind::ToolCall => theme.block_tool_call,
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

/// Keys consumed by mode switching this frame (cleared each frame).
#[derive(Resource, Default)]
pub struct ConsumedModeKeys(pub std::collections::HashSet<KeyCode>);

/// Clear consumed keys at the start of each frame.
///
/// This must run before any system that reads or writes to ConsumedModeKeys.
pub fn clear_consumed_keys(mut consumed: ResMut<ConsumedModeKeys>) {
    consumed.0.clear();
}

/// Handle vim-style mode switching with input presence transitions.
///
/// Key bindings:
/// - `i` in Normal → Chat mode + Docked presence (expand from minimized)
/// - `Space` in Normal → Chat mode + Overlay presence (summon floating)
/// - `Backtick` in Normal → Shell mode + Docked presence
/// - `Escape` in Insert → Normal mode + Minimized presence (collapse)
/// - `:` in Normal → Command mode (no presence change)
///
/// Note: Chat/Shell modes only allowed in Conversation screen.
/// Note: Keys already in ConsumedModeKeys are skipped (e.g., `i` consumed by block editing)
pub fn handle_mode_switch(
    mut key_events: MessageReader<KeyboardInput>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    modal_open: Option<Res<crate::ui::constellation::ModalDialogOpen>>,
    compose_blocks: Query<&ComposeBlock>,
    screen: Res<State<AppScreen>>,
) {
    // Note: consumed.0.clear() happens in clear_consumed_keys system
    // which runs at the start of each frame, before block editing and mode switching

    // Skip when a modal dialog is open to prevent mode changes during dialog input
    if modal_open.is_some_and(|m| m.0) {
        for _ in key_events.read() {} // Consume events
        return;
    }

    // Mode switching works globally - no focus required.
    // Focus determines which cell receives text input, not mode switching.
    // This allows Space to summon the input from anywhere in the conversation.

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Skip keys already consumed by earlier systems (e.g., block editing)
        if consumed.0.contains(&event.key_code) {
            continue;
        }

        match mode.0 {
            EditorMode::Normal => {
                // Input modes only make sense in Conversation screen
                // (Dashboard has no input field to route to)
                if *screen.get() != AppScreen::Conversation {
                    continue;
                }

                // In normal mode:
                // - i enters Chat (docked) - unless consumed by block editing
                // - Space enters Chat (overlay)
                // - ` (backtick) enters Shell
                // - : also enters Shell (kaish handles commands natively)
                // - v enters Visual
                match event.key_code {
                    KeyCode::KeyI => {
                        mode.0 = EditorMode::Input(InputKind::Chat);
                        // InputPresence no longer used with ComposeBlock
                        info!("Mode: CHAT");
                    }
                    KeyCode::Space => {
                        // Space enters chat mode
                        mode.0 = EditorMode::Input(InputKind::Chat);
                        consumed.0.insert(KeyCode::Space);
                        info!("Mode: CHAT (via Space)");
                    }
                    KeyCode::Backquote => {
                        // Backtick enters shell mode (kaish REPL)
                        mode.0 = EditorMode::Input(InputKind::Shell);
                        consumed.0.insert(KeyCode::Backquote);
                        info!("Mode: SHELL");
                    }
                    KeyCode::Semicolon if event.text.as_deref() == Some(":") => {
                        // Colon also enters Shell - kaish handles : commands natively
                        mode.0 = EditorMode::Input(InputKind::Shell);
                        consumed.0.insert(KeyCode::Semicolon);
                        info!("Mode: SHELL (command)");
                    }
                    KeyCode::KeyV => {
                        mode.0 = EditorMode::Visual;
                        consumed.0.insert(KeyCode::KeyV);
                        info!("Mode: VISUAL");
                    }
                    _ => {}
                }
            }
            EditorMode::Input(_) | EditorMode::Visual => {
                // Escape returns to normal mode
                if event.key_code == KeyCode::Escape {
                    mode.0 = EditorMode::Normal;
                    consumed.0.insert(KeyCode::Escape);

                    // Check if ComposeBlock has draft content
                    let compose_empty = compose_blocks
                        .iter()
                        .next()
                        .map(|compose| compose.is_empty())
                        .unwrap_or(true);

                    if compose_empty {
                        info!("Mode: NORMAL");
                    } else {
                        info!("Mode: NORMAL (draft preserved in ComposeBlock)");
                    }
                }
            }
        }
    }
}

/// Handle keyboard input for the focused cell (CellEditor-based cells only).
///
/// Note: ComposeBlock has its own input handling in handle_compose_block_input.
/// This system handles input for CellEditor-based entities like BlockCells in edit mode.
pub fn handle_cell_input(
    mut key_events: MessageReader<KeyboardInput>,
    focus: Res<FocusTarget>,
    mode: Res<CurrentMode>,
    modal_open: Option<Res<crate::ui::constellation::ModalDialogOpen>>,
    consumed: Res<ConsumedModeKeys>,
    mut editors: Query<&mut CellEditor>,
) {
    // Skip when a modal dialog is open to prevent input leakage
    if modal_open.is_some_and(|m| m.0) {
        for _ in key_events.read() {} // Consume events
        return;
    }

    let Some(focused_entity) = focus.entity else {
        return;
    };

    let Ok(mut editor) = editors.get_mut(focused_entity) else {
        return;
    };

    // Skip text input on the frame when mode changes (e.g., 'i' to enter insert)
    // This prevents the mode-switch key from being inserted as text
    if mode.is_changed() {
        return;
    }

    // Only handle text input in Chat/Shell mode
    if !mode.0.accepts_input() {
        // In Normal mode, handle navigation with h/j/k/l
        if mode.0 == EditorMode::Normal {
            for event in key_events.read() {
                if !event.state.is_pressed() {
                    continue;
                }
                // Skip keys consumed by mode switching
                if consumed.0.contains(&event.key_code) {
                    continue;
                }
                match event.key_code {
                    KeyCode::KeyH | KeyCode::ArrowLeft => editor.move_left(),
                    KeyCode::KeyL | KeyCode::ArrowRight => editor.move_right(),
                    KeyCode::Home | KeyCode::Digit0 => editor.move_home(),
                    KeyCode::End | KeyCode::Digit4 if event.text.as_deref() == Some("$") => {
                        editor.move_end()
                    }
                    _ => {}
                }
            }
        }
        return;
    }

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Skip keys consumed by mode switching (e.g., 'i' to enter insert)
        if consumed.0.contains(&event.key_code) {
            continue;
        }

        // Handle special keys first (before text input)
        // These may have text fields set but should be handled specially
        match event.key_code {
            KeyCode::Backspace => {
                editor.backspace();
                continue;
            }
            KeyCode::Delete => {
                editor.delete();
                continue;
            }
            KeyCode::Enter => {
                // Enter inserts newline in CellEditor-based editing
                editor.insert("\n");
                continue;
            }
            KeyCode::Tab => {
                editor.insert("    ");
                continue;
            }
            KeyCode::ArrowLeft => {
                editor.move_left();
                continue;
            }
            KeyCode::ArrowRight => {
                editor.move_right();
                continue;
            }
            KeyCode::Home => {
                editor.move_home();
                continue;
            }
            KeyCode::End => {
                editor.move_end();
                continue;
            }
            _ => {}
        }

        // Handle text input via the text field
        if let Some(ref text) = event.text {
            for c in text.chars() {
                // Skip control characters
                if c.is_control() {
                    continue;
                }
                editor.insert(&c.to_string());
            }
        }
    }
}

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

        // Initialize with current editor text
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(
            &mut font_system,
            &editor.text(),
            attrs,
            cosmic_text::Shaping::Advanced,
        );

        // Use try_insert to gracefully handle entity despawns between query and command application
        commands.entity(entity).try_insert(buffer);
        info!("Initialized MsdfTextBuffer for entity {:?}", entity);
    }
}

/// Format content blocks for display.
///
/// This produces a text representation with visual markers for different block types.
/// Collapsed thinking blocks are shown as a single line.
fn format_blocks_for_display(blocks: &[BlockSnapshot]) -> String {
    if blocks.is_empty() {
        return String::new();
    }

    let mut output = String::new();

    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            output.push_str("\n\n");
        }

        match block.kind {
            BlockKind::Thinking => {
                if block.collapsed {
                    // Collapsed: show indicator
                    output.push_str("💭 [Thinking collapsed - Tab to expand]");
                } else {
                    // Expanded: show with dimmed header
                    output.push_str("💭 ─── Thinking ───\n");
                    output.push_str(&block.content);
                    output.push_str("\n───────────────────");
                }
            }
            BlockKind::Text => {
                output.push_str(&block.content);
            }
            BlockKind::ToolCall => {
                output.push_str("🔧 Tool: ");
                if let Some(ref name) = block.tool_name {
                    output.push_str(name);
                }
                output.push('\n');
                // Pretty-print JSON input with real newlines in string values
                if let Some(ref input) = block.tool_input {
                    output.push_str(&display_json_value(input, 0));
                }
            }
            BlockKind::ToolResult => {
                if block.is_error {
                    output.push_str("❌ Error:\n");
                } else {
                    output.push_str("📤 Result:\n");
                }
                output.push_str(&block.content);
            }
            BlockKind::ShellCommand => {
                output.push_str("$ ");
                output.push_str(&block.content);
            }
            BlockKind::ShellOutput => {
                // Use display hint for richer formatting (tables, trees)
                let formatted = format_for_display(&block.content, block.display_hint.as_deref());
                output.push_str(&formatted.text);
            }
            BlockKind::Drift => {
                output.push_str(&format_drift_block(block, None));
            }
        }
    }

    output
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
    let width = 72;

    // Determine direction arrow: → if we sent it, ← if we received it
    let arrow = match local_ctx {
        Some(local) if ctx == local => "→",
        _ => "←",
    };

    match block.drift_kind {
        Some(DriftKind::Push) => {
            let preview = block.content.lines().next().unwrap_or("");
            format!("{} {} ({})  {}\n", arrow, ctx_label, model, preview)
        }
        Some(DriftKind::Pull) | Some(DriftKind::Distill) => {
            let header = format!("pulled from {} ({})", ctx_label, model);
            draw_box(&header, &block.content, width)
        }
        Some(DriftKind::Merge) => {
            let header = format!("⇄ merged from {} ({})", ctx_label, model);
            draw_box(&header, &block.content, width)
        }
        Some(DriftKind::Commit) => {
            format!("📝 {}  {}\n", ctx_label, block.content.lines().next().unwrap_or(""))
        }
        None => {
            format!("🌊 {} ({})  {}\n", ctx_label, model, block.content.lines().next().unwrap_or(""))
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
pub fn handle_collapse_toggle(
    mut key_events: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    focus: Res<FocusTarget>,
    mut cells: Query<(&mut CellEditor, &mut CellState)>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(focused_entity) = focus.entity else {
        return;
    };

    let Ok((mut editor, mut cell_state)) = cells.get_mut(focused_entity) else {
        return;
    };

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Tab toggles collapse
        if event.key_code == KeyCode::Tab {
            // Find thinking blocks to toggle
            let thinking_blocks: Vec<_> = editor
                .blocks()
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Thinking))
                .map(|b| b.id.clone())
                .collect();

            if !thinking_blocks.is_empty() {
                // Toggle all thinking blocks via the editor
                for block_id in &thinking_blocks {
                    editor.toggle_block_collapse(block_id);
                }
                // Get the current collapsed state for logging
                let collapsed = editor
                    .blocks()
                    .iter()
                    .find(|b| matches!(b.kind, BlockKind::Thinking))
                    .map(|b| b.collapsed)
                    .unwrap_or(false);
                info!(
                    "Thinking blocks: {}",
                    if collapsed { "collapsed" } else { "expanded" }
                );
            } else {
                // Toggle whole cell collapse
                cell_state.collapsed = !cell_state.collapsed;
                info!(
                    "Cell collapse: {}",
                    if cell_state.collapsed {
                        "collapsed"
                    } else {
                        "expanded"
                    }
                );
            }
        }
    }
}

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
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO;
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

/// Update cursor position and visibility based on focused cell and mode.
pub fn update_cursor(
    focus: Res<FocusTarget>,
    mode: Res<CurrentMode>,
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
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO;
    let line_height = text_metrics.cell_line_height;
    let x = config.left + (col as f32 * char_width);
    let y = config.top + (row as f32 * line_height);

    node.left = Val::Px(x - 2.0); // Slight offset for beam alignment
    node.top = Val::Px(y);

    // Update cursor mode and wandering orb params
    if let Some(material) = cursor_materials.get_mut(&material_node.0) {
        let cursor_mode = match mode.0 {
            EditorMode::Input(_) => CursorMode::Beam,
            EditorMode::Normal => CursorMode::Block,
            EditorMode::Visual => CursorMode::Block,
        };
        material.time.y = cursor_mode as u8 as f32;

        // Cursor colors from theme
        let color = match mode.0 {
            EditorMode::Normal => theme.cursor_normal,
            EditorMode::Input(_) => theme.cursor_insert,
            EditorMode::Visual => theme.cursor_visual,
        };
        material.color = color;

        // params: x=orb_size, y=intensity, z=wander_speed, w=blink_rate
        material.params = match mode.0 {
            EditorMode::Input(_) => Vec4::new(0.25, 1.2, 2.0, 0.0),  // Larger orb, faster wander, no blink
            EditorMode::Normal => Vec4::new(0.2, 1.0, 1.5, 0.6),    // Medium orb, gentle blink
            EditorMode::Visual => Vec4::new(0.22, 1.1, 1.8, 0.0),   // Slightly larger, no blink
        };
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
pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    mode: Res<CurrentMode>,
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
            let text = event.text.clone();
            let cell_id = doc_cell_id.clone();
            let conv = conv_id.to_string();

            let tx = channel.sender();
            match mode.0 {
                EditorMode::Input(InputKind::Shell) => {
                    // Shell mode: fire-and-forget, results via ServerEvent broadcast
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
                }
                EditorMode::Input(InputKind::Chat) => {
                    // Chat mode: fire-and-forget, results via ServerEvent broadcast
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
                _ => {
                    warn!("Unexpected prompt submission in {:?} mode", mode.0);
                }
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
        return;
    }

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

/// Handle mouse wheel scrolling for the conversation area.
pub fn handle_scroll_input(
    mut scroll_state: ResMut<ConversationScrollState>,
    mut mouse_wheel: MessageReader<MouseWheel>,
    mode: Res<CurrentMode>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    // Scroll speed multipliers - tuned for responsive feel
    // Line units (discrete mouse wheel clicks): ~3 lines per click
    const LINE_SCROLL_SPEED: f32 = 60.0;
    // Pixel units (touchpad, smooth scroll): direct 1:1 feels natural
    const PIXEL_SCROLL_SPEED: f32 = 1.5;

    // Handle mouse wheel - accumulate all events this frame
    for event in mouse_wheel.read() {
        // Scroll up = negative delta = decrease offset (show earlier content)
        // Scroll down = positive delta = increase offset (show later content)
        let multiplier = match event.unit {
            bevy::input::mouse::MouseScrollUnit::Line => LINE_SCROLL_SPEED,
            bevy::input::mouse::MouseScrollUnit::Pixel => PIXEL_SCROLL_SPEED,
        };
        let delta = -event.y * multiplier;
        scroll_state.scroll_by(delta);
    }

    // Note: j/k now handled by navigate_blocks for block-level navigation
    // This system keeps page navigation and Shift+G

    // Page down/up with Ctrl+d/u in Normal mode
    if mode.0 == EditorMode::Normal {
        // Page down/up with Ctrl+d/u
        if keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight) {
            let half_page = scroll_state.visible_height * 0.5;
            if keys.just_pressed(KeyCode::KeyD) {
                scroll_state.scroll_by(half_page);
            }
            if keys.just_pressed(KeyCode::KeyU) {
                scroll_state.scroll_by(-half_page);
            }
        }
        // G to go to bottom, gg to go to top
        if keys.just_pressed(KeyCode::KeyG)
            && (keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight)) {
                // Shift+G = go to bottom
                scroll_state.scroll_to_end();
            }
            // Note: gg (double tap) would need state tracking, skip for now
    }
}

// ============================================================================
// BLOCK FOCUS NAVIGATION (Phase 2)
// ============================================================================

use super::components::FocusedBlockCell;

/// Navigate between blocks with j/k in Normal mode.
///
/// This is the core Phase 2 feature: j/k moves focus between blocks rather
/// than just scrolling. The focused block gets visual highlighting and
/// scroll-to-view behavior.
///
/// Key bindings (Normal mode only):
/// - `j` → Focus next block
/// - `k` → Focus previous block
/// - `G` (Shift+G) → Focus last block
/// - `g` then `g` → Focus first block (TODO: needs double-tap state)
/// - `Home` → Focus first block
/// - `End` → Focus last block
pub fn navigate_blocks(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<CurrentMode>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    block_cells: Query<(Entity, &BlockCell, &BlockCellLayout)>,
    mut focus: ResMut<FocusTarget>,
    mut scroll_state: ResMut<ConversationScrollState>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    // Get ordered block IDs from the document
    let blocks = editor.blocks();
    if blocks.is_empty() {
        return;
    }

    // Determine navigation direction
    let nav = if keys.just_pressed(KeyCode::KeyJ) {
        Some(NavigationDirection::Next)
    } else if keys.just_pressed(KeyCode::KeyK) {
        Some(NavigationDirection::Previous)
    } else if keys.just_pressed(KeyCode::Home) {
        Some(NavigationDirection::First)
    } else if keys.just_pressed(KeyCode::End)
        || (keys.just_pressed(KeyCode::KeyG)
            && (keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight)))
    {
        Some(NavigationDirection::Last)
    } else {
        None
    };

    let Some(direction) = nav else {
        return;
    };

    // Find current focus index
    let current_idx = focus
        .block_id
        .as_ref()
        .and_then(|id| blocks.iter().position(|b| &b.id == id));

    // Calculate new index based on direction
    let new_idx = match direction {
        NavigationDirection::Next => match current_idx {
            Some(i) if i + 1 < blocks.len() => i + 1,
            Some(i) => i, // Stay at end
            None => 0,    // Start at first
        },
        NavigationDirection::Previous => match current_idx {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,                // Stay at start
            None => blocks.len() - 1,    // Start at last
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

/// Navigation direction for block focus.
#[derive(Debug, Clone, Copy)]
enum NavigationDirection {
    Next,
    Previous,
    First,
    Last,
}

/// Scroll to keep a block visible in the viewport.
///
/// If the block is above the viewport, scroll up to show its top.
/// If below, scroll down to show its bottom.
fn scroll_to_block_visible(
    scroll_state: &mut ConversationScrollState,
    layout: &BlockCellLayout,
) {
    let block_top = layout.y_offset;
    let block_bottom = layout.y_offset + layout.height;
    let view_top = scroll_state.offset;
    let view_bottom = scroll_state.offset + scroll_state.visible_height;

    // Margin to keep around the block (so it's not right at the edge)
    const MARGIN: f32 = 20.0;

    if block_top < view_top + MARGIN {
        // Block is above viewport - scroll up
        scroll_state.target_offset = (block_top - MARGIN).max(0.0);
        scroll_state.offset = scroll_state.target_offset;
        scroll_state.following = false; // User navigated, disable auto-follow
    } else if block_bottom > view_bottom - MARGIN {
        // Block is below viewport - scroll down
        let target = block_bottom - scroll_state.visible_height + MARGIN;
        scroll_state.target_offset = target.min(scroll_state.max_offset());
        scroll_state.offset = scroll_state.target_offset;
        // If we scrolled to the very bottom, enable following
        scroll_state.following = scroll_state.is_at_bottom();
    }
}

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

/// Handle `f` keybinding to expand the focused block to full screen.
///
/// In Normal mode with a focused block:
/// - `f` pushes ExpandedBlock view onto ViewStack
/// - The block content is shown full-screen for easier reading/editing
pub fn handle_expand_block(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<CurrentMode>,
    focus: Res<FocusTarget>,
    mut view_stack: ResMut<crate::ui::state::ViewStack>,
) {
    // Only in Normal mode with a focused block
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(ref block_id) = focus.block_id else {
        return;
    };

    // `f` expands the focused block
    if keys.just_pressed(KeyCode::KeyF) {
        view_stack.push(crate::ui::state::View::ExpandedBlock {
            block_id: block_id.clone(),
        });
        info!("Expanded block: {:?}", block_id);
    }
}

/// Handle Esc to pop ViewStack when in an overlay view.
///
/// In Normal mode with an overlay view (like ExpandedBlock):
/// - Esc pops the view stack, returning to the previous view
/// - At root view, Esc is handled by mode switching instead
pub fn handle_view_pop(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<CurrentMode>,
    mut view_stack: ResMut<crate::ui::state::ViewStack>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    // Only when Esc is pressed and we're not at root
    if keys.just_pressed(KeyCode::Escape) && !view_stack.is_at_root() {
        view_stack.pop();
    }
}

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

        let color = block_color(block, &theme);
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut fs, &block.content, attrs, cosmic_text::Shaping::Advanced);
        drop(fs);

        // Spawn the view entity
        let entity = commands
            .spawn((
                ExpandedBlockView,
                MsdfText,
                buffer,
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
            commands.entity(entity).despawn();
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
    if conversation_container.is_empty() {
        return;
    }

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
        debug!("sync_main_cell: no main cell entity");
        return;
    };
    let Some(ref source_doc) = sync_state.doc else {
        debug!("sync_main_cell: no document in sync state");
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
    sync_gen: Res<crate::connection::actor_plugin::SyncGeneration>,
) {
    use kaijutsu_client::ServerEvent;
    use super::components::CachedDocument;

    // Check if we're at the bottom before processing events (for auto-scroll)
    let was_at_bottom = scroll_state.is_at_bottom();

    // Get agent ID for creating documents
    let agent_id = format!("user:{}", whoami::username());

    // Handle initial document state from ContextJoined
    for result in result_events.read() {
        if let RpcResultMessage::ContextJoined { seat, document_id, initial_state } = result {
            let context_name = seat.id.context.clone();

            // Create or update cache entry
            if !doc_cache.contains(document_id) {
                let mut cached = CachedDocument {
                    doc: BlockDocument::new(document_id, &agent_id),
                    sync: super::sync::SyncManager::new(),
                    context_name: context_name.clone(),
                    synced_at_generation: 0,
                    last_accessed: std::time::Instant::now(),
                    scroll_offset: 0.0,
                    seat_info: Some(seat.clone()),
                };

                // Apply initial state if provided
                if let Some(state) = initial_state {
                    match cached.sync.apply_initial_state(&mut cached.doc, &state.document_id, &state.ops) {
                        Ok(result) => {
                            info!("Cache: initial sync for '{}' ({}) result: {:?}", context_name, document_id, result);
                        }
                        Err(e) => {
                            error!("Cache: initial sync error for '{}' ({}): {}", context_name, document_id, e);
                        }
                    }
                }

                cached.synced_at_generation = sync_gen.0;
                doc_cache.insert(document_id.clone(), cached);
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
        match event {
            ServerEvent::BlockInserted { document_id, block, ops } => {
                // Route to cache entry
                if let Some(cached) = doc_cache.get_mut(document_id) {
                    match cached.sync.apply_block_inserted(&mut cached.doc, document_id, block, ops) {
                        Ok(result) => {
                            cached.synced_at_generation = sync_gen.0;
                            trace!("Cache: block insert for {} {:?}: {:?}", document_id, block.id, result);
                        }
                        Err(e) => {
                            trace!("Cache: block insert error for {} {:?}: {}", document_id, block.id, e);
                        }
                    }
                }

                // Mirror to DocumentSyncState if active
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    match sync_state.apply_block_inserted(document_id, &agent_id, block, ops) {
                        Ok(result) => {
                            trace!("Block insert sync result for {:?}: {:?}", block.id, result);
                        }
                        Err(e) => {
                            trace!("Block insert sync error for {:?}: {}", block.id, e);
                        }
                    }
                }
            }

            ServerEvent::BlockTextOps {
                document_id,
                block_id,
                ops,
            } => {
                // Route to cache entry
                if let Some(cached) = doc_cache.get_mut(document_id) {
                    match cached.sync.apply_text_ops(&mut cached.doc, document_id, ops) {
                        Ok(result) => {
                            cached.synced_at_generation = sync_gen.0;
                            trace!("Cache: text ops for {} {:?}: {:?}", document_id, block_id, result);
                        }
                        Err(e) => {
                            trace!("Cache: text ops error for {} {:?}: {}", document_id, block_id, e);
                        }
                    }
                }

                // Mirror to DocumentSyncState if active
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    match sync_state.apply_text_ops(document_id, &agent_id, ops) {
                        Ok(result) => {
                            trace!("Text ops sync result for {:?}: {:?}", block_id, result);
                        }
                        Err(e) => {
                            trace!("Text ops sync error for {:?}: {}", block_id, e);
                        }
                    }
                }
            }

            ServerEvent::BlockStatusChanged {
                document_id,
                block_id,
                status,
            } => {
                if let Some(cached) = doc_cache.get_mut(document_id)
                    && let Err(e) = cached.doc.set_status(block_id, *status)
                {
                    warn!("Cache: failed to update block status for {}: {}", document_id, e);
                }
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    let Some(ref mut doc) = sync_state.doc else { continue; };
                    if document_id != doc.document_id() { continue; }
                    if let Err(e) = doc.set_status(block_id, *status) {
                        warn!("Failed to update block status: {}", e);
                    }
                }
            }
            ServerEvent::BlockDeleted {
                document_id,
                block_id,
            } => {
                if let Some(cached) = doc_cache.get_mut(document_id)
                    && let Err(e) = cached.doc.delete_block(block_id)
                {
                    warn!("Cache: failed to delete block for {}: {}", document_id, e);
                }
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    let Some(ref mut doc) = sync_state.doc else { continue; };
                    if document_id != doc.document_id() { continue; }
                    if let Err(e) = doc.delete_block(block_id) {
                        warn!("Failed to delete block: {}", e);
                    }
                }
            }
            ServerEvent::BlockCollapsedChanged {
                document_id,
                block_id,
                collapsed,
            } => {
                if let Some(cached) = doc_cache.get_mut(document_id)
                    && let Err(e) = cached.doc.set_collapsed(block_id, *collapsed)
                {
                    warn!("Cache: failed to update collapsed for {}: {}", document_id, e);
                }
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    let Some(ref mut doc) = sync_state.doc else { continue; };
                    if document_id != doc.document_id() { continue; }
                    if let Err(e) = doc.set_collapsed(block_id, *collapsed) {
                        warn!("Failed to update block collapsed state: {}", e);
                    }
                }
            }
            ServerEvent::BlockMoved {
                document_id,
                block_id,
                after_id,
            } => {
                if let Some(cached) = doc_cache.get_mut(document_id)
                    && let Err(e) = cached.doc.move_block(block_id, after_id.as_ref())
                {
                    warn!("Cache: failed to move block for {}: {}", document_id, e);
                }
                if active_doc_id.as_deref() == Some(document_id.as_str()) {
                    let Some(ref mut doc) = sync_state.doc else { continue; };
                    if document_id != doc.document_id() { continue; }
                    if let Err(e) = doc.move_block(block_id, after_id.as_ref()) {
                        warn!("Failed to move block: {}", e);
                    }
                }
            }
            // Resource events are not block-related — ignore here
            ServerEvent::ResourceUpdated { .. } | ServerEvent::ResourceListChanged { .. } => {}
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
            let oplog_bytes = cached.doc.oplog_bytes();
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

/// Format a single block for display.
///
/// Returns the formatted text for one block, including visual markers.
/// `local_ctx`: optional local context ID for drift push direction.
pub fn format_single_block(block: &BlockSnapshot, local_ctx: Option<&str>) -> String {
    match block.kind {
        BlockKind::Thinking => {
            if block.collapsed {
                "💭 [Thinking collapsed - Tab to expand]".to_string()
            } else {
                format!(
                    "💭 ─── Thinking ───\n{}\n───────────────────",
                    block.content
                )
            }
        }
        BlockKind::Text => block.content.clone(),
        BlockKind::ToolCall => {
            let name = block.tool_name.as_deref().unwrap_or("unknown");
            let mut output = format!("🔧 Tool: {}\n", name);
            if let Some(ref input) = block.tool_input {
                output.push_str(&display_json_value(input, 0));
            }
            output
        }
        BlockKind::ToolResult => {
            if block.is_error {
                format!("❌ Error:\n{}", block.content)
            } else {
                format!("📤 Result:\n{}", block.content)
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
    for block_id in &current_blocks {
        if !container.contains(block_id) {
            // Spawn new BlockCell with timeline visibility tracking
            let entity = commands
                .spawn((
                    BlockCell::new(block_id.clone()),
                    BlockCellLayout::default(),
                    MsdfText,
                    MsdfTextAreaConfig::default(),
                    TimelineVisibility {
                        created_at_version: current_version,
                        opacity: 1.0,
                        is_past: false,
                    },
                ))
                .id();
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
                ))
                .id();

            container.role_headers.push(entity);
        }
        prev_role = Some(block.role);
    }
}

/// Initialize MsdfTextBuffers for RoleHeaders that don't have one.
pub fn init_role_header_buffers(
    mut commands: Commands,
    role_headers: Query<(Entity, &RoleHeader), (With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, header) in role_headers.iter() {
        // Use UI metrics for headers (slightly smaller than content)
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        buffer.set_snap_x(true); // monospace: snap to pixel grid

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
        commands.entity(entity).try_insert(buffer);
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
        commands.entity(entity).try_insert(buffer);
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
    windows: Query<&Window>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_layout_gen: Local<u64>,
    mut last_window_size: Local<(f32, f32)>,
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

    // Get window size for wrap width and visible height calculation
    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // === Performance optimization: skip if nothing changed ===
    // Check if content or window changed since last layout
    let window_changed = (window_width, window_height) != *last_window_size;
    let content_changed = layout_gen.0 != *last_layout_gen;

    if !window_changed && !content_changed {
        // Nothing to re-layout - skip expensive computation
        return;
    }

    // Record current state for next frame comparison
    *last_window_size = (window_width, window_height);
    *last_layout_gen = layout_gen.0;
    // === End performance optimization ===

    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);

    // Note: visible_height is now updated only in apply_block_cell_positions
    // to consolidate scroll state updates and prevent double-clamping issues

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

        // Calculate wrap width accounting for indentation
        let indent = indent_level as f32 * INDENT_WIDTH;
        let wrap_width = base_width - indent;

        // Compute height from visual line count (after text wrapping)
        // This shapes the buffer if needed and returns accurate wrapped line count
        // Pixel alignment via metrics_cache helps small text render crisply
        let line_count = buffer.visual_line_count(&mut font_system, wrap_width, Some(&mut metrics_cache));
        // Tight height: just the lines, minimal padding for future chrome
        let height = (line_count as f32) * layout.line_height + 4.0;

        block_layout.y_offset = y_offset;
        block_layout.height = height;
        block_layout.indent_level = indent_level;

        y_offset += height + BLOCK_SPACING;
    }

    // Update scroll state with total content height
    scroll_state.content_height = y_offset;
}

/// Apply layout positions to BlockCell MsdfTextAreaConfig for rendering.
///
/// **Performance optimization:** This system tracks the last-applied layout generation
/// and scroll offset. It skips work when neither layout nor scroll has changed.
/// Combined with layout_block_cells optimization, this means scrolling only runs
/// this lightweight position update, not the expensive layout computation.
pub fn apply_block_cell_positions(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut MsdfTextAreaConfig), With<BlockCell>>,
    mut role_headers: Query<(&RoleHeaderLayout, &mut MsdfTextAreaConfig), (With<RoleHeader>, Without<BlockCell>)>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    dag_view: Query<(&ComputedNode, &UiGlobalTransform), With<super::components::ConversationContainer>>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_applied_gen: Local<u64>,
    mut prev_scroll: Local<f32>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    // === Performance optimization: skip if nothing changed ===
    let layout_changed = layout_gen.0 != *last_applied_gen;
    // Use 1.0 pixel threshold to prevent sub-pixel jitter during streaming
    let scroll_changed = (scroll_state.offset - *prev_scroll).abs() >= 1.0;

    if !layout_changed && !scroll_changed {
        // Neither layout nor scroll changed - skip position updates
        return;
    }

    // Record current state for next frame comparison
    *last_applied_gen = layout_gen.0;
    *prev_scroll = scroll_state.offset;
    // === End performance optimization ===

    // Derive visible bounds from the DagView's actual computed layout.
    // This respects the flex layout (North dock, ComposeBlock, South dock)
    // instead of relying on hardcoded constants that drift out of sync.
    let Ok((node, transform)) = dag_view.single() else {
        warn!("apply_block_cell_positions: DagView (ConversationContainer) not found");
        return;
    };
    let (_, _, translation) = transform.to_scale_angle_translation();
    let content = node.content_box();
    let visible_top = translation.y + content.min.y;
    let visible_bottom = translation.y + content.max.y;
    let base_width = content.width();
    let visible_height = (visible_bottom - visible_top).max(100.0);
    let margin = layout.workspace_margin_left;

    // Update scroll state with visible area
    // Note: smooth_scroll handles clamping, so we don't call clamp_target() here
    scroll_state.visible_height = visible_height;

    let scroll_offset = scroll_state.offset;

    for entity in &container.block_cells {
        let Ok((block_layout, mut config)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Calculate actual position with scroll
        let indent = block_layout.indent_level as f32 * INDENT_WIDTH;
        let left = margin + indent;
        let width = base_width - indent;
        let content_top = visible_top + block_layout.y_offset - scroll_offset;

        config.left = left;
        config.top = content_top;
        config.scale = 1.0;

        // Clamp bounds to intersection of visible area and block area
        // This prevents text from rendering outside its block when partially scrolled
        let block_bottom = content_top + block_layout.height;
        let clamped_top = visible_top.max(content_top).max(0.0);
        let clamped_bottom = visible_bottom.min(block_bottom).max(clamped_top + 1.0);
        config.bounds = crate::text::TextBounds {
            left: left as i32,
            top: clamped_top as i32,
            right: (left + width) as i32,
            bottom: clamped_bottom as i32,
        };
    }

    // Position role headers
    for entity in &container.role_headers {
        let Ok((header_layout, mut config)) = role_headers.get_mut(*entity) else {
            continue;
        };

        let content_top = visible_top + header_layout.y_offset - scroll_offset;
        let header_bottom = content_top + ROLE_HEADER_HEIGHT;
        let clamped_top = visible_top.max(content_top).max(0.0);
        let clamped_bottom = visible_bottom.min(header_bottom).max(clamped_top + 1.0);

        config.left = margin;
        config.top = content_top;
        config.scale = 1.0;
        config.bounds = crate::text::TextBounds {
            left: margin as i32,
            top: clamped_top as i32,
            right: (margin + base_width) as i32,
            bottom: clamped_bottom as i32,
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

/// Handle entering/exiting edit mode on a focused BlockCell.
///
/// Key bindings:
/// - `i` in Normal mode with FocusedBlockCell → Enter edit mode on that block
/// - `Escape` in edit mode → Exit edit mode, return to Normal
///
/// This system modifies mode_switch behavior:
/// - When a BlockCell is focused, `i` edits that block instead of ComposeBlock
/// - When no BlockCell is focused, `i` still routes to ComposeBlock for new input
pub fn handle_block_edit_mode(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    screen: Res<State<AppScreen>>,
    focus: Res<FocusTarget>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    _containers: Query<&BlockCellContainer>,
    focused_block_cells: Query<Entity, With<FocusedBlockCell>>,
    editing_block_cells: Query<Entity, With<EditingBlockCell>>,
) {
    // Only in Conversation screen
    if *screen.get() != AppScreen::Conversation {
        return;
    }

    // Handle `i` to enter edit mode on focused BlockCell
    if mode.0 == EditorMode::Normal && keys.just_pressed(KeyCode::KeyI) {
        // Check if there's a focused BlockCell to edit
        if let Ok(focused_entity) = focused_block_cells.single() {
            // Get the block info to validate it's editable
            if let Some(ref block_id) = focus.block_id {
                // Check if this block exists and is editable
                // For now, only User and Text blocks are editable
                if let Some(main_ent) = entities.main_cell {
                    if let Ok(editor) = main_cells.get(main_ent) {
                        let blocks = editor.blocks();
                        if let Some(block) = blocks.iter().find(|b| &b.id == block_id) {
                            // Only allow editing User role Text blocks for now
                            // (extending to other types is future work)
                            if block.role == kaijutsu_crdt::Role::User
                                && block.kind == BlockKind::Text
                            {
                                // Enter edit mode on this block
                                let content_len = block.content.len();
                                commands.entity(focused_entity).insert((
                                    EditingBlockCell,
                                    BlockEditCursor { offset: content_len },
                                ));

                                // Set mode to Input (Chat) - this enables cursor rendering
                                mode.0 = EditorMode::Input(InputKind::Chat);
                                consumed.0.insert(KeyCode::KeyI);
                                info!(
                                    "Entered edit mode on block {:?} (User Text, {} chars)",
                                    block_id, content_len
                                );
                                return;
                            } else {
                                info!(
                                    "Block {:?} is {:?}/{:?} - not editable (only User Text)",
                                    block_id, block.role, block.kind
                                );
                            }
                        }
                    }
                }
            }
        }
        // No focused block or not editable - fall through to default `i` behavior
        // (which is handled by handle_mode_switch)
    }

    // Handle Escape to exit edit mode
    if mode.0.accepts_input() && keys.just_pressed(KeyCode::Escape) {
        // Check if we're editing a BlockCell
        for entity in editing_block_cells.iter() {
            // Remove edit markers
            commands
                .entity(entity)
                .remove::<EditingBlockCell>()
                .remove::<BlockEditCursor>();
            info!("Exited edit mode on block cell {:?}", entity);
        }
        // Note: mode.0 will be set to Normal by handle_mode_switch
    }
}

/// Handle keyboard input for editing BlockCells.
///
/// Routes text input to the editing block via CRDT operations on the MainCell's
/// BlockDocument. This ensures all edits are properly tracked and synced.
pub fn handle_block_cell_input(
    mut key_events: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    consumed: Res<ConsumedModeKeys>,
    entities: Res<EditorEntities>,
    mut main_cells: Query<&mut CellEditor, With<MainCell>>,
    mut editing_cells: Query<(&BlockCell, &mut BlockEditCursor), With<EditingBlockCell>>,
) {
    // Only process input in Input modes
    if !mode.0.accepts_input() {
        return;
    }

    // Get the editing block cell (if any)
    let Ok((block_cell, mut cursor)) = editing_cells.single_mut() else {
        return; // No block being edited
    };

    // Get the main cell editor for CRDT operations
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(mut editor) = main_cells.get_mut(main_ent) else {
        return;
    };

    // Skip on frame when mode changes
    if mode.is_changed() {
        for _ in key_events.read() {} // Consume events
        return;
    }

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Skip keys consumed by mode switching
        if consumed.0.contains(&event.key_code) {
            continue;
        }

        // Handle special keys
        match event.key_code {
            KeyCode::Backspace => {
                if cursor.offset > 0 {
                    // Find previous character boundary
                    if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
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
                continue;
            }
            KeyCode::Delete => {
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
                continue;
            }
            KeyCode::Enter => {
                // Insert newline in block content
                if editor
                    .doc
                    .edit_text(&block_cell.block_id, cursor.offset, "\n", 0)
                    .is_ok()
                {
                    cursor.offset += 1;
                }
                continue;
            }
            KeyCode::ArrowLeft => {
                if cursor.offset > 0 {
                    if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                        let text = &block.content;
                        let mut new_offset = cursor.offset - 1;
                        while new_offset > 0 && !text.is_char_boundary(new_offset) {
                            new_offset -= 1;
                        }
                        cursor.offset = new_offset;
                    }
                }
                continue;
            }
            KeyCode::ArrowRight => {
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
                continue;
            }
            KeyCode::Home => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let before_cursor = &text[..cursor.offset];
                    cursor.offset = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
                }
                continue;
            }
            KeyCode::End => {
                if let Some(block) = editor.doc.get_block_snapshot(&block_cell.block_id) {
                    let text = &block.content;
                    let after_cursor = &text[cursor.offset..];
                    cursor.offset += after_cursor.find('\n').unwrap_or(after_cursor.len());
                }
                continue;
            }
            _ => {}
        }

        // Handle text input
        if let Some(ref text) = event.text {
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
    }
}

/// Update cursor rendering to show cursor in editing BlockCell.
///
/// When a BlockCell is being edited, the cursor should render at that block's
/// position, not in the PromptCell.
pub fn update_block_edit_cursor(
    editing_cells: Query<(&BlockCell, &BlockEditCursor, &BlockCellLayout, &MsdfTextAreaConfig), With<EditingBlockCell>>,
    entities: Res<EditorEntities>,
    mode: Res<CurrentMode>,
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

    // Show cursor in edit mode
    *visibility = if mode.0.accepts_input() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    // Position cursor relative to block cell's text area
    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO;
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

        // Initialize with placeholder text
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));
        buffer.set_text(&mut font_system, "Type here...", attrs, cosmic_text::Shaping::Advanced);
        // Shape the text so glyphs are populated for MSDF rendering
        buffer.visual_line_count(&mut font_system, 800.0, None);

        commands.entity(entity).insert(buffer);
    }
}

/// Handle keyboard input for ComposeBlock.
///
/// When in INSERT mode with ComposeBlock focused, process text input,
/// cursor movement, and submit on Enter.
///
/// NOTE: If there's an active bubble, input goes there instead of here.
pub fn handle_compose_block_input(
    mut keyboard: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    screen: Res<State<AppScreen>>,
    mut compose_blocks: Query<&mut ComposeBlock>,
    mut submit_writer: MessageWriter<PromptSubmitted>,
    bubble_registry: Res<BubbleRegistry>,
) {
    // Only handle in Conversation screen
    if *screen.get() != AppScreen::Conversation {
        return;
    }

    // Only handle in INSERT mode (Chat or Shell)
    if !mode.0.accepts_input() {
        return;
    }

    // If there's an active bubble, input goes there instead
    if bubble_registry.active().is_some() {
        // Consume events so they don't get processed by other systems
        for _ in keyboard.read() {}
        return;
    }

    let Ok(mut compose) = compose_blocks.single_mut() else {
        return;
    };

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        use bevy::input::keyboard::Key;
        match (&event.logical_key, &event.text) {
            // Enter submits (without Shift)
            (Key::Enter, _) => {
                if !compose.is_empty() {
                    let text = compose.take();
                    info!("ComposeBlock submitted: {} chars", text.len());
                    submit_writer.write(PromptSubmitted { text });
                }
            }
            // Backspace
            (Key::Backspace, _) => {
                compose.backspace();
            }
            // Delete
            (Key::Delete, _) => {
                compose.delete();
            }
            // Arrow keys
            (Key::ArrowLeft, _) => {
                compose.move_left();
            }
            (Key::ArrowRight, _) => {
                compose.move_right();
            }
            // Text input
            (_, Some(text)) => {
                compose.insert(text);
            }
            _ => {}
        }
    }
}

/// Sync ComposeBlock text to its MsdfTextBuffer.
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
        let color = if compose.is_empty() {
            theme.fg_dim // Placeholder is dimmed
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

// ============================================================================
// MOBILE INPUT BUBBLE SYSTEMS
// ============================================================================
//
// ARCHITECTURE: Floating CRDT-backed input bubbles that can be stashed, recalled,
// and positioned spatially. Multiple bubbles can exist simultaneously for
// different draft contexts.
//
// State Machine:
//   Normal Mode
//       │
//       ├─ Space → Spawn new Active bubble (stash previous active)
//       │           └─ mode → Input(Chat)
//       │
//       └─ Tab → Recall stashed[0] (cycle through stashed)
//
//   Active Bubble
//       │
//       ├─ Esc → Stash bubble → Normal mode
//       │
//       ├─ Enter → Submit content via PromptSubmitted, despawn bubble
//       │
//       └─ Cmd+Backspace → Dismiss without submit

use super::components::{
    BubbleConfig, BubblePosition, BubbleRegistry,
    BubbleSpawnContext, BubbleState, InputBubble,
};

/// Handle Space in Normal mode to spawn or recall a bubble.
///
/// - If no bubbles exist: spawn a new one
/// - If bubbles are stashed: recall the most recent
/// - If a bubble is active: spawn new (stash current)
///
/// Also handles Tab for cycling through stashed bubbles.
pub fn handle_bubble_spawn(
    mut commands: Commands,
    mut keyboard: MessageReader<KeyboardInput>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    screen: Res<State<AppScreen>>,
    mut registry: ResMut<BubbleRegistry>,
    config: Res<BubbleConfig>,
    focus: Res<FocusTarget>,
    current_conv: Res<CurrentConversation>,
    mut bubble_query: Query<&mut InputBubble>,
) {
    // Only in Conversation screen
    if *screen.get() != AppScreen::Conversation {
        return;
    }

    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Skip already consumed keys
        if consumed.0.contains(&event.key_code) {
            continue;
        }

        match event.key_code {
            KeyCode::Space => {
                consumed.0.insert(KeyCode::Space);

                // If there's an active bubble, stash it first
                if registry.active().is_some() {
                    registry.stash_active();
                    info!("Stashed active bubble");
                }

                // Check max stashed limit
                while registry.stashed().len() >= config.max_stashed {
                    // Remove oldest stashed bubble
                    if let Some(oldest_id) = registry.stashed().last().cloned() {
                        if let Some(entity) = registry.get_entity(&oldest_id) {
                            commands.entity(entity).despawn();
                        }
                        registry.unregister(&oldest_id);
                        info!("Removed oldest stashed bubble (max limit)");
                    }
                }

                // Spawn a new bubble
                let bubble = InputBubble::new();
                let id = bubble.id.clone();

                // Capture spawn context
                let context = BubbleSpawnContext {
                    focused_block_id: focus.block_id.clone(),
                    conversation_id: current_conv.id().map(|s| s.to_string()).unwrap_or_default(),
                };

                // Spawn bubble as a root-level entity with absolute positioning
                // (no parent needed - uses ZIndex for layering)
                let entity = commands
                    .spawn((
                        bubble,
                        context,
                        BubbleState::Active,
                        BubblePosition::bottom_center(),
                        MsdfText,
                        MsdfTextAreaConfig::default(),
                        ZIndex(crate::constants::ZLayer::BUBBLE_LAYER),
                    ))
                    .id();

                registry.register(id.clone(), entity);
                registry.set_active(id.clone());

                // Enter Chat input mode
                mode.0 = EditorMode::Input(InputKind::Chat);
                info!("Spawned new bubble: {:?}, entered Chat mode", id);
            }

            KeyCode::Tab => {
                // Cycle through stashed bubbles
                if registry.stashed().is_empty() {
                    continue;
                }

                consumed.0.insert(KeyCode::Tab);

                if let Some(id) = registry.cycle() {
                    // Update state of recalled bubble
                    if let Some(entity) = registry.get_entity(&id) {
                        if let Ok(mut _bubble) = bubble_query.get_mut(entity) {
                            // Update any state needed on recall
                        }
                        // Update BubbleState component
                        commands.entity(entity).insert(BubbleState::Active);
                    }

                    // Enter Chat input mode
                    mode.0 = EditorMode::Input(InputKind::Chat);
                    info!("Recalled bubble: {:?}", id);
                }
            }

            _ => {}
        }
    }
}

/// Handle input to the active bubble.
///
/// Routes keyboard input to the active bubble's InputBubble methods.
pub fn handle_bubble_input(
    mut keyboard: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    consumed: Res<ConsumedModeKeys>,
    registry: Res<BubbleRegistry>,
    mut bubbles: Query<&mut InputBubble, With<BubbleState>>,
) {
    // Only in Input modes
    if !mode.0.accepts_input() {
        return;
    }

    // Get the active bubble
    let Some(active_entity) = registry.active_entity() else {
        return;
    };

    let Ok(mut bubble) = bubbles.get_mut(active_entity) else {
        return;
    };

    // Skip on frame when mode changes
    if mode.is_changed() {
        for _ in keyboard.read() {} // Consume events
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Skip consumed keys
        if consumed.0.contains(&event.key_code) {
            continue;
        }

        // Handle special keys
        match event.key_code {
            KeyCode::Backspace => {
                bubble.backspace();
                continue;
            }
            KeyCode::Delete => {
                bubble.delete();
                continue;
            }
            KeyCode::ArrowLeft => {
                bubble.move_left();
                continue;
            }
            KeyCode::ArrowRight => {
                bubble.move_right();
                continue;
            }
            KeyCode::Home => {
                bubble.move_home();
                continue;
            }
            KeyCode::End => {
                bubble.move_end();
                continue;
            }
            // Enter and Escape handled by separate systems
            KeyCode::Enter | KeyCode::Escape => continue,
            _ => {}
        }

        // Handle text input
        if let Some(ref text) = event.text {
            for c in text.chars() {
                if c.is_control() {
                    continue;
                }
                bubble.insert(&c.to_string());
            }
        }
    }
}

/// Handle Enter to submit the active bubble's content.
///
/// Extracts text from the bubble, fires PromptSubmitted, and despawns.
pub fn handle_bubble_submit(
    mut commands: Commands,
    mut keyboard: MessageReader<KeyboardInput>,
    mut mode: ResMut<CurrentMode>,
    mut registry: ResMut<BubbleRegistry>,
    bubbles: Query<&InputBubble>,
    mut submit_writer: MessageWriter<PromptSubmitted>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    // Only in Input modes
    if !mode.0.accepts_input() {
        return;
    }

    let Some(active_id) = registry.active().cloned() else {
        return;
    };

    let Some(active_entity) = registry.active_entity() else {
        return;
    };

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        if event.key_code == KeyCode::Enter {
            // Shift+Enter for newline (handled by handle_bubble_input via text)
            let shift_held = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
            if shift_held {
                continue;
            }

            // Get bubble content
            let Ok(bubble) = bubbles.get(active_entity) else {
                continue;
            };

            let text = bubble.text().trim().to_string();
            if text.is_empty() {
                continue;
            }

            // Fire submit event
            submit_writer.write(PromptSubmitted { text: text.clone() });
            info!("Bubble submitted: {} chars", text.len());

            // Despawn the bubble
            commands.entity(active_entity).despawn();
            registry.unregister(&active_id);

            // Return to Normal mode
            mode.0 = EditorMode::Normal;
        }
    }
}

/// Handle Esc to stash the active bubble, Cmd+Backspace to dismiss.
pub fn handle_bubble_navigation(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    mut registry: ResMut<BubbleRegistry>,
    bubbles: Query<&InputBubble>,
) {
    // Only in Input modes with an active bubble
    if !mode.0.accepts_input() {
        return;
    }

    let Some(active_id) = registry.active().cloned() else {
        return;
    };

    let Some(active_entity) = registry.active_entity() else {
        return;
    };

    // Cmd+Backspace (or Ctrl+Backspace) to dismiss without submit
    let cmd_held = keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight)
        || keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);

    if cmd_held && keys.just_pressed(KeyCode::Backspace) {
        consumed.0.insert(KeyCode::Backspace);

        // Despawn without submit
        commands.entity(active_entity).despawn();
        registry.unregister(&active_id);

        // Return to Normal mode
        mode.0 = EditorMode::Normal;
        info!("Bubble dismissed (Cmd+Backspace)");
        return;
    }

    // Esc to stash (if bubble has content) or dismiss (if empty)
    if keys.just_pressed(KeyCode::Escape) {
        consumed.0.insert(KeyCode::Escape);

        let Ok(bubble) = bubbles.get(active_entity) else {
            return;
        };

        if bubble.is_empty() {
            // Empty bubble - just dismiss
            commands.entity(active_entity).despawn();
            registry.unregister(&active_id);
            info!("Empty bubble dismissed on Esc");
        } else {
            // Has content - stash it
            registry.stash_active();
            commands.entity(active_entity).insert(BubbleState::Stashed);
            info!("Bubble stashed on Esc");
        }

        // Return to Normal mode
        mode.0 = EditorMode::Normal;
    }
}

/// Initialize MsdfTextBuffer for InputBubble entities.
pub fn init_bubble_buffers(
    mut commands: Commands,
    bubbles: Query<Entity, (With<InputBubble>, With<MsdfText>, Without<MsdfTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in bubbles.iter() {
        let metrics = text_metrics.scaled_cell_metrics();
        let buffer = MsdfTextBuffer::new(&mut font_system, metrics);
        commands.entity(entity).try_insert(buffer);
        info!("Initialized bubble buffer for {:?}", entity);
    }
}

/// Sync InputBubble content to MsdfTextBuffer.
///
/// Runs when InputBubble changes OR when MsdfTextBuffer is first added (initial sync).
pub fn sync_bubble_buffers(
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    mut bubbles: Query<
        (&InputBubble, &BubbleState, &mut MsdfTextBuffer, &mut MsdfTextAreaConfig),
        Or<(Changed<InputBubble>, Added<MsdfTextBuffer>)>,
    >,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (bubble, state, mut buffer, mut config) in bubbles.iter_mut() {
        let text = bubble.text();
        let attrs = cosmic_text::Attrs::new().family(cosmic_text::Family::Name("Noto Sans Mono"));

        // Show placeholder when empty
        let display_text = if text.is_empty() {
            "Type here..."
        } else {
            &text
        };

        buffer.set_text(&mut font_system, display_text, attrs, cosmic_text::Shaping::Advanced);

        // Color based on state
        config.default_color = match state {
            BubbleState::Active => {
                if text.is_empty() {
                    theme.fg_dim
                } else {
                    theme.block_user
                }
            }
            BubbleState::Stashed => theme.fg_dim,
        };
    }
}

/// Layout and position active bubble based on BubblePosition.
pub fn layout_bubble_position(
    windows: Query<&Window>,
    config: Res<BubbleConfig>,
    registry: Res<BubbleRegistry>,
    mut bubbles: Query<(&BubblePosition, &BubbleState, &mut MsdfTextAreaConfig), With<InputBubble>>,
) {
    let (win_width, win_height) = windows
        .iter()
        .next()
        .map(|w| (w.width(), w.height()))
        .unwrap_or((1280.0, 800.0));

    for (position, state, mut text_config) in bubbles.iter_mut() {
        // Only position active bubbles (stashed become pills)
        if *state != BubbleState::Active {
            continue;
        }

        // Calculate position from percentages
        let width = config.default_width;
        let height = config.default_height;
        let x = (win_width * position.x_percent) - (width / 2.0);
        let y = (win_height * position.y_percent) - (height / 2.0);

        // Clamp to screen bounds
        let x = x.max(10.0).min(win_width - width - 10.0);
        let y = y.max(10.0).min(win_height - height - 10.0);

        // Padding inside bubble
        let padding = 16.0;
        text_config.left = x + padding;
        text_config.top = y + padding;
        text_config.scale = 1.0;
        text_config.bounds = crate::text::TextBounds {
            left: (x + padding) as i32,
            top: (y + padding) as i32,
            right: (x + width - padding) as i32,
            bottom: (y + height - padding) as i32,
        };
    }

    // Position stashed pills in bottom-left corner
    let mut pill_y = win_height - 40.0;
    let pill_x = 20.0;

    for stashed_id in registry.stashed() {
        if let Some(entity) = registry.get_entity(stashed_id) {
            if let Ok((_, state, mut text_config)) = bubbles.get_mut(entity) {
                if *state == BubbleState::Stashed {
                    text_config.left = pill_x;
                    text_config.top = pill_y;
                    text_config.bounds = crate::text::TextBounds {
                        left: pill_x as i32,
                        top: pill_y as i32,
                        right: (pill_x + config.pill_width) as i32,
                        bottom: (pill_y + config.pill_height) as i32,
                    };
                    pill_y -= config.pill_height + 8.0;
                }
            }
        }
    }
}

/// Update cursor position for active bubble.
pub fn update_bubble_cursor(
    registry: Res<BubbleRegistry>,
    mode: Res<CurrentMode>,
    entities: Res<EditorEntities>,
    bubbles: Query<(&InputBubble, &MsdfTextAreaConfig), With<BubbleState>>,
    mut cursor_query: Query<(&mut Node, &mut Visibility), With<CursorMarker>>,
    text_metrics: Res<TextMetrics>,
) {
    // Only when we have an active bubble in input mode
    if !mode.0.accepts_input() {
        return;
    }

    let Some(active_entity) = registry.active_entity() else {
        return;
    };

    let Some(cursor_ent) = entities.cursor else {
        return;
    };

    let Ok((bubble, config)) = bubbles.get(active_entity) else {
        return;
    };

    let Ok((mut node, mut visibility)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    // Show cursor
    *visibility = Visibility::Inherited;

    // Calculate cursor position within bubble text
    let text = bubble.text();
    let offset = bubble.cursor.offset.min(text.len());
    let before_cursor = if offset > 0 && offset <= text.len() {
        &text[..offset]
    } else {
        ""
    };
    let row = before_cursor.matches('\n').count();
    let col = before_cursor
        .rfind('\n')
        .map(|pos| offset - pos - 1)
        .unwrap_or(offset);

    let char_width = text_metrics.cell_font_size * MONOSPACE_WIDTH_RATIO;
    let line_height = text_metrics.cell_line_height;
    let x = config.left + (col as f32 * char_width);
    let y = config.top + (row as f32 * line_height);

    node.left = Val::Px(x - 2.0);
    node.top = Val::Px(y);
}

/// Sync bubble visibility based on state and mode.
pub fn sync_bubble_visibility(
    registry: Res<BubbleRegistry>,
    mode: Res<CurrentMode>,
    mut bubbles: Query<(&BubbleState, &mut Visibility), With<InputBubble>>,
) {
    for (state, mut vis) in bubbles.iter_mut() {
        *vis = match state {
            BubbleState::Active => {
                if mode.0.accepts_input() || registry.active().is_some() {
                    Visibility::Inherited
                } else {
                    Visibility::Hidden
                }
            }
            BubbleState::Stashed => {
                // Stashed bubbles shown as small indicators
                Visibility::Inherited
            }
        };
    }
}
