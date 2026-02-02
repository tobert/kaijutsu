//! Client-side agent registry.
//!
//! Mirrors the server's agent registry for local UI updates.

#![allow(dead_code)] // Phase 2 infrastructure - not all used yet

use bevy::prelude::*;
use std::collections::HashMap;

/// Agent capability (mirrors kernel type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentCapability {
    SpellCheck,
    Grammar,
    Format,
    Review,
    Generate,
    Refactor,
    Explain,
    Translate,
    Summarize,
    Custom,
}

impl AgentCapability {
    /// Get a human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SpellCheck => "Spell Check",
            Self::Grammar => "Grammar",
            Self::Format => "Format",
            Self::Review => "Review",
            Self::Generate => "Generate",
            Self::Refactor => "Refactor",
            Self::Explain => "Explain",
            Self::Translate => "Translate",
            Self::Summarize => "Summarize",
            Self::Custom => "Custom",
        }
    }

    /// Get a short key for UI display.
    pub fn key(&self) -> &'static str {
        match self {
            Self::SpellCheck => "S",
            Self::Grammar => "G",
            Self::Format => "F",
            Self::Review => "R",
            Self::Generate => "N",
            Self::Refactor => "X",
            Self::Explain => "E",
            Self::Translate => "T",
            Self::Summarize => "U",
            Self::Custom => "C",
        }
    }
}

/// Agent status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    Ready,
    Busy,
    Offline,
}

/// Information about an attached agent.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub nick: String,
    pub instance: String,
    pub provider: String,
    pub model_id: String,
    pub capabilities: Vec<AgentCapability>,
    pub status: AgentStatus,
    pub attached_at: u64,
    pub last_activity: u64,
}

impl AgentInfo {
    /// Check if agent has a capability.
    pub fn has_capability(&self, cap: AgentCapability) -> bool {
        self.capabilities.contains(&cap) || self.capabilities.contains(&AgentCapability::Custom)
    }

    /// Get display name (nick + instance).
    pub fn display_name(&self) -> String {
        if self.instance.is_empty() || self.instance == "default" {
            self.nick.clone()
        } else {
            format!("{}:{}", self.nick, self.instance)
        }
    }

    /// Get model display (provider/model).
    pub fn model_display(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }
}

/// Client-side registry of attached agents.
///
/// This resource mirrors the server's agent registry for UI rendering.
/// Updated via RPC subscription events.
#[derive(Resource, Default)]
pub struct AgentRegistry {
    /// Map of agents by nick.
    pub agents: HashMap<String, AgentInfo>,
    /// Version number, incremented on changes for change detection.
    pub version: u64,
}

impl AgentRegistry {
    /// Update or insert an agent.
    pub fn upsert(&mut self, info: AgentInfo) {
        self.agents.insert(info.nick.clone(), info);
        self.version += 1;
    }

    /// Remove an agent.
    pub fn remove(&mut self, nick: &str) -> Option<AgentInfo> {
        let removed = self.agents.remove(nick);
        if removed.is_some() {
            self.version += 1;
        }
        removed
    }

    /// Get an agent by nick.
    pub fn get(&self, nick: &str) -> Option<&AgentInfo> {
        self.agents.get(nick)
    }

    /// List all agents.
    pub fn list(&self) -> Vec<&AgentInfo> {
        self.agents.values().collect()
    }

    /// Find agents with a specific capability.
    pub fn with_capability(&self, cap: AgentCapability) -> Vec<&AgentInfo> {
        self.agents
            .values()
            .filter(|a| a.has_capability(cap))
            .collect()
    }

    /// Number of attached agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }

    /// Clear all agents (on disconnect).
    pub fn clear(&mut self) {
        self.agents.clear();
        self.version += 1;
    }
}

/// Event for agent activity updates.
#[derive(Message, Debug, Clone)]
pub enum AgentActivityMessage {
    /// Agent started working on a block.
    Started {
        nick: String,
        block_id: String,
        action: String,
    },
    /// Agent progress update.
    Progress {
        nick: String,
        block_id: String,
        message: String,
        percent: f32,
    },
    /// Agent completed work.
    Completed {
        nick: String,
        block_id: String,
        success: bool,
    },
    /// Agent cursor moved.
    CursorMoved {
        nick: String,
        block_id: String,
        offset: u64,
    },
}
