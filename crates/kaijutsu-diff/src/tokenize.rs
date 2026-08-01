//! Token sources fed to imara-diff.
//!
//! Lines come from imara-diff itself (`&str` tokenizes to lines by default, and
//! **the line token keeps its trailing newline** — that is load-bearing: it is
//! how a file that lost its final newline shows up as a change at all).
//!
//! Words we tokenize ourselves. Upstream `imara-diff` 0.2.0 does not ship a
//! word tokenizer — the `sources::words` helper referenced in survey notes
//! lives in gitoxide's *fork*, `gix-imara-diff`. Writing our own is a few lines
//! and buys control over what counts as a word, which is exactly the knob the
//! deferred profiles will want.

use imara_diff::TokenSource;

/// Normalize line terminators to `\n`.
///
/// CRLF is normalized **on ingest**, not at emission, so no `\r` ever reaches
/// a [`crate::DiffLine`] and formatting is unconditionally LF. The cost is that
/// a change consisting only of line terminators becomes invisible; callers get
/// [`crate::DiffError::LineEndingsOnly`] rather than an empty diff.
///
/// Returns a borrowed slice when there is nothing to do, so the common case
/// does not allocate.
pub fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// A [`TokenSource`] over the words of a string.
///
/// A word is one of:
///
/// - a run of alphanumeric characters and `_` (identifiers and numbers),
/// - a run of spaces,
/// - any other single character (punctuation, tabs, newlines).
///
/// Every byte of the input belongs to exactly one token and tokens are emitted
/// in order, which is what lets [`crate::refine`] recover byte offsets by
/// accumulating token lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Words<'a>(&'a str);

impl<'a> Words<'a> {
    /// Tokenize `data` into words.
    pub fn new(data: &'a str) -> Self {
        Self(data)
    }
}

impl<'a> Iterator for Words<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let first = self.0.chars().next()?;
        let len = if first == ' ' {
            self.0
                .char_indices()
                .find(|(_, c)| *c != ' ')
                .map_or(self.0.len(), |(i, _)| i)
        } else if first.is_alphanumeric() || first == '_' {
            self.0
                .char_indices()
                .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
                .map_or(self.0.len(), |(i, _)| i)
        } else {
            first.len_utf8()
        };
        let (word, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(word)
    }
}

impl<'a> TokenSource for Words<'a> {
    type Token = &'a str;
    type Tokenizer = Self;

    fn tokenize(&self) -> Self::Tokenizer {
        *self
    }

    fn estimate_tokens(&self) -> u32 {
        // Words average a bit over three bytes in code; over-reserving costs a
        // little memory, under-reserving costs a rehash.
        (self.0.len() / 3) as u32
    }
}

/// Split `text` into lines **without** terminators, plus whether the text ends
/// with a newline.
///
/// `""` is zero lines. `"a"` is one line without a final newline. `"a\n"` is
/// one line with one. `"a\n\n"` is two lines (the second empty) with one.
pub fn split_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), true);
    }
    match text.strip_suffix('\n') {
        Some(body) => (body.split('\n').collect(), true),
        None => (text.split('\n').collect(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_cover_every_byte_in_order() {
        let src = "let mut foo_bar = 2;\n";
        let toks: Vec<&str> = Words::new(src).collect();
        assert_eq!(toks.concat(), src);
        assert_eq!(
            toks,
            vec![
                "let", " ", "mut", " ", "foo_bar", " ", "=", " ", "2", ";", "\n"
            ]
        );
    }

    #[test]
    fn words_keeps_runs_of_spaces_together() {
        let toks: Vec<&str> = Words::new("a    b").collect();
        assert_eq!(toks, vec!["a", "    ", "b"]);
    }

    #[test]
    fn words_splits_on_char_boundaries_for_multibyte() {
        let toks: Vec<&str> = Words::new("こんにちは、世界").collect();
        // Kana/kanji are alphanumeric; the ideographic comma is not.
        assert_eq!(toks, vec!["こんにちは", "、", "世界"]);
    }

    #[test]
    fn normalize_newlines_is_borrow_when_clean() {
        assert!(matches!(
            normalize_newlines("a\nb\n"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(normalize_newlines("a\r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn split_lines_distinguishes_final_newline() {
        assert_eq!(split_lines(""), (vec![], true));
        assert_eq!(split_lines("a"), (vec!["a"], false));
        assert_eq!(split_lines("a\n"), (vec!["a"], true));
        assert_eq!(split_lines("a\n\n"), (vec!["a", ""], true));
    }
}
