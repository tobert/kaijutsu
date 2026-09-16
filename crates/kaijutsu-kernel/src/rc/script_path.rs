//! Canonical rc file paths and the executable subset used by discovery.

use kaijutsu_types::paths;
use regex::Regex;
use std::sync::OnceLock;

/// Administration addresses executable scripts and Markdown companion data.
const RC_FILENAME_PATTERN: &str = r"(S\d{1,3})-([a-z][a-z0-9_-]*)\.(kai|md)";

fn rc_path_pattern() -> String {
    let verbs = crate::rc::RC_VERBS.join("|");
    let root = paths::RC_ROOT;
    let file = RC_FILENAME_PATTERN;
    format!(r"^{root}/([a-z][a-z0-9_-]*)/({verbs})/{file}$")
}

fn rc_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&rc_path_pattern()).expect("rc path regex compiles"))
}

/// Accept only canonical executable entries: `SXX-name.kai`.
pub fn is_rc_script_filename(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    name.ends_with(".kai") && RE.get_or_init(|| {
        Regex::new(&format!(r"^{RC_FILENAME_PATTERN}$")).expect("rc filename regex compiles")
    })
    .is_match(name)
}

/// Parsed components of a canonical rc script or Markdown data path.
pub struct RcPathParts {
    pub context_type: String,
    pub verb: String,
    pub sort_key: String,
    pub name: String,
    pub extension: String,
}

/// Validate and split a canonical rc path.
///
/// Format: `/config/rc/<context_type>/<verb>/SXX-name.{kai,md}`. Type and
/// name are lowercase identifiers (`[a-z][a-z0-9_-]*`); sort_key matches
/// `S\d{1,3}`. Valid verbs are [`crate::rc::RC_VERBS`].
pub fn parse_rc_path(path: &str) -> Result<RcPathParts, String> {
    let caps = rc_path_regex().captures(path).ok_or_else(|| {
        let verbs = crate::rc::RC_VERBS.join(", ");
        format!(
            "invalid rc path: '{path}'\n\
             expected /config/rc/<context_type>/<verb>/SXX-name.{{kai,md}}\n\
             - context_type and name must be lowercase ([a-z][a-z0-9_-]*)\n\
             - verb must be one of: {verbs}\n\
             - sort_key must be S followed by 1-3 digits (e.g. S00, S05, S100)\n\
             - extension must be 'kai' or 'md'"
        )
    })?;
    Ok(RcPathParts {
        context_type: caps[1].to_string(),
        verb: caps[2].to_string(),
        sort_key: caps[3].to_string(),
        name: caps[4].to_string(),
        extension: caps[5].to_string(),
    })
}
