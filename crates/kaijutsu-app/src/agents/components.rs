//! Agent-related ECS components.

#![allow(dead_code)] // Phase 2 infrastructure - not all used yet

use bevy::prelude::*;

/// Marker component for an agent presence indicator.
///
/// Attached to UI entities that show agent activity (badges, cursors, etc).
#[derive(Component)]
pub struct AgentIndicator {
    /// Nick of the agent this indicator belongs to.
    pub nick: String,
}

/// Marker for the agent list panel in the conversation header.
#[derive(Component)]
pub struct AgentListPanel;

/// Marker for a single agent badge in the UI.
#[derive(Component)]
pub struct AgentBadge {
    /// Nick of the agent.
    pub nick: String,
}

/// Visual state for agent activity indicators.
#[derive(Component, Default)]
pub struct AgentActivityIndicator {
    /// Whether the agent is currently processing.
    pub is_active: bool,
    /// Progress percentage (0.0 to 1.0) if known.
    pub progress: Option<f32>,
    /// Current status message.
    pub message: Option<String>,
}

/// Marker for the agent cursor indicator.
///
/// Shows where an agent's cursor is in a block.
#[derive(Component)]
pub struct AgentCursor {
    /// Nick of the agent.
    pub nick: String,
    /// Block being edited.
    pub block_id: String,
    /// Offset in the block.
    pub offset: u64,
}
