//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::components::{
    BlockContentSnapshot, BlockSnapshot, Cell, CellEditor, CellKind, CellPosition, CellState,
    ConversationScrollState, CurrentMode, EditorMode, FocusedCell, MainCell, PromptCell,
    PromptContainer, PromptSubmitted, ViewingConversation, WorkspaceLayout,
};
use crate::conversation::{ConversationRegistry, CurrentConversation};
use crate::text::{GlyphonText, SharedFontSystem, TextAreaConfig, TextBuffer};

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

/// Handle vim-style mode switching.
pub fn handle_mode_switch(
    mut key_events: MessageReader<KeyboardInput>,
    mut mode: ResMut<CurrentMode>,
    mut consumed: ResMut<ConsumedModeKeys>,
    focused: Res<FocusedCell>,
) {
    // Clear consumed keys from last frame
    consumed.0.clear();

    // Only handle mode switches when a cell is focused
    if focused.0.is_none() {
        return;
    }

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match mode.0 {
            EditorMode::Normal => {
                // In normal mode, i enters insert, : enters command, v enters visual
                match event.key_code {
                    KeyCode::KeyI => {
                        mode.0 = EditorMode::Insert;
                        consumed.0.insert(KeyCode::KeyI);
                        info!("Mode: INSERT");
                    }
                    KeyCode::Semicolon if event.text.as_deref() == Some(":") => {
                        mode.0 = EditorMode::Command;
                        consumed.0.insert(KeyCode::Semicolon);
                        info!("Mode: COMMAND");
                    }
                    KeyCode::KeyV => {
                        mode.0 = EditorMode::Visual;
                        consumed.0.insert(KeyCode::KeyV);
                        info!("Mode: VISUAL");
                    }
                    _ => {}
                }
            }
            EditorMode::Insert | EditorMode::Command | EditorMode::Visual => {
                // Escape returns to normal mode
                if event.key_code == KeyCode::Escape {
                    mode.0 = EditorMode::Normal;
                    consumed.0.insert(KeyCode::Escape);
                    info!("Mode: NORMAL");
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
) {
    let Some(focused_entity) = focused.0 else {
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

    // Only handle text input in Insert mode
    if mode.0 != EditorMode::Insert {
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

/// Initialize TextBuffer for cells that don't have one yet.
pub fn init_cell_buffers(
    mut commands: Commands,
    cells_without_buffer: Query<(Entity, &CellEditor), (With<GlyphonText>, Without<TextBuffer>)>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, editor) in cells_without_buffer.iter() {
        // Create a new buffer with default metrics
        let metrics = glyphon::Metrics::new(16.0, 20.0);
        let mut buffer = TextBuffer::new(&mut font_system, metrics);

        // Initialize with current editor text
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(
            &mut font_system,
            &editor.text(),
            &attrs,
            glyphon::Shaping::Advanced,
        );

        commands.entity(entity).insert(buffer);
        info!("Initialized TextBuffer for entity {:?}", entity);
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

        match &block.content {
            BlockContentSnapshot::Thinking { collapsed, .. } => {
                if *collapsed {
                    // Collapsed: show indicator
                    output.push_str("💭 [Thinking collapsed - Tab to expand]");
                } else {
                    // Expanded: show with dimmed header
                    output.push_str("💭 ─── Thinking ───\n");
                    output.push_str(block.content.text());
                    output.push_str("\n───────────────────");
                }
            }
            BlockContentSnapshot::Text { .. } => {
                output.push_str(block.content.text());
            }
            BlockContentSnapshot::ToolUse { name, input, .. } => {
                output.push_str("🔧 Tool: ");
                output.push_str(name);
                output.push('\n');
                // Pretty-print JSON input
                if let Ok(pretty) = serde_json::to_string_pretty(input) {
                    output.push_str(&pretty);
                } else {
                    output.push_str(&input.to_string());
                }
            }
            BlockContentSnapshot::ToolResult {
                content, is_error, ..
            } => {
                if *is_error {
                    output.push_str("❌ Error:\n");
                } else {
                    output.push_str("📤 Result:\n");
                }
                output.push_str(content);
            }
        }
    }

    output
}

/// Update TextBuffer from CellEditor when dirty.
///
/// For cells with content blocks, formats them with visual markers.
/// For plain text cells, uses the text directly.
pub fn sync_cell_buffers(
    mut cells: Query<(&CellEditor, &mut TextBuffer), Changed<CellEditor>>,
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
            // Full height without max cap
            (line_count as f32) * layout.line_height + 24.0
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
    layout: Res<WorkspaceLayout>,
    windows: Query<&Window>,
) {
    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // Position prompt at bottom of window (above status bar)
    let prompt_y = window_height - layout.prompt_bottom_offset;

    // Full width minus margins on both sides
    let margin = layout.workspace_margin_left;
    let prompt_width = window_width - (margin * 2.0);

    for (state, mut config) in cells.iter_mut() {
        let height = state.computed_height.max(layout.prompt_min_height);

        config.left = margin;
        config.top = prompt_y;
        config.scale = 1.0;
        config.bounds = glyphon::TextBounds {
            left: margin as i32,
            top: prompt_y as i32,
            right: (margin + prompt_width) as i32,
            bottom: (prompt_y + height) as i32,
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

        // NOTE: Bevy srgba returns f32 0.0-1.0, glyphon expects u8 0-255
        let srgba = color.to_srgba();
        config.default_color = glyphon::Color::rgba(
            (srgba.red * 255.0) as u8,
            (srgba.green * 255.0) as u8,
            (srgba.blue * 255.0) as u8,
            (srgba.alpha * 255.0) as u8,
        );
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
            Cell::code("rust"),
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
                .filter(|b| matches!(b.content, BlockContentSnapshot::Thinking { .. }))
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
                    .find(|b| matches!(b.content, BlockContentSnapshot::Thinking { .. }))
                    .map(|b| b.content.is_collapsed())
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
) {
    if cursor_entity.0.is_some() {
        return;
    }

    // Wandering spirit cursor
    let color = Vec4::new(1.0, 0.5, 0.75, 0.95); // Hot pink

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
            ZIndex(10), // Above text
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
            EditorMode::Insert => CursorMode::Beam,
            EditorMode::Normal => CursorMode::Block,
            EditorMode::Visual => CursorMode::Block,
            EditorMode::Command => CursorMode::Underline,
        };
        material.time.y = cursor_mode as u8 as f32;

        // Soft, muted colors - aesthetic terminal style
        let color = match mode.0 {
            EditorMode::Insert => Vec4::new(1.0, 0.5, 0.75, 0.95),   // Hot pink 🌸
            EditorMode::Normal => Vec4::new(0.85, 0.92, 1.0, 0.85),  // Soft ice blue
            EditorMode::Visual => Vec4::new(0.95, 0.85, 0.6, 0.9),   // Warm gold
            EditorMode::Command => Vec4::new(0.7, 1.0, 0.8, 0.9),    // Soft mint
        };
        material.color = color;

        // params: x=orb_size, y=intensity, z=wander_speed, w=blink_rate
        material.params = match mode.0 {
            EditorMode::Insert => Vec4::new(0.25, 1.2, 2.0, 0.0),  // Larger orb, faster wander, no blink
            EditorMode::Normal => Vec4::new(0.2, 1.0, 1.5, 0.6),   // Medium orb, gentle blink
            EditorMode::Visual => Vec4::new(0.22, 1.1, 1.8, 0.0),  // Slightly larger, no blink
            EditorMode::Command => Vec4::new(0.18, 0.9, 2.5, 0.8), // Smaller, fast wander
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
        let text = block.content.text();

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

    // Create a prompt cell with UserMessage kind
    // Note: Not parented to container - uses absolute positioning for glyphon
    let cell = Cell::new(CellKind::UserMessage);
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
        ))
        .id();

    prompt_entity.0 = Some(entity);
    focused.0 = Some(entity); // Auto-focus the prompt on startup
    info!("Spawned prompt cell with id {:?}", cell_id.0);
}

/// Handle prompt submission (Enter key in INSERT mode while focused on PromptCell).
pub fn handle_prompt_submit(
    mut key_events: MessageReader<KeyboardInput>,
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    keys: Res<ButtonInput<KeyCode>>,
    mut editors: Query<&mut CellEditor>,
    prompt_cells: Query<Entity, With<PromptCell>>,
    mut submit_events: MessageWriter<PromptSubmitted>,
) {
    // Only handle in Insert mode
    if mode.0 != EditorMode::Insert {
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
        }
    }
}

/// Send submitted prompts to the server-side LLM.
///
/// The server handles:
/// 1. Adding the user message block to the conversation
/// 2. Streaming the LLM response as blocks
/// 3. Broadcasting block changes to all connected clients
///
/// We also add the user message locally for immediate feedback,
/// then let server sync handle any conflicts.
pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    current_conv: Res<CurrentConversation>,
    mut registry: ResMut<ConversationRegistry>,
    mut scroll_state: ResMut<ConversationScrollState>,
    cmds: Option<Res<crate::connection::ConnectionCommands>>,
) {
    // Get the current conversation ID
    let Some(conv_id) = current_conv.id() else {
        warn!("No current conversation to add message to");
        return;
    };

    for event in submit_events.read() {
        // Add user message locally for immediate feedback
        if let Some(conv) = registry.get_mut(conv_id) {
            let author = conv
                .participants
                .iter()
                .find(|p| p.is_user())
                .map(|p| p.id.clone())
                .unwrap_or_else(|| format!("user:{}", whoami::username()));

            if let Some(block_id) = conv.add_text_message(&author, &event.text) {
                info!(
                    "Added user message locally to conversation {}: block {}",
                    conv_id, block_id
                );
                scroll_state.scroll_to_bottom = true;
            }
        }

        // Send prompt to server for LLM processing
        if let Some(ref cmds) = cmds {
            cmds.send(crate::connection::ConnectionCommand::Prompt {
                content: event.text.clone(),
                model: None, // Use server default
                cell_id: conv_id.to_string(),
            });
            info!("Sent prompt to server for conversation {}", conv_id);
        } else {
            warn!("No connection available, prompt not sent to server");
        }
    }
}

/// Auto-focus the prompt cell when entering INSERT mode.
/// In conversation UI, INSERT mode always means typing in the prompt.
pub fn auto_focus_prompt(
    mode: Res<CurrentMode>,
    mut focused: ResMut<FocusedCell>,
    prompt_entity: Res<PromptCellEntity>,
) {
    // Only when entering INSERT mode
    if !mode.is_changed() || mode.0 != EditorMode::Insert {
        return;
    }

    // Always focus the prompt when entering INSERT mode
    if let Some(entity) = prompt_entity.0 {
        focused.0 = Some(entity);
        info!("Focused prompt cell for INSERT mode");
    }
}

/// Scroll conversation to bottom when requested (e.g., after new message).
pub fn scroll_to_bottom(
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    if !scroll_state.scroll_to_bottom {
        return;
    }

    scroll_state.scroll_to_end();
    scroll_state.scroll_to_bottom = false;
}

/// Handle mouse wheel scrolling for the conversation area.
pub fn handle_scroll_input(
    mut scroll_state: ResMut<ConversationScrollState>,
    mut mouse_wheel: MessageReader<MouseWheel>,
    mode: Res<CurrentMode>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    // Scroll speed multiplier
    const SCROLL_SPEED: f32 = 40.0;

    // Handle mouse wheel
    for event in mouse_wheel.read() {
        // Scroll up = negative delta = decrease offset (show earlier content)
        // Scroll down = positive delta = increase offset (show later content)
        let delta = -event.y * SCROLL_SPEED;
        scroll_state.scroll_by(delta);
    }

    // Handle j/k navigation in Normal mode
    if mode.0 == EditorMode::Normal {
        if keys.just_pressed(KeyCode::KeyJ) {
            info!(
                "scroll j: offset={} -> {}, content={}, visible={}",
                scroll_state.offset,
                scroll_state.offset + SCROLL_SPEED,
                scroll_state.content_height,
                scroll_state.visible_height
            );
            scroll_state.scroll_by(SCROLL_SPEED);
        }
        if keys.just_pressed(KeyCode::KeyK) {
            info!(
                "scroll k: offset={} -> {}, content={}, visible={}",
                scroll_state.offset,
                scroll_state.offset - SCROLL_SPEED,
                scroll_state.content_height,
                scroll_state.visible_height
            );
            scroll_state.scroll_by(-SCROLL_SPEED);
        }
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
        if keys.just_pressed(KeyCode::KeyG) {
            if keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight) {
                // Shift+G = go to bottom
                scroll_state.scroll_to_end();
            }
            // Note: gg (double tap) would need state tracking, skip for now
        }
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
    let cell = Cell::new(CellKind::System);
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

/// Layout the main cell to fill the space between header and prompt.
///
/// Applies scroll offset from ConversationScrollState:
/// - `bounds` stays fixed (the visible clipping window)
/// - `top` offsets by negative scroll amount (content moves up)
pub fn layout_main_cell(
    mut cells: Query<(&CellState, &mut TextAreaConfig), With<MainCell>>,
    layout: Res<WorkspaceLayout>,
    windows: Query<&Window>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // Visible area bounds (fixed clipping window)
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - layout.prompt_area_height;
    let visible_height = visible_bottom - visible_top;

    // Full width minus margins
    let margin = layout.workspace_margin_left;
    let width = window_width - (margin * 2.0);

    // Update scroll state with visible area
    scroll_state.visible_height = visible_height;
    scroll_state.clamp_offset();

    let scroll_offset = scroll_state.offset;

    for (_state, mut config) in cells.iter_mut() {
        // Content top position = visible top - scroll offset
        // When scroll_offset = 0, content starts at visible_top
        // When scroll_offset = 100, content starts 100px above visible area (scrolled down)
        let content_top = visible_top - scroll_offset;

        config.left = margin;
        config.top = content_top;
        config.scale = 1.0;

        // Bounds are fixed clipping window (what's visible)
        config.bounds = glyphon::TextBounds {
            left: margin as i32,
            top: visible_top as i32,
            right: (margin + width) as i32,
            bottom: visible_bottom as i32,
        };
    }
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
        let len = last_block.content.text().len();
        editor.cursor = super::components::BlockCursor::at(last_block.id.clone(), len);
    }

    // Mark dirty so rendering updates
    editor.dirty = true;

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
// BLOCK CELL SYSTEMS (Per-Block UI Rendering)
// ============================================================================

use super::components::{BlockCell, BlockCellContainer, BlockCellLayout, TurnCell, TurnCellContainer};

/// Format a single block for display.
///
/// Returns the formatted text for one block, including visual markers.
pub fn format_single_block(block: &BlockSnapshot) -> String {
    match &block.content {
        BlockContentSnapshot::Thinking { collapsed, .. } => {
            if *collapsed {
                "💭 [Thinking collapsed - Tab to expand]".to_string()
            } else {
                format!(
                    "💭 ─── Thinking ───\n{}\n───────────────────",
                    block.content.text()
                )
            }
        }
        BlockContentSnapshot::Text { .. } => block.content.text().to_string(),
        BlockContentSnapshot::ToolUse { name, input, .. } => {
            let mut output = format!("🔧 Tool: {}\n", name);
            if let Ok(pretty) = serde_json::to_string_pretty(input) {
                output.push_str(&pretty);
            } else {
                output.push_str(&input.to_string());
            }
            output
        }
        BlockContentSnapshot::ToolResult {
            content, is_error, ..
        } => {
            if *is_error {
                format!("❌ Error:\n{}", content)
            } else {
                format!("📤 Result:\n{}", content)
            }
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
    block_cells: Query<(Entity, &BlockCell)>,
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

/// Initialize TextBuffers for BlockCells that don't have one.
pub fn init_block_cell_buffers(
    mut commands: Commands,
    block_cells: Query<Entity, (With<BlockCell>, With<GlyphonText>, Without<TextBuffer>)>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in block_cells.iter() {
        let metrics = glyphon::Metrics::new(16.0, 20.0);
        let buffer = TextBuffer::new(&mut font_system, metrics);
        commands.entity(entity).insert(buffer);
    }
}

/// Sync BlockCell TextBuffers with their corresponding block content.
///
/// Only updates cells whose content has changed (tracked via version).
pub fn sync_block_cell_buffers(
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, (With<MainCell>, Changed<CellEditor>)>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&mut BlockCell, &mut TextBuffer)>,
    font_system: Res<SharedFontSystem>,
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

    // Get all blocks for lookup
    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();

    for entity in &container.block_cells {
        let Ok((mut block_cell, mut buffer)) = block_cells.get_mut(*entity) else {
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
        let text = format_single_block(block);
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(&mut font_system, &text, &attrs, glyphon::Shaping::Advanced);

        block_cell.last_render_version = doc_version;
    }
}

/// Layout BlockCells vertically within the conversation area.
///
/// Computes heights and positions for each block, accounting for:
/// - Block content height
/// - Spacing between blocks
/// - Indentation for nested tool results
/// - Space for turn headers before first block of each turn
pub fn layout_block_cells(
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCell, &mut BlockCellLayout, &TextBuffer)>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    mut registry: ResMut<ConversationRegistry>,
    current: Res<CurrentConversation>,
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

    const BLOCK_SPACING: f32 = 16.0;
    const TURN_HEADER_HEIGHT: f32 = 28.0;
    const TURN_HEADER_SPACING: f32 = 8.0;
    let mut y_offset = 0.0;

    // Get blocks for tool_use_id lookup (for nesting)
    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();

    // Build a map of tool_use_id -> ToolUse block_id for nesting
    let mut tool_use_ids: std::collections::HashMap<String, kaijutsu_crdt::BlockId> =
        std::collections::HashMap::new();
    for block in blocks.values() {
        if let BlockContentSnapshot::ToolUse { id, .. } = &block.content {
            tool_use_ids.insert(id.clone(), block.id.clone());
        }
    }

    // Build turn info: map block_id to (turn_index, is_first_in_turn)
    let mut block_turn_info: std::collections::HashMap<kaijutsu_crdt::BlockId, (usize, bool)> =
        std::collections::HashMap::new();
    if let Some(conv_id) = current.id() {
        if let Some(conv) = registry.get_mut(conv_id) {
            for (turn_idx, turn) in conv.turns().iter().enumerate() {
                for (block_idx, block_id) in turn.block_ids.iter().enumerate() {
                    block_turn_info.insert(block_id.clone(), (turn_idx, block_idx == 0));
                }
            }
        }
    }

    for entity in &container.block_cells {
        let Ok((block_cell, mut block_layout, buffer)) = block_cells.get_mut(*entity) else {
            continue;
        };

        // Check if this is the first block of a turn - if so, add space for turn header
        if let Some(&(_, is_first)) = block_turn_info.get(&block_cell.block_id) {
            if is_first {
                y_offset += TURN_HEADER_HEIGHT + TURN_HEADER_SPACING;
            }
        }

        // Determine indentation level
        let indent_level = if let Some(block) = blocks.get(&block_cell.block_id) {
            if let BlockContentSnapshot::ToolResult { tool_use_id, .. } = &block.content {
                // ToolResult nested under ToolUse
                if tool_use_ids.contains_key(tool_use_id) {
                    1
                } else {
                    0
                }
            } else {
                0
            }
        } else {
            0
        };

        // Compute height from buffer line count
        let line_count = buffer.buffer().lines.len().max(1);
        let height = layout.height_for_lines(line_count);

        block_layout.y_offset = y_offset;
        block_layout.height = height;
        block_layout.indent_level = indent_level;

        y_offset += height + BLOCK_SPACING;
    }

    // Update scroll state with total content height
    scroll_state.content_height = y_offset;
}

/// Apply layout positions to BlockCell TextAreaConfig for rendering.
pub fn apply_block_cell_positions(
    main_entity: Res<MainCellEntity>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut TextAreaConfig), With<BlockCell>>,
    layout: Res<WorkspaceLayout>,
    scroll_state: Res<ConversationScrollState>,
    windows: Query<&Window>,
) {
    let Some(main_ent) = main_entity.0 else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // Visible area
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - layout.prompt_area_height;
    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);
    const INDENT_WIDTH: f32 = 32.0;

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

        // Bounds are the fixed visible clipping window
        config.bounds = glyphon::TextBounds {
            left: left as i32,
            top: visible_top as i32,
            right: (left + width) as i32,
            bottom: visible_bottom as i32,
        };
    }
}

// ============================================================================
// TURN HEADER SYSTEMS
// ============================================================================

/// Computed layout for a turn header.
#[derive(Component, Debug, Default)]
pub struct TurnCellLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
    /// Height of the turn header.
    pub height: f32,
}

/// Spawn or update TurnCell entities to match conversation turns.
///
/// Turns are computed from consecutive blocks by the same author.
/// Each turn gets a header showing author name and timestamp.
pub fn spawn_turn_headers(
    mut commands: Commands,
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut turn_containers: Query<&mut TurnCellContainer>,
    mut registry: ResMut<ConversationRegistry>,
    current: Res<CurrentConversation>,
    _existing_turns: Query<(Entity, &TurnCell)>,
) {
    let Some(main_ent) = main_entity.0 else {
        return;
    };

    // Verify main cell exists
    let Ok(_editor) = main_cells.get(main_ent) else {
        return;
    };

    // Get conversation to compute turns
    let Some(conv_id) = current.id() else {
        return;
    };
    let Some(conv) = registry.get_mut(conv_id) else {
        return;
    };

    // Get or create the TurnCellContainer
    let mut container = if let Ok(c) = turn_containers.get_mut(main_ent) {
        c
    } else {
        commands.entity(main_ent).insert(TurnCellContainer::default());
        return; // Will run again next frame with the container
    };

    // Get turns from conversation (cached)
    let turns = conv.turns();

    // For now, simple approach: clear and rebuild if turn count changed
    // (In future, could diff more precisely)
    let current_count = container.turn_cells.len();
    if current_count != turns.len() {
        // Despawn existing turn cells
        for entity in container.turn_cells.drain(..) {
            commands.entity(entity).try_despawn();
        }

        // Spawn new turn cells
        for (idx, turn) in turns.iter().enumerate() {
            let entity = commands
                .spawn((
                    TurnCell::new(
                        turn.author.clone(),
                        turn.display_name.clone(),
                        turn.timestamp,
                        idx,
                    ),
                    TurnCellLayout::default(),
                    GlyphonText,
                    TextAreaConfig::default(),
                ))
                .id();
            container.add(entity);
        }
    }
}

/// Initialize TextBuffers for TurnCells that don't have one.
pub fn init_turn_cell_buffers(
    mut commands: Commands,
    turn_cells: Query<(Entity, &TurnCell), (With<GlyphonText>, Without<TextBuffer>)>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, turn) in turn_cells.iter() {
        let metrics = glyphon::Metrics::new(14.0, 18.0); // Slightly smaller for headers
        let mut buffer = TextBuffer::new(&mut font_system, metrics);

        // Format turn header: "@DisplayName  HH:MM"
        let header_text = format!("@{} · {}", turn.display_name, turn.formatted_time());
        let attrs = glyphon::Attrs::new()
            .family(glyphon::Family::SansSerif)
            .weight(glyphon::Weight::BOLD);
        buffer.set_text(&mut font_system, &header_text, &attrs, glyphon::Shaping::Advanced);

        commands.entity(entity).insert(buffer);
    }
}

/// Layout turn headers interleaved with blocks.
///
/// This needs to coordinate with block layout to position turn headers
/// above their corresponding blocks.
pub fn layout_turn_headers(
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    turn_containers: Query<&TurnCellContainer>,
    block_cells: Query<(&BlockCell, &BlockCellLayout)>,
    mut turn_cells: Query<(&TurnCell, &mut TurnCellLayout)>,
    mut registry: ResMut<ConversationRegistry>,
    current: Res<CurrentConversation>,
) {
    let Some(main_ent) = main_entity.0 else {
        return;
    };

    let Ok(_editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(block_container) = containers.get(main_ent) else {
        return;
    };

    let Ok(turn_container) = turn_containers.get(main_ent) else {
        return;
    };

    let Some(conv_id) = current.id() else {
        return;
    };
    let Some(conv) = registry.get_mut(conv_id) else {
        return;
    };

    // Get turns for block->turn mapping (cached)
    let turns = conv.turns();

    // Map block_id to turn index
    let mut block_to_turn: std::collections::HashMap<kaijutsu_crdt::BlockId, usize> =
        std::collections::HashMap::new();
    for (turn_idx, turn) in turns.iter().enumerate() {
        for block_id in &turn.block_ids {
            block_to_turn.insert(block_id.clone(), turn_idx);
        }
    }

    // Find the first block of each turn and position turn header just above it
    let mut first_block_of_turn: std::collections::HashMap<usize, f32> =
        std::collections::HashMap::new();

    for entity in &block_container.block_cells {
        let Ok((block_cell, block_layout)) = block_cells.get(*entity) else {
            continue;
        };

        if let Some(&turn_idx) = block_to_turn.get(&block_cell.block_id) {
            first_block_of_turn.entry(turn_idx).or_insert(block_layout.y_offset);
        }
    }

    const TURN_HEADER_HEIGHT: f32 = 28.0;

    // Position each turn header
    for entity in &turn_container.turn_cells {
        let Ok((turn_cell, mut turn_layout)) = turn_cells.get_mut(*entity) else {
            continue;
        };

        if let Some(&block_y) = first_block_of_turn.get(&turn_cell.turn_index) {
            // Position header just above the first block of this turn
            // Note: This assumes block layout already accounts for turn header space
            turn_layout.y_offset = block_y - TURN_HEADER_HEIGHT;
            turn_layout.height = TURN_HEADER_HEIGHT;
        }
    }
}

/// Apply layout positions to TurnCell TextAreaConfig for rendering.
pub fn apply_turn_cell_positions(
    main_entity: Res<MainCellEntity>,
    turn_containers: Query<&TurnCellContainer>,
    mut turn_cells: Query<(&TurnCellLayout, &mut TextAreaConfig), With<TurnCell>>,
    layout: Res<WorkspaceLayout>,
    scroll_state: Res<ConversationScrollState>,
    windows: Query<&Window>,
) {
    let Some(main_ent) = main_entity.0 else {
        return;
    };

    let Ok(container) = turn_containers.get(main_ent) else {
        return;
    };

    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - layout.prompt_area_height;
    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);
    let scroll_offset = scroll_state.offset;

    for entity in &container.turn_cells {
        let Ok((turn_layout, mut config)) = turn_cells.get_mut(*entity) else {
            continue;
        };

        let content_top = visible_top + turn_layout.y_offset - scroll_offset;

        config.left = margin;
        config.top = content_top;
        config.scale = 1.0;

        config.bounds = glyphon::TextBounds {
            left: margin as i32,
            top: visible_top as i32,
            right: (margin + base_width) as i32,
            bottom: visible_bottom as i32,
        };
    }
}
