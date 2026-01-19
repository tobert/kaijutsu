//! Conversation commands.
//!
//! Handles `:conv` subcommands:
//! - `:conv list` - list all conversations
//! - `:conv new [name]` - create a new conversation
//! - `:conv switch <id>` - switch to a conversation
//! - `:conv delete <id>` - delete a conversation
//! - `:conv rename <name>` - rename current conversation
//! - `:conv` - show help for conversation commands

use bevy::prelude::*;
use kaijutsu_kernel::{Conversation, Participant};

use crate::conversation::{ConversationRegistry, ConversationStore, CurrentConversation};
use super::{CommandBuffer, CommandOutput};

/// Handle conversation commands.
pub fn handle_conversation_commands(
    command_buffer: Res<CommandBuffer>,
    mut command_output: ResMut<CommandOutput>,
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<crate::cell::CurrentMode>,
    mut registry: ResMut<ConversationRegistry>,
    mut current: ResMut<CurrentConversation>,
    store: Option<Res<ConversationStore>>,
) {
    use crate::cell::EditorMode;

    // Handle quick switch shortcuts (Ctrl+1/2/3)
    if mode.0 == EditorMode::Normal {
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        if ctrl {
            let ids = registry.ids().to_vec();
            if keys.just_pressed(KeyCode::Digit1) && !ids.is_empty() {
                current.0 = Some(ids[0].clone());
                registry.move_to_front(&ids[0]);
                command_output.success(format!("Switched to conversation 1"));
                return;
            }
            if keys.just_pressed(KeyCode::Digit2) && ids.len() > 1 {
                current.0 = Some(ids[1].clone());
                registry.move_to_front(&ids[1]);
                command_output.success(format!("Switched to conversation 2"));
                return;
            }
            if keys.just_pressed(KeyCode::Digit3) && ids.len() > 2 {
                current.0 = Some(ids[2].clone());
                registry.move_to_front(&ids[2]);
                command_output.success(format!("Switched to conversation 3"));
                return;
            }
        }
    }

    // Only process commands in command mode when Enter is pressed
    if mode.0 != EditorMode::Command {
        return;
    }
    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }

    // Check if this is a conv command
    if command_buffer.command() != Some("conv") {
        return;
    }

    let cmd_args = command_buffer.args();
    let subcommand = cmd_args.first().copied().unwrap_or("help");
    let args: Vec<&str> = if cmd_args.len() > 1 {
        cmd_args[1..].to_vec()
    } else {
        vec![]
    };

    match subcommand {
        "list" | "ls" => {
            handle_list(&registry, &current, &mut command_output);
        }
        "new" | "create" => {
            let name = if args.is_empty() {
                format!("Conversation {}", registry.len() + 1)
            } else {
                args.join(" ")
            };
            handle_new(&mut registry, &mut current, &name, store.as_deref(), &mut command_output);
        }
        "switch" | "sw" => {
            if args.is_empty() {
                command_output.error("Usage: :conv switch <id or number>");
            } else {
                handle_switch(&mut registry, &mut current, args[0], &mut command_output);
            }
        }
        "delete" | "del" | "rm" => {
            if args.is_empty() {
                command_output.error("Usage: :conv delete <id or number>");
            } else {
                handle_delete(&mut registry, &mut current, args[0], store.as_deref(), &mut command_output);
            }
        }
        "rename" | "name" => {
            if args.is_empty() {
                command_output.error("Usage: :conv rename <new name>");
            } else {
                let new_name = args.join(" ");
                handle_rename(&mut registry, &current, &new_name, store.as_deref(), &mut command_output);
            }
        }
        "help" | "?" | _ => {
            command_output.success(
                "Conversation commands:\n\
                 :conv list           - List all conversations\n\
                 :conv new [name]     - Create new conversation\n\
                 :conv switch <id>    - Switch to conversation\n\
                 :conv delete <id>    - Delete conversation\n\
                 :conv rename <name>  - Rename current conversation\n\
                 Ctrl+1/2/3           - Quick switch"
            );
        }
    }
}

/// List all conversations.
fn handle_list(
    registry: &ConversationRegistry,
    current: &CurrentConversation,
    output: &mut CommandOutput,
) {
    if registry.is_empty() {
        output.success("No conversations");
        return;
    }

    let mut lines = vec!["Conversations:".to_string()];
    for (i, conv) in registry.iter().enumerate() {
        let marker = if current.id() == Some(&conv.id) { "→ " } else { "  " };
        let short_id = &conv.id[..8.min(conv.id.len())];
        lines.push(format!(
            "{}{}. {} [{}] ({} msgs)",
            marker,
            i + 1,
            conv.name,
            short_id,
            conv.message_count()
        ));
    }
    output.success(lines.join("\n"));
}

/// Create a new conversation.
fn handle_new(
    registry: &mut ConversationRegistry,
    current: &mut CurrentConversation,
    name: &str,
    store: Option<&ConversationStore>,
    output: &mut CommandOutput,
) {
    // Create conversation with local user
    let agent_id = format!("user:{}", whoami::username());
    let mut conv = Conversation::new(name, &agent_id);

    // Add local user as participant
    let display_name = whoami::realname_os()
        .into_string()
        .unwrap_or_else(|_| whoami::username());
    conv.add_participant(Participant::user(&agent_id, &display_name));

    // Save to database
    if let Some(store) = store {
        store.save(&conv);
    }

    let conv_id = conv.id.clone();
    registry.add(conv);
    registry.move_to_front(&conv_id);
    current.0 = Some(conv_id.clone());

    output.success(format!("Created conversation: {}", name));
}

/// Delete a conversation.
fn handle_delete(
    registry: &mut ConversationRegistry,
    current: &mut CurrentConversation,
    id_or_number: &str,
    store: Option<&ConversationStore>,
    output: &mut CommandOutput,
) {
    // Find the conversation
    let target_id = resolve_conversation_id(registry, id_or_number);

    let Some(target_id) = target_id else {
        output.error(format!("No conversation matching: {}", id_or_number));
        return;
    };

    // Don't delete the last conversation
    if registry.len() == 1 {
        output.error("Cannot delete the last conversation");
        return;
    }

    // Get the name before deleting
    let name = registry.get(&target_id).map(|c| c.name.clone()).unwrap_or_default();

    // If deleting current, switch to another first
    if current.id() == Some(&target_id) {
        let ids = registry.ids().to_vec();
        let next_id = ids.iter().find(|id| *id != &target_id);
        if let Some(next) = next_id {
            current.0 = Some(next.clone());
        }
    }

    // Delete from database
    if let Some(store) = store {
        store.delete(&target_id);
    }

    // Remove from registry
    registry.remove(&target_id);

    output.success(format!("Deleted conversation: {}", name));
}

/// Rename the current conversation.
fn handle_rename(
    registry: &mut ConversationRegistry,
    current: &CurrentConversation,
    new_name: &str,
    store: Option<&ConversationStore>,
    output: &mut CommandOutput,
) {
    let Some(conv_id) = current.id() else {
        output.error("No current conversation");
        return;
    };

    let Some(conv) = registry.get_mut(conv_id) else {
        output.error("Current conversation not found");
        return;
    };

    let old_name = conv.name.clone();
    conv.name = new_name.to_string();
    // Update timestamp
    conv.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Save to database
    if let Some(store) = store {
        store.save(conv);
    }

    output.success(format!("Renamed '{}' → '{}'", old_name, new_name));
}

/// Helper to resolve a conversation ID from a number or prefix.
fn resolve_conversation_id(registry: &ConversationRegistry, id_or_number: &str) -> Option<String> {
    // Try parsing as a number first
    if let Ok(num) = id_or_number.parse::<usize>() {
        let ids = registry.ids().to_vec();
        if num > 0 && num <= ids.len() {
            return Some(ids[num - 1].clone());
        }
        return None;
    }

    // Try matching by ID prefix
    let ids = registry.ids().to_vec();
    let matches: Vec<&String> = ids.iter()
        .filter(|id| id.starts_with(id_or_number))
        .collect();

    if matches.len() == 1 {
        Some(matches[0].clone())
    } else {
        None
    }
}

/// Switch to a conversation.
fn handle_switch(
    registry: &mut ConversationRegistry,
    current: &mut CurrentConversation,
    id_or_number: &str,
    output: &mut CommandOutput,
) {
    // Try parsing as a number first
    if let Ok(num) = id_or_number.parse::<usize>() {
        let ids = registry.ids().to_vec();
        if num > 0 && num <= ids.len() {
            let id = &ids[num - 1];
            registry.move_to_front(id);
            current.0 = Some(id.clone());
            if let Some(conv) = registry.get(id) {
                output.success(format!("Switched to: {}", conv.name));
            }
            return;
        } else {
            output.error(format!("No conversation #{}", num));
            return;
        }
    }

    // Try matching by ID prefix
    let ids = registry.ids().to_vec();
    let matches: Vec<&String> = ids.iter()
        .filter(|id| id.starts_with(id_or_number))
        .collect();

    match matches.len() {
        0 => {
            output.error(format!("No conversation matching: {}", id_or_number));
        }
        1 => {
            let id = matches[0];
            registry.move_to_front(id);
            current.0 = Some(id.clone());
            if let Some(conv) = registry.get(id) {
                output.success(format!("Switched to: {}", conv.name));
            }
        }
        _ => {
            output.error(format!(
                "Multiple matches for '{}': {}",
                id_or_number,
                matches.iter().map(|id| &id[..8.min(id.len())]).collect::<Vec<_>>().join(", ")
            ));
        }
    }
}
