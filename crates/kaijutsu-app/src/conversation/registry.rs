//! Conversation registry resources.

use bevy::prelude::*;
use kaijutsu_kernel::Conversation;
use std::collections::HashMap;

/// Registry of all conversations.
///
/// Stores conversations by ID and maintains display order.
#[derive(Resource, Default)]
pub struct ConversationRegistry {
    /// Conversations by ID.
    conversations: HashMap<String, Conversation>,
    /// Ordered list of conversation IDs for display.
    order: Vec<String>,
}

impl ConversationRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a conversation to the registry.
    ///
    /// If a conversation with the same ID exists, it will be replaced.
    pub fn add(&mut self, conv: Conversation) {
        let id = conv.id.clone();
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.conversations.insert(id, conv);
    }

    /// Remove a conversation by ID.
    pub fn remove(&mut self, id: &str) -> Option<Conversation> {
        self.order.retain(|oid| oid != id);
        self.conversations.remove(id)
    }

    /// Get a conversation by ID.
    pub fn get(&self, id: &str) -> Option<&Conversation> {
        self.conversations.get(id)
    }

    /// Get a mutable conversation by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        self.conversations.get_mut(id)
    }

    /// Get all conversations in display order.
    pub fn iter(&self) -> impl Iterator<Item = &Conversation> {
        self.order.iter().filter_map(|id| self.conversations.get(id))
    }

    /// Get the number of conversations.
    pub fn len(&self) -> usize {
        self.conversations.len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.conversations.is_empty()
    }

    /// Get ordered list of conversation IDs.
    pub fn ids(&self) -> &[String] {
        &self.order
    }

    /// Move a conversation to the front of the order (most recent).
    pub fn move_to_front(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|oid| oid == id) {
            let id = self.order.remove(pos);
            self.order.insert(0, id);
        }
    }

}

/// Resource tracking the currently active conversation.
///
/// The active conversation is what's displayed in the MainCell and
/// what receives messages from the prompt.
#[derive(Resource, Default)]
pub struct CurrentConversation(pub Option<String>);

impl CurrentConversation {
    /// Get the current conversation ID, if any.
    pub fn id(&self) -> Option<&str> {
        self.0.as_deref()
    }
}
