//! ReadEngine — read file content with optional line numbers and windowing.

use std::sync::Arc;

use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};

use super::cache::{CacheReadError, FileDocumentCache};
use super::guard::WorkspaceGuard;
use super::path::resolve_str;

/// Default cap on lines returned when `limit` is omitted — bounds output so a
/// huge file can't flood the conversation (matches the harness Read default).
const DEFAULT_LINE_LIMIT: u32 = 2000;
/// Lines longer than this are truncated with a marker (in characters, so we
/// never split a UTF-8 boundary).
const MAX_LINE_CHARS: usize = 2000;

/// Engine for reading file content through the CRDT cache.
pub struct ReadEngine {
    cache: Arc<FileDocumentCache>,
    guard: Option<WorkspaceGuard>,
}

impl ReadEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache, guard: None }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[derive(Deserialize)]
struct ReadParams {
    path: String,
    offset: Option<u32>,
    limit: Option<u32>,
}

impl ReadEngine {
    pub fn description(&self) -> &str {
        "Read file content with optional line numbers and windowed ranges"
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.read")]
    pub async fn execute(
        &self,
        params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let p: ReadParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let path = match resolve_str(&ctx.cwd, &p.path) {
            Ok(s) => s,
            Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
        };

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_read(ctx, &path)
        {
            return Ok(denied);
        }

        // NotCached (binary or missing) → explicit not-found message so the
        // model gets a clear signal rather than a generic error string.
        // Backend → real error; surface it so wrong content is never returned.
        match self.cache.try_read_content(&path).await {
            Ok(content) => Ok(ExecResult::success(render(&content, &path, p.offset, p.limit))),
            Err(CacheReadError::NotCached) => Ok(ExecResult::failure(
                1,
                format!("{}: not found or not a text file", path),
            )),
            Err(CacheReadError::Backend(e)) => Ok(ExecResult::failure(1, e)),
        }
    }
}

/// Render file content as 1-indexed `cat -n`-style lines, windowed by `offset`
/// (1-indexed start line) and `limit` (defaulting to [`DEFAULT_LINE_LIMIT`]),
/// with over-long lines truncated and a footer when the window is partial.
fn render(content: &str, path: &str, offset: Option<u32>, limit: Option<u32>) -> String {
    if content.is_empty() {
        return format!("(empty file: {})", path);
    }

    let total: usize = content.lines().count();
    // `offset` is the 1-indexed start line (matches the numbers we print and
    // grep's output). Omitted → start at line 1.
    let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0) as usize;
    let limit = limit.unwrap_or(DEFAULT_LINE_LIMIT) as usize;
    let end = start.saturating_add(limit).min(total);

    if start >= total {
        return format!(
            "(offset {} is past end of file: {} has {} line{})",
            start + 1,
            path,
            total,
            if total == 1 { "" } else { "s" },
        );
    }

    let width = total.to_string().len().max(4);
    let mut out: Vec<String> = content
        .lines()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(i, line)| format!("{:>width$}→ {}", i + 1, truncate_line(line), width = width))
        .collect();

    // Footer only when the window doesn't cover the whole file, so the model
    // knows there's more rather than assuming it saw everything.
    if start > 0 || end < total {
        out.push(format!(
            "\n({} lines total; showing {}-{})",
            total,
            start + 1,
            end
        ));
    }

    out.join("\n")
}

/// Truncate a single line to [`MAX_LINE_CHARS`] characters, appending a marker
/// noting how many characters were elided.
fn truncate_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.chars().count() <= MAX_LINE_CHARS {
        return std::borrow::Cow::Borrowed(line);
    }
    let kept: String = line.chars().take(MAX_LINE_CHARS).collect();
    let elided = line.chars().count() - MAX_LINE_CHARS;
    std::borrow::Cow::Owned(format!("{}… (+{} chars truncated)", kept, elided))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_notes_emptiness() {
        assert!(render("", "/x.rs", None, None).contains("empty file"));
    }

    #[test]
    fn offset_is_one_indexed() {
        let content = "a\nb\nc\nd\n";
        // offset=2 should start at line 2 ("b"), not line 3.
        let out = render(content, "/x", Some(2), Some(1));
        assert!(out.contains("2→ b"), "got: {out}");
        assert!(!out.contains("3→ c"));
    }

    #[test]
    fn default_limit_bounds_output_and_adds_footer() {
        let content: String = (1..=3000).map(|n| format!("line{n}\n")).collect();
        let out = render(&content, "/big", None, None);
        let shown = out.lines().filter(|l| l.contains("→ line")).count();
        assert_eq!(shown, DEFAULT_LINE_LIMIT as usize);
        assert!(out.contains("3000 lines total; showing 1-2000"), "got footer: {out}");
    }

    #[test]
    fn full_short_file_has_no_footer() {
        let out = render("one\ntwo\n", "/x", None, None);
        assert!(!out.contains("lines total"));
        assert!(out.contains("1→ one"));
        assert!(out.contains("2→ two"));
    }

    #[test]
    fn long_lines_are_truncated() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let content = format!("{long}\nshort\n");
        let out = render(&content, "/x", None, None);
        assert!(out.contains("(+50 chars truncated)"), "got: {out}");
        assert!(out.contains("2→ short"));
    }

    #[test]
    fn offset_past_end_is_explicit() {
        let out = render("a\nb\n", "/x", Some(99), None);
        assert!(out.contains("past end of file"), "got: {out}");
    }
}
