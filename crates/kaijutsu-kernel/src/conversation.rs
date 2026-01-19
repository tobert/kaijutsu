//! Conversation model for Kaijutsu.
//!
//! A conversation is a `BlockDocument` containing message blocks from multiple participants.
//! Conversations can mount multiple resource kernels for access to files, repos, etc.
//!
//! # Architecture
//!
//! ```text
//! Conversation
//!   ├── BlockDocument (messages as blocks)
//!   │     └── Blocks with author attribution
//!   ├── Participants (users and models)
//!   └── Mounts (access to resource kernels)
//! ```
//!
//! # Example
//!
//! ```ignore
//! let mut conv = Conversation::new("my-chat", "alice");
//! conv.add_participant(Participant::user("user:amy", "Amy"));
//! conv.add_participant(Participant::model("model:claude", "Claude", "anthropic", "claude-3-opus"));
//!
//! // Add a message as a user
//! conv.add_text_message("user:amy", "Help me with this code");
//!
//! // Add a response as a model
//! conv.add_thinking_message("model:claude", "Let me analyze this...");
//! conv.add_text_message("model:claude", "I can help with that!");
//! ```

use kaijutsu_crdt::{BlockDocument, BlockId};
use serde::{Deserialize, Serialize};

/// A conversation with participants and resource access.
///
/// Conversations are the primary collaboration primitive in Kaijutsu.
/// They contain:
/// - A `BlockDocument` holding all messages as CRDT-tracked blocks
/// - A list of participants (users and models) who can contribute
/// - Mounts providing access to resource kernels (worktrees, repos, etc.)
pub struct Conversation {
    /// Unique identifier for this conversation.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// The message blocks (source of truth for content).
    pub doc: BlockDocument,
    /// Participants in this conversation.
    pub participants: Vec<Participant>,
    /// Mounted resource kernels.
    pub mounts: Vec<Mount>,
    /// When the conversation was created (Unix millis).
    pub created_at: u64,
    /// When the conversation was last updated (Unix millis).
    pub updated_at: u64,
}

impl std::fmt::Debug for Conversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conversation")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("message_count", &self.doc.block_count())
            .field("participants", &self.participants.len())
            .field("mounts", &self.mounts.len())
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl Conversation {
    /// Create a new conversation.
    pub fn new(name: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let name = name.into();
        let agent_id = agent_id.into();
        let id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            id: id.clone(),
            name,
            doc: BlockDocument::new(&id, &agent_id),
            participants: Vec::new(),
            mounts: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a conversation with a specific ID.
    pub fn with_id(id: impl Into<String>, name: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let id = id.into();
        let name = name.into();
        let agent_id = agent_id.into();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            id: id.clone(),
            name,
            doc: BlockDocument::new(&id, &agent_id),
            participants: Vec::new(),
            mounts: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    // =========================================================================
    // Participants
    // =========================================================================

    /// Add a participant to the conversation.
    pub fn add_participant(&mut self, participant: Participant) {
        // Avoid duplicates
        if !self.participants.iter().any(|p| p.id == participant.id) {
            self.participants.push(participant);
            self.touch();
        }
    }

    /// Remove a participant by ID.
    pub fn remove_participant(&mut self, participant_id: &str) {
        self.participants.retain(|p| p.id != participant_id);
        self.touch();
    }

    /// Get a participant by ID.
    pub fn get_participant(&self, id: &str) -> Option<&Participant> {
        self.participants.iter().find(|p| p.id == id)
    }

    /// Check if a participant exists.
    pub fn has_participant(&self, id: &str) -> bool {
        self.participants.iter().any(|p| p.id == id)
    }

    // =========================================================================
    // Mounts
    // =========================================================================

    /// Add a mount to the conversation.
    pub fn add_mount(&mut self, mount: Mount) {
        // Replace existing mount at same path
        self.mounts.retain(|m| m.mount_path != mount.mount_path);
        self.mounts.push(mount);
        self.touch();
    }

    /// Remove a mount by path.
    pub fn remove_mount(&mut self, path: &str) {
        self.mounts.retain(|m| m.mount_path != path);
        self.touch();
    }

    /// Get a mount by path.
    pub fn get_mount(&self, path: &str) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.mount_path == path)
    }

    // =========================================================================
    // Messages
    // =========================================================================

    /// Add a text message to the conversation.
    ///
    /// The author must be a participant in the conversation.
    pub fn add_text_message(&mut self, author: &str, text: impl Into<String>) -> Option<BlockId> {
        let last_id = self.doc.blocks_ordered().last().map(|b| b.id.clone());
        let block_id = self.doc
            .insert_text_block_with_author(last_id.as_ref(), text, author)
            .ok()?;
        self.touch();
        Some(block_id)
    }

    /// Add a thinking/reasoning block to the conversation.
    pub fn add_thinking_message(&mut self, author: &str, text: impl Into<String>) -> Option<BlockId> {
        let last_id = self.doc.blocks_ordered().last().map(|b| b.id.clone());
        let block_id = self.doc
            .insert_thinking_block_with_author(last_id.as_ref(), text, author)
            .ok()?;
        self.touch();
        Some(block_id)
    }

    /// Add a tool use block to the conversation.
    pub fn add_tool_use(
        &mut self,
        author: &str,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Option<BlockId> {
        let last_id = self.doc.blocks_ordered().last().map(|b| b.id.clone());
        let block_id = self.doc
            .insert_tool_use_with_author(last_id.as_ref(), tool_id, name, input, author)
            .ok()?;
        self.touch();
        Some(block_id)
    }

    /// Add a tool result block to the conversation.
    pub fn add_tool_result(
        &mut self,
        author: &str,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Option<BlockId> {
        let last_id = self.doc.blocks_ordered().last().map(|b| b.id.clone());
        let block_id = self.doc
            .insert_tool_result_with_author(last_id.as_ref(), tool_use_id, content, is_error, author)
            .ok()?;
        self.touch();
        Some(block_id)
    }

    /// Append text to an existing block.
    pub fn append_to_message(&mut self, block_id: &BlockId, text: &str) -> bool {
        if self.doc.append_text(block_id, text).is_ok() {
            self.touch();
            true
        } else {
            false
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the number of messages (blocks) in the conversation.
    pub fn message_count(&self) -> usize {
        self.doc.block_count()
    }

    /// Check if the conversation is empty.
    pub fn is_empty(&self) -> bool {
        self.doc.is_empty()
    }

    /// Get all messages in order.
    pub fn messages(&self) -> Vec<kaijutsu_crdt::BlockSnapshot> {
        self.doc.blocks_ordered()
    }

    /// Get the full text content of the conversation.
    pub fn full_text(&self) -> String {
        self.doc.full_text()
    }

    // =========================================================================
    // Internal
    // =========================================================================

    /// Update the updated_at timestamp.
    fn touch(&mut self) {
        self.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
    }
}

/// Someone participating in the conversation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Participant {
    /// Unique identifier (e.g., "user:amy", "model:claude-1").
    pub id: String,
    /// Display name shown in UI.
    pub display_name: String,
    /// What kind of participant this is.
    pub kind: ParticipantKind,
    /// When they joined the conversation (Unix millis).
    pub joined_at: u64,
}

impl Participant {
    /// Create a new participant.
    pub fn new(id: impl Into<String>, display_name: impl Into<String>, kind: ParticipantKind) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            kind,
            joined_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// Create a user participant.
    pub fn user(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self::new(id, display_name, ParticipantKind::User)
    }

    /// Create a model participant.
    pub fn model(
        id: impl Into<String>,
        display_name: impl Into<String>,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            display_name,
            ParticipantKind::Model {
                provider: provider.into(),
                model_id: model_id.into(),
            },
        )
    }

    /// Check if this is a user participant.
    pub fn is_user(&self) -> bool {
        matches!(self.kind, ParticipantKind::User)
    }

    /// Check if this is a model participant.
    pub fn is_model(&self) -> bool {
        matches!(self.kind, ParticipantKind::Model { .. })
    }
}

/// The kind of participant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ParticipantKind {
    /// A human user.
    User,
    /// An AI model.
    Model {
        /// Provider (e.g., "anthropic", "openai").
        provider: String,
        /// Model ID (e.g., "claude-3-opus", "gpt-4").
        model_id: String,
    },
}

/// A mounted resource kernel.
///
/// Mounts provide access to external resources like git repos, worktrees, or other kernels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    /// ID of the kernel providing the resource.
    pub kernel_id: String,
    /// Path where the resource is mounted (e.g., "/project").
    pub mount_path: String,
    /// Access level for this mount.
    pub access: AccessLevel,
}

impl Mount {
    /// Create a new mount.
    pub fn new(
        kernel_id: impl Into<String>,
        mount_path: impl Into<String>,
        access: AccessLevel,
    ) -> Self {
        Self {
            kernel_id: kernel_id.into(),
            mount_path: mount_path.into(),
            access,
        }
    }

    /// Create a read-only mount.
    pub fn read_only(kernel_id: impl Into<String>, mount_path: impl Into<String>) -> Self {
        Self::new(kernel_id, mount_path, AccessLevel::Read)
    }

    /// Create a read-write mount.
    pub fn read_write(kernel_id: impl Into<String>, mount_path: impl Into<String>) -> Self {
        Self::new(kernel_id, mount_path, AccessLevel::ReadWrite)
    }
}

/// Access level for a mount.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AccessLevel {
    /// Read-only access.
    Read,
    /// Read and write access.
    ReadWrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_creation() {
        let conv = Conversation::new("Test Chat", "alice");
        assert_eq!(conv.name, "Test Chat");
        assert!(conv.is_empty());
        assert!(conv.participants.is_empty());
    }

    #[test]
    fn test_participants() {
        let mut conv = Conversation::new("Test", "alice");

        let user = Participant::user("user:amy", "Amy");
        let model = Participant::model("model:claude", "Claude", "anthropic", "claude-3-opus");

        conv.add_participant(user.clone());
        conv.add_participant(model.clone());

        assert_eq!(conv.participants.len(), 2);
        assert!(conv.has_participant("user:amy"));
        assert!(conv.has_participant("model:claude"));

        // No duplicates
        conv.add_participant(Participant::user("user:amy", "Amy Again"));
        assert_eq!(conv.participants.len(), 2);

        // Remove
        conv.remove_participant("user:amy");
        assert!(!conv.has_participant("user:amy"));
    }

    #[test]
    fn test_messages() {
        let mut conv = Conversation::new("Test", "alice");
        conv.add_participant(Participant::user("user:amy", "Amy"));
        conv.add_participant(Participant::model("model:claude", "Claude", "anthropic", "claude-3-opus"));

        // Add messages
        let user_msg = conv.add_text_message("user:amy", "Hello!");
        assert!(user_msg.is_some());

        let thinking = conv.add_thinking_message("model:claude", "Let me think...");
        assert!(thinking.is_some());

        let response = conv.add_text_message("model:claude", "Hi there!");
        assert!(response.is_some());

        assert_eq!(conv.message_count(), 3);

        // Check authors
        let messages = conv.messages();
        assert_eq!(messages[0].author, "user:amy");
        assert_eq!(messages[1].author, "model:claude");
        assert_eq!(messages[2].author, "model:claude");
    }

    #[test]
    fn test_mounts() {
        let mut conv = Conversation::new("Test", "alice");

        conv.add_mount(Mount::read_only("kernel-123", "/project"));
        conv.add_mount(Mount::read_write("kernel-456", "/notes"));

        assert_eq!(conv.mounts.len(), 2);
        assert!(conv.get_mount("/project").is_some());

        // Replace mount at same path
        conv.add_mount(Mount::read_write("kernel-789", "/project"));
        assert_eq!(conv.mounts.len(), 2);
        assert_eq!(conv.get_mount("/project").unwrap().kernel_id, "kernel-789");
    }
}
