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
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::form::{
    msdf_label, msdf_text,
    ActiveFormField, AsyncSlot, FormField, ListItem, SelectableList,
    TreeCategory, TreeCursorTarget, TreeItem, TreeView,
};
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
    pub parent_provider: Option<String>,
    pub parent_model: Option<String>,
    pub models_loaded: bool,
    pub tools_loaded: bool,
}

/// Field IDs for ActiveFormField / FormField matching.
const FIELD_NAME: u8 = 0;
const FIELD_MODEL: u8 = 1;
const FIELD_TOOLS: u8 = 2;

#[derive(Clone, Debug, PartialEq)]
pub enum ForkFormField {
    Name,
    ModelList,
    ToolList,
}

impl ForkFormField {
    fn id(&self) -> u8 {
        match self {
            ForkFormField::Name => FIELD_NAME,
            ForkFormField::ModelList => FIELD_MODEL,
            ForkFormField::ToolList => FIELD_TOOLS,
        }
    }

    fn from_id(id: u8) -> Self {
        match id {
            FIELD_NAME => ForkFormField::Name,
            FIELD_MODEL => ForkFormField::ModelList,
            _ => ForkFormField::ToolList,
        }
    }

    fn next(&self) -> Self {
        Self::from_id((self.id() + 1) % 3)
    }

    fn prev(&self) -> Self {
        Self::from_id((self.id() + 2) % 3)
    }
}

/// Marker for the name field container.
#[derive(Component)]
struct ForkFormNameField;

/// Marker for the name input text display.
#[derive(Component)]
struct ForkFormNameDisplay;

/// Marker for the model list container (holds the SelectableList).
#[derive(Component)]
struct ForkFormModelContainer;

/// Marker for the tool tree container (holds the TreeView).
#[derive(Component)]
struct ForkFormToolContainer;

/// Marker for the "Loading models..." text.
#[derive(Component)]
struct ForkFormLoadingText;

/// Marker for the "Loading tools..." text.
#[derive(Component)]
struct ForkFormToolLoadingText;

struct FetchedTools {
    categories: Vec<TreeCategory>,
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
    app.init_resource::<AsyncSlot<FetchedModels>>()
        .init_resource::<AsyncSlot<FetchedTools>>()
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
    model_slot: Res<AsyncSlot<FetchedModels>>,
    tool_slot: Res<AsyncSlot<FetchedTools>>,
    actor: Option<Res<RpcActor>>,
) {
    if !existing.is_empty() {
        return;
    }

    for msg in events.read() {
        info!("Opening fork form for context {}", msg.source_context_id.short());

        next_screen.set(Screen::ForkForm);
        model_slot.clear();
        tool_slot.clear();

        spawn_fork_form(&mut commands, &theme, msg);

        // Kick off async model + tool fetch (parallel)
        if let Some(ref actor) = actor {
            let handle = actor.handle.clone();
            let slot = model_slot.sender();
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
                                    ProviderModels { name: p.name, models }
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

            let handle2 = actor.handle.clone();
            let tool_sender = tool_slot.sender();
            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle2.get_tool_schemas().await {
                        Ok(schemas) => {
                            let mut by_category: BTreeMap<String, Vec<TreeItem>> =
                                BTreeMap::new();
                            for s in schemas {
                                by_category.entry(s.category.clone()).or_default().push(
                                    TreeItem { label: s.name.clone(), enabled: true },
                                );
                            }
                            let categories: Vec<TreeCategory> = by_category
                                .into_iter()
                                .map(|(name, mut items)| {
                                    items.sort_by(|a, b| a.label.cmp(&b.label));
                                    TreeCategory { name, expanded: false, items }
                                })
                                .collect();
                            *tool_sender.lock().unwrap() = Some(FetchedTools { categories });
                        }
                        Err(e) => {
                            error!("Failed to fetch tool schemas: {}", e);
                            *tool_sender.lock().unwrap() = Some(FetchedTools { categories: vec![] });
                        }
                    }
                })
                .detach();
        } else {
            warn!("No RPC actor available, fork form will have no model list");
            *model_slot.sender().lock().unwrap() = Some(FetchedModels { providers: vec![] });
            *tool_slot.sender().lock().unwrap() = Some(FetchedTools { categories: vec![] });
        }
    }
}

// ============================================================================
// SPAWN FORM UI — two-column layout
// ============================================================================

/// Helper: spawn a labeled bordered container with an extra marker on the container.
fn spawn_field_section<M: Bundle>(
    parent: &mut ChildSpawnerCommands,
    field_id: u8,
    label: &str,
    theme: &Theme,
    is_active: bool,
    min_height: f32,
    max_height: Option<f32>,
    marker: M,
    inner: impl FnOnce(&mut ChildSpawnerCommands),
) {
    let outline_color = if is_active { theme.accent } else { theme.border };

    parent
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(6.0),
            ..default()
        })
        .with_children(|section| {
            msdf_label(section, label, 12.0, theme.fg_dim);

            let mut node = Node {
                width: Val::Percent(100.0),
                min_height: Val::Px(min_height),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                border_radius: BorderRadius::all(Val::Px(4.0)),
                row_gap: Val::Px(2.0),
                overflow: Overflow::scroll_y(),
                ..default()
            };
            if let Some(max) = max_height {
                node.max_height = Val::Px(max);
            }

            section
                .spawn((
                    FormField { field_id },
                    marker,
                    node,
                    BackgroundColor(theme.panel_bg),
                    BorderColor::all(outline_color),
                    Outline::new(Val::Px(1.0), Val::ZERO, outline_color),
                    Interaction::None,
                ))
                .with_children(inner);
        });
}

fn spawn_fork_form(commands: &mut Commands, theme: &Theme, msg: &OpenForkForm) {
    let title = format!("Fork from {}", msg.source_context_id.short());

    commands
        .spawn((
            ForkFormRoot {
                source_context_id: msg.source_context_id,
                source_context: msg.source_context.clone(),
            },
            ForkFormState {
                name_text: String::new(),
                active_field: ForkFormField::ModelList,
                parent_provider: msg.parent_provider.clone(),
                parent_model: msg.parent_model.clone(),
                models_loaded: false,
                tools_loaded: false,
            },
            ActiveFormField(FIELD_MODEL),
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
                msdf_label(form, &title, 18.0, theme.fg);

                // ── Two-column row ──
                form.spawn(Node {
                    width: Val::Percent(100.0),
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(20.0),
                    ..default()
                })
                .with_children(|columns| {
                    // ═══ LEFT COLUMN: Name + Model ═══
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
                            spawn_field_section(
                                left, FIELD_NAME, "Name (optional)", theme,
                                false, 36.0, None,
                                ForkFormNameField,
                                |container| {
                                    container.spawn((
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
                                },
                            );

                            // ── Model section ──
                            spawn_field_section(
                                left, FIELD_MODEL, "Model", theme,
                                true, 80.0, Some(300.0),
                                ForkFormModelContainer,
                                |container| {
                                    container.spawn((
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
                                },
                            );
                        });

                    // ═══ RIGHT COLUMN: Tools tree ═══
                    columns
                        .spawn(Node {
                            flex_basis: Val::Percent(50.0),
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(6.0),
                            ..default()
                        })
                        .with_children(|right| {
                            spawn_field_section(
                                right, FIELD_TOOLS, "Tools", theme,
                                false, 80.0, Some(420.0),
                                ForkFormToolContainer,
                                |container| {
                                    container.spawn((
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
                                },
                            );
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
                    buttons.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(20.0), Val::Px(10.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.fg_dim.with_alpha(0.2)),
                    ))
                    .with_children(|btn| {
                        msdf_text(btn, "Cancel", 13.0, theme.fg_dim, 60.0);
                    });

                    buttons.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(20.0), Val::Px(10.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.accent.with_alpha(0.3)),
                    ))
                    .with_children(|btn| {
                        msdf_text(btn, "Fork", 13.0, theme.accent, 60.0);
                    });
                });

                // ── Hints ──
                msdf_label(
                    form,
                    "Tab: field | j/k: select | Space: toggle | Enter: expand/fork | Esc: cancel",
                    11.0,
                    theme.fg_dim,
                );
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
    model_slot: Res<AsyncSlot<FetchedModels>>,
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

    let Some(fetched) = model_slot.take() else {
        return;
    };

    // Remove loading text
    for entity in loading_query.iter() {
        commands.entity(entity).despawn();
    }

    let Ok(container) = container_query.single() else {
        return;
    };

    // Build flat model list with ListItem entries
    let mut list_items = Vec::new();
    let mut current_provider = String::new();

    for provider in &fetched.providers {
        for model_id in &provider.models {
            let is_inherited = state.parent_provider.as_deref() == Some(&provider.name)
                && state.parent_model.as_deref() == Some(model_id.as_str());

            if provider.name != current_provider {
                current_provider = provider.name.clone();
                list_items.push(ListItem::header(&provider.name));
            }

            let suffix = if is_inherited { "  (inherited)" } else { "" };
            list_items.push(ListItem::new(model_id).with_suffix(suffix));
        }
    }

    state.models_loaded = true;

    if list_items.is_empty() {
        let hint = commands
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
        commands.entity(container).add_child(hint);
        return;
    }

    // Find inherited model index (skip headers)
    let inherited_idx = list_items
        .iter()
        .enumerate()
        .find(|(_, item)| !item.is_header && !item.suffix.is_empty())
        .map(|(i, _)| i)
        .unwrap_or_else(|| {
            list_items.iter().position(|i| !i.is_header).unwrap_or(0)
        });

    let mut list = SelectableList::new(list_items, 13.0);
    list.selected = inherited_idx;
    commands.entity(container).insert(list);

    info!("Fork form models loaded");
}

// ============================================================================
// POLL ASYNC TOOL RESULT
// ============================================================================

fn poll_fork_form_tools(
    mut commands: Commands,
    theme: Res<Theme>,
    tool_slot: Res<AsyncSlot<FetchedTools>>,
    mut state_query: Query<&mut ForkFormState>,
    loading_query: Query<Entity, With<ForkFormToolLoadingText>>,
    container_query: Query<Entity, With<ForkFormToolContainer>>,
) {
    let Ok(mut state) = state_query.single_mut() else {
        return;
    };
    if state.tools_loaded {
        return;
    }

    let Some(fetched) = tool_slot.take() else {
        return;
    };

    for entity in loading_query.iter() {
        commands.entity(entity).despawn();
    }

    state.tools_loaded = true;

    let Ok(container) = container_query.single() else {
        return;
    };

    if fetched.categories.is_empty() {
        let hint = commands
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
        commands.entity(container).add_child(hint);
        return;
    }

    let tree = TreeView::new(fetched.categories, 13.0);
    commands.entity(container).insert(tree);

    info!("Fork form tools loaded");
}

// ============================================================================
// INPUT HANDLING
// ============================================================================

fn handle_fork_form_input(
    mut actions: MessageReader<ActionFired>,
    mut text_events: MessageReader<TextInputReceived>,
    mut next_screen: ResMut<NextState<Screen>>,
    mut state_query: Query<(&mut ForkFormState, &ForkFormRoot, &mut ActiveFormField)>,
    mut name_display: Query<&mut MsdfUiText, With<ForkFormNameDisplay>>,
    mut model_list: Query<&mut SelectableList, With<ForkFormModelContainer>>,
    mut tool_tree: Query<&mut TreeView, With<ForkFormToolContainer>>,
    theme: Res<Theme>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((mut state, form_root, mut active_field)) = state_query.single_mut() else {
        return;
    };

    let mut text_changed = false;

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
        for TextInputReceived(text) in text_events.read() {
            if text.contains(' ') {
                if let Ok(mut tree) = tool_tree.single_mut() {
                    tree.toggle_item();
                }
            }
        }
    } else {
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
                state.active_field = state.active_field.next();
                active_field.0 = state.active_field.id();
            }
            Action::CycleFocusBackward => {
                state.active_field = state.active_field.prev();
                active_field.0 = state.active_field.id();
            }
            Action::SpatialNav(dir) if state.active_field == ForkFormField::ModelList => {
                if let Ok(mut list) = model_list.single_mut() {
                    if dir.y < 0.0 { list.select_prev(); }
                    else if dir.y > 0.0 { list.select_next(); }
                }
            }
            Action::FocusNextBlock if state.active_field == ForkFormField::ModelList => {
                if let Ok(mut list) = model_list.single_mut() { list.select_next(); }
            }
            Action::FocusPrevBlock if state.active_field == ForkFormField::ModelList => {
                if let Ok(mut list) = model_list.single_mut() { list.select_prev(); }
            }
            Action::SpatialNav(dir) if state.active_field == ForkFormField::ToolList => {
                if let Ok(mut tree) = tool_tree.single_mut() {
                    if dir.y < 0.0 { tree.cursor_prev(); }
                    else if dir.y > 0.0 { tree.cursor_next(); }
                }
            }
            Action::FocusNextBlock if state.active_field == ForkFormField::ToolList => {
                if let Ok(mut tree) = tool_tree.single_mut() { tree.cursor_next(); }
            }
            Action::FocusPrevBlock if state.active_field == ForkFormField::ToolList => {
                if let Ok(mut tree) = tool_tree.single_mut() { tree.cursor_prev(); }
            }
            Action::Activate => {
                if state.active_field == ForkFormField::ToolList {
                    let is_category = tool_tree
                        .single()
                        .ok()
                        .and_then(|t| t.resolve_cursor())
                        .map(|c| matches!(c, TreeCursorTarget::Category(_)))
                        .unwrap_or(false);

                    if is_category {
                        if let Ok(mut tree) = tool_tree.single_mut() { tree.toggle_expand(); }
                    } else {
                        submit_if_ready(
                            &state, form_root, &model_list, &tool_tree,
                            &bootstrap, &conn_state, actor.as_deref(), &mut next_screen,
                        );
                    }
                } else {
                    submit_if_ready(
                        &state, form_root, &model_list, &tool_tree,
                        &bootstrap, &conn_state, actor.as_deref(), &mut next_screen,
                    );
                }
            }
            Action::Unfocus => {
                info!("Fork form cancelled");
                next_screen.set(Screen::Constellation);
            }
            _ => {}
        }
    }

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
}

// ============================================================================
// SUBMIT
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn submit_if_ready(
    state: &ForkFormState,
    form_root: &ForkFormRoot,
    model_list: &Query<&mut SelectableList, With<ForkFormModelContainer>>,
    tool_tree: &Query<&mut TreeView, With<ForkFormToolContainer>>,
    bootstrap: &BootstrapChannel,
    conn_state: &RpcConnectionState,
    actor: Option<&RpcActor>,
    next_screen: &mut ResMut<NextState<Screen>>,
) {
    if !state.models_loaded {
        return;
    }

    let Some(actor) = actor else {
        error!("Cannot fork: no active RPC actor");
        return;
    };

    // Get selected model from SelectableList
    let selected_model = model_list
        .single()
        .ok()
        .and_then(|list| {
            let item = list.selected_item()?;
            if item.is_header { return None; }
            let mut provider = String::new();
            for i in (0..list.selected).rev() {
                if list.items[i].is_header {
                    provider = list.items[i].label.clone();
                    break;
                }
            }
            Some((provider, item.label.clone()))
        });

    let disabled_tools: Vec<String> = tool_tree
        .single()
        .ok()
        .map(|tree| tree.disabled_items())
        .unwrap_or_default();
    let has_tool_filter = !disabled_tools.is_empty();

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

    let selected_provider = selected_model.as_ref().map(|(p, _)| p.clone());
    let selected_model_name = selected_model.as_ref().map(|(_, m)| m.clone());
    let parent_provider = state.parent_provider.clone();
    let parent_model = state.parent_model.clone();

    info!(
        "Fork submit: from={}, label='{}', model={:?}, disabled_tools={}",
        source_ctx_id.short(), fork_label, selected_model_name, disabled_tools.len()
    );

    next_screen.set(Screen::Constellation);

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
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

            let model_changed =
                selected_model_name != parent_model || selected_provider != parent_provider;
            if model_changed {
                if let (Some(provider), Some(model)) = (&selected_provider, &selected_model_name) {
                    match handle.set_context_model(new_ctx_id, provider, model).await {
                        Ok(true) => info!("Model set on {}: {}/{}", new_ctx_id.short(), provider, model),
                        Ok(false) => warn!("set_context_model returned false"),
                        Err(e) => error!("Failed to set model: {}", e),
                    }
                }
            }

            if has_tool_filter {
                use kaijutsu_client::rpc::ClientToolFilter;
                match handle
                    .set_context_tool_filter(new_ctx_id, ClientToolFilter::DenyList(disabled_tools))
                    .await
                {
                    Ok(true) => info!("Tool filter set on {}", new_ctx_id.short()),
                    Ok(false) => warn!("set_context_tool_filter returned false"),
                    Err(e) => error!("Failed to set tool filter: {}", e),
                }
            }

            let instance = Uuid::new_v4().to_string();
            let _ = bootstrap_tx.send(BootstrapCommand::SpawnActor {
                config, kernel_id, context_id: Some(new_ctx_id), instance,
            });
        })
        .detach();
}
