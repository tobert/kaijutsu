//! EditEngine — file editing through CRDT, two addressing modes:
//!
//! * **String mode** (`old_string`/`new_string`): exact substring replacement.
//! * **Hashline mode** (`anchor`/`new_string`): replace a line or range named by
//!   the `N:hash` anchors that `read` prints, verifying the hash still matches
//!   before writing (stale → fail loud, never splice).
//!
//! The CRDT text primitive ([`BlockStore::edit_text`]) is **character**-indexed.
//! `str::match_indices` and `str::len` are **byte**-valued. Conflating the two
//! silently corrupts any file containing multibyte UTF-8 before the edit site —
//! so the planner converts byte offsets to char offsets explicitly, and every
//! edit is read back and compared against the content we *intended* to write
//! (fail loud on any mismatch).

use std::sync::Arc;

use serde::Deserialize;

use crate::execution::{ExecContext, ExecResult};

use super::cache::{CacheReadError, FileDocumentCache};
use super::guard::WorkspaceGuard;
use super::hashline::line_hash;
use super::path::{deny_etc_write, is_rc_path, rc_write_denied, resolve_str};

/// Engine for editing files via exact string replacement or hashline anchors.
pub struct EditEngine {
    cache: Arc<FileDocumentCache>,
    guard: Option<WorkspaceGuard>,
}

impl EditEngine {
    pub fn new(cache: Arc<FileDocumentCache>) -> Self {
        Self { cache, guard: None }
    }

    pub fn with_guard(mut self, guard: WorkspaceGuard) -> Self {
        self.guard = Some(guard);
        self
    }
}

#[derive(Deserialize)]
struct EditParams {
    path: String,
    /// String mode: exact substring to replace. Mutually exclusive with `anchor`.
    #[serde(default)]
    old_string: Option<String>,
    /// Replacement text (both modes). In hashline mode this is the full new line
    /// content for the anchored range (empty deletes the line(s)).
    new_string: String,
    /// String mode: replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    replace_all: bool,
    /// Hashline mode: `N:hash` (one line) or `N:hash..M:hash` (inclusive range),
    /// using the anchors printed by `read`. Mutually exclusive with `old_string`.
    #[serde(default)]
    anchor: Option<String>,
}

// ── Pure planning core (no I/O — unit-tested directly) ──────────────────────

/// One character-indexed replacement to apply to the block's text.
#[derive(Debug, PartialEq, Eq)]
struct ReplaceOp {
    /// Character (NOT byte) offset where deletion begins.
    char_offset: usize,
    /// Number of characters to delete.
    char_delete: usize,
    /// Text inserted at `char_offset` after deletion.
    insert: String,
}

/// A fully resolved edit: the ops to apply (ascending by offset) plus the exact
/// content the file must hold afterward. `expected` is derived independently of
/// the char-offset arithmetic, so reading the file back and comparing to it
/// catches any indexing bug in the apply path.
#[derive(Debug)]
struct EditPlan {
    ops: Vec<ReplaceOp>,
    expected: String,
    /// Count for the human-facing success message.
    replacements: usize,
}

/// Char offset of byte position `byte` within `s` (number of chars before it).
fn byte_to_char(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// Plan a string-mode (exact substring) edit. Errors are caller-facing messages.
fn plan_string_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditPlan, String> {
    if old.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    let byte_offsets: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();

    if byte_offsets.is_empty() {
        return Err("old_string not found. Make sure it matches exactly (whitespace \
             included), or use hashline `anchor` addressing instead."
            .to_string());
    }
    if !replace_all && byte_offsets.len() > 1 {
        return Err(format!(
            "old_string found {} times. Pass replace_all: true, add surrounding \
             context to make it unique, or address one line with `anchor`.",
            byte_offsets.len()
        ));
    }

    let char_delete = old.chars().count();
    let take = if replace_all { byte_offsets.len() } else { 1 };
    let ops = byte_offsets
        .iter()
        .take(take)
        .map(|&b| ReplaceOp {
            char_offset: byte_to_char(content, b),
            char_delete,
            insert: new.to_string(),
        })
        .collect::<Vec<_>>();

    // Computed via std replace — independent of the char-offset math above, so
    // the post-write read-back comparison is a real check, not a tautology.
    let expected = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };

    let replacements = ops.len();
    Ok(EditPlan {
        ops,
        expected,
        replacements,
    })
}

/// A parsed `N:hash` anchor endpoint (1-indexed line number).
struct Endpoint {
    line: usize,
    hash: String,
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    let (num, hash) = s.split_once(':').ok_or_else(|| {
        format!("anchor endpoint `{s}` must be `LINE:hash` (e.g. `42:a3f1`)")
    })?;
    let line: usize = num
        .trim()
        .parse()
        .map_err(|_| format!("anchor line `{num}` is not a number"))?;
    if line == 0 {
        return Err("anchor line numbers are 1-indexed (got 0)".to_string());
    }
    if hash.is_empty() {
        return Err(format!("anchor endpoint `{s}` is missing its hash"));
    }
    Ok(Endpoint {
        line,
        hash: hash.trim().to_ascii_lowercase(),
    })
}

/// Render the current `N:hash→ content` lines over `[start, end]` (1-indexed,
/// inclusive, clamped) so a stale-anchor error tells the model the truth.
fn annotate_region(lines: &[&str], start: usize, end: usize) -> String {
    let lo = start.saturating_sub(1);
    let hi = end.min(lines.len());
    lines[lo..hi]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}:{}→ {}", lo + i + 1, line_hash(l), l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plan a hashline-mode edit: verify the anchor hashes still match, then replace
/// the line range with `new` (empty deletes the lines, terminators and all).
fn plan_anchor_edit(content: &str, anchor: &str, new: &str) -> Result<EditPlan, String> {
    let (start, end) = match anchor.split_once("..") {
        Some((a, b)) => (parse_endpoint(a)?, parse_endpoint(b)?),
        None => {
            let e = parse_endpoint(anchor)?;
            let line = e.line;
            let hash = e.hash.clone();
            (e, Endpoint { line, hash })
        }
    };
    if start.line > end.line {
        return Err(format!(
            "anchor range start line {} is after end line {}",
            start.line, end.line
        ));
    }

    let lines: Vec<&str> = content.lines().collect();

    // Range validation first — also rejects an empty file (0 lines) cleanly,
    // before we build the piece view whose count would otherwise trip the
    // lockstep debug_assert below on `""` (lines()==0, split_inclusive==1).
    if end.line > lines.len() {
        return Err(format!(
            "anchor line {} is past end of file ({} line{})",
            end.line,
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        ));
    }

    // Staleness check: re-hash the live lines the anchor names.
    for ep in [&start, &end] {
        let actual = line_hash(lines[ep.line - 1]);
        if actual != ep.hash {
            return Err(format!(
                "anchor stale: line {} is now `{}`, not `{}`. The file changed \
                 since you read it — re-read and retry. Current lines:\n{}",
                ep.line,
                actual,
                ep.hash,
                annotate_region(&lines, start.line, end.line)
            ));
        }
    }

    // `lines()` (terminator-stripped, used for hashing) and
    // `split_inclusive('\n')` (terminator-keeping, used for the byte/char span)
    // yield the same count for non-empty content, so a line index addresses both
    // views in lockstep.
    let pieces: Vec<&str> = content.split_inclusive('\n').collect();
    debug_assert_eq!(lines.len(), pieces.len());

    let prefix: String = pieces[..start.line - 1].concat();
    let body: String = pieces[start.line - 1..end.line].concat();
    let suffix: String = pieces[end.line..].concat();

    // Preserve the body's existing terminator (`\r\n`, `\n`, or none for a final
    // line without one) on replace; consume it on delete so whole lines vanish.
    let terminator = if body.ends_with("\r\n") {
        "\r\n"
    } else if body.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let insert = if new.is_empty() {
        String::new()
    } else {
        format!("{new}{terminator}")
    };

    let char_offset = prefix.chars().count();
    let char_delete = body.chars().count();

    // Rebuilt by pure string concat — independent of the char-offset arithmetic
    // the CRDT ops use, so the post-write read-back genuinely checks those
    // offsets rather than restating them.
    let expected = format!("{prefix}{insert}{suffix}");

    Ok(EditPlan {
        ops: vec![ReplaceOp {
            char_offset,
            char_delete,
            insert,
        }],
        expected,
        replacements: 1,
    })
}

// ── Engine (I/O shell) ──────────────────────────────────────────────────────

impl EditEngine {
    pub fn description(&self) -> &str {
        "Edit a file. String mode: exact `old_string`→`new_string` substring \
         replacement (whitespace-exact; set replace_all for many). Hashline mode: \
         pass `anchor` (`N:hash` or `N:hash..M:hash`, the anchors `read` prints) \
         to replace a line/range by reference — the hash is reverified before \
         writing, so a stale edit fails loud instead of corrupting. In hashline \
         mode `new_string` is the full new line content (empty deletes)."
    }

    #[tracing::instrument(skip(self, params, ctx), name = "engine.edit")]
    pub async fn execute(&self, params: &str, ctx: &ExecContext) -> anyhow::Result<ExecResult> {
        let p: EditParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let path = match resolve_str(&ctx.cwd, &p.path) {
            Ok(s) => s,
            Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
        };

        // See write.rs: rc tree needs the rc-write capability; rest of /etc
        // is denied flat.
        if is_rc_path(&path) {
            if !self
                .guard
                .as_ref()
                .is_some_and(|g| g.context_allows_rc_write(ctx))
            {
                return Ok(rc_write_denied(&path));
            }
        } else if let Some(denied) = deny_etc_write(&path) {
            return Ok(denied);
        }

        if let Some(ref guard) = self.guard
            && let Err(denied) = guard.check_write(ctx, &path)
        {
            return Ok(denied);
        }

        // Resolve addressing mode before touching the file.
        match (&p.anchor, &p.old_string) {
            (Some(_), Some(_)) => {
                return Ok(ExecResult::failure(
                    1,
                    "provide either `anchor` (hashline) or `old_string` (string mode), not both",
                ));
            }
            (None, None) => {
                return Ok(ExecResult::failure(
                    1,
                    "edit needs `old_string` (string mode) or `anchor` (hashline mode)",
                ));
            }
            (None, Some(old)) if *old == p.new_string => {
                return Ok(ExecResult::failure(
                    1,
                    "old_string and new_string are identical",
                ));
            }
            _ => {}
        }

        let (ctx_id, block_id) = match self.cache.get_or_load(&path).await {
            Ok(ids) => ids,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let content = match self.cache.read_content(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let plan = if let Some(anchor) = &p.anchor {
            plan_anchor_edit(&content, anchor, &p.new_string)
        } else {
            let old = p.old_string.as_deref().unwrap_or_default();
            plan_string_edit(&content, old, &p.new_string, p.replace_all)
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(msg) => return Ok(ExecResult::failure(1, format!("{}: {}", path, msg))),
        };

        let store = self.cache.block_store();
        // Apply highest offset first so earlier (char) offsets stay valid.
        for op in plan.ops.iter().rev() {
            if let Err(e) =
                store.edit_text(ctx_id, &block_id, op.char_offset, &op.insert, op.char_delete)
            {
                return Ok(ExecResult::failure(1, e.to_string()));
            }
        }

        self.cache.mark_dirty(&path);
        // Write-through so the edit lands on disk for external tools (cargo,
        // git) rather than sitting dirty in the cache.
        if let Err(e) = self.cache.flush_one(&path).await {
            return Ok(ExecResult::failure(
                1,
                format!("edited CRDT but failed to flush {}: {}", path, e),
            ));
        }

        // Fail-loud verification: read the file back and confirm it holds exactly
        // what we planned. A mismatch means the apply path mangled offsets — far
        // better to surface it than report a false success over corrupted bytes.
        // Note: this read-back is from the CRDT cache (the tool's source of
        // truth), so it verifies the edit consolidated correctly; a faulty VFS
        // flush is caught separately by `flush_one`'s error above, not here.
        let updated = match self.cache.try_read_content(&path).await {
            Ok(c) => c,
            Err(CacheReadError::Backend(e)) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("edit applied but post-write read failed for {}: {}", path, e),
                ));
            }
            Err(CacheReadError::NotCached) => {
                return Ok(ExecResult::failure(
                    1,
                    format!(
                        "edit applied but {} could not be read back to verify it",
                        path
                    ),
                ));
            }
        };
        if updated != plan.expected {
            return Ok(ExecResult::failure(
                1,
                format!(
                    "edit verification FAILED for {}: the file does not match the \
                     requested change (the edit was misapplied). Re-read the file \
                     before further edits.",
                    path
                ),
            ));
        }

        // Point the preview at the edit site by char offset (not str::find,
        // which would jump to an earlier identical occurrence of the new text).
        // An offset at EOF (end-of-file deletion) maps to the file's end.
        let first_byte = plan
            .ops
            .first()
            .map(|op| {
                updated
                    .char_indices()
                    .nth(op.char_offset)
                    .map(|(b, _)| b)
                    .unwrap_or(updated.len())
            })
            .unwrap_or(0);
        let match_len = plan.ops.first().map(|op| op.insert.len()).unwrap_or(0);
        let context = extract_context(&updated, first_byte, match_len);

        Ok(ExecResult::success(format!(
            "Replaced {} occurrence{} in {}\n\n{}",
            plan.replacements,
            if plan.replacements == 1 { "" } else { "s" },
            path,
            context
        )))
    }
}

/// Extract a few lines of context around a byte position, for the success
/// message. Walks true piece byte-lengths so the line mapping is correct for
/// CRLF files and a final line without a terminator.
fn extract_context(content: &str, pos: usize, match_len: usize) -> String {
    let pieces: Vec<&str> = content.split_inclusive('\n').collect();
    if pieces.is_empty() {
        return String::new();
    }

    // Byte position → 0-indexed line, clamped to the last line at/after EOF.
    let line_of = |byte: usize| -> usize {
        let mut acc = 0;
        for (i, piece) in pieces.iter().enumerate() {
            acc += piece.len();
            if byte < acc {
                return i;
            }
        }
        pieces.len() - 1
    };

    let first = line_of(pos);
    let last = line_of(pos + match_len);
    let start = first.saturating_sub(2);
    let end = (last + 3).min(pieces.len());
    let width = end.to_string().len().max(4);

    pieces[start..end]
        .iter()
        .enumerate()
        .map(|(i, piece)| {
            let line = piece.strip_suffix('\n').unwrap_or(piece);
            let line = line.strip_suffix('\r').unwrap_or(line);
            format!("{:>width$}→ {}", start + i + 1, line, width = width)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── String mode ─────────────────────────────────────────────────────────

    #[test]
    fn string_unique_match_plans_one_op() {
        let plan = plan_string_edit("foo bar baz", "bar", "QUX", false).unwrap();
        assert_eq!(plan.replacements, 1);
        assert_eq!(plan.expected, "foo QUX baz");
        assert_eq!(
            plan.ops,
            vec![ReplaceOp {
                char_offset: 4,
                char_delete: 3,
                insert: "QUX".to_string(),
            }]
        );
    }

    #[test]
    fn string_no_match_is_an_error() {
        let err = plan_string_edit("hello", "xyz", "Z", false).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn string_ambiguous_match_refused_without_replace_all() {
        let err = plan_string_edit("a a a", "a", "b", false).unwrap_err();
        assert!(err.contains("found 3 times"), "got: {err}");
    }

    #[test]
    fn string_replace_all_replaces_every_occurrence() {
        let plan = plan_string_edit("a a a", "a", "b", true).unwrap();
        assert_eq!(plan.replacements, 3);
        assert_eq!(plan.expected, "b b b");
    }

    #[test]
    fn string_empty_old_is_rejected() {
        let err = plan_string_edit("anything", "", "x", false).unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    /// The bug that corrupted docs/issues.md: byte offsets fed to a char-indexed
    /// CRDT. With multibyte content before the match, byte offset != char offset
    /// and byte length != char length. The plan must use *char* coordinates.
    #[test]
    fn string_multibyte_uses_char_offsets_not_byte_offsets() {
        // "α=1\n改善\ntarget\n": 'α' is 2 bytes, each kanji is 3 bytes, so the
        // byte offset of "target" is far past its char offset.
        let content = "α=1\n改善\ntarget\n";
        let byte_off = content.find("target").unwrap();
        assert!(byte_off > byte_to_char(content, byte_off), "fixture must be multibyte");

        let plan = plan_string_edit(content, "target", "TASK", false).unwrap();
        let op = &plan.ops[0];
        // Char offset, not byte offset.
        assert_eq!(op.char_offset, byte_to_char(content, byte_off));
        assert!(op.char_offset < byte_off, "must be the smaller char index");
        assert_eq!(op.char_delete, 6); // "target" is 6 chars
        assert_eq!(plan.expected, "α=1\n改善\nTASK\n");
    }

    #[test]
    fn string_multibyte_within_old_string_counts_chars() {
        let plan = plan_string_edit("x 改善 y", "改善", "kaizen", false).unwrap();
        let op = &plan.ops[0];
        assert_eq!(op.char_offset, 2); // "x " is 2 chars
        assert_eq!(op.char_delete, 2); // 改善 is 2 chars, not 6 bytes
        assert_eq!(plan.expected, "x kaizen y");
    }

    // ── Hashline mode ────────────────────────────────────────────────────────

    fn anchor_for(content: &str, line_1indexed: usize) -> String {
        let l = content.lines().nth(line_1indexed - 1).unwrap();
        format!("{}:{}", line_1indexed, line_hash(l))
    }

    #[test]
    fn anchor_single_line_replace() {
        let content = "one\ntwo\nthree\n";
        let plan = plan_anchor_edit(&content, &anchor_for(content, 2), "TWO").unwrap();
        assert_eq!(plan.expected, "one\nTWO\nthree\n");
        assert_eq!(plan.replacements, 1);
    }

    #[test]
    fn anchor_range_replace_collapses_lines() {
        let content = "a\nb\nc\nd\n";
        let anchor = format!("{}..{}", anchor_for(content, 2), {
            let l = content.lines().nth(2).unwrap();
            format!("3:{}", line_hash(l))
        });
        let plan = plan_anchor_edit(&content, &anchor, "X\nY").unwrap();
        assert_eq!(plan.expected, "a\nX\nY\nd\n");
    }

    #[test]
    fn anchor_empty_new_deletes_the_line() {
        let content = "keep\ndrop\nkeep2\n";
        let plan = plan_anchor_edit(&content, &anchor_for(content, 2), "").unwrap();
        assert_eq!(plan.expected, "keep\nkeep2\n");
    }

    #[test]
    fn anchor_last_line_without_trailing_newline() {
        let content = "a\nb\nc"; // no final newline
        let plan = plan_anchor_edit(&content, &anchor_for(content, 3), "C").unwrap();
        assert_eq!(plan.expected, "a\nb\nC");
    }

    #[test]
    fn anchor_multibyte_line_replace() {
        let content = "α\n改善\nz\n";
        let plan = plan_anchor_edit(&content, &anchor_for(content, 2), "kaizen").unwrap();
        assert_eq!(plan.expected, "α\nkaizen\nz\n");
    }

    #[test]
    fn anchor_stale_hash_fails_loud() {
        let content = "one\ntwo\nthree\n";
        // Anchor line 2 but with a wrong hash → staleness.
        let stale = "2:dead";
        let err = plan_anchor_edit(&content, stale, "X").unwrap_err();
        assert!(err.contains("stale"), "got: {err}");
        // The error shows the current truth so the model can retry.
        assert!(err.contains(&line_hash("two")), "should show current hash: {err}");
    }

    #[test]
    fn anchor_crlf_preserves_line_endings() {
        // The hash is over the CR-stripped line, but the replacement must keep
        // the file's \r\n terminator rather than silently converting to \n.
        let content = "a\r\nb\r\nc\r\n";
        let plan = plan_anchor_edit(&content, &anchor_for(content, 2), "B").unwrap();
        assert_eq!(plan.expected, "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn anchor_crlf_delete_consumes_full_terminator() {
        let content = "a\r\nb\r\nc\r\n";
        let plan = plan_anchor_edit(&content, &anchor_for(content, 2), "").unwrap();
        assert_eq!(plan.expected, "a\r\nc\r\n");
    }

    #[test]
    fn anchor_empty_file_errors_without_panic() {
        // Regression: the lockstep debug_assert (lines()==0 vs split_inclusive==1)
        // used to panic in debug before this clean error.
        let err = plan_anchor_edit("", "1:abcd", "x").unwrap_err();
        assert!(err.contains("past end"), "got: {err}");
    }

    #[test]
    fn anchor_out_of_range_is_an_error() {
        let content = "a\nb\n";
        let err = plan_anchor_edit(&content, "9:abcd", "x").unwrap_err();
        assert!(err.contains("past end"), "got: {err}");
    }

    #[test]
    fn anchor_malformed_is_an_error() {
        let content = "a\nb\n";
        assert!(plan_anchor_edit(&content, "nope", "x").is_err());
        assert!(plan_anchor_edit(&content, "0:abcd", "x").is_err());
        assert!(plan_anchor_edit(&content, "2:", "x").is_err());
    }

    #[test]
    fn anchor_reversed_range_is_an_error() {
        let content = "a\nb\nc\n";
        let anchor = format!("{}..{}", anchor_for(content, 3), {
            let l = content.lines().next().unwrap();
            format!("1:{}", line_hash(l))
        });
        let err = plan_anchor_edit(&content, &anchor, "x").unwrap_err();
        assert!(err.contains("after end"), "got: {err}");
    }
}
