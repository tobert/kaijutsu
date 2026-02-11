//! Widget update systems for the tiling WM.
//!
//! These systems reactively update widget pane text content based on
//! application state changes (mode, connection, drift contexts).
//! Uses the tiling reconciler's `WidgetPaneText` marker to find and
//! update MSDF text content in dock widget panes.

use bevy::prelude::*;

use super::tiling::PaneContent;
use super::tiling_reconciler::WidgetPaneText;
use crate::cell::{CurrentMode, EditorMode, InputKind, ContextSwitchRequested};
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

/// Update mode widget text when CurrentMode changes.
pub fn update_mode_widget(
    mode: Res<CurrentMode>,
    theme: Res<Theme>,
    widget_panes: Query<(&WidgetPaneText, &Children)>,
    mut texts: Query<&mut MsdfUiText>,
) {
    if !mode.is_changed() {
        return;
    }

    let color = match mode.0 {
        EditorMode::Normal => theme.mode_normal,
        EditorMode::Input(InputKind::Chat) => theme.mode_chat,
        EditorMode::Input(InputKind::Shell) => theme.mode_shell,
        EditorMode::Visual => theme.mode_visual,
    };

    for (widget, children) in widget_panes.iter() {
        if !matches!(widget.widget_type, PaneContent::Mode) {
            continue;
        }
        for child in children.iter() {
            if let Ok(mut msdf_text) = texts.get_mut(child) {
                msdf_text.text = mode.0.name().to_string();
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

/// Update contexts widget when DriftState or DocumentCache changes.
///
/// Shows MRU context badges from DocumentCache as clickable children, with
/// active context highlighted. Falls back to drift state for single-text.
pub fn update_contexts_widget(
    mut commands: Commands,
    drift_state: Res<DriftState>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    widget_panes: Query<(Entity, &WidgetPaneText, &Children)>,
    existing_badges: Query<Entity, With<ContextBadge>>,
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
        for entity in existing_badges.iter() {
            commands.entity(entity).despawn();
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

        // Despawn old badges
        for entity in existing_badges.iter() {
            commands.entity(entity).despawn();
        }

        // Spawn new badges
        let max_display = 5;
        for (i, doc_id) in mru_ids.iter().enumerate() {
            if i >= max_display {
                let remaining = mru_ids.len() - max_display;
                let overflow = commands
                    .spawn(Node {
                        padding: UiRect::axes(Val::Px(4.0), Val::Px(2.0)),
                        ..default()
                    })
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
                commands.entity(widget_entity).add_child(overflow);
                break;
            }

            let ctx_name = doc_cache
                .get(doc_id)
                .map(|c| c.context_name.as_str())
                .unwrap_or("?");

            let short = if ctx_name.len() > 12 {
                &ctx_name[..12]
            } else {
                ctx_name
            };

            let is_active = active_doc_id == Some(doc_id.as_str());
            let label = if is_active {
                format!("[{}]", short)
            } else {
                short.to_string()
            };
            let color = if is_active { theme.accent } else { theme.fg_dim };

            let badge = commands
                .spawn((
                    ContextBadge {
                        context_name: ctx_name.to_string(),
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
            commands.entity(widget_entity).add_child(badge);
        }

        // Append staged count
        let staged = drift_state.staged_count();
        if staged > 0 {
            let staged_text = commands
                .spawn(Node {
                    padding: UiRect::left(Val::Px(8.0)),
                    ..default()
                })
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
            commands.entity(widget_entity).add_child(staged_text);
        }
        return;
    }

    // No cached docs — despawn badges and fall back to single text
    for entity in existing_badges.iter() {
        commands.entity(entity).despawn();
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
                handle_context_badge_click,
            )
                .chain(),
        );
    }
}
