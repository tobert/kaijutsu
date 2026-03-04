//! Declarative form schema and builder system.
//!
//! A `Form` component describes a form's structure declaratively — title, layout,
//! fields, buttons, hints. The `build_form` system fires on `Added<Form>` and
//! produces the full entity tree as children of the form entity.
//!
//! Two presentations: `FormPresentation::FullViewport` (fork form) and
//! `FormPresentation::Modal` (model picker). The caller attaches both `Form`
//! and `FormPresentation` to the entity; `build_form` reads both.
//!
//! Field containers are content-agnostic: domain code queries `FormFieldContainer(id)`
//! to insert `SelectableList`, `TreeView`, or any future content component.

use bevy::prelude::*;

use bevy_vello::prelude::{UiVelloScene, UiVelloText, VelloFont};
use crate::text::{FontHandles, vello_style};
use crate::ui::form::field::{ActiveFormField, FormField};
use crate::ui::form::text::vello_label;
use crate::ui::theme::Theme;

// ============================================================================
// SCHEMA TYPES
// ============================================================================

/// Declarative form. Attach to an entity alongside `FormPresentation` →
/// `build_form` produces the entity tree.
#[derive(Component)]
pub struct Form {
    pub title: String,
    pub layout: FormLayout,
    pub buttons: Vec<ButtonDesc>,
    pub hints: String,
    pub field_count: u8,
    pub initial_field: u8,
}

/// How fields are arranged within the form.
pub enum FormLayout {
    /// Single column of fields.
    Column(Vec<FieldDesc>),
    /// Two-column layout (e.g. fork form: left=Name+Model, right=Tools).
    TwoColumn {
        left: Vec<FieldDesc>,
        right: Vec<FieldDesc>,
    },
}

/// Content-agnostic field description. The container gets `FormFieldContainer(field_id)`;
/// domain code inserts content components on it later.
pub struct FieldDesc {
    pub field_id: u8,
    /// Label above the field. Empty string = no label shown.
    pub label: String,
    pub min_height: f32,
    pub max_height: Option<f32>,
    /// If set, spawns a loading placeholder text inside the field container.
    pub loading_text: Option<String>,
    /// Whether to show a bordered section with outline (true) or bare container (false).
    pub bordered: bool,
}

/// A button in the button row.
pub struct ButtonDesc {
    pub label: String,
    pub primary: bool,
}

/// How the form is presented. Determines the outermost wrapping.
#[derive(Component)]
pub enum FormPresentation {
    /// Full-viewport overlay (centered content with max width/height).
    /// `max_width: None` means fill container width (responsive).
    FullViewport { max_width: Option<f32>, max_height_pct: f32 },
    /// Modal dialog over a backdrop.
    Modal { max_width: f32, min_height: f32 },
}

/// Marker on each field's inner container. Domain code queries this to insert content.
#[derive(Component)]
pub struct FormFieldContainer(pub u8);

/// Marker on the form root entity. Inserted by `build_form`.
#[derive(Component)]
pub struct FormRoot;

/// Marker on loading placeholder text within a field container.
#[derive(Component)]
pub struct FormLoadingText(pub u8);

/// Marker on the modal dialog panel entity. The `sync_modal_panel_scene` system
/// draws the panel background + border from `ComputedNode` dimensions.
#[derive(Component)]
pub struct ModalPanel;

/// Marker on button entities. The `sync_form_button_scenes` system
/// draws button backgrounds from `ComputedNode` dimensions.
#[derive(Component)]
pub struct FormButton {
    pub primary: bool,
}

/// Marker on the full-viewport form overlay. Background drawn via Vello scene
/// (not `BackgroundColor`, which would cover all Vello text underneath).
#[derive(Component)]
pub struct FormOverlay;

// ============================================================================
// BUILD SYSTEM
// ============================================================================

/// Fires on `Added<Form>`. Reads `Form` + `FormPresentation` and spawns
/// the complete entity tree as children.
pub fn build_form(
    mut commands: Commands,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    query: Query<(Entity, &Form, &FormPresentation), Added<Form>>,
) {
    for (entity, form, presentation) in query.iter() {
        info!("build_form: building {:?} with {} fields", entity, form.field_count);
        // Insert FormRoot + ActiveFormField on the form entity
        commands.entity(entity).insert((
            FormRoot,
            ActiveFormField(form.initial_field),
        ));

        match presentation {
            FormPresentation::FullViewport {
                max_width,
                max_height_pct,
            } => {
                build_full_viewport(&mut commands, entity, form, &theme, &font_handles, *max_width, *max_height_pct);
            }
            FormPresentation::Modal { max_width, min_height } => {
                build_modal(&mut commands, entity, form, &theme, &font_handles, *max_width, *min_height);
            }
        }
    }
}

// ============================================================================
// FULL VIEWPORT BUILDER
// ============================================================================

fn build_full_viewport(
    commands: &mut Commands,
    form_entity: Entity,
    form: &Form,
    theme: &Theme,
    font_handles: &FontHandles,
    max_width: Option<f32>,
    max_height_pct: f32,
) {
    let font = font_handles.mono.clone();
    // Content wrapper (centered, constrained)
    let mut node = Node {
        width: Val::Percent(100.0),
        max_height: Val::Percent(max_height_pct),
        flex_direction: FlexDirection::Column,
        row_gap: Val::Px(16.0),
        padding: UiRect::all(Val::Px(24.0)),
        ..default()
    };
    if let Some(mw) = max_width {
        node.max_width = Val::Px(mw);
    }
    let content = commands
        .spawn(node)
        .with_children(|content| {
            // Title
            vello_label(content, &font, &form.title, 18.0, theme.fg);

            // Layout container
            match &form.layout {
                FormLayout::Column(fields) => {
                    content
                        .spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(16.0),
                            ..default()
                        })
                        .with_children(|col| {
                            for field in fields {
                                spawn_field(col, field, theme, &font);
                            }
                        });
                }
                FormLayout::TwoColumn { left, right } => {
                    content
                        .spawn(Node {
                            width: Val::Percent(100.0),
                            flex_direction: FlexDirection::Row,
                            column_gap: Val::Px(20.0),
                            ..default()
                        })
                        .with_children(|columns| {
                            // Left column
                            columns
                                .spawn(Node {
                                    flex_basis: Val::Percent(50.0),
                                    flex_grow: 1.0,
                                    flex_direction: FlexDirection::Column,
                                    row_gap: Val::Px(16.0),
                                    ..default()
                                })
                                .with_children(|col| {
                                    for field in left {
                                        spawn_field(col, field, theme, &font);
                                    }
                                });

                            // Right column
                            columns
                                .spawn(Node {
                                    flex_basis: Val::Percent(50.0),
                                    flex_grow: 1.0,
                                    flex_direction: FlexDirection::Column,
                                    row_gap: Val::Px(6.0),
                                    ..default()
                                })
                                .with_children(|col| {
                                    for field in right {
                                        spawn_field(col, field, theme, &font);
                                    }
                                });
                        });
                }
            }

            // Button row
            if !form.buttons.is_empty() {
                content
                    .spawn(Node {
                        width: Val::Percent(100.0),
                        flex_direction: FlexDirection::Row,
                        justify_content: JustifyContent::End,
                        column_gap: Val::Px(12.0),
                        margin: UiRect::top(Val::Px(8.0)),
                        ..default()
                    })
                    .with_children(|buttons| {
                        for desc in &form.buttons {
                            spawn_button(buttons, desc, theme, &font);
                        }
                    });
            }

            // Hints
            if !form.hints.is_empty() {
                vello_label(content, &font, &form.hints, 11.0, theme.fg_dim);
            }
        })
        .id();

    commands.entity(form_entity).add_child(content);
}

// ============================================================================
// MODAL BUILDER
// ============================================================================

fn build_modal(
    commands: &mut Commands,
    form_entity: Entity,
    form: &Form,
    theme: &Theme,
    font_handles: &FontHandles,
    max_width: f32,
    min_height: f32,
) {
    let font = font_handles.mono.clone();
    // Dialog panel — Vello scene draws the border/bg (sync_modal_panel_scene fills it in)
    let dialog = commands
        .spawn((
            ModalPanel,
            Node {
                width: Val::Percent(100.0),
                max_width: Val::Px(max_width),
                min_height: Val::Px(min_height),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(20.0)),
                row_gap: Val::Px(12.0),
                ..default()
            },
            UiVelloScene::default(),
        ))
        .with_children(|dialog| {
            // Title
            vello_label(dialog, &font, &form.title, 16.0, theme.fg);

            // Fields
            let all_fields = match &form.layout {
                FormLayout::Column(fields) => fields.as_slice(),
                FormLayout::TwoColumn { left, .. } => {
                    // Modal shouldn't use TwoColumn, but handle gracefully
                    left.as_slice()
                }
            };

            for field in all_fields {
                if field.bordered {
                    spawn_field(dialog, field, theme, &font);
                } else {
                    spawn_bare_field(dialog, field, theme, &font);
                }
            }

            // Hints
            if !form.hints.is_empty() {
                vello_label(dialog, &font, &form.hints, 11.0, theme.fg_dim);
            }
        })
        .id();

    commands.entity(form_entity).add_child(dialog);
}

// ============================================================================
// FIELD SPAWNING
// ============================================================================

/// Spawn a bordered field section: label + outlined container with FormFieldContainer marker.
fn spawn_field(
    parent: &mut ChildSpawnerCommands,
    desc: &FieldDesc,
    theme: &Theme,
    font: &Handle<VelloFont>,
) {
    parent
        .spawn(Node {
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(6.0),
            ..default()
        })
        .with_children(|section| {
            // Label (if non-empty)
            if !desc.label.is_empty() {
                vello_label(section, font, &desc.label, 12.0, theme.fg_dim);
            }

            // Bordered container — Vello scene draws the border
            let mut node = Node {
                width: Val::Percent(100.0),
                min_height: Val::Px(desc.min_height),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                row_gap: Val::Px(2.0),
                overflow: Overflow::scroll_y(),
                ..default()
            };
            if let Some(max) = desc.max_height {
                node.max_height = Val::Px(max);
            }

            let font = font.clone();
            let fg_dim = theme.fg_dim;
            let loading_text = desc.loading_text.clone();
            let field_id = desc.field_id;

            section
                .spawn((
                    FormField {
                        field_id: desc.field_id,
                    },
                    FormFieldContainer(desc.field_id),
                    node,
                    UiVelloScene::default(),
                    Interaction::None,
                ))
                .with_children(|container| {
                    if let Some(ref text) = loading_text {
                        container.spawn((
                            FormLoadingText(field_id),
                            UiVelloText {
                                value: text.clone(),
                                style: vello_style(&font, fg_dim, 14.0),
                                ..default()
                            },
                            Node {
                                width: Val::Percent(100.0),
                                ..default()
                            },
                        ));
                    }
                });
        });
}

/// Spawn a bare field container (no border, no label). Used for simple modal fields.
fn spawn_bare_field(
    parent: &mut ChildSpawnerCommands,
    desc: &FieldDesc,
    theme: &Theme,
    font: &Handle<VelloFont>,
) {
    let font = font.clone();
    let fg_dim = theme.fg_dim;
    let loading_text = desc.loading_text.clone();
    let field_id = desc.field_id;

    parent
        .spawn((
            FormFieldContainer(desc.field_id),
            Node {
                width: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
        ))
        .with_children(|container| {
            if let Some(ref text) = loading_text {
                container.spawn((
                    FormLoadingText(field_id),
                    UiVelloText {
                        value: text.clone(),
                        style: vello_style(&font, fg_dim, 12.0),
                        ..default()
                    },
                    Node {
                        width: Val::Percent(100.0),
                        ..default()
                    },
                ));
            }
        });
}

// ============================================================================
// BUTTON SPAWNING
// ============================================================================

fn spawn_button(
    parent: &mut ChildSpawnerCommands,
    desc: &ButtonDesc,
    theme: &Theme,
    font: &Handle<VelloFont>,
) {
    let text_color = if desc.primary { theme.accent } else { theme.fg_dim };

    // Scene is filled in by sync_form_button_scenes (PostUpdate, after layout)
    parent.spawn((
        FormButton { primary: desc.primary },
        Node {
            padding: UiRect::axes(Val::Px(20.0), Val::Px(10.0)),
            ..default()
        },
        UiVelloText {
            value: desc.label.clone(),
            style: vello_style(font, text_color, 13.0),
            ..default()
        },
        UiVelloScene::default(),
    ));
}
