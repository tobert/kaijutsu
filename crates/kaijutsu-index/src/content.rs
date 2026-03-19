//! Extract indexable text from block snapshots.

use kaijutsu_types::{BlockKind, BlockSnapshot, Role};
use sha2::{Digest, Sha256};

/// Extract indexable text from a context's blocks and compute a content hash.
///
/// Filters to non-compacted, terminal-status, text/thinking blocks.
/// Concatenates with role prefixes. Truncates to `max_chars`.
///
/// Returns `(text, sha256_hex)`.
pub fn extract_context_content(
    blocks: &[BlockSnapshot],
    max_chars: usize,
) -> (String, String) {
    let mut buf = String::new();

    for block in blocks {
        // Skip non-terminal, compacted, or irrelevant blocks
        if block.compacted {
            continue;
        }
        if !block.status.is_terminal() {
            continue;
        }
        match block.kind {
            BlockKind::Text | BlockKind::Thinking => {}
            _ => continue,
        }
        if block.content.is_empty() {
            continue;
        }

        // Add role prefix
        let prefix = match block.role {
            Role::User => "[User]: ",
            Role::Model => "[Assistant]: ",
            Role::System => "[System]: ",
            Role::Tool => "[Tool]: ",
            Role::Asset => "[Asset]: ",
        };

        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(prefix);
        buf.push_str(&block.content);

        if buf.len() >= max_chars {
            buf.truncate(max_chars);
            break;
        }
    }

    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(buf.as_bytes());
        format!("{:x}", hasher.finalize())
    };

    (buf, hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, ContextId, PrincipalId, Status};

    fn test_block(role: Role, kind: BlockKind, content: &str) -> BlockSnapshot {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        BlockSnapshot {
            id: BlockId { context_id: ctx, agent_id: agent, seq: 1 },
            parent_id: None,
            role,
            kind,
            status: Status::Done,
            content: content.to_string(),
            compacted: false,
            ephemeral: false,
            collapsed: false,
            created_at: 0,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            source_context: None,
            source_model: None,
            drift_kind: None,
            tool_kind: None,
            file_path: None,
            tool_use_id: None,
            output: None,
            content_type: None,
            order_key: None,
            updated_at: 0,
            status_at: 0,
            collapsed_at: 0,
            ephemeral_at: 0,
            compacted_at: 0,
            tool_meta_at: 0,
        }
    }

    #[test]
    fn test_extract_basic() {
        let blocks = vec![
            test_block(Role::User, BlockKind::Text, "What is Rust?"),
            test_block(Role::Model, BlockKind::Text, "Rust is a systems programming language."),
        ];

        let (text, hash) = extract_context_content(&blocks, 10000);
        assert!(text.contains("[User]: What is Rust?"));
        assert!(text.contains("[Assistant]: Rust is a systems"));
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex
    }

    #[test]
    fn test_skips_compacted() {
        let mut block = test_block(Role::User, BlockKind::Text, "compacted");
        block.compacted = true;

        let (text, _) = extract_context_content(&[block], 10000);
        assert!(text.is_empty());
    }

    #[test]
    fn test_skips_running() {
        let mut block = test_block(Role::Model, BlockKind::Text, "in progress");
        block.status = Status::Running;

        let (text, _) = extract_context_content(&[block], 10000);
        assert!(text.is_empty());
    }

    #[test]
    fn test_skips_tool_calls() {
        let block = test_block(Role::Model, BlockKind::ToolCall, "{}");
        let (text, _) = extract_context_content(&[block], 10000);
        assert!(text.is_empty());
    }

    #[test]
    fn test_truncation() {
        let block = test_block(Role::User, BlockKind::Text, &"x".repeat(1000));
        let (text, _) = extract_context_content(&[block], 100);
        assert!(text.len() <= 100);
    }

    #[test]
    fn test_deterministic_hash() {
        let blocks = vec![
            test_block(Role::User, BlockKind::Text, "hello world"),
        ];
        let (_, hash1) = extract_context_content(&blocks, 10000);
        let (_, hash2) = extract_context_content(&blocks, 10000);
        assert_eq!(hash1, hash2);
    }
}
