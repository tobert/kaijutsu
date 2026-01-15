# Kaijutsu Bootstrap Plan

*Last updated: 2026-01-15*

## Goal

Get a basic Bevy shell running that demonstrates:
1. The sacred input bar (always visible, mode-aware)
2. Context view area (placeholder for DAG blocks)
3. Sidebar with room/agent/equipment sections
4. Mode switching (Normal/Insert/Command)

**No server connection yet** - pure client-side proof of concept.

## MVP Checklist

### Must Have
- [ ] Bevy 0.18 app scaffold
- [ ] Window with title "会術 Kaijutsu"
- [ ] Fixed layout: title bar, sidebar, context area, input bar
- [ ] Mode indicator (Normal/Insert/Command)
- [ ] Basic keyboard handling (`i` → Insert, `Esc` → Normal, `:` → Command)
- [ ] Isekai theme (dark bg, colored borders, accent colors)

### Nice to Have (Phase 1.5)
- [ ] Input field accepts text in Insert mode
- [ ] Echo typed text to context area
- [ ] Directional navigation (`j/k`) in context area
- [ ] `` ` `` shows "console coming soon" placeholder

### Deferred
- Server connection (Phase 2)
- Real DAG block rendering (Phase 2)
- Quake console (Phase 3)

## Project Setup

### Cargo.toml

```toml
[package]
name = "kaijutsu"
version = "0.1.0"
edition = "2024"

[dependencies]
bevy = { version = "0.18", features = ["default"] }
```

### Directory Structure

```
kaijutsu/
├── Cargo.toml
├── CLAUDE.md
├── docs/
│   ├── 01-architecture.md
│   ├── 02-ui-system.md
│   ├── 03-bootstrap.md       # This file
│   └── 04-kaish-console.md
├── src/
│   ├── main.rs               # Entry point, plugins
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── shell.rs          # Main layout
│   │   ├── input.rs          # Sacred input bar
│   │   ├── context.rs        # Context view (DAG blocks)
│   │   ├── sidebar.rs        # Rooms, agents, equipment
│   │   └── theme.rs          # Colors, styles
│   └── state/
│       ├── mod.rs
│       └── mode.rs           # Mode state machine
└── assets/
    └── fonts/
        └── .gitkeep
```

## Implementation

### Step 1: Entry Point

```rust
// src/main.rs
use bevy::prelude::*;

mod ui;
mod state;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "会術 Kaijutsu".into(),
                resolution: (1280., 800.).into(),
                ..default()
            }),
            ..default()
        }))
        .init_state::<state::mode::Mode>()
        .init_resource::<ui::theme::Theme>()
        .add_systems(Startup, ui::shell::setup)
        .add_systems(Update, (
            state::mode::handle_mode_input,
            ui::shell::update_mode_indicator,
        ))
        .run();
}
```

### Step 2: Mode State

```rust
// src/state/mode.rs
use bevy::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, States, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Command,
}

impl Mode {
    pub fn indicator(&self) -> &'static str {
        match self {
            Mode::Normal => "[N]",
            Mode::Insert => "[I]",
            Mode::Command => "[:]",
        }
    }
}

pub fn handle_mode_input(
    keys: Res<ButtonInput<KeyCode>>,
    current: Res<State<Mode>>,
    mut next: ResMut<NextState<Mode>>,
) {
    match current.get() {
        Mode::Normal => {
            if keys.just_pressed(KeyCode::KeyI) {
                next.set(Mode::Insert);
            }
            if keys.just_pressed(KeyCode::Semicolon) && keys.pressed(KeyCode::ShiftLeft) {
                next.set(Mode::Command);
            }
        }
        Mode::Insert | Mode::Command => {
            if keys.just_pressed(KeyCode::Escape) {
                next.set(Mode::Normal);
            }
        }
    }
}
```

### Step 3: Theme

```rust
// src/ui/theme.rs
use bevy::prelude::*;

#[derive(Resource)]
pub struct Theme {
    pub bg: Color,
    pub panel_bg: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub accent2: Color,
    pub border: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::srgb(0.05, 0.07, 0.09),
            panel_bg: Color::srgba(0.05, 0.07, 0.09, 0.9),
            fg: Color::srgb(0.79, 0.82, 0.84),
            fg_dim: Color::srgb(0.5, 0.5, 0.5),
            accent: Color::srgb(0.34, 0.65, 1.0),
            accent2: Color::srgb(0.97, 0.47, 0.73),
            border: Color::srgb(0.19, 0.21, 0.24),
        }
    }
}
```

### Step 4: Shell Layout

```rust
// src/ui/shell.rs
use bevy::prelude::*;
use super::theme::Theme;
use crate::state::mode::Mode;

#[derive(Component)]
pub struct ModeIndicator;

pub fn setup(mut commands: Commands, theme: Res<Theme>) {
    commands.spawn(Camera2d);

    // Root container
    commands.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            ..default()
        },
        BackgroundColor(theme.bg),
    )).with_children(|root| {
        // Title bar
        title_bar(root, &theme);

        // Middle: sidebar + context
        root.spawn(Node {
            width: Val::Percent(100.0),
            flex_grow: 1.0,
            flex_direction: FlexDirection::Row,
            ..default()
        }).with_children(|middle| {
            sidebar(middle, &theme);
            context_area(middle, &theme);
        });

        // Input bar (SACRED)
        input_bar(root, &theme);
    });
}

fn title_bar(parent: &mut ChildBuilder, theme: &Theme) {
    parent.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Px(40.0),
            padding: UiRect::horizontal(Val::Px(16.0)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            border: UiRect::bottom(Val::Px(2.0)),
            ..default()
        },
        BorderColor(theme.border),
        BackgroundColor(theme.panel_bg),
    )).with_children(|bar| {
        bar.spawn((
            Text::new("【会術】 Kaijutsu"),
            TextFont { font_size: 20.0, ..default() },
            TextColor(theme.accent),
        ));
        bar.spawn((
            Text::new("▣ room: lobby"),
            TextFont { font_size: 14.0, ..default() },
            TextColor(theme.fg),
        ));
    });
}

fn sidebar(parent: &mut ChildBuilder, theme: &Theme) {
    parent.spawn((
        Node {
            width: Val::Px(180.0),
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(12.0)),
            border: UiRect::right(Val::Px(2.0)),
            row_gap: Val::Px(16.0),
            ..default()
        },
        BorderColor(theme.border),
        BackgroundColor(theme.panel_bg),
    )).with_children(|side| {
        sidebar_section(side, theme, "ROOMS", &["> lobby", "  dev", "  ops"]);
        sidebar_section(side, theme, "AGENTS", &["◉ opus", "◉ haiku", "○ local"]);
        sidebar_section(side, theme, "EQUIP", &["filesystem", "web_search"]);
    });
}

fn sidebar_section(parent: &mut ChildBuilder, theme: &Theme, title: &str, items: &[&str]) {
    parent.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: Val::Px(4.0),
        ..default()
    }).with_children(|section| {
        section.spawn((
            Text::new(title),
            TextFont { font_size: 12.0, ..default() },
            TextColor(theme.accent2),
        ));
        for item in items {
            section.spawn((
                Text::new(*item),
                TextFont { font_size: 14.0, ..default() },
                TextColor(theme.fg),
            ));
        }
    });
}

fn context_area(parent: &mut ChildBuilder, theme: &Theme) {
    parent.spawn((
        Node {
            flex_grow: 1.0,
            height: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(16.0)),
            row_gap: Val::Px(8.0),
            ..default()
        },
        BackgroundColor(theme.bg),
    )).with_children(|ctx| {
        // Placeholder messages
        message(ctx, theme, "amy", "@claude help me refactor this code", theme.fg);
        message(ctx, theme, "claude-opus", "I'll analyze the codebase...", theme.accent);
    });
}

fn message(parent: &mut ChildBuilder, theme: &Theme, sender: &str, content: &str, color: Color) {
    parent.spawn(Node {
        flex_direction: FlexDirection::Row,
        column_gap: Val::Px(8.0),
        ..default()
    }).with_children(|msg| {
        msg.spawn((
            Text::new(format!("{}:", sender)),
            TextFont { font_size: 14.0, ..default() },
            TextColor(theme.accent2),
        ));
        msg.spawn((
            Text::new(content),
            TextFont { font_size: 14.0, ..default() },
            TextColor(color),
        ));
    });
}

fn input_bar(parent: &mut ChildBuilder, theme: &Theme) {
    parent.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Px(50.0),
            padding: UiRect::horizontal(Val::Px(16.0)),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            border: UiRect::top(Val::Px(2.0)),
            ..default()
        },
        BorderColor(theme.border),
        BackgroundColor(theme.panel_bg),
    )).with_children(|bar| {
        bar.spawn((
            Text::new("> _"),
            TextFont { font_size: 14.0, ..default() },
            TextColor(theme.fg),
        ));
        bar.spawn((
            Text::new("[N]"),
            TextFont { font_size: 14.0, ..default() },
            TextColor(theme.accent),
            ModeIndicator,
        ));
    });
}

pub fn update_mode_indicator(
    mode: Res<State<Mode>>,
    mut query: Query<&mut Text, With<ModeIndicator>>,
) {
    if mode.is_changed() {
        for mut text in &mut query {
            **text = mode.get().indicator().to_string();
        }
    }
}
```

## Testing

```bash
cd ~/src/kaijutsu
cargo run
```

**Expected:**
1. Window with "会術 Kaijutsu" title bar
2. Dark isekai-styled layout
3. Sidebar with ROOMS, AGENTS, EQUIP sections
4. Context area with placeholder messages
5. Input bar at bottom with mode indicator
6. Press `i` → `[I]`, `Esc` → `[N]`, `Shift+;` → `[:]`

## Next Steps

1. **Text input** - Capture characters in Insert mode
2. **Echo messages** - Send to context area on Enter
3. **Navigation** - `j/k` to move in context area
4. **Console placeholder** - `` ` `` shows coming soon message
5. **Server stub** - Start Cap'n Proto client
