//! Shared helpers for the golden-corpus (`goldens.rs`) and differential
//! (`differential.rs`) tests. Lives under `tests/support/` rather than
//! `tests/support.rs` so cargo does not treat it as its own test binary —
//! standard integration-test sharing pattern.
//!
//! Each test binary compiles its own copy of this module and uses a
//! different subset of it (`differential.rs` has no use for
//! `render_strip_result`, `goldens.rs` has no use for `ColorClass`
//! directly), so `dead_code` is expected per-binary rather than a sign
//! something here is actually unused.
#![allow(dead_code)]

use kaijutsu_types::{StyleAttrs, StyleColor, StyleSpan};
use std::path::{Path, PathBuf};

/// Corpus fixtures, in a fixed and deliberate order (not a directory
/// listing) so a new file must be wired in here to be exercised — the same
/// reason `nasty_inputs()` in `properties.rs` is a literal `Vec`, not a
/// glob.
pub const CORPUS_FILES: &[&str] = &[
    "ls_mixed_dir.raw",
    "git_diff_synthetic.raw",
    "git_log_kaijutsu.raw",
    "git_log_throwaway.raw",
    "sgr_torture.raw",
];

pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

pub fn read_fixture(name: &str) -> Vec<u8> {
    let path = corpus_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {path:?}: {e}"))
}

/// `Some(StyleColor)` / vt100's `Color` both collapse to this for
/// comparison and for the golden span table — "what kind of color, ignoring
/// which library's enum it's wearing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorClass {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<Option<StyleColor>> for ColorClass {
    fn from(c: Option<StyleColor>) -> Self {
        match c {
            None => ColorClass::Default,
            Some(StyleColor::Indexed(n)) => ColorClass::Indexed(n),
            Some(StyleColor::Rgb(r, g, b)) => ColorClass::Rgb(r, g, b),
        }
    }
}

impl std::fmt::Display for ColorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColorClass::Default => write!(f, "default"),
            ColorClass::Indexed(n) => write!(f, "indexed({n})"),
            ColorClass::Rgb(r, g, b) => write!(f, "rgb({r},{g},{b})"),
        }
    }
}

/// Names of the set bits in `attrs`, in a fixed order — for both the golden
/// span table and any debug output in the differential test.
pub fn attr_names(attrs: StyleAttrs) -> Vec<&'static str> {
    let table: &[(StyleAttrs, &str)] = &[
        (StyleAttrs::BOLD, "bold"),
        (StyleAttrs::DIM, "dim"),
        (StyleAttrs::ITALIC, "italic"),
        (StyleAttrs::UNDERLINE, "underline"),
        (StyleAttrs::INVERSE, "inverse"),
        (StyleAttrs::STRIKETHROUGH, "strikethrough"),
        (StyleAttrs::BLINK, "blink"),
    ];
    table.iter().filter(|(bit, _)| attrs.contains(*bit)).map(|(_, name)| *name).collect()
}

/// A readable rendering of a `strip()` result: the clean text, delimited so
/// leading/trailing whitespace and control characters are visible, plus a
/// compact span table. This is the string insta snapshots in `goldens.rs`.
pub fn render_strip_result(text: &str, spans: &[StyleSpan]) -> String {
    let mut out = String::new();
    out.push_str("=== text (");
    out.push_str(&text.len().to_string());
    out.push_str(" bytes) ===\n");
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("=== end text ===\n\n");

    out.push_str(&format!("=== spans ({}) ===\n", spans.len()));
    for span in spans {
        let fg = ColorClass::from(span.fg);
        let bg = ColorClass::from(span.bg);
        let attrs = attr_names(span.attrs);
        let attrs_repr = if attrs.is_empty() { "-".to_string() } else { attrs.join("+") };
        out.push_str(&format!(
            "{:>5}..{:<5} fg={:<14} bg={:<14} attrs={}\n",
            span.start, span.end, fg, bg, attrs_repr
        ));
    }
    if spans.is_empty() {
        out.push_str("(none)\n");
    }
    out
}
