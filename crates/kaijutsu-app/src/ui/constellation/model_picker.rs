//! Model picker dialog for constellation nodes
//!
//! Pressing `m` on a focused constellation node opens a modal dialog
//! showing available LLM providers/models from `get_llm_config()`.
//! Select with j/k + Enter to call `set_default_model()`.
//!
//! Input is dispatched via `ActionFired` messages from the focus-based
//! input system (FocusArea::Dialog context).

use bevy::prelude::*;
use std::sync::{Arc, Mutex};

use crate::connection::RpcActor;
use crate::input::action::Action;
use crate::input::events::ActionFired;
use crate::input::focus::FocusArea;
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

use super::create_dialog::ModalDialogOpen;

// ============================================================================
// MESSAGES
// ============================================================================

/// Message to open the model picker for a specific context.
#[derive(Message, Clone, Debug)]
pub struct OpenModelPicker {
    pub context_name: String,
}

// ============================================================================
// RESOURCES
// ============================================================================

/// Shared slot for async LLM config fetch result.
#[derive(Resource, Default)]
pub struct ModelPickerResult(pub Arc<Mutex<Option<FetchedLlmConfig>>>);

/// Fetched LLM config data.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct FetchedLlmConfig {
    pub current_provider: String,
    pub current_model: String,
    /// (provider_name, default_model, available)
    pub models: Vec<(String, String, bool)>,
}

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the model picker dialog overlay.
#[derive(Component)]
#[allow(dead_code)]
pub struct ModelPickerDialog {
    pub context_name: String,
}

/// State for model selection within the picker.
#[derive(Component)]
pub struct ModelPickerSelection {
    pub items: Vec<(String, String)>, // (provider, model)
    pub selected: usize,
}

/// Marker for a model item row in the picker list.
#[derive(Component)]
pub struct ModelPickerItem {
    pub index: usize,
}

/// Marker for the "Loading..." text while fetching config.
#[derive(Component)]
pub struct ModelPickerLoading;

// ============================================================================
// SYSTEMS
// ============================================================================

pub fn setup_model_picker_systems(app: &mut App) {
    app.add_message::<OpenModelPicker>()
        .init_resource::<ModelPickerResult>()
        .add_systems(
            Update,
            (
                handle_open_model_picker,
                poll_model_picker_result,
                handle_model_picker_input,
            )
                .chain(),
        );
}

/// Handle `OpenModelPicker` — kick off async fetch, show loading dialog.
fn handle_open_model_picker(
    mut commands: Commands,
    mut events: MessageReader<OpenModelPicker>,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut focus: ResMut<FocusArea>,
    existing: Query<Entity, With<ModelPickerDialog>>,
    theme: Res<Theme>,
    actor: Option<Res<RpcActor>>,
    result_slot: Res<ModelPickerResult>,
) {
    if !existing.is_empty() {
        return;
    }

    for event in events.read() {
        let Some(ref actor) = actor else {
            warn!("Cannot open model picker: no active RPC actor");
            continue;
        };

        modal_state.0 = true;
        *focus = FocusArea::Dialog;

        // Clear any stale result
        *result_slot.0.lock().unwrap() = None;

        // Spawn loading dialog
        commands
            .spawn((
                ModelPickerDialog {
                    context_name: event.context_name.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                ZIndex(crate::constants::ZLayer::MODAL),
                Interaction::None,
            ))
            .with_children(|overlay| {
                overlay
                    .spawn((
                        Node {
                            width: Val::Px(300.0),
                            min_height: Val::Px(120.0),
                            flex_direction: FlexDirection::Column,
                            padding: UiRect::all(Val::Px(20.0)),
                            border_radius: BorderRadius::all(Val::Px(8.0)),
                            row_gap: Val::Px(12.0),
                            ..default()
                        },
                        BackgroundColor(theme.panel_bg),
                        BorderColor::all(theme.border),
                        Outline::new(Val::Px(1.0), Val::ZERO, theme.border),
                    ))
                    .with_children(|dialog| {
                        dialog.spawn((
                            MsdfUiText::new(format!("Model: @{}", event.context_name))
                                .with_font_size(16.0)
                                .with_color(theme.fg),
                            UiTextPositionCache::default(),
                            Node::default(),
                        ));
                        dialog.spawn((
                            ModelPickerLoading,
                            MsdfUiText::new("Loading models...")
                                .with_font_size(12.0)
                                .with_color(theme.fg_dim),
                            UiTextPositionCache::default(),
                            Node::default(),
                        ));
                    });
            });

        // Fetch config async via shared slot
        let handle = actor.handle.clone();
        let slot = result_slot.0.clone();

        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                match handle.get_llm_config().await {
                    Ok(config) => {
                        let models: Vec<_> = config
                            .providers
                            .iter()
                            .map(|p| (p.name.clone(), p.default_model.clone(), p.available))
                            .collect();
                        *slot.lock().unwrap() = Some(FetchedLlmConfig {
                            current_provider: config.default_provider,
                            current_model: config.default_model,
                            models,
                        });
                    }
                    Err(e) => {
                        error!("Failed to fetch LLM config: {}", e);
                    }
                }
            })
            .detach();

        break; // Only handle one open request
    }
}

/// Poll the shared result slot. When data arrives, populate the dialog.
fn poll_model_picker_result(
    mut commands: Commands,
    theme: Res<Theme>,
    result_slot: Res<ModelPickerResult>,
    dialogs: Query<Entity, With<ModelPickerDialog>>,
    loading: Query<Entity, With<ModelPickerLoading>>,
    existing_selection: Query<&ModelPickerSelection>,
) {
    // Nothing to do if no dialog or already populated
    if dialogs.is_empty() || !existing_selection.is_empty() {
        return;
    }

    // Check for result
    let data = result_slot.0.lock().unwrap().take();
    let Some(data) = data else {
        return;
    };

    let Ok(dialog_entity) = dialogs.single() else {
        return;
    };

    // Remove loading indicator
    for entity in loading.iter() {
        commands.entity(entity).despawn();
    }

    // Find current selection index
    let mut selected = 0;
    let items: Vec<(String, String)> = data
        .models
        .iter()
        .enumerate()
        .map(|(i, (provider, model, _))| {
            if *provider == data.current_provider {
                selected = i;
            }
            (provider.clone(), model.clone())
        })
        .collect();

    if items.is_empty() {
        return;
    }

    // Add selection state to dialog
    commands.entity(dialog_entity).insert(ModelPickerSelection {
        items: items.clone(),
        selected,
    });

    // Spawn model items as children of the dialog entity
    for (i, (provider, model, available)) in data.models.iter().enumerate() {
        let is_current = i == selected;
        let color = if !available {
            theme.fg_dim.with_alpha(0.3)
        } else if is_current {
            theme.accent
        } else {
            theme.fg
        };
        let bg = if is_current {
            theme.accent.with_alpha(0.15)
        } else {
            Color::NONE
        };
        let prefix = if is_current { "▸ " } else { "  " };

        let item = commands
            .spawn((
                ModelPickerItem { index: i },
                Node {
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(bg),
            ))
            .with_children(|row| {
                row.spawn((
                    MsdfUiText::new(format!("{}{}/{}", prefix, provider, model))
                        .with_font_size(13.0)
                        .with_color(color),
                    UiTextPositionCache::default(),
                    Node::default(),
                ));
            })
            .id();

        commands.entity(dialog_entity).add_child(item);
    }

    // Hint text
    let hint = commands
        .spawn(Node {
            margin: UiRect::top(Val::Px(8.0)),
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((
                MsdfUiText::new("j/k: navigate │ Enter: select │ Esc: cancel")
                    .with_font_size(10.0)
                    .with_color(theme.fg_dim),
                UiTextPositionCache::default(),
                Node::default(),
            ));
        })
        .id();
    commands.entity(dialog_entity).add_child(hint);
}

/// Handle actions in the model picker (j/k/Enter/Escape via ActionFired).
fn handle_model_picker_input(
    mut commands: Commands,
    mut actions: MessageReader<ActionFired>,
    mut modal_state: ResMut<ModalDialogOpen>,
    mut focus: ResMut<FocusArea>,
    mut dialogs: Query<(Entity, &ModelPickerDialog, Option<&mut ModelPickerSelection>)>,
    mut items: Query<(&ModelPickerItem, &mut BackgroundColor, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
    theme: Res<Theme>,
    actor: Option<Res<RpcActor>>,
) {
    let Ok((dialog_entity, _dialog, selection)) = dialogs.single_mut() else {
        return;
    };

    let Some(mut selection) = selection else {
        // Still loading — only handle Escape/Unfocus
        for ActionFired(action) in actions.read() {
            if matches!(action, Action::Unfocus) {
                close_model_picker(&mut commands, dialog_entity, &mut modal_state, &mut focus);
            }
        }
        return;
    };

    for ActionFired(action) in actions.read() {
        match action {
            Action::Unfocus => {
                close_model_picker(&mut commands, dialog_entity, &mut modal_state, &mut focus);
                return;
            }
            Action::Activate => {
                let (provider, model) = &selection.items[selection.selected];
                info!("Model selected: {}/{}", provider, model);

                if let Some(ref actor) = actor {
                    let handle = actor.handle.clone();
                    let provider = provider.clone();
                    let model = model.clone();

                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            match handle.set_default_model(&provider, &model).await {
                                Ok(true) => info!("Model set to {}/{}", provider, model),
                                Ok(false) => warn!("set_default_model returned false"),
                                Err(e) => error!("Failed to set model: {}", e),
                            }
                        })
                        .detach();
                }

                close_model_picker(&mut commands, dialog_entity, &mut modal_state, &mut focus);
                return;
            }
            Action::FocusNextBlock => {
                let old_selected = selection.selected;
                if selection.selected < selection.items.len() - 1 {
                    selection.selected += 1;
                }
                if old_selected != selection.selected {
                    update_picker_visuals(&selection, &mut items, &mut texts, &theme);
                }
            }
            Action::FocusPrevBlock => {
                let old_selected = selection.selected;
                if selection.selected > 0 {
                    selection.selected -= 1;
                }
                if old_selected != selection.selected {
                    update_picker_visuals(&selection, &mut items, &mut texts, &theme);
                }
            }
            _ => {}
        }
    }
}

/// Update visual selection in the model picker list.
fn update_picker_visuals(
    selection: &ModelPickerSelection,
    items: &mut Query<(&ModelPickerItem, &mut BackgroundColor, &Children)>,
    texts: &mut Query<&mut MsdfUiText>,
    theme: &Theme,
) {
    for (item, mut bg, children) in items.iter_mut() {
        let is_selected = item.index == selection.selected;
        *bg = BackgroundColor(if is_selected {
            theme.accent.with_alpha(0.15)
        } else {
            Color::NONE
        });

        for child in children.iter() {
            if let Ok(mut text) = texts.get_mut(child) {
                let (provider, model) = &selection.items[item.index];
                let prefix = if is_selected { "▸ " } else { "  " };
                text.text = format!("{}{}/{}", prefix, provider, model);
                text.color = bevy_to_rgba8(if is_selected {
                    theme.accent
                } else {
                    theme.fg
                });
            }
        }
    }
}

/// Close the model picker and restore focus.
///
/// Known limitation: hardcoded return to Constellation (see create_dialog::close_dialog).
fn close_model_picker(
    commands: &mut Commands,
    dialog_entity: Entity,
    modal_state: &mut ModalDialogOpen,
    focus: &mut FocusArea,
) {
    modal_state.0 = false;
    *focus = FocusArea::Constellation;
    commands.entity(dialog_entity).despawn();
}
