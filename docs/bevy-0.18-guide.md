# Bevy 0.18 API Guide

*Discoveries made while building Kaijutsu*

This guide documents API changes and patterns discovered while working with Bevy 0.18. It serves as a quick reference for common patterns.

## Event System → Message System

Bevy 0.18 renamed the event system to "messages":

| Old (0.14-0.17) | New (0.18) |
|-----------------|------------|
| `#[derive(Event)]` | `#[derive(Message)]` |
| `EventReader<T>` | `MessageReader<T>` |
| `EventWriter<T>` | `MessageWriter<T>` |
| `events.send(x)` | `messages.write(x)` |
| `app.add_event::<T>()` | `app.add_message::<T>()` |

### Example

```rust
// Define a message (was Event)
#[derive(Message)]
pub struct ChatMessage {
    pub sender: String,
    pub content: String,
}

// Register in app (required!)
app.add_message::<ChatMessage>()

// Send messages
fn send_message(mut writer: MessageWriter<ChatMessage>) {
    writer.write(ChatMessage {
        sender: "amy".to_string(),
        content: "Hello!".to_string(),
    });
}

// Receive messages
fn receive_messages(mut reader: MessageReader<ChatMessage>) {
    for msg in reader.read() {
        println!("{}: {}", msg.sender, msg.content);
    }
}
```

## UI Hierarchy

### ChildSpawnerCommands

The type for the closure parameter in `with_children()` changed:

| Old | New |
|-----|-----|
| `ChildBuilder` | `ChildSpawnerCommands` |

Import from `bevy::ecs::hierarchy::ChildSpawnerCommands`.

```rust
use bevy::{ecs::hierarchy::ChildSpawnerCommands, prelude::*};

fn build_ui(parent: &mut ChildSpawnerCommands) {
    parent.spawn((
        Node { /* ... */ },
        BackgroundColor(Color::BLACK),
    ));
}
```

### BorderColor

`BorderColor` now has per-side colors (like CSS):

```rust
// Old - tuple struct
BorderColor(Color::WHITE)

// New - use ::all() for uniform color
BorderColor::all(Color::WHITE)

// Or per-side
BorderColor {
    top: Color::RED,
    right: Color::GREEN,
    bottom: Color::BLUE,
    left: Color::YELLOW,
}
```

## Window Resolution

`WindowResolution` accepts `(u32, u32)`, not `(f64, f64)`:

```rust
// Old
resolution: (1280., 800.).into()

// New
resolution: (1280, 800).into()
```

## Query Changes

### Single Entity Queries

```rust
// Old
query.get_single()

// New - returns Result
query.single()
```

## Keyboard Input

Keyboard input uses `MessageReader<KeyboardInput>`:

```rust
use bevy::input::keyboard::{Key, KeyboardInput};

fn handle_input(mut keyboard: MessageReader<KeyboardInput>) {
    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match (&event.logical_key, &event.text) {
            (Key::Enter, _) => { /* handle enter */ }
            (Key::Backspace, _) => { /* handle backspace */ }
            (_, Some(text)) => { /* handle text input */ }
            _ => {}
        }
    }
}
```

## IME (Input Method Editor)

For CJK and other complex input methods:

```rust
use bevy::window::Ime;

fn handle_ime(mut ime: MessageReader<Ime>) {
    for event in ime.read() {
        match event {
            Ime::Preedit { value, cursor, .. } => {
                // Composing text (e.g., typing Japanese)
                if cursor.is_some() {
                    // Show preedit text
                }
            }
            Ime::Commit { value, .. } => {
                // Final committed text
            }
            Ime::Enabled { .. } => { /* IME activated */ }
            Ime::Disabled { .. } => { /* IME deactivated */ }
            _ => {}
        }
    }
}
```

## States

States work similarly but use `init_state`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, States, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Command,
}

// In app setup
app.init_state::<Mode>()

// In systems
fn handle_input(
    current: Res<State<Mode>>,
    mut next: ResMut<NextState<Mode>>,
) {
    if *current.get() == Mode::Normal {
        next.set(Mode::Insert);
    }
}
```

## Resources

```rust
#[derive(Resource, Default)]
pub struct MyResource {
    pub value: i32,
}

// In app setup
app.init_resource::<MyResource>()

// In systems
fn use_resource(res: Res<MyResource>) { /* read */ }
fn modify_resource(mut res: ResMut<MyResource>) { /* write */ }
```

## Common Patterns

### Text with Font

```rust
commands.spawn((
    Text::new("Hello"),
    TextFont {
        font_size: 20.0,
        ..default()
    },
    TextColor(Color::WHITE),
));
```

### Flexbox Layout

```rust
commands.spawn((
    Node {
        width: Val::Percent(100.0),
        height: Val::Px(50.0),
        flex_direction: FlexDirection::Row,
        align_items: AlignItems::Center,
        justify_content: JustifyContent::SpaceBetween,
        padding: UiRect::all(Val::Px(16.0)),
        ..default()
    },
    BackgroundColor(Color::srgb(0.1, 0.1, 0.1)),
));
```

## References

- [Bevy 0.18 source](~/src/bevy)
- [Bevy examples](~/src/bevy/examples/)
- [Text input example](~/src/bevy/examples/input/text_input.rs)
- [Message example](~/src/bevy/examples/ecs/message.rs)
