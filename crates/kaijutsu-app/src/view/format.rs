//! Pure formatting functions for block rendering.
//!
//! All functions in this module are stateless — they map block data to
//! display strings. No ECS, no systems, just data transforms.

use crate::ui::theme::Theme;
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, DriftKind, ErrorCategory, Role, Status};
use kaijutsu_types::{ContextId, OutputData, OutputEntryType, OutputNode};

/// Looks up a sibling block by id within the same document, for resolving an
/// `Error` block's provenance (parent ToolResult/ToolCall, interrupted turn).
/// The render buffer (`RenderBlockStore`) already indexes blocks by id, so
/// this is an O(1) lookup, not a document scan — see `render.rs`'s call site.
pub type BlockLookup<'a> = &'a dyn Fn(&BlockId) -> Option<BlockSnapshot>;

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
        // Task rendering deliberately trails this slice (docs/tasks.md) —
        // role-based coloring is the same honest placeholder Text/File get,
        // not a dedicated task palette.
        BlockKind::Text | BlockKind::File | BlockKind::Task => match block.role {
            Role::User => theme.block_user,
            Role::Model => theme.block_assistant,
            Role::System => theme.fg_dim,
            Role::Tool | Role::Asset => theme.block_tool_result,
        },
        BlockKind::Thinking => theme.block_thinking,
        BlockKind::ToolCall => {
            if block.role == Role::User {
                theme.block_user // user-initiated shell — same color as user text
            } else if block.status == Status::Done {
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
        BlockKind::Error => {
            match block.error.as_ref().map(|e| e.severity) {
                Some(kaijutsu_types::ErrorSeverity::Warning) => theme.block_error_warning,
                Some(kaijutsu_types::ErrorSeverity::Fatal) => theme.block_error_fatal,
                _ => theme.block_error_severity,
            }
        }
        BlockKind::Notification => theme.block_notification,
        BlockKind::Resource => theme.block_resource,
        BlockKind::Trace => theme.fg_dim,
        BlockKind::Drift => match block.drift_kind {
            Some(DriftKind::Push) => theme.block_drift_push,
            Some(DriftKind::Pull) | Some(DriftKind::Distill) => theme.block_drift_pull,
            Some(DriftKind::Merge) => theme.block_drift_merge,
            Some(DriftKind::Notification) => theme.block_drift_pull,
            Some(DriftKind::Fork) => theme.block_drift_merge,
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
    let ctx_short = block
        .source_context
        .map(|c| c.short())
        .unwrap_or_else(|| "?".to_string());
    let model = block
        .source_model
        .as_deref()
        .map(truncate_model)
        .unwrap_or("unknown");
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
            format!(
                "\u{21c4} merged from {} ({})\n{}",
                ctx_label, model, block.content
            )
        }
        Some(DriftKind::Notification) => {
            let preview = block.content.lines().next().unwrap_or("");
            format!("\u{1f514} {}  {}", ctx_label, preview)
        }
        Some(DriftKind::Fork) => {
            format!("\u{2442} {}\n{}", ctx_label, block.content)
        }
        None => {
            format!(
                "~ {} ({})  {}",
                ctx_label,
                model,
                block.content.lines().next().unwrap_or("")
            )
        }
    }
}

/// Number of detail lines shown in a collapsed Error block's stub preview.
const ERROR_STUB_DETAIL_LINES: usize = 3;

/// Total line budget a collapsed Error stub can occupy: provenance + summary
/// + up to `ERROR_STUB_DETAIL_LINES` detail lines + a trailing "N more
/// lines" hint. Used by `geometry::estimate_block_height` so a freshly
/// seeded row isn't sized for the single line a `Thinking` stub gets.
pub const ERROR_STUB_MAX_LINES: usize = 2 + ERROR_STUB_DETAIL_LINES + 1;

/// The key that toggles block collapse, for the stub's "N more lines" hint.
/// Mirrors `input::defaults`'s `Action::CollapseToggle` binding (plain `c`
/// in the Navigation context) — this module can't read the binding table
/// (pure formatting, no ECS resources), so keep this in sync by hand if
/// that binding ever moves.
const COLLAPSE_TOGGLE_KEY_HINT: &str = "c";

/// Format a `BlockKind::Error` block: a provenance line (what failed) always
/// first, then either the full summary+detail (expanded) or a capped stub
/// (collapsed) — errors are never dropped from the document, only their
/// on-screen footprint is capped (see CLAUDE.md "durable state" doctrine).
fn format_error_block(block: &BlockSnapshot, resolve_parent: BlockLookup) -> String {
    let provenance = error_provenance_line(block, resolve_parent);
    let detail = block.error.as_ref().and_then(|p| p.detail.as_deref());

    if block.collapsed {
        format_error_stub(&provenance, &block.content, detail)
    } else {
        match detail {
            Some(detail) => format!("{provenance}\n{}\n{detail}", block.content),
            None => format!("{provenance}\n{}", block.content),
        }
    }
}

/// Build the capped stub: provenance, summary, first `ERROR_STUB_DETAIL_LINES`
/// lines of detail, then a "N more lines" hint if detail was truncated.
fn format_error_stub(provenance: &str, summary: &str, detail: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(provenance);
    out.push('\n');
    out.push_str(summary);

    let Some(detail) = detail else {
        return out;
    };
    let lines: Vec<&str> = detail.lines().collect();
    if lines.is_empty() {
        return out;
    }
    let shown = lines.len().min(ERROR_STUB_DETAIL_LINES);
    out.push('\n');
    out.push_str(&lines[..shown].join("\n"));

    let remaining = lines.len() - shown;
    if remaining > 0 {
        out.push('\n');
        out.push_str(&format!(
            "\u{2026} {remaining} more line{} ({COLLAPSE_TOGGLE_KEY_HINT} to expand)",
            if remaining == 1 { "" } else { "s" }
        ));
    }
    out
}

/// Resolve "what did this error belong to" for the stub/expanded header —
/// Amy, looking at the live screen: *"I have no idea what these tool errors
/// and stream errors should be connected to."* Every `Error` block's
/// `parent_id` points at the block that failed; this walks that edge (and,
/// for tool errors, one more hop from the ToolResult to its ToolCall) to
/// name it. An unresolvable parent is reported as `orphan` rather than
/// omitted — that absence is itself information worth surfacing.
fn error_provenance_line(block: &BlockSnapshot, resolve_parent: BlockLookup) -> String {
    let category = block.error.as_ref().map(|p| p.category);
    let prefix = match category {
        Some(c) => format!("{} error", c.as_str()),
        None => "error".to_string(),
    };

    let Some(parent_id) = block.parent_id else {
        return format!("{prefix} \u{2190} orphan (no parent recorded)");
    };
    let Some(parent) = resolve_parent(&parent_id) else {
        return format!("{prefix} \u{2190} orphan (parent block not found)");
    };

    if category == Some(ErrorCategory::Tool) {
        describe_tool_provenance(&prefix, &parent, resolve_parent)
    } else {
        format!("{prefix} \u{2190} {}", describe_block(&parent))
    }
}

/// Tool errors attach to the failed `ToolResult`; walk one more hop to its
/// `ToolCall` for the tool name and a per-call ordinal (the call's
/// agent-local `seq`, cheap and already on hand — no document scan needed).
fn describe_tool_provenance(prefix: &str, parent: &BlockSnapshot, resolve_parent: BlockLookup) -> String {
    let call = match parent.kind {
        BlockKind::ToolCall => Some(parent.clone()),
        BlockKind::ToolResult => parent.parent_id.and_then(|id| resolve_parent(&id)),
        _ => None,
    };
    match call {
        Some(c) if c.kind == BlockKind::ToolCall => {
            let name = c.tool_name.as_deref().unwrap_or("unknown tool");
            format!("{prefix} \u{2190} {name} (#{})", c.id.seq)
        }
        _ => format!("{prefix} \u{2190} orphan (tool call not found)"),
    }
}

/// Human label for a resolved parent block — used for every non-Tool error
/// category (Stream, Rpc, Render, Parse, Validation, Kernel).
fn describe_block(block: &BlockSnapshot) -> String {
    match block.kind {
        BlockKind::Text => match block.role {
            Role::Model => "assistant turn".to_string(),
            Role::User => "user message".to_string(),
            Role::System => "system message".to_string(),
            Role::Tool | Role::Asset => "tool text".to_string(),
        },
        BlockKind::ToolCall => format!(
            "tool call {}",
            block.tool_name.as_deref().unwrap_or("(unnamed)")
        ),
        BlockKind::ToolResult => "tool result".to_string(),
        BlockKind::Thinking => "assistant thinking".to_string(),
        other => format!("{other:?} block"),
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
                .map(|(k, v)| format!("{inner_pad}\"{k}\": {}", display_json_value(v, indent + 1)))
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

/// Pre-computed layout for an OutputData table.
///
/// Maps each line's columns to byte ranges in the formatted text,
/// enabling per-cell coloring in the Vello rich content renderer.
pub struct OutputLayout {
    pub rows: Vec<OutputLayoutRow>,
}

/// Layout info for a single row in the formatted output.
#[allow(dead_code)]
pub struct OutputLayoutRow {
    /// Entry type of this row's node (for coloring the name column).
    pub entry_type: OutputEntryType,
    /// Whether this is a header row.
    pub is_header: bool,
    /// Byte start..end per column within the line.
    pub col_byte_ranges: Vec<(usize, usize)>,
    /// Byte offset of this line within the full formatted text.
    pub line_start: usize,
}

/// Compute layout mapping for a formatted OutputData table.
///
/// Returns `None` for non-tabular data (simple text, flat lists, trees).
/// The byte ranges reference positions in the string returned by `format_output_data`.
pub fn compute_output_layout(data: &OutputData, formatted_text: &str) -> Option<OutputLayout> {
    // Only tabular data gets rich coloring
    if data.as_text().is_some() {
        return None;
    }
    let is_tabular = data.headers.is_some() || data.is_tabular();
    let is_tree = !data.is_flat();
    let is_flat_names = !data.root.is_empty() && data.root.iter().all(|n| n.cells.is_empty());

    if !is_tabular && !is_tree && !is_flat_names {
        return None;
    }

    // For flat name lists: one name per line, full line = name column
    if !is_tabular && !is_tree && is_flat_names {
        let mut rows = Vec::new();
        let mut offset = 0;
        for node in &data.root {
            let name = node.display_name();
            rows.push(OutputLayoutRow {
                entry_type: node.entry_type,
                is_header: false,
                col_byte_ranges: vec![(offset, offset + name.len())],
                line_start: offset,
            });
            offset += name.len() + 1; // +1 for newline
        }
        return Some(OutputLayout { rows });
    }

    // For trees: compute byte ranges from indented lines
    if is_tree {
        let mut rows = Vec::new();
        let mut flat_nodes = Vec::new();
        fn collect_nodes(node: &OutputNode, flat: &mut Vec<OutputEntryType>) {
            flat.push(node.entry_type);
            for child in &node.children {
                collect_nodes(child, flat);
            }
        }
        for node in &data.root {
            collect_nodes(node, &mut flat_nodes);
        }

        let mut offset = 0;
        for (i, line) in formatted_text.lines().enumerate() {
            let entry_type = flat_nodes.get(i).copied().unwrap_or_default();
            rows.push(OutputLayoutRow {
                entry_type,
                is_header: false,
                col_byte_ranges: vec![(offset, offset + line.len())],
                line_start: offset,
            });
            offset += line.len() + 1;
        }
        return Some(OutputLayout { rows });
    }

    // Tabular: recompute column widths (same logic as format_output_table)
    let headers = data.headers.as_deref();
    let node_rows: Vec<Vec<&str>> = data
        .root
        .iter()
        .map(|node| {
            let mut row = vec![node.display_name()];
            row.extend(node.cells.iter().map(|s| s.as_str()));
            row
        })
        .collect();

    let header_cols = headers.map_or(0, |h| h.len());
    let max_row_cols = node_rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let num_cols = header_cols.max(max_row_cols);
    if num_cols == 0 {
        return None;
    }

    let mut widths = vec![0usize; num_cols];
    if let Some(hdrs) = headers {
        for (i, h) in hdrs.iter().enumerate() {
            widths[i] = widths[i].max(h.len());
        }
    }
    for row in &node_rows {
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let gap = 2; // "  "
    let mut rows = Vec::new();
    let mut byte_offset = 0;

    // Header row
    if let Some(hdrs) = headers {
        let line_start = byte_offset;
        let mut col_ranges = Vec::new();
        let mut col_offset = byte_offset;
        for (i, h) in hdrs.iter().enumerate() {
            let start = col_offset;
            let end = start + h.len();
            col_ranges.push((start, end));
            if i + 1 < num_cols {
                col_offset = start + widths[i] + gap;
            } else {
                col_offset = end;
            }
        }
        let line_len = formatted_text.lines().next().map_or(0, |l| l.len());
        byte_offset += line_len + 1; // +1 for newline
        rows.push(OutputLayoutRow {
            entry_type: OutputEntryType::Text,
            is_header: true,
            col_byte_ranges: col_ranges,
            line_start,
        });
    }

    // Data rows
    for (row_idx, node_row) in node_rows.iter().enumerate() {
        let line_start = byte_offset;
        let mut col_ranges = Vec::new();
        let mut col_offset = byte_offset;
        for (i, &width) in widths.iter().enumerate() {
            let cell = node_row.get(i).copied().unwrap_or("");
            let start = col_offset;
            let end = start + cell.len();
            col_ranges.push((start, end));
            if i + 1 < num_cols {
                col_offset = start + width + gap;
            } else {
                col_offset = end;
            }
        }
        let line_idx = if headers.is_some() {
            row_idx + 1
        } else {
            row_idx
        };
        let line_len = formatted_text.lines().nth(line_idx).map_or(0, |l| l.len());
        byte_offset += line_len + 1;

        rows.push(OutputLayoutRow {
            entry_type: data.root[row_idx].entry_type,
            is_header: false,
            col_byte_ranges: col_ranges,
            line_start,
        });
    }

    Some(OutputLayout { rows })
}

/// Format structured `OutputData` into space-aligned text.
///
/// Dispatch order:
/// 1. Simple text → passthrough
/// 2. Tabular (has headers or cells) → `format_output_table()`
/// 3. Tree (has children) → indented tree
/// 4. Flat list (names only) → one name per line
/// 5. Fallback → `to_canonical_string()`
pub fn format_output_data(data: &OutputData) -> String {
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
        return data
            .root
            .iter()
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
    let rows: Vec<Vec<&str>> = data
        .root
        .iter()
        .map(|node| {
            let mut row = vec![node.display_name()];
            row.extend(node.cells.iter().map(|s| s.as_str()));
            row
        })
        .collect();

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
        let line: String = hdrs
            .iter()
            .enumerate()
            .map(|(i, h)| {
                if i + 1 < num_cols {
                    format!("{:<width$}", h, width = widths[i])
                } else {
                    h.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(gap);
        lines.push(line);
    }

    // Data rows
    for row in &rows {
        let line: String = (0..num_cols)
            .map(|i| {
                let cell = row.get(i).copied().unwrap_or("");
                if i + 1 < num_cols {
                    format!("{:<width$}", cell, width = widths[i])
                } else {
                    cell.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(gap);
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
/// Trailing whitespace is always stripped — LLM streaming and tool output
/// commonly leave trailing newlines that inflate block height.
/// `local_ctx`: optional local context ID for drift push direction.
/// `resolve_parent`: sibling-block lookup, consulted only for `BlockKind::Error`
/// blocks to resolve provenance (what failed). Pass `&|_| None` where no
/// document is available (e.g. a block rendered outside its context).
pub fn format_single_block(
    block: &BlockSnapshot,
    local_ctx: Option<ContextId>,
    resolve_parent: BlockLookup,
) -> String {
    let raw = format_block_inner(block, local_ctx, resolve_parent);
    // Universal trim — catches trailing whitespace from any block kind
    // (Thinking, ToolResult with output data, File, Drift, etc.)
    let trimmed = raw.trim_end();
    if trimmed.len() == raw.len() {
        raw
    } else {
        trimmed.to_string()
    }
}

/// Inner formatting dispatch — may produce trailing whitespace.
fn format_block_inner(
    block: &BlockSnapshot,
    local_ctx: Option<ContextId>,
    resolve_parent: BlockLookup,
) -> String {
    match block.kind {
        BlockKind::Thinking => {
            if block.collapsed {
                "Thinking [collapsed]".to_string()
            } else {
                format!("Thinking\n{}", block.content)
            }
        }
        BlockKind::Text => block.content.to_string(),
        BlockKind::ToolCall => {
            let name = block.tool_name.as_deref().unwrap_or("unknown");
            // For shell commands, show just the code value
            // For other tools, show "ToolName: primary_arg" on one line
            if let Some(ref input_str) = block.tool_input
                && let Ok(input_val) = serde_json::from_str::<serde_json::Value>(input_str)
            {
                if let Some(obj) = input_val.as_object() {
                    // Shell: show the code (with $ prefix for user-initiated)
                    if let Some(code) = obj.get("code").and_then(|v| v.as_str()) {
                        return if block.role == Role::User {
                            format!("$ {}", code)
                        } else {
                            code.to_string()
                        };
                    }
                    // Single-arg tool: "ToolName: value"
                    if obj.len() == 1 {
                        let (key, val) = obj.iter().next().unwrap();
                        let val_str = match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).unwrap_or_default(),
                        };
                        let _ = key; // suppress unused
                        return format!("{}: {}", name, val_str);
                    }
                    // Multi-arg: "ToolName: primary" then remaining args
                    if !obj.is_empty() {
                        let mut iter = obj.iter();
                        let (_, first_val) = iter.next().unwrap();
                        let primary = match first_val {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).unwrap_or_default(),
                        };
                        let mut output = format!("{}: {}", name, primary);
                        let remaining: serde_json::Map<String, serde_json::Value> =
                            iter.map(|(k, v)| (k.clone(), v.clone())).collect();
                        if !remaining.is_empty() {
                            let args = format_tool_args(&serde_json::Value::Object(remaining));
                            output.push('\n');
                            output.push_str(&args);
                        }
                        return output;
                    }
                }
                if !input_val.is_null() {
                    let args = format_tool_args(&input_val);
                    if !args.is_empty() {
                        return format!("{}: {}", name, args);
                    }
                }
            }
            name.to_string()
        }
        BlockKind::ToolResult => {
            // Prefer structured OutputData when available
            let body = if let Some(ref output) = block.output {
                let formatted = format_output_data(output);
                if formatted.is_empty() {
                    None
                } else {
                    Some(formatted)
                }
            } else {
                let trimmed = block.content.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            };

            // stderr is now stored separately from content (stdout). Surface it
            // after the body so failures/warnings stay visible in the UI — it
            // used to be merged into content at the source.
            let body = match (body, block.stderr.as_deref().map(str::trim)) {
                (Some(b), Some(err)) if !err.is_empty() => Some(format!("{b}\n{err}")),
                (None, Some(err)) if !err.is_empty() => Some(err.to_string()),
                (b, _) => b,
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
        BlockKind::Error => format_error_block(block, resolve_parent),
        BlockKind::Notification => {
            if let Some(ref payload) = block.notification {
                if let Some(ref detail) = payload.detail {
                    format!("{}\n{}", block.content, detail)
                } else {
                    block.content.clone()
                }
            } else {
                block.content.clone()
            }
        }
        BlockKind::Resource => {
            if let Some(ref payload) = block.resource {
                if let Some(ref text) = payload.text {
                    // Preview up to ~1 KB inline; truncation is a UI concern,
                    // LLM hydration uses RESOURCE_CONTENT_HYDRATION_BUDGET instead.
                    if text.chars().count() > 1024 {
                        let head: String = text.chars().take(1024).collect();
                        format!("{}\n{}\n...[truncated]", block.content, head)
                    } else {
                        format!("{}\n{}", block.content, text)
                    }
                } else if let Some(ref blob) = payload.blob_base64 {
                    let bytes = blob.len().saturating_mul(3) / 4;
                    format!("{}\n[binary: {} bytes]", block.content, bytes)
                } else {
                    block.content.clone()
                }
            } else {
                block.content.clone()
            }
        }
        BlockKind::Trace => block.content.clone(),
        // Placeholder — a real task view (status glyph, subtask indent,
        // groom actions) is deferred (docs/tasks.md); this just shows the
        // status and title so a Task block isn't invisible in the meantime.
        BlockKind::Task => format!("[{}] {}", block.task_status.as_str(), block.content),
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
        let result = format_single_block(&block, None, &|_| None);
        // New format: "ToolName: primary_arg" — no status tag, no box chars
        assert!(!result.contains('┌'));
        assert!(!result.contains('└'));
        assert!(result.contains("read_file"));
        assert!(result.contains("/etc/hosts"));
        // Status tag moved to border label — not in body
        assert!(!result.contains("[running]"));
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
        let result = format_single_block(&block, None, &|_| None);
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
        let result = format_single_block(&result_block, None, &|_| None);
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
        let data = OutputData::nodes(vec![OutputNode::new("src").with_children(vec![
            OutputNode::new("main.rs"),
            OutputNode::new("lib").with_children(vec![OutputNode::new("utils.rs")]),
        ])]);
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
        let result = format_single_block(&block, None, &|_| None);
        assert!(!result.contains('\t'), "should use OutputData, not raw TSV");
        assert!(result.contains("PID"));
        assert!(result.contains("init"));
    }

    #[test]
    fn test_compute_output_layout_table_byte_ranges() {
        use kaijutsu_types::OutputEntryType;
        let data = OutputData::table(
            vec!["NAME".into(), "SIZE".into()],
            vec![
                OutputNode::new("foo")
                    .with_entry_type(OutputEntryType::File)
                    .with_cells(vec!["100".into()]),
                OutputNode::new("bar")
                    .with_entry_type(OutputEntryType::Directory)
                    .with_cells(vec!["4096".into()]),
            ],
        );
        let text = format_output_data(&data);
        let layout = compute_output_layout(&data, &text).expect("should produce layout");
        assert_eq!(layout.rows.len(), 3); // header + 2 data rows

        // Header row
        assert!(layout.rows[0].is_header);
        let (hs, he) = layout.rows[0].col_byte_ranges[0];
        assert_eq!(&text[hs..he], "NAME");

        // First data row — name column
        assert!(!layout.rows[1].is_header);
        let (ns, ne) = layout.rows[1].col_byte_ranges[0];
        assert_eq!(&text[ns..ne], "foo");
        assert_eq!(layout.rows[1].entry_type, OutputEntryType::File);

        // Second data row — name column
        let (ns2, ne2) = layout.rows[2].col_byte_ranges[0];
        assert_eq!(&text[ns2..ne2], "bar");
        assert_eq!(layout.rows[2].entry_type, OutputEntryType::Directory);
    }

    #[test]
    fn test_compute_output_layout_flat_list() {
        use kaijutsu_types::OutputEntryType;
        let data = OutputData::nodes(vec![
            OutputNode::new("src").with_entry_type(OutputEntryType::Directory),
            OutputNode::new("main.rs").with_entry_type(OutputEntryType::File),
        ]);
        let text = format_output_data(&data);
        let layout = compute_output_layout(&data, &text).expect("should produce layout");
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.rows[0].entry_type, OutputEntryType::Directory);
        assert_eq!(layout.rows[1].entry_type, OutputEntryType::File);

        let (s, e) = layout.rows[0].col_byte_ranges[0];
        assert_eq!(&text[s..e], "src");
    }

    #[test]
    fn test_compute_output_layout_simple_text_returns_none() {
        let data = OutputData::text("hello");
        let text = format_output_data(&data);
        assert!(compute_output_layout(&data, &text).is_none());
    }

    #[test]
    fn test_user_shell_tool_call_has_dollar_prefix() {
        let block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            ToolKind::Shell,
            "shell",
            serde_json::json!({"code": "cargo check"}),
            Role::User,
            None,
        );
        let result = format_single_block(&block, None, &|_| None);
        assert_eq!(result, "$ cargo check");
    }

    #[test]
    fn test_model_shell_tool_call_no_prefix() {
        let block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            ToolKind::Shell,
            "shell",
            serde_json::json!({"code": "cargo check"}),
            Role::Model,
            None,
        );
        let result = format_single_block(&block, None, &|_| None);
        assert_eq!(result, "cargo check");
    }

    #[test]
    fn test_user_shell_tool_call_color() {
        let theme = Theme::default();
        let block = BlockSnapshot::tool_call(
            test_block_id(),
            None,
            ToolKind::Shell,
            "shell",
            serde_json::json!({"code": "ls"}),
            Role::User,
            None,
        );
        let color = block_color(&block, &theme);
        assert_eq!(
            color, theme.block_user,
            "user shell should use block_user color"
        );
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
        let result = format_single_block(&result_block, None, &|_| None);
        assert_eq!(result, "file contents here");
    }

    #[test]
    fn test_trim_end_all_block_kinds() {
        // Text block — trailing whitespace stripped
        let mut block = BlockSnapshot::text(test_block_id(), None, Role::Model, "hello\n\n\n");
        assert_eq!(format_single_block(&block, None, &|_| None), "hello");

        // Thinking block — trailing whitespace stripped
        block.kind = BlockKind::Thinking;
        block.content = "deep thought\n\n\n".to_string();
        let result = format_single_block(&block, None, &|_| None);
        assert_eq!(result, "Thinking\ndeep thought");

        // ToolResult with trailing whitespace
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let call_id = BlockId::new(ctx, agent, 0);
        let mut result_block = BlockSnapshot::tool_result(
            BlockId::new(ctx, agent, 1),
            call_id,
            ToolKind::Shell,
            "output\n\n\n",
            false,
            Some(0),
            None,
        );
        assert_eq!(format_single_block(&result_block, None, &|_| None), "output");

        // ToolResult with OutputData text having trailing whitespace
        result_block.output = Some(OutputData::text("table output\n\n\n"));
        assert_eq!(format_single_block(&result_block, None, &|_| None), "table output");
    }

    // ------------------------------------------------------------------
    // BlockKind::Error — provenance line, stub cap, and full expanded view
    // ------------------------------------------------------------------

    fn tool_error_chain() -> (BlockSnapshot, BlockSnapshot, BlockSnapshot) {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let call_id = BlockId::new(ctx, agent, 0);
        let call = BlockSnapshot::tool_call(
            call_id,
            None,
            ToolKind::Shell,
            "kaish_exec",
            serde_json::json!({"code": "false"}),
            Role::Model,
            None,
        );
        let result_id = BlockId::new(ctx, agent, 1);
        let result = BlockSnapshot::tool_result(
            result_id,
            call_id,
            ToolKind::Shell,
            "",
            true,
            Some(1),
            None,
        );
        let error_id = BlockId::new(ctx, agent, 2);
        let payload = kaijutsu_types::ErrorPayload {
            category: kaijutsu_types::ErrorCategory::Tool,
            severity: kaijutsu_types::ErrorSeverity::Error,
            code: None,
            detail: Some("line1\nline2".to_string()),
            span: None,
            source_kind: Some(BlockKind::ToolResult),
        };
        let error_block = BlockSnapshot::error_for(error_id, result_id, payload, "tool error: boom");
        (call, result, error_block)
    }

    #[test]
    fn error_block_expanded_shows_tool_provenance_and_full_detail() {
        let (call, result, error_block) = tool_error_chain();
        let siblings = vec![call, result];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "tool error \u{2190} kaish_exec (#0)");
        assert_eq!(lines.next().unwrap(), "tool error: boom");
        assert_eq!(lines.next().unwrap(), "line1");
        assert_eq!(lines.next().unwrap(), "line2");
        assert!(lines.next().is_none(), "expanded view has no truncation hint");
    }

    #[test]
    fn error_block_collapsed_caps_detail_at_three_lines_with_a_hint() {
        let (call, result, mut error_block) = tool_error_chain();
        error_block.collapsed = true;
        error_block.error.as_mut().unwrap().detail =
            Some("l1\nl2\nl3\nl4\nl5".to_string());
        let siblings = vec![call, result];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "tool error \u{2190} kaish_exec (#0)",
                "tool error: boom",
                "l1",
                "l2",
                "l3",
                "\u{2026} 2 more lines (c to expand)",
            ]
        );
    }

    #[test]
    fn error_block_collapsed_singular_hint_for_one_remaining_line() {
        let (call, result, mut error_block) = tool_error_chain();
        error_block.collapsed = true;
        error_block.error.as_mut().unwrap().detail = Some("l1\nl2\nl3\nl4".to_string());
        let siblings = vec![call, result];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        assert!(text.ends_with("\u{2026} 1 more line (c to expand)"));
    }

    #[test]
    fn error_block_collapsed_short_detail_has_no_hint() {
        let (call, result, mut error_block) = tool_error_chain();
        error_block.collapsed = true; // detail is already only 2 lines
        let siblings = vec![call, result];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        assert!(!text.contains("more line"));
        assert!(text.ends_with("line2"));
    }

    #[test]
    fn error_block_stream_provenance_names_the_interrupted_turn() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let turn_id = BlockId::new(ctx, agent, 0);
        let turn = BlockSnapshot::text(turn_id, None, Role::Model, "part of a reply...");
        let error_id = BlockId::new(ctx, agent, 1);
        let payload = kaijutsu_types::ErrorPayload {
            category: kaijutsu_types::ErrorCategory::Stream,
            severity: kaijutsu_types::ErrorSeverity::Error,
            code: None,
            detail: Some("connection reset".to_string()),
            span: None,
            source_kind: None,
        };
        let error_block = BlockSnapshot::error_for(error_id, turn_id, payload, "stream error: boom");
        let siblings = vec![turn];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        assert!(text.starts_with("stream error \u{2190} assistant turn\n"));
    }

    #[test]
    fn error_block_with_no_parent_id_is_reported_as_orphan() {
        let payload = kaijutsu_types::ErrorPayload {
            category: kaijutsu_types::ErrorCategory::Rpc,
            severity: kaijutsu_types::ErrorSeverity::Fatal,
            code: None,
            detail: None,
            span: None,
            source_kind: None,
        };
        let error_block =
            BlockSnapshot::system_error(test_block_id(), payload, "rpc error: connection lost");

        let text = format_single_block(&error_block, None, &|_| None);
        assert!(text.starts_with("rpc error \u{2190} orphan (no parent recorded)\n"));
    }

    #[test]
    fn error_block_with_unresolvable_parent_is_reported_as_orphan_not_omitted() {
        let (_, _, error_block) = tool_error_chain();
        // No lookup match at all — the parent ToolResult isn't in this
        // document (e.g. excluded past the mirror's window, or a bug).
        let text = format_single_block(&error_block, None, &|_| None);
        assert!(text.starts_with("tool error \u{2190} orphan (parent block not found)\n"));
    }

    #[test]
    fn error_block_tool_provenance_orphan_when_tool_call_missing() {
        let (_, result, error_block) = tool_error_chain();
        // The ToolResult resolves, but its own parent (the ToolCall) does not.
        let siblings = vec![result];
        let lookup = |id: &BlockId| siblings.iter().find(|b| &b.id == id).cloned();

        let text = format_single_block(&error_block, None, &lookup);
        assert!(text.starts_with("tool error \u{2190} orphan (tool call not found)\n"));
    }
}
