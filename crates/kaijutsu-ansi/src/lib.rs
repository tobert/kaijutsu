//! `ansi-strip` — the first Kaijutsu ingest transform.
//!
//! Escape bytes are the terminal world's storage-engine ops, and Kaijutsu
//! doctrine says players consume *projected facts, not encodings*. So terminal
//! output is stripped at ingestion: the escape sequences become a semantic
//! [`StyleSpan`] map, the clean text becomes block content, and the byte-exact
//! original is kept as provenance by the kernel. Design:
//! `docs/ansi-and-beyond.md`.
//!
//! ```
//! use kaijutsu_ansi::strip;
//!
//! let (text, spans) = strip(b"plain \x1b[1;31mred bold\x1b[0m tail");
//! assert_eq!(text, "plain red bold tail");
//! assert_eq!(spans.len(), 1);
//! assert_eq!((spans[0].start, spans[0].end), (6, 14));
//! ```
//!
//! # What this is not
//!
//! It is not a terminal. The parser is alacritty's [`vte`] — the canonical DEC
//! state machine with no execution semantics — and everything on top of it is
//! SGR arithmetic plus span assembly. Three invariants are *stated*, not merely
//! tested (`docs/ansi-and-beyond.md`, "Standing safety invariants"):
//!
//! 1. **We never respond.** DSR / DA answerback injection is dead by
//!    construction: there is no output channel here at all.
//! 2. **We never execute OSC.** OSC 52 (clipboard write) and OSC 8 (hyperlink)
//!    are consumed inert — their payloads reach neither the text nor a span.
//! 3. **Cursor and screen control produce nothing.** No cursor motion means no
//!    overwrite, so what a model reads and what a human sees cannot be made to
//!    diverge by hidden-text games.
//!
//! Losslessness is deliberately *not* a goal — round-trip duty belongs to the
//! provenance bytes, which is why the span map is allowed to be 80/20 and
//! re-derivable ([`PARSER_VERSION`] tags which parser produced a given map).
//!
//! # Totality
//!
//! [`strip`] and [`StripParser::feed`] accept arbitrary bytes — truncated
//! sequences, invalid UTF-8, adversarial parameter floods — without panicking,
//! in memory proportional to the input.
//!
//! # Chunking
//!
//! [`StripParser`] is the streaming form, for callers whose bytes arrive in
//! flushes (kaish output, background process drains). A chunk boundary may fall
//! *anywhere*: mid escape sequence, mid SGR parameter, mid UTF-8 codepoint.
//! `vte` carries both kinds of partial state across `feed` calls, so splitting
//! the input changes nothing about the result — never pre-convert bytes to a
//! `String` before feeding, since a lossy conversion would corrupt a codepoint
//! split across the boundary.
//!
//! ```
//! use kaijutsu_ansi::{strip, StripParser};
//!
//! let input = "\x1b[32mgrün\x1b[0m".as_bytes();
//! let mut parser = StripParser::new();
//! for i in 0..input.len() {
//!     parser.feed(&input[i..i + 1]);
//! }
//! assert_eq!(parser.finish(), strip(input));
//! ```

mod sgr;

use sgr::SgrState;

// Re-exported so a caller can name the whole vocabulary of this transform's
// output from one crate. These are kaijutsu-types' definitions, not ours —
// spans are durable block state, not a parser detail.
pub use kaijutsu_types::{ProvenanceTag, StyleAttrs, StyleColor, StyleSpan};

/// Version of this transform, recorded on every block it projects.
///
/// Bump it whenever the *output* of the transform changes for some input — a
/// wider SGR set, a different control-character policy, a bug fix in span
/// assembly. The CI invariant `strip(original) == (content, style_spans)` only
/// holds for blocks tagged with the current version; older ones are
/// re-derivable via `kj block reproject`.
pub const PARSER_VERSION: u32 = 1;

/// Transform name recorded in [`ProvenanceTag`] and in the kernel's
/// `block_provenance` table.
pub const TRANSFORM_NAME: &str = "ansi-strip";

/// The provenance tag for blocks projected by *this* build of the transform.
pub fn provenance_tag() -> ProvenanceTag {
    ProvenanceTag { transform: TRANSFORM_NAME.to_string(), version: PARSER_VERSION }
}

/// Strip ANSI escape sequences from `bytes`, returning the clean text and the
/// style spans over it.
///
/// Span offsets are byte offsets into the **returned string**, always on UTF-8
/// character boundaries. Exactly equivalent to
/// [`StripParser::new`] + [`feed`](StripParser::feed) + [`finish`](StripParser::finish).
pub fn strip(bytes: &[u8]) -> (String, Vec<StyleSpan>) {
    let mut parser = StripParser::new();
    parser.feed(bytes);
    parser.finish()
}

/// Incremental `ansi-strip`: feed bytes as they arrive, in any chunking.
///
/// State that survives a chunk boundary lives in two places — `vte`'s state
/// machine (partial escape sequences, partial UTF-8 codepoints) and the
/// [`Sink`] (accumulated text, spans, active SGR state) — so a caller only has
/// to keep the parser alive across flushes.
pub struct StripParser {
    parser: vte::Parser,
    sink: Sink,
}

impl StripParser {
    /// A parser with empty output and default (unstyled) SGR state.
    pub fn new() -> Self {
        StripParser { parser: vte::Parser::new(), sink: Sink::default() }
    }

    /// Consume a chunk. Callable any number of times; the chunk may end
    /// mid-sequence or mid-codepoint.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.parser.advance(&mut self.sink, chunk);
    }

    /// The clean text accumulated so far.
    pub fn text(&self) -> &str {
        &self.sink.text
    }

    /// The spans accumulated so far. Complete for the text returned by
    /// [`text`](Self::text) — spans are closed as characters are emitted, not
    /// at [`finish`](Self::finish), so a mid-stream read is already consistent.
    pub fn spans(&self) -> &[StyleSpan] {
        &self.sink.spans
    }

    /// Finish, yielding the clean text and its spans.
    ///
    /// Anything still in flight is dropped, which is the honest reading of a
    /// truncated stream: a half-received escape sequence never happened, and a
    /// half-received codepoint is not a character.
    pub fn finish(self) -> (String, Vec<StyleSpan>) {
        (self.sink.text, self.sink.spans)
    }
}

impl Default for StripParser {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StripParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // vte::Parser is not Debug, and its internals are not ours to report.
        f.debug_struct("StripParser")
            .field("text_len", &self.sink.text.len())
            .field("spans", &self.sink.spans.len())
            .field("style", &self.sink.style)
            .finish()
    }
}

/// The `vte::Perform` side: turns parser callbacks into text plus spans.
#[derive(Debug, Default)]
struct Sink {
    text: String,
    spans: Vec<StyleSpan>,
    /// Rendition the *next* emitted character will wear.
    style: SgrState,
    /// Set once output grows past `u32::MAX` and span offsets can no longer be
    /// represented. Text keeps accumulating correctly; styling stops rather
    /// than being recorded wrong. A 4 GiB block is already pathological, and a
    /// panic here would break the totality invariant.
    offsets_exhausted: bool,
}

impl Sink {
    /// Append one character and extend (or open) the span covering it.
    ///
    /// Every emitted character carries the active rendition, control
    /// characters included — a background color set across a tab or a newline
    /// covers it, exactly as a terminal would.
    fn push_char(&mut self, c: char) {
        let start = self.text.len();
        self.text.push(c);
        let end = self.text.len();

        if self.style.is_default() || self.offsets_exhausted {
            return;
        }
        let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
            self.offsets_exhausted = true;
            return;
        };

        // Coalesce with the previous span when it is adjacent and identical.
        // This is what keeps redundant SGR (`ESC[31m` twice, or reset-then-set
        // with no text between) from fragmenting a run.
        if let Some(last) = self.spans.last_mut()
            && last.end == start
            && last.fg == self.style.fg
            && last.bg == self.style.bg
            && last.attrs == self.style.attrs
        {
            last.end = end;
            return;
        }

        self.spans.push(StyleSpan {
            start,
            end,
            fg: self.style.fg,
            bg: self.style.bg,
            attrs: self.style.attrs,
        });
    }
}

impl vte::Perform for Sink {
    fn print(&mut self, c: char) {
        self.push_char(c);
    }

    /// C0/C1 control functions.
    ///
    /// Newline and tab are structure a reader needs; carriage return is kept
    /// verbatim under "preserve, don't render" — collapsing `\r`-overwrite
    /// progress bars is a downstream decision, not this transform's
    /// (`docs/ansi-and-beyond.md`, "Open questions"). Every other C0 — BEL,
    /// backspace, form feed, vertical tab, the C1 block — is dropped: none of
    /// them mean anything in a projection with no cursor.
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\t' | b'\r' => self.push_char(char::from(byte)),
            _ => {}
        }
    }

    /// CSI. Only SGR (`m`) with no intermediates affects the projection;
    /// cursor motion, erase (ED/EL), scroll regions, private modes (`?`
    /// intermediates) and everything else are consumed and produce nothing.
    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        // `_ignore` means vte hit its parameter or intermediate cap and
        // dropped the tail. The prefix it did parse is still well-formed SGR,
        // so applying it beats discarding a hundred-parameter line wholesale.
        if action == 'm' && intermediates.is_empty() {
            self.style.apply(params);
        }
    }

    /// OSC — window titles, OSC 8 hyperlinks, OSC 52 clipboard writes.
    /// Consumed inert: never executed, never answered, payload never leaks
    /// into the text. This is safety invariant 2.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    /// DCS start. We select no handler, so the string body goes nowhere.
    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    /// DCS / APC / PM / SOS body bytes — dropped, never printed.
    fn put(&mut self, _byte: u8) {}

    /// DCS terminator.
    fn unhook(&mut self) {}

    /// Two-byte escapes: charset selection, RIS, DECSC/DECRC, index/reverse
    /// index. All of them are terminal state we do not have.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{StyleAttrs, StyleColor};

    /// Spans as `(start, end, fg, bg, attr bits)` — terser to assert on.
    type Shape = (u32, u32, Option<StyleColor>, Option<StyleColor>, u16);

    fn shape(spans: &[StyleSpan]) -> Vec<Shape> {
        spans.iter().map(|s| (s.start, s.end, s.fg, s.bg, s.attrs.0)).collect()
    }

    fn attrs_of(input: &str) -> StyleAttrs {
        let (_, spans) = strip(input.as_bytes());
        spans.first().map(|s| s.attrs).unwrap_or_default()
    }

    #[test]
    fn plain_ascii_is_identity_with_no_spans() {
        let input = "hello, world\nsecond line\twith a tab\n";
        let (text, spans) = strip(input.as_bytes());
        assert_eq!(text, input);
        assert!(spans.is_empty());
    }

    #[test]
    fn transform_identity_is_stable() {
        assert_eq!(TRANSFORM_NAME, "ansi-strip");
        let tag = provenance_tag();
        assert_eq!(tag.transform, TRANSFORM_NAME);
        assert_eq!(tag.version, PARSER_VERSION);
    }

    #[test]
    fn basic_color_span() {
        let (text, spans) = strip(b"a\x1b[31mred\x1b[0mb");
        assert_eq!(text, "aredb");
        assert_eq!(shape(&spans), vec![(1, 4, Some(StyleColor::Indexed(1)), None, 0)]);
    }

    #[test]
    fn every_simple_attribute_maps() {
        assert_eq!(attrs_of("\x1b[1mx"), StyleAttrs::BOLD);
        assert_eq!(attrs_of("\x1b[2mx"), StyleAttrs::DIM);
        assert_eq!(attrs_of("\x1b[3mx"), StyleAttrs::ITALIC);
        assert_eq!(attrs_of("\x1b[4mx"), StyleAttrs::UNDERLINE);
        assert_eq!(attrs_of("\x1b[5mx"), StyleAttrs::BLINK);
        assert_eq!(attrs_of("\x1b[6mx"), StyleAttrs::BLINK);
        assert_eq!(attrs_of("\x1b[7mx"), StyleAttrs::INVERSE);
        assert_eq!(attrs_of("\x1b[9mx"), StyleAttrs::STRIKETHROUGH);
    }

    #[test]
    fn attribute_resets_clear_exactly_their_bit() {
        // Everything on, then each reset in turn.
        let all = "\x1b[1;2;3;4;5;7;9m";
        let full = StyleAttrs::BOLD
            | StyleAttrs::DIM
            | StyleAttrs::ITALIC
            | StyleAttrs::UNDERLINE
            | StyleAttrs::BLINK
            | StyleAttrs::INVERSE
            | StyleAttrs::STRIKETHROUGH;
        assert_eq!(attrs_of(&format!("{all}x")), full);

        let mut without_bold = full;
        without_bold.remove(StyleAttrs::BOLD);
        assert_eq!(attrs_of(&format!("{all}\x1b[21mx")), without_bold);

        let mut without_intensity = full;
        without_intensity.remove(StyleAttrs::BOLD | StyleAttrs::DIM);
        assert_eq!(attrs_of(&format!("{all}\x1b[22mx")), without_intensity);

        for (code, bit) in [
            (23, StyleAttrs::ITALIC),
            (24, StyleAttrs::UNDERLINE),
            (25, StyleAttrs::BLINK),
            (27, StyleAttrs::INVERSE),
            (29, StyleAttrs::STRIKETHROUGH),
        ] {
            let mut expected = full;
            expected.remove(bit);
            assert_eq!(attrs_of(&format!("{all}\x1b[{code}mx")), expected, "SGR {code}");
        }
    }

    #[test]
    fn underline_subparameter_zero_turns_it_off() {
        assert_eq!(attrs_of("\x1b[4mx"), StyleAttrs::UNDERLINE);
        // 4:3 is curly underline — a style we do not model, still underlined.
        assert_eq!(attrs_of("\x1b[4:3mx"), StyleAttrs::UNDERLINE);
        assert_eq!(attrs_of("\x1b[4m\x1b[4:0mx"), StyleAttrs::default());
    }

    #[test]
    fn indexed_colors_cover_both_ranges() {
        for n in 0..8u8 {
            let (_, spans) = strip(format!("\x1b[{}mx", 30 + u16::from(n)).as_bytes());
            assert_eq!(spans[0].fg, Some(StyleColor::Indexed(n)));
            let (_, spans) = strip(format!("\x1b[{}mx", 40 + u16::from(n)).as_bytes());
            assert_eq!(spans[0].bg, Some(StyleColor::Indexed(n)));
            // Bright ranges land in slots 8..16.
            let (_, spans) = strip(format!("\x1b[{}mx", 90 + u16::from(n)).as_bytes());
            assert_eq!(spans[0].fg, Some(StyleColor::Indexed(n + 8)));
            let (_, spans) = strip(format!("\x1b[{}mx", 100 + u16::from(n)).as_bytes());
            assert_eq!(spans[0].bg, Some(StyleColor::Indexed(n + 8)));
        }
    }

    #[test]
    fn extended_colors_semicolon_form() {
        let (_, spans) = strip(b"\x1b[38;5;196mx");
        assert_eq!(spans[0].fg, Some(StyleColor::Indexed(196)));
        let (_, spans) = strip(b"\x1b[48;5;17mx");
        assert_eq!(spans[0].bg, Some(StyleColor::Indexed(17)));
        let (_, spans) = strip(b"\x1b[38;2;12;34;56mx");
        assert_eq!(spans[0].fg, Some(StyleColor::Rgb(12, 34, 56)));
        let (_, spans) = strip(b"\x1b[48;2;255;0;127mx");
        assert_eq!(spans[0].bg, Some(StyleColor::Rgb(255, 0, 127)));
    }

    #[test]
    fn extended_colors_subparameter_form() {
        let (_, spans) = strip(b"\x1b[38:5:196mx");
        assert_eq!(spans[0].fg, Some(StyleColor::Indexed(196)));
        let (_, spans) = strip(b"\x1b[38:2:12:34:56mx");
        assert_eq!(spans[0].fg, Some(StyleColor::Rgb(12, 34, 56)));
        // ITU form with an (empty) color space id in slot 2.
        let (_, spans) = strip(b"\x1b[38:2::12:34:56mx");
        assert_eq!(spans[0].fg, Some(StyleColor::Rgb(12, 34, 56)));
        let (_, spans) = strip(b"\x1b[48:2::1:2:3mx");
        assert_eq!(spans[0].bg, Some(StyleColor::Rgb(1, 2, 3)));
    }

    #[test]
    fn extended_color_continues_the_same_sequence() {
        // A color followed by more parameters in one CSI: both must land.
        let (text, spans) = strip(b"\x1b[1;38;2;10;20;30;4;48;5;9mx");
        assert_eq!(text, "x");
        assert_eq!(spans[0].fg, Some(StyleColor::Rgb(10, 20, 30)));
        assert_eq!(spans[0].bg, Some(StyleColor::Indexed(9)));
        assert_eq!(spans[0].attrs, StyleAttrs::BOLD | StyleAttrs::UNDERLINE);
    }

    #[test]
    fn default_color_codes_clear_only_their_channel() {
        let (_, spans) = strip(b"\x1b[31;42ma\x1b[39mb\x1b[49mc");
        assert_eq!(
            shape(&spans),
            vec![
                (0, 1, Some(StyleColor::Indexed(1)), Some(StyleColor::Indexed(2)), 0),
                (1, 2, None, Some(StyleColor::Indexed(2)), 0),
            ]
        );
        // "c" is unstyled, so it gets no span at all.
    }

    #[test]
    fn reset_clears_everything() {
        let (text, spans) = strip(b"\x1b[1;31;44ma\x1b[0mb\x1b[mc");
        assert_eq!(text, "abc");
        assert_eq!(
            shape(&spans),
            vec![(0, 1, Some(StyleColor::Indexed(1)), Some(StyleColor::Indexed(4)), StyleAttrs::BOLD.0)]
        );
    }

    #[test]
    fn empty_parameter_is_a_reset() {
        // `CSI ;31m` is "default, then red" — the empty slot means zero.
        let (_, spans) = strip(b"\x1b[1m\x1b[;31mx");
        assert_eq!(shape(&spans), vec![(0, 1, Some(StyleColor::Indexed(1)), None, 0)]);
    }

    #[test]
    fn identical_adjacent_runs_coalesce() {
        // Redundant re-set, reset-and-restore with no text between, and a
        // no-op CSI in the middle: one span, not four.
        let (text, spans) = strip(b"\x1b[31ma\x1b[31mb\x1b[0m\x1b[31mc\x1b[2Kd");
        assert_eq!(text, "abcd");
        assert_eq!(shape(&spans), vec![(0, 4, Some(StyleColor::Indexed(1)), None, 0)]);
    }

    #[test]
    fn conceal_does_not_hide_text_from_anyone() {
        // SGR 8 is not modelled on purpose: honouring it would let output be
        // invisible to the human while the model still read it. Everyone gets
        // the same text, and a red span rides through unchanged.
        let (text, spans) = strip(b"\x1b[31mvisible\x1b[8mhidden\x1b[28mvisible\x1b[0m");
        assert_eq!(text, "visiblehiddenvisible");
        assert_eq!(shape(&spans), vec![(0, 20, Some(StyleColor::Indexed(1)), None, 0)]);
    }

    #[test]
    fn unstyled_runs_never_get_spans() {
        let (text, spans) = strip(b"\x1b[0mplain\x1b[39;49;22mstill plain");
        assert_eq!(text, "plainstill plain");
        assert!(spans.is_empty());
    }

    #[test]
    fn control_characters_keep_only_the_structural_three() {
        // BEL, backspace, vertical tab, form feed, NUL, SO/SI are dropped;
        // \n, \t, \r survive.
        let (text, spans) = strip(b"a\x07b\x08c\x0bd\x0ce\x00f\n\tg\rh");
        assert_eq!(text, "abcdef\n\tg\rh");
        assert!(spans.is_empty());
    }

    #[test]
    fn control_characters_carry_the_active_style() {
        let (text, spans) = strip(b"\x1b[41ma\tb\x1b[0m");
        assert_eq!(text, "a\tb");
        assert_eq!(shape(&spans), vec![(0, 3, None, Some(StyleColor::Indexed(1)), 0)]);
    }

    #[test]
    fn cursor_and_erase_sequences_vanish_entirely() {
        let input = b"\x1b[2J\x1b[H\x1b[10;20Hcell\x1b[K\x1b[1A\x1b[3D\x1b[?25l\x1b[?1049h!";
        let (text, spans) = strip(input);
        assert_eq!(text, "cell!");
        assert!(spans.is_empty());
    }

    #[test]
    fn osc_payloads_never_leak() {
        // Window title (BEL-terminated), OSC 8 hyperlink (ST-terminated) with
        // its visible label, and an OSC 52 clipboard write.
        let (text, spans) = strip(b"\x1b]0;my title\x07A");
        assert_eq!(text, "A");
        assert!(spans.is_empty());

        let (text, _) = strip(b"\x1b]8;;https://example.invalid/secret\x1b\\label\x1b]8;;\x1b\\");
        assert_eq!(text, "label");

        let (text, _) = strip(b"before\x1b]52;c;c2VjcmV0\x07after");
        assert_eq!(text, "beforeafter");
    }

    #[test]
    fn dcs_and_apc_payloads_never_leak() {
        let (text, _) = strip(b"a\x1bP1$r0m\x1b\\b");
        assert_eq!(text, "ab");
        let (text, _) = strip(b"a\x1b_payload\x1b\\b");
        assert_eq!(text, "ab");
        // Sixel-ish DCS body full of printable bytes.
        let (text, _) = strip(b"x\x1bPq#0;2;0;0;0#0~~@@vv@@~~@@~~$\x1b\\y");
        assert_eq!(text, "xy");
    }

    #[test]
    fn two_byte_escapes_vanish() {
        // Charset selection, RIS, save/restore cursor, reverse index.
        let (text, _) = strip(b"\x1b(0q\x1b(Bz\x1bc\x1b7\x1b8\x1bM!");
        assert_eq!(text, "qz!");
    }

    #[test]
    fn multibyte_text_keeps_char_boundaries() {
        let (text, spans) = strip("plain \x1b[32m日本語\x1b[0m 🎺!".as_bytes());
        assert_eq!(text, "plain 日本語 🎺!");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 6);
        assert_eq!(spans[0].end, 6 + "日本語".len() as u32);
        assert!(text.is_char_boundary(spans[0].start as usize));
        assert!(text.is_char_boundary(spans[0].end as usize));
    }

    #[test]
    fn streaming_accessors_track_the_stream() {
        let mut parser = StripParser::new();
        parser.feed(b"\x1b[31mab");
        assert_eq!(parser.text(), "ab");
        assert_eq!(shape(parser.spans()), vec![(0, 2, Some(StyleColor::Indexed(1)), None, 0)]);
        parser.feed(b"c\x1b[0md");
        assert_eq!(parser.text(), "abcd");
        assert_eq!(shape(parser.spans()), vec![(0, 3, Some(StyleColor::Indexed(1)), None, 0)]);
        assert_eq!(parser.finish().0, "abcd");
    }

    #[test]
    fn feed_is_callable_with_empty_chunks() {
        let mut parser = StripParser::new();
        for _ in 0..4 {
            parser.feed(b"");
        }
        parser.feed(b"\x1b[1mx");
        for _ in 0..4 {
            parser.feed(b"");
        }
        assert_eq!(parser.finish(), strip(b"\x1b[1mx"));
    }

    #[test]
    fn debug_impl_reports_progress() {
        let mut parser = StripParser::new();
        parser.feed(b"\x1b[31mabc");
        let rendered = format!("{parser:?}");
        assert!(rendered.contains("text_len: 3"), "{rendered}");
        assert!(rendered.contains("spans: 1"), "{rendered}");
    }
}
