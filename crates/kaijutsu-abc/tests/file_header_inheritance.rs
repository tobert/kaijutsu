//! Tests for ABC v2.1 §2.2 file-header inheritance.
//!
//! Info fields appearing before the first `X:` in a multi-tune file
//! apply as defaults to every tune in the file. Tune-level fields win
//! on conflict.

use kaijutsu_abc::{parse_with_mode, InfoField, ParseMode};

#[test]
fn pre_x_content_does_not_become_a_phantom_tune() {
    let abc = "\
%abc-2.1
O:England
R:Reel

X:1
T:Tune One
K:G
CDE|

X:2
T:Tune Two
K:D
DEF|
";
    let result = parse_with_mode(abc, ParseMode::Generous);
    // Two X:N lines → exactly two tunes (not three, no phantom from
    // the pre-X header).
    assert_eq!(result.value.len(), 2, "tunes: {:?}",
        result.value.iter().map(|t| (&t.header.title, t.header.reference)).collect::<Vec<_>>(),
    );
    assert_eq!(result.value[0].header.reference, 1);
    assert_eq!(result.value[1].header.reference, 2);
}

#[test]
fn file_level_composer_inherited_when_tune_doesnt_set_it() {
    let abc = "\
C:File-level composer

X:1
T:Tune One
K:G
CDE|

X:2
T:Tune Two
C:Tune-level composer
K:D
DEF|
";
    let result = parse_with_mode(abc, ParseMode::Generous);
    // Tune 1 doesn't set C: — inherits "File-level composer".
    assert_eq!(
        result.value[0].header.composer.as_deref(),
        Some("File-level composer"),
    );
    // Tune 2 sets its own C: — wins over file-level.
    assert_eq!(
        result.value[1].header.composer.as_deref(),
        Some("Tune-level composer"),
    );
}

#[test]
fn file_level_other_fields_inherited_into_each_tune() {
    let abc = "\
O:England
H:File-level history note

X:1
T:Tune One
K:G
CDE|

X:2
T:Tune Two
K:D
DEF|
";
    let result = parse_with_mode(abc, ParseMode::Generous);

    let has_origin = |fields: &[InfoField]| {
        fields
            .iter()
            .any(|f| f.field_type == 'O' && f.value == "England")
    };
    let has_history = |fields: &[InfoField]| {
        fields
            .iter()
            .any(|f| f.field_type == 'H' && f.value.contains("File-level history"))
    };

    assert!(has_origin(&result.value[0].header.other_fields));
    assert!(has_origin(&result.value[1].header.other_fields));
    assert!(has_history(&result.value[0].header.other_fields));
    assert!(has_history(&result.value[1].header.other_fields));
}

#[test]
fn empty_file_header_keeps_single_tune_unchanged() {
    let abc = "X:1\nT:Solo\nK:C\nCDE|\n";
    let result = parse_with_mode(abc, ParseMode::Generous);
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].header.title, "Solo");
}

#[test]
fn no_x_at_all_still_parses_as_single_fragment() {
    let abc = "CDEF GABc|\n";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert_eq!(result.value.len(), 1);
}
