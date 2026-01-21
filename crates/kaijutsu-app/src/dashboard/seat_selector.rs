//! Seat Selector - UI component for viewing and switching seats
//!
//! Displays the current seat (if any) and allows switching between
//! active seats or returning to the dashboard.

use bevy::prelude::*;

use crate::connection::{ConnectionCommand, ConnectionCommands};
use crate::text::{GlyphonUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

use super::DashboardState;

// ============================================================================
// Components
// ============================================================================

/// Marker for the seat selector container
#[derive(Component)]
pub struct SeatSelector;

/// Marker for the current seat display text
#[derive(Component)]
pub struct CurrentSeatText;

/// Marker for the seat count indicator
#[derive(Component)]
pub struct SeatCountText;

/// Marker for a seat option in the dropdown
#[derive(Component)]
pub struct SeatOption {
    pub index: usize,
}

/// Marker for the "Dashboard" button/option
#[derive(Component)]
pub struct DashboardButton;

/// Marker for the dropdown container (hidden by default)
#[derive(Component)]
pub struct SeatDropdown;

// ============================================================================
// Setup
// ============================================================================

/// Spawn the seat selector UI in the header area (called from main.rs header setup)
pub fn spawn_seat_selector(commands: &mut Commands, theme: &Theme) -> Entity {
    commands
        .spawn((
            SeatSelector,
            Button,
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                padding: UiRect::axes(Val::Px(12.0), Val::Px(6.0)),
                border: UiRect::all(Val::Px(1.0)),
                column_gap: Val::Px(8.0),
                ..default()
            },
            BorderColor::all(theme.border),
            BackgroundColor(theme.panel_bg),
        ))
        .with_children(|selector| {
            // Seat icon (席)
            selector.spawn((
                GlyphonUiText::new("席")
                    .with_font_size(14.0)
                    .with_color(theme.accent),
                UiTextPositionCache::default(),
                Node {
                    min_width: Val::Px(20.0),
                    min_height: Val::Px(20.0),
                    ..default()
                },
            ));

            // Current seat text
            selector.spawn((
                CurrentSeatText,
                GlyphonUiText::new("No seat")
                    .with_font_size(12.0)
                    .with_color(theme.fg_dim),
                UiTextPositionCache::default(),
                Node {
                    min_width: Val::Px(150.0),
                    min_height: Val::Px(16.0),
                    ..default()
                },
            ));

            // Seat count badge (shows when multiple seats)
            selector.spawn((
                SeatCountText,
                GlyphonUiText::new("")
                    .with_font_size(10.0)
                    .with_color(theme.bg),
                UiTextPositionCache::default(),
                Node {
                    min_width: Val::Px(20.0),
                    min_height: Val::Px(16.0),
                    padding: UiRect::axes(Val::Px(4.0), Val::Px(2.0)),
                    display: Display::None, // Hidden until multiple seats
                    ..default()
                },
                BackgroundColor(theme.accent),
            ));
        })
        .id()
}

/// Spawn the dropdown menu (initially hidden) - call at root level for absolute positioning
pub fn spawn_seat_dropdown(commands: &mut Commands, theme: &Theme) -> Entity {
    commands
        .spawn((
            SeatDropdown,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(40.0), // Below the selector
                right: Val::Px(0.0),
                flex_direction: FlexDirection::Column,
                min_width: Val::Px(250.0),
                padding: UiRect::all(Val::Px(4.0)),
                border: UiRect::all(Val::Px(1.0)),
                display: Display::None, // Hidden by default
                ..default()
            },
            BorderColor::all(theme.border),
            BackgroundColor(theme.panel_bg),
            ZIndex(200), // Above everything
        ))
        .with_children(|dropdown| {
            // Dashboard option (always visible)
            dropdown
                .spawn((
                    DashboardButton,
                    Button,
                    Node {
                        width: Val::Percent(100.0),
                        padding: UiRect::all(Val::Px(8.0)),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ))
                .with_children(|btn| {
                    btn.spawn((
                        GlyphonUiText::new("◀ Dashboard")
                            .with_font_size(12.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(100.0),
                            min_height: Val::Px(16.0),
                            ..default()
                        },
                    ));
                });
        })
        .id()
}

// ============================================================================
// Systems
// ============================================================================

/// Update the seat selector display based on current state
pub fn update_seat_selector(
    state: Res<DashboardState>,
    mut current_text: Query<&mut GlyphonUiText, With<CurrentSeatText>>,
    mut count_text: Query<(&mut GlyphonUiText, &mut Node), (With<SeatCountText>, Without<CurrentSeatText>)>,
    theme: Res<Theme>,
) {
    if !state.is_changed() {
        return;
    }

    // Update current seat text
    for mut text in current_text.iter_mut() {
        if let Some(seat) = &state.current_seat {
            // Format: @nick:instance • context
            text.text = format!("@{}:{} • {}", seat.id.nick, seat.id.instance, seat.id.context);
            text.color = crate::text::bevy_to_glyphon_color(theme.accent);
        } else {
            text.text = "No seat".into();
            text.color = crate::text::bevy_to_glyphon_color(theme.fg_dim);
        }
    }

    // Update seat count badge
    for (mut text, mut node) in count_text.iter_mut() {
        let count = state.my_seats.len();
        if count > 1 {
            text.text = format!("{}", count);
            node.display = Display::Flex;
        } else {
            node.display = Display::None;
        }
    }
}

/// Handle clicking on the seat selector (toggle dropdown)
pub fn handle_seat_selector_click(
    interaction: Query<&Interaction, (Changed<Interaction>, With<SeatSelector>)>,
    mut dropdown: Query<&mut Node, With<SeatDropdown>>,
) {
    for interaction in interaction.iter() {
        if *interaction == Interaction::Pressed {
            for mut node in dropdown.iter_mut() {
                // Toggle visibility
                node.display = if node.display == Display::None {
                    Display::Flex
                } else {
                    Display::None
                };
            }
        }
    }
}

/// Handle clicking on the Dashboard button
pub fn handle_dashboard_click(
    interaction: Query<&Interaction, (Changed<Interaction>, With<DashboardButton>)>,
    mut state: ResMut<DashboardState>,
    mut dropdown: Query<&mut Node, With<SeatDropdown>>,
    conn: Res<ConnectionCommands>,
) {
    for interaction in interaction.iter() {
        if *interaction == Interaction::Pressed {
            // Leave current seat and show dashboard
            if state.current_seat.is_some() {
                conn.send(ConnectionCommand::LeaveSeat);
            }
            state.visible = true;

            // Hide dropdown
            for mut node in dropdown.iter_mut() {
                node.display = Display::None;
            }
        }
    }
}

/// Handle clicking on a seat option in the dropdown
pub fn handle_seat_option_click(
    interaction: Query<(&Interaction, &SeatOption), Changed<Interaction>>,
    state: Res<DashboardState>,
    mut dropdown: Query<&mut Node, With<SeatDropdown>>,
    conn: Res<ConnectionCommands>,
) {
    for (interaction, option) in interaction.iter() {
        if *interaction == Interaction::Pressed {
            if let Some(seat_info) = state.my_seats.get(option.index) {
                // Switch to this seat
                conn.send(ConnectionCommand::TakeSeat {
                    nick: seat_info.id.nick.clone(),
                    instance: seat_info.id.instance.clone(),
                    kernel: seat_info.id.kernel.clone(),
                    context: seat_info.id.context.clone(),
                });

                // Hide dropdown
                for mut node in dropdown.iter_mut() {
                    node.display = Display::None;
                }
            }
        }
    }
}

/// Close dropdown when clicking outside
pub fn close_dropdown_on_outside_click(
    mouse: Res<ButtonInput<MouseButton>>,
    selector_interaction: Query<&Interaction, With<SeatSelector>>,
    dropdown_interaction: Query<&Interaction, With<SeatDropdown>>,
    mut dropdown: Query<&mut Node, With<SeatDropdown>>,
) {
    if mouse.just_pressed(MouseButton::Left) {
        // Check if click was on selector or dropdown
        let on_selector = selector_interaction
            .iter()
            .any(|i| *i != Interaction::None);
        let on_dropdown = dropdown_interaction
            .iter()
            .any(|i| *i != Interaction::None);

        if !on_selector && !on_dropdown {
            for mut node in dropdown.iter_mut() {
                node.display = Display::None;
            }
        }
    }
}

/// Rebuild dropdown options when seats list changes
pub fn rebuild_seat_options(
    state: Res<DashboardState>,
    mut commands: Commands,
    dropdown: Query<Entity, With<SeatDropdown>>,
    existing_options: Query<Entity, With<SeatOption>>,
    theme: Res<Theme>,
) {
    if !state.is_changed() {
        return;
    }

    // Remove existing seat options
    for entity in existing_options.iter() {
        commands.entity(entity).try_despawn();
    }

    // Add new seat options to dropdown
    for dropdown_entity in dropdown.iter() {
        commands.entity(dropdown_entity).with_children(|dropdown| {
            for (i, seat) in state.my_seats.iter().enumerate() {
                // Skip current seat
                if let Some(current) = &state.current_seat {
                    if seat.id == current.id {
                        continue;
                    }
                }

                dropdown
                    .spawn((
                        SeatOption { index: i },
                        Button,
                        Node {
                            width: Val::Percent(100.0),
                            padding: UiRect::all(Val::Px(8.0)),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                    ))
                    .with_children(|btn| {
                        btn.spawn((
                            GlyphonUiText::new(format!(
                                "@{}:{} • {}",
                                seat.id.nick, seat.id.instance, seat.id.context
                            ))
                            .with_font_size(12.0)
                            .with_color(theme.fg_dim),
                            UiTextPositionCache::default(),
                            Node {
                                min_width: Val::Px(200.0),
                                min_height: Val::Px(16.0),
                                ..default()
                            },
                        ));
                    });
            }
        });
    }
}
