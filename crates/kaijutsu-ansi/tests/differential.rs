//! Differential SGR attribution: for each golden corpus fixture, compare
//! *our* span attribution against an independent terminal emulator's
//! (`vt100`, a second implementation of "what does this escape sequence
//! mean") reading of the same bytes.
//!
//! # Scope — read this before trusting a failure
//!
//! `vt100` is grid-based (rows × fixed columns, a cursor that can move
//! backward, wrap, and overwrite); `ansi-strip` is stream-based (one
//! forward pass, no cursor, no overwrite). Those models only agree at all
//! for byte streams where nothing exploits the difference:
//!
//! - **No `\r`.** A lone `\r` in our output means "the byte is preserved,
//!   unrendered" (`docs/ansi-and-beyond.md`, "Open questions"); in a real
//!   terminal it returns the cursor to column 0 and overwrites. A fixture
//!   containing `\r` is skipped for this test wholesale.
//! - **`\n` does not reset the column.** None of these fixtures were
//!   captured through a pty (they're redirected straight to a file), so
//!   there is no ONLCR translation inserting a `\r` before each `\n` the
//!   way a live terminal session would see. `vt100::Screen::lf` (what a
//!   bare `0x0A` triggers) is ECMA-48 IND — row+1, column *unchanged* — not
//!   NEL. The cursor walk below does the same (row+1 only) to stay aligned
//!   with vt100's real position; this was found the hard way, as a very
//!   reproducible 8-column-off failure starting on the second fixture line.
//! - **No wrapping.** The `vt100::Parser` screen is sized far wider (4096
//!   cols) than any fixture line, so no line auto-wraps into the next row.
//! - **No other control bytes we drop but vt100 interprets** (BEL,
//!   backspace, form feed, vertical tab, NUL) — any of these would move
//!   vt100's cursor in a way our text-driven walk can't reconstruct, since
//!   the byte never appears in our stripped text at all to be counted.
//! - **ASCII only, for the whole fixture.** `vt100::Cell` positions
//!   double-width (wide) characters as one cell plus a continuation cell;
//!   reproducing that column accounting here — without vendoring a
//!   `unicode-width` table — is exactly the kind of ad-hoc parser surface
//!   `docs/ansi-and-beyond.md` says to avoid. Given the no-CR-on-`\n` point
//!   above, column position *carries across lines* (nothing resets it to
//!   0), so a single wide character anywhere would desync every character
//!   walked afterward in the whole fixture, not just that line — so the
//!   gate is fixture-wide, not line-by-line. `git_log_kaijutsu.raw` (an
//!   em dash) and `sgr_torture.raw` (CJK + emoji) are skipped by this rule;
//!   see [`skip_reason`].
//!
//! Within that scope: for every character, our fg color class, bg color
//! class, and bold flag must agree with vt100's cell at the position our
//! own cursor-tracking says it landed on. A disagreement here is a
//! **finding to report**, not something to loosen the comparison to hide —
//! see this crate's `docs/issues.md` if one turns up.

mod support;

use kaijutsu_ansi::strip;
use support::{CORPUS_FILES, ColorClass, read_fixture};

/// Wide enough that no corpus fixture line wraps; tall enough that no
/// fixture scrolls its early lines off the top.
const SCREEN_COLS: u16 = 4096;
const SCREEN_ROWS: u16 = 4096;

/// Why a fixture is unusable for this comparison, or `None` if it's in
/// scope. See the module docs for the reasoning behind each rule.
fn skip_reason(raw: &[u8]) -> Option<&'static str> {
    // C0 controls we drop but vt100 interprets as cursor motion; the byte
    // never reaches our stripped text, so our walk can't account for it.
    const DESYNCING_BYTES: [u8; 6] = [0x00, 0x07, 0x08, 0x0B, 0x0C, 0x0D];
    if raw.iter().any(|b| DESYNCING_BYTES.contains(b)) {
        return Some("contains \\r or another control byte vt100 interprets but we drop");
    }
    if !raw.is_ascii() {
        return Some("contains non-ASCII bytes (wide-character column accounting out of scope)");
    }
    None
}

/// Next tab stop from `col`, matching both `vt100::grid::Pos::col_tab` and
/// the ECMA-48 default of 8-column stops that kaish/most terminals use.
fn tab_stop(col: u16) -> u16 {
    col - (col % 8) + 8
}

#[test]
fn every_corpus_file_is_covered() {
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir(support::corpus_dir())
        .expect("tests/corpus should exist")
        .map(|e| e.expect("readable dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    let wired: std::collections::BTreeSet<String> =
        CORPUS_FILES.iter().map(|s| s.to_string()).collect();
    assert_eq!(on_disk, wired, "tests/corpus/*.raw vs support::CORPUS_FILES mismatch");
}

#[test]
fn fg_bg_bold_agree_with_vt100_on_comparable_fixtures() {
    let mut total_compared = 0usize;
    let mut in_scope = 0usize;

    for &fixture in CORPUS_FILES {
        let raw = read_fixture(fixture);
        if let Some(reason) = skip_reason(&raw) {
            eprintln!("differential: skipping {fixture}: {reason}");
            continue;
        }
        in_scope += 1;

        let (text, spans) = strip(&raw);

        let mut vt = vt100::Parser::new(SCREEN_ROWS, SCREEN_COLS, 0);
        vt.process(&raw);
        let screen = vt.screen();

        let mut row: u16 = 0;
        let mut col: u16 = 0;
        let mut compared_in_fixture = 0usize;

        for (byte_offset, c) in text.char_indices() {
            match c {
                '\n' => {
                    row += 1;
                    continue;
                },
                '\t' => {
                    col = tab_stop(col);
                    continue;
                },
                '\r' => unreachable!("skip_reason excludes \\r"),
                _ => {},
            }

            let cell = screen
                .cell(row, col)
                .unwrap_or_else(|| panic!("{fixture}: no vt100 cell at ({row},{col}) for {c:?}"));

            let span = spans
                .iter()
                .find(|s| (s.start as usize) <= byte_offset && byte_offset < (s.end as usize));
            let our_fg = ColorClass::from(span.and_then(|s| s.fg));
            let our_bg = ColorClass::from(span.and_then(|s| s.bg));
            let our_bold = span.is_some_and(|s| s.attrs.contains(kaijutsu_types::StyleAttrs::BOLD));

            let their_fg = vt100_color_class(cell.fgcolor());
            let their_bg = vt100_color_class(cell.bgcolor());
            let their_bold = cell.bold();

            assert_eq!(
                our_fg, their_fg,
                "{fixture}: fg disagreement at byte {byte_offset} ({c:?}, row {row} col {col}): ours={our_fg} vt100={their_fg}"
            );
            assert_eq!(
                our_bg, their_bg,
                "{fixture}: bg disagreement at byte {byte_offset} ({c:?}, row {row} col {col}): ours={our_bg} vt100={their_bg}"
            );
            assert_eq!(
                our_bold, their_bold,
                "{fixture}: bold disagreement at byte {byte_offset} ({c:?}, row {row} col {col}): ours={our_bold} vt100={their_bold}"
            );

            compared_in_fixture += 1;
            col += 1;
        }

        assert!(
            compared_in_fixture > 0,
            "{fixture}: in scope but nothing was actually compared \u{2014} scope check is probably wrong"
        );
        total_compared += compared_in_fixture;
    }

    assert!(total_compared > 0, "no fixture produced any comparison at all");
    eprintln!(
        "differential: compared {total_compared} characters across {in_scope}/{} fixtures",
        CORPUS_FILES.len()
    );
}

fn vt100_color_class(c: vt100::Color) -> ColorClass {
    match c {
        vt100::Color::Default => ColorClass::Default,
        vt100::Color::Idx(n) => ColorClass::Indexed(n),
        vt100::Color::Rgb(r, g, b) => ColorClass::Rgb(r, g, b),
    }
}
