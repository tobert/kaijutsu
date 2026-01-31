//! DAG visualization as ASCII tree.
//!
//! Provides functions to format a ConversationDAG as a human-readable tree.

use kaijutsu_crdt::{BlockId, BlockKind, ConversationDAG};

/// Format a DAG as ASCII tree lines.
pub fn format_dag_tree(dag: &ConversationDAG, max_depth: Option<u32>, expand_tools: bool) -> Vec<String> {
    let mut lines = Vec::new();

    for (idx, root_id) in dag.roots.iter().enumerate() {
        let is_last_root = idx == dag.roots.len() - 1;
        format_dag_node(dag, root_id, 0, "", is_last_root, max_depth, expand_tools, &mut lines);
    }

    lines
}

/// Recursively format a DAG node and its children.
fn format_dag_node(
    dag: &ConversationDAG,
    block_id: &BlockId,
    depth: usize,
    prefix: &str,
    is_last: bool,
    max_depth: Option<u32>,
    expand_tools: bool,
    lines: &mut Vec<String>,
) {
    // Check max depth
    if let Some(max) = max_depth {
        if depth as u32 > max {
            return;
        }
    }

    let block = match dag.get(block_id) {
        Some(b) => b,
        None => return,
    };

    // Build connector
    let connector = if depth == 0 {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    // Format block ID as agent/seq
    let short_id = format!("{}/{}", block_id.agent_id, block_id.seq);

    // Format role/kind
    let role_kind = format!("[{}/{}]", block.role.as_str(), block.kind.as_str());

    // Format content summary (truncated)
    let summary = format_content_summary(&block.content, 40);

    // Check if this is a tool_call with a single tool_result child (for collapsing)
    let children = dag.get_children(block_id);
    let can_collapse = !expand_tools
        && block.kind == BlockKind::ToolCall
        && children.len() == 1
        && dag.get(&children[0]).map(|c| c.kind == BlockKind::ToolResult).unwrap_or(false);

    if can_collapse {
        // Collapsed tool format: tool_name(...) → ✓/✗
        let result_block = dag.get(&children[0]).unwrap();
        let tool_name = block.tool_name.as_deref().unwrap_or("tool");
        let status_icon = if result_block.is_error { "✗" } else { "✓" };

        let line = format!("{}{}{}({}) → {}",
            prefix, connector, tool_name, summary, status_icon);
        lines.push(line);

        // Skip children since we collapsed them
        return;
    }

    // Normal format
    let line = format!("{}{}{} {} \"{}\"",
        prefix, connector, short_id, role_kind, summary);
    lines.push(line);

    // Process children
    let children = dag.get_children(block_id);
    let child_prefix = if depth == 0 {
        ""
    } else if is_last {
        &format!("{}   ", prefix)
    } else {
        &format!("{}│  ", prefix)
    };

    for (i, child_id) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;
        format_dag_node(dag, child_id, depth + 1, child_prefix, is_last_child, max_depth, expand_tools, lines);
    }
}

/// Format content as a truncated summary.
fn format_content_summary(content: &str, max_chars: usize) -> String {
    // Take first line only and truncate
    let first_line = content.lines().next().unwrap_or("");
    let trimmed = first_line.trim();

    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(max_chars - 3).collect();
        format!("{}...", truncated)
    }
}
