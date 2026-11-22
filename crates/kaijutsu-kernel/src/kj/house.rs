//! The house verbs: the `kj` verbs that run the kaijutsu house itself.
//!
//! A worker context (coder, toolie, musician) does its task and does not need
//! to administer the kernel. Its loadout leaves out the `house` capability, so
//! these verbs are refused and left out of `kj help`. This narrows focus; it
//! is not a security boundary (`docs/instrument-design.md`, "Many hands, one
//! trust boundary"). The verb's own capability (`operator`, `drive`, ...)
//! still applies on top.
//!
//! Every top-level verb is in exactly one table. A test walks the verbs the
//! command tree reflects, so a new verb fails until it is classified here.

/// Top-level verbs that need the `house` capability (kernel administration).
pub(crate) const HOUSE_VERBS: &[&str] = &[
    "alias", "attach", "audio", "backend", "binding", "cc", "config", "council", "cp",
    "db", "doc", "embedding", "hook", "mcp", "midi", "play", "policy",
    "preset", "price", "rc", "roster", "swap", "system", "transport",
];

/// Top-level verbs for direct-kohai orchestration: accessible to `senpai` and
/// `house` seats (and `drive` is also reachable by a worker holding `drive`,
/// such as a kohai waking `.senpai`).
pub(crate) const SENPAI_VERBS: &[&str] = &[
    "character", "drive", "interrupt", "ledger",
];

/// Top-level verbs a worker context keeps. Read by the classification test,
/// which is what makes a new verb choose a table.
#[cfg(test)]
pub(crate) const WORKER_VERBS: &[&str] = &[
    "block", "cache", "cas", "context", "diff", "drift", "fork", "stage", "handoff",
    "kaish", "models", "search", "shadow", "synth", "vfs", "vi", "wait", "watch", "workspace",
];

/// Short forms of top-level verbs, and the verb each one names.
pub(crate) const VERB_ALIASES: &[(&str, &str)] = &[("ctx", "context"), ("ws", "workspace")];

/// The verb a typed word names: itself, or the verb its short form stands for.
fn canonical(word: &str) -> &str {
    VERB_ALIASES
        .iter()
        .find(|(alias, _)| *alias == word)
        .map_or(word, |(_, verb)| verb)
}

/// True if `word` names a house-only verb.
pub(crate) fn is_house_verb(word: &str) -> bool {
    HOUSE_VERBS.contains(&canonical(word))
}

/// True if `word` names a senpai orchestration verb.
pub(crate) fn is_senpai_verb(word: &str) -> bool {
    SENPAI_VERBS.contains(&canonical(word))
}

/// `kj help` text filtered for a seat's capabilities:
/// - `has_house`: keeps all verbs.
/// - `has_senpai` (without `has_house`): keeps worker + senpai verbs, drops house verbs.
/// - neither: keeps worker verbs only, drops both house and senpai verbs.
pub(crate) fn filter_help(help: &str, has_house: bool, has_senpai: bool) -> String {
    if has_house {
        return help.to_string();
    }
    let mut out = String::with_capacity(help.len());
    let mut group = Group::default();
    let mut in_commands_section = false;
    let mut in_fence = false;
    let mut dropping = false;
    for line in help.split_inclusive('\n') {
        if line.starts_with("## ") || (in_commands_section && line.starts_with("### ")) {
            group.flush_into(&mut out);
            group.in_group = line.starts_with("### ");
            if line.starts_with("## ") {
                in_commands_section = line.trim_end() == "## Commands";
            }
        }
        if in_commands_section && line.starts_with("```") {
            in_fence = !in_fence;
            dropping = false;
        } else if in_commands_section && in_fence {
            if !line.starts_with([' ', '\t']) && !line.trim().is_empty() {
                let verb = line.split_whitespace().next().unwrap_or("");
                dropping = is_house_verb(verb) || (!has_senpai && is_senpai_verb(verb));
                group.entries += 1;
                group.kept += usize::from(!dropping);
            }
            if dropping {
                continue;
            }
        }
        if group.in_group {
            group.text.push_str(line);
        } else {
            out.push_str(line);
        }
    }
    group.flush_into(&mut out);
    out
}

/// `kj help` text without house or senpai verbs (worker view).
#[cfg(test)]
pub(crate) fn without_house_verbs(help: &str) -> String {
    filter_help(help, false, false)
}

/// One `### ` group of the command list, held until its end decides
/// whether it is kept.
#[derive(Default)]
struct Group {
    in_group: bool,
    text: String,
    entries: usize,
    kept: usize,
}

impl Group {
    fn flush_into(&mut self, out: &mut String) {
        if self.entries == 0 || self.kept > 0 {
            out.push_str(&self.text);
        }
        *self = Group::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_entry_takes_its_continuation_lines_with_it() {
        let help = "## Commands\n\n```\nledger   list\n         more\nblock    list\nrc       list\n```\n\nRun it.\n";
        let got = without_house_verbs(help);
        assert!(!got.contains("ledger") && !got.contains("more") && !got.contains("rc "), "{got}");
        assert!(got.contains("block    list") && got.contains("Run it."), "{got}");
    }

    #[test]
    fn text_outside_the_command_list_is_kept() {
        let help = "# kj\n\n```\nledger is outside the list\n```\n\n## Commands\n\n```\nledger x\n```\n";
        let got = without_house_verbs(help);
        assert!(got.contains("ledger is outside the list"), "{got}");
        assert!(!got.contains("ledger x"), "{got}");
    }

    #[test]
    fn a_group_left_with_no_entries_is_dropped_with_its_heading() {
        let help = "## Commands\n\n### Work\n\n```\nblock    list\nrc       list\n```\n\n### House\n\n```\nledger   list\n         more\n```\n\n## Next\n\nRun it.\n";
        let got = without_house_verbs(help);
        assert!(got.contains("### Work") && got.contains("block    list"), "{got}");
        assert!(!got.contains("### House") && !got.contains("ledger") && !got.contains("rc "), "{got}");
        assert!(got.contains("## Next\n\nRun it."), "{got}");
        assert_eq!(got.matches("```").count(), 2, "one fence pair remains: {got}");
    }

    #[test]
    fn aliases_follow_their_verb() {
        assert!(!is_house_verb("ctx") && !is_house_verb("ws"));
        assert!(is_senpai_verb("ledger"));
        assert!(is_house_verb("rc"));
    }
}
