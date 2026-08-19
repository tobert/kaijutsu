//! Golden corpus: real terminal output, captured once as committed fixture
//! bytes (`tests/corpus/*.raw`), run through `strip()`, and snapshotted with
//! `insta`. Fixtures are frozen — regenerating them at test time would defeat
//! the point (a golden is a promise about *this* captured byte stream, not
//! about whatever `ls`/`git` happen to emit today on whatever machine runs
//! the test).
//!
//! Review a snapshot change like a diff review: does the new span table
//! still put color where a human reading the original capture would expect
//! it, and is the text still escape-free? `cargo insta review` (or
//! `INSTA_UPDATE=always cargo test` on first generation, then hand-review
//! before `cargo insta accept`).

mod support;

use kaijutsu_ansi::strip;
use support::{CORPUS_FILES, read_fixture, render_strip_result};

#[test]
fn ls_mixed_dir() {
    check("ls_mixed_dir.raw");
}

#[test]
fn git_diff_synthetic() {
    check("git_diff_synthetic.raw");
}

#[test]
fn git_log_kaijutsu() {
    check("git_log_kaijutsu.raw");
}

#[test]
fn git_log_throwaway() {
    check("git_log_throwaway.raw");
}

#[test]
fn sgr_torture() {
    check("sgr_torture.raw");
}

/// Every corpus file has a dedicated `#[test]` above (for a readable
/// `cargo test` failure list); this just double-checks the two lists agree,
/// so a fixture can't silently go untested.
#[test]
fn every_corpus_file_has_a_test() {
    let on_disk: std::collections::BTreeSet<String> = std::fs::read_dir(support::corpus_dir())
        .expect("tests/corpus should exist")
        .map(|e| e.expect("readable dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    let wired: std::collections::BTreeSet<String> =
        CORPUS_FILES.iter().map(|s| s.to_string()).collect();
    assert_eq!(on_disk, wired, "tests/corpus/*.raw vs support::CORPUS_FILES mismatch");
}

fn check(fixture: &str) {
    let bytes = read_fixture(fixture);
    let (text, spans) = strip(&bytes);

    // Sanity the golden review notes lean on: no escape byte or other raw
    // control character survives into the text, past the three we keep
    // structurally (\n \t \r).
    assert!(!text.contains('\u{1b}'), "ESC leaked into stripped text for {fixture}");
    for c in text.chars() {
        assert!(
            c == '\n' || c == '\t' || c == '\r' || !c.is_control(),
            "control char {c:?} survived stripping {fixture}"
        );
    }

    let rendering = render_strip_result(&text, &spans);
    insta::assert_snapshot!(fixture, rendering);
}
