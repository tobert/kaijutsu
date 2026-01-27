//! Dashboard - Lobby experience for kernel/context/seat navigation
//!
//! The dashboard provides a 3-column layout for browsing:
//! - Kernels (left)
//! - Contexts within selected kernel (middle)
//! - User's seats (right)
//!
//! ## State-Driven Visibility
//!
//! The dashboard's visibility is controlled by `AppScreen` state:
//! - `AppScreen::Dashboard` → Dashboard visible, Conversation hidden
//! - `AppScreen::Conversation` → Dashboard hidden, Conversation visible
//!
//! State transitions are triggered by:
//! - `SeatTaken` event → switch to `AppScreen::Conversation`
//! - `SeatLeft` event → switch to `AppScreen::Dashboard`

pub mod seat_selector;

use bevy::prelude::*;
use kaijutsu_client::{Context, KernelInfo, SeatInfo};

use crate::connection::{ConnectionCommand, ConnectionCommands, ConnectionEvent};
use crate::shaders::nine_slice::{ChasingBorder, ChasingBorderMaterial};
use crate::text::{GlyphonUiText, UiTextPositionCache};
use crate::ui::state::AppScreen;
use crate::ui::theme::Theme;
use crate::HeaderContainer;

pub use seat_selector::{spawn_seat_dropdown, spawn_seat_selector};

/// Plugin for the dashboard/lobby experience
pub struct DashboardPlugin;

impl Plugin for DashboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DashboardState>()
            // PostStartup ensures ContentArea and header exist before we add dashboard to them
            .add_systems(PostStartup, (setup_dashboard, setup_seat_selector_ui).chain())
            .add_systems(
                Update,
                (
                    // Dashboard systems
                    handle_dashboard_events,
                    handle_kernel_selection,
                    handle_context_selection,
                    handle_take_seat,
                    handle_dashboard_keyboard,
                    // List rebuild systems
                    rebuild_kernel_list,
                    rebuild_context_list,
                    rebuild_seats_list,
                    // Seat selector systems
                    seat_selector::update_seat_selector,
                    seat_selector::handle_seat_selector_click,
                    seat_selector::handle_dashboard_click,
                    seat_selector::handle_seat_option_click,
                    seat_selector::close_dropdown_on_outside_click,
                    seat_selector::rebuild_seat_options,
                    seat_selector::sync_dropdown_visibility,
                ),
            );
    }
}

/// State for the dashboard
///
/// Note: Visibility is now controlled by `AppScreen` state, not a field here.
#[derive(Resource, Default)]
pub struct DashboardState {
    /// Available kernels
    pub kernels: Vec<KernelInfo>,
    /// Currently selected kernel index
    pub selected_kernel: Option<usize>,
    /// Contexts in the selected kernel
    pub contexts: Vec<Context>,
    /// Currently selected context index
    pub selected_context: Option<usize>,
    /// User's active seats across all kernels
    pub my_seats: Vec<SeatInfo>,
    /// Current seat (if any)
    pub current_seat: Option<SeatInfo>,
}

impl DashboardState {
    /// Get the selected kernel, if any
    pub fn selected_kernel(&self) -> Option<&KernelInfo> {
        self.selected_kernel.and_then(|i| self.kernels.get(i))
    }

    /// Get the selected context, if any
    pub fn selected_context(&self) -> Option<&Context> {
        self.selected_context.and_then(|i| self.contexts.get(i))
    }
}

// ============================================================================
// Markers
// ============================================================================

/// Marker for the dashboard root node
#[derive(Component)]
pub struct DashboardRoot;

/// Marker for the kernel list container
#[derive(Component)]
pub struct KernelList;

/// Marker for a kernel list item
#[derive(Component)]
pub struct KernelListItem {
    pub index: usize,
}

/// Marker for the context list container
#[derive(Component)]
pub struct ContextList;

/// Marker for a context list item
#[derive(Component)]
pub struct ContextListItem {
    pub index: usize,
}

/// Marker for the seats list container
#[derive(Component)]
pub struct SeatsList;

/// Marker for a seat list item
#[derive(Component)]
pub struct SeatListItem;

/// Marker for the "Take Seat" button
#[derive(Component)]
pub struct TakeSeatButton;

// ============================================================================
// Setup
// ============================================================================

fn setup_dashboard(
    mut commands: Commands,
    theme: Res<Theme>,
    mut chasing_materials: ResMut<Assets<ChasingBorderMaterial>>,
    content_area: Query<Entity, With<crate::ui::state::ContentArea>>,
) {
    // Get the ContentArea entity to parent the dashboard under
    let Ok(content_entity) = content_area.single() else {
        warn!("ContentArea not found - dashboard cannot be attached");
        return;
    };

    // Pre-create chasing border materials for each column
    // Sharp cyan border with rainbow cycling chase - parameters from theme
    let column_border_material = chasing_materials.add(
        ChasingBorderMaterial::from_theme(theme.accent, Color::WHITE)
            .with_thickness(1.0)
            .with_glow(theme.effect_chase_glow_radius, theme.effect_chase_glow_intensity)
            .with_chase_speed(theme.effect_chase_speed)
            .with_chase_width(theme.effect_chase_width)
            .with_color_cycle(theme.effect_chase_color_cycle),
    );

    // Root container - flex child in ContentArea
    // Visibility controlled by AppScreen state transitions (Display::Flex by default)
    let dashboard = commands
        .spawn((
            DashboardRoot,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                display: Display::Flex, // Visible by default (Dashboard is initial state)
                ..default()
            },
            BackgroundColor(theme.bg),
            Visibility::Inherited, // Visible by default, state transitions toggle this
        ))
        .with_children(|root| {
            // Main content - 3 columns
            root.spawn((Node {
                width: Val::Percent(100.0),
                flex_grow: 1.0,
                flex_direction: FlexDirection::Row,
                padding: UiRect::all(Val::Px(20.0)),
                column_gap: Val::Px(20.0),
                ..default()
            },))
                .with_children(|content| {
                    // Column 1: Kernels
                    spawn_dashboard_column_with_border(content, &theme, "KERNELS", KernelList, column_border_material.clone());

                    // Column 2: Contexts
                    spawn_dashboard_column_with_border(content, &theme, "CONTEXTS", ContextList, column_border_material.clone());

                    // Column 3: Your Seats
                    spawn_dashboard_column_with_border(content, &theme, "YOUR SEATS", SeatsList, column_border_material.clone());
                });

            // Footer with "Take Seat" input
            root.spawn((
                Node {
                    width: Val::Percent(100.0),
                    padding: UiRect::all(Val::Px(20.0)),
                    border: UiRect::top(Val::Px(1.0)),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(12.0),
                    ..default()
                },
                BorderColor::all(theme.border),
            ))
            .with_children(|footer| {
                footer.spawn((
                    GlyphonUiText::new("Take a seat:")
                        .with_font_size(14.0)
                        .with_color(theme.fg_dim),
                    UiTextPositionCache::default(),
                    Node {
                        min_width: Val::Px(100.0),
                        min_height: Val::Px(20.0),
                        ..default()
                    },
                ));

                // Take Seat button
                footer
                    .spawn((
                        TakeSeatButton,
                        Button,
                        Node {
                            padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                            ..default()
                        },
                        BackgroundColor(theme.accent),
                    ))
                    .with_children(|btn| {
                        btn.spawn((
                            GlyphonUiText::new("Take Seat 席")
                                .with_font_size(14.0)
                                .with_color(theme.bg),
                            UiTextPositionCache::default(),
                            Node {
                                min_width: Val::Px(100.0),
                                min_height: Val::Px(20.0),
                                ..default()
                            },
                        ));
                    });
            });
        })
        .id();

    // Attach dashboard to ContentArea
    commands.entity(content_entity).add_child(dashboard);
}

/// Spawn a dashboard column with chasing neon border effect
fn spawn_dashboard_column_with_border<M: Component>(
    parent: &mut ChildSpawnerCommands,
    theme: &Theme,
    title: &str,
    marker: M,
    border_material: Handle<ChasingBorderMaterial>,
) {
    // Outer container with chasing border material
    parent
        .spawn((
            ChasingBorder,
            Node {
                flex_grow: 1.0,
                flex_direction: FlexDirection::Column,
                // Small padding to create space between border and content
                padding: UiRect::all(Val::Px(4.0)),
                ..default()
            },
            // The material renders the animated border
            MaterialNode(border_material),
        ))
        .with_children(|outer| {
            // Inner content area with background
            outer
                .spawn((
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        flex_direction: FlexDirection::Column,
                        padding: UiRect::all(Val::Px(12.0)),
                        ..default()
                    },
                    BackgroundColor(theme.panel_bg),
                ))
                .with_children(|col| {
                    // Column header
                    col.spawn((
                        GlyphonUiText::new(title)
                            .with_font_size(12.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(100.0),
                            min_height: Val::Px(16.0),
                            margin: UiRect::bottom(Val::Px(12.0)),
                            ..default()
                        },
                    ));

                    // Content area (scrollable)
                    col.spawn((
                        marker,
                        Node {
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            overflow: Overflow::scroll_y(),
                            row_gap: Val::Px(4.0),
                            ..default()
                        },
                    ));
                });
        });
}

// ============================================================================
// Systems
// ============================================================================

/// Handle connection events that affect the dashboard
fn handle_dashboard_events(
    mut events: MessageReader<ConnectionEvent>,
    mut state: ResMut<DashboardState>,
    mut next_screen: ResMut<NextState<AppScreen>>,
    conn: Res<ConnectionCommands>,
    mut registry: ResMut<crate::conversation::ConversationRegistry>,
    mut current_conv: ResMut<crate::conversation::CurrentConversation>,
) {
    for event in events.read() {
        match event {
            ConnectionEvent::KernelList(kernels) => {
                state.kernels = kernels.clone();
                // Select first kernel if none selected
                if state.selected_kernel.is_none() && !state.kernels.is_empty() {
                    state.selected_kernel = Some(0);
                    // Request contexts for the selected kernel
                    conn.send(ConnectionCommand::ListContexts);
                }
            }
            ConnectionEvent::ContextsList(contexts) => {
                state.contexts = contexts.clone();
                // Auto-select first context if available
                if !state.contexts.is_empty() {
                    state.selected_context = Some(0);
                } else {
                    state.selected_context = None;
                }
            }
            ConnectionEvent::MySeatsList(seats) => {
                state.my_seats = seats.clone();
            }
            ConnectionEvent::SeatTaken { seat } => {
                state.current_seat = Some(seat.clone());

                // NOTE: We no longer reset sync_state here.
                // BlockCellInitialState event (sent after SeatTaken) sets up the document
                // from full oplog and establishes the frontier for incremental sync.

                // Use the cell_id from the seat (server provides the main document ID)
                // This ensures the client uses the same cell_id the server uses for BlockInserted events
                let cell_id = seat.cell_id.clone();
                let agent_id = format!("user:{}", whoami::username());

                // Create conversation using the server-provided cell_id
                // (with_id uses the ID for BlockDocument::new which sets cell_id)
                let conv = kaijutsu_kernel::Conversation::with_id(
                    &cell_id,  // becomes both conversation ID and doc cell_id
                    &cell_id,  // name (display)
                    &agent_id,
                );
                registry.add(conv);
                current_conv.0 = Some(cell_id.clone());

                info!("Created conversation for cell {}", cell_id);

                // Transition to Conversation screen
                next_screen.set(AppScreen::Conversation);
            }
            ConnectionEvent::SeatLeft => {
                state.current_seat = None;
                // Transition to Dashboard screen
                next_screen.set(AppScreen::Dashboard);
            }
            ConnectionEvent::Connected => {
                // Stay on Dashboard - kernel list will be requested after attach
            }
            ConnectionEvent::AttachedKernel(_) => {
                // Now that we're attached, request kernel list and contexts
                conn.send(ConnectionCommand::ListKernels);
                conn.send(ConnectionCommand::ListMySeats);
                conn.send(ConnectionCommand::ListContexts);
            }
            ConnectionEvent::Disconnected => {
                state.kernels.clear();
                state.contexts.clear();
                state.my_seats.clear();
                state.current_seat = None;
                // Return to Dashboard on disconnect
                next_screen.set(AppScreen::Dashboard);
            }
            _ => {}
        }
    }
}

/// Handle kernel selection (clicking on kernel list item)
fn handle_kernel_selection(
    interaction: Query<(&Interaction, &KernelListItem), Changed<Interaction>>,
    mut state: ResMut<DashboardState>,
    conn: Res<ConnectionCommands>,
) {
    for (interaction, item) in interaction.iter() {
        if *interaction == Interaction::Pressed {
            state.selected_kernel = Some(item.index);
            state.contexts.clear();
            state.selected_context = None;

            // Attach to kernel and request contexts
            if let Some(kernel) = state.kernels.get(item.index) {
                conn.send(ConnectionCommand::AttachKernel {
                    id: kernel.id.clone(),
                });
                conn.send(ConnectionCommand::ListContexts);
            }
        }
    }
}

/// Handle context selection
fn handle_context_selection(
    interaction: Query<(&Interaction, &ContextListItem), Changed<Interaction>>,
    mut state: ResMut<DashboardState>,
) {
    for (interaction, item) in interaction.iter() {
        if *interaction == Interaction::Pressed {
            state.selected_context = Some(item.index);
        }
    }
}

/// Handle "Take Seat" button click
fn handle_take_seat(
    interaction: Query<&Interaction, (Changed<Interaction>, With<TakeSeatButton>)>,
    state: Res<DashboardState>,
    conn: Res<ConnectionCommands>,
) {
    for interaction in interaction.iter() {
        if *interaction == Interaction::Pressed {
            // Get selected kernel and context
            if let (Some(_kernel), Some(context)) =
                (state.selected_kernel(), state.selected_context())
            {
                // Default instance name based on "default" for now
                // (hostname crate would require an additional dependency)
                let instance = std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "default".to_string());

                conn.send(ConnectionCommand::JoinContext {
                    context: context.name.clone(),
                    instance,
                });
            }
        }
    }
}

/// Handle keyboard input on the Dashboard.
/// Enter takes you into the selected context (or default if nothing selected).
fn handle_dashboard_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    screen: Res<State<AppScreen>>,
    mut state: ResMut<DashboardState>,
    conn: Res<ConnectionCommands>,
) {
    // Only handle keys when on Dashboard
    if *screen.get() != AppScreen::Dashboard {
        return;
    }

    if keys.just_pressed(KeyCode::Enter) {
        // Auto-select defaults if nothing selected
        if state.selected_kernel.is_none() && !state.kernels.is_empty() {
            state.selected_kernel = Some(0);
        }
        if state.selected_context.is_none() && !state.contexts.is_empty() {
            state.selected_context = Some(0);
        }

        // Take seat with selected context
        if let (Some(_kernel), Some(context)) =
            (state.selected_kernel(), state.selected_context())
        {
            let instance = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "default".to_string());

            info!("Enter pressed - taking seat in context: {}", context.name);
            conn.send(ConnectionCommand::JoinContext {
                context: context.name.clone(),
                instance,
            });
        }
    }
}

/// Setup the seat selector UI components (runs after main UI setup)
fn setup_seat_selector_ui(
    mut commands: Commands,
    theme: Res<Theme>,
    header_query: Query<Entity, With<HeaderContainer>>,
) {
    // Spawn the seat selector as a child of the header
    if let Some(header_entity) = header_query.iter().next() {
        let seat_selector = spawn_seat_selector(&mut commands, &theme);
        commands.entity(header_entity).add_child(seat_selector);
    }

    // Spawn the dropdown at root level (absolute positioned)
    spawn_seat_dropdown(&mut commands, &theme);
}

// ============================================================================
// List Rebuild Systems
// ============================================================================

/// Rebuild kernel list when state changes
fn rebuild_kernel_list(
    mut commands: Commands,
    state: Res<DashboardState>,
    theme: Res<Theme>,
    list_query: Query<Entity, With<KernelList>>,
    item_query: Query<Entity, With<KernelListItem>>,
) {
    if !state.is_changed() {
        return;
    }

    // Despawn existing items
    for entity in item_query.iter() {
        commands.entity(entity).despawn();
    }

    // Spawn new items
    let Ok(list_entity) = list_query.single() else {
        return;
    };

    commands.entity(list_entity).with_children(|parent| {
        for (index, kernel) in state.kernels.iter().enumerate() {
            let is_selected = state.selected_kernel == Some(index);
            let bg_color = if is_selected {
                theme.selection_bg
            } else {
                Color::NONE
            };

            parent
                .spawn((
                    KernelListItem { index },
                    Button,
                    Node {
                        width: Val::Percent(100.0),
                        padding: UiRect::all(Val::Px(8.0)),
                        ..default()
                    },
                    BackgroundColor(bg_color),
                ))
                .with_children(|item| {
                    let display = format!(
                        "{} ({} users)",
                        kernel.name,
                        kernel.user_count
                    );
                    item.spawn((
                        GlyphonUiText::new(&display)
                            .with_font_size(14.0)
                            .with_color(theme.fg),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(150.0),
                            min_height: Val::Px(20.0),
                            ..default()
                        },
                    ));
                });
        }
    });
}

/// Rebuild context list when state changes
fn rebuild_context_list(
    mut commands: Commands,
    state: Res<DashboardState>,
    theme: Res<Theme>,
    list_query: Query<Entity, With<ContextList>>,
    item_query: Query<Entity, With<ContextListItem>>,
) {
    if !state.is_changed() {
        return;
    }

    // Despawn existing items
    for entity in item_query.iter() {
        commands.entity(entity).despawn();
    }

    // Spawn new items
    let Ok(list_entity) = list_query.single() else {
        return;
    };

    commands.entity(list_entity).with_children(|parent| {
        for (index, context) in state.contexts.iter().enumerate() {
            let is_selected = state.selected_context == Some(index);
            let bg_color = if is_selected {
                theme.selection_bg
            } else {
                Color::NONE
            };

            parent
                .spawn((
                    ContextListItem { index },
                    Button,
                    Node {
                        width: Val::Percent(100.0),
                        padding: UiRect::all(Val::Px(8.0)),
                        ..default()
                    },
                    BackgroundColor(bg_color),
                ))
                .with_children(|item| {
                    item.spawn((
                        GlyphonUiText::new(&context.name)
                            .with_font_size(14.0)
                            .with_color(theme.fg),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(150.0),
                            min_height: Val::Px(20.0),
                            ..default()
                        },
                    ));
                });
        }
    });
}

/// Rebuild seats list when state changes
fn rebuild_seats_list(
    mut commands: Commands,
    state: Res<DashboardState>,
    theme: Res<Theme>,
    list_query: Query<Entity, With<SeatsList>>,
    item_query: Query<Entity, With<SeatListItem>>,
) {
    if !state.is_changed() {
        return;
    }

    // Despawn existing items
    for entity in item_query.iter() {
        commands.entity(entity).despawn();
    }

    // Spawn new items
    let Ok(list_entity) = list_query.single() else {
        return;
    };

    commands.entity(list_entity).with_children(|parent| {
        for seat in state.my_seats.iter() {
            parent
                .spawn((
                    SeatListItem,
                    Button,
                    Node {
                        width: Val::Percent(100.0),
                        padding: UiRect::all(Val::Px(8.0)),
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(2.0),
                        ..default()
                    },
                    BackgroundColor(Color::NONE),
                ))
                .with_children(|item| {
                    // Nick and instance
                    let nick_text = format!("@{}:{}", seat.id.nick, seat.id.instance);
                    item.spawn((
                        GlyphonUiText::new(&nick_text)
                            .with_font_size(14.0)
                            .with_color(theme.fg),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(150.0),
                            min_height: Val::Px(18.0),
                            ..default()
                        },
                    ));

                    // Context and kernel
                    let context_text = format!("  :{}@{}", seat.id.context, seat.id.kernel);
                    item.spawn((
                        GlyphonUiText::new(&context_text)
                            .with_font_size(12.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node {
                            min_width: Val::Px(150.0),
                            min_height: Val::Px(16.0),
                            ..default()
                        },
                    ));
                });
        }
    });
}
