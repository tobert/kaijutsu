use bevy::{input::keyboard::{Key, KeyboardInput}, prelude::*, ui::widget::NodeImageMode};

use super::theme::Theme;

/// Console height options (percentage of window)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ConsoleHeight {
    #[default]
    Quarter = 25,
    Half = 50,
    ThreeQuarter = 75,
    Full = 100,
}

impl ConsoleHeight {
    fn as_percent(&self) -> f32 {
        match self {
            ConsoleHeight::Quarter => 25.0,
            ConsoleHeight::Half => 50.0,
            ConsoleHeight::ThreeQuarter => 75.0,
            ConsoleHeight::Full => 100.0,
        }
    }

    fn cycle(&self) -> Self {
        match self {
            ConsoleHeight::Quarter => ConsoleHeight::Half,
            ConsoleHeight::Half => ConsoleHeight::ThreeQuarter,
            ConsoleHeight::ThreeQuarter => ConsoleHeight::Full,
            ConsoleHeight::Full => ConsoleHeight::Quarter,
        }
    }
}

/// Resource tracking console state
#[derive(Resource)]
pub struct ConsoleState {
    pub visible: bool,
    pub height: ConsoleHeight,
    pub input: String,
    pub history: Vec<ConsoleLine>,
    pub command_history: Vec<String>,
    pub history_index: Option<usize>,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            visible: false,
            height: ConsoleHeight::Quarter,
            input: String::new(),
            history: vec![
                ConsoleLine::system("【kaish】 Console ready."),
                ConsoleLine::system("Type 'help' for commands. Press ` to close."),
            ],
            command_history: Vec::new(),
            history_index: None,
        }
    }
}

/// A line in the console output
#[derive(Clone)]
pub struct ConsoleLine {
    pub text: String,
    pub line_type: LineType,
}

#[derive(Clone, Copy, PartialEq)]
pub enum LineType {
    Input,
    Output,
    Error,
    System,
}

impl ConsoleLine {
    pub fn input(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            line_type: LineType::Input,
        }
    }

    pub fn output(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            line_type: LineType::Output,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            line_type: LineType::Error,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            line_type: LineType::System,
        }
    }
}

/// Marker for the console panel
#[derive(Component)]
pub struct ConsolePanel;

/// Marker for the console output text
#[derive(Component)]
pub struct ConsoleOutputText;

/// Marker for the console input display
#[derive(Component)]
pub struct ConsoleInputDisplay;

/// Spawn the console panel (hidden by default)
pub fn setup_console(mut commands: Commands, theme: Res<Theme>, asset_server: Res<AssetServer>) {
    // Load the console frame image
    let frame_image: Handle<Image> = asset_server.load("ui/console-frame-border.png");

    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(0.0),
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                height: Val::Percent(25.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.02, 0.05, 0.95)),
            Visibility::Hidden,
            ConsolePanel,
        ))
        .with_children(|console| {
            // Content container (inside the frame)
            console
                .spawn(Node {
                    flex_direction: FlexDirection::Column,
                    flex_grow: 1.0,
                    padding: UiRect::all(Val::Px(24.0)), // Extra padding for frame border
                    ..default()
                })
                .with_children(|content| {
                    // Header
                    content.spawn((
                        Text::new("【kaish】 /room/lobby"),
                        TextFont {
                            font_size: 14.0,
                            ..default()
                        },
                        TextColor(theme.accent),
                    ));

                    // Output scrollback area
                    content.spawn((
                        Node {
                            flex_grow: 1.0,
                            margin: UiRect::vertical(Val::Px(8.0)),
                            overflow: Overflow::clip_y(),
                            ..default()
                        },
                        Text::new(""),
                        TextFont {
                            font_size: 13.0,
                            ..default()
                        },
                        TextColor(theme.fg_dim),
                        ConsoleOutputText,
                    ));

                    // Input line
                    content
                        .spawn(Node {
                            flex_direction: FlexDirection::Row,
                            align_items: AlignItems::Center,
                            ..default()
                        })
                        .with_children(|input_row| {
                            input_row.spawn((
                                Text::new("> "),
                                TextFont {
                                    font_size: 14.0,
                                    ..default()
                                },
                                TextColor(theme.accent),
                            ));
                            input_row.spawn((
                                Text::new("_"),
                                TextFont {
                                    font_size: 14.0,
                                    ..default()
                                },
                                TextColor(theme.fg),
                                ConsoleInputDisplay,
                            ));
                        });
                });

            // Frame overlay with 9-slice (spawned last = renders on top)
            console.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    right: Val::Px(0.0),
                    bottom: Val::Px(0.0),
                    ..default()
                },
                ImageNode {
                    image: frame_image,
                    image_mode: NodeImageMode::Sliced(TextureSlicer {
                        // console-frame-border.png: 5695x1623, border ~150px
                        border: BorderRect::all(150.0),
                        center_scale_mode: SliceScaleMode::Stretch,
                        sides_scale_mode: SliceScaleMode::Stretch,
                        max_corner_scale: 1.0,
                    }),
                    ..default()
                },
                Pickable::IGNORE,
            ));
        });
}

/// Toggle console visibility with backtick
pub fn toggle_console(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<ConsoleState>,
    mut query: Query<(&mut Visibility, &mut Node), With<ConsolePanel>>,
) {
    // Backtick toggles visibility
    if keys.just_pressed(KeyCode::Backquote) {
        state.visible = !state.visible;

        for (mut vis, mut node) in &mut query {
            *vis = if state.visible {
                Visibility::Visible
            } else {
                Visibility::Hidden
            };
            // Update height
            node.height = Val::Percent(state.height.as_percent());
        }
    }

    // Ctrl+Backtick cycles height (when visible)
    if state.visible
        && keys.just_pressed(KeyCode::Backquote)
        && keys.pressed(KeyCode::ControlLeft)
    {
        state.height = state.height.cycle();
        for (_, mut node) in &mut query {
            node.height = Val::Percent(state.height.as_percent());
        }
    }
}

/// Handle keyboard input when console is visible
pub fn handle_console_input(
    mut keyboard_events: MessageReader<KeyboardInput>,
    mut state: ResMut<ConsoleState>,
) {
    if !state.visible {
        return;
    }

    for event in keyboard_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match (&event.logical_key, &event.text) {
            // Enter executes command
            (Key::Enter, _) => {
                let input = std::mem::take(&mut state.input);
                if !input.is_empty() {
                    execute_command(&mut state, &input);
                }
            }
            // Backspace removes last character
            (Key::Backspace, _) => {
                state.input.pop();
            }
            // Up arrow for command history
            (Key::ArrowUp, _) => {
                if !state.command_history.is_empty() {
                    let idx = state.history_index.map(|i| i.saturating_sub(1)).unwrap_or(
                        state.command_history.len().saturating_sub(1),
                    );
                    state.history_index = Some(idx);
                    state.input = state.command_history[idx].clone();
                }
            }
            // Down arrow for command history
            (Key::ArrowDown, _) => {
                if let Some(idx) = state.history_index {
                    if idx + 1 < state.command_history.len() {
                        state.history_index = Some(idx + 1);
                        state.input = state.command_history[idx + 1].clone();
                    } else {
                        state.history_index = None;
                        state.input.clear();
                    }
                }
            }
            // Regular text input
            (_, Some(text)) => {
                // Skip backtick (toggle key)
                if text != "`" {
                    state.input.push_str(text);
                }
            }
            _ => {}
        }
    }
}

/// Execute a console command
fn execute_command(state: &mut ConsoleState, input: &str) {
    // Add to history display
    state.history.push(ConsoleLine::input(format!("> {}", input)));

    // Add to command history (for up/down recall)
    state.command_history.push(input.to_string());
    state.history_index = None;

    // Parse and execute
    let parts: Vec<&str> = input.trim().split_whitespace().collect();
    let response = match parts.as_slice() {
        ["help"] => ConsoleLine::output(
            "Commands: help, ls, pwd, echo <text>, clear, room <name>",
        ),
        ["ls"] => ConsoleLine::output("Cargo.toml  src/  docs/  assets/  README.md"),
        ["pwd"] => ConsoleLine::output("/room/lobby/worktree"),
        ["clear"] => {
            state.history.clear();
            return;
        }
        ["echo", rest @ ..] => ConsoleLine::output(rest.join(" ")),
        ["room", name] => ConsoleLine::system(format!("Switching to room: {}", name)),
        ["room"] => ConsoleLine::error("Usage: room <name>"),
        [] => return,
        [cmd, ..] => ConsoleLine::error(format!("Unknown command: {}", cmd)),
    };

    state.history.push(response);

    // Keep history bounded
    while state.history.len() > 100 {
        state.history.remove(0);
    }
}

/// Update console display
pub fn update_console_display(
    state: Res<ConsoleState>,
    mut input_query: Query<&mut Text, With<ConsoleInputDisplay>>,
    mut output_query: Query<&mut Text, (With<ConsoleOutputText>, Without<ConsoleInputDisplay>)>,
) {
    if !state.is_changed() {
        return;
    }

    // Update input display
    for mut text in &mut input_query {
        **text = if state.input.is_empty() {
            "_".to_string()
        } else {
            format!("{}▏", state.input)
        };
    }

    // Update output display - build all lines into one string
    // (per-line coloring would require TextSpan, which we'll add later)
    let output_text: String = state
        .history
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    for mut text in &mut output_query {
        **text = output_text.clone();
    }
}
