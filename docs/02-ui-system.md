# Kaijutsu UI System

*Last updated: 2026-01-15*
*Based on Bevy 0.18.0 (released 2026-01-13)*

## Design Philosophy

**Input is sacred.** The input bar never moves. Everything else adapts around it.

**Context is everything.** The main view isn't "chat" - it's a cognitive load management system for navigating agent conversations at scale.

## Bevy 0.18 UI Foundation

### Features We'll Use

| Feature | Crate | Purpose |
|---------|-------|---------|
| **bevy_ui_widgets** | `bevy_ui_widgets` | Unstyled widgets (Button, Slider, Checkbox, Menu, Popover) |
| **DirectionalNavigation** | `bevy_input_focus` | Gamepad/keyboard navigation |
| **TabNavigation** | `bevy_input_focus` | Tab-based focus cycling |
| **Popover** | `bevy_ui_widgets` | Floating panels, tooltips |
| **AutoDirectionalNavigation** | `bevy_ui` | Spatial-based nav graph |

### Widget Pattern (External State)

Bevy 0.18's widgets use **external state management**:
- Widgets emit events on interaction
- App updates both widget state AND game state
- No two-way binding complexity

```rust
(
    slider(0.0, 100.0, 50.0),
    observe(
        |value_change: On<ValueChange<f32>>,
         mut state: ResMut<AppState>| {
            state.value = value_change.value;
        },
    )
)
```

### Declarative Children

The `children![]` macro enables clean declarative UI:

```rust
(
    Node {
        width: percent(100),
        height: px(65),
        ..default()
    },
    Button,
    children![
        (Text::new("Click me"), TextFont { font_size: 20.0, ..default() }),
    ],
)
```

## Mode System

Inspired by vim, the UI has **modes** that change input behavior:

| Mode | Purpose | Enter | Exit |
|------|---------|-------|------|
| **Normal** | Navigate, read, explore | `Esc` | - |
| **Insert** | Type in input bar | `i` | `Esc` |
| **Command** | Slash commands | `:` | `Esc`, `Enter` |

Future: **Browse** mode for exploring room configurations (skill-tree style).

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, States, Hash)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Command,
}
```

## Layout Structure

```
┌─────────────────────────────────────────────────────────────────┐
│ 【会術】 Kaijutsu                    ▣ room: lobby  @opus  ◉    │  ← Title bar
├───────────┬─────────────────────────────────────────────────────┤
│ ┌───────┐ │                                                     │
│ │ ROOMS │ │  ┌─────────────────────────────────────────────┐   │
│ │───────│ │  │ atobey: @claude help me refactor            │   │
│ │>lobby │ │  ├─────────────────────────────────────────────┤   │
│ │ dev   │ │  │ claude-opus ──────────────────── [expand ▼] │   │  ← Context View
│ │ ops   │ │  │ I'll analyze the codebase...                │   │    (DAG blocks)
│ └───────┘ │  │ ┌─ tool: Read ────────────────────────────┐ │   │
│           │  │ │ src/main.rs (collapsed)                 │ │   │
│ ┌───────┐ │  │ └─────────────────────────────────────────┘ │   │
│ │AGENTS │ │  │ Here are my suggestions:                    │   │
│ │───────│ │  │ 1. Extract the config parsing...           │   │
│ │◉ opus │ │  └─────────────────────────────────────────────┘   │
│ │◉ haiku│ │                                                     │
│ │○ local│ │  ════════════════════════════════════════════════  │
│ └───────┘ │  │ 会sh console (Quake-style, ` to toggle)    ▼25%│ │  ← Drops DOWN
│ ┌───────┐ │  │ /room/src> ls                                 │ │    above input
│ │EQUIP  │ │  │ Cargo.toml  src/  README.md                   │ │
│ │───────│ │  │ > _                                           │ │
│ │ fs    │ │  └───────────────────────────────────────────────┘ │
│ │ web   │ │                                                     │
│ └───────┘ │                                                     │
├───────────┴─────────────────────────────────────────────────────┤
│ > @opus what do you think about...                          [I] │  ← Input (SACRED)
└─────────────────────────────────────────────────────────────────┘
```

## Core UI Components

### 1. Input Bar (Sacred)

Always visible. Never moves. The center of the user's universe.

```rust
#[derive(Component)]
struct InputBar;

#[derive(Component)]
struct InputText(String);

#[derive(Component)]
struct ModeIndicator;
```

**Behaviors by mode:**
- **Normal**: Input bar shows last message preview, accepts navigation keys
- **Insert**: Full text editing, `@` triggers agent mention completion
- **Command**: Prefix with `:`, autocomplete commands

**Future**: Controller support via small fast agent predicting inputs.

### 2. Context View (DAG Blocks)

Not a chat log. A navigable tree of context.

```rust
#[derive(Component)]
struct ContextView;

#[derive(Component)]
struct ContextBlock {
    row_id: u64,
    row_type: RowType,
    parent_id: Option<u64>,
    collapsed: bool,
}

#[derive(Clone, Copy)]
enum RowType {
    Chat,
    AgentResponse,
    ToolCall,
    ToolResult,
    SystemMessage,
}
```

**Features:**
- Collapse/expand blocks (tool calls, long responses)
- Navigate with `j/k` or arrows
- `Enter` to expand/focus a block
- `o` to open in detail panel
- Visual threading via indentation

**Scale challenge:** 5-10 Claudes going ham. Need aggressive collapse defaults, smart summarization, activity indicators.

### 3. Quake Console

Drops down from top when toggled. Connects to room's shared kaish kernel.

```rust
#[derive(Component)]
struct QuakeConsole {
    height_percent: f32,  // 25, 50, 75, 100
    visible: bool,
}
```

**Key:** `` ` `` or `Ctrl+`` ` toggles. Input bar stays where it is.

### 4. Sidebar

Room list, agents, equipment. Classic left-side panel.

```rust
#[derive(Component)]
enum SidebarSection {
    Rooms,
    Agents,
    Equipment,
}
```

## Styling (Isekai Theme)

```rust
#[derive(Resource)]
pub struct Theme {
    // Background
    pub bg: Color,
    pub panel_bg: Color,

    // Text
    pub fg: Color,
    pub fg_dim: Color,

    // Accents
    pub accent: Color,      // Cyan - primary actions
    pub accent2: Color,     // Magenta - highlights
    pub success: Color,     // Green
    pub warning: Color,     // Orange
    pub error: Color,       // Red

    // Chrome
    pub border: Color,
    pub border_glow: bool,
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
            success: Color::srgb(0.25, 0.73, 0.31),
            warning: Color::srgb(0.83, 0.6, 0.13),
            error: Color::srgb(0.97, 0.32, 0.29),
            border: Color::srgb(0.19, 0.21, 0.24),
            border_glow: true,
        }
    }
}
```

## Implementation Phases

### Phase 1: Basic Shell (MVP)
- Single window with fixed layout
- Title bar, sidebar placeholders, context area, input bar
- Mode indicator (Normal/Insert/Command)
- Basic keyboard mode switching
- Dummy content

### Phase 2: Context View
- Render message DAG as collapsible blocks
- Row types with visual distinction
- Navigate with keyboard
- Collapse/expand

### Phase 3: Server Connection
- SSH + Cap'n Proto integration
- Real room data
- Live message updates

### Phase 4: Quake Console
- Toggle animation
- Connect to room's kaish kernel
- Structured output rendering

### Phase 5: Polish
- Equipment panel
- Room switching
- Fork UI
- Gamepad navigation

## Key Dependencies

```toml
[dependencies]
bevy = { version = "0.18", features = ["default"] }

# SSH + RPC (later phases)
russh = "0.45"
capnp = "0.20"
capnp-rpc = "0.20"
```

## References

- [Bevy 0.18 Release Notes](https://bevy.org/news/bevy-0-18/)
- [bevy_ui_widgets examples](~/src/bevy/examples/ui/standard_widgets.rs)
- [DirectionalNavigation example](~/src/bevy/examples/ui/directional_navigation.rs)
