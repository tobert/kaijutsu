//! Tests for the [`ParseMode`] contract.
//!
//! These pin the per-mode promotion of warnings into errors so that
//! "harder errors for completely broken inputs" stays honest:
//!
//! - **Strict**  — missing X:/K:, body-before-K:, unrecognized syntax → errors
//! - **Generous** — historical lenient behavior (warnings, never errors)
//! - **Fragment** — missing-header silent, unrecognized syntax still warns
//!
//! Reserved characters from ABC v2.1 §8.1 (`# * ; ? @`) are always
//! warnings regardless of mode — they're spec-legal input the parser
//! doesn't have a meaning for, not invalid input.

use kaijutsu_abc::{parse_with_mode, FeedbackLevel, ParseMode};

fn errors_contain(result: &kaijutsu_abc::ParseResult<Vec<kaijutsu_abc::Tune>>, needle: &str) -> bool {
    result
        .feedback
        .iter()
        .filter(|f| f.level == FeedbackLevel::Error)
        .any(|f| f.message.contains(needle))
}

fn warnings_contain(result: &kaijutsu_abc::ParseResult<Vec<kaijutsu_abc::Tune>>, needle: &str) -> bool {
    result
        .feedback
        .iter()
        .filter(|f| f.level == FeedbackLevel::Warning)
        .any(|f| f.message.contains(needle))
}

fn no_errors(result: &kaijutsu_abc::ParseResult<Vec<kaijutsu_abc::Tune>>) -> bool {
    !result.has_errors()
}

// ---------- Strict mode ----------

#[test]
fn strict_errors_on_missing_x_field() {
    let abc = "T:Test\nK:C\nCDE|";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        errors_contain(&result, "Missing X:"),
        "expected missing-X: error, got feedback: {:?}",
        result.feedback,
    );
}

#[test]
fn strict_errors_on_missing_k_field() {
    let abc = "X:1\nT:Test\nM:4/4\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        errors_contain(&result, "Missing K:"),
        "expected missing-K: error, got feedback: {:?}",
        result.feedback,
    );
}

#[test]
fn strict_errors_on_body_before_k() {
    let abc = "X:1\nT:Test\nCDE|\nK:C\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        errors_contain(&result, "Body started before K:"),
        "expected body-before-K: error, got feedback: {:?}",
        result.feedback,
    );
}

#[test]
fn strict_errors_on_unrecognized_construct() {
    // `j` is not a note, decoration, field, or reserved character — it
    // hits the unknown-char fallback and must error in strict mode.
    let abc = "X:1\nT:Test\nK:C\nCDEjFGA|\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        errors_contain(&result, "Unrecognized construct"),
        "expected unrecognized-construct error, got feedback: {:?}",
        result.feedback,
    );
}

#[test]
fn strict_accepts_well_formed_tune() {
    let abc = "X:1\nT:Test\nM:4/4\nL:1/4\nK:C\nCDEF|GABc|\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        no_errors(&result),
        "well-formed tune should have no errors, got: {:?}",
        result.feedback,
    );
}

// ---------- Reserved chars per §8.1 stay warnings in every mode ----------

#[test]
fn reserved_chars_warn_even_in_strict() {
    // Per §8.1 example: `@a !pp! #bc2/3* [K:C#]` — the `@`, `#`, `*` are
    // reserved-for-future-use and current software should ignore them
    // with at most a warning.  We pick a subset that doesn't trip the
    // "unrecognized construct" path (no `!`).
    let abc = "X:1\nT:Test\nK:C\n#a*b@c|\n";
    let result = parse_with_mode(abc, ParseMode::Strict);

    assert!(
        warnings_contain(&result, "Reserved character"),
        "expected reserved-char warnings, got: {:?}",
        result.feedback,
    );
    assert!(
        no_errors(&result),
        "reserved chars must not promote to errors, got: {:?}",
        result.feedback,
    );
}

// ---------- Generous mode preserves legacy behavior ----------

#[test]
fn generous_warns_on_missing_x_field() {
    let abc = "T:Test\nK:C\nCDE|";
    let result = parse_with_mode(abc, ParseMode::Generous);
    assert!(no_errors(&result));
    assert!(warnings_contain(&result, "Missing X:"));
}

#[test]
fn generous_warns_on_unrecognized_construct() {
    // `j` hits the unknown-char fallback (not a note, decoration, field,
    // or reserved char).
    let abc = "X:1\nT:Test\nK:C\nCDEjFGA|\n";
    let result = parse_with_mode(abc, ParseMode::Generous);
    assert!(
        no_errors(&result),
        "generous mode must not error on unknown constructs, got: {:?}",
        result.feedback,
    );
    assert!(warnings_contain(&result, "Skipping unknown character"));
}

// ---------- Fragment mode ----------

#[test]
fn fragment_silent_on_missing_headers() {
    // Bare music fragment as it appears in spec §4.11.
    let abc = "(c (d e f) g a)";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(no_errors(&result));
    assert!(
        !warnings_contain(&result, "Missing X:")
            && !warnings_contain(&result, "Missing K:")
            && !warnings_contain(&result, "Missing M:")
            && !warnings_contain(&result, "Body started before K:"),
        "fragment mode should not complain about absent headers, got: {:?}",
        result.feedback,
    );
}

#[test]
fn invisible_space_y_consumed_silently() {
    // §6.1: `y` is the invisible-space engraver hint. Should be consumed
    // without any warning in any mode.
    let abc = "X:1\nT:Test\nK:C\nCDEyFGA|\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(
        no_errors(&result),
        "y should be silent, got: {:?}",
        result.feedback,
    );
    assert!(
        !warnings_contain(&result, "unknown") && !warnings_contain(&result, "Unrecognized"),
        "y should not warn, got: {:?}",
        result.feedback,
    );

    // y2 form (with count) — also a no-op
    let abc = "X:1\nT:Test\nK:C\nCDEy2FGA|\n";
    let result = parse_with_mode(abc, ParseMode::Strict);
    assert!(no_errors(&result), "y2 should be silent, got: {:?}", result.feedback);
}

#[test]
fn fragment_still_warns_on_unrecognized_construct() {
    // A fragment containing a construct the parser doesn't know
    // (`j` is not a note, decoration, field, or reserved char). Fragment
    // mode is about "no header required", not "any byte is fine".
    let abc = "CDEjFGA";
    let result = parse_with_mode(abc, ParseMode::Fragment);
    assert!(no_errors(&result));
    assert!(warnings_contain(&result, "Skipping unknown character"));
}
