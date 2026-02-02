//! Agent systems for Bevy.

use bevy::prelude::*;
use bevy::input::keyboard::{Key, KeyboardInput};

use super::components::*;
use super::registry::*;
use crate::cell::{CurrentMode, EditorMode, FocusedBlockCell};

/// Handle processing chain key triggers.
///
/// Listens for Ctrl+key combinations in Normal mode to invoke agents:
/// - Ctrl+S: Spell-check
/// - Ctrl+R: Review
/// - Ctrl+G: Generate
pub fn handle_processing_chain_triggers(
    mut keyboard: MessageReader<KeyboardInput>,
    mode: Res<CurrentMode>,
    focused_block: Query<(), With<FocusedBlockCell>>,
    registry: Res<AgentRegistry>,
    mut activity_writer: MessageWriter<AgentActivityMessage>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    // Need a focused block to invoke on
    if focused_block.is_empty() {
        return;
    }

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        // Check for Ctrl modifier
        // Note: In Bevy 0.18, we check the key directly
        // The Ctrl state would need to be tracked separately via KeyCode
        // For now, we'll use Ctrl+key as a single logical key
        let capability = match &event.logical_key {
            // These will need proper Ctrl detection - placeholder for now
            Key::Character(c) if c == "s" => Some(AgentCapability::SpellCheck),
            Key::Character(c) if c == "r" => Some(AgentCapability::Review),
            Key::Character(c) if c == "g" => Some(AgentCapability::Generate),
            Key::Character(c) if c == "f" => Some(AgentCapability::Format),
            Key::Character(c) if c == "e" => Some(AgentCapability::Explain),
            _ => None,
        };

        if let Some(cap) = capability {
            // Find an agent with this capability
            let agents = registry.with_capability(cap);
            if let Some(agent) = agents.first() {
                info!(
                    "Invoking {} agent ({}) for {:?}",
                    agent.nick,
                    agent.model_display(),
                    cap
                );

                // TODO: Actually invoke via RPC
                // For now, just emit a local event for testing
                activity_writer.write(AgentActivityMessage::Started {
                    nick: agent.nick.clone(),
                    block_id: "placeholder".to_string(),
                    action: cap.name().to_string(),
                });
            } else {
                warn!("No agent available for {:?}", cap);
            }
        }
    }
}

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
