//! Diagnostic runner over ABC v2.1 spec-derived fixtures.
//!
//! Fixtures live under `tests/fixtures/spec/<section-slug>/NN.abc` with a
//! sibling `NN.md` holding the spec excerpt that produced them. This runner
//! tries to `parse()` each one and reports the landscape — it does NOT
//! assert correctness yet. Use it to find sections to focus on, then add
//! targeted tests with real assertions as the parser improves.
//!
//! # Source attribution
//!
//! Fixtures consumed here are reproduced verbatim from the **ABC v2.1
//! music standard** by Chris Walshaw, published at
//! <https://abcnotation.com/wiki/abc:standard:v2.1> and cached locally at
//! `crates/kaijutsu-abc/docs/abc-spec-cache.md`. That content is licensed
//! under CC BY-NC-SA 3.0; see `tests/fixtures/spec/README.md` for full
//! provenance and license details. This test runner itself is not
//! derivative of the spec.

use kaijutsu_abc::{parse_with_mode, ParseMode};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Regression baseline. Ratchets *down* as parser gaps are filled — never
/// up. If a change drives a bound below the baseline, lower the baseline
/// in the same commit so it stays honest.
///
/// History:
///   3001 — initial extraction after ParseMode split
///   1946 — fixture curation (24 non-ABC pseudo-syntax fixtures pruned)
///    650 — w:/W: lyric line recognition (§5)
///    637 — s: symbol line recognition (§4.15)
///    417 — +: continuation lines folded into preceding field (§3.3)
///    358 — broken rhythm < and > operators per §4.4
///    354 — voice overlay & marker per §7.4
///    351 — alternate decoration syntax +f+ per §4.14
const MAX_TOTAL_WARNINGS: usize = 351;
const MAX_TOTAL_ERRORS: usize = 0;

struct Outcome {
    section: String,
    name: String,
    parsed_clean: bool,
    error_count: usize,
    warning_count: usize,
    first_error: Option<String>,
    warning_messages: Vec<String>,
    raw_warnings: Vec<String>,
}

/// Strip the variable bits out of a warning message so identical-shape
/// warnings collapse into one pattern. e.g.
///   "Skipping unknown character '!'" -> "Skipping unknown character '_'"
///   "Invalid key root 'Q', assuming C" -> "Invalid key root '_', assuming C"
fn normalize_message(msg: &str) -> String {
    // Replace anything inside single-quotes with '_'
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            out.push('\'');
            out.push('_');
            for inner in chars.by_ref() {
                if inner == '\'' {
                    out.push('\'');
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("spec")
}

fn collect_fixtures() -> Vec<(String, PathBuf)> {
    let root = fixture_root();
    let mut out = Vec::new();
    let Ok(sections) = fs::read_dir(&root) else {
        return out;
    };
    let mut sections: Vec<_> = sections.flatten().collect();
    sections.sort_by_key(|d| d.file_name());
    for section in sections {
        if !section.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let section_name = section.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(section.path()) else {
            continue;
        };
        let mut files: Vec<_> = files
            .flatten()
            .filter(|f| f.path().extension().map_or(false, |e| e == "abc"))
            .collect();
        files.sort_by_key(|f| f.file_name());
        for file in files {
            out.push((section_name.clone(), file.path()));
        }
    }
    out
}

fn run() -> Vec<Outcome> {
    collect_fixtures()
        .into_iter()
        .map(|(section, path)| {
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let content = fs::read_to_string(&path).expect("read fixture");
            // Spec examples are fragments per §2.3 — most have no header,
            // and the ones that do still parse correctly in Fragment mode.
            let result = parse_with_mode(&content, ParseMode::Fragment);
            let errors: Vec<_> = result
                .feedback
                .iter()
                .filter(|f| {
                    matches!(f.level, kaijutsu_abc::FeedbackLevel::Error)
                })
                .collect();
            let warning_messages: Vec<String> = result
                .feedback
                .iter()
                .filter(|f| {
                    matches!(f.level, kaijutsu_abc::FeedbackLevel::Warning)
                })
                .map(|f| normalize_message(&f.message))
                .collect();
            let raw_warnings: Vec<String> = result
                .feedback
                .iter()
                .filter(|f| {
                    matches!(f.level, kaijutsu_abc::FeedbackLevel::Warning)
                })
                .map(|f| f.message.clone())
                .collect();
            Outcome {
                section,
                name,
                parsed_clean: errors.is_empty(),
                error_count: errors.len(),
                warning_count: warning_messages.len(),
                first_error: errors.first().map(|e| {
                    format!("L{}:C{} {}", e.line, e.column, e.message)
                }),
                warning_messages,
                raw_warnings,
            }
        })
        .collect()
}

#[test]
fn spec_fixture_landscape() {
    let outcomes = run();
    if outcomes.is_empty() {
        panic!("no spec fixtures found under tests/fixtures/spec — did the extractor run?");
    }

    let mut by_section: BTreeMap<&str, Vec<&Outcome>> = BTreeMap::new();
    for o in &outcomes {
        by_section.entry(&o.section).or_default().push(o);
    }

    let mut total_clean = 0;
    let mut total = 0;

    println!("\n=== ABC v2.1 spec fixture landscape ===\n");
    for (section, items) in &by_section {
        let clean = items.iter().filter(|o| o.parsed_clean).count();
        total_clean += clean;
        total += items.len();
        println!("§ {} — {}/{} clean", section, clean, items.len());
        for o in items {
            let status = if o.parsed_clean { "ok  " } else { "FAIL" };
            let mut tags = Vec::new();
            if o.error_count > 1 {
                tags.push(format!("{} errs", o.error_count));
            }
            if o.warning_count > 0 {
                tags.push(format!("{} warn", o.warning_count));
            }
            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", tags.join(", "))
            };
            println!(
                "    [{}] {}{}{}",
                status,
                o.name,
                tag_str,
                o.first_error
                    .as_ref()
                    .map(|e| format!("  — {}", e))
                    .unwrap_or_default()
            );
        }
        println!();
    }

    println!("=== summary: {}/{} fixtures parse without errors ===", total_clean, total);

    // Aggregate warning patterns across all fixtures so we can see what the
    // parser is actually grumbling about, not just how often.
    let mut pattern_counts: BTreeMap<String, usize> = BTreeMap::new();
    for o in &outcomes {
        for msg in &o.warning_messages {
            *pattern_counts.entry(msg.clone()).or_default() += 1;
        }
    }
    let mut patterns: Vec<_> = pattern_counts.into_iter().collect();
    patterns.sort_by(|a, b| b.1.cmp(&a.1));
    let total_warnings: usize = patterns.iter().map(|(_, n)| *n).sum();

    println!(
        "\n=== top warning patterns ({} total, {} distinct) ===\n",
        total_warnings,
        patterns.len()
    );
    for (msg, count) in patterns.iter().take(25) {
        println!("  {:>6}×  {}", count, msg);
    }
    if patterns.len() > 25 {
        let tail: usize = patterns.iter().skip(25).map(|(_, n)| *n).sum();
        println!("  {:>6}×  (… {} more distinct patterns)", tail, patterns.len() - 25);
    }

    // Per-character breakdown of the "Skipping unknown character" fallback —
    // this is what tells us which constructs are most worth implementing.
    let mut char_counts: BTreeMap<char, usize> = BTreeMap::new();
    for o in &outcomes {
        for msg in &o.raw_warnings {
            if let Some(rest) = msg.strip_prefix("Skipping unknown character '") {
                if let Some(c) = rest.chars().next() {
                    *char_counts.entry(c).or_default() += 1;
                }
            }
        }
    }
    let mut chars: Vec<_> = char_counts.into_iter().collect();
    chars.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n=== unknown chars hitting the body.rs fallback ===\n");
    for (c, n) in chars.iter().take(15) {
        let codepoint = format!("U+{:04X}", *c as u32);
        println!("  {:>6}×  '{}'  ({})", n, c, codepoint);
    }

    // Regression rail: tally totals and assert they don't exceed the
    // captured baseline. Lower the constants whenever real progress is
    // made — this is a ratchet, not a soft target.
    let total_errors: usize = outcomes.iter().map(|o| o.error_count).sum();
    let total_warns: usize = outcomes.iter().map(|o| o.warning_count).sum();
    println!(
        "\n=== regression rail: warnings={}/{} errors={}/{} ===",
        total_warns, MAX_TOTAL_WARNINGS, total_errors, MAX_TOTAL_ERRORS
    );

    assert!(
        total_errors <= MAX_TOTAL_ERRORS,
        "Spec fixtures emitted {} errors (baseline {}). Either fix the regression \
         or, if this is intentional, raise MAX_TOTAL_ERRORS with justification.",
        total_errors,
        MAX_TOTAL_ERRORS,
    );
    assert!(
        total_warns <= MAX_TOTAL_WARNINGS,
        "Spec fixtures emitted {} warnings (baseline {}). Either fix the regression \
         or, if this is intentional, raise MAX_TOTAL_WARNINGS with justification.",
        total_warns,
        MAX_TOTAL_WARNINGS,
    );

    // If totals are well below the baseline, suggest lowering it so the
    // rail keeps biting. (10% slack for noise.)
    if total_warns + total_warns / 10 < MAX_TOTAL_WARNINGS {
        println!(
            "  hint: warnings ({}) are well under baseline ({}); consider \
             lowering MAX_TOTAL_WARNINGS",
            total_warns, MAX_TOTAL_WARNINGS,
        );
    }
}
