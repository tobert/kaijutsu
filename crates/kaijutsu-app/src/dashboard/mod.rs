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

use crate::connection::{
    BootstrapChannel, BootstrapCommand, ConnectionStatusMessage, RpcActor, RpcResultChannel,
    RpcResultMessage,
};
use crate::shaders::nine_slice::ChasingBorder;
use crate::text::{MsdfUiText, UiTextPositionCache};
use crate::ui::state::AppScreen;
use crate::ui::theme::Theme;
use crate::HeaderContainer;

pub use seat_selector::{spawn_seat_dropdown, spawn_seat_selector};

/// System set for dashboard event handling.
/// Other plugins can schedule systems `.after(DashboardEventHandling)` to ensure
/// they run after SeatTaken creates the conversation in the registry.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct DashboardEventHandling;

/// Plugin for the dashboard/lobby experience
pub struct DashboardPlugin;

impl Plugin for DashboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DashboardState>()
            // PostStartup ensures header exists before we add seat selector to it
            .add_systems(PostStartup, setup_seat_selector_ui)
            .add_systems(
                Update,
                (
                    // Dashboard event handling - creates conversation on SeatTaken
                    // Other systems (like handle_block_events) must run after this
                    handle_dashboard_events.in_set(DashboardEventHandling),
                    handle_kernel_selection,
                    handle_context_selection,
                    handle_take_seat,
                    handle_dashboard_keyboard,
                    // Filler systems - populate layout-spawned column markers
                    fill_kernel_list_column,
                    fill_context_list_column,
                    fill_seats_list_column,
                    fill_dashboard_footer,
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
// Layout Column Markers (spawned by layout builders, filled by filler systems)
// ============================================================================

/// Marker for KernelList column container (spawned by layout, filled by filler)
#[derive(Component)]
pub struct KernelListColumn;

/// Marker for ContextList column container (spawned by layout, filled by filler)
#[derive(Component)]
pub struct ContextListColumn;

/// Marker for SeatsList column container (spawned by layout, filled by filler)
#[derive(Component)]
pub struct SeatsListColumn;

/// Marker for dashboard footer container (spawned by layout, filled by filler)
#[derive(Component)]
pub struct DashboardFooter;


// ============================================================================
// Systems
// ============================================================================

/// Handle connection lifecycle and RPC result events that affect the dashboard.
///
/// Reads:
/// - `ConnectionStatusMessage` — connection lifecycle (Connected, Disconnected)
/// - `RpcResultMessage` — results from async RPC calls (kernel lists, seat taken, etc.)
///
/// On Connected, fires async list requests via ActorHandle + RpcResultChannel.
fn handle_dashboard_events(
    mut status_events: MessageReader<ConnectionStatusMessage>,
    mut result_events: MessageReader<RpcResultMessage>,
    mut state: ResMut<DashboardState>,
    mut next_screen: ResMut<NextState<AppScreen>>,
    actor: Option<Res<RpcActor>>,
    channel: Res<RpcResultChannel>,
    mut registry: ResMut<crate::conversation::ConversationRegistry>,
    mut current_conv: ResMut<crate::conversation::CurrentConversation>,
) {
    // Handle connection lifecycle
    for ConnectionStatusMessage(status) in status_events.read() {
        match status {
            kaijutsu_client::ConnectionStatus::Connected => {
                // Actor just (re)connected — fire list requests
                if let Some(ref actor) = actor {
                    fire_dashboard_list_requests(&actor.handle, actor.generation, &channel);
                }
            }
            kaijutsu_client::ConnectionStatus::Disconnected => {
                state.kernels.clear();
                state.contexts.clear();
                state.my_seats.clear();
                state.current_seat = None;
                next_screen.set(AppScreen::Dashboard);
            }
            _ => {}
        }
    }

    // Handle RPC results — discard stale list results from previous actors
    let current_gen = actor.as_ref().map(|a| a.generation).unwrap_or(0);
    for result in result_events.read() {
        match result {
            RpcResultMessage::KernelList { kernels, generation } if *generation == current_gen => {
                state.kernels = kernels.clone();
                if state.selected_kernel.is_none() && !state.kernels.is_empty() {
                    state.selected_kernel = Some(0);
                }
            }
            RpcResultMessage::ContextList { contexts, generation } if *generation == current_gen => {
                state.contexts = contexts.clone();
                if !state.contexts.is_empty() {
                    state.selected_context = Some(0);
                } else {
                    state.selected_context = None;
                }
            }
            RpcResultMessage::MySeatsList { seats, generation } if *generation == current_gen => {
                state.my_seats = seats.clone();
            }
            RpcResultMessage::ContextJoined { seat, document_id, .. } => {
                state.current_seat = Some(seat.clone());

                // Create conversation metadata if it doesn't exist (idempotent)
                if registry.get(document_id).is_none() {
                    let conv = kaijutsu_kernel::Conversation::with_id(document_id, document_id);
                    registry.add(conv);
                    info!("Created conversation metadata for {}", document_id);
                }

                current_conv.0 = Some(document_id.clone());
                next_screen.set(AppScreen::Conversation);
            }
            RpcResultMessage::ContextLeft => {
                state.current_seat = None;
                next_screen.set(AppScreen::Dashboard);
            }
            _ => {}
        }
    }
}

/// Fire async list/info requests on the actor and route results through RpcResultChannel.
///
/// Called when the actor reports Connected. Fetches lists for dashboard display,
/// kernel info for state tracking, and (if not lobby) document state for joining.
/// `generation` is stamped on results so the dashboard can discard stale responses.
fn fire_dashboard_list_requests(
    handle: &kaijutsu_client::ActorHandle,
    generation: u64,
    channel: &RpcResultChannel,
) {
    // List kernels
    let h = handle.clone();
    let tx = channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match h.list_kernels().await {
                Ok(kernels) => { let _ = tx.send(RpcResultMessage::KernelList { kernels, generation }); }
                Err(e) => log::warn!("list_kernels failed: {e}"),
            }
        })
        .detach();

    // List contexts
    let h = handle.clone();
    let tx = channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match h.list_contexts().await {
                Ok(contexts) => { let _ = tx.send(RpcResultMessage::ContextList { contexts, generation }); }
                Err(e) => log::warn!("list_contexts failed: {e}"),
            }
        })
        .detach();

    // Get kernel info + context ID to determine if we auto-joined a non-lobby context
    let h = handle.clone();
    let tx = channel.sender();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            // Get kernel info for state tracking
            match h.get_info().await {
                Ok(info) => { let _ = tx.send(RpcResultMessage::KernelAttached(Ok(info.clone()))); }
                Err(e) => log::warn!("get_info failed: {e}"),
            }

            // Get context ID — if not "lobby", fetch document state for the seat
            match h.get_context_id().await {
                Ok((kernel_id, context_name)) => {
                    if context_name != "lobby" {
                        let document_id = format!("{}@{}", kernel_id, context_name);
                        let initial_state = match h.get_document_state(&document_id).await {
                            Ok(state) => Some(state),
                            Err(e) => {
                                log::warn!("get_document_state failed: {e}");
                                None
                            }
                        };
                        let seat = kaijutsu_client::SeatInfo {
                            id: kaijutsu_client::SeatId {
                                nick: String::new(), // Will be filled by identity
                                instance: "bevy-client".into(),
                                kernel: kernel_id,
                                context: context_name,
                            },
                            owner: String::new(),
                            status: kaijutsu_client::SeatStatus::Active,
                            last_activity: 0,
                            cursor_block: None,
                        };
                        let _ = tx.send(RpcResultMessage::ContextJoined {
                            seat,
                            document_id,
                            initial_state,
                        });
                    }
                }
                Err(e) => log::warn!("get_context_id failed: {e}"),
            }
        })
        .detach();
}

/// Handle kernel selection (clicking on kernel list item).
///
/// Selecting a different kernel respawns the actor with the new kernel_id.
fn handle_kernel_selection(
    interaction: Query<(&Interaction, &KernelListItem), Changed<Interaction>>,
    mut state: ResMut<DashboardState>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
) {
    for (interaction, item) in interaction.iter() {
        if *interaction == Interaction::Pressed {
            state.selected_kernel = Some(item.index);
            state.contexts.clear();
            state.selected_context = None;

            // Respawn actor with selected kernel
            if let Some(kernel) = state.kernels.get(item.index) {
                let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
                    config: conn_state.ssh_config.clone(),
                    kernel_id: kernel.id.clone(),
                    context_name: "lobby".into(),
                    instance: "bevy-client".into(),
                });
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

/// Handle "Take Seat" button click.
///
/// Respawns the actor with the selected kernel + context so it auto-joins.
fn handle_take_seat(
    interaction: Query<&Interaction, (Changed<Interaction>, With<TakeSeatButton>)>,
    state: Res<DashboardState>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
) {
    for interaction in interaction.iter() {
        if *interaction == Interaction::Pressed
            && let (Some(kernel), Some(context)) =
                (state.selected_kernel(), state.selected_context())
        {
            let instance = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "default".to_string());

            let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
                config: conn_state.ssh_config.clone(),
                kernel_id: kernel.id.clone(),
                context_name: context.name.clone(),
                instance,
            });
        }
    }
}

/// Handle keyboard input on the Dashboard.
/// Enter takes you into the selected context (or default if nothing selected).
fn handle_dashboard_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    screen: Res<State<AppScreen>>,
    mut state: ResMut<DashboardState>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<crate::connection::RpcConnectionState>,
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

        // Respawn actor with selected kernel + context
        if let (Some(kernel), Some(context)) =
            (state.selected_kernel(), state.selected_context())
        {
            let instance = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "default".to_string());

            info!("Enter pressed - taking seat in context: {}", context.name);
            let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
                config: conn_state.ssh_config.clone(),
                kernel_id: kernel.id.clone(),
                context_name: context.name.clone(),
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
                        MsdfUiText::new(&display)
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
                        MsdfUiText::new(&context.name)
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
                        MsdfUiText::new(&nick_text)
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
                        MsdfUiText::new(&context_text)
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

// ============================================================================
// Layout Filler Systems
// These populate layout-spawned column markers with chasing borders and content
// ============================================================================

/// Fill newly spawned KernelListColumn with chasing border and inner container
fn fill_kernel_list_column(
    mut commands: Commands,
    theme: Res<Theme>,
    material_cache: Res<crate::ui::materials::MaterialCache>,
    new_columns: Query<Entity, Added<KernelListColumn>>,
) {
    for column_entity in new_columns.iter() {
        spawn_column_content(&mut commands, column_entity, &theme, &material_cache, "KERNELS", KernelList);
    }
}

/// Fill newly spawned ContextListColumn with chasing border and inner container
fn fill_context_list_column(
    mut commands: Commands,
    theme: Res<Theme>,
    material_cache: Res<crate::ui::materials::MaterialCache>,
    new_columns: Query<Entity, Added<ContextListColumn>>,
) {
    for column_entity in new_columns.iter() {
        spawn_column_content(&mut commands, column_entity, &theme, &material_cache, "CONTEXTS", ContextList);
    }
}

/// Fill newly spawned SeatsListColumn with chasing border and inner container
fn fill_seats_list_column(
    mut commands: Commands,
    theme: Res<Theme>,
    material_cache: Res<crate::ui::materials::MaterialCache>,
    new_columns: Query<Entity, Added<SeatsListColumn>>,
) {
    for column_entity in new_columns.iter() {
        spawn_column_content(&mut commands, column_entity, &theme, &material_cache, "YOUR SEATS", SeatsList);
    }
}

/// Fill newly spawned DashboardFooter with Take Seat button
fn fill_dashboard_footer(
    mut commands: Commands,
    theme: Res<Theme>,
    new_footers: Query<Entity, Added<DashboardFooter>>,
) {
    for footer_entity in new_footers.iter() {
        // Apply border color to footer (already has structure from layout builder)
        commands.entity(footer_entity).insert(BorderColor::all(theme.border));

        // Add footer content as children
        commands.entity(footer_entity).with_children(|footer| {
            footer.spawn((
                MsdfUiText::new("Take a seat:")
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
                        MsdfUiText::new("Take Seat 席")
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
    }
}

/// Helper: Spawn chasing border and inner content area for a dashboard column
fn spawn_column_content<M: Component>(
    commands: &mut Commands,
    column_entity: Entity,
    theme: &Theme,
    material_cache: &crate::ui::materials::MaterialCache,
    title: &str,
    marker: M,
) {
    // Add chasing border material and styling to the column
    commands.entity(column_entity).insert((
        ChasingBorder,
        MaterialNode(material_cache.chasing_border.clone()),
        Node {
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(Val::Px(4.0)),
            ..default()
        },
    ));

    // Spawn inner content as child
    commands.entity(column_entity).with_children(|outer| {
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
                    MsdfUiText::new(title)
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

                // Scrollable content area with the marker
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
