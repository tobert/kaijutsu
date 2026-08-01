//! The three roundtrip properties that define the dialect.
//!
//! 1. `parse ∘ format == id` on models.
//! 2. `format ∘ parse == id` on canonical text.
//! 3. Arbitrary dialect-valid text canonicalizes — `format ∘ parse` is a fixed
//!    point after one application.
//!
//! The generators lean adversarial on purpose: file *content* includes lines
//! that look like diff syntax (`--- a/x`, `@@ -1 +1 @@`, `\ No newline at end
//! of file`, `diff --git a/x b/x`), because the only thing standing between
//! those and a mis-parse is the hunk's declared line counts. Paths include
//! spaces, quotes, backslashes, tabs, non-ASCII, and a literal `a/` prefix.

use kaijutsu_diff::{
    DiffModel, DiffOptions, FileChange, FileSpec, diff, format, parse, truncate_to_bytes,
};
use proptest::prelude::*;

// ── generators ──────────────────────────────────────────────────────────────

fn path_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("src/main.rs".to_string()),
        Just("two words.txt".to_string()),
        Just("say \"hi\".md".to_string()),
        Just("back\\slash".to_string()),
        Just("tab\there".to_string()),
        Just("docs/設計.md".to_string()),
        // A path whose own first component collides with the `a/` header
        // prefix: stripping must happen exactly once.
        Just("a/foo".to_string()),
        Just("b/bar".to_string()),
        "[a-z]{1,6}(/[a-z]{1,6}){0,2}",
    ]
}

fn line_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just(" ".to_string()),
        Just("\t".to_string()),
        Just("-not a deletion".to_string()),
        Just("+not an insertion".to_string()),
        Just("\\ No newline at end of file".to_string()),
        Just("@@ -1 +1 @@".to_string()),
        Just("--- a/x".to_string()),
        Just("+++ b/x".to_string()),
        Just("diff --git a/x b/x".to_string()),
        Just("#!kaijutsu-diff truncated: 1 file(s), 1 hunk(s), 1 line(s) omitted".to_string()),
        // Bare CRs: normalization turns these into line breaks, so they must
        // never reach a model. Running them through every roundtrip property
        // is stronger than asserting it once.
        Just("cr\rinside".to_string()),
        Just("trailing\r".to_string()),
        "[a-z ()=;{}]{0,24}",
        "[\\p{Hiragana}\\p{Han}]{0,8}",
    ]
}

fn content_strategy() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec(line_strategy(), 0..10),
        any::<bool>(),
    )
        .prop_map(|(lines, trailing_newline)| {
            if lines.is_empty() {
                return String::new();
            }
            let mut text = lines.join("\n");
            if trailing_newline {
                text.push('\n');
            }
            text
        })
}

fn change_strategy() -> impl Strategy<Value = FileChange> {
    prop_oneof![
        Just(FileChange::Modified),
        Just(FileChange::Added),
        Just(FileChange::Deleted),
        Just(FileChange::Renamed),
    ]
}

/// One file's inputs, kept owned so the borrowed [`FileSpec`]s can be built
/// against them inside the property body.
type FileInput = (String, String, FileChange, String, String);

fn file_input_strategy() -> impl Strategy<Value = FileInput> {
    (
        path_strategy(),
        path_strategy(),
        change_strategy(),
        content_strategy(),
        content_strategy(),
    )
}

fn model_strategy() -> impl Strategy<Value = Vec<FileInput>> {
    proptest::collection::vec(file_input_strategy(), 0..4)
}

/// Build a model, or `None` when the generator happened to produce a
/// terminator-only change — which `diff_file` refuses by design rather than
/// returning an empty diff. Every other error is a real failure.
fn build(inputs: &[FileInput]) -> Option<DiffModel> {
    let specs: Vec<FileSpec<'_>> = inputs
        .iter()
        .map(|(old, new, change, before, after)| match change {
            // The constructors enforce the invariants the change kinds carry:
            // an added file has no pre-image, a deleted file no post-image, and
            // a non-rename has one path, not two.
            FileChange::Modified => FileSpec::modified(new, before, after),
            FileChange::Added => FileSpec::added(new, after),
            FileChange::Deleted => FileSpec::deleted(new, before),
            FileChange::Renamed => FileSpec::renamed(old, new, before, after),
        })
        .collect();
    match diff(&specs, &DiffOptions::default()) {
        Ok(model) => Some(model),
        Err(kaijutsu_diff::DiffError::LineEndingsOnly { .. }) => None,
        Err(e) => panic!("unexpected generation failure: {e}"),
    }
}

/// No `\r` may survive into any line of any model — the claim the crate docs
/// make, checked on every generated model rather than on a handful of cases.
fn assert_no_carriage_returns(model: &DiffModel) {
    for file in &model.files {
        for hunk in &file.hunks {
            for line in &hunk.lines {
                assert!(
                    !line.text.contains('\r'),
                    "CR survived into {:?}",
                    line.text
                );
            }
        }
    }
}

// ── the properties ──────────────────────────────────────────────────────────

proptest! {
    /// Property 1: formatting a model and parsing it back yields the same model.
    ///
    /// This is the one that makes plain unified text a lossless carrier for the
    /// viewer: word spans and line numbers are *derived*, so parse recomputes
    /// them rather than reading them, and must land on the same answer.
    #[test]
    fn parse_after_format_is_identity_on_models(inputs in model_strategy()) {
        let model = build(&inputs);
        prop_assume!(model.is_some());
        let model = model.unwrap();
        assert_no_carriage_returns(&model);
        let text = format(&model);
        let reparsed = parse(&text).unwrap_or_else(|e| panic!("{e}\n--- text ---\n{text}"));
        assert_no_carriage_returns(&reparsed);
        prop_assert_eq!(reparsed, model);
    }

    /// Property 2: canonical text survives a parse/format round trip byte for byte.
    #[test]
    fn format_after_parse_is_identity_on_canonical_text(inputs in model_strategy()) {
        let model = build(&inputs);
        prop_assume!(model.is_some());
        let text = format(&model.unwrap());
        let round = format(&parse(&text).unwrap());
        prop_assert_eq!(round, text);
    }

    /// Property 3: dialect-valid but non-canonical text canonicalizes to a
    /// fixed point — one pass through the crate is enough, forever.
    #[test]
    fn external_text_canonicalizes_to_a_fixed_point(
        inputs in model_strategy(),
        variant in 0usize..4,
    ) {
        let model = build(&inputs);
        prop_assume!(model.is_some());
        let canonical = format(&model.unwrap());
        let external = decanonicalize(&canonical, variant);
        let once = format(&parse(&external).unwrap_or_else(|e| {
            panic!("variant {variant}: {e}\n--- text ---\n{external}")
        }));
        let twice = format(&parse(&once).unwrap());
        prop_assert_eq!(once, twice);
    }

    /// The lossless de-canonicalizations must also preserve the *model*, not
    /// merely reach a fixed point. (Variant 2 drops `diff --git` and rename
    /// headers, which is genuinely lossy for a rename onto an identical path,
    /// so it is excluded here and covered by the fixed-point property above.)
    #[test]
    fn lossless_external_variants_preserve_the_model(
        inputs in model_strategy(),
        variant in prop_oneof![Just(0usize), Just(1), Just(3)],
    ) {
        let model = build(&inputs);
        prop_assume!(model.is_some());
        let model = model.unwrap();
        let external = decanonicalize(&format(&model), variant);
        prop_assert_eq!(parse(&external).unwrap(), model);
    }

    /// Truncation never produces something that parses as a complete patch, and
    /// never exceeds its budget once the budget can hold the marker at all.
    #[test]
    fn truncation_stays_within_budget_and_admits_it(
        inputs in model_strategy(),
        budget in 256usize..4096,
    ) {
        let model = build(&inputs);
        prop_assume!(model.is_some());
        let model = model.unwrap();
        let cut = truncate_to_bytes(&model, budget);
        let text = format(&cut);
        prop_assert!(text.len() <= budget, "{} bytes over a {budget} budget", text.len());
        // Whatever came out is still valid, still parses, and still says so.
        let reparsed = parse(&text).unwrap_or_else(|e| panic!("{e}\n--- text ---\n{text}"));
        prop_assert_eq!(&reparsed, &cut);
        if cut.is_complete() {
            prop_assert_eq!(&cut, &model);
        } else {
            prop_assert!(text.starts_with(kaijutsu_diff::TRUNCATION_MARKER_PREFIX));
            prop_assert!(cut.line_count() < model.line_count() || cut.files.len() < model.files.len());
        }
    }
}

// ── de-canonicalization: valid dialect, non-canonical spelling ──────────────

/// Rewrite canonical text into an equally valid but non-canonical form.
///
/// Each variant mimics a real producer: git's blob-id headers, a `diff -u` with
/// no git wrapper, tools that always spell out `,1`, and mail transports that
/// eat the trailing space off an empty context line.
fn decanonicalize(text: &str, variant: usize) -> String {
    match variant {
        0 => rewrite_lines(text, |line| {
            if let Some(expanded) = expand_hunk_counts(line) {
                vec![expanded]
            } else {
                vec![line.to_string()]
            }
        }),
        1 => rewrite_lines(text, |line| {
            if line.starts_with("diff --git ") {
                vec![
                    line.to_string(),
                    "index 3b18e51..8d0e4f1 100644".to_string(),
                ]
            } else {
                vec![line.to_string()]
            }
        }),
        2 => rewrite_lines(text, |line| {
            let dropped = line.starts_with("diff --git ")
                || line.starts_with("rename from ")
                || line.starts_with("rename to ");
            if dropped {
                Vec::new()
            } else {
                vec![line.to_string()]
            }
        }),
        _ => rewrite_lines(text, |line| {
            // An empty context line is exactly `" "`; anything longer is real
            // content and must not be touched.
            vec![if line == " " {
                String::new()
            } else {
                line.to_string()
            }]
        }),
    }
}

fn rewrite_lines(text: &str, f: impl Fn(&str) -> Vec<String>) -> String {
    let Some(body) = text.strip_suffix('\n') else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    for line in body.split('\n') {
        for produced in f(line) {
            out.push_str(&produced);
            out.push('\n');
        }
    }
    out
}

/// `@@ -3 +4 @@` → `@@ -3,1 +4,1 @@`. Returns `None` for non-header lines.
///
/// Body lines are always prefixed with ` `, `+`, `-`, or `\`, so a line
/// beginning `@@ ` is unambiguously a hunk header even when the file being
/// diffed contains `@@ -1 +1 @@` as literal content.
fn expand_hunk_counts(line: &str) -> Option<String> {
    let rest = line.strip_prefix("@@ ")?;
    let (ranges, tail) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let expand = |spec: &str, sign: char| -> Option<String> {
        let spec = spec.strip_prefix(sign)?;
        Some(match spec.split_once(',') {
            Some(_) => format!("{sign}{spec}"),
            None => format!("{sign}{spec},1"),
        })
    };
    Some(format!(
        "@@ {} {} @@{tail}",
        expand(old, '-')?,
        expand(new, '+')?
    ))
}

// ── worked examples, so a failing property has company ──────────────────────

#[test]
fn expand_hunk_counts_only_touches_headers() {
    assert_eq!(
        expand_hunk_counts("@@ -3 +4 @@").unwrap(),
        "@@ -3,1 +4,1 @@"
    );
    assert_eq!(
        expand_hunk_counts("@@ -3,2 +4,5 @@ fn x()").unwrap(),
        "@@ -3,2 +4,5 @@ fn x()"
    );
    assert!(expand_hunk_counts(" @@ -1 +1 @@").is_none());
    assert!(expand_hunk_counts("+@@ -1 +1 @@").is_none());
}

#[test]
fn content_that_looks_like_diff_syntax_survives() {
    let before = "--- a/x\n@@ -1 +1 @@\ndiff --git a/x b/x\n";
    let after = "--- a/y\n@@ -1 +1 @@\ndiff --git a/x b/x\n";
    let model = build(&[(
        "f.txt".into(),
        "f.txt".into(),
        FileChange::Modified,
        before.into(),
        after.into(),
    )])
    .unwrap();
    let text = format(&model);
    assert_eq!(parse(&text).unwrap(), model);
}
