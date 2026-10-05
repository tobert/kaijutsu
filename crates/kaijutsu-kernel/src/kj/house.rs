//! The house verbs: the `kj` verbs that run the kaijutsu house itself.
//!
//! A worker seat (coder, toolie, musician) does its task and does not need to
//! administer the kernel. Its loadout leaves out the `house` capability, so
//! these verbs are refused and left out of `kj help`. This narrows focus; it
//! is not a security boundary (`docs/instrument-design.md`, "Many hands, one
//! trust boundary"). The verb's own capability (`operator`, `drive`, ...)
//! still applies on top.
//!
//! Every top-level verb is in exactly one table. A test walks the verbs the
//! command tree reflects, so a new verb fails until it is classified here.

/// Top-level verbs that need the `house` capability.
pub(crate) const HOUSE_VERBS: &[&str] = &[
    "alias", "attach", "audio", "backend", "binding", "cast", "cc", "character", "config", "cp",
    "db", "doc", "drive", "hook", "interrupt", "ledger", "mcp", "midi", "play", "policy",
    "preset", "rc", "roster", "swap", "system", "transport",
];

/// Top-level verbs a worker seat keeps.
pub(crate) const WORKER_VERBS: &[&str] = &[
    "block", "cache", "cas", "context", "diff", "drift", "editor", "fork", "stage", "handoff",
    "kaish", "model", "models", "search", "synth", "vfs", "wait", "workspace",
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

/// True if `word` names a house verb.
pub(crate) fn is_house_verb(word: &str) -> bool {
    HOUSE_VERBS.contains(&canonical(word))
}

/// `kj help` text without the house verbs' entries.
///
/// An entry starts on an unindented line of the command list and runs
/// through the indented lines after it. Only text inside the first fenced
/// block that follows a `## Commands` heading is filtered; the rest of the
/// help is kept as written.
pub(crate) fn without_house_verbs(help: &str) -> String {
    let mut out = String::with_capacity(help.len());
    let mut in_commands_section = false;
    let mut in_fence = false;
    let mut dropping = false;
    for line in help.split_inclusive('\n') {
        if line.starts_with("## ") {
            in_commands_section = line.trim_end() == "## Commands";
        }
        if in_commands_section && line.starts_with("```") {
            in_fence = !in_fence;
            dropping = false;
        } else if in_commands_section && in_fence {
            if !line.starts_with([' ', '\t']) && !line.trim().is_empty() {
                let verb = line.split_whitespace().next().unwrap_or("");
                dropping = is_house_verb(verb);
            }
            if dropping {
                continue;
            }
        }
        out.push_str(line);
    }
    out
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
    fn aliases_follow_their_verb() {
        assert!(!is_house_verb("ctx") && !is_house_verb("ws"));
        assert!(is_house_verb("ledger"));
    }
}
