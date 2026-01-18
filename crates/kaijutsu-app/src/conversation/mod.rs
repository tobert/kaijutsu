//! Conversation management for Kaijutsu client.
//!
//! This module provides Bevy resources and systems for managing conversations:
//! - `ConversationRegistry`: Stores all conversations
//! - `CurrentConversation`: Tracks which conversation is currently active
//! - `ConversationStore`: SQLite persistence wrapper
//! - Systems for loading/saving and switching conversations

mod registry;

pub use registry::{ConversationRegistry, CurrentConversation};

use bevy::prelude::*;
use kaijutsu_kernel::{Conversation, ConversationDb};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Plugin for conversation management.
pub struct ConversationPlugin;

impl Plugin for ConversationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConversationRegistry>()
            .init_resource::<CurrentConversation>()
            .add_systems(Startup, (setup_store, load_or_create_conversations).chain());
    }
}

/// Wrapper resource for the conversation database.
#[derive(Resource)]
pub struct ConversationStore {
    db: Arc<Mutex<ConversationDb>>,
}

impl ConversationStore {
    /// Save a conversation to the database.
    pub fn save(&self, conv: &Conversation) {
        if let Ok(db) = self.db.lock() {
            if let Err(e) = db.save(conv) {
                error!("Failed to save conversation {}: {}", conv.id, e);
            }
        }
    }

    /// Delete a conversation from the database.
    pub fn delete(&self, id: &str) {
        if let Ok(db) = self.db.lock() {
            if let Err(e) = db.delete(id) {
                error!("Failed to delete conversation {}: {}", id, e);
            }
        }
    }
}

/// Get the default database path.
fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kaijutsu")
        .join("conversations.db")
}

/// Setup the conversation store on startup.
fn setup_store(mut commands: Commands) {
    let db_path = default_db_path();

    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("Failed to create data directory: {}", e);
        }
    }

    match ConversationDb::open(&db_path) {
        Ok(db) => {
            info!("Opened conversation database at {:?}", db_path);
            commands.insert_resource(ConversationStore {
                db: Arc::new(Mutex::new(db)),
            });
        }
        Err(e) => {
            error!("Failed to open conversation database: {}", e);
            // Fall back to in-memory database
            if let Ok(db) = ConversationDb::in_memory() {
                warn!("Using in-memory conversation database (data will not persist)");
                commands.insert_resource(ConversationStore {
                    db: Arc::new(Mutex::new(db)),
                });
            }
        }
    }
}

/// Load existing conversations or create a default one.
fn load_or_create_conversations(
    store: Option<Res<ConversationStore>>,
    mut registry: ResMut<ConversationRegistry>,
    mut current: ResMut<CurrentConversation>,
) {
    let agent_id = format!("user:{}", whoami::username());
    let display_name = whoami::realname_os()
        .into_string()
        .unwrap_or_else(|_| whoami::username());

    // Try to load from database
    if let Some(ref store) = store {
        if let Ok(db) = store.db.lock() {
            match db.load_all() {
                Ok(conversations) if !conversations.is_empty() => {
                    info!("Loaded {} conversations from database", conversations.len());
                    for conv in conversations {
                        let id = conv.id.clone();
                        registry.add(conv);
                        // Set the first (most recent) as current
                        if current.0.is_none() {
                            current.0 = Some(id);
                        }
                    }
                    return;
                }
                Ok(_) => {
                    info!("No existing conversations found, creating default");
                }
                Err(e) => {
                    error!("Failed to load conversations: {}", e);
                }
            }
        }
    }

    // Create a default conversation
    let mut conv = Conversation::new("Main", &agent_id);
    conv.add_participant(kaijutsu_kernel::Participant::user(&agent_id, &display_name));

    let conv_id = conv.id.clone();

    // Save to database
    if let Some(ref store) = store {
        store.save(&conv);
    }

    registry.add(conv);
    current.0 = Some(conv_id.clone());

    info!("Created default conversation: {}", conv_id);
}
