//! Deterministic, non-model summary for a completed `Thinking` block.
//!
//! `summarize_thinking` is pure and synchronous: no embedder, no model call.
//! It exists so the kernel can store a durable one-line stand-in for a
//! thinking block's first real sentence, for display only — the summary
//! never enters model hydration. See `docs/issues.md`, "Thinking folds to a
//! summary line once the player has moved on".

/// A qualifying sentence must be at least this many characters, so a short
/// interjection ("Hmm." or "Okay,") is skipped in favor of the next real
/// sentence.
const MIN_SENTENCE_CHARS: usize = 20;

/// The summary is capped at this many characters (on a char boundary, not a
/// byte boundary — multi-byte text such as Japanese must not split a
/// character). An ellipsis is appended when the source is cut, so the
/// rendered result can run one character past this cap.
const MAX_SUMMARY_CHARS: usize = 120;

/// Summarize a `Thinking` block's text to one display line.
///
/// Rules, in order:
/// - `None` when `text` is empty or all whitespace.
/// - Each line has a leading markdown marker stripped (`#`, `-`, `*`, `>`,
///   or a numbered marker like `1.`), so a heading or list item doesn't leak
///   its punctuation into the summary.
/// - The result is the first sentence. A sentence ends at a newline
///   (always), or at `.`, `!`, or `?` when that mark is followed by
///   whitespace or the end of the text — so a period inside a path or
///   filename (`foo.rs`) is never mistaken for a sentence boundary — with
///   at least [`MIN_SENTENCE_CHARS`] characters.
/// - When no sentence qualifies (e.g. text with no ASCII sentence
///   punctuation, such as Japanese prose), the first non-empty line stands
///   in instead, so this only returns `None` for genuinely empty input.
/// - The chosen text is capped at [`MAX_SUMMARY_CHARS`] characters on a char
///   boundary, with `…` appended when it was cut.
pub fn summarize_thinking(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let stripped_lines: Vec<&str> = trimmed.lines().map(strip_markdown_marker).collect();
    let joined = stripped_lines.join("\n");

    let candidate = split_into_sentences(&joined)
        .into_iter()
        .map(str::trim)
        .find(|s| s.chars().count() >= MIN_SENTENCE_CHARS)
        .map(str::to_string)
        .or_else(|| {
            stripped_lines
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty())
                .map(str::to_string)
        })?;

    Some(cap_chars(&candidate, MAX_SUMMARY_CHARS))
}

/// Split `text` into rough sentence segments, the boundary mark dropped
/// (same shape as `str::split`, but the boundary rule is smarter than a
/// bare character class). A newline always ends a sentence. `.`, `!`, or
/// `?` end one only when followed by whitespace or the end of the text —
/// unlike `kaijutsu_index::synthesis::split_sentences`'s plain
/// `split(['.', '!', '?', '\n'])`, this does not cut `foo.rs` into `foo` and
/// `rs`.
fn split_into_sentences(text: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        let is_boundary = match ch {
            '\n' => true,
            '.' | '!' | '?' => chars
                .peek()
                .is_none_or(|&(_, next)| next.is_whitespace()),
            _ => false,
        };
        if is_boundary {
            segments.push(&text[start..idx]);
            start = idx + ch.len_utf8();
        }
    }
    segments.push(&text[start..]);
    segments
}

/// Strip one leading markdown marker from a line, if present: `#` (any
/// count), `- `, `* `, `> `, or a numbered marker (`1. `, `12. `, ...). The
/// marker must be followed by whitespace or end-of-line, so `*bold*` and
/// similar inline emphasis are left alone.
fn strip_markdown_marker(line: &str) -> &str {
    let s = line.trim_start();

    if let Some(rest) = s.strip_prefix('#') {
        let rest = rest.trim_start_matches('#');
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return rest.trim_start();
        }
        return s;
    }

    for marker in ["- ", "* ", "> "] {
        if let Some(rest) = s.strip_prefix(marker) {
            return rest.trim_start();
        }
    }

    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
    if digits_end > 0
        && let Some(rest) = s[digits_end..].strip_prefix(". ")
    {
        return rest.trim_start();
    }

    s
}

/// Truncate `s` to at most `max` characters, on a char boundary, appending
/// `…` when it was cut.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let capped: String = s.chars().take(max).collect();
    format!("{capped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_none() {
        assert_eq!(summarize_thinking(""), None);
        assert_eq!(summarize_thinking("   \n\t  "), None);
    }

    #[test]
    fn one_short_line_is_that_line() {
        assert_eq!(
            summarize_thinking("Thinking..."),
            Some("Thinking...".to_string())
        );
    }

    #[test]
    fn long_paragraph_picks_first_sentence() {
        let text = "This is the first real sentence of the thinking block. \
                     It goes on with a second sentence that would also qualify.";
        assert_eq!(
            summarize_thinking(text),
            Some("This is the first real sentence of the thinking block".to_string())
        );
    }

    #[test]
    fn leading_short_fragment_is_skipped() {
        let text = "Hmm. Let me think about this problem a little more carefully.";
        assert_eq!(
            summarize_thinking(text),
            Some("Let me think about this problem a little more carefully".to_string())
        );
    }

    #[test]
    fn caps_at_120_chars_with_ellipsis() {
        let sentence = "a".repeat(200);
        let text = format!("{sentence}.");
        let summary = summarize_thinking(&text).expect("long sentence summarizes");
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        assert_eq!(
            summary.chars().take(MAX_SUMMARY_CHARS).collect::<String>(),
            "a".repeat(MAX_SUMMARY_CHARS)
        );
    }

    #[test]
    fn markdown_markers_are_stripped() {
        assert_eq!(
            summarize_thinking("# Heading line that is definitely long enough to qualify"),
            Some("Heading line that is definitely long enough to qualify".to_string())
        );
        assert_eq!(
            summarize_thinking("- a bullet point long enough to clear the sentence bar"),
            Some("a bullet point long enough to clear the sentence bar".to_string())
        );
        assert_eq!(
            summarize_thinking("* another bullet long enough to clear the sentence bar"),
            Some("another bullet long enough to clear the sentence bar".to_string())
        );
        assert_eq!(
            summarize_thinking("> a quoted line long enough to clear the sentence bar"),
            Some("a quoted line long enough to clear the sentence bar".to_string())
        );
        assert_eq!(
            summarize_thinking("1. a numbered item long enough to clear the sentence bar"),
            Some("a numbered item long enough to clear the sentence bar".to_string())
        );
    }

    #[test]
    fn a_period_inside_a_filename_is_not_a_sentence_boundary() {
        let text = "Let me check foo.rs before touching bar.rs. Then the test.";
        assert_eq!(
            summarize_thinking(text),
            Some("Let me check foo.rs before touching bar.rs".to_string())
        );
    }

    #[test]
    fn japanese_paragraph_returns_first_line() {
        let text = "これは最初の行です\nこれは二行目です";
        assert_eq!(
            summarize_thinking(text),
            Some("これは最初の行です".to_string())
        );
    }
}
