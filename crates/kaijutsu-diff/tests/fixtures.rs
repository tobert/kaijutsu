//! Golden fixture tests — the dialect authority, exercised.
//!
//! These are deliberately integration tests rather than unit tests: they read
//! the same files, through the same public [`kaijutsu_diff::fixtures`] helpers,
//! that kernel and app tests will use. If this file can reach a fixture, so can
//! they.

use std::collections::BTreeSet;

use kaijutsu_diff::{DiffModel, fixtures, format, parse};

/// Every fixture on disk must appear in the inventory constants, and vice
/// versa. A fixture nobody tests is a dialect claim nobody checks.
#[test]
fn inventory_matches_the_directory() {
    let mut on_disk = BTreeSet::new();
    for group in ["canonical", "external", "invalid"] {
        let dir = fixtures::path(group);
        for entry in std::fs::read_dir(&dir).expect("fixture group directory") {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".diff") {
                on_disk.insert(format!("{group}/{name}"));
            }
        }
    }

    let declared: BTreeSet<String> = fixtures::CANONICAL
        .iter()
        .chain(fixtures::EXTERNAL.iter())
        .map(|s| s.to_string())
        .chain(fixtures::INVALID.iter().map(|(f, _)| f.to_string()))
        .collect();

    assert_eq!(
        on_disk, declared,
        "fixture inventory and directory disagree"
    );
}

/// `format ∘ parse == id` on canonical text — roundtrip property #2, pinned to
/// hand-written bytes rather than generated ones so the canonical form cannot
/// drift without a test noticing.
#[test]
fn canonical_fixtures_round_trip_byte_for_byte() {
    for name in fixtures::CANONICAL {
        let text = fixtures::read(name);
        let model = parse(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(format(&model), text, "{name} is not canonical");
    }
}

/// `format ∘ parse` is idempotent on valid non-canonical input — roundtrip
/// property #3.
#[test]
fn external_fixtures_canonicalize_and_then_hold_still() {
    for name in fixtures::EXTERNAL {
        let text = fixtures::read(name);
        let once = format(&parse(&text).unwrap_or_else(|e| panic!("{name}: {e}")));
        let twice = format(&parse(&once).unwrap_or_else(|e| panic!("{name} (2nd pass): {e}")));
        assert_eq!(once, twice, "{name} does not canonicalize to a fixed point");
        assert_eq!(
            parse(&once).unwrap(),
            parse(&text).unwrap(),
            "{name}: canonicalization changed the model"
        );
    }
}

/// Malformed input is a typed error, never a silent empty model.
#[test]
fn invalid_fixtures_are_rejected_with_the_expected_variant() {
    for (name, expected) in fixtures::INVALID {
        match parse(&fixtures::read(name)) {
            Ok(model) => panic!("{name} parsed instead of failing: {model:?}"),
            Err(e) => assert_eq!(e.variant_name(), *expected, "{name}: wrong error ({e})"),
        }
    }
}

/// Spot-check the structural claims the fixtures exist to pin, so a fixture
/// that quietly stops meaning what its filename says still fails.
#[test]
fn fixtures_mean_what_their_names_say() {
    use kaijutsu_diff::FileChange;

    let multi = parse(&fixtures::read("canonical/multi_file.diff")).unwrap();
    assert_eq!(multi.files.len(), 3);
    assert_eq!(multi.files[0].change, FileChange::Modified);
    assert_eq!(multi.files[1].change, FileChange::Added);
    assert_eq!(multi.files[2].change, FileChange::Deleted);
    assert_eq!(multi.stat().to_string(), "3 files, +3 −3");

    let renamed = parse(&fixtures::read("canonical/rename_pure.diff")).unwrap();
    assert_eq!(renamed.files[0].change, FileChange::Renamed);
    assert!(renamed.files[0].hunks.is_empty());

    let no_newline = parse(&fixtures::read("canonical/no_newline.diff")).unwrap();
    let lines = &no_newline.files[0].hunks[0].lines;
    assert!(lines.iter().filter(|l| l.no_newline).count() == 2);

    let quoted = parse(&fixtures::read("canonical/quoted_path.diff")).unwrap();
    assert_eq!(quoted.files[0].new_path, "dir/two words.txt");

    let octal = parse(&fixtures::read("external/git_octal_quoted_path.diff")).unwrap();
    assert_eq!(octal.files[0].new_path, "café.txt");

    let blank = parse(&fixtures::read("canonical/empty_context_line.diff")).unwrap();
    assert_eq!(blank.files[0].hunks[0].lines[1].text, "");

    let truncated = parse(&fixtures::read("canonical/truncated.diff")).unwrap();
    let t = truncated.truncated.expect("marker must survive parsing");
    assert_eq!(
        (t.omitted_files, t.omitted_hunks, t.omitted_lines),
        (2, 3, 41)
    );
    assert!(!truncated.is_complete());
}

/// A truncated fixture must not read as a complete patch to anything — the
/// marker is the first thing in the file and is not a valid diff construct.
#[test]
fn truncated_text_never_looks_complete() {
    let text = fixtures::read("canonical/truncated.diff");
    assert!(text.starts_with(kaijutsu_diff::TRUNCATION_MARKER_PREFIX));
    let stripped: String = text.lines().skip(1).map(|l| format!("{l}\n")).collect();
    // Without the marker the remainder *is* a valid patch — which is exactly
    // why the marker has to be there and has to survive the round trip.
    let complete = parse(&stripped).unwrap();
    assert!(complete.is_complete());
    assert_ne!(complete, parse(&text).unwrap());
}

/// The `DiffModel` default is a complete, empty diff — not a truncated one.
#[test]
fn empty_input_is_a_complete_empty_model() {
    assert_eq!(parse("").unwrap(), DiffModel::default());
    assert!(parse("").unwrap().is_complete());
}
