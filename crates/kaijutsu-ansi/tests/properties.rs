//! Properties of the `ansi-strip` projection.
//!
//! The chunk-boundary property is the one that catches *our* bugs: kaish output
//! arrives in flushes and escape sequences straddle them, so "split anywhere ≡
//! one shot" is the contract the kernel's background-exec hook leans on
//! (`docs/ansi-and-beyond.md`, "Test ladder").

use kaijutsu_ansi::{StripParser, strip};
use kaijutsu_types::StyleSpan;

/// Inputs chosen to put something interesting on *every* byte boundary:
/// sequences that straddle, parameters that split, codepoints that split.
fn nasty_inputs() -> Vec<Vec<u8>> {
    let mut inputs: Vec<Vec<u8>> = vec![
        // Plain and near-plain.
        b"".to_vec(),
        b"hello world".to_vec(),
        b"line one\nline two\ttabbed\r\n".to_vec(),
        // SGR straddling every position.
        b"a\x1b[31mred\x1b[0mb".to_vec(),
        b"\x1b[1;4;38;5;208mfancy\x1b[0m".to_vec(),
        // Truecolor parameters — the longest parameter run we care about.
        b"\x1b[38;2;255;128;64mtruecolor\x1b[39m tail".to_vec(),
        b"\x1b[48:2::10:20:30msubparams\x1b[m".to_vec(),
        // OSC in both terminations, including a payload that looks like text.
        b"pre\x1b]0;a title with spaces\x07post".to_vec(),
        b"pre\x1b]8;;https://example.invalid/x\x1b\\link\x1b]8;;\x1b\\post".to_vec(),
        b"\x1b]52;c;YmFzZTY0\x07clip".to_vec(),
        // DCS / APC bodies.
        b"a\x1bP1$r0m\x1b\\b\x1b_apc body\x1b\\c".to_vec(),
        // Cursor / erase noise interleaved with text.
        b"\x1b[2J\x1b[H\x1b[10;20Hx\x1b[K\x1b[1A\x1b[?25ly".to_vec(),
        // Malformed / truncated tails.
        b"trailing esc\x1b".to_vec(),
        b"truncated csi \x1b[38;5".to_vec(),
        b"open osc \x1b]0;never terminated".to_vec(),
        b"\x1b[".to_vec(),
        b"\x1b[;;;;m?".to_vec(),
        // Invalid UTF-8 mixed with valid text.
        b"good \xff\xfe bad \x1b[32mgreen\x1b[0m".to_vec(),
        b"\xe6\x97".to_vec(), // truncated three-byte codepoint at EOF
    ];

    // Multi-byte UTF-8 that must survive a split at every internal byte.
    inputs.push("\x1b[32mgrün 日本語 🎺 combining é\x1b[0m tail".as_bytes().to_vec());
    // A styled multi-byte run bracketed by sequences on both sides.
    inputs.push("\x1b[1;38;2;1;2;3m🎺🎺🎺\x1b[0m🎺".as_bytes().to_vec());
    // Something long enough to exercise coalescing across many runs.
    let mut striped = Vec::new();
    for i in 0..64u16 {
        striped.extend_from_slice(format!("\x1b[{}mcell{i} ", 30 + (i % 8)).as_bytes());
    }
    striped.extend_from_slice(b"\x1b[0m\n");
    inputs.push(striped);

    inputs
}

fn feed_splits(input: &[u8], cuts: &[usize]) -> (String, Vec<StyleSpan>) {
    let mut parser = StripParser::new();
    let mut prev = 0;
    for &cut in cuts {
        parser.feed(&input[prev..cut]);
        prev = cut;
    }
    parser.feed(&input[prev..]);
    parser.finish()
}

#[test]
fn split_at_every_position_equals_one_shot() {
    for input in nasty_inputs() {
        let expected = strip(&input);
        for cut in 0..=input.len() {
            let got = feed_splits(&input, &[cut]);
            assert_eq!(
                got.0, expected.0,
                "text differs splitting at {cut} of {input:?}"
            );
            assert_eq!(
                got.1, expected.1,
                "spans differ splitting at {cut} of {input:?}"
            );
        }
    }
}

#[test]
fn split_at_every_pair_of_positions_equals_one_shot() {
    // Two cuts catch state that survives one boundary but not two — a
    // parameter split across three chunks, a codepoint split into thirds.
    for input in nasty_inputs().into_iter().filter(|i| i.len() <= 64) {
        let expected = strip(&input);
        for a in 0..=input.len() {
            for b in a..=input.len() {
                let got = feed_splits(&input, &[a, b]);
                assert_eq!(got.0, expected.0, "text differs splitting at {a},{b} of {input:?}");
                assert_eq!(got.1, expected.1, "spans differ splitting at {a},{b} of {input:?}");
            }
        }
    }
}

#[test]
fn one_byte_at_a_time_equals_one_shot() {
    for input in nasty_inputs() {
        let expected = strip(&input);
        let mut parser = StripParser::new();
        for byte in &input {
            parser.feed(std::slice::from_ref(byte));
        }
        assert_eq!(parser.finish(), expected, "byte-at-a-time differs for {input:?}");
    }
}

/// Deterministic 64-bit xorshift — a fixed seed makes the "random" splits a
/// reproducible test, not a flaky one.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 { 0 } else { (self.next_u64() % bound as u64) as usize }
    }
}

#[test]
fn randomized_splits_equal_one_shot() {
    let mut rng = Rng(0x5EED_1234_ABCD_0F0F);
    for input in nasty_inputs() {
        let expected = strip(&input);
        for round in 0..64 {
            let mut cuts: Vec<usize> = (0..rng.below(6) + 1)
                .map(|_| rng.below(input.len() + 1))
                .collect();
            cuts.sort_unstable();
            let got = feed_splits(&input, &cuts);
            assert_eq!(got.0, expected.0, "text differs, round {round}, cuts {cuts:?}, {input:?}");
            assert_eq!(got.1, expected.1, "spans differ, round {round}, cuts {cuts:?}, {input:?}");
        }
    }
}

#[test]
fn stripping_is_idempotent() {
    for input in nasty_inputs() {
        let (text, _) = strip(&input);
        let (again, spans) = strip(text.as_bytes());
        assert_eq!(again, text, "not idempotent for {input:?}");
        // Second pass has no escape sequences left to interpret, so the
        // projection is unstyled by construction.
        assert!(spans.is_empty(), "second pass invented spans for {input:?}");
    }
}

#[test]
fn output_never_contains_escape_bytes() {
    for input in nasty_inputs() {
        let (text, _) = strip(&input);
        assert!(!text.contains('\u{1b}'), "escape leaked for {input:?}");
        for c in text.chars() {
            let ok = c == '\n' || c == '\t' || c == '\r' || !c.is_control();
            assert!(ok, "control {c:?} survived for {input:?}");
        }
    }
}

#[test]
fn spans_are_sorted_disjoint_and_char_aligned() {
    for input in nasty_inputs() {
        let (text, spans) = strip(&input);
        let mut prev_end = 0u32;
        for span in &spans {
            assert!(span.start < span.end, "empty or inverted span {span:?} for {input:?}");
            assert!(span.start >= prev_end, "spans overlap or unsorted for {input:?}");
            assert!(
                (span.end as usize) <= text.len(),
                "span past end of text for {input:?}"
            );
            assert!(
                text.is_char_boundary(span.start as usize),
                "start off a char boundary for {input:?}"
            );
            assert!(
                text.is_char_boundary(span.end as usize),
                "end off a char boundary for {input:?}"
            );
            prev_end = span.end;
        }
    }
}

#[test]
fn no_span_is_default_styled_and_neighbours_differ() {
    for input in nasty_inputs() {
        let (_, spans) = strip(&input);
        for span in &spans {
            assert!(
                span.fg.is_some() || span.bg.is_some() || !span.attrs.is_empty(),
                "default-styled span emitted for {input:?}"
            );
        }
        for pair in spans.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let identical = a.fg == b.fg && a.bg == b.bg && a.attrs == b.attrs;
            assert!(
                !(a.end == b.start && identical),
                "adjacent identical spans not coalesced for {input:?}"
            );
        }
    }
}

/// The stripped text is the input minus recognized sequences: on well-formed
/// SGR-only input, a naive "drop CSI ... final-byte" stripper must agree.
#[test]
fn differential_against_a_naive_sgr_stripper() {
    let cases: &[&str] = &[
        "plain",
        "\x1b[31mred\x1b[0m",
        "a\x1b[1;32mb\x1b[0mc\x1b[38;5;9md\x1b[m",
        "\x1b[38;2;1;2;3mrgb\x1b[0m\n\x1b[4munder\x1b[24m done\n",
        "日本語\x1b[33m🎺\x1b[0m",
    ];
    for case in cases {
        let (text, _) = strip(case.as_bytes());
        assert_eq!(text, naive_strip(case), "differential mismatch for {case:?}");
    }
}

/// Deliberately dumb reference: skip `ESC [`, then bytes until a byte in
/// `0x40..=0x7e`. Only valid for the well-formed CSI-only corpus above.
fn naive_strip(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[test]
fn malformed_input_is_total() {
    let cases: Vec<Vec<u8>> = vec![
        b"\x1b".to_vec(),
        b"\x1b[".to_vec(),
        b"\x1b[38".to_vec(),
        b"\x1b[38;".to_vec(),
        b"\x1b[38;2;".to_vec(),
        b"\x1b[38;2;1;2".to_vec(),
        b"\x1b]".to_vec(),
        b"\x1bP".to_vec(),
        b"\x1b_".to_vec(),
        b"\x1b[999999999999m!".to_vec(),
        b"\x1b[38;5;99999m!".to_vec(),
        b"\x1b[-1;;;m!".to_vec(),
        b"\x1b[38;2;300;400;500mx".to_vec(),
        vec![0xff; 64],
        (0u8..=255).collect(),
        (0u8..=255).rev().collect(),
    ];
    for case in cases {
        // The property is simply that this returns.
        let (text, spans) = strip(&case);
        for span in &spans {
            assert!(span.start < span.end);
            assert!(text.is_char_boundary(span.start as usize));
            assert!(text.is_char_boundary(span.end as usize));
        }
    }
}

#[test]
fn parameter_overflow_is_capped_not_fatal() {
    // vte caps parameters at 32 and sets its `ignore` flag; the prefix it did
    // parse still applies and nothing leaks into the text.
    let mut input = b"\x1b[".to_vec();
    input.extend(std::iter::repeat_n(b"1;".as_slice(), 200).flatten().copied());
    input.extend_from_slice(b"31mx");
    let (text, spans) = strip(&input);
    assert_eq!(text, "x");
    // Bold arrived in the parsed prefix; the trailing red was cut off by the
    // cap. Either way: exactly one span over "x", never a text leak.
    assert_eq!(spans.len(), 1);
    assert_eq!((spans[0].start, spans[0].end), (0, 1));
    assert!(spans[0].attrs.contains(kaijutsu_types::StyleAttrs::BOLD));
}

#[test]
fn a_realistic_ls_color_line_projects_cleanly() {
    // Shape of GNU `ls --color` output, including the reset-before-newline
    // habit and a bright directory color.
    let input = b"\x1b[0m\x1b[01;34mcrates\x1b[0m  \x1b[01;32mrun.sh\x1b[0m  README.md\n";
    let (text, spans) = strip(input);
    assert_eq!(text, "crates  run.sh  README.md\n");
    assert_eq!(spans.len(), 2);
    assert_eq!((spans[0].start, spans[0].end), (0, 6));
    assert_eq!((spans[1].start, spans[1].end), (8, 14));
    assert!(spans.iter().all(|s| s.attrs.contains(kaijutsu_types::StyleAttrs::BOLD)));
}

#[test]
fn a_realistic_cargo_progress_line_keeps_carriage_returns() {
    // `\r`-overwrite progress: preserve, don't render (open question in the
    // design doc). Every frame stays; the cursor codes do not.
    let input = b"\r\x1b[K   Compiling kaijutsu-ansi v0.1.0\r\x1b[K    Finished dev\n";
    let (text, spans) = strip(input);
    assert_eq!(text, "\r   Compiling kaijutsu-ansi v0.1.0\r    Finished dev\n");
    assert!(spans.is_empty());
}
