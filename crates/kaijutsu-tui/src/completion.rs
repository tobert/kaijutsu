//! Slash completion: in compose, a leading `/` completes over the `kj`
//! command catalog (`kaijutsu_client::rpc::KjCommandInfo`,
//! `get_kj_command_catalog`).
//!
//! Pure: [`complete`] takes the draft text and the catalog and returns
//! candidates; nothing here reaches the kernel or draws a cell. The catalog
//! itself is one RPC round trip the compose path fetches and caches
//! (`docs/tui.md`, "What is reused, what is new" — the `KjCommandInfo`
//! catalog every client reads the same way); `Tab` drives [`SlashState`]
//! from wherever compose text lives.

use kaijutsu_client::rpc::KjCommandInfo;
use ratatui::text::{Line, Span};

use crate::present::Palette;

/// One completion candidate: the bare command name and its one-line help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub help: String,
}

/// The candidates a draft's `/` prefix matches, plus which one `Tab`
/// currently has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCompletion {
    /// The text typed after `/`, before any completion — what every
    /// candidate's name starts with.
    pub prefix: String,
    pub candidates: Vec<Candidate>,
    selected: usize,
}

impl SlashCompletion {
    /// The currently selected candidate, or `None` when nothing matched.
    pub fn current(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// Advance to the next candidate, wrapping — repeated `Tab` cycles the
    /// popup instead of leaving it stuck on the first match.
    pub fn cycle(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }
}

/// Compute the candidates for a compose draft's leading `/`, or `None` when
/// the draft is not a slash command in progress (no leading `/`, or a space
/// already ended the command-name position — completion only ever proposes
/// the verb itself, never its arguments).
pub fn complete(draft: &str, catalog: &[KjCommandInfo]) -> Option<SlashCompletion> {
    let rest = draft.strip_prefix('/')?;
    if rest.contains(' ') || rest.contains('\t') {
        return None;
    }
    let mut candidates: Vec<Candidate> = catalog
        .iter()
        .filter(|c| c.name.starts_with(rest))
        .map(|c| Candidate { name: c.name.clone(), help: c.description.clone() })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    Some(SlashCompletion { prefix: rest.to_string(), candidates, selected: 0 })
}

/// The compose text after accepting `candidate` — `/name ` with the cursor
/// left to type the command's arguments, or a plain `/name` with no trailing
/// space when the command takes none (`argv_prefix` is the catalog's own
/// invocation shape; a command with no separate input, e.g. a bare `kj
/// status`, has an empty `input_hint`).
pub fn accept(name: &str, input_hint_is_empty: bool) -> String {
    if input_hint_is_empty {
        format!("/{name}")
    } else {
        format!("/{name} ")
    }
}

/// The popup: one line per candidate, the selected one highlighted, above
/// the compose line — `docs/tui.md`'s grown-view treatment does not name
/// this one explicitly (it rides compose, not a chord), so it stays small: a
/// handful of rows, never the whole catalog.
const MAX_ROWS: usize = 8;

pub fn render_popup(completion: &SlashCompletion, width: u16, palette: &Palette) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    if completion.candidates.is_empty() {
        return vec![Line::from(Span::styled(
            format!("  (no command matches /{})", completion.prefix),
            palette.divider(),
        ))];
    }
    completion
        .candidates
        .iter()
        .take(MAX_ROWS)
        .enumerate()
        .map(|(i, c)| {
            let style = if i == completion.selected_index() { palette.warning() } else { palette.status() };
            let text = clip(&format!("  /{}  {}", c.name, c.help), width);
            Line::from(Span::styled(text, style))
        })
        .collect()
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
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
    fn no_leading_slash_completes_nothing() {
        assert_eq!(complete("hello", &catalog()), None);
    }

    #[test]
    fn a_space_after_the_verb_ends_completion() {
        assert_eq!(complete("/stage ex", &catalog()), None);
    }

    #[test]
    fn a_bare_slash_lists_the_whole_catalog_sorted() {
        let completion = complete("/", &catalog()).expect("some completion");
        let names: Vec<&str> = completion.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ledger", "stage", "status"]);
    }

    #[test]
    fn a_prefix_narrows_to_matching_names() {
        let completion = complete("/st", &catalog()).expect("some completion");
        let names: Vec<&str> = completion.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["stage", "status"]);
    }

    #[test]
    fn an_unmatched_prefix_yields_no_candidates_but_still_completes() {
        let completion = complete("/zzz", &catalog()).expect("some completion");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn cycle_wraps_around_the_candidate_list() {
        let mut completion = complete("/", &catalog()).expect("some completion");
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
        assert_eq!(accept("stage", false), "/stage ");
        assert_eq!(accept("status", true), "/status");
    }

    #[test]
    fn the_popup_highlights_the_selected_candidate() {
        let mut completion = complete("/", &catalog()).expect("some completion");
        completion.cycle();
        let lines = render_popup(&completion, 80, &Palette::builtin());
        assert_eq!(lines.len(), 3);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(text[1].contains("/stage"), "got {text:?}");
    }

    #[test]
    fn an_empty_match_says_so_in_the_popup() {
        let completion = complete("/zzz", &catalog()).expect("some completion");
        let lines = render_popup(&completion, 80, &Palette::builtin());
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("no command matches"), "got {text:?}");
    }
}
