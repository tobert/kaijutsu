//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::components::{
    BlockKind, BlockSnapshot, Cell, CellEditor, CellPosition, CellState,
    ConversationScrollState, CurrentMode, EditorMode, FocusedCell, InputKind, MainCell,
    PromptCell, PromptContainer, PromptSubmitted, ViewingConversation, WorkspaceLayout,
};
use crate::conversation::{ConversationRegistry, CurrentConversation};
use crate::text::{bevy_to_glyphon_color, GlyphonText, SharedFontSystem, TextAreaConfig, GlyphonTextBuffer, TextMetrics};
use crate::ui::state::{AppScreen, InputPosition, InputShadowHeight};
use crate::ui::theme::Theme;

// ============================================================================
// LAYOUT CONSTANTS
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
const INDENT_WIDTH: f32 = 24.0;

/// Vertical spacing between blocks.
const BLOCK_SPACING: f32 = 2.0;

/// Height reserved for role transition headers (e.g., "User", "Assistant").
const ROLE_HEADER_HEIGHT: f32 = 20.0;

/// Spacing between role header and block content.
const ROLE_HEADER_SPACING: f32 = 2.0;

/// Height of the status bar at bottom of window.
const STATUS_BAR_HEIGHT: f32 = 24.0;

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
    }
}

/// Spawn a new cell entity with all required components.
pub fn spawn_cell(
    commands: &mut Commands,
    cell: Cell,
    position: CellPosition,
    initial_text: &str,
) -> Entity {
    commands
        .spawn((
            cell,
            CellEditor::default().with_text(initial_text),
            CellState::new(),
            position,
            // Text rendering components
            GlyphonText,
            TextAreaConfig::default(),
        ))
        .id()
}

/// Keys consumed by mode switching this frame (cleared each frame).
#[derive(Resource, Default)]
pub struct ConsumedModeKeys(pub std::collections::HashSet<KeyCode>);

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
pub fn handle_mode_switch(
    mut key_events: MessageReader<KeyboardInput>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    mut presence: ResMut<InputPresence>,
    prompt_cells: Query<&CellEditor, With<PromptCell>>,
    screen: Res<State<AppScreen>>,
) {
    // Clear consumed keys from last frame
    consumed.0.clear();

    // Mode switching works globally - no focus required.
    // Focus determines which cell receives text input, not mode switching.
    // This allows Space to summon the input from anywhere in the conversation.

    for event in key_events.read() {
        if !event.state.is_pressed() {
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
                // - i enters Chat (docked)
                // - Space enters Chat (overlay)
                // - ` (backtick) enters Shell
                // - : also enters Shell (kaish handles commands natively)
                // - v enters Visual
                match event.key_code {
                    KeyCode::KeyI => {
                        mode.0 = EditorMode::Input(InputKind::Chat);
                        presence.0 = InputPresenceKind::Docked;
                        consumed.0.insert(KeyCode::KeyI);
                        info!("Mode: CHAT, Presence: DOCKED");
                    }
                    KeyCode::Space => {
                        // Space summons the overlay input
                        mode.0 = EditorMode::Input(InputKind::Chat);
                        presence.0 = InputPresenceKind::Overlay;
                        consumed.0.insert(KeyCode::Space);
                        info!("Mode: CHAT, Presence: OVERLAY");
                    }
                    KeyCode::Backquote => {
                        // Backtick enters shell mode (kaish REPL)
                        mode.0 = EditorMode::Input(InputKind::Shell);
                        presence.0 = InputPresenceKind::Docked;
                        consumed.0.insert(KeyCode::Backquote);
                        info!("Mode: SHELL, Presence: DOCKED");
                    }
                    KeyCode::Semicolon if event.text.as_deref() == Some(":") => {
                        // Colon also enters Shell - kaish handles : commands natively
                        mode.0 = EditorMode::Input(InputKind::Shell);
                        presence.0 = InputPresenceKind::Docked;
                        consumed.0.insert(KeyCode::Semicolon);
                        info!("Mode: SHELL (command), Presence: DOCKED");
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

                    // Set presence based on prompt content
                    // If prompt is empty, minimize. If has content, stay docked.
                    let prompt_empty = prompt_cells
                        .iter()
                        .next()
                        .map(|editor| editor.text().trim().is_empty())
                        .unwrap_or(true);

                    if prompt_empty {
                        presence.0 = InputPresenceKind::Minimized;
                        info!("Mode: NORMAL, Presence: MINIMIZED");
                    } else {
                        presence.0 = InputPresenceKind::Docked;
                        info!("Mode: NORMAL, Presence: DOCKED (draft preserved)");
                    }
                }
            }
        }
    }
}

/// Handle keyboard input for the focused cell.
pub fn handle_cell_input(
    mut key_events: MessageReader<KeyboardInput>,
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    consumed: Res<ConsumedModeKeys>,
    mut editors: Query<&mut CellEditor>,
    prompt_cells: Query<Entity, With<PromptCell>>,
) {
    let Some(focused_entity) = focused.0 else {
        return;
    };

    let Ok(mut editor) = editors.get_mut(focused_entity) else {
        return;
    };

    // Check if focused cell is the prompt cell (for Enter handling)
    let is_prompt = prompt_cells.contains(focused_entity);

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
                // For PromptCell, let handle_prompt_submit deal with Enter
                // (Enter submits, Shift+Enter adds newline)
                if is_prompt {
                    continue;
                }
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

/// Initialize GlyphonTextBuffer for cells that don't have one yet.
pub fn init_cell_buffers(
    mut commands: Commands,
    cells_without_buffer: Query<(Entity, &CellEditor), (With<GlyphonText>, Without<GlyphonTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, editor) in cells_without_buffer.iter() {
        // Create a new buffer with DPI-aware metrics
        let metrics = text_metrics.scaled_cell_metrics();
        let mut buffer = GlyphonTextBuffer::new(&mut font_system, metrics);

        // Initialize with current editor text
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(
            &mut font_system,
            &editor.text(),
            &attrs,
            glyphon::Shaping::Advanced,
        );

        commands.entity(entity).insert(buffer);
        info!("Initialized GlyphonTextBuffer for entity {:?}", entity);
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
                // Pretty-print JSON input
                if let Some(ref input) = block.tool_input {
                    if let Ok(pretty) = serde_json::to_string_pretty(input) {
                        output.push_str(&pretty);
                    } else {
                        output.push_str(&input.to_string());
                    }
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
                output.push_str(&block.content);
            }
        }
    }

    output
}

/// Update GlyphonTextBuffer from CellEditor when dirty.
///
/// For cells with content blocks, formats them with visual markers.
/// For plain text cells, uses the text directly.
pub fn sync_cell_buffers(
    mut cells: Query<(&CellEditor, &mut GlyphonTextBuffer), Changed<CellEditor>>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (editor, mut buffer) in cells.iter_mut() {
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);

        // Use block-formatted text if we have blocks, otherwise use raw text
        let display_text = if editor.has_blocks() {
            format_blocks_for_display(&editor.blocks())
        } else {
            editor.text()
        };

        buffer.set_text(
            &mut font_system,
            &display_text,
            &attrs,
            glyphon::Shaping::Advanced,
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

/// Layout the prompt cell at the bottom of the window.
/// Uses full window width minus margins for a wide input area.
pub fn layout_prompt_cell_position(
    mut cells: Query<(&CellState, &mut TextAreaConfig), With<PromptCell>>,
    pos: Res<InputPosition>,
    layout: Res<WorkspaceLayout>,
) {
    // Use InputPosition for positioning (computed from presence/dock/window)
    // Add padding inside the frame for the text
    let padding = 20.0;
    let text_left = pos.x + padding;
    let text_top = pos.y + padding * 0.5;
    let text_width = pos.width - (padding * 2.0);

    for (state, mut config) in cells.iter_mut() {
        let height = state.computed_height.max(layout.prompt_min_height);

        config.left = text_left;
        config.top = text_top;
        config.scale = 1.0;
        config.bounds = glyphon::TextBounds {
            left: text_left as i32,
            top: text_top as i32,
            right: (text_left + text_width) as i32,
            bottom: (text_top + height) as i32,
        };
    }
}

/// Visual indication for focused cell.
pub fn highlight_focused_cell(
    focused: Res<FocusedCell>,
    mut cells: Query<(Entity, &mut TextAreaConfig), With<Cell>>,
    theme: Option<Res<crate::ui::theme::Theme>>,
) {
    let Some(ref theme) = theme else {
        warn_once!("Theme resource unavailable for cell highlighting");
        return;
    };

    for (entity, mut config) in cells.iter_mut() {
        let color = if Some(entity) == focused.0 {
            theme.accent // Brighter for focused
        } else {
            theme.fg_dim
        };
        config.default_color = bevy_to_glyphon_color(color);
    }
}

/// Click to focus a cell.
///
/// FUTURE: This system will evolve to support threaded conversations:
/// - Clicking a BlockCell could focus it for reply (input follows focus)
/// - Input area might "attach" to the focused block or move on-screen
/// - Consider: FocusedCell vs ActiveThread vs ReplyTarget as separate concepts
///
/// For now: Any cell with TextAreaConfig can receive focus.
/// The cursor renders at the focused cell's position.
pub fn click_to_focus(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cells: Query<(Entity, &TextAreaConfig), With<Cell>>,
    mut focused: ResMut<FocusedCell>,
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
            focused.0 = Some(entity);
            return;
        }
    }

    // Clicked outside any cell - clear focus
    focused.0 = None;
}

/// Debug: spawn a test cell on F2.
pub fn debug_spawn_cell(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    cells: Query<&CellPosition, With<Cell>>,
) {
    if keys.just_pressed(KeyCode::F2) {
        // Find next available row (start at 0 if no cells exist)
        let next_row = cells
            .iter()
            .map(|p| p.row)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);

        spawn_cell(
            &mut commands,
            Cell::new(),
            CellPosition::new(next_row),
            "// New cell\nfn main() {\n    println!(\"Hello!\");\n}\n",
        );

        info!("Spawned debug cell at row {}", next_row);
    }
}

/// Toggle collapse state of focused cell or thinking blocks (Tab in Normal mode).
///
/// Behavior:
/// - If the cell has thinking blocks, Tab toggles the first thinking block's collapse state
/// - Otherwise, Tab toggles the whole cell's collapse state
pub fn handle_collapse_toggle(
    mut key_events: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    focused: Res<FocusedCell>,
    mut cells: Query<(&mut CellEditor, &mut CellState)>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(focused_entity) = focused.0 else {
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

/// Resource tracking the cursor entity.
#[derive(Resource, Default)]
pub struct CursorEntity(pub Option<Entity>);

/// Font metrics for cursor positioning.
const CHAR_WIDTH: f32 = 8.4;   // Approximate monospace char width at 14px
const LINE_HEIGHT: f32 = 20.0; // Line height from text config

/// Spawn the cursor entity if it doesn't exist.
pub fn spawn_cursor(
    mut commands: Commands,
    mut cursor_entity: ResMut<CursorEntity>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<crate::ui::theme::Theme>,
) {
    if cursor_entity.0.is_some() {
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

    let entity = commands
        .spawn((
            CursorMarker,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(CHAR_WIDTH + 8.0),  // Slightly larger for bloom
                height: Val::Px(LINE_HEIGHT + 4.0),
                ..default()
            },
            BackgroundColor(Color::NONE),  // Explicit transparent - let shader handle all rendering
            MaterialNode(material),
            ZIndex(20), // Above text, below modals
            Visibility::Hidden, // Start hidden until we have a focused cell
        ))
        .id();

    cursor_entity.0 = Some(entity);
    info!("Spawned cursor entity");
}

/// Update cursor position and visibility based on focused cell and mode.
pub fn update_cursor(
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    cursor_entity: Res<CursorEntity>,
    cells: Query<(&CellEditor, &TextAreaConfig)>,
    mut cursor_query: Query<(&mut Node, &mut Visibility, &MaterialNode<CursorBeamMaterial>), With<CursorMarker>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<crate::ui::theme::Theme>,
) {
    let Some(cursor_ent) = cursor_entity.0 else {
        return;
    };

    let Ok((mut node, mut visibility, material_node)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    // Hide cursor if no cell is focused
    let Some(focused_entity) = focused.0 else {
        *visibility = Visibility::Hidden;
        return;
    };

    let Ok((editor, config)) = cells.get(focused_entity) else {
        *visibility = Visibility::Hidden;
        return;
    };

    // Show cursor
    *visibility = Visibility::Inherited;

    // Calculate cursor position by walking blocks directly
    let (row, col) = cursor_position(editor);

    // Position relative to cell bounds
    let x = config.left + (col as f32 * CHAR_WIDTH);
    let y = config.top + (row as f32 * LINE_HEIGHT);

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
fn cursor_position(editor: &CellEditor) -> (usize, usize) {
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
// PROMPT CELL SYSTEMS
// ============================================================================

/// Resource tracking the prompt cell entity.
#[derive(Resource, Default)]
pub struct PromptCellEntity(pub Option<Entity>);

/// Spawn the prompt cell when the PromptContainer appears.
pub fn spawn_prompt_cell(
    mut commands: Commands,
    mut prompt_entity: ResMut<PromptCellEntity>,
    mut focused: ResMut<FocusedCell>,
    prompt_container: Query<Entity, Added<PromptContainer>>,
    layout: Res<WorkspaceLayout>,
) {
    // Only spawn once when we see the PromptContainer
    if prompt_entity.0.is_some() {
        return;
    }

    // Check if prompt container exists
    if prompt_container.is_empty() {
        return;
    }

    // Create a prompt cell
    // Note: Not parented to container - uses absolute positioning for glyphon
    let cell = Cell::new();
    let cell_id = cell.id.clone();

    let entity = commands
        .spawn((
            cell,
            CellEditor::default().with_text(""),
            CellState {
                computed_height: layout.min_cell_height,
                collapsed: false,
            },
            // Row value is unused - PromptCell marker + Without<PromptCell> filters handle exclusion
            CellPosition::new(0),
            GlyphonText,
            TextAreaConfig::default(),
            PromptCell,
            // Start hidden - sync_prompt_visibility controls based on InputPresence
            // This prevents stray glyphon text when on Dashboard
            Visibility::Hidden,
        ))
        .id();

    prompt_entity.0 = Some(entity);
    focused.0 = Some(entity); // Auto-focus the prompt on startup
    info!("Spawned prompt cell with id {:?}", cell_id.0);
}

/// Handle prompt submission (Enter key in INSERT mode while focused on PromptCell).
///
/// After submission:
/// - Editor is cleared
/// - Mode returns to Normal
/// - Presence set to Minimized (just the chasing line)
pub fn handle_prompt_submit(
    mut key_events: MessageReader<KeyboardInput>,
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    keys: Res<ButtonInput<KeyCode>>,
    mut editors: Query<&mut CellEditor>,
    prompt_cells: Query<Entity, With<PromptCell>>,
    mut submit_events: MessageWriter<PromptSubmitted>,
) {
    // Only handle in Input modes (Chat or Shell)
    if !mode.0.accepts_input() {
        return;
    }

    let Some(focused_entity) = focused.0 else {
        return;
    };

    // Check if focused cell is the prompt cell
    if !prompt_cells.contains(focused_entity) {
        return;
    }

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Enter submits (unless Shift is held for newline)
        if event.key_code == KeyCode::Enter {
            let shift_held = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

            if shift_held {
                // Shift+Enter = newline (handled by normal input)
                continue;
            }

            // Submit the prompt
            let Ok(mut editor) = editors.get_mut(focused_entity) else {
                continue;
            };

            let text = editor.text().trim().to_string();
            if text.is_empty() {
                continue;
            }

            // Send the submit message
            submit_events.write(PromptSubmitted { text: text.clone() });
            info!("Prompt submitted: {:?}", text);

            // Clear the editor (CRDT-tracked)
            editor.clear();

            // Stay in current mode after submit (REPL-style)
            // User can press Escape to return to Normal mode
            info!("Prompt submitted, staying in {:?} mode", mode.0);
        }
    }
}

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
///
/// IMPORTANT: Uses the conversation's document cell_id (not the registry ID)
/// so that BlockInserted events from the server match our document's cell_id.
pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    mode: Res<CurrentMode>,
    current_conv: Res<CurrentConversation>,
    registry: Res<ConversationRegistry>,
    mut scroll_state: ResMut<ConversationScrollState>,
    cmds: Option<Res<crate::connection::ConnectionCommands>>,
) {
    // Get the current conversation ID
    let Some(conv_id) = current_conv.id() else {
        warn!("No current conversation to add message to");
        return;
    };

    // Get the conversation's document cell_id (not the registry ID)
    // This is critical: the server sends BlockInserted events with the document's
    // cell_id, and handle_block_events validates against editor.doc.cell_id()
    let Some(conv) = registry.get(conv_id) else {
        warn!("Conversation {} not in registry", conv_id);
        return;
    };
    let doc_cell_id = conv.doc.cell_id().to_string();

    for event in submit_events.read() {
        if let Some(ref cmds) = cmds {
            match mode.0 {
                EditorMode::Input(InputKind::Shell) => {
                    // Shell mode: execute as kaish command
                    cmds.send(crate::connection::ConnectionCommand::ShellExecute {
                        command: event.text.clone(),
                        cell_id: doc_cell_id.clone(),
                    });
                    info!(
                        "Sent shell command to server (conv={}, cell_id={})",
                        conv_id, doc_cell_id
                    );
                }
                EditorMode::Input(InputKind::Chat) => {
                    // Chat mode: send to LLM
                    cmds.send(crate::connection::ConnectionCommand::Prompt {
                        content: event.text.clone(),
                        model: None, // Use server default
                        cell_id: doc_cell_id.clone(),
                    });
                    info!(
                        "Sent prompt to server (conv={}, cell_id={})",
                        conv_id, doc_cell_id
                    );
                }
                _ => {
                    // Other modes shouldn't submit prompts, but handle gracefully
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

/// Auto-focus the prompt cell when entering Input mode.
/// In conversation UI, input modes always mean typing in the prompt.
pub fn auto_focus_prompt(
    mode: Res<CurrentMode>,
    mut focused: ResMut<FocusedCell>,
    prompt_entity: Res<PromptCellEntity>,
) {
    // Only when entering Input mode
    if !mode.is_changed() || !mode.0.accepts_input() {
        return;
    }

    // Always focus the prompt when entering Input mode
    if let Some(entity) = prompt_entity.0 {
        focused.0 = Some(entity);
        info!("Focused prompt cell for {} mode", mode.0.name());
    }
}

/// Smooth scroll interpolation system.
///
/// Runs every frame to smoothly interpolate scroll position toward target.
///
/// Key insight: In follow mode, we lock directly to bottom (no interpolation).
/// Interpolation is only used for manual scrolling and transitions INTO follow mode.
/// This prevents the "chasing a moving target" stutter during streaming.
pub fn smooth_scroll(
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    // Clear the user_scrolled flag at end of frame
    // (This system runs late in the frame after block events)
    scroll_state.user_scrolled_this_frame = false;

    let max = scroll_state.max_offset();

    // In follow mode, lock directly to bottom - no interpolation needed
    // This is how terminals work: content grows, viewport stays anchored
    if scroll_state.following {
        // Jitter prevention: only update if max changed by at least 1 pixel
        // This prevents sub-pixel oscillation during streaming
        if (max - scroll_state.offset).abs() >= 1.0 {
            scroll_state.offset = max;
            scroll_state.target_offset = max;
        }
        return;
    }

    // Not following: immediate scroll (no smoothing for now)
    scroll_state.clamp_target();
    scroll_state.offset = scroll_state.target_offset;
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

use super::components::{ConversationFocus, FocusedBlockCell};

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
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    block_cells: Query<(Entity, &BlockCell, &BlockCellLayout)>,
    mut focus: ResMut<ConversationFocus>,
    mut scroll_state: ResMut<ConversationScrollState>,
    focused_markers: Query<Entity, With<FocusedBlockCell>>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(main_ent) = main_entity.0 else {
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
    focus.focus(new_block.id.clone());

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
    mut focused_configs: Query<(&BlockCell, &mut TextAreaConfig), With<FocusedBlockCell>>,
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    theme: Res<Theme>,
) {
    // For now, we indicate focus by slightly brightening the text color
    // Future: could add a background highlight or border via a separate UI element

    let Some(main_ent) = main_entity.0 else {
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
            config.default_color = bevy_to_glyphon_color(focused_color);
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
    focus: Res<ConversationFocus>,
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

/// Tracks the expanded block view entity.
#[derive(Resource, Default)]
pub struct ExpandedBlockEntity(pub Option<Entity>);

/// Spawn the ExpandedBlockView when ViewStack enters ExpandedBlock state.
pub fn spawn_expanded_block_view(
    mut commands: Commands,
    view_stack: Res<ViewStack>,
    mut expanded_entity: ResMut<ExpandedBlockEntity>,
    existing_views: Query<Entity, With<ExpandedBlockView>>,
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
) {
    // Check if we need to spawn or despawn
    let should_show = view_stack.has_expanded_block();

    if should_show && expanded_entity.0.is_none() {
        // Spawn the expanded block view
        let Some(block_id) = view_stack.expanded_block_id() else {
            return;
        };

        // Get the block content from MainCell
        let Some(main_ent) = main_entity.0 else {
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
        let mut buffer = GlyphonTextBuffer::new(&mut fs, metrics);

        let color = block_color(block, &theme);
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(&mut fs, &block.content, &attrs, glyphon::Shaping::Advanced);
        drop(fs);

        // Spawn the view entity
        let entity = commands
            .spawn((
                ExpandedBlockView,
                GlyphonText,
                buffer,
                TextAreaConfig {
                    left: 40.0,
                    top: 60.0,  // Leave room for header
                    scale: 1.0,
                    bounds: glyphon::TextBounds {
                        left: 0,
                        top: 0,
                        right: 1200,
                        bottom: 800,
                    },
                    default_color: bevy_to_glyphon_color(color),
                },
                Visibility::Inherited,
                // Store block info for updates
                ExpandedBlockInfo {
                    block_id: block_id.clone(),
                    block_kind: block.kind,
                },
            ))
            .id();

        expanded_entity.0 = Some(entity);
        info!("Spawned ExpandedBlockView for {:?}", block_id);
    } else if !should_show && expanded_entity.0.is_some() {
        // Despawn when leaving ExpandedBlock view
        for entity in existing_views.iter() {
            commands.entity(entity).despawn();
        }
        expanded_entity.0 = None;
        info!("Despawned ExpandedBlockView");
    }
}

/// Stores info about the currently expanded block.
#[derive(Component)]
pub struct ExpandedBlockInfo {
    pub block_id: kaijutsu_crdt::BlockId,
    pub block_kind: BlockKind,
}

/// Update the ExpandedBlockView content when the block changes.
pub fn sync_expanded_block_content(
    view_stack: Res<ViewStack>,
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut expanded_views: Query<
        (&ExpandedBlockInfo, &mut GlyphonTextBuffer, &mut TextAreaConfig),
        With<ExpandedBlockView>,
    >,
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    windows: Query<&Window>,
) {
    if !view_stack.has_expanded_block() {
        return;
    }

    let Some(main_ent) = main_entity.0 else {
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
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(&mut fs, &block.content, &attrs, glyphon::Shaping::Advanced);
        drop(fs);

        // Update color
        let color = block_color(block, &theme);
        config.default_color = bevy_to_glyphon_color(color);

        // Update bounds to fill screen (with padding)
        config.bounds = glyphon::TextBounds {
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

/// Resource tracking the main kernel/shell cell entity.
#[derive(Resource, Default)]
pub struct MainCellEntity(pub Option<Entity>);

/// Spawn the main kernel cell on startup.
///
/// This is the primary workspace cell that displays kernel output, shell interactions,
/// and agent conversations. It fills the space between the header and prompt.
pub fn spawn_main_cell(
    mut commands: Commands,
    mut main_entity: ResMut<MainCellEntity>,
    conversation_container: Query<Entity, Added<super::components::ConversationContainer>>,
) {
    // Only spawn once when we see the ConversationContainer
    if main_entity.0.is_some() {
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

    // NOTE: MainCell does NOT get GlyphonText/TextAreaConfig.
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

    main_entity.0 = Some(entity);
    info!("Spawned main kernel cell with id {:?}", cell_id.0);
}

/// Sync the MainCell's content with the current conversation.
///
/// This system:
/// 1. Checks if there's a current conversation
/// 2. Checks if the conversation's version has changed
/// 3. If changed, rebuilds the MainCell's BlockDocument from conversation blocks
///
/// This provides a simple "copy on change" approach. More sophisticated CRDT
/// sync can be added later for multi-user editing.
pub fn sync_main_cell_to_conversation(
    current_conv: Res<CurrentConversation>,
    registry: Res<ConversationRegistry>,
    main_entity: Res<MainCellEntity>,
    mut main_cell: Query<(&mut CellEditor, Option<&mut ViewingConversation>), With<MainCell>>,
    mut commands: Commands,
) {
    // Need both a current conversation and a main cell
    let Some(conv_id) = current_conv.id() else {
        debug!("sync_main_cell: no current conversation");
        return;
    };
    let Some(entity) = main_entity.0 else {
        debug!("sync_main_cell: no main cell entity");
        return;
    };
    let Some(conv) = registry.get(conv_id) else {
        debug!("sync_main_cell: conversation {} not in registry", conv_id);
        return;
    };

    // Get the main cell's editor and viewing component
    let Ok((mut editor, viewing_opt)) = main_cell.get_mut(entity) else {
        debug!("sync_main_cell: couldn't get main cell editor");
        return;
    };

    // Check if we need to sync
    let conv_version = conv.doc.version();
    let needs_sync = match viewing_opt {
        Some(ref viewing) => {
            // Check if conversation changed or version advanced
            viewing.conversation_id != conv_id || viewing.last_sync_version != conv_version
        }
        None => true, // No viewing component yet, need to initialize
    };

    if !needs_sync {
        return;
    }

    // Rebuild the editor's BlockDocument from the conversation
    // This is a simple approach - replace the entire document
    let agent_id = editor.doc.agent_id().to_string();
    let snapshot = conv.doc.snapshot();

    // Create new document from snapshot
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
            viewing.last_sync_version = conv_version;
        }
        None => {
            commands.entity(entity).insert(ViewingConversation {
                conversation_id: conv_id.to_string(),
                last_sync_version: conv_version,
            });
        }
    }

    info!(
        "Synced MainCell to conversation {} (version {})",
        conv_id, conv_version
    );
}

// ============================================================================
// BLOCK EVENT HANDLING (Server → Client Block Sync)
// ============================================================================

use crate::connection::ConnectionEvent;

/// Handle block events from the server and update the MainCell's BlockDocument.
///
/// This system processes streamed block events (inserted, edited, deleted, etc.)
/// from the server and applies them to the local document for live updates.
///
/// **Frontier-based sync protocol:**
/// - First BlockInserted (or cell_id change) → full sync via `from_oplog()`
/// - Subsequent BlockInserted → incremental merge via `merge_ops_owned()`
/// - BlockTextOps → always incremental merge
///
/// Implements terminal-like auto-scroll: if the user is at the bottom when
/// new content arrives, we stay at the bottom. If they've scrolled up to
/// read history, we don't interrupt them.
pub fn handle_block_events(
    mut events: MessageReader<ConnectionEvent>,
    mut main_cells: Query<&mut CellEditor, With<MainCell>>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut sync_state: ResMut<super::components::DocumentSyncState>,
    layout_gen: Res<super::components::LayoutGeneration>,
) {
    let Ok(mut editor) = main_cells.single_mut() else {
        return;
    };

    // Check if we're at the bottom before processing events (for auto-scroll)
    let was_at_bottom = scroll_state.is_at_bottom();

    for event in events.read() {
        match event {
            // Initial state from server - delegate to SyncManager
            ConnectionEvent::BlockCellInitialState { cell_id, ops, blocks: _ } => {
                match sync_state.apply_initial_state(&mut editor.doc, cell_id, ops) {
                    Ok(result) => {
                        trace!("Initial state sync result: {:?}", result);
                    }
                    Err(e) => {
                        // SyncManager already logged the error
                        trace!("Initial state sync error: {}", e);
                    }
                }
            }

            // Block insertion - delegate to SyncManager
            ConnectionEvent::BlockInserted { cell_id, block, ops } => {
                match sync_state.apply_block_inserted(&mut editor.doc, cell_id, block, ops) {
                    Ok(result) => {
                        trace!("Block insert sync result for {:?}: {:?}", block.id, result);
                    }
                    Err(e) => {
                        // SyncManager already logged the error and reset frontier if needed
                        trace!("Block insert sync error for {:?}: {}", block.id, e);
                    }
                }
            }

            // Text streaming ops - delegate to SyncManager
            ConnectionEvent::BlockTextOps {
                cell_id,
                block_id,
                ops,
            } => {
                match sync_state.apply_text_ops(&mut editor.doc, cell_id, ops) {
                    Ok(result) => {
                        trace!("Text ops sync result for {:?}: {:?}", block_id, result);
                    }
                    Err(e) => {
                        // SyncManager already logged the error and reset frontier if needed
                        trace!("Text ops sync error for {:?}: {}", block_id, e);
                    }
                }
            }

            // Non-CRDT events stay in the system (no sync state changes)
            ConnectionEvent::BlockStatusChanged {
                cell_id,
                block_id,
                status,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                if let Err(e) = editor.doc.set_status(block_id, *status) {
                    warn!("Failed to update block status: {}", e);
                }
            }
            ConnectionEvent::BlockDeleted {
                cell_id,
                block_id,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                if let Err(e) = editor.doc.delete_block(block_id) {
                    warn!("Failed to delete block: {}", e);
                }
            }
            ConnectionEvent::BlockCollapsedChanged {
                cell_id,
                block_id,
                collapsed,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                if let Err(e) = editor.doc.set_collapsed(block_id, *collapsed) {
                    warn!("Failed to update block collapsed state: {}", e);
                }
            }
            ConnectionEvent::BlockMoved {
                cell_id,
                block_id,
                after_id,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                if let Err(e) = editor.doc.move_block(block_id, after_id.as_ref()) {
                    warn!("Failed to move block: {}", e);
                }
            }
            // Ignore other connection events - they're handled elsewhere
            _ => {}
        }
    }

    // Terminal-like auto-scroll: if we were at the bottom before processing
    // events and content changed, enable follow mode to smoothly track new content.
    // If user had scrolled up, we don't interrupt them.
    // IMPORTANT: Don't re-enable following if user explicitly scrolled this frame.
    // Use LayoutGeneration to detect content changes (bumped by sync_block_cell_buffers).
    if was_at_bottom && layout_gen.0 > scroll_state.last_content_gen && !scroll_state.user_scrolled_this_frame {
        scroll_state.start_following();
        scroll_state.last_content_gen = layout_gen.0;
    }
}

// ============================================================================
// BLOCK CELL SYSTEMS (Per-Block UI Rendering)
// ============================================================================

use super::components::{BlockCell, BlockCellContainer, BlockCellLayout};

/// Format a single block for display.
///
/// Returns the formatted text for one block, including visual markers.
pub fn format_single_block(block: &BlockSnapshot) -> String {
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
                if let Ok(pretty) = serde_json::to_string_pretty(input) {
                    output.push_str(&pretty);
                } else {
                    output.push_str(&input.to_string());
                }
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
            block.content.clone()
        }
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
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    _block_cells: Query<(Entity, &BlockCell)>,
) {
    let Some(main_ent) = main_entity.0 else {
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

    // Find blocks to add (in document but not in container)
    for block_id in &current_blocks {
        if !container.contains(block_id) {
            // Spawn new BlockCell
            let entity = commands
                .spawn((
                    BlockCell::new(block_id.clone()),
                    BlockCellLayout::default(),
                    GlyphonText,
                    TextAreaConfig::default(),
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

/// Initialize GlyphonTextBuffers for BlockCells that don't have one.
pub fn init_block_cell_buffers(
    mut commands: Commands,
    block_cells: Query<Entity, (With<BlockCell>, With<GlyphonText>, Without<GlyphonTextBuffer>)>,
    font_system: Res<SharedFontSystem>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in block_cells.iter() {
        let metrics = text_metrics.scaled_cell_metrics();
        let buffer = GlyphonTextBuffer::new(&mut font_system, metrics);
        commands.entity(entity).insert(buffer);
    }
}

/// Sync BlockCell GlyphonTextBuffers with their corresponding block content.
///
/// Only updates cells whose content has changed (tracked via version).
/// When any buffer is updated, bumps LayoutGeneration to trigger re-layout.
/// Also applies block-specific text colors based on BlockKind and Role.
///
/// Note: We don't use Changed<CellEditor> because sync_main_cell_to_conversation
/// mutates editor.doc directly which doesn't trigger Bevy change detection.
/// Instead, we rely on block_cell.last_render_version for dirty tracking.
pub fn sync_block_cell_buffers(
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&mut BlockCell, &mut GlyphonTextBuffer, &mut TextAreaConfig)>,
    font_system: Res<SharedFontSystem>,
    theme: Res<Theme>,
    mut layout_gen: ResMut<super::components::LayoutGeneration>,
) {
    let Some(main_ent) = main_entity.0 else {
        return;
    };

    // Only run when the main cell editor changes
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    let doc_version = editor.version();

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
        let Ok((mut block_cell, mut buffer, mut config)) = block_cells.get_mut(*entity) else {
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
        let mut text = String::new();

        // Prepend role header on transitions
        if let Some(&(is_transition, role)) = is_role_transition.get(&block_cell.block_id) {
            if is_transition {
                let header = match role {
                    kaijutsu_crdt::Role::User => "── USER ──────────────────────",
                    kaijutsu_crdt::Role::Model => "── ASSISTANT ─────────────────",
                    kaijutsu_crdt::Role::System => "── SYSTEM ────────────────────",
                    kaijutsu_crdt::Role::Tool => "── TOOL ──────────────────────",
                };
                text.push_str(header);
                text.push('\n');
            }
        }

        text.push_str(&format_single_block(block));
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(&mut font_system, &text, &attrs, glyphon::Shaping::Advanced);

        // Apply block-specific color based on BlockKind and Role
        let color = block_color(block, &theme);
        config.default_color = bevy_to_glyphon_color(color);

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
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCell, &mut BlockCellLayout, &mut GlyphonTextBuffer)>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    shadow_height: Res<InputShadowHeight>,
    registry: Res<ConversationRegistry>,
    current: Res<CurrentConversation>,
    font_system: Res<SharedFontSystem>,
    windows: Query<&Window>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_layout_gen: Local<u64>,
    mut last_window_size: Local<(f32, f32)>,
) {
    let Some(main_ent) = main_entity.0 else {
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

    // Update visible_height early so smooth_scroll has correct max_offset
    // Use InputShadowHeight (0 when minimized, docked_height when visible)
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - shadow_height.0 - STATUS_BAR_HEIGHT;
    scroll_state.visible_height = visible_bottom - visible_top;

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
    let _ = (&current, &registry); // suppress unused warnings

    for entity in &container.block_cells {
        let Ok((block_cell, mut block_layout, mut buffer)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Check if this is a role transition - if so, add space for role header
        if block_is_role_transition.get(&block_cell.block_id) == Some(&true) {
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
        let line_count = buffer.visual_line_count(&mut font_system, wrap_width);
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

/// Apply layout positions to BlockCell TextAreaConfig for rendering.
///
/// **Performance optimization:** This system tracks the last-applied layout generation
/// and scroll offset. It skips work when neither layout nor scroll has changed.
/// Combined with layout_block_cells optimization, this means scrolling only runs
/// this lightweight position update, not the expensive layout computation.
pub fn apply_block_cell_positions(
    main_entity: Res<MainCellEntity>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut TextAreaConfig), With<BlockCell>>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    shadow_height: Res<InputShadowHeight>,
    windows: Query<&Window>,
    layout_gen: Res<super::components::LayoutGeneration>,
    mut last_applied_gen: Local<u64>,
    mut prev_scroll: Local<f32>,
) {
    let Some(main_ent) = main_entity.0 else {
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

    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // Visible area (use shadow_height, 0 when minimized)
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - shadow_height.0 - STATUS_BAR_HEIGHT;
    let visible_height = visible_bottom - visible_top;
    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);

    // Update scroll state with visible area and clamp offset
    scroll_state.visible_height = visible_height;
    scroll_state.clamp_target();

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
        config.bounds = glyphon::TextBounds {
            left: left as i32,
            top: visible_top.max(content_top) as i32,
            right: (left + width) as i32,
            bottom: visible_bottom.min(block_bottom) as i32,
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

// ============================================================================
// INPUT AREA POSITION SYSTEMS
// ============================================================================

use crate::ui::state::{
    InputBackdrop, InputDock, InputFrame, InputLayer, InputPresence,
    InputPresenceKind, InputShadow,
};

/// Compute the input position based on presence, dock, and window size.
///
/// This is the single source of truth for input area positioning.
/// Runs whenever presence, dock, or window changes.
pub fn compute_input_position(
    presence: Res<InputPresence>,
    dock: Res<InputDock>,
    theme: Res<Theme>,
    window: Query<&Window>,
    mut pos: ResMut<InputPosition>,
) {
    // Only recompute when relevant resources change
    if !presence.is_changed() && !dock.is_changed() && !theme.is_changed() {
        // Window size changes need to be checked
        // (Query change detection handles this implicitly)
    }

    let Ok(win) = window.single() else {
        return;
    };

    let win_width = win.width();
    let win_height = win.height();

    match presence.0 {
        InputPresenceKind::Overlay => {
            // Centered, 60% width (from theme), content-height
            let width = win_width * theme.input_overlay_width_pct;
            pos.x = (win_width - width) * 0.5;
            pos.y = win_height * 0.3; // Upper-center
            pos.width = width;
            pos.height = theme.input_docked_height * 1.2; // Slightly taller in overlay
            pos.show_backdrop = true;
            pos.show_frame = true;
        }
        InputPresenceKind::Docked => {
            // InputDockKind::Bottom - full-width at bottom
            let _ = dock; // Acknowledge dock for future variants
            pos.x = 0.0;
            pos.y = win_height - theme.input_docked_height;
            pos.width = win_width;
            pos.height = theme.input_docked_height;
            pos.show_backdrop = false;
            pos.show_frame = true;
        }
        InputPresenceKind::Minimized => {
            // Full-width thin line at bottom
            pos.x = 0.0;
            pos.y = win_height - theme.input_minimized_height;
            pos.width = win_width;
            pos.height = theme.input_minimized_height;
            pos.show_backdrop = false;
            pos.show_frame = false;
        }
        InputPresenceKind::Hidden => {
            pos.height = 0.0;
            pos.show_backdrop = false;
            pos.show_frame = false;
        }
    }
}

/// Sync InputLayer visibility based on InputPresence.
///
/// The InputLayer is visible when presence is Docked or Overlay.
/// It's hidden when Minimized (only shadow line shows) or Hidden (dashboard).
pub fn sync_input_layer_visibility(
    presence: Res<InputPresence>,
    mut layer_query: Query<&mut Visibility, With<InputLayer>>,
) {
    if !presence.is_changed() {
        return;
    }

    let target = if presence.is_visible() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    for mut vis in layer_query.iter_mut() {
        *vis = target;
    }
}

/// Sync InputBackdrop visibility based on InputPresence.
///
/// The backdrop is only visible in Overlay mode.
pub fn sync_backdrop_visibility(
    presence: Res<InputPresence>,
    mut backdrop_query: Query<&mut Visibility, With<InputBackdrop>>,
) {
    if !presence.is_changed() {
        return;
    }

    let target = if presence.shows_backdrop() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    for mut vis in backdrop_query.iter_mut() {
        *vis = target;
    }
}

/// Apply computed InputPosition to the InputFrame node.
///
/// Updates the InputFrame's position and size based on InputPosition.
pub fn apply_input_position(
    pos: Res<InputPosition>,
    mut frame_query: Query<(&mut Node, &mut Visibility), With<InputFrame>>,
) {
    if !pos.is_changed() {
        return;
    }

    for (mut node, mut vis) in frame_query.iter_mut() {
        // Update position and size
        node.left = Val::Px(pos.x);
        node.top = Val::Px(pos.y);
        node.width = Val::Px(pos.width);
        node.height = Val::Px(pos.height);

        // Frame visibility based on show_frame flag
        *vis = if pos.show_frame {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Sync InputShadow height based on presence.
///
/// When docked, the shadow reserves full docked height.
/// When minimized/hidden, it's just the line height.
pub fn sync_input_shadow_height(
    presence: Res<InputPresence>,
    theme: Res<Theme>,
    mut shadow_query: Query<&mut Node, With<InputShadow>>,
    mut shadow_height: ResMut<InputShadowHeight>,
) {
    if !presence.is_changed() && !theme.is_changed() {
        return;
    }

    let new_height = match presence.0 {
        InputPresenceKind::Docked => theme.input_docked_height,
        InputPresenceKind::Overlay => 0.0,   // Overlay floats, no space reserved
        InputPresenceKind::Minimized => 0.0, // Hidden completely
        InputPresenceKind::Hidden => 0.0,
    };

    shadow_height.0 = new_height;

    for mut node in shadow_query.iter_mut() {
        node.min_height = Val::Px(new_height);
        node.height = Val::Px(new_height);
    }
}

/// Sync PromptCell visibility with InputPresence.
///
/// PromptCell is a root entity (not parented to InputLayer) so it needs
/// its own visibility management for glyphon text extraction. Without this,
/// the glyphon text renders even when the input area is hidden.
pub fn sync_prompt_visibility(
    presence: Res<InputPresence>,
    prompt_entity: Res<PromptCellEntity>,
    mut query: Query<&mut Visibility>,
) {
    if !presence.is_changed() {
        return;
    }

    let Some(entity) = prompt_entity.0 else { return };
    let Ok(mut vis) = query.get_mut(entity) else { return };

    *vis = if presence.is_visible() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
}

/// Sync InputPresence with AppScreen state.
///
/// When transitioning to Dashboard, hide the input.
/// When transitioning to Conversation, show docked (unless already in a valid state).
pub fn sync_presence_with_screen(
    screen: Res<State<AppScreen>>,
    mut presence: ResMut<InputPresence>,
) {
    if !screen.is_changed() {
        return;
    }

    match screen.get() {
        AppScreen::Dashboard => {
            // Hide input when on dashboard
            presence.0 = InputPresenceKind::Hidden;
            info!("AppScreen::Dashboard -> Presence: HIDDEN");
        }
        AppScreen::Conversation => {
            // Show minimized when entering conversation (user can expand with i/Space)
            // Only change if currently hidden - don't override if already docked/overlay
            if matches!(presence.0, InputPresenceKind::Hidden) {
                presence.0 = InputPresenceKind::Minimized;
                info!("AppScreen::Conversation -> Presence: MINIMIZED");
            }
        }
    }
}
