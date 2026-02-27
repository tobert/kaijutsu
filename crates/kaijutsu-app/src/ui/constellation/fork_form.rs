//! Full-viewport fork configuration form.
//!
//! Two-column layout: left (Name + Model), right (Tools tree).
//! Tools use a 2-level expanding tree: categories collapse/expand,
//! individual tools toggle with Space.
//!
//! Uses `Screen::ForkForm` for screen-level transitions. The form root is tagged
//! with `DespawnOnExit(Screen::ForkForm)` for automatic cleanup when leaving.
//! Camera deactivation happens via `OnExit(Screen::Constellation)` in the screen
//! state machine — no manual camera hacks needed.
//!
//! Model is immutable on a context — fork to change it.

use bevy::prelude::*;
use kaijutsu_crdt::ContextId;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::screen::Screen;
use crate::ui::theme::Theme;


// ============================================================================
// MESSAGE
// ============================================================================

/// Message requesting the fork form to open.
#[derive(Message, Clone, Debug)]
pub struct OpenForkForm {
    pub source_context: String,
    pub source_context_id: ContextId,
    pub parent_provider: Option<String>,
    pub parent_model: Option<String>,
}

// ============================================================================
// COMPONENTS & RESOURCES
// ============================================================================

/// Root marker for the fork form (full viewport entity).
#[derive(Component)]
pub struct ForkFormRoot {
    pub source_context_id: ContextId,
    #[allow(dead_code)] // Phase 3: used for display in fork lineage
    pub source_context: String,
}

/// Form state — tracks input, selection, and fetched data.
#[derive(Component)]
pub struct ForkFormState {
    pub name_text: String,
    pub active_field: ForkFormField,
    // Model selection
    pub selected_model_index: usize,
    pub models: Vec<ModelEntry>,
    pub parent_provider: Option<String>,
    pub parent_model: Option<String>,
    pub models_loaded: bool,
    // Tool tree
    pub categories: Vec<ToolCategory>,
    pub tools_loaded: bool,
    /// Index into the flattened visible list (categories + expanded children).
    pub tool_cursor: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ForkFormField {
    Name,
    ModelList,
    ToolList,
}

#[derive(Clone, Debug)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    pub is_inherited: bool,
}

#[derive(Clone, Debug)]
pub struct ToolCategory {
    pub name: String,
    pub expanded: bool,
    pub tools: Vec<ToolEntry>,
}

impl ToolCategory {
    fn enabled_count(&self) -> usize {
        self.tools.iter().filter(|t| t.enabled).count()
    }

    fn total_count(&self) -> usize {
        self.tools.len()
    }

    /// Number of visible rows this category contributes (1 header + children if expanded).
    fn visible_rows(&self) -> usize {
        if self.expanded { 1 + self.tools.len() } else { 1 }
    }
}

#[derive(Clone, Debug)]
pub struct ToolEntry {
    pub name: String,
    #[allow(dead_code)] // Future: tooltip
    pub description: String,
    pub enabled: bool,
}

/// What the tool cursor is pointing at.
enum ToolCursorTarget {
    Category(usize),
    Tool(usize, usize), // (category_idx, tool_idx)
}

/// Resolve a flat cursor index to a (category, tool) target.
fn resolve_tool_cursor(categories: &[ToolCategory], cursor: usize) -> Option<ToolCursorTarget> {
    let mut pos = 0;
    for (ci, cat) in categories.iter().enumerate() {
        if pos == cursor {
            return Some(ToolCursorTarget::Category(ci));
        }
        pos += 1;
        if cat.expanded {
            for ti in 0..cat.tools.len() {
                if pos == cursor {
                    return Some(ToolCursorTarget::Tool(ci, ti));
                }
                pos += 1;
            }
        }
    }
    None
}

/// Total number of visible rows in the tool tree.
fn total_visible_rows(categories: &[ToolCategory]) -> usize {
    categories.iter().map(|c| c.visible_rows()).sum()
}

/// Marker for the name input text display.
#[derive(Component)]
struct ForkFormNameDisplay;

/// Marker for the model list container (children are model items).
#[derive(Component)]
struct ForkFormModelContainer;

/// Marker for individual model items. Index matches ForkFormState.models.
#[derive(Component)]
struct ForkFormModelItem(usize);

/// Marker for the "Loading models..." text.
#[derive(Component)]
struct ForkFormLoadingText;

/// Marker for the name field container (for active-field border highlight).
#[derive(Component)]
struct ForkFormNameField;

/// Marker for the tool tree container.
#[derive(Component)]
struct ForkFormToolContainer;

/// Marker for a row in the tool tree. Flat index into visible rows.
#[derive(Component)]
struct ForkFormToolRow(usize);

/// Marker for the "Loading tools..." text.
#[derive(Component)]
struct ForkFormToolLoadingText;

/// Async result slot for model fetch.
#[derive(Resource, Default)]
struct ForkFormModelResult(Arc<Mutex<Option<FetchedModels>>>);

/// Async result slot for tool schema fetch.
#[derive(Resource, Default)]
struct ForkFormToolResult(Arc<Mutex<Option<FetchedTools>>>);

struct FetchedTools {
    categories: Vec<ToolCategory>,
}

struct FetchedModels {
    providers: Vec<ProviderModels>,
}

struct ProviderModels {
    name: String,
    models: Vec<String>,
}

// ============================================================================
// SETUP
// ============================================================================

pub fn setup_fork_form_systems(app: &mut App) {
    app.init_resource::<ForkFormModelResult>()
        .init_resource::<ForkFormToolResult>()
        .add_message::<OpenForkForm>()
        .add_systems(
            Update,
            (
                handle_open_fork_form,
                poll_fork_form_models.run_if(in_state(Screen::ForkForm)),
                poll_fork_form_tools.run_if(in_state(Screen::ForkForm)),
                handle_fork_form_input.run_if(in_state(Screen::ForkForm)),
            ),
        );
}

// ============================================================================
// OPEN FORM
// ============================================================================

fn handle_open_fork_form(
    mut commands: Commands,
    theme: Res<Theme>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut events: MessageReader<OpenForkForm>,
    existing: Query<Entity, With<ForkFormRoot>>,
    result_slot: Res<ForkFormModelResult>,
    tool_result_slot: Res<ForkFormToolResult>,
    actor: Option<Res<RpcActor>>,
) {
    if !existing.is_empty() {
        return;
    }

    for msg in events.read() {
        info!("Opening fork form for context {}", msg.source_context_id.short());

        next_screen.set(Screen::ForkForm);

        // Clear async slots
        *result_slot.0.lock().unwrap() = None;
        *tool_result_slot.0.lock().unwrap() = None;

        // Spawn the form UI (tagged with DespawnOnExit for auto-cleanup)
        spawn_fork_form(&mut commands, &theme, msg);

        // Kick off async model + tool fetch (parallel)
        if let Some(ref actor) = actor {
            let handle = actor.handle.clone();
            let slot = result_slot.0.clone();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle.get_llm_config().await {
                        Ok(config) => {
                            let providers: Vec<ProviderModels> = config
                                .providers
                                .into_iter()
                                .filter(|p| p.available)
                                .map(|p| {
                                    let models = if p.models.is_empty() {
                                        if p.default_model.is_empty() {
                                            Vec::new()
                                        } else {
                                            vec![p.default_model.clone()]
                                        }
                                    } else {
                                        p.models
                                    };
                                    ProviderModels {
                                        name: p.name,
                                        models,
                                    }
                                })
                                .filter(|p| !p.models.is_empty())
                                .collect();
                            *slot.lock().unwrap() = Some(FetchedModels { providers });
                        }
                        Err(e) => {
                            error!("Failed to fetch LLM config: {}", e);
                            *slot.lock().unwrap() = Some(FetchedModels { providers: vec![] });
                        }
                    }
                })
                .detach();

            // Parallel: fetch tool schemas
            let handle2 = actor.handle.clone();
            let tool_slot = tool_result_slot.0.clone();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle2.get_tool_schemas().await {
                        Ok(schemas) => {
                            // Group by category using BTreeMap for sorted order
                            let mut by_category: BTreeMap<String, Vec<ToolEntry>> =
                                BTreeMap::new();
                            for s in schemas {
                                by_category
                                    .entry(s.category.clone())
                                    .or_default()
                                    .push(ToolEntry {
                                        name: s.name.clone(),
                                        description: s.description.clone(),
                                        enabled: true,
                                    });
                            }
                            // Sort tools within each category
                            let categories: Vec<ToolCategory> = by_category
                                .into_iter()
                                .map(|(name, mut tools)| {
                                    tools.sort_by(|a, b| a.name.cmp(&b.name));
                                    ToolCategory {
                                        name,
                                        expanded: false,
                                        tools,
                                    }
                                })
                                .collect();
                            *tool_slot.lock().unwrap() =
                                Some(FetchedTools { categories });
                        }
                        Err(e) => {
                            error!("Failed to fetch tool schemas: {}", e);
                            *tool_slot.lock().unwrap() =
                                Some(FetchedTools { categories: vec![] });
                        }
                    }
                })
                .detach();
        } else {
            warn!("No RPC actor available, fork form will have no model list");
            *result_slot.0.lock().unwrap() = Some(FetchedModels { providers: vec![] });
            *tool_result_slot.0.lock().unwrap() =
                Some(FetchedTools { categories: vec![] });
        }
    }
}

// ============================================================================
// SPAWN FORM UI — two-column layout
// ============================================================================

fn spawn_fork_form(commands: &mut Commands, theme: &Theme, msg: &OpenForkForm) {
    let title = format!("Fork from {}", msg.source_context_id.short());

    // Full-viewport root — DespawnOnExit auto-cleans when leaving ForkForm screen
    commands
        .spawn((
            ForkFormRoot {
                source_context_id: msg.source_context_id,
                source_context: msg.source_context.clone(),
            },
            ForkFormState {
                name_text: String::new(),
                active_field: ForkFormField::ModelList,
                selected_model_index: 0,
                models: Vec::new(),
                parent_provider: msg.parent_provider.clone(),
                parent_model: msg.parent_model.clone(),
                models_loaded: false,
                categories: Vec::new(),
                tools_loaded: false,
                tool_cursor: 0,
            },
            DespawnOnExit(Screen::ForkForm),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(theme.bg),
            ZIndex(crate::constants::ZLayer::MODAL),
        ))
        .with_children(|root| {
            // Form container (centered, wide enough for 2 columns)
            root.spawn(Node {
                width: Val::Px(720.0),
                max_height: Val::Percent(85.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(16.0),
                padding: UiRect::all(Val::Px(24.0)),
                ..default()
            })
            .with_children(|form| {
                // Title
                form.spawn((
                    MsdfUiText::new(&title)
                        .with_font_size(18.0)
                        .with_color(theme.fg),
                    UiTextPositionCache::default(),
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(22.0),
                        ..default()
                    },
                ));

                // ── Two-column row ──
                form.spawn(Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(20.0),
                    ..default()
                })
                .with_children(|columns| {
                    // ══════════════════════════════════════════
                    // LEFT COLUMN: Name + Model
                    // ══════════════════════════════════════════
                    columns
                        .spawn(Node {
                            flex_basis: Val::Percent(50.0),
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(16.0),
                            ..default()
                        })
                        .with_children(|left| {
                            // ── Name section ──
                            left.spawn(Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(6.0),
                                ..default()
                            })
                            .with_children(|section| {
                                section.spawn((
                                    MsdfUiText::new("Name (optional)")
                                        .with_font_size(12.0)
                                        .with_color(theme.fg_dim),
                                    UiTextPositionCache::default(),
                                    Node {
                                        width: Val::Percent(100.0),
                                        height: Val::Px(14.0),
                                        ..default()
                                    },
                                ));
                                section
                                    .spawn((
                                        ForkFormNameField,
                                        Node {
                                            width: Val::Percent(100.0),
                                            height: Val::Px(36.0),
                                            padding: UiRect::all(Val::Px(8.0)),
                                            border_radius: BorderRadius::all(Val::Px(4.0)),
                                            ..default()
                                        },
                                        BackgroundColor(theme.panel_bg),
                                        BorderColor::all(theme.border),
                                        Outline::new(Val::Px(1.0), Val::ZERO, theme.border),
                                    ))
                                    .with_children(|field| {
                                        field.spawn((
                                            ForkFormNameDisplay,
                                            MsdfUiText::new("hex ID if blank")
                                                .with_font_size(14.0)
                                                .with_color(theme.fg_dim),
                                            UiTextPositionCache::default(),
                                            Node {
                                                width: Val::Percent(100.0),
                                                height: Val::Px(16.0),
                                                ..default()
                                            },
                                        ));
                                    });
                            });

                            // ── Model section ──
                            left.spawn(Node {
                                width: Val::Percent(100.0),
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(6.0),
                                ..default()
                            })
                            .with_children(|section| {
                                section.spawn((
                                    MsdfUiText::new("Model")
                                        .with_font_size(12.0)
                                        .with_color(theme.fg_dim),
                                    UiTextPositionCache::default(),
                                    Node {
                                        width: Val::Percent(100.0),
                                        height: Val::Px(14.0),
                                        ..default()
                                    },
                                ));
                                section
                                    .spawn((
                                        ForkFormModelContainer,
                                        Node {
                                            width: Val::Percent(100.0),
                                            min_height: Val::Px(80.0),
                                            max_height: Val::Px(300.0),
                                            flex_direction: FlexDirection::Column,
                                            padding: UiRect::all(Val::Px(8.0)),
                                            border_radius: BorderRadius::all(Val::Px(4.0)),
                                            row_gap: Val::Px(2.0),
                                            overflow: Overflow::scroll_y(),
                                            ..default()
                                        },
                                        BackgroundColor(theme.panel_bg),
                                        BorderColor::all(theme.accent),
                                        Outline::new(Val::Px(1.0), Val::ZERO, theme.accent),
                                    ))
                                    .with_children(|list| {
                                        list.spawn((
                                            ForkFormLoadingText,
                                            MsdfUiText::new("Loading models...")
                                                .with_font_size(14.0)
                                                .with_color(theme.fg_dim),
                                            UiTextPositionCache::default(),
                                            Node {
                                                width: Val::Percent(100.0),
                                                height: Val::Px(16.0),
                                                ..default()
                                            },
                                        ));
                                    });
                            });
                        });

                    // ══════════════════════════════════════════
                    // RIGHT COLUMN: Tools tree
                    // ══════════════════════════════════════════
                    columns
                        .spawn(Node {
                            flex_basis: Val::Percent(50.0),
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(6.0),
                            ..default()
                        })
                        .with_children(|right| {
                            right.spawn((
                                MsdfUiText::new("Tools")
                                    .with_font_size(12.0)
                                    .with_color(theme.fg_dim),
                                UiTextPositionCache::default(),
                                Node {
                                    width: Val::Percent(100.0),
                                    height: Val::Px(14.0),
                                    ..default()
                                },
                            ));
                            right
                                .spawn((
                                    ForkFormToolContainer,
                                    Node {
                                        width: Val::Percent(100.0),
                                        min_height: Val::Px(80.0),
                                        max_height: Val::Px(420.0),
                                        flex_direction: FlexDirection::Column,
                                        padding: UiRect::all(Val::Px(8.0)),
                                        border_radius: BorderRadius::all(Val::Px(4.0)),
                                        row_gap: Val::Px(1.0),
                                        overflow: Overflow::scroll_y(),
                                        ..default()
                                    },
                                    BackgroundColor(theme.panel_bg),
                                    BorderColor::all(theme.border),
                                    Outline::new(Val::Px(1.0), Val::ZERO, theme.border),
                                ))
                                .with_children(|list| {
                                    list.spawn((
                                        ForkFormToolLoadingText,
                                        MsdfUiText::new("Loading tools...")
                                            .with_font_size(14.0)
                                            .with_color(theme.fg_dim),
                                        UiTextPositionCache::default(),
                                        Node {
                                            width: Val::Percent(100.0),
                                            height: Val::Px(16.0),
                                            ..default()
                                        },
                                    ));
                                });
                        });
                });

                // ── Button row ──
                form.spawn(Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::End,
                    column_gap: Val::Px(12.0),
                    margin: UiRect::top(Val::Px(8.0)),
                    ..default()
                })
                .with_children(|buttons| {
                    // Cancel
                    buttons.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(20.0), Val::Px(10.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.fg_dim.with_alpha(0.2)),
                    ))
                    .with_children(|btn| {
                        btn.spawn((
                            MsdfUiText::new("Cancel")
                                .with_font_size(13.0)
                                .with_color(theme.fg_dim),
                            UiTextPositionCache::default(),
                            Node {
                                width: Val::Px(60.0),
                                height: Val::Px(15.0),
                                ..default()
                            },
                        ));
                    });

                    // Fork
                    buttons.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(20.0), Val::Px(10.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.accent.with_alpha(0.3)),
                    ))
                    .with_children(|btn| {
                        btn.spawn((
                            MsdfUiText::new("Fork")
                                .with_font_size(13.0)
                                .with_color(theme.accent),
                            UiTextPositionCache::default(),
                            Node {
                                width: Val::Px(60.0),
                                height: Val::Px(15.0),
                                ..default()
                            },
                        ));
                    });
                });

                // ── Hints ──
                form.spawn((
                    MsdfUiText::new(
                        "Tab: field | j/k: select | Space: toggle | Enter: expand/fork | Esc: cancel",
                    )
                    .with_font_size(11.0)
                    .with_color(theme.fg_dim),
                    UiTextPositionCache::default(),
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(13.0),
                        margin: UiRect::top(Val::Px(4.0)),
                        ..default()
                    },
                ));
            });
        });

    info!("Fork form spawned");
}

// ============================================================================
// POLL ASYNC MODEL RESULT
// ============================================================================

fn poll_fork_form_models(
    mut commands: Commands,
    theme: Res<Theme>,
    result_slot: Res<ForkFormModelResult>,
    mut state_query: Query<&mut ForkFormState>,
    loading_query: Query<Entity, With<ForkFormLoadingText>>,
    container_query: Query<Entity, With<ForkFormModelContainer>>,
) {
    let Ok(mut state) = state_query.single_mut() else {
        return;
    };
    if state.models_loaded {
        return;
    }

    let data = result_slot.0.lock().unwrap().take();
    let Some(fetched) = data else {
        return;
    };

    // Remove loading text
    for entity in loading_query.iter() {
        commands.entity(entity).despawn();
    }

    // Build flat model list, marking inherited
    let mut models = Vec::new();
    for provider in &fetched.providers {
        for model_id in &provider.models {
            let is_inherited = state.parent_provider.as_deref() == Some(&provider.name)
                && state.parent_model.as_deref() == Some(model_id.as_str());
            models.push(ModelEntry {
                provider: provider.name.clone(),
                model: model_id.clone(),
                is_inherited,
            });
        }
    }

    let inherited_idx = models.iter().position(|m| m.is_inherited).unwrap_or(0);
    state.selected_model_index = inherited_idx;
    state.models = models.clone();
    state.models_loaded = true;

    let Ok(container) = container_query.single() else {
        return;
    };

    if models.is_empty() {
        let hint_entity = commands
            .spawn((
                MsdfUiText::new("No models available (will inherit parent)")
                    .with_font_size(13.0)
                    .with_color(theme.fg_dim),
                UiTextPositionCache::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(15.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(container).add_child(hint_entity);
        return;
    }

    let mut current_provider = String::new();
    for (i, entry) in models.iter().enumerate() {
        if entry.provider != current_provider {
            current_provider = entry.provider.clone();
            let header_entity = commands
                .spawn(Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(18.0),
                    margin: UiRect::top(if i == 0 { Val::Px(0.0) } else { Val::Px(8.0) }),
                    ..default()
                })
                .with_children(|row| {
                    row.spawn((
                        MsdfUiText::new(&entry.provider)
                            .with_font_size(11.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Px(13.0),
                            ..default()
                        },
                    ));
                })
                .id();
            commands.entity(container).add_child(header_entity);
        }

        let is_selected = i == inherited_idx;
        let indicator = if is_selected { "\u{25B8} " } else { "  " };
        let suffix = if entry.is_inherited { "  (inherited)" } else { "" };
        let label = format!("{}{}{}", indicator, entry.model, suffix);
        let color = if is_selected { theme.accent } else { theme.fg };

        let item_entity = commands
            .spawn((
                ForkFormModelItem(i),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(20.0),
                    padding: UiRect::left(Val::Px(12.0)),
                    ..default()
                },
                BackgroundColor(if is_selected {
                    theme.accent.with_alpha(0.1)
                } else {
                    Color::NONE
                }),
            ))
            .with_children(|row| {
                row.spawn((
                    MsdfUiText::new(&label)
                        .with_font_size(13.0)
                        .with_color(color),
                    UiTextPositionCache::default(),
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(15.0),
                        ..default()
                    },
                ));
            })
            .id();
        commands.entity(container).add_child(item_entity);
    }

    info!("Fork form populated with {} models", models.len());
}

// ============================================================================
// POLL ASYNC TOOL RESULT
// ============================================================================

fn poll_fork_form_tools(
    mut commands: Commands,
    theme: Res<Theme>,
    tool_result_slot: Res<ForkFormToolResult>,
    mut state_query: Query<&mut ForkFormState>,
    loading_query: Query<Entity, With<ForkFormToolLoadingText>>,
    container_query: Query<Entity, With<ForkFormToolContainer>>,
    existing_rows: Query<Entity, With<ForkFormToolRow>>,
) {
    let Ok(mut state) = state_query.single_mut() else {
        return;
    };
    if state.tools_loaded {
        return;
    }

    let data = tool_result_slot.0.lock().unwrap().take();
    let Some(fetched) = data else {
        return;
    };

    // Remove loading text
    for entity in loading_query.iter() {
        commands.entity(entity).despawn();
    }

    state.categories = fetched.categories;
    state.tools_loaded = true;

    let Ok(container) = container_query.single() else {
        return;
    };

    if state.categories.is_empty() {
        let hint_entity = commands
            .spawn((
                MsdfUiText::new("No tools available")
                    .with_font_size(13.0)
                    .with_color(theme.fg_dim),
                UiTextPositionCache::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(15.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(container).add_child(hint_entity);
        return;
    }

    // Render collapsed category rows
    for entity in existing_rows.iter() {
        commands.entity(entity).try_despawn();
    }
    rebuild_tool_tree(&mut commands, &theme, &state.categories, state.tool_cursor, container);

    info!(
        "Fork form populated with {} tool categories",
        state.categories.len()
    );
}

/// Rebuild all tool tree entities under the container.
fn rebuild_tool_tree(
    commands: &mut Commands,
    theme: &Theme,
    categories: &[ToolCategory],
    cursor: usize,
    container: Entity,
) {

    let mut flat_idx = 0;
    for cat in categories {
        let is_cursor = flat_idx == cursor;
        let arrow = if cat.expanded { "\u{25BE}" } else { "\u{25B8}" }; // ▾ or ▸
        let label = format!(
            "{} {} ({}/{})",
            arrow,
            cat.name,
            cat.enabled_count(),
            cat.total_count()
        );
        let color = if is_cursor { theme.accent } else { theme.fg };

        let row = commands
            .spawn((
                ForkFormToolRow(flat_idx),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(20.0),
                    ..default()
                },
                BackgroundColor(if is_cursor {
                    theme.accent.with_alpha(0.1)
                } else {
                    Color::NONE
                }),
            ))
            .with_children(|r| {
                r.spawn((
                    MsdfUiText::new(&label)
                        .with_font_size(13.0)
                        .with_color(color),
                    UiTextPositionCache::default(),
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(15.0),
                        ..default()
                    },
                ));
            })
            .id();
        commands.entity(container).add_child(row);
        flat_idx += 1;

        if cat.expanded {
            for tool in &cat.tools {
                let is_cursor = flat_idx == cursor;
                let checkbox = if tool.enabled { "[x]" } else { "[ ]" };
                let label = format!("  {} {}", checkbox, tool.name);
                let color = if is_cursor {
                    theme.accent
                } else if tool.enabled {
                    theme.fg
                } else {
                    theme.fg_dim
                };

                let row = commands
                    .spawn((
                        ForkFormToolRow(flat_idx),
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Px(20.0),
                            padding: UiRect::left(Val::Px(8.0)),
                            ..default()
                        },
                        BackgroundColor(if is_cursor {
                            theme.accent.with_alpha(0.1)
                        } else {
                            Color::NONE
                        }),
                    ))
                    .with_children(|r| {
                        r.spawn((
                            MsdfUiText::new(&label)
                                .with_font_size(13.0)
                                .with_color(color),
                            UiTextPositionCache::default(),
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(15.0),
                                ..default()
                            },
                        ));
                    })
                    .id();
                commands.entity(container).add_child(row);
                flat_idx += 1;
            }
        }
    }
}

// ============================================================================
// INPUT HANDLING
// ============================================================================

fn handle_fork_form_input(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut text_events: MessageReader<TextInputReceived>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut state_query: Query<(&mut ForkFormState, &ForkFormRoot)>,
    mut name_display: Query<&mut MsdfUiText, With<ForkFormNameDisplay>>,
    mut model_items: Query<(&ForkFormModelItem, &mut BackgroundColor, &Children)>,
    mut model_texts: Query<
        &mut MsdfUiText,
        (
            Without<ForkFormNameDisplay>,
            Without<ForkFormLoadingText>,
            Without<ForkFormToolLoadingText>,
        ),
    >,
    // Combined outline + entity queries to stay under 16-param limit
    mut outlines: ParamSet<(
        Query<&mut Outline, (With<ForkFormNameField>, Without<ForkFormModelContainer>, Without<ForkFormToolContainer>)>,
        Query<&mut Outline, (With<ForkFormModelContainer>, Without<ForkFormNameField>, Without<ForkFormToolContainer>)>,
        Query<&mut Outline, (With<ForkFormToolContainer>, Without<ForkFormNameField>, Without<ForkFormModelContainer>)>,
    )>,
    tool_queries: (Query<Entity, With<ForkFormToolContainer>>, Query<Entity, With<ForkFormToolRow>>),
    theme: Res<Theme>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((mut state, form_root)) = state_query.single_mut() else {
        return;
    };

    let mut text_changed = false;
    let mut model_selection_changed = false;
    let mut tool_tree_changed = false;

    // Handle text input
    if state.active_field == ForkFormField::Name {
        for TextInputReceived(text) in text_events.read() {
            for c in text.chars() {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    state.name_text.push(c);
                    text_changed = true;
                }
            }
        }
    } else if state.active_field == ForkFormField::ToolList {
        // Space toggles tool/category
        for TextInputReceived(text) in text_events.read() {
            if text.contains(' ') {
                if let Some(target) = resolve_tool_cursor(&state.categories, state.tool_cursor) {
                    match target {
                        ToolCursorTarget::Category(ci) => {
                            // Toggle all tools in category
                            let all_enabled =
                                state.categories[ci].tools.iter().all(|t| t.enabled);
                            let new_state = !all_enabled;
                            for tool in &mut state.categories[ci].tools {
                                tool.enabled = new_state;
                            }
                            tool_tree_changed = true;
                        }
                        ToolCursorTarget::Tool(ci, ti) => {
                            state.categories[ci].tools[ti].enabled =
                                !state.categories[ci].tools[ti].enabled;
                            tool_tree_changed = true;
                        }
                    }
                }
            }
        }
    } else {
        // Drain text events to avoid stale reads
        for _ in text_events.read() {}
    }

    // Handle actions
    for ActionFired(action) in actions.read() {
        match action {
            Action::Backspace if state.active_field == ForkFormField::Name => {
                state.name_text.pop();
                text_changed = true;
            }
            Action::CycleFocusForward => {
                state.active_field = match state.active_field {
                    ForkFormField::Name => ForkFormField::ModelList,
                    ForkFormField::ModelList => ForkFormField::ToolList,
                    ForkFormField::ToolList => ForkFormField::Name,
                };
                set_outlines(&state, &theme, &mut outlines);
            }
            Action::CycleFocusBackward => {
                state.active_field = match state.active_field {
                    ForkFormField::Name => ForkFormField::ToolList,
                    ForkFormField::ModelList => ForkFormField::Name,
                    ForkFormField::ToolList => ForkFormField::ModelList,
                };
                set_outlines(&state, &theme, &mut outlines);
            }
            // ── Model list navigation ──
            Action::SpatialNav(dir) if state.active_field == ForkFormField::ModelList => {
                if dir.y < 0.0 && state.selected_model_index > 0 {
                    state.selected_model_index -= 1;
                    model_selection_changed = true;
                } else if dir.y > 0.0
                    && state.selected_model_index + 1 < state.models.len()
                {
                    state.selected_model_index += 1;
                    model_selection_changed = true;
                }
            }
            Action::FocusNextBlock if state.active_field == ForkFormField::ModelList => {
                if state.selected_model_index + 1 < state.models.len() {
                    state.selected_model_index += 1;
                    model_selection_changed = true;
                }
            }
            Action::FocusPrevBlock if state.active_field == ForkFormField::ModelList => {
                if state.selected_model_index > 0 {
                    state.selected_model_index -= 1;
                    model_selection_changed = true;
                }
            }
            // ── Tool tree navigation ──
            Action::SpatialNav(dir) if state.active_field == ForkFormField::ToolList => {
                let max = total_visible_rows(&state.categories);
                if dir.y < 0.0 && state.tool_cursor > 0 {
                    state.tool_cursor -= 1;
                    tool_tree_changed = true;
                } else if dir.y > 0.0 && state.tool_cursor + 1 < max {
                    state.tool_cursor += 1;
                    tool_tree_changed = true;
                }
            }
            Action::FocusNextBlock if state.active_field == ForkFormField::ToolList => {
                let max = total_visible_rows(&state.categories);
                if state.tool_cursor + 1 < max {
                    state.tool_cursor += 1;
                    tool_tree_changed = true;
                }
            }
            Action::FocusPrevBlock if state.active_field == ForkFormField::ToolList => {
                if state.tool_cursor > 0 {
                    state.tool_cursor -= 1;
                    tool_tree_changed = true;
                }
            }
            // ── Activate (Enter) ──
            Action::Activate => {
                if state.active_field == ForkFormField::ToolList {
                    // Enter on a category toggles expand/collapse
                    if let Some(ToolCursorTarget::Category(ci)) =
                        resolve_tool_cursor(&state.categories, state.tool_cursor)
                    {
                        state.categories[ci].expanded = !state.categories[ci].expanded;
                        tool_tree_changed = true;
                    } else {
                        // Enter on a tool row → submit the form
                        if state.models_loaded {
                            submit_fork_form(
                                &state,
                                form_root,
                                &bootstrap,
                                &conn_state,
                                actor.as_deref(),
                            );
                            next_screen.set(Screen::Constellation);
                        }
                    }
                } else {
                    // Submit fork from Name or Model field
                    if state.models_loaded {
                        submit_fork_form(
                            &state,
                            form_root,
                            &bootstrap,
                            &conn_state,
                            actor.as_deref(),
                        );
                        next_screen.set(Screen::Constellation);
                    }
                }
            }
            Action::Unfocus => {
                info!("Fork form cancelled");
                next_screen.set(Screen::Constellation);
            }
            _ => {}
        }
    }

    // Update name display
    if text_changed {
        if let Ok(mut msdf) = name_display.single_mut() {
            if state.name_text.is_empty() {
                msdf.text = "hex ID if blank".to_string();
                msdf.color = bevy_to_rgba8(theme.fg_dim);
            } else {
                msdf.text = state.name_text.clone();
                msdf.color = bevy_to_rgba8(theme.fg);
            }
        }
    }

    // Update model item visuals
    if model_selection_changed {
        for (item, mut bg, children) in model_items.iter_mut() {
            let is_selected = item.0 == state.selected_model_index;
            *bg = if is_selected {
                BackgroundColor(theme.accent.with_alpha(0.1))
            } else {
                BackgroundColor(Color::NONE)
            };

            for child in children.iter() {
                if let Ok(mut msdf) = model_texts.get_mut(child) {
                    let entry = &state.models[item.0];
                    let indicator = if is_selected { "\u{25B8} " } else { "  " };
                    let suffix = if entry.is_inherited { "  (inherited)" } else { "" };
                    msdf.text = format!("{}{}{}", indicator, entry.model, suffix);
                    msdf.color =
                        bevy_to_rgba8(if is_selected { theme.accent } else { theme.fg });
                }
            }
        }
    }

    // Rebuild tool tree if anything changed (expand/collapse, toggle, cursor move)
    if tool_tree_changed {
        let (ref tool_container_q, ref tool_rows_q) = tool_queries;
        for entity in tool_rows_q.iter() {
            commands.entity(entity).try_despawn();
        }
        if let Ok(container) = tool_container_q.single() {
            rebuild_tool_tree(
                &mut commands,
                &theme,
                &state.categories,
                state.tool_cursor,
                container,
            );
        }
    }
}

/// Update outline colors based on active field. Takes a ParamSet to work
/// within the 16-param system limit.
fn set_outlines(state: &ForkFormState, theme: &Theme, outlines: &mut ParamSet<(
    Query<&mut Outline, (With<ForkFormNameField>, Without<ForkFormModelContainer>, Without<ForkFormToolContainer>)>,
    Query<&mut Outline, (With<ForkFormModelContainer>, Without<ForkFormNameField>, Without<ForkFormToolContainer>)>,
    Query<&mut Outline, (With<ForkFormToolContainer>, Without<ForkFormNameField>, Without<ForkFormModelContainer>)>,
)>) {
    let name_color = if state.active_field == ForkFormField::Name { theme.accent } else { theme.border };
    let model_color = if state.active_field == ForkFormField::ModelList { theme.accent } else { theme.border };
    let tool_color = if state.active_field == ForkFormField::ToolList { theme.accent } else { theme.border };

    if let Ok(mut outline) = outlines.p0().single_mut() {
        outline.color = name_color;
    }
    if let Ok(mut outline) = outlines.p1().single_mut() {
        outline.color = model_color;
    }
    if let Ok(mut outline) = outlines.p2().single_mut() {
        outline.color = tool_color;
    }
}

// ============================================================================
// SUBMIT
// ============================================================================

fn submit_fork_form(
    state: &ForkFormState,
    form_root: &ForkFormRoot,
    bootstrap: &BootstrapChannel,
    conn_state: &RpcConnectionState,
    actor: Option<&RpcActor>,
) {
    let Some(actor) = actor else {
        error!("Cannot fork: no active RPC actor");
        return;
    };

    let selected = state.models.get(state.selected_model_index);
    let fork_label = state.name_text.clone();

    let handle = actor.handle.clone();
    let source_ctx_id = form_root.source_context_id;
    let config = conn_state.ssh_config.clone();
    let kernel_id = conn_state
        .current_kernel
        .as_ref()
        .map(|k| k.id.to_string())
        .unwrap_or_else(|| crate::constants::DEFAULT_KERNEL_ID.to_string());
    let bootstrap_tx = bootstrap.tx.clone();

    let selected_provider = selected.map(|s| s.provider.clone());
    let selected_model = selected.map(|s| s.model.clone());
    let parent_provider = state.parent_provider.clone();
    let parent_model = state.parent_model.clone();

    // Collect disabled tools for DenyList
    let disabled_tools: Vec<String> = state
        .categories
        .iter()
        .flat_map(|cat| cat.tools.iter())
        .filter(|t| !t.enabled)
        .map(|t| t.name.clone())
        .collect();
    let has_tool_filter = !disabled_tools.is_empty();

    info!(
        "Fork submit: from={}, label='{}', model={:?}, disabled_tools={}",
        source_ctx_id.short(),
        fork_label,
        selected_model,
        disabled_tools.len()
    );

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            // Step 1: Fork
            let new_ctx_id = match handle
                .fork_from_version(source_ctx_id, 0, &fork_label)
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    error!("Fork failed: {}", e);
                    return;
                }
            };
            info!("Fork created: {}", new_ctx_id);

            // Step 2: Set model if different from parent
            let model_changed =
                selected_model != parent_model || selected_provider != parent_provider;
            if model_changed {
                if let (Some(provider), Some(model)) = (&selected_provider, &selected_model) {
                    match handle.set_context_model(new_ctx_id, provider, model).await {
                        Ok(true) => {
                            info!(
                                "Model set on {}: {}/{}",
                                new_ctx_id.short(),
                                provider,
                                model
                            )
                        }
                        Ok(false) => warn!("set_context_model returned false"),
                        Err(e) => error!("Failed to set model: {}", e),
                    }
                }
            }

            // Step 3: Set per-context tool filter if any tools were disabled
            if has_tool_filter {
                use kaijutsu_client::rpc::ClientToolFilter;
                match handle
                    .set_context_tool_filter(
                        new_ctx_id,
                        ClientToolFilter::DenyList(disabled_tools),
                    )
                    .await
                {
                    Ok(true) => info!("Tool filter set on {}", new_ctx_id.short()),
                    Ok(false) => warn!("set_context_tool_filter returned false"),
                    Err(e) => error!("Failed to set tool filter: {}", e),
                }
            }

            // Step 4: Switch to new context
            let instance = Uuid::new_v4().to_string();
            let _ = bootstrap_tx.send(BootstrapCommand::SpawnActor {
                config,
                kernel_id,
                context_id: Some(new_ctx_id),
                instance,
            });
        })
        .detach();
}
