//! Full-viewport fork configuration form.
//!
//! Two-column layout: left (Name + Model), right (Tools tree).
//! Tools use a 2-level expanding tree: categories collapse/expand,
//! individual tools toggle with Space.
//!
//! Uses the Form schema system for declarative UI construction. The form root
//! is tagged with `DespawnOnExit(Screen::ForkForm)` for automatic cleanup.
//! Camera deactivation happens via `OnExit(Screen::Constellation)` in the screen
//! state machine — no manual camera hacks needed.
//!
//! Model is immutable on a context — fork to change it.

use bevy::prelude::*;
use kaijutsu_crdt::ContextId;
use kaijutsu_types::KernelId;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState};
use crate::input::action::Action;
use crate::input::events::{ActionFired, TextInputReceived};
use bevy_vello::prelude::UiVelloText;
use crate::text::{FontHandles, vello_style, bevy_color_to_brush};
use crate::ui::form::{
    handle_form_action, handle_form_space, ActiveFormField, AsyncSlot, ButtonDesc, FieldDesc, Form,
    FormActionResult, FormFieldContainer, FormLayout, FormLoadingText, FormPresentation, ListItem,
    SelectableList, TreeCategory, TreeItem, TreeView,
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
    pub parent_provider: Option<String>,
    pub parent_model: Option<String>,
    pub models_loaded: bool,
    pub tools_loaded: bool,
}

/// Field IDs.
const FIELD_NAME: u8 = 0;
const FIELD_MODEL: u8 = 1;
const FIELD_TOOLS: u8 = 2;

/// Marker for the name input text display.
#[derive(Component)]
struct ForkFormNameDisplay;

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
                init_name_display.run_if(in_state(Screen::ForkForm)),
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
        info!(
            "Opening fork form for context {}",
            msg.source_context_id.short()
        );

        next_screen.set(Screen::ForkForm);
        model_slot.clear();
        tool_slot.clear();

        let title = format!("Fork from {}", msg.source_context_id.short());

        commands.spawn((
            ForkFormRoot {
                source_context_id: msg.source_context_id,
                source_context: msg.source_context.clone(),
            },
            ForkFormState {
                name_text: String::new(),
                parent_provider: msg.parent_provider.clone(),
                parent_model: msg.parent_model.clone(),
                models_loaded: false,
                tools_loaded: false,
            },
            Form {
                title,
                layout: FormLayout::TwoColumn {
                    left: vec![
                        FieldDesc {
                            field_id: FIELD_NAME,
                            label: "Name (optional)".into(),
                            min_height: 36.0,
                            max_height: None,
                            loading_text: None,
                            bordered: true,
                        },
                        FieldDesc {
                            field_id: FIELD_MODEL,
                            label: "Model".into(),
                            min_height: 80.0,
                            max_height: Some(300.0),
                            loading_text: Some("Loading models...".into()),
                            bordered: true,
                        },
                    ],
                    right: vec![FieldDesc {
                        field_id: FIELD_TOOLS,
                        label: "Tools".into(),
                        min_height: 80.0,
                        max_height: Some(420.0),
                        loading_text: Some("Loading tools...".into()),
                        bordered: true,
                    }],
                },
                buttons: vec![
                    ButtonDesc {
                        label: "Cancel".into(),
                        primary: false,
                    },
                    ButtonDesc {
                        label: "Fork".into(),
                        primary: true,
                    },
                ],
                hints: "Tab: field | j/k: select | Space: toggle | Enter: expand/fork | Esc: cancel".into(),
                field_count: 3,
                initial_field: FIELD_MODEL,
            },
            FormPresentation::FullViewport {
                width: 720.0,
                max_height_pct: 85.0,
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
        ));

        info!("Fork form spawned");

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
                                    TreeItem {
                                        label: s.name.clone(),
                                        enabled: true,
                                    },
                                );
                            }
                            let categories: Vec<TreeCategory> = by_category
                                .into_iter()
                                .map(|(name, mut items)| {
                                    items.sort_by(|a, b| a.label.cmp(&b.label));
                                    TreeCategory {
                                        name,
                                        expanded: false,
                                        items,
                                    }
                                })
                                .collect();
                            *tool_sender.lock().unwrap() = Some(FetchedTools { categories });
                        }
                        Err(e) => {
                            error!("Failed to fetch tool schemas: {}", e);
                            *tool_sender.lock().unwrap() =
                                Some(FetchedTools { categories: vec![] });
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
// NAME FIELD INITIALIZATION
// ============================================================================

/// When the name field container is first created, spawn the name display text.
fn init_name_display(
    mut commands: Commands,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    containers: Query<(Entity, &FormFieldContainer), Added<FormFieldContainer>>,
    existing: Query<&ForkFormNameDisplay>,
) {
    if !existing.is_empty() {
        return;
    }

    for (entity, ffc) in containers.iter() {
        if ffc.0 == FIELD_NAME {
            let child = commands
                .spawn((
                    ForkFormNameDisplay,
                    UiVelloText {
                        value: "hex ID if blank".into(),
                        style: vello_style(&font_handles.mono, theme.fg_dim, 14.0),
                        ..default()
                    },
                    Node {
                        width: Val::Percent(100.0),
                        ..default()
                    },
                ))
                .id();
            commands.entity(entity).add_child(child);
        }
    }
}

// ============================================================================
// POLL ASYNC MODEL RESULT
// ============================================================================

fn poll_fork_form_models(
    mut commands: Commands,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    model_slot: Res<AsyncSlot<FetchedModels>>,
    mut state_query: Query<&mut ForkFormState>,
    loading_query: Query<(Entity, &FormLoadingText)>,
    container_query: Query<(Entity, &FormFieldContainer), Without<SelectableList>>,
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

    // Remove model loading text
    for (entity, flt) in loading_query.iter() {
        if flt.0 == FIELD_MODEL {
            commands.entity(entity).despawn();
        }
    }

    let Some((container, _)) = container_query
        .iter()
        .find(|(_, ffc)| ffc.0 == FIELD_MODEL)
    else {
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
                UiVelloText {
                    value: "No models available (will inherit parent)".into(),
                    style: vello_style(&font_handles.mono, theme.fg_dim, 13.0),
                    ..default()
                },
                Node {
                    width: Val::Percent(100.0),
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
            list_items
                .iter()
                .position(|i| !i.is_header)
                .unwrap_or(0)
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
    font_handles: Res<FontHandles>,
    tool_slot: Res<AsyncSlot<FetchedTools>>,
    mut state_query: Query<&mut ForkFormState>,
    loading_query: Query<(Entity, &FormLoadingText)>,
    container_query: Query<(Entity, &FormFieldContainer), Without<TreeView>>,
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

    // Remove tool loading text
    for (entity, flt) in loading_query.iter() {
        if flt.0 == FIELD_TOOLS {
            commands.entity(entity).despawn();
        }
    }

    state.tools_loaded = true;

    let Some((container, _)) = container_query
        .iter()
        .find(|(_, ffc)| ffc.0 == FIELD_TOOLS)
    else {
        return;
    };

    if fetched.categories.is_empty() {
        let hint = commands
            .spawn((
                UiVelloText {
                    value: "No tools available".into(),
                    style: vello_style(&font_handles.mono, theme.fg_dim, 13.0),
                    ..default()
                },
                Node {
                    width: Val::Percent(100.0),
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
    mut form_query: Query<
        (
            &Form,
            &mut ForkFormState,
            &ForkFormRoot,
            &mut ActiveFormField,
        ),
    >,
    mut name_display: Query<&mut UiVelloText, With<ForkFormNameDisplay>>,
    mut list_query: Query<(&FormFieldContainer, &mut SelectableList)>,
    mut tree_query: Query<(&FormFieldContainer, &mut TreeView)>,
    theme: Res<Theme>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((form, mut state, form_root, mut active_field)) = form_query.single_mut() else {
        return;
    };

    let mut text_changed = false;

    // Handle text input
    if active_field.0 == FIELD_NAME {
        for TextInputReceived(text) in text_events.read() {
            for c in text.chars() {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    state.name_text.push(c);
                    text_changed = true;
                }
            }
        }
    } else if active_field.0 == FIELD_TOOLS {
        for TextInputReceived(text) in text_events.read() {
            if text.contains(' ') {
                handle_form_space(active_field.0, &mut tree_query);
            }
        }
    } else {
        for _ in text_events.read() {}
    }

    // Handle actions
    for ActionFired(action) in actions.read() {
        // Backspace on name field — domain-specific, handle before form nav
        if matches!(action, Action::Backspace) && active_field.0 == FIELD_NAME {
            state.name_text.pop();
            text_changed = true;
            continue;
        }

        match handle_form_action(
            action,
            form,
            &mut active_field,
            &mut list_query,
            &mut tree_query,
        ) {
            FormActionResult::Submit => {
                submit_if_ready(
                    &state,
                    form_root,
                    &list_query,
                    &tree_query,
                    &bootstrap,
                    &conn_state,
                    actor.as_deref(),
                    &mut next_screen,
                );
            }
            FormActionResult::Cancel => {
                info!("Fork form cancelled");
                next_screen.set(Screen::Constellation);
            }
            FormActionResult::Consumed | FormActionResult::Ignored => {}
        }
    }

    if text_changed && let Ok(mut vt) = name_display.single_mut() {
        if state.name_text.is_empty() {
            vt.value = "hex ID if blank".to_string();
            vt.style.brush = bevy_color_to_brush(theme.fg_dim);
        } else {
            vt.value = state.name_text.clone();
            vt.style.brush = bevy_color_to_brush(theme.fg);
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
    list_query: &Query<(&FormFieldContainer, &mut SelectableList)>,
    tree_query: &Query<(&FormFieldContainer, &mut TreeView)>,
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

    // Get selected model from SelectableList in the model field
    let selected_model = list_query
        .iter()
        .find(|(ffc, _)| ffc.0 == FIELD_MODEL)
        .and_then(|(_, list)| {
            let item = list.selected_item()?;
            if item.is_header {
                return None;
            }
            let mut provider = String::new();
            for i in (0..list.selected).rev() {
                if list.items[i].is_header {
                    provider = list.items[i].label.clone();
                    break;
                }
            }
            Some((provider, item.label.clone()))
        });

    let disabled_tools: Vec<String> = tree_query
        .iter()
        .find(|(ffc, _)| ffc.0 == FIELD_TOOLS)
        .map(|(_, tree)| tree.disabled_items())
        .unwrap_or_default();
    let has_tool_filter = !disabled_tools.is_empty();

    let fork_label = state.name_text.clone();
    let handle = actor.handle.clone();
    let source_ctx_id = form_root.source_context_id;
    let config = conn_state.ssh_config.clone();
    let kernel_id = conn_state.kernel_id.unwrap_or_else(KernelId::nil);
    let bootstrap_tx = bootstrap.tx.clone();

    let selected_provider = selected_model.as_ref().map(|(p, _)| p.clone());
    let selected_model_name = selected_model.as_ref().map(|(_, m)| m.clone());
    let parent_provider = state.parent_provider.clone();
    let parent_model = state.parent_model.clone();

    info!(
        "Fork submit: from={}, label='{}', model={:?}, disabled_tools={}",
        source_ctx_id.short(),
        fork_label,
        selected_model_name,
        disabled_tools.len()
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
            if model_changed
                && let (Some(provider), Some(model)) =
                    (&selected_provider, &selected_model_name)
            {
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
                config,
                kernel_id,
                context_id: Some(new_ctx_id),
                instance,
            });
        })
        .detach();
}
