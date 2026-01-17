//! Cell systems for input handling and rendering.

use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;

use super::components::{
    Cell, CellEditor, CellPosition, CellState, CurrentMode, EditorMode, FocusedCell,
    WorkspaceLayout,
};
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
            &editor.text,
            &attrs,
            glyphon::Shaping::Advanced,
        );

        commands.entity(entity).insert(buffer);
        info!("Initialized TextBuffer for entity {:?}", entity);
    }
}

/// Update TextBuffer from CellEditor when dirty.
pub fn sync_cell_buffers(
    mut cells: Query<(&CellEditor, &mut TextBuffer), Changed<CellEditor>>,
    font_system: Res<SharedFontSystem>,
) {
    let Ok(mut font_system) = font_system.0.lock() else {
        return;
    };

    for (editor, mut buffer) in cells.iter_mut() {
        let attrs = glyphon::Attrs::new().family(glyphon::Family::Monospace);
        buffer.set_text(
            &mut font_system,
            &editor.text,
            &attrs,
            glyphon::Shaping::Advanced,
        );
    }
}

/// Compute cell heights based on content.
pub fn compute_cell_heights(
    mut cells: Query<(&CellEditor, &mut CellState), Changed<CellEditor>>,
    layout: Res<WorkspaceLayout>,
) {
    for (editor, mut state) in cells.iter_mut() {
        let line_count = editor.text.lines().count().max(1);
        state.computed_height = layout.height_for_lines(line_count);
    }
}

/// Layout cells based on grid position and computed heights.
///
/// Uses a two-pass approach to properly accumulate variable cell heights.
pub fn layout_cells(
    mut cells: Query<(&CellPosition, &CellState, &mut TextAreaConfig)>,
    layout: Res<WorkspaceLayout>,
) {
    use std::collections::BTreeMap;

    // First pass: collect maximum height for each row
    let mut row_heights: BTreeMap<u32, f32> = BTreeMap::new();
    for (position, state, _) in cells.iter() {
        let current_max = row_heights.entry(position.row).or_insert(layout.min_cell_height);
        *current_max = current_max.max(state.computed_height);
    }

    // Build cumulative Y offsets for each row
    let mut row_offsets: BTreeMap<u32, f32> = BTreeMap::new();
    let mut y_offset = layout.workspace_margin_top;

    // Handle sparse rows - iterate through the range
    let max_row = row_heights.keys().max().copied().unwrap_or(0);
    for row in 0..=max_row {
        row_offsets.insert(row, y_offset);
        let row_height = row_heights
            .get(&row)
            .copied()
            .unwrap_or(layout.min_cell_height);
        y_offset += row_height + layout.cell_margin;
    }

    // Second pass: apply positions using accumulated offsets
    for (position, state, mut config) in cells.iter_mut() {
        let x = layout.workspace_margin_left
            + (position.col as f32 * (layout.cell_width + layout.cell_margin));
        let y = row_offsets
            .get(&position.row)
            .copied()
            .unwrap_or(layout.workspace_margin_top);

        config.left = x;
        config.top = y;
        config.scale = 1.0;
        config.bounds = glyphon::TextBounds {
            left: x as i32,
            top: y as i32,
            right: (x + layout.cell_width) as i32,
            bottom: (y + state.computed_height) as i32,
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

/// Toggle collapse state of focused cell (Tab in Normal mode).
pub fn handle_collapse_toggle(
    mut key_events: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    focused: Res<FocusedCell>,
    mut cells: Query<&mut CellState>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let Some(focused_entity) = focused.0 else {
        return;
    };

    let Ok(mut state) = cells.get_mut(focused_entity) else {
        return;
    };

    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Tab toggles collapse
        if event.key_code == KeyCode::Tab {
            state.collapsed = !state.collapsed;
            info!(
                "Cell collapse: {}",
                if state.collapsed {
                    "collapsed"
                } else {
                    "expanded"
                }
            );
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
    let (row, col) = cursor_row_col(&editor.text, editor.cursor);

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
