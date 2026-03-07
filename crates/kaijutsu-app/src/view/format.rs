//! Pure formatting functions for block rendering.
//!
//! All functions in this module are stateless — they map block data to
//! display strings. No ECS, no systems, just data transforms.

use kaijutsu_crdt::{BlockKind, BlockSnapshot, DriftKind, Role, Status};
use kaijutsu_types::{ContextId, OutputData, OutputNode};
use crate::ui::theme::Theme;

/// Map a block to its semantic text color based on BlockKind and Role.
///
/// This enables visual distinction between different block types:
/// - User messages: soft white
/// - Assistant messages: light blue
/// - Thinking: dim gray (de-emphasized)
/// - Tool calls: amber
/// - Tool results: green (error: red)
/// - Shell: cyan for commands, gray for output
pub fn block_color(block: &BlockSnapshot, theme: &Theme) -> bevy::prelude::Color {
    match block.kind {
        BlockKind::Text | BlockKind::File => {
            match block.role {
                Role::User => theme.block_user,
                Role::Model => theme.block_assistant,
                Role::System => theme.fg_dim,
                Role::Tool | Role::Asset => theme.block_tool_result,
            }
        }
        BlockKind::Thinking => theme.block_thinking,
        BlockKind::ToolCall => {
            if block.status == Status::Done {
                theme.fg
            } else {
                theme.block_tool_call
            }
        }
        BlockKind::ToolResult => {
            if block.is_error {
                theme.block_tool_error
            } else {
                theme.block_tool_result
            }
        }
        BlockKind::Drift => match block.drift_kind {
            Some(DriftKind::Push) => theme.block_drift_push,
            Some(DriftKind::Pull) | Some(DriftKind::Distill) => theme.block_drift_pull,
            Some(DriftKind::Merge) => theme.block_drift_merge,
            Some(DriftKind::Commit) => theme.block_drift_commit,
            None => theme.fg_dim,
        },
    }
}

/// Strip provider prefix from model name for compact display.
///
/// `"anthropic/claude-sonnet-4-5"` → `"claude-sonnet-4-5"`
/// `"claude-opus-4-6"` → `"claude-opus-4-6"`
fn truncate_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Format a drift block with variant-specific visual treatment.
///
/// `local_ctx`: if provided, determines push direction arrow (→ outgoing, ← incoming).
fn format_drift_block(block: &BlockSnapshot, local_ctx: Option<ContextId>) -> String {
    let ctx_short = block.source_context.map(|c| c.short()).unwrap_or_else(|| "?".to_string());
    let model = block.source_model.as_deref().map(truncate_model).unwrap_or("unknown");
    let ctx_label = format!("@{}", ctx_short);

    // Determine direction arrow: → if we sent it, ← if we received it
    let arrow = match (local_ctx, block.source_context) {
        (Some(local), Some(source)) if source == local => "\u{2192}",
        _ => "\u{2190}",
    };

    match block.drift_kind {
        Some(DriftKind::Push) => {
            let preview = block.content.lines().next().unwrap_or("");
            format!("{} {} ({})  {}", arrow, ctx_label, model, preview)
        }
        Some(DriftKind::Pull) | Some(DriftKind::Distill) => {
            format!("pulled from {} ({})\n{}", ctx_label, model, block.content)
        }
        Some(DriftKind::Merge) => {
            format!("\u{21c4} merged from {} ({})\n{}", ctx_label, model, block.content)
        }
        Some(DriftKind::Commit) => {
            format!("# {}  {}", ctx_label, block.content.lines().next().unwrap_or(""))
        }
        None => {
            format!("~ {} ({})  {}", ctx_label, model, block.content.lines().next().unwrap_or(""))
        }
    }
}

/// Display a JSON value with real newlines in string values.
///
/// `serde_json::to_string_pretty` re-escapes newlines in string values as `\n`,
/// which renders as literal backslash-n in the UI. This walks the JSON tree and
/// outputs string content directly so embedded newlines display correctly.
fn display_json_value(value: &serde_json::Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            let continuation_pad = "  ".repeat(indent + 1);
            let indented = s.replace('\n', &format!("\n{continuation_pad}"));
            format!("\"{indented}\"")
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            let inner_pad = "  ".repeat(indent + 1);
            let items: Vec<String> = arr
                .iter()
                .map(|v| format!("{inner_pad}{}", display_json_value(v, indent + 1)))
                .collect();
            format!("[\n{}\n{pad}]", items.join(",\n"))
        }
        serde_json::Value::Object(obj) => {
            if obj.is_empty() {
                return "{}".to_string();
            }
            let inner_pad = "  ".repeat(indent + 1);
            let entries: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{inner_pad}\"{k}\": {}",
                        display_json_value(v, indent + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{pad}}}", entries.join(",\n"))
        }
    }
}

/// Format tool call arguments as compact key: value lines.
///
/// Flat JSON objects render as `key: value` per line (unquoted strings).
/// Multiline string values show first line + `(N lines)` suffix.
/// Values > 60 chars are truncated. Total > 5 lines shows first 4 + count.
/// Non-object inputs fall through to `display_json_value`.
fn format_tool_args(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return display_json_value(value, 0),
    };

    let max_lines = 5;
    let max_value_len = 60;
    let mut lines: Vec<String> = Vec::new();

    for (key, val) in obj {
        let formatted = match val {
            serde_json::Value::String(s) => {
                let first_line = s.lines().next().unwrap_or("");
                let line_count = s.lines().count();
                if line_count > 1 {
                    let truncated = if first_line.len() > max_value_len {
                        format!("{}...", &first_line[..max_value_len])
                    } else {
                        first_line.to_string()
                    };
                    format!("{} ({} lines)", truncated, line_count)
                } else if s.len() > max_value_len {
                    format!("{}...", &s[..max_value_len])
                } else {
                    s.clone()
                }
            }
            serde_json::Value::Null => "null".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            other => {
                let json = serde_json::to_string(other).unwrap_or_default();
                if json.len() > max_value_len {
                    format!("{}...", &json[..max_value_len])
                } else {
                    json
                }
            }
        };
        lines.push(format!("{}: {}", key, formatted));
    }

    if lines.len() > max_lines {
        let remaining = lines.len() - (max_lines - 1);
        lines.truncate(max_lines - 1);
        lines.push(format!("... ({} more)", remaining));
    }

    lines.join("\n")
}

/// Format structured `OutputData` into space-aligned text.
///
/// Dispatch order:
/// 1. Simple text → passthrough
/// 2. Tabular (has headers or cells) → `format_output_table()`
/// 3. Tree (has children) → indented tree
/// 4. Flat list (names only) → one name per line
/// 5. Fallback → `to_canonical_string()`
fn format_output_data(data: &OutputData) -> String {
    // 1. Simple text passthrough
    if let Some(text) = data.as_text() {
        return text.to_string();
    }

    // 2. Tabular
    if data.headers.is_some() || data.is_tabular() {
        return format_output_table(data);
    }

    // 3. Tree
    if !data.is_flat() {
        return format_output_tree(data);
    }

    // 4. Flat list (names only)
    if !data.root.is_empty() && data.root.iter().all(|n| n.cells.is_empty()) {
        return data.root.iter()
            .map(|n| n.display_name().to_string())
            .collect::<Vec<_>>()
            .join("\n");
    }

    // 5. Fallback
    data.to_canonical_string()
}

/// Format tabular OutputData as space-padded aligned columns.
fn format_output_table(data: &OutputData) -> String {
    let headers = data.headers.as_deref();

    // Build rows as [name, cell0, cell1, ...]
    let rows: Vec<Vec<&str>> = data.root.iter().map(|node| {
        let mut row = vec![node.display_name()];
        row.extend(node.cells.iter().map(|s| s.as_str()));
        row
    }).collect();

    // Determine number of columns
    let header_cols = headers.map_or(0, |h| h.len());
    let max_row_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let num_cols = header_cols.max(max_row_cols);

    if num_cols == 0 {
        return String::new();
    }

    // Calculate max width per column
    let mut widths = vec![0usize; num_cols];
    if let Some(hdrs) = headers {
        for (i, h) in hdrs.iter().enumerate() {
            widths[i] = widths[i].max(h.len());
        }
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let gap = "  ";
    let mut lines = Vec::new();

    // Header line
    if let Some(hdrs) = headers {
        let line: String = hdrs.iter().enumerate().map(|(i, h)| {
            if i + 1 < num_cols {
                format!("{:<width$}", h, width = widths[i])
            } else {
                h.to_string()
            }
        }).collect::<Vec<_>>().join(gap);
        lines.push(line);
    }

    // Data rows
    for row in &rows {
        let line: String = (0..num_cols).map(|i| {
            let cell = row.get(i).copied().unwrap_or("");
            if i + 1 < num_cols {
                format!("{:<width$}", cell, width = widths[i])
            } else {
                cell.to_string()
            }
        }).collect::<Vec<_>>().join(gap);
        lines.push(line);
    }

    lines.join("\n")
}

/// Format tree OutputData with indentation.
fn format_output_tree(data: &OutputData) -> String {
    let mut lines = Vec::new();
    for node in &data.root {
        format_tree_node(node, 0, &mut lines);
    }
    lines.join("\n")
}

fn format_tree_node(node: &OutputNode, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    if node.cells.is_empty() {
        lines.push(format!("{}{}", indent, node.display_name()));
    } else {
        let cells = node.cells.join("  ");
        lines.push(format!("{}{}  {}", indent, node.display_name(), cells));
    }
    for child in &node.children {
        format_tree_node(child, depth + 1, lines);
    }
}

/// Format a single block for display.
///
/// Returns the formatted text for one block, including visual markers.
/// `local_ctx`: optional local context ID for drift push direction.
pub fn format_single_block(block: &BlockSnapshot, local_ctx: Option<ContextId>) -> String {
    match block.kind {
        BlockKind::Thinking => {
            if block.collapsed {
                "Thinking [collapsed]".to_string()
            } else {
                format!("Thinking\n{}", block.content)
            }
        }
        BlockKind::Text => block.content.clone(),
        BlockKind::ToolCall => {
            let name = block.tool_name.as_deref().unwrap_or("unknown");
            let status_tag = match block.status {
                Status::Running => " [running]",
                Status::Pending => " [pending]",
                _ => "",
            };
            let mut output = format!("{}{}", name, status_tag);
            if let Some(ref input_str) = block.tool_input {
                if let Ok(input_val) = serde_json::from_str::<serde_json::Value>(input_str) {
                    if !input_val.is_null() {
                        let args = format_tool_args(&input_val);
                        if !args.is_empty() {
                            output.push('\n');
                            output.push_str(&args);
                        }
                    }
                }
            }
            output
        }
        BlockKind::ToolResult => {
            // Prefer structured OutputData when available
            let body = if let Some(ref output) = block.output {
                let formatted = format_output_data(output);
                if formatted.is_empty() { None } else { Some(formatted) }
            } else {
                let trimmed = block.content.trim();
                if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
            };

            if block.is_error {
                match body {
                    None => "\u{2717}".to_string(),
                    Some(text) => text,
                }
            } else {
                body.unwrap_or_default()
            }
        }
        BlockKind::File => {
            let path = block.file_path.as_deref().unwrap_or("file");
            format!("{}\n{}", path, block.content)
        }
        BlockKind::Drift => format_drift_block(block, local_ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, OutputData, OutputNode, PrincipalId, ToolKind};

    fn test_block_id() -> BlockId {
        BlockId::new(ContextId::new(), PrincipalId::new(), 0)
    }

    #[test]
    fn test_format_tool_args_flat_object() {
        let input = serde_json::json!({
            "path": "/etc/hosts",
            "limit": 10
        });
        let result = format_tool_args(&input);
        assert!(result.contains("path: /etc/hosts"));
        assert!(result.contains("limit: 10"));
    }

    #[test]
    fn test_format_tool_args_truncates_long_values() {
        let long_val = "x".repeat(80);
        let input = serde_json::json!({ "data": long_val });
        let result = format_tool_args(&input);
        assert!(result.contains("..."));
        assert!(result.len() < 80);
    }

    #[test]
    fn test_format_tool_args_multiline_string() {
        let input = serde_json::json!({
            "content": "line1\nline2\nline3\nline4"
        });
        let result = format_tool_args(&input);
        assert!(result.contains("line1"));
        assert!(result.contains("(4 lines)"));
    }

    #[test]
    fn test_format_tool_args_many_keys_truncated() {
        let input = serde_json::json!({
            "a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6
        });
        let result = format_tool_args(&input);
        assert!(result.contains("... ("));
        assert!(result.contains("more)"));
    }

    #[test]
    fn test_format_tool_args_empty_object() {
        let input = serde_json::json!({});
        let result = format_tool_args(&input);
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_format_tool_args_non_object() {
        let input = serde_json::json!("just a string");
        let result = format_tool_args(&input);
        assert!(result.contains("just a string"));
    }

    #[test]
    fn test_tool_call_plain_text() {
        let block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            ToolKind::Mcp,
            "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
            Role::Model,
            None,
        );
        let result = format_single_block(&block, None);
        assert!(!result.contains('┌'));
        assert!(!result.contains('└'));
        assert!(result.contains("read_file"));
        assert!(result.contains("path: /etc/hosts"));
        assert!(result.contains("[running]"));
    }

    #[test]
    fn test_tool_call_empty_args() {
        let mut block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            ToolKind::Mcp,
            "list_all",
            serde_json::json!(null),
            Role::Model,
            None,
        );
        block.status = Status::Done;
        let result = format_single_block(&block, None);
        assert_eq!(result, "list_all");
    }

    #[test]
    fn test_tool_result_success_empty() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let call_id = BlockId::new(ctx, agent, 0);
        let result_block = BlockSnapshot::tool_result(
            BlockId::new(ctx, agent, 1),
            call_id,
            ToolKind::Mcp,
            "",
            false,
            Some(0),
            None,
        );
        let result = format_single_block(&result_block, None);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_output_table_with_headers() {
        let data = OutputData::table(
            vec!["PID".into(), "NAME".into(), "STATUS".into()],
            vec![
                OutputNode::new("1").with_cells(vec!["init".into(), "running".into()]),
                OutputNode::new("42").with_cells(vec!["bash".into(), "sleeping".into()]),
                OutputNode::new("1337").with_cells(vec!["vim".into(), "running".into()]),
            ],
        );
        let result = format_output_data(&data);
        assert!(!result.contains('\t'), "should not contain tabs");
        // Check alignment: PID column should be padded to 4 chars ("1337")
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 4); // header + 3 rows
        assert!(lines[0].starts_with("PID "));
        assert!(lines[1].starts_with("1   "));
        assert!(lines[3].starts_with("1337"));
    }

    #[test]
    fn test_format_output_table_no_headers() {
        let data = OutputData::nodes(vec![
            OutputNode::new("foo").with_cells(vec!["100".into()]),
            OutputNode::new("barbaz").with_cells(vec!["2".into()]),
        ]);
        let result = format_output_data(&data);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("foo   "));
        assert!(lines[1].starts_with("barbaz"));
    }

    #[test]
    fn test_format_output_simple_text() {
        let data = OutputData::text("hello world");
        assert_eq!(format_output_data(&data), "hello world");
    }

    #[test]
    fn test_format_output_flat_list() {
        let data = OutputData::nodes(vec![
            OutputNode::new("alpha"),
            OutputNode::new("beta"),
            OutputNode::new("gamma"),
        ]);
        assert_eq!(format_output_data(&data), "alpha\nbeta\ngamma");
    }

    #[test]
    fn test_format_output_tree() {
        let data = OutputData::nodes(vec![
            OutputNode::new("src").with_children(vec![
                OutputNode::new("main.rs"),
                OutputNode::new("lib").with_children(vec![
                    OutputNode::new("utils.rs"),
                ]),
            ]),
        ]);
        let result = format_output_data(&data);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines[0], "src");
        assert_eq!(lines[1], "  main.rs");
        assert_eq!(lines[2], "  lib");
        assert_eq!(lines[3], "    utils.rs");
    }

    #[test]
    fn test_tool_result_with_output_data() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let call_id = BlockId::new(ctx, agent, 0);
        let output = OutputData::table(
            vec!["PID".into(), "CMD".into()],
            vec![
                OutputNode::new("1").with_cells(vec!["init".into()]),
                OutputNode::new("2").with_cells(vec!["bash".into()]),
            ],
        );
        let mut block = BlockSnapshot::tool_result(
            BlockId::new(ctx, agent, 1),
            call_id,
            ToolKind::Shell,
            "1\tinit\n2\tbash",
            false,
            Some(0),
            None,
        );
        block.output = Some(output);
        let result = format_single_block(&block, None);
        assert!(!result.contains('\t'), "should use OutputData, not raw TSV");
        assert!(result.contains("PID"));
        assert!(result.contains("init"));
    }

    #[test]
    fn test_tool_result_success_short() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let call_id = BlockId::new(ctx, agent, 0);
        let result_block = BlockSnapshot::tool_result(
            BlockId::new(ctx, agent, 1),
            call_id,
            ToolKind::Mcp,
            "file contents here",
            false,
            Some(0),
            None,
        );
        let result = format_single_block(&result_block, None);
        assert_eq!(result, "file contents here");
    }
}
