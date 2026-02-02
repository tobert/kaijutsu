//! Agent Registry for managing attached agents.
//!
//! Agents are autonomous participants that can edit content alongside humans.
//! Each agent has capabilities (spell-check, review, generate, etc.) and can
//! be invoked on focused content.
//!
//! # Architecture
//!
//! - **AgentInfo**: Metadata about an attached agent (nick, provider, model, capabilities)
//! - **AgentRegistry**: Tracks attached agents and their state
//! - **AgentCapability**: What actions an agent can perform
//!
//! Agents integrate with the existing Participant model but add:
//! - Explicit capability declarations
//! - Activity tracking
//! - Invocation protocol

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

/// Agent capabilities - what actions an agent can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentCapability {
    /// Quick spell-checking
    SpellCheck,
    /// Grammar correction
    Grammar,
    /// Code/text formatting
    Format,
    /// Code/content review (slower, thoughtful)
    Review,
    /// Content generation
    Generate,
    /// Code refactoring suggestions
    Refactor,
    /// Explain selected content
    Explain,
    /// Translation to other languages
    Translate,
    /// Summarize long content
    Summarize,
    /// Custom capability (action specified at invocation time)
    Custom,
}

impl AgentCapability {
    /// Convert from capnp enum value.
    pub fn from_capnp(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::SpellCheck),
            1 => Some(Self::Grammar),
            2 => Some(Self::Format),
            3 => Some(Self::Review),
            4 => Some(Self::Generate),
            5 => Some(Self::Refactor),
            6 => Some(Self::Explain),
            7 => Some(Self::Translate),
            8 => Some(Self::Summarize),
            9 => Some(Self::Custom),
            _ => None,
        }
    }

    /// Convert to capnp enum value.
    pub fn to_capnp(&self) -> u16 {
        match self {
            Self::SpellCheck => 0,
            Self::Grammar => 1,
            Self::Format => 2,
            Self::Review => 3,
            Self::Generate => 4,
            Self::Refactor => 5,
            Self::Explain => 6,
            Self::Translate => 7,
            Self::Summarize => 8,
            Self::Custom => 9,
        }
    }

    /// Get a human-readable name for this capability.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SpellCheck => "spell-check",
            Self::Grammar => "grammar",
            Self::Format => "format",
            Self::Review => "review",
            Self::Generate => "generate",
            Self::Refactor => "refactor",
            Self::Explain => "explain",
            Self::Translate => "translate",
            Self::Summarize => "summarize",
            Self::Custom => "custom",
        }
    }
}

/// Agent status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    /// Available to handle requests.
    #[default]
    Ready,
    /// Currently processing a request.
    Busy,
    /// Not currently responding.
    Offline,
}

impl AgentStatus {
    /// Convert from capnp enum value.
    pub fn from_capnp(value: u16) -> Self {
        match value {
            0 => Self::Ready,
            1 => Self::Busy,
            2 => Self::Offline,
            _ => Self::Offline,
        }
    }

    /// Convert to capnp enum value.
    pub fn to_capnp(&self) -> u16 {
        match self {
            Self::Ready => 0,
            Self::Busy => 1,
            Self::Offline => 2,
        }
    }
}

/// Configuration for attaching an agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Display name: "spell-check", "claude-review".
    pub nick: String,
    /// Instance ID: "quick", "deep", "haiku".
    pub instance: String,
    /// LLM provider: "anthropic", "openai", "local".
    pub provider: String,
    /// Model ID: "claude-3-haiku", "gpt-4-mini".
    pub model_id: String,
    /// What this agent can do.
    pub capabilities: Vec<AgentCapability>,
}

/// Information about an attached agent.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// Display name.
    pub nick: String,
    /// Instance ID.
    pub instance: String,
    /// LLM provider.
    pub provider: String,
    /// Model ID.
    pub model_id: String,
    /// What this agent can do.
    pub capabilities: Vec<AgentCapability>,
    /// Current status.
    pub status: AgentStatus,
    /// When the agent was attached (Unix timestamp ms).
    pub attached_at: u64,
    /// Last activity (Unix timestamp ms).
    pub last_activity: u64,
}

impl AgentInfo {
    /// Create a new AgentInfo from config.
    pub fn from_config(config: AgentConfig) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            nick: config.nick,
            instance: config.instance,
            provider: config.provider,
            model_id: config.model_id,
            capabilities: config.capabilities,
            status: AgentStatus::Ready,
            attached_at: now,
            last_activity: now,
        }
    }

    /// Check if this agent has a specific capability.
    pub fn has_capability(&self, cap: AgentCapability) -> bool {
        self.capabilities.contains(&cap) || self.capabilities.contains(&AgentCapability::Custom)
    }

    /// Update the last activity timestamp.
    pub fn touch(&mut self) {
        self.last_activity = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    }
}

/// Agent activity event.
#[derive(Debug, Clone)]
pub enum AgentActivityEvent {
    /// Agent started working on content.
    Started {
        agent: String,
        block_id: String,
        action: String,
    },
    /// Agent is making progress.
    Progress {
        agent: String,
        block_id: String,
        message: String,
        percent: f32,
    },
    /// Agent finished.
    Completed {
        agent: String,
        block_id: String,
        success: bool,
    },
    /// Agent cursor position changed.
    CursorMoved {
        agent: String,
        block_id: String,
        offset: u64,
    },
}

/// Registry for tracking attached agents.
pub struct AgentRegistry {
    /// Attached agents by nick.
    agents: HashMap<String, AgentInfo>,
    /// Event broadcast channel.
    events: broadcast::Sender<AgentActivityEvent>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            agents: HashMap::new(),
            events,
        }
    }

    /// Attach an agent to this registry.
    ///
    /// Returns the AgentInfo if successful, or an error if the nick is already taken.
    pub fn attach(&mut self, config: AgentConfig) -> Result<AgentInfo, AgentError> {
        if self.agents.contains_key(&config.nick) {
            return Err(AgentError::NickTaken(config.nick));
        }

        let info = AgentInfo::from_config(config);
        let nick = info.nick.clone();
        self.agents.insert(nick, info.clone());
        Ok(info)
    }

    /// Detach an agent from this registry.
    pub fn detach(&mut self, nick: &str) -> Option<AgentInfo> {
        self.agents.remove(nick)
    }

    /// Get an agent by nick.
    pub fn get(&self, nick: &str) -> Option<&AgentInfo> {
        self.agents.get(nick)
    }

    /// Get a mutable reference to an agent by nick.
    pub fn get_mut(&mut self, nick: &str) -> Option<&mut AgentInfo> {
        self.agents.get_mut(nick)
    }

    /// List all attached agents.
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

    /// Update an agent's capabilities.
    pub fn set_capabilities(
        &mut self,
        nick: &str,
        capabilities: Vec<AgentCapability>,
    ) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get_mut(nick)
            .ok_or_else(|| AgentError::NotFound(nick.to_string()))?;
        agent.capabilities = capabilities;
        agent.touch();
        Ok(())
    }

    /// Update an agent's status.
    pub fn set_status(&mut self, nick: &str, status: AgentStatus) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get_mut(nick)
            .ok_or_else(|| AgentError::NotFound(nick.to_string()))?;
        agent.status = status;
        agent.touch();
        Ok(())
    }

    /// Subscribe to agent activity events.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentActivityEvent> {
        self.events.subscribe()
    }

    /// Emit an activity event.
    ///
    /// Note: This method only broadcasts the event. To update the agent's
    /// last_activity timestamp, the caller should call touch() on the agent
    /// if they have mutable access (see Kernel::emit_agent_event).
    pub fn emit(&self, event: AgentActivityEvent) {
        // Broadcast the event (ignore if no subscribers)
        let _ = self.events.send(event);
    }

    /// Number of attached agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }

    /// Check if an agent with this nick is attached.
    pub fn contains(&self, nick: &str) -> bool {
        self.agents.contains_key(nick)
    }
}

/// Errors that can occur in agent operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    /// The nick is already taken by another agent.
    #[error("agent nick already taken: {0}")]
    NickTaken(String),
    /// Agent not found.
    #[error("agent not found: {0}")]
    NotFound(String),
    /// Agent doesn't have the required capability.
    #[error("agent lacks capability: {0}")]
    MissingCapability(String),
    /// Agent is busy.
    #[error("agent is busy")]
    Busy,
    /// Agent is offline.
    #[error("agent is offline")]
    Offline,
}

/// Shared agent registry (Arc-wrapped for async access).
pub type SharedAgentRegistry = Arc<tokio::sync::RwLock<AgentRegistry>>;

/// Create a new shared agent registry.
pub fn shared_agent_registry() -> SharedAgentRegistry {
    Arc::new(tokio::sync::RwLock::new(AgentRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attach_detach() {
        let mut registry = AgentRegistry::new();

        let config = AgentConfig {
            nick: "spell-check".to_string(),
            instance: "quick".to_string(),
            provider: "anthropic".to_string(),
            model_id: "claude-3-haiku".to_string(),
            capabilities: vec![AgentCapability::SpellCheck, AgentCapability::Grammar],
        };

        let info = registry.attach(config.clone()).unwrap();
        assert_eq!(info.nick, "spell-check");
        assert!(info.has_capability(AgentCapability::SpellCheck));
        assert!(info.has_capability(AgentCapability::Grammar));
        assert!(!info.has_capability(AgentCapability::Review));

        // Can't attach same nick twice
        assert!(registry.attach(config).is_err());

        // Detach
        let detached = registry.detach("spell-check");
        assert!(detached.is_some());
        assert!(!registry.contains("spell-check"));
    }

    #[test]
    fn test_capability_filter() {
        let mut registry = AgentRegistry::new();

        registry
            .attach(AgentConfig {
                nick: "spell".to_string(),
                instance: "quick".to_string(),
                provider: "local".to_string(),
                model_id: "tiny".to_string(),
                capabilities: vec![AgentCapability::SpellCheck],
            })
            .unwrap();

        registry
            .attach(AgentConfig {
                nick: "reviewer".to_string(),
                instance: "deep".to_string(),
                provider: "anthropic".to_string(),
                model_id: "claude-3-opus".to_string(),
                capabilities: vec![AgentCapability::Review, AgentCapability::Refactor],
            })
            .unwrap();

        let spell_agents = registry.with_capability(AgentCapability::SpellCheck);
        assert_eq!(spell_agents.len(), 1);
        assert_eq!(spell_agents[0].nick, "spell");

        let review_agents = registry.with_capability(AgentCapability::Review);
        assert_eq!(review_agents.len(), 1);
        assert_eq!(review_agents[0].nick, "reviewer");
    }
}
