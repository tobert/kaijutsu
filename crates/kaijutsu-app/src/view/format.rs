//! Pure formatting functions for block rendering.
//!
//! All functions in this module are stateless — they map block data to
//! display strings. No ECS, no systems, just data transforms.

use kaijutsu_crdt::{BlockKind, BlockSnapshot, DriftKind, Role, Status};
use kaijutsu_types::ContextId;
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
            let content = block.content.trim();
            if block.is_error {
                if content.is_empty() {
                    "error".to_string()
                } else {
                    format!("error \u{2717}\n{}", content)
                }
            } else if content.is_empty() {
                "done".to_string()
            } else {
                let line_count = content.lines().count();
                if line_count <= 3 {
                    format!("done\n{}", content)
                } else {
                    format!("result\n{}", content)
                }
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
    use kaijutsu_types::{BlockId, PrincipalId, ToolKind};

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
        assert_eq!(result, "done");
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
        assert!(result.starts_with("done\n"));
        assert!(result.contains("file contents here"));
    }
}
