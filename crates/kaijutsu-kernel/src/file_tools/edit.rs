//! EditEngine — string-replacement file editing through CRDT.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::tools::{ExecResult, ExecutionEngine};

use super::cache::FileDocumentCache;

/// Engine for editing files via exact string replacement.
pub struct EditEngine {
    cache: Arc<FileDocumentCache>,
}

impl EditEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache }
    }
}

#[derive(Deserialize)]
struct EditParams {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl ExecutionEngine for EditEngine {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by exact string replacement"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)",
                    "default": false
                }
            },
            "required": ["path", "old_string", "new_string"]
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: EditParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        if p.old_string == p.new_string {
            return Ok(ExecResult::failure(1, "old_string and new_string are identical"));
        }

        let (doc_id, block_id) = match self.cache.get_or_load(&p.path).await {
            Ok(ids) => ids,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let content = match self.cache.read_content(&p.path).await {
            Ok(c) => c,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        // Find all match positions
        let matches: Vec<usize> = content
            .match_indices(&p.old_string)
            .map(|(idx, _)| idx)
            .collect();

        if matches.is_empty() {
            return Ok(ExecResult::failure(
                1,
                format!(
                    "old_string not found in {}. Make sure it matches exactly.",
                    p.path
                ),
            ));
        }

        if !p.replace_all && matches.len() > 1 {
            return Ok(ExecResult::failure(
                1,
                format!(
                    "old_string found {} times in {}. Use replace_all: true or provide more context to make it unique.",
                    matches.len(),
                    p.path
                ),
            ));
        }

        let store = self.cache.block_store();

        // Process matches in reverse order so earlier offsets stay valid
        let mut replacements = 0;
        for &offset in matches.iter().rev() {
            if !p.replace_all && replacements > 0 {
                break;
            }
            if let Err(e) = store.edit_text(
                &doc_id,
                &block_id,
                offset,
                &p.new_string,
                p.old_string.len(),
            ) {
                return Ok(ExecResult::failure(1, e));
            }
            replacements += 1;
        }

        self.cache.mark_dirty(&p.path);

        // Build context around the first replacement for confirmation
        let updated = self.cache.read_content(&p.path).await.unwrap_or_default();
        let first_pos = updated.find(&p.new_string).unwrap_or(0);
        let context = extract_context(&updated, first_pos, p.new_string.len());

        Ok(ExecResult::success(format!(
            "Replaced {} occurrence{} in {}\n\n{}",
            replacements,
            if replacements == 1 { "" } else { "s" },
            p.path,
            context
        )))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Extract a few lines of context around a byte position.
fn extract_context(content: &str, pos: usize, match_len: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    // Find which line the position is on
    let mut byte_offset = 0;
    let mut target_line = 0;
    for (i, line) in lines.iter().enumerate() {
        let line_end = byte_offset + line.len() + 1; // +1 for newline
        if pos < line_end {
            target_line = i;
            break;
        }
        byte_offset = line_end;
    }

    // Show 2 lines before and after
    let start = target_line.saturating_sub(2);
    let match_end_line = {
        let mut bl = byte_offset;
        let mut el = target_line;
        for (i, line) in lines.iter().enumerate().skip(target_line) {
            bl += line.len() + 1;
            el = i;
            if bl > pos + match_len {
                break;
            }
        }
        el
    };
    let end = (match_end_line + 3).min(lines.len());
    let width = end.to_string().len().max(4);

    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}→ {}", start + i + 1, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}
