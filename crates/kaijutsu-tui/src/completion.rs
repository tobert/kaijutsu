//! `:kj ` completion: once `kj` is typed in the `:` bar (`compose.rs`),
//! `Tab` completes over the `kj` command catalog
//! (`kaijutsu_client::rpc::KjCommandInfo`, `get_kj_command_catalog`).
//!
//! Pure: [`complete`] takes the text typed after `:kj ` and the catalog and
//! returns candidates; nothing here reaches the kernel or draws a cell. The
//! catalog itself is one RPC round trip the bridge fetches and caches at
//! connect (`docs/tui.md`, "What is reused, what is new" — the
//! `KjCommandInfo` catalog every client reads the same way); `Tab` drives
//! [`KjCompletion`] from `run.rs`, reading `Compose::kj_typed`.

use kaijutsu_client::rpc::KjCommandInfo;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::present::Palette;

/// Maximum number of candidate rows shown in the `:kj ` popup at once.
const MAX_POPUP_ROWS: usize = 6;

/// One completion candidate: the bare command name and its one-line help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub help: String,
}

/// The candidates a `:kj ` prefix matches, plus which one `Tab` currently
/// has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KjCompletion {
    /// The text typed after `:kj `, before any completion — what every
    /// candidate's name starts with.
    pub prefix: String,
    pub candidates: Vec<Candidate>,
    selected: usize,
}

impl KjCompletion {
    /// The currently selected candidate, or `None` when nothing matched.
    pub fn current(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// Index of the currently selected candidate.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Advance to the next candidate, wrapping — repeated `Tab` cycles the
    /// selection instead of leaving it stuck on the first match.
    pub fn cycle(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
    }
}

/// Render the `:kj ` completion popup lines (drawn in the overlay band right
/// above the compose bar). Empty when no candidates matched or width is zero.
pub fn popup_lines(completion: &KjCompletion, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let total = completion.candidates.len();
    if total == 0 || width == 0 {
        return Vec::new();
    }
    let width = usize::from(width);
    let visible = total.min(MAX_POPUP_ROWS);
    let start = if completion.selected + 1 > visible {
        (completion.selected + 1 - visible).min(total - visible)
    } else {
        0
    };
    let slice = &completion.candidates[start..start + visible];
    let name_col = slice
        .iter()
        .map(|c| c.name.width())
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(4));

    let mut lines = Vec::with_capacity(visible + 1);
    let header = if total == 1 {
        ":kj (1/1)".to_string()
    } else {
        format!(":kj ({}/{})  Tab cycles", completion.selected + 1, total)
    };
    lines.push(Line::from(Span::styled(
        truncate_display(&header, width),
        palette.divider(),
    )));

    for (offset, cand) in slice.iter().enumerate() {
        let idx = start + offset;
        let is_selected = idx == completion.selected;
        let marker = if is_selected { "▸ " } else { "  " };
        let padded_name = format!("{:<w$}", cand.name, w = name_col);
        let prefix = format!("{marker}{padded_name}");
        let prefix_w = prefix.width();
        let help_budget = width.saturating_sub(prefix_w + 2);
        let help = if help_budget > 0 && !cand.help.is_empty() {
            format!("  {}", truncate_display(&cand.help, help_budget))
        } else {
            String::new()
        };

        let name_style = if is_selected {
            palette.status_active()
        } else {
            palette.compose()
        };
        let help_style = if is_selected {
            palette.copy_selection()
        } else {
            palette.divider()
        };
        lines.push(Line::from(vec![
            Span::styled(truncate_display(&prefix, width), name_style),
            Span::styled(help, help_style),
        ]));
    }
    lines
}

fn truncate_display(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Compute the candidates for `typed` — the text after `:kj ` in the bar
/// (`Compose::kj_typed`) — or `None` when a space has already ended the
/// verb position: completion only ever proposes the verb itself, never its
/// arguments.
pub fn complete(typed: &str, catalog: &[KjCommandInfo]) -> Option<KjCompletion> {
    if typed.contains(' ') || typed.contains('\t') {
        return None;
    }
    let mut candidates: Vec<Candidate> = catalog
        .iter()
        .filter(|c| c.name.starts_with(typed))
        .map(|c| Candidate { name: c.name.clone(), help: c.description.clone() })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    Some(KjCompletion { prefix: typed.to_string(), candidates, selected: 0 })
}

/// The bar body after accepting `candidate` (no `:` prefix — the seam
/// `Compose::set_command_body` writes into): `kj name ` with the cursor left
/// to type the command's arguments, or a plain `kj name` with no trailing
/// space when the command takes none (`argv_prefix` is the catalog's own
/// invocation shape; a command with no separate input, e.g. a bare `kj
/// status`, has an empty `input_hint`).
pub fn accept(name: &str, input_hint_is_empty: bool) -> String {
    if input_hint_is_empty {
        format!("kj {name}")
    } else {
        format!("kj {name} ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<KjCommandInfo> {
        vec![
            KjCommandInfo {
                name: "stage".to_string(),
                description: "stage a block for exclude/include".to_string(),
                input_hint: "<verb>".to_string(),
                argv_prefix: vec!["stage".to_string()],
            },
            KjCommandInfo {
                name: "status".to_string(),
                description: "show connection and context status".to_string(),
                input_hint: String::new(),
                argv_prefix: vec!["status".to_string()],
            },
            KjCommandInfo {
                name: "ledger".to_string(),
                description: "answer pending approval-ledger asks".to_string(),
                input_hint: "<verb>".to_string(),
                argv_prefix: vec!["ledger".to_string()],
            },
        ]
    }

    #[test]
    fn a_space_after_the_verb_ends_completion() {
        assert_eq!(complete("stage ex", &catalog()), None);
    }

    #[test]
    fn an_empty_prefix_lists_the_whole_catalog_sorted() {
        let completion = complete("", &catalog()).expect("some completion");
        let names: Vec<&str> = completion.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ledger", "stage", "status"]);
    }

    #[test]
    fn a_prefix_narrows_to_matching_names() {
        let completion = complete("st", &catalog()).expect("some completion");
        let names: Vec<&str> = completion.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["stage", "status"]);
    }

    #[test]
    fn an_unmatched_prefix_yields_no_candidates_but_still_completes() {
        let completion = complete("zzz", &catalog()).expect("some completion");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn cycle_wraps_around_the_candidate_list() {
        let mut completion = complete("", &catalog()).expect("some completion");
        assert_eq!(completion.current().unwrap().name, "ledger");
        completion.cycle();
        assert_eq!(completion.current().unwrap().name, "stage");
        completion.cycle();
        assert_eq!(completion.current().unwrap().name, "status");
        completion.cycle();
        assert_eq!(completion.current().unwrap().name, "ledger", "cycle wraps");
    }

    #[test]
    fn accept_appends_a_trailing_space_only_when_the_command_takes_input() {
        assert_eq!(accept("stage", false), "kj stage ");
        assert_eq!(accept("status", true), "kj status");
    }

    #[test]
    fn popup_lines_renders_header_and_highlights_selected_candidate() {
        let mut completion = complete("", &catalog()).expect("some completion");
        let palette = Palette::tokyo_midnight();
        let lines = popup_lines(&completion, 60, &palette);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text.len(), 4, "header + 3 candidates");
        assert!(text[0].contains("(1/3)"));
        assert!(text[1].starts_with("▸ ledger"));
        assert!(text[2].starts_with("  stage"));

        completion.cycle();
        let lines2 = popup_lines(&completion, 60, &palette);
        let text2: Vec<String> = lines2
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text2[0].contains("(2/3)"));
        assert!(text2[2].starts_with("▸ stage"));
    }

    #[test]
    fn popup_lines_windows_long_catalog_around_selection() {
        let many: Vec<KjCommandInfo> = (0..10)
            .map(|i| KjCommandInfo {
                name: format!("cmd{i:02}"),
                description: format!("help {i}"),
                input_hint: String::new(),
                argv_prefix: vec![format!("cmd{i:02}")],
            })
            .collect();
        let mut completion = complete("", &many).expect("some completion");
        for _ in 0..8 {
            completion.cycle();
        }
        assert_eq!(completion.selected_index(), 8);
        let lines = popup_lines(&completion, 40, &Palette::tokyo_midnight());
        assert_eq!(lines.len(), MAX_POPUP_ROWS + 1);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(joined[0].contains("(9/10)"));
        assert!(joined.iter().any(|r| r.starts_with("▸ cmd08")));
    }

    #[test]
    fn popup_lines_is_empty_when_nothing_matched() {
        let completion = complete("zzz", &catalog()).expect("some completion");
        assert!(popup_lines(&completion, 60, &Palette::tokyo_midnight()).is_empty());
    }
}
