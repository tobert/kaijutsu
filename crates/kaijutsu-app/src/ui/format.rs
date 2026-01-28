//! Display hint formatting for Bevy UI rendering.
//!
//! This module provides functions to format output based on display hints
//! for different audiences:
//! - **UI/Human**: Pretty tables, traditional trees, colors
//! - **Model/Agent**: Token-efficient compact formats
//!
//! # Usage
//!
//! ```ignore
//! use kaijutsu_app::ui::format::{format_for_display, format_for_model};
//!
//! let content = "file1.txt\nfile2.txt";
//! let hint_json = r#"{"type":"table","rows":[["file1.txt"],["file2.txt"]]}"#;
//!
//! // For UI display
//! let pretty = format_for_display(content, Some(hint_json));
//!
//! // For model consumption
//! let compact = format_for_model(content, Some(hint_json));
//! ```

use kaijutsu_kernel::tools::{DisplayHint, EntryType};

/// Formatted output with optional styling information.
#[derive(Debug, Clone)]
pub struct FormattedOutput {
    /// The formatted text content.
    pub text: String,
    /// Whether this output has special formatting applied.
    #[allow(dead_code)]
    pub is_formatted: bool,
}

impl FormattedOutput {
    /// Create plain output without special formatting.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_formatted: false,
        }
    }

    /// Create styled output with special formatting.
    pub fn styled(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_formatted: true,
        }
    }
}

/// Format output for display in the Bevy UI (human-friendly).
///
/// Parses the display hint JSON and renders appropriately:
/// - Tables → column-aligned output
/// - Trees → traditional box-drawing format
/// - Formatted → pre-rendered user format
/// - None → raw content
pub fn format_for_display(content: &str, hint_json: Option<&str>) -> FormattedOutput {
    let Some(json) = hint_json else {
        return FormattedOutput::plain(content);
    };

    let Ok(hint) = serde_json::from_str::<DisplayHint>(json) else {
        return FormattedOutput::plain(content);
    };

    match hint {
        DisplayHint::None => FormattedOutput::plain(content),

        DisplayHint::Formatted { user, .. } => FormattedOutput::styled(user),

        DisplayHint::Table { headers, rows, entry_types } => {
            let text = render_table(&headers, &rows, entry_types.as_deref());
            FormattedOutput::styled(text)
        }

        DisplayHint::Tree { traditional, .. } => FormattedOutput::styled(traditional),
    }
}

/// Format output for model/agent consumption (token-efficient).
///
/// Returns compact formats suitable for LLM context:
/// - Tables → newline-separated list (from raw content)
/// - Trees → compact brace notation
/// - Formatted → pre-rendered model format
/// - None → raw content
#[allow(dead_code)]
pub fn format_for_model(content: &str, hint_json: Option<&str>) -> String {
    let Some(json) = hint_json else {
        return content.to_string();
    };

    let Ok(hint) = serde_json::from_str::<DisplayHint>(json) else {
        return content.to_string();
    };

    match hint {
        DisplayHint::None => content.to_string(),

        DisplayHint::Formatted { model, .. } => model,

        // For tables, raw content is already the compact format (one item per line)
        DisplayHint::Table { .. } => content.to_string(),

        DisplayHint::Tree { compact, .. } => compact,
    }
}

/// Render a table with proper column alignment.
fn render_table(
    headers: &Option<Vec<String>>,
    rows: &[Vec<String>],
    entry_types: Option<&[EntryType]>,
) -> String {
    if rows.is_empty() {
        return String::new();
    }

    // Calculate column widths
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; num_cols];

    // Include headers in width calculation
    if let Some(hdrs) = headers {
        for (i, h) in hdrs.iter().enumerate() {
            if i < num_cols {
                col_widths[i] = col_widths[i].max(h.len());
            }
        }
    }

    // Include rows in width calculation (accounting for entry type suffixes)
    for (row_idx, row) in rows.iter().enumerate() {
        let entry_type = entry_types.and_then(|et| et.get(row_idx));
        for (i, cell) in row.iter().enumerate() {
            if i < num_cols {
                // First column gets a suffix for entry types
                let effective_len = if i == 0 && entry_type.is_some() {
                    cell.len() + 1 // +1 for the suffix character (/, *, @)
                } else {
                    cell.len()
                };
                col_widths[i] = col_widths[i].max(effective_len);
            }
        }
    }

    let mut output = String::new();

    // Render headers if present
    if let Some(hdrs) = headers {
        let header_line: Vec<String> = hdrs
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let width = col_widths.get(i).copied().unwrap_or(0);
                format!("{:width$}", h, width = width)
            })
            .collect();
        output.push_str(&header_line.join("  "));
        output.push('\n');

        // Separator line
        let sep: Vec<String> = col_widths.iter().map(|&w| "-".repeat(w)).collect();
        output.push_str(&sep.join("  "));
        output.push('\n');
    }

    // Render rows
    for (row_idx, row) in rows.iter().enumerate() {
        let entry_type = entry_types.and_then(|et| et.get(row_idx));

        let formatted_cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let width = col_widths.get(i).copied().unwrap_or(0);

                // Apply type-based suffix for first column, then pad to width
                if i == 0 {
                    let with_suffix = match entry_type {
                        Some(EntryType::Directory) => format!("{}/", cell),
                        Some(EntryType::Executable) => format!("{}*", cell),
                        Some(EntryType::Symlink) => format!("{}@", cell),
                        _ => cell.clone(),
                    };
                    format!("{:width$}", with_suffix, width = width)
                } else {
                    format!("{:width$}", cell, width = width)
                }
            })
            .collect();

        output.push_str(&formatted_cells.join("  "));
        if row_idx < rows.len() - 1 {
            output.push('\n');
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_plain() {
        let output = format_for_display("hello world", None);
        assert_eq!(output.text, "hello world");
        assert!(!output.is_formatted);
    }

    #[test]
    fn test_format_formatted_hint() {
        let hint = serde_json::to_string(&DisplayHint::Formatted {
            user: "Pretty Output".to_string(),
            model: "compact".to_string(),
        }).unwrap();

        let display = format_for_display("raw", Some(&hint));
        assert_eq!(display.text, "Pretty Output");
        assert!(display.is_formatted);

        let model = format_for_model("raw", Some(&hint));
        assert_eq!(model, "compact");
    }

    #[test]
    fn test_format_table() {
        let hint = serde_json::to_string(&DisplayHint::Table {
            headers: Some(vec!["Name".to_string(), "Size".to_string()]),
            rows: vec![
                vec!["foo.rs".to_string(), "1024".to_string()],
                vec!["bar.rs".to_string(), "2048".to_string()],
            ],
            entry_types: Some(vec![EntryType::File, EntryType::File]),
        }).unwrap();

        let display = format_for_display("foo.rs\nbar.rs", Some(&hint));
        assert!(display.is_formatted);
        assert!(display.text.contains("Name"));
        assert!(display.text.contains("foo.rs"));

        let model = format_for_model("foo.rs\nbar.rs", Some(&hint));
        assert_eq!(model, "foo.rs\nbar.rs");
    }

    #[test]
    fn test_format_tree() {
        let hint = serde_json::to_string(&DisplayHint::Tree {
            root: "project".to_string(),
            structure: serde_json::json!({}),
            traditional: "project/\n└── src/".to_string(),
            compact: "project/{src/}".to_string(),
        }).unwrap();

        let display = format_for_display("", Some(&hint));
        assert!(display.text.contains("└──"));

        let model = format_for_model("", Some(&hint));
        assert_eq!(model, "project/{src/}");
    }

    #[test]
    fn test_format_with_entry_types() {
        let hint = serde_json::to_string(&DisplayHint::Table {
            headers: None,
            rows: vec![
                vec!["src".to_string()],
                vec!["main.rs".to_string()],
                vec!["run.sh".to_string()],
            ],
            entry_types: Some(vec![
                EntryType::Directory,
                EntryType::File,
                EntryType::Executable,
            ]),
        }).unwrap();

        let display = format_for_display("src\nmain.rs\nrun.sh", Some(&hint));
        assert!(display.text.contains("src/"));  // Directory suffix
        assert!(display.text.contains("run.sh*")); // Executable suffix
    }
}
