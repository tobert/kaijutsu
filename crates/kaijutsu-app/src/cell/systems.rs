//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::components::{
    BlockKind, BlockSnapshot, Cell, CellEditor, CellPosition, CellState,
    ConversationScrollState, CurrentMode, EditorMode, FocusedCell, MainCell, PromptCell,
    PromptContainer, PromptSubmitted, ViewingConversation, WorkspaceLayout,
};
use crate::conversation::{ConversationRegistry, CurrentConversation};
use crate::text::{bevy_to_glyphon_color, GlyphonText, SharedFontSystem, TextAreaConfig, GlyphonTextBuffer};
use crate::ui::state::{AppScreen, InputPosition, InputShadowHeight};

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
                // Chat/Shell modes only make sense in Conversation screen
                // (Dashboard has no input field to route to)
                if *screen.get() != AppScreen::Conversation {
                    continue;
                }

                // In normal mode, i enters chat (docked), Space enters chat (overlay),
                // ` (backtick) enters shell mode, : enters command, v enters visual
                match event.key_code {
                    KeyCode::KeyI => {
                        mode.0 = EditorMode::Chat;
                        presence.0 = InputPresenceKind::Docked;
                        consumed.0.insert(KeyCode::KeyI);
                        info!("Mode: CHAT, Presence: DOCKED");
                    }
                    KeyCode::Space => {
                        // Space summons the overlay input
                        mode.0 = EditorMode::Chat;
                        presence.0 = InputPresenceKind::Overlay;
                        consumed.0.insert(KeyCode::Space);
                        info!("Mode: CHAT, Presence: OVERLAY");
                    }
                    KeyCode::Backquote => {
                        // Backtick enters shell mode (kaish REPL)
                        mode.0 = EditorMode::Shell;
                        presence.0 = InputPresenceKind::Docked;
                        consumed.0.insert(KeyCode::Backquote);
                        info!("Mode: SHELL, Presence: DOCKED");
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
            EditorMode::Chat | EditorMode::Shell | EditorMode::Command | EditorMode::Visual => {
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
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (entity, editor) in cells_without_buffer.iter() {
        // Create a new buffer with default metrics
        let metrics = glyphon::Metrics::new(16.0, 20.0);
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
            EditorMode::Chat | EditorMode::Shell => CursorMode::Beam,
            EditorMode::Normal => CursorMode::Block,
            EditorMode::Visual => CursorMode::Block,
            EditorMode::Command => CursorMode::Underline,
        };
        material.time.y = cursor_mode as u8 as f32;

        // Cursor colors from theme (Chat and Shell share cursor_insert for now)
        let color = match mode.0 {
            EditorMode::Normal => theme.cursor_normal,
            EditorMode::Chat | EditorMode::Shell => theme.cursor_insert,
            EditorMode::Command => theme.cursor_command,
            EditorMode::Visual => theme.cursor_visual,
        };
        material.color = color;

        // params: x=orb_size, y=intensity, z=wander_speed, w=blink_rate
        material.params = match mode.0 {
            EditorMode::Chat | EditorMode::Shell => Vec4::new(0.25, 1.2, 2.0, 0.0),  // Larger orb, faster wander, no blink
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
    // Only handle in Chat or Shell mode (input modes)
    if !matches!(mode.0, EditorMode::Chat | EditorMode::Shell) {
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
                EditorMode::Shell => {
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
                EditorMode::Chat => {
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

/// Auto-focus the prompt cell when entering Chat or Shell mode.
/// In conversation UI, input modes always mean typing in the prompt.
pub fn auto_focus_prompt(
    mode: Res<CurrentMode>,
    mut focused: ResMut<FocusedCell>,
    prompt_entity: Res<PromptCellEntity>,
) {
    // Only when entering Chat or Shell mode
    if !mode.is_changed() || !matches!(mode.0, EditorMode::Chat | EditorMode::Shell) {
        return;
    }

    // Always focus the prompt when entering INSERT mode
    if let Some(entity) = prompt_entity.0 {
        focused.0 = Some(entity);
        info!("Focused prompt cell for INSERT mode");
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
    let max = scroll_state.max_offset();

    // In follow mode, lock directly to bottom - no interpolation needed
    // This is how terminals work: content grows, viewport stays anchored
    if scroll_state.following {
        scroll_state.offset = max;
        scroll_state.target_offset = max;
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
        if keys.just_pressed(KeyCode::KeyG)
            && (keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight)) {
                // Shift+G = go to bottom
                scroll_state.scroll_to_end();
            }
            // Note: gg (double tap) would need state tracking, skip for now
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
// BLOCK EVENT HANDLING (Server → Client Block Sync)
// ============================================================================

use crate::connection::ConnectionEvent;

/// Handle block events from the server and update the MainCell's BlockDocument.
///
/// This system processes streamed block events (inserted, edited, deleted, etc.)
/// from the server and applies them to the local document for live updates.
///
/// Implements terminal-like auto-scroll: if the user is at the bottom when
/// new content arrives, we stay at the bottom. If they've scrolled up to
/// read history, we don't interrupt them.
pub fn handle_block_events(
    mut events: MessageReader<ConnectionEvent>,
    mut main_cells: Query<&mut CellEditor, With<MainCell>>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    let Ok(mut editor) = main_cells.single_mut() else {
        return;
    };

    // Check if we're at the bottom before processing events (for auto-scroll)
    let was_at_bottom = scroll_state.is_at_bottom();

    for event in events.read() {
        match event {
            ConnectionEvent::BlockInserted { cell_id, block } => {
                info!(
                    "Received BlockInserted: cell_id='{}', block_id={:?}, MainCell cell_id='{}'",
                    cell_id,
                    block.id,
                    editor.doc.cell_id()
                );

                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    // Event for different cell - skip (future: route to correct cell)
                    warn!(
                        "Block event for cell_id '{}' but MainCell has '{}', dropping block {:?}",
                        cell_id,
                        editor.doc.cell_id(),
                        block.id
                    );
                    continue;
                }

                // Skip if block already exists (idempotent)
                if editor.doc.get_block_snapshot(&block.id).is_some() {
                    continue;
                }

                // Find insertion point: after parent (if exists), otherwise append at end
                let after_id = if let Some(ref parent_id) = block.parent_id {
                    // Verify parent exists before inserting child
                    if editor.doc.get_block_snapshot(parent_id).is_none() {
                        warn!(
                            "Block {:?} has parent_id {:?} but parent not found - appending at end",
                            block.id, parent_id
                        );
                        editor.doc.blocks_ordered().last().map(|b| b.id.clone())
                    } else {
                        Some(parent_id.clone())
                    }
                } else {
                    // No parent - append after the last block
                    editor.doc.blocks_ordered().last().map(|b| b.id.clone())
                };

                info!(
                    "Inserting block {:?} (kind={:?}, parent={:?}, content_len={}) after {:?}",
                    block.id, block.kind, block.parent_id, block.content.len(), after_id
                );

                match editor.doc.insert_from_snapshot((**block).clone(), after_id.as_ref()) {
                    Ok(id) => {
                        info!("Inserted block {:?} successfully", id);
                        editor.dirty = true;
                    }
                    Err(e) => {
                        error!(
                            "Failed to insert block {:?} after {:?}: {}",
                            block.id, after_id, e
                        );
                    }
                }
            }
            ConnectionEvent::BlockEdited {
                cell_id,
                block_id,
                pos,
                insert,
                delete,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                // Log current block state for debugging sync issues
                let current_len = editor
                    .doc
                    .get_block_snapshot(block_id)
                    .map(|b| b.content.len())
                    .unwrap_or(0);

                match editor.doc.edit_text(block_id, *pos as usize, insert, *delete as usize) {
                    Ok(()) => {
                        editor.dirty = true;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to apply block edit: {} (block {:?} has len={}, edit pos={}, insert_len={}, delete={})",
                            e, block_id, current_len, pos, insert.len(), delete
                        );
                    }
                }
            }
            ConnectionEvent::BlockTextOps {
                cell_id,
                block_id,
                ops,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                // Deserialize and merge CRDT operations
                match serde_json::from_slice::<kaijutsu_crdt::SerializedOpsOwned>(ops) {
                    Ok(serialized_ops) => {
                        match editor.doc.merge_ops_owned(serialized_ops) {
                            Ok(()) => {
                                editor.dirty = true;
                                trace!("Merged CRDT ops for block {:?}", block_id);
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to merge CRDT ops for block {:?}: {}",
                                    block_id, e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize CRDT ops for block {:?}: {}",
                            block_id, e
                        );
                    }
                }
            }
            ConnectionEvent::BlockStatusChanged {
                cell_id,
                block_id,
                status,
            } => {
                // Validate cell ID matches our document
                if cell_id != editor.doc.cell_id() {
                    continue;
                }

                match editor.doc.set_status(block_id, *status) {
                    Ok(()) => {
                        editor.dirty = true;
                    }
                    Err(e) => {
                        warn!("Failed to update block status: {}", e);
                    }
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

                match editor.doc.delete_block(block_id) {
                    Ok(()) => {
                        editor.dirty = true;
                    }
                    Err(e) => {
                        warn!("Failed to delete block: {}", e);
                    }
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

                match editor.doc.set_collapsed(block_id, *collapsed) {
                    Ok(()) => {
                        editor.dirty = true;
                    }
                    Err(e) => {
                        warn!("Failed to update block collapsed state: {}", e);
                    }
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

                match editor.doc.move_block(block_id, after_id.as_ref()) {
                    Ok(()) => {
                        editor.dirty = true;
                    }
                    Err(e) => {
                        warn!("Failed to move block: {}", e);
                    }
                }
            }
            // Ignore other connection events - they're handled elsewhere
            _ => {}
        }
    }

    // Terminal-like auto-scroll: if we were at the bottom before processing
    // events and content changed, enable follow mode to smoothly track new content.
    // If user had scrolled up, we don't interrupt them.
    if was_at_bottom && editor.dirty {
        scroll_state.start_following();
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
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for entity in block_cells.iter() {
        let metrics = glyphon::Metrics::new(16.0, 20.0);
        let buffer = GlyphonTextBuffer::new(&mut font_system, metrics);
        commands.entity(entity).insert(buffer);
    }
}

/// Sync BlockCell GlyphonTextBuffers with their corresponding block content.
///
/// Only updates cells whose content has changed (tracked via version).
pub fn sync_block_cell_buffers(
    main_entity: Res<MainCellEntity>,
    main_cells: Query<&CellEditor, (With<MainCell>, Changed<CellEditor>)>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&mut BlockCell, &mut GlyphonTextBuffer)>,
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
/// - Block content height (using visual line count after wrapping)
/// - Spacing between blocks
/// - Indentation for nested tool results
/// - Space for turn headers before first block of each turn
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

    // Terminal-like tight spacing - room for 1px chrome later
    const BLOCK_SPACING: f32 = 2.0;
    const ROLE_HEADER_HEIGHT: f32 = 20.0; // Single line height
    const ROLE_HEADER_SPACING: f32 = 2.0;
    const INDENT_WIDTH: f32 = 16.0; // Tighter indent
    let mut y_offset = 0.0;

    // Get window size for wrap width and visible height calculation
    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));
    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);

    // Update visible_height early so smooth_scroll has correct max_offset
    // Use InputShadowHeight (0 when minimized, docked_height when visible)
    // Plus status bar height (always 24px)
    const STATUS_BAR_HEIGHT: f32 = 24.0;
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
pub fn apply_block_cell_positions(
    main_entity: Res<MainCellEntity>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut TextAreaConfig), With<BlockCell>>,
    layout: Res<WorkspaceLayout>,
    mut scroll_state: ResMut<ConversationScrollState>,
    shadow_height: Res<InputShadowHeight>,
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

    // Visible area (use shadow_height, 0 when minimized)
    const STATUS_BAR_HEIGHT: f32 = 24.0;
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - shadow_height.0 - STATUS_BAR_HEIGHT;
    let visible_height = visible_bottom - visible_top;
    let margin = layout.workspace_margin_left;
    let base_width = window_width - (margin * 2.0);
    const INDENT_WIDTH: f32 = 32.0;

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
use crate::ui::theme::Theme;

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
