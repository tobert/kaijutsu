//! Widget update systems for the tiling WM.
//!
//! These systems reactively update widget pane text content based on
//! application state changes (mode, connection, drift contexts).
//! Uses the tiling reconciler's `WidgetPaneText` marker to find and
//! update MSDF text content in dock widget panes.

use bevy::prelude::*;

use super::tiling::PaneContent;
use super::tiling_reconciler::WidgetPaneText;
use crate::cell::ContextSwitchRequested;
use crate::input::FocusArea;
use crate::connection::RpcConnectionState;
use crate::text::{bevy_to_rgba8, MsdfUiText, UiTextPositionCache};
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
        FocusArea::EditingBlock { .. } => theme.mode_chat,
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
                parts.push(format!("@{}", ctx.short_id));
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
        FocusArea::EditingBlock { .. } => "Enter: newline │ Esc: stop editing │ ←/→: cursor │ Home/End: line",
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
        app.add_systems(
            Update,
            (
                update_mode_widget,
                update_connection_widget,
                update_contexts_widget,
                update_hints_widget,
                handle_context_badge_click,
            )
                .chain(),
        );
    }
}
