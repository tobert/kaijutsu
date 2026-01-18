//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::components::{
    Block, BlockContent, Cell, CellEditor, CellKind, CellPosition, CellState,
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
fn format_blocks_for_display(blocks: &[&Block]) -> String {
    if blocks.is_empty() {
        return String::new();
    }

    let mut output = String::new();

    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            output.push_str("\n\n");
        }

        match &block.content {
            BlockContent::Thinking { collapsed, .. } => {
                if *collapsed {
                    // Collapsed: show indicator
                    output.push_str("💭 [Thinking collapsed - Tab to expand]");
                } else {
                    // Expanded: show with dimmed header
                    output.push_str("💭 ─── Thinking ───\n");
                    output.push_str(&block.text());
                    output.push_str("\n───────────────────");
                }
            }
            BlockContent::Text { .. } => {
                output.push_str(&block.text());
            }
            BlockContent::ToolUse { name, input, .. } => {
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
            BlockContent::ToolResult {
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
pub fn compute_cell_heights(
    mut cells: Query<(&CellEditor, &mut CellState), Changed<CellEditor>>,
    layout: Res<WorkspaceLayout>,
) {
    for (editor, mut state) in cells.iter_mut() {
        let line_count = if editor.has_blocks() {
            // Count lines from formatted blocks
            format_blocks_for_display(&editor.blocks()).lines().count().max(1)
        } else {
            editor.text().lines().count().max(1)
        };
        state.computed_height = layout.height_for_lines(line_count);
    }
}

/// Layout conversation cells based on grid position and computed heights.
///
/// Uses a two-pass approach to properly accumulate variable cell heights.
/// Applies scroll offset for vertical scrolling.
/// Excludes prompt cell (handled separately).
pub fn layout_cells(
    mut cells: Query<
        (&CellPosition, &CellState, &mut TextAreaConfig),
        (Without<PromptCell>, Without<MainCell>),
    >,
    layout: Res<WorkspaceLayout>,
    windows: Query<&Window>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    use std::collections::BTreeMap;

    // Get window height
    let window_height = windows
        .iter()
        .next()
        .map(|w| w.resolution.height())
        .unwrap_or(800.0);

    // Calculate visible area (between header and prompt)
    let visible_top = layout.workspace_margin_top;
    let visible_bottom = window_height - layout.prompt_area_height;
    let visible_height = visible_bottom - visible_top;

    // Update scroll state with visible area dimensions
    scroll_state.visible_height = visible_height;

    // First pass: collect maximum height for each row
    // Note: PromptCell is already excluded by Without<PromptCell> filter
    let mut row_heights: BTreeMap<u32, f32> = BTreeMap::new();
    for (position, state, _) in cells.iter() {
        let current_max = row_heights.entry(position.row).or_insert(layout.min_cell_height);
        *current_max = current_max.max(state.computed_height);
    }

    // Build cumulative Y offsets for each row (content coordinates, not screen)
    let mut row_offsets: BTreeMap<u32, f32> = BTreeMap::new();
    let mut content_y = 0.0; // Start at 0 in content space

    // Handle sparse rows - iterate through the range
    let max_row = row_heights.keys().max().copied().unwrap_or(0);
    for row in 0..=max_row {
        row_offsets.insert(row, content_y);
        let row_height = row_heights
            .get(&row)
            .copied()
            .unwrap_or(layout.min_cell_height);
        content_y += row_height + layout.cell_margin;
    }

    // Total content height
    let content_height = content_y;
    scroll_state.content_height = content_height;

    // Clamp scroll offset to valid range
    scroll_state.clamp_offset();

    // Get current scroll offset
    let scroll_offset = scroll_state.offset;

    // Second pass: apply positions with scroll offset
    for (position, state, mut config) in cells.iter_mut() {
        let x = layout.workspace_margin_left
            + (position.col as f32 * (layout.cell_width + layout.cell_margin));

        // Content Y position (before scroll)
        let content_y = row_offsets
            .get(&position.row)
            .copied()
            .unwrap_or(0.0);

        // Screen Y position (after scroll offset, relative to visible area)
        let screen_y = visible_top + content_y - scroll_offset;
        let cell_bottom = screen_y + state.computed_height;

        // Check if cell is visible (even partially)
        let is_visible = cell_bottom > visible_top && screen_y < visible_bottom;

        if is_visible {
            // Clip to visible area
            let clipped_top = screen_y.max(visible_top);
            let clipped_bottom = cell_bottom.min(visible_bottom);

            config.left = x;
            config.top = screen_y; // Use actual position for text layout
            config.scale = 1.0;
            config.bounds = glyphon::TextBounds {
                left: x as i32,
                top: clipped_top as i32,
                right: (x + layout.cell_width) as i32,
                bottom: clipped_bottom as i32,
            };
        } else {
            // Cell is off-screen - set zero bounds to skip rendering
            config.left = 0.0;
            config.top = -1000.0; // Off-screen
            config.bounds = glyphon::TextBounds {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
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
            CellPosition::new(0, next_row),
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
                .filter(|b| matches!(b.content, BlockContent::Thinking { .. }))
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
                    .find(|b| matches!(b.content, BlockContent::Thinking { .. }))
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

/// Cache for collapsed parent IDs to avoid rebuilding every frame.
#[derive(Resource, Default)]
pub struct CollapsedParentsCache {
    /// IDs of cells that are currently collapsed
    pub ids: std::collections::HashSet<String>,
}

/// Resource tracking drag state.
#[derive(Resource, Default)]
pub struct DragState {
    /// Entity being dragged, if any.
    pub dragging: Option<Entity>,
    /// Original row before drag.
    pub original_row: u32,
    /// Mouse position when drag started.
    pub start_y: f32,
}

/// Start dragging a cell on mouse down (in Normal mode).
pub fn start_cell_drag(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    mode: Res<CurrentMode>,
    mut drag: ResMut<DragState>,
    layout: Res<WorkspaceLayout>,
    cells: Query<(Entity, &CellPosition, &TextAreaConfig), With<Cell>>,
) {
    if mode.0 != EditorMode::Normal {
        return;
    }

    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }

    let Some(cursor_pos) = windows.iter().next().and_then(|w| w.cursor_position()) else {
        return;
    };

    // Check if clicking on a cell header area for drag
    for (entity, position, config) in cells.iter() {
        let bounds = &config.bounds;
        // Only trigger drag on the header area (configurable height)
        if cursor_pos.x >= bounds.left as f32
            && cursor_pos.x <= bounds.right as f32
            && cursor_pos.y >= bounds.top as f32
            && cursor_pos.y <= (bounds.top as f32 + layout.drag_header_height)
        {
            drag.dragging = Some(entity);
            drag.original_row = position.row;
            drag.start_y = cursor_pos.y;
            info!("Started dragging cell at row {}", position.row);
            return;
        }
    }
}

/// Update cell position while dragging.
pub fn update_cell_drag(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    mut drag: ResMut<DragState>,
    layout: Res<WorkspaceLayout>,
    mut cells: Query<&mut CellPosition, With<Cell>>,
) {
    let Some(dragging_entity) = drag.dragging else {
        return;
    };

    // End drag on mouse release
    if mouse.just_released(MouseButton::Left) {
        if let Ok(pos) = cells.get(dragging_entity) {
            info!("Dropped cell at row {}", pos.row);
        }
        drag.dragging = None;
        return;
    }

    let Some(cursor_pos) = windows.iter().next().and_then(|w| w.cursor_position()) else {
        return;
    };

    // Calculate new row based on drag offset
    let delta_y = cursor_pos.y - drag.start_y;
    let row_height = layout.max_cell_height + layout.cell_margin;
    let row_offset = (delta_y / row_height).round() as i32;

    let new_row = (drag.original_row as i32 + row_offset).max(0) as u32;

    if let Ok(mut position) = cells.get_mut(dragging_entity) {
        if position.row != new_row {
            position.row = new_row;
        }
    }
}

/// Update the collapsed parents cache when CellState changes.
pub fn update_collapsed_cache(
    mut cache: ResMut<CollapsedParentsCache>,
    changed: Query<&Cell, Changed<CellState>>,
    collapsed_cells: Query<(&Cell, &CellState)>,
) {
    // Only rebuild when something changed
    if changed.is_empty() {
        return;
    }

    // Rebuild the cache with current collapsed cell IDs
    cache.ids = collapsed_cells
        .iter()
        .filter(|(_, state)| state.collapsed)
        .map(|(cell, _)| cell.id.0.clone())
        .collect();
}

/// Hide cells whose parent is collapsed.
///
/// Uses the cached set of collapsed parent IDs to avoid rebuilding every frame.
pub fn apply_collapse_visibility(
    cache: Res<CollapsedParentsCache>,
    cells: Query<(Entity, &Cell)>,
    mut configs: Query<&mut TextAreaConfig>,
) {
    // Hide children of collapsed parents
    for (entity, cell) in cells.iter() {
        if let Some(ref parent_id) = cell.parent {
            if cache.ids.contains(&parent_id.0) {
                // Hide this cell by moving it off-screen
                if let Ok(mut config) = configs.get_mut(entity) {
                    config.bounds.left = -10000;
                    config.bounds.right = -9000;
                }
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

    // Calculate cursor position from text index
    let text = editor.text();
    let (row, col) = cursor_row_col(&text, editor.cursor_offset());

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

/// Calculate row and column from byte index in text.
fn cursor_row_col(text: &str, cursor: usize) -> (usize, usize) {
    let before_cursor = &text[..cursor.min(text.len())];
    let row = before_cursor.matches('\n').count();
    let col = before_cursor
        .rfind('\n')
        .map(|pos| cursor - pos - 1)
        .unwrap_or(cursor);
    (row, col)
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
                min_height: 40.0,
                collapsed: false,
            },
            // Row value is unused - PromptCell marker + Without<PromptCell> filters handle exclusion
            CellPosition::new(0, 0),
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

/// Add submitted prompts as blocks in the current conversation.
///
/// Instead of creating separate Cell entities for each message, we now
/// append messages as blocks to the current conversation's BlockDocument.
/// The MainCell will render these blocks.
pub fn handle_prompt_submitted(
    mut submit_events: MessageReader<PromptSubmitted>,
    current_conv: Res<CurrentConversation>,
    mut registry: ResMut<ConversationRegistry>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    // Get the current conversation ID
    let Some(conv_id) = current_conv.id() else {
        warn!("No current conversation to add message to");
        return;
    };

    for event in submit_events.read() {
        // Get the conversation and add the message
        if let Some(conv) = registry.get_mut(conv_id) {
            // Determine the author - use the first user participant or default
            let author = conv
                .participants
                .iter()
                .find(|p| p.is_user())
                .map(|p| p.id.clone())
                .unwrap_or_else(|| format!("user:{}", whoami::username()));

            // Add the message as a text block with author attribution
            if let Some(block_id) = conv.add_text_message(&author, &event.text) {
                info!(
                    "Added user message to conversation {}: block {}",
                    conv_id, block_id
                );

                // Request scroll to bottom
                scroll_state.scroll_to_bottom = true;
            } else {
                warn!("Failed to add message to conversation {}", conv_id);
            }
        } else {
            warn!("Conversation {} not found in registry", conv_id);
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
            scroll_state.scroll_by(SCROLL_SPEED);
        }
        if keys.just_pressed(KeyCode::KeyK) {
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

    let entity = commands
        .spawn((
            cell,
            CellEditor::default().with_text(welcome_text),
            CellState {
                computed_height: 400.0, // Will be updated by layout
                min_height: 200.0,
                collapsed: false,
            },
            CellPosition::new(0, 0),
            GlyphonText,
            TextAreaConfig::default(),
            MainCell,
        ))
        .id();

    main_entity.0 = Some(entity);
    info!("Spawned main kernel cell with id {:?}", cell_id.0);
}

/// Layout the main cell to fill the space between header and prompt.
pub fn layout_main_cell(
    mut cells: Query<(&CellState, &mut TextAreaConfig), With<MainCell>>,
    layout: Res<WorkspaceLayout>,
    windows: Query<&Window>,
) {
    let (window_width, window_height) = windows
        .iter()
        .next()
        .map(|w| (w.resolution.width(), w.resolution.height()))
        .unwrap_or((1280.0, 800.0));

    // Main cell fills from header to prompt area
    let top = layout.workspace_margin_top;
    let bottom = window_height - layout.prompt_area_height;
    let height = bottom - top;

    // Full width minus margins
    let margin = layout.workspace_margin_left;
    let width = window_width - (margin * 2.0);

    for (state, mut config) in cells.iter_mut() {
        let cell_height = height.max(state.min_height);

        config.left = margin;
        config.top = top;
        config.scale = 1.0;
        config.bounds = glyphon::TextBounds {
            left: margin as i32,
            top: top as i32,
            right: (margin + width) as i32,
            bottom: (top + cell_height) as i32,
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
        return;
    };
    let Some(entity) = main_entity.0 else {
        return;
    };
    let Some(conv) = registry.get(conv_id) else {
        return;
    };

    // Get the main cell's editor and viewing component
    let Ok((mut editor, viewing_opt)) = main_cell.get_mut(entity) else {
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
        let len = last_block.text().len();
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

    debug!(
        "Synced MainCell to conversation {} (version {})",
        conv_id, conv_version
    );
}
