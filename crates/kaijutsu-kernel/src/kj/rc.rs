//! Run-control (rc) subcommands: add, list, rm.
//!
//! Manages persistent lifecycle scripts at canonical paths
//! `/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`. The path itself is
//! the user-facing key; structural columns (context_type, verb, sort_key,
//! name, extension) are derived during install via `parse_rc_path`.
//!
//! Lifecycle dispatch (Phase 5) reads scripts from this table; this
//! module is admin-only.

use kaijutsu_types::ContentType;
use regex::Regex;
use std::sync::OnceLock;

use crate::kernel_db::RcScriptRow;

use super::parse::extract_named_arg;
use super::{KjCaller, KjDispatcher, KjResult};

/// Canonical rc path format. `attach` and `drift` are reserved verbs —
/// scripts install fine but lifecycle dispatch is a no-op until those
/// hooks are wired (tracked in `docs/issues.md`).
const RC_PATH_PATTERN: &str = r"^/etc/rc/([a-z][a-z0-9_-]*)/(create|fork|attach|drift)/(S\d{1,3})-([a-z][a-z0-9_-]*)\.(kai|md)$";

fn rc_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(RC_PATH_PATTERN).expect("rc path regex compiles"))
}

/// Parsed components of a canonical rc path.
pub struct RcPathParts {
    pub context_type: String,
    pub verb: String,
    pub sort_key: String,
    pub name: String,
    pub extension: String,
}

/// Validate and split a canonical rc path.
///
/// Format: `/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`. Type and
/// name are lowercase identifiers (`[a-z][a-z0-9_-]*`); sort_key matches
/// `S\d{1,3}`. Verbs: create, fork, attach, drift.
pub fn parse_rc_path(path: &str) -> Result<RcPathParts, String> {
    let caps = rc_path_regex().captures(path).ok_or_else(|| {
        format!(
            "invalid rc path: '{path}'\n\
             expected /etc/rc/<context_type>/<verb>/SXX-name.{{kai,md}}\n\
             - context_type and name must be lowercase ([a-z][a-z0-9_-]*)\n\
             - verb must be one of: create, fork, attach, drift\n\
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

impl KjDispatcher {
    pub(crate) fn dispatch_rc(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.rc_help());
        }
        match argv[0].as_str() {
            "add" => self.rc_add(argv, caller),
            "list" | "ls" => self.rc_list(argv),
            "rm" | "remove" => self.rc_rm(argv),
            "help" | "--help" | "-h" => {
                KjResult::ok_ephemeral(self.rc_help(), ContentType::Markdown)
            }
            other => KjResult::Err(format!(
                "kj rc: unknown subcommand '{}'\n\n{}",
                other,
                self.rc_help()
            )),
        }
    }

    fn rc_help(&self) -> String {
        // Inline help; if the docs/help/ tree gets a kj-rc.md later, swap
        // for include_str! to match preset/workspace style.
        r#"# kj rc — run-control lifecycle scripts

Scripts run at context lifecycle moments based on `context_type`. The path
encodes everything: `/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`.

## Commands

- `kj rc add <path> --content <body>` — install a script
- `kj rc list [--type=...] [--verb=...]` — list installed scripts
- `kj rc rm <path>` — remove a script

## Verbs

- `create` — fires when a fresh context is created
- `fork` — fires when a context is forked from a parent
- `attach`, `drift` — reserved (lifecycle not yet wired)

## Ordering

Scripts run lexically by `(sort_key, name)`. Use 2- or 3-digit padding
consistently per directory: `S05`, `S10` sort correctly; `S5`, `S10`
do NOT (`S10` < `S5` lexically).

## File types

- `.md` — content is inserted as a block in the new context
- `.kai` — content is executed via kaish with `KJ_CONTEXT`,
  `KJ_VERB`, and (for fork) `KJ_PARENT_CONTEXT` overlay vars

## Example

```
echo "You are a planner. Be concise." | \\
    kj rc add /etc/rc/planner/create/S00-prompt.md
kj context create my-plan --type=planner
```
"#
        .to_string()
    }

    fn rc_add(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc add: missing <path>\nusage: kj rc add <path> --content <body>"
                        .to_string(),
                );
            }
        };

        let parts = match parse_rc_path(&path) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc add: {e}")),
        };

        let content = match extract_named_arg(argv, &["--content"]) {
            Some(c) => c,
            None => {
                return KjResult::Err(
                    "kj rc add: missing --content <body>\n\
                     (stdin support is a follow-up; pipe via --content for now)"
                        .to_string(),
                );
            }
        };

        let row = RcScriptRow {
            kernel_id: self.kernel_id(),
            context_type: parts.context_type.clone(),
            verb: parts.verb.clone(),
            sort_key: parts.sort_key.clone(),
            name: parts.name.clone(),
            extension: parts.extension.clone(),
            content,
            path: path.clone(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
        };

        let db = self.kernel_db().lock();
        match db.insert_rc_script(&row) {
            Ok(()) => KjResult::ok(format!(
                "installed rc script '{}' (type={}, verb={}, sort={}, name={})",
                path, parts.context_type, parts.verb, parts.sort_key, parts.name
            )),
            Err(e) => KjResult::Err(format!("kj rc add: {e}")),
        }
    }

    fn rc_list(&self, argv: &[String]) -> KjResult {
        let type_filter = extract_named_arg(argv, &["--type"]);
        let verb_filter = extract_named_arg(argv, &["--verb"]);

        let db = self.kernel_db().lock();
        let scripts = match db.list_rc_scripts_all(self.kernel_id()) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj rc list: {e}")),
        };

        let filtered: Vec<&RcScriptRow> = scripts
            .iter()
            .filter(|s| match &type_filter {
                Some(t) => s.context_type == *t,
                None => true,
            })
            .filter(|s| match &verb_filter {
                Some(v) => s.verb == *v,
                None => true,
            })
            .collect();

        if filtered.is_empty() {
            return KjResult::ok("(no rc scripts)".to_string());
        }

        let lines: Vec<String> = filtered
            .iter()
            .map(|s| format!("  {}  ({} bytes)", s.path, s.content.len()))
            .collect();
        KjResult::ok(lines.join("\n"))
    }

    fn rc_rm(&self, argv: &[String]) -> KjResult {
        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc rm: missing <path>\nusage: kj rc rm <path>".to_string(),
                );
            }
        };

        let db = self.kernel_db().lock();
        match db.delete_rc_script(self.kernel_id(), &path) {
            Ok(true) => KjResult::ok(format!("removed rc script '{}'", path)),
            Ok(false) => KjResult::Err(format!("kj rc rm: '{}' not found", path)),
            Err(e) => KjResult::Err(format!("kj rc rm: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_valid_canonical_forms() {
        for (path, expected_type, expected_verb, expected_ext) in [
            ("/etc/rc/planner/create/S00-prompt.md", "planner", "create", "md"),
            ("/etc/rc/coder/fork/S05-record.kai", "coder", "fork", "kai"),
            ("/etc/rc/test_v2/attach/S100-many.md", "test_v2", "attach", "md"),
            ("/etc/rc/long-name-here/drift/S0-noop.kai", "long-name-here", "drift", "kai"),
        ] {
            let parts = parse_rc_path(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(parts.context_type, expected_type, "type for {path}");
            assert_eq!(parts.verb, expected_verb, "verb for {path}");
            assert_eq!(parts.extension, expected_ext, "ext for {path}");
        }
    }

    #[test]
    fn path_rejects_uppercase() {
        assert!(parse_rc_path("/etc/rc/Planner/create/S00-foo.md").is_err());
        assert!(parse_rc_path("/etc/rc/planner/create/S00-Foo.md").is_err());
    }

    #[test]
    fn path_rejects_unknown_verb() {
        assert!(parse_rc_path("/etc/rc/planner/spawn/S00-foo.md").is_err());
        assert!(parse_rc_path("/etc/rc/planner/destroy/S00-foo.kai").is_err());
    }

    #[test]
    fn path_rejects_unknown_extension() {
        assert!(parse_rc_path("/etc/rc/planner/create/S00-foo.sh").is_err());
        assert!(parse_rc_path("/etc/rc/planner/create/S00-foo.txt").is_err());
    }

    #[test]
    fn path_rejects_missing_s_prefix() {
        assert!(parse_rc_path("/etc/rc/planner/create/00-foo.md").is_err());
        assert!(parse_rc_path("/etc/rc/planner/create/foo.md").is_err());
    }

    #[test]
    fn path_rejects_wrong_root() {
        assert!(parse_rc_path("/rc/planner/create/S00-foo.md").is_err());
        assert!(parse_rc_path("/etc/init/planner/create/S00-foo.md").is_err());
    }

    #[test]
    fn attach_and_drift_install_paths_validate() {
        // Reserved-verb scripts validate now; lifecycle dispatch will
        // no-op them until those hooks land.
        assert!(parse_rc_path("/etc/rc/test/attach/S00-foo.md").is_ok());
        assert!(parse_rc_path("/etc/rc/test/drift/S00-foo.kai").is_ok());
    }
}
