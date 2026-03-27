//! Agent systems for Bevy.

use super::components::*;
use super::plugin::AgentInvocationChannel;
use super::registry::*;
use bevy::prelude::*;

use crate::ui::drift::DriftState;
use crate::view::components::ContextSwitchRequested;
use crate::view::document::DocumentCache;

// Agent key triggers removed — will be redesigned with Action bindings.

/// Handle incoming agent activity messages.
///
/// Updates the registry and spawns/updates indicators.
pub fn handle_agent_activity(
    mut reader: MessageReader<AgentActivityMessage>,
    mut registry: ResMut<AgentRegistry>,
) {
    for event in reader.read() {
        match event {
            AgentActivityMessage::Started {
                nick,
                block_id,
                action,
            } => {
                info!("Agent {} started {} on block {}", nick, action, block_id);
                if let Some(agent) = registry.agents.get_mut(nick) {
                    agent.status = AgentStatus::Busy;
                    registry.version += 1;
                }
            }
            AgentActivityMessage::Progress {
                nick,
                block_id,
                message,
                percent,
            } => {
                debug!(
                    "Agent {} progress on {}: {} ({:.0}%)",
                    nick,
                    block_id,
                    message,
                    percent * 100.0
                );
            }
            AgentActivityMessage::Completed {
                nick,
                block_id,
                success,
            } => {
                info!(
                    "Agent {} {} on block {}",
                    nick,
                    if *success { "completed" } else { "failed" },
                    block_id
                );
                if let Some(agent) = registry.agents.get_mut(nick) {
                    agent.status = AgentStatus::Ready;
                    registry.version += 1;
                }
            }
            AgentActivityMessage::CursorMoved {
                nick,
                block_id,
                offset,
            } => {
                debug!("Agent {} cursor at {}:{}", nick, block_id, offset);
            }
        }
    }
}

/// Sync agent badges in the UI.
///
/// Creates/updates/removes badge entities based on registry state.
/// Phase 3: badge spawning not yet implemented, deregistered from Update schedule.
#[allow(dead_code)]
pub fn sync_agent_badges(
    registry: Res<AgentRegistry>,
    mut commands: Commands,
    existing_badges: Query<(Entity, &AgentBadge)>,
) {
    if !registry.is_changed() {
        return;
    }

    // Remove badges for agents that no longer exist
    for (entity, badge) in existing_badges.iter() {
        if registry.get(&badge.nick).is_none() {
            commands.entity(entity).despawn();
        }
    }

    // TODO: Create badges for new agents
    // This would involve spawning UI entities with AgentBadge components
}

/// Poll the agent invocation channel and dispatch actions.
///
/// Invocations arrive from the kernel via `AgentCommands` callback →
/// mpsc channel → this system. Each invocation carries a oneshot reply.
pub fn poll_agent_invocations(
    channel: Res<AgentInvocationChannel>,
    doc_cache: Res<DocumentCache>,
    drift: Res<DriftState>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
) {
    let Ok(rx) = channel.rx.lock() else {
        return;
    };
    while let Ok(invocation) = rx.try_recv() {
        let result = dispatch_agent_action(
            &invocation.action,
            &invocation.params,
            &doc_cache,
            &drift,
            &mut switch_writer,
        );
        let _ = invocation.reply.send(result);
    }
}

fn dispatch_agent_action(
    action: &str,
    params: &[u8],
    doc_cache: &DocumentCache,
    drift: &DriftState,
    switch_writer: &mut MessageWriter<ContextSwitchRequested>,
) -> Result<Vec<u8>, String> {
    match action {
        "switch_context" => {
            #[derive(serde::Deserialize)]
            struct Params {
                context_id: String,
            }
            let p: Params =
                serde_json::from_slice(params).map_err(|e| format!("invalid params: {e}"))?;

            // Reuse same resolution logic as brp_methods.rs
            let ctx_id = kaijutsu_types::ContextId::parse(&p.context_id)
                .or_else(|_| {
                    let items = drift.contexts.iter().map(|c| {
                        let label = if c.label.is_empty() {
                            None
                        } else {
                            Some(c.label.as_str())
                        };
                        (c.id, label)
                    });
                    kaijutsu_types::resolve_context_prefix(items, &p.context_id)
                })
                .map_err(|e| format!("cannot resolve context_id: {e}"))?;

            let was_cached = doc_cache.contains(ctx_id);
            switch_writer.write(ContextSwitchRequested { context_id: ctx_id });

            serde_json::to_vec(&serde_json::json!({
                "context_id": ctx_id.to_string(),
                "was_cached": was_cached,
            }))
            .map_err(|e| format!("serialize: {e}"))
        }

        "active_context" => {
            let active = doc_cache.active_id();
            let mru: Vec<_> = doc_cache
                .mru_ids()
                .iter()
                .map(|&id| {
                    let name = doc_cache
                        .get(id)
                        .map(|d| d.context_name.clone())
                        .unwrap_or_default();
                    serde_json::json!({
                        "context_id": id.to_string(),
                        "name": name,
                        "is_active": Some(id) == active,
                    })
                })
                .collect();

            serde_json::to_vec(&serde_json::json!({
                "active_id": active.map(|id| id.to_string()),
                "mru": mru,
            }))
            .map_err(|e| format!("serialize: {e}"))
        }

        _ => Err(format!("unknown action: {action}")),
    }
}
