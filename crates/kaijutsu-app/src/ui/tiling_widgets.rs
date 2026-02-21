//! Widget update systems for the tiling WM.
//!
//! These systems reactively update widget pane text content based on
//! application state changes (mode, connection, drift contexts).
//! Uses the tiling reconciler's `WidgetPaneText` marker to find and
//! update MSDF text content in dock widget panes.

use std::collections::VecDeque;

use bevy::prelude::*;

use super::tiling::{DockWidget, PaneContent, TilingTree};
use super::tiling_reconciler::WidgetPaneText;
use crate::cell::ContextSwitchRequested;
use crate::connection::actor_plugin::ServerEventMessage;
use crate::input::FocusArea;
use crate::connection::RpcConnectionState;
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
use crate::ui::constellation::{ActivityState, Constellation};
use crate::ui::drift::DriftState;
use crate::ui::theme::Theme;

// ============================================================================
// CONTEXT BADGE (clickable context switcher in South dock)
// ============================================================================

/// Clickable badge for a context in the context strip widget.
#[derive(Component, Debug, Clone)]
pub struct ContextBadge {
    pub context_name: String,
}

// ============================================================================
// DOCK LAYOUT — BRP-mutable dock configuration
// ============================================================================

/// Declarative dock layout resource.
///
/// Source of truth for what widgets appear in the North and South docks.
/// Mutating this via BRP triggers `sync_dock_layout_to_tiling_tree` which
/// rebuilds the dock TileNode children and triggers the reconciler.
#[derive(Resource, Clone, Reflect)]
#[reflect(Resource)]
pub struct DockLayout {
    pub north: Vec<DockWidget>,
    pub south: Vec<DockWidget>,
}

impl Default for DockLayout {
    fn default() -> Self {
        Self {
            north: vec![
                DockWidget::Title,
                DockWidget::Spacer,
                DockWidget::EventPulse,
                DockWidget::Connection,
            ],
            south: vec![
                DockWidget::Mode,
                DockWidget::ModelBadge,
                DockWidget::Spacer,
                DockWidget::AgentActivity,
                DockWidget::BlockActivity,
                DockWidget::Spacer,
                DockWidget::Contexts,
                DockWidget::Spacer,
                DockWidget::Hints,
            ],
        }
    }
}

/// Sync DockLayout changes to the TilingTree.
///
/// When DockLayout is mutated (via BRP or internally), rebuilds dock children
/// in the tiling tree, which triggers the reconciler to respawn dock entities.
pub fn sync_dock_layout_to_tiling_tree(
    dock_layout: Res<DockLayout>,
    mut tree: ResMut<TilingTree>,
) {
    if !dock_layout.is_changed() {
        return;
    }
    tree.rebuild_docks(&dock_layout.north, &dock_layout.south);
}

// ============================================================================
// WIDGET UPDATE SYSTEMS
// ============================================================================

/// Update mode widget text when FocusArea changes.
///
/// Displays the current focus area name (COMPOSE, NAVIGATE, EDITING, etc.)
/// with appropriate color from the theme.
pub fn update_mode_widget(
    focus_area: Res<FocusArea>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !focus_area.is_changed() {
        return;
    }

    let color = match focus_area.as_ref() {
        FocusArea::Compose => theme.mode_chat,
        FocusArea::Conversation => theme.mode_normal,
        FocusArea::EditingBlock => theme.mode_chat,
        FocusArea::Constellation => theme.mode_visual,
        FocusArea::Dialog => theme.mode_shell,
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::Mode) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                msdf_text.text = focus_area.name().to_string();
                msdf_text.color = bevy_to_rgba8(color);
            }
        }
    }
}

/// Update connection widget when RpcConnectionState changes.
pub fn update_connection_widget(
    conn_state: Res<RpcConnectionState>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !conn_state.is_changed() {
        return;
    }

    let (text, color) = if conn_state.connected {
        let status = conn_state
            .identity
            .as_ref()
            .map(|i| format!("✓ @{}", i.username))
            .unwrap_or_else(|| "✓ Connected".to_string());
        (status, theme.success)
    } else if conn_state.reconnect_attempt > 0 {
        (
            format!("⟳ Reconnecting ({})...", conn_state.reconnect_attempt),
            theme.warning,
        )
    } else {
        ("⚡ Disconnected".to_string(), theme.error)
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::Connection) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                msdf_text.text = text.clone();
                msdf_text.color = bevy_to_rgba8(color);
            }
        }
    }
}

/// Marker for auxiliary badge strip entities (overflow "+N", staged count)
/// that should be despawned on structural badge changes but aren't ContextBadges.
#[derive(Component)]
pub struct BadgeAuxiliary;

/// Update contexts widget when DriftState or DocumentCache changes.
///
/// Shows MRU context badges from DocumentCache as clickable children, with
/// active context highlighted. Falls back to drift state for single-text.
///
/// Uses diff-based updates: when only the active highlight changes (same set
/// of context names), updates text/color in-place to preserve Interaction state.
/// Full respawn only happens when the badge set membership changes.
pub fn update_contexts_widget(
    mut commands: Commands,
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    widget_panes: Query<(Entity, &WidgetPaneText, &Children)>,
    existing_badges: Query<(Entity, &ContextBadge, Option<&Children>)>,
    aux_entities: Query<Entity, With<BadgeAuxiliary>>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    // Find the contexts widget entity
    let Some((widget_entity, _, widget_children)) = widget_panes
        .iter()
        .find(|(_, w, _)| matches!(w.widget_type, PaneContent::Contexts))
    else {
        return;
    };

    // If there's an active notification, show as single text (despawn badges)
    if let Some(ref notif) = drift_state.notification {
        for (entity, _, _) in existing_badges.iter() {
            if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
        }
        for entity in aux_entities.iter() {
            if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
        }
        for child in widget_children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                msdf_text.text = format!("← @{}: \"{}\"", notif.source_ctx, notif.preview);
                msdf_text.color = bevy_to_rgba8(theme.accent);
            }
        }
        return;
    }

    let mru_ids = doc_cache.mru_ids();
    let active_doc_id = doc_cache.active_id();

    if !mru_ids.is_empty() {
        // Clear the original text child
        for child in widget_children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                msdf_text.text = String::new();
            }
        }

        let max_display = 5;

        // Build desired badge list: (context_name, short_label, is_active)
        let desired: Vec<(String, String, bool)> = mru_ids
            .iter()
            .take(max_display)
            .map(|doc_id| {
                let ctx_name = doc_cache
                    .get(doc_id)
                    .map(|c| c.context_name.clone())
                    .unwrap_or_else(|| "?".to_string());
                let is_active = active_doc_id == Some(doc_id.as_str());
                let short = if ctx_name.len() > 12 {
                    ctx_name[..12].to_string()
                } else {
                    ctx_name.clone()
                };
                (ctx_name, short, is_active)
            })
            .collect();

        // Full rebuild: despawn existing badges and recreate in MRU order.
        // Badge count is <=5; always rebuilding ensures correct visual ordering.
        for (entity, _, _) in existing_badges.iter() {
            if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
        }
        for entity in aux_entities.iter() {
            if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
        }

        for (ctx_name, short, is_active) in &desired {
            let label = if *is_active {
                format!("[{}]", short)
            } else {
                short.clone()
            };
            let color = if *is_active { theme.accent } else { theme.fg_dim };

            let badge = commands
                .spawn((
                    ContextBadge {
                        context_name: ctx_name.clone(),
                    },
                    Node {
                        padding: UiRect::axes(Val::Px(6.0), Val::Px(2.0)),
                        ..default()
                    },
                    Interaction::None,
                ))
                .with_children(|parent| {
                    parent.spawn((
                        MsdfUiText::new(&label)
                            .with_font_size(11.0)
                            .with_color(color),
                        UiTextPositionCache::default(),
                        Node::default(),
                    ));
                })
                .id();
            if let Ok(mut ec) = commands.get_entity(widget_entity) { ec.add_child(badge); }
        }

        // Overflow indicator
        if mru_ids.len() > max_display {
            let remaining = mru_ids.len() - max_display;
            let overflow = commands
                .spawn((
                    BadgeAuxiliary,
                    Node {
                        padding: UiRect::axes(Val::Px(4.0), Val::Px(2.0)),
                        ..default()
                    },
                ))
                .with_children(|parent| {
                    parent.spawn((
                        MsdfUiText::new(&format!("+{}", remaining))
                            .with_font_size(11.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node::default(),
                    ));
                })
                .id();
            if let Ok(mut ec) = commands.get_entity(widget_entity) { ec.add_child(overflow); }
        }

        // Staged count
        let staged = drift_state.staged_count();
        if staged > 0 {
            let staged_text = commands
                .spawn((
                    BadgeAuxiliary,
                    Node {
                        padding: UiRect::left(Val::Px(8.0)),
                        ..default()
                    },
                ))
                .with_children(|parent| {
                    parent.spawn((
                        MsdfUiText::new(&format!("·{} staged", staged))
                            .with_font_size(11.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node::default(),
                    ));
                })
                .id();
            if let Ok(mut ec) = commands.get_entity(widget_entity) { ec.add_child(staged_text); }
        }
        return;
    }

    // No cached docs — despawn badges and fall back to single text
    for (entity, _, _) in existing_badges.iter() {
        if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
    }
    for entity in aux_entities.iter() {
        if let Ok(mut ec) = commands.get_entity(entity) { ec.despawn(); }
    }

    // Fall back to drift state contexts as single text
    for child in widget_children.iter() {
        if let Ok(mut msdf_text) = texts.get_mut(child) {
            if drift_state.contexts.is_empty() {
                msdf_text.text = String::new();
                continue;
            }

            let mut parts: Vec<String> = Vec::new();
            let max_display = 5;

            for (i, ctx) in drift_state.contexts.iter().enumerate() {
                if i >= max_display {
                    let remaining = drift_state.contexts.len() - max_display;
                    parts.push(format!("+{} more", remaining));
                    break;
                }
                parts.push(format!("@{}", ctx.id.short()));
            }

            let mut text = parts.join(" ");
            let staged = drift_state.staged_count();
            if staged > 0 {
                text.push_str(&format!("  ·{} staged", staged));
            }

            let color = if drift_state.local_context_id.is_some() {
                theme.accent
            } else {
                theme.fg_dim
            };

            msdf_text.text = text;
            msdf_text.color = bevy_to_rgba8(color);
        }
    }
}

/// Update hints widget to show context-sensitive key hints based on FocusArea.
///
/// Hints change based on what has focus — compose, navigation, constellation, etc.
pub fn update_hints_widget(
    focus_area: Res<FocusArea>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !focus_area.is_changed() {
        return;
    }

    let hints = match focus_area.as_ref() {
        FocusArea::Compose => "Enter: submit │ Shift+Enter: newline │ Tab: navigate │ Esc: back │ :/`: shell prefix",
        FocusArea::Conversation => "j/k: navigate │ Tab: compose │ f: expand │ i: compose │ `: constellation │ Alt+hjkl: pane",
        FocusArea::EditingBlock => "Enter: newline │ Esc: stop editing │ ←/→: cursor │ Home/End: line",
        FocusArea::Constellation => "h/j/k/l: navigate │ Enter: switch │ f: fork │ m: model │ Tab: compose │ +/-: zoom │ 0: reset",
        FocusArea::Dialog => "Enter: confirm │ Esc: cancel │ j/k: navigate",
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::Hints) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                if msdf_text.text != hints {
                    msdf_text.text = hints.to_string();
                }
            }
        }
    }
}

// ============================================================================
// LIVE DATA WIDGET SYSTEMS
// ============================================================================

/// Rolling event counter state for the EventPulse widget.
#[derive(Default)]
pub(crate) struct EventPulseState {
    /// Timestamps of events within the rolling window.
    timestamps: VecDeque<f64>,
}

/// Update EventPulse widget — shows server event rate in a rolling 5s window.
///
/// Counts all incoming `ServerEventMessage` and displays `~N ops` when active
/// or `quiet` when idle.
pub fn update_event_pulse_widget(
    mut state: Local<EventPulseState>,
    time: Res<Time>,
    mut events: MessageReader<ServerEventMessage>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    let now = time.elapsed_secs_f64();
    let window = 5.0;

    // Record new events
    let count = events.read().count();
    for _ in 0..count {
        state.timestamps.push_back(now);
    }

    // Expire old events
    while let Some(&front) = state.timestamps.front() {
        if now - front > window {
            state.timestamps.pop_front();
        } else {
            break;
        }
    }

    let total = state.timestamps.len();
    let (text, color) = if total > 0 {
        (format!("~{} ops", total), theme.accent)
    } else {
        ("quiet".to_string(), theme.fg_dim)
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::EventPulse) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                if msdf_text.text != text {
                    msdf_text.text = text.clone();
                    msdf_text.color = bevy_to_rgba8(color);
                }
            }
        }
    }
}

/// Update ModelBadge widget — shows active context's model name.
///
/// Reads DriftState to find the active context and extracts a shortened
/// model name (e.g. "opus-4.6" from "claude-opus-4-6").
pub fn update_model_badge_widget(
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !drift_state.is_changed() && !doc_cache.is_changed() {
        return;
    }

    // Find the active context's model from drift state
    let model_text = if let Some(active_id) = doc_cache.active_id() {
        // Look up context info from drift_state by matching document_id
        drift_state
            .contexts
            .iter()
            .find(|ctx| ctx.document_id == active_id)
            .map(|ctx| {
                if ctx.model.is_empty() {
                    "—".to_string()
                } else {
                    shorten_model_name(&ctx.model)
                }
            })
            .unwrap_or_else(|| "—".to_string())
    } else {
        "—".to_string()
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::ModelBadge) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                if msdf_text.text != model_text {
                    msdf_text.text = model_text.clone();
                    msdf_text.color = bevy_to_rgba8(theme.fg_dim);
                }
            }
        }
    }
}

/// Shorten a model name for display (e.g. "claude-opus-4-6" → "opus-4.6").
fn shorten_model_name(model: &str) -> String {
    let m = model
        .strip_prefix("claude-")
        .unwrap_or(model);
    // Replace version separator: "opus-4-6" → "opus-4.6"
    // Find the pattern "X-N-N" at the end and replace last dash with dot
    if let Some(pos) = m.rfind('-') {
        if pos > 0 && m[pos + 1..].chars().all(|c| c.is_ascii_digit()) {
            return format!("{}.{}", &m[..pos], &m[pos + 1..]);
        }
    }
    m.to_string()
}

/// Update AgentActivity widget — summarizes non-idle activity across constellation nodes.
///
/// Reads the Constellation resource and counts nodes with non-idle activity states.
/// Shows "streaming" for 1 streaming node, "2 active" for multiple, nothing when idle.
pub fn update_agent_activity_widget(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !constellation.is_changed() {
        return;
    }

    let mut streaming = 0u32;
    let mut waiting = 0u32;
    let mut active = 0u32;

    for node in &constellation.nodes {
        match node.activity {
            ActivityState::Streaming => streaming += 1,
            ActivityState::Waiting => waiting += 1,
            ActivityState::Active => active += 1,
            _ => {}
        }
    }

    let total_active = streaming + waiting + active;
    let (text, color) = if streaming == 1 && total_active == 1 {
        ("streaming".to_string(), theme.accent)
    } else if total_active > 0 {
        (format!("{} active", total_active), theme.accent)
    } else {
        (String::new(), theme.fg_dim)
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::AgentActivity) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                if msdf_text.text != text {
                    msdf_text.text = text.clone();
                    msdf_text.color = bevy_to_rgba8(color);
                }
            }
        }
    }
}

/// Tracks running block counts for the BlockActivity widget.
#[derive(Default)]
pub(crate) struct BlockActivityCounts {
    /// Running block count for the currently active document.
    pub(crate) running: u32,
    /// Last active document ID we were tracking.
    pub(crate) last_active_doc: Option<String>,
}

/// Update BlockActivity widget — shows running block count for active document.
///
/// Reads `ServerEventMessage::BlockStatusChanged` events and tracks Running vs Done
/// transitions. Shows "N running" when blocks are executing, empty when all done.
pub fn update_block_activity_widget(
    mut state: Local<BlockActivityCounts>,
    mut events: MessageReader<ServerEventMessage>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    let active_doc = doc_cache.active_id().map(|s| s.to_string());

    // Reset counts on context switch
    if active_doc != state.last_active_doc {
        state.running = 0;
        state.last_active_doc = active_doc.clone();
    }

    // Process status change events for the active document
    for event in events.read() {
        if let kaijutsu_client::ServerEvent::BlockStatusChanged {
            document_id,
            status,
            ..
        } = &event.0
        {
            if active_doc.as_deref() == Some(document_id.as_str()) {
                match status {
                    kaijutsu_crdt::Status::Running => {
                        state.running = state.running.saturating_add(1);
                    }
                    kaijutsu_crdt::Status::Done | kaijutsu_crdt::Status::Error => {
                        state.running = state.running.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    }

    let text = if state.running > 0 {
        format!("{} running", state.running)
    } else {
        String::new()
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::BlockActivity) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                if msdf_text.text != text {
                    msdf_text.text = text.clone();
                    msdf_text.color = bevy_to_rgba8(theme.accent);
                }
            }
        }
    }
}

/// Handle clicks on context badges in the South dock strip.
pub fn handle_context_badge_click(
    badges: Query<(&Interaction, &ContextBadge), Changed<Interaction>>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
) {
    for (interaction, badge) in badges.iter() {
        if *interaction == Interaction::Pressed {
            info!("Context badge clicked: {}", badge.context_name);
            switch_writer.write(ContextSwitchRequested {
                context_name: badge.context_name.clone(),
            });
        }
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for tiling widget update systems.
pub struct TilingWidgetsPlugin;

impl Plugin for TilingWidgetsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DockLayout>()
            .register_type::<DockLayout>()
            .add_systems(
                Update,
                (
                    sync_dock_layout_to_tiling_tree,
                    update_mode_widget,
                    update_connection_widget,
                    update_contexts_widget,
                    update_hints_widget,
                    update_event_pulse_widget,
                    update_model_badge_widget,
                    update_agent_activity_widget,
                    update_block_activity_widget,
                    handle_context_badge_click,
                ),
            );
    }
}
