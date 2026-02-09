//! Markdown → rich-text span conversion for cosmic-text rendering.
//!
//! Uses pulldown-cmark (the same parser as rustdoc) to convert markdown into
//! styled spans that cosmic-text can render via `set_rich_text()`.
//!
//! ```text
//! "**bold** and *italic*"
//!     ↓ pulldown-cmark events
//! [RichSpan { bold: true, text: "bold" },
//!  RichSpan { text: " and " },
//!  RichSpan { italic: true, text: "italic" }]
//!     ↓ to_cosmic_spans
//! [("bold", Attrs::weight(BOLD)),
//!  (" and ", Attrs::new()),
//!  ("italic", Attrs::style(Italic))]
//! ```

use cosmic_text::{Attrs, Family, Style, Weight};
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

/// Theme colors for markdown rendering (cosmic-text Color = packed u32).
#[derive(Clone, Debug)]
pub struct MarkdownColors {
    /// Heading text color (bright accent).
    pub heading: cosmic_text::Color,
    /// Inline `code` color.
    pub code: cosmic_text::Color,
    /// Bold/strong emphasis color (None = inherit base color).
    pub strong: Option<cosmic_text::Color>,
    /// Fenced code block color.
    pub code_block: cosmic_text::Color,
}

impl Default for MarkdownColors {
    fn default() -> Self {
        Self {
            heading: cosmic_text::Color::rgb(0xBB, 0x9A, 0xF7),  // Purple accent
            code: cosmic_text::Color::rgb(0x9E, 0xCE, 0x6A),      // Green
            strong: None,                                           // Inherit
            code_block: cosmic_text::Color::rgb(0x7A, 0xA2, 0xF7), // Blue
        }
    }
}

/// A styled text span parsed from markdown.
#[derive(Clone, Debug, PartialEq)]
pub struct RichSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub heading_level: Option<u8>,
    pub code_block: bool,
}

impl RichSpan {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            italic: false,
            code: false,
            heading_level: None,
            code_block: false,
        }
    }
}

/// Parse markdown text into a sequence of styled spans.
///
/// Walks pulldown-cmark events, tracking a style stack for nesting:
/// - `**bold**` → RichSpan { bold: true }
/// - `*italic*` → RichSpan { italic: true }
/// - `` `code` `` → RichSpan { code: true }
/// - `# Heading` → RichSpan { heading_level: Some(1), bold: true }
/// - Fenced code blocks → RichSpan { code_block: true }
pub fn parse_to_rich_spans(text: &str) -> Vec<RichSpan> {
    let parser = Parser::new(text);
    let mut spans = Vec::new();

    // Style stack: depth counters for nested formatting
    let mut bold_depth: u32 = 0;
    let mut italic_depth: u32 = 0;
    let mut heading_level: Option<u8> = None;
    let mut code_block = false;
    let mut list_depth: u32 = 0;
    let mut item_index: Vec<Option<u64>> = Vec::new(); // None = unordered, Some(n) = ordered
    let mut need_item_prefix = false;

    for event in parser {
        match event {
            // ── Block-level tags ──
            Event::Start(Tag::Heading { level, .. }) => {
                heading_level = Some(heading_level_to_u8(level));
                bold_depth += 1; // Headings are bold
            }
            Event::End(TagEnd::Heading(_)) => {
                heading_level = None;
                bold_depth = bold_depth.saturating_sub(1);
                push_span(&mut spans, "\n");
            }

            Event::Start(Tag::Paragraph) => {
                // Add blank line before paragraphs (unless at start)
                if !spans.is_empty() && !ends_with_newlines(&spans, 2) {
                    push_span(&mut spans, "\n");
                }
            }
            Event::End(TagEnd::Paragraph) => {
                push_span(&mut spans, "\n");
            }

            Event::Start(Tag::CodeBlock(_)) => {
                code_block = true;
                if !spans.is_empty() {
                    push_span(&mut spans, "\n");
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                code_block = false;
            }

            Event::Start(Tag::List(first_item)) => {
                list_depth += 1;
                item_index.push(first_item);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                item_index.pop();
            }

            Event::Start(Tag::Item) => {
                need_item_prefix = true;
            }
            Event::End(TagEnd::Item) => {
                // Ensure newline after list item
                if !ends_with_newline(&spans) {
                    push_span(&mut spans, "\n");
                }
                // Increment ordered list counter
                if let Some(Some(n)) = item_index.last_mut() {
                    *n += 1;
                }
            }

            Event::Start(Tag::BlockQuote(_)) => {
                // Simple: just treat as indented text
            }
            Event::End(TagEnd::BlockQuote(_)) => {}

            // ── Inline tags ──
            Event::Start(Tag::Strong) => bold_depth += 1,
            Event::End(TagEnd::Strong) => bold_depth = bold_depth.saturating_sub(1),

            Event::Start(Tag::Emphasis) => italic_depth += 1,
            Event::End(TagEnd::Emphasis) => italic_depth = italic_depth.saturating_sub(1),

            Event::Start(Tag::Strikethrough) => {} // No strikethrough in cosmic-text
            Event::End(TagEnd::Strikethrough) => {}

            Event::Start(Tag::Link { .. }) | Event::Start(Tag::Image { .. }) => {
                // Just render the link text, skip URL
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {}

            // ── Text content ──
            Event::Text(cow) => {
                let content = cow.as_ref();

                // Emit list item prefix before first text in an item
                if need_item_prefix {
                    let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                    let prefix = match item_index.last() {
                        Some(Some(n)) => format!("{indent}{n}. "),
                        _ => format!("{indent}• "),
                    };
                    push_span(&mut spans, &prefix);
                    need_item_prefix = false;
                }

                let mut span = RichSpan::new(content);
                span.bold = bold_depth > 0;
                span.italic = italic_depth > 0;
                span.heading_level = heading_level;
                span.code_block = code_block;
                spans.push(span);
            }

            Event::Code(cow) => {
                // Inline code
                if need_item_prefix {
                    let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                    let prefix = match item_index.last() {
                        Some(Some(n)) => format!("{indent}{n}. "),
                        _ => format!("{indent}• "),
                    };
                    push_span(&mut spans, &prefix);
                    need_item_prefix = false;
                }

                let mut span = RichSpan::new(cow.as_ref());
                span.code = true;
                span.bold = bold_depth > 0;
                span.italic = italic_depth > 0;
                spans.push(span);
            }

            Event::SoftBreak => push_span(&mut spans, "\n"),
            Event::HardBreak => push_span(&mut spans, "\n"),
            Event::Rule => push_span(&mut spans, "────────────────────\n"),

            // HTML passthrough, footnotes, etc. — render as plain text
            Event::Html(cow) | Event::InlineHtml(cow) => {
                push_span(&mut spans, cow.as_ref());
            }

            _ => {}
        }
    }

    spans
}

/// Convert RichSpans to cosmic-text `(text, Attrs)` pairs for `set_rich_text()`.
pub fn to_cosmic_spans<'a>(
    spans: &'a [RichSpan],
    base_attrs: &Attrs<'a>,
    colors: &MarkdownColors,
) -> Vec<(&'a str, Attrs<'a>)> {
    spans
        .iter()
        .map(|span| {
            let mut attrs = base_attrs.clone();

            if span.bold {
                attrs = attrs.weight(Weight::BOLD);
            }
            if span.italic {
                attrs = attrs.style(Style::Italic);
            }
            if span.code {
                attrs = attrs.family(Family::Monospace).color(colors.code);
            }
            if span.code_block {
                attrs = attrs.family(Family::Monospace).color(colors.code_block);
            }
            if span.heading_level.is_some() {
                attrs = attrs.weight(Weight::BOLD).color(colors.heading);
            }
            if span.bold && !span.code && span.heading_level.is_none() {
                if let Some(color) = colors.strong {
                    attrs = attrs.color(color);
                }
            }

            (span.text.as_str(), attrs)
        })
        .collect()
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Push a plain text span (no formatting).
fn push_span(spans: &mut Vec<RichSpan>, text: &str) {
    spans.push(RichSpan::new(text));
}

/// Check if the last span ends with a newline.
fn ends_with_newline(spans: &[RichSpan]) -> bool {
    spans.last().is_some_and(|s| s.text.ends_with('\n'))
}

/// Check if the last span(s) end with N+ consecutive newlines.
fn ends_with_newlines(spans: &[RichSpan], n: usize) -> bool {
    // Collect trailing text from spans
    let mut trailing = String::new();
    for span in spans.iter().rev() {
        trailing = format!("{}{trailing}", span.text);
        if trailing.len() >= n {
            break;
        }
    }
    let newline_count = trailing.chars().rev().take_while(|c| *c == '\n').count();
    newline_count >= n
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        let spans = parse_to_rich_spans("hello world");
        assert_eq!(spans.len(), 2); // text + paragraph end newline
        assert_eq!(spans[0].text, "hello world");
        assert!(!spans[0].bold);
        assert!(!spans[0].italic);
    }

    #[test]
    fn bold_text() {
        let spans = parse_to_rich_spans("**bold**");
        let bold_spans: Vec<_> = spans.iter().filter(|s| s.bold).collect();
        assert_eq!(bold_spans.len(), 1);
        assert_eq!(bold_spans[0].text, "bold");
    }

    #[test]
    fn italic_text() {
        let spans = parse_to_rich_spans("*italic*");
        let italic_spans: Vec<_> = spans.iter().filter(|s| s.italic).collect();
        assert_eq!(italic_spans.len(), 1);
        assert_eq!(italic_spans[0].text, "italic");
    }

    #[test]
    fn inline_code() {
        let spans = parse_to_rich_spans("`code`");
        let code_spans: Vec<_> = spans.iter().filter(|s| s.code).collect();
        assert_eq!(code_spans.len(), 1);
        assert_eq!(code_spans[0].text, "code");
    }

    #[test]
    fn heading() {
        let spans = parse_to_rich_spans("# Title");
        let heading_spans: Vec<_> = spans.iter().filter(|s| s.heading_level.is_some()).collect();
        assert_eq!(heading_spans.len(), 1);
        assert_eq!(heading_spans[0].text, "Title");
        assert_eq!(heading_spans[0].heading_level, Some(1));
        assert!(heading_spans[0].bold);
    }

    #[test]
    fn mixed_formatting() {
        let spans = parse_to_rich_spans("normal **bold** *italic* `code`");
        assert!(spans.iter().any(|s| s.text == "bold" && s.bold));
        assert!(spans.iter().any(|s| s.text == "italic" && s.italic));
        assert!(spans.iter().any(|s| s.text == "code" && s.code));
    }

    #[test]
    fn list_items() {
        let spans = parse_to_rich_spans("- one\n- two\n- three");
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("• one"));
        assert!(text.contains("• two"));
        assert!(text.contains("• three"));
    }

    #[test]
    fn ordered_list() {
        let spans = parse_to_rich_spans("1. first\n2. second\n3. third");
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("1. first"));
        assert!(text.contains("2. second"));
        assert!(text.contains("3. third"));
    }

    #[test]
    fn code_block() {
        let spans = parse_to_rich_spans("```\nfn main() {}\n```");
        let code_spans: Vec<_> = spans.iter().filter(|s| s.code_block).collect();
        assert!(!code_spans.is_empty());
        assert!(code_spans.iter().any(|s| s.text.contains("fn main")));
    }

    #[test]
    fn cosmic_spans_bold_attrs() {
        let spans = parse_to_rich_spans("**bold**");
        let base = Attrs::new().family(Family::Name("Noto Sans Mono"));
        let colors = MarkdownColors::default();
        let cosmic = to_cosmic_spans(&spans, &base, &colors);

        // Find the bold span
        let bold_span = cosmic.iter().find(|(t, _)| *t == "bold");
        assert!(bold_span.is_some());
    }
}
