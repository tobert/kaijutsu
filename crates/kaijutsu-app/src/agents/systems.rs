//! Agent systems for Bevy.

use bevy::prelude::*;
use super::components::*;
use super::registry::*;

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
            AgentActivityMessage::Started { nick, block_id, action } => {
                info!("Agent {} started {} on block {}", nick, action, block_id);
                if let Some(agent) = registry.agents.get_mut(nick) {
                    agent.status = AgentStatus::Busy;
                    registry.version += 1;
                }
            }
            AgentActivityMessage::Progress { nick, block_id, message, percent } => {
                debug!(
                    "Agent {} progress on {}: {} ({:.0}%)",
                    nick, block_id, message, percent * 100.0
                );
            }
            AgentActivityMessage::Completed { nick, block_id, success } => {
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
            AgentActivityMessage::CursorMoved { nick, block_id, offset } => {
                debug!("Agent {} cursor at {}:{}", nick, block_id, offset);
            }
        }
    }
}

/// Sync agent badges in the UI.
///
/// Creates/updates/removes badge entities based on registry state.
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
