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
            "show" | "cat" => self.rc_show(argv),
            "edit" | "update" => self.rc_edit(argv),
            "reseed" => self.rc_reseed(argv),
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

- `kj rc add <path> [--content <body>] [--timeout <secs>]` — install a script
- `kj rc list [--type=...] [--verb=...]` — list installed scripts
- `kj rc show <path> [--json]` — print one script's content + metadata
- `kj rc edit <path> [--content <body>] [--timeout <secs>]` — update content / timeout (preserves created_at)
- `kj rc rm <path>` — remove a script
- `kj rc reseed [--type <ctx_type>]` — overwrite built-in seed scripts from the in-code defaults (destructive: clobbers user edits to seeded paths)

`--content` may be omitted when content is piped on stdin (e.g.
`cat prompt.md | kj rc add /etc/rc/...`). Explicit `--content` wins
when both are supplied.

`--timeout` sets a per-script wall-clock budget for `.kai` execution;
omit to inherit the kernel default. `.md` scripts ignore it.

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
                    "kj rc add: missing content\n\
                     usage: kj rc add <path> --content <body>\n\
                     or:    <something producing the body> | kj rc add <path>"
                        .to_string(),
                );
            }
        };

        // Per-script wall-clock budget. Omit → falls back to the kernel
        // policy at lifecycle dispatch time; explicit 0 is rejected
        // (would deadlock the script). `.md` scripts accept the column
        // but don't execute, so the value is recorded for documentation
        // even though it has no runtime effect.
        let timeout_secs = match extract_named_arg(argv, &["--timeout"]) {
            None => None,
            Some(s) => match s.parse::<u32>() {
                Ok(0) => {
                    return KjResult::Err(
                        "kj rc add: --timeout must be > 0 (omit to use the kernel default)"
                            .to_string(),
                    );
                }
                Ok(n) => Some(n),
                Err(_) => {
                    return KjResult::Err(format!(
                        "kj rc add: --timeout must be a positive integer (got '{s}')"
                    ));
                }
            },
        };

        let row = RcScriptRow {
            context_type: parts.context_type.clone(),
            verb: parts.verb.clone(),
            sort_key: parts.sort_key.clone(),
            name: parts.name.clone(),
            extension: parts.extension.clone(),
            content,
            path: path.clone(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: caller.principal_id,
            timeout_secs,
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
        let scripts = match db.list_rc_scripts_all() {
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

        // Iteration handles: full rc-script paths. Paths are the
        // resolver key for `kj rc rm` / `kj rc show`, and they're absolute
        // (`/etc/rc/<type>/<verb>/<sort>-<name>.<ext>`) so there's nothing
        // to truncate.
        let paths = serde_json::Value::Array(
            filtered
                .iter()
                .map(|s| serde_json::Value::String(s.path.clone()))
                .collect(),
        );

        if filtered.is_empty() {
            return KjResult::ok_with_data("(no rc scripts)".to_string(), paths);
        }

        let lines: Vec<String> = filtered
            .iter()
            .map(|s| {
                let timeout = match s.timeout_secs {
                    Some(n) => format!(", timeout={}s", n),
                    None => String::new(),
                };
                format!("  {}  ({} bytes{})", s.path, s.content.len(), timeout)
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), paths)
    }

    fn rc_show(&self, argv: &[String]) -> KjResult {
        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc show: missing <path>\nusage: kj rc show <path> [--json]"
                        .to_string(),
                );
            }
        };
        let json = super::parse::has_flag(argv, &["--json"]);

        let row = {
            let db = self.kernel_db().lock();
            match db.get_rc_script(&path) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return KjResult::Err(format!("kj rc show: '{path}' not found"));
                }
                Err(e) => return KjResult::Err(format!("kj rc show: {e}")),
            }
        };

        let timeout_json = match row.timeout_secs {
            Some(n) => serde_json::Value::Number(n.into()),
            None => serde_json::Value::Null,
        };
        let record = serde_json::json!({
            "path": row.path,
            "context_type": row.context_type,
            "verb": row.verb,
            "sort_key": row.sort_key,
            "name": row.name,
            "extension": row.extension,
            "timeout_secs": timeout_json,
            "created_at": row.created_at,
            "created_by": row.created_by.to_hex(),
            "content_length": row.content.len(),
            "content": row.content,
        });

        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }

        let timeout_str = row
            .timeout_secs
            .map(|n| format!("{n}s"))
            .unwrap_or_else(|| "(kernel default)".into());
        // Fence content with the extension so .md renders as markdown and
        // .kai displays as a shell-ish block in surfaces that highlight it.
        let fence_tag = row.extension.as_str();
        let out = format!(
            "path:       {}\ntype:       {}\nverb:       {}\nsort_key:   {}\nname:       {}\nextension:  {}\ntimeout:    {}\ncreated_at: {}\ncreated_by: {}\nlength:     {} bytes\n\n```{}\n{}\n```\n",
            row.path,
            row.context_type,
            row.verb,
            row.sort_key,
            row.name,
            row.extension,
            timeout_str,
            row.created_at,
            row.created_by.to_hex(),
            row.content.len(),
            fence_tag,
            row.content,
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }

    fn rc_edit(&self, argv: &[String]) -> KjResult {
        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc edit: missing <path>\n\
                     usage: kj rc edit <path> [--content <body>] [--timeout <secs>]"
                        .to_string(),
                );
            }
        };

        let content = extract_named_arg(argv, &["--content"]);
        let timeout_raw = extract_named_arg(argv, &["--timeout"]);

        if content.is_none() && timeout_raw.is_none() {
            return KjResult::Err(
                "kj rc edit: nothing to change\n\
                 supply --content <body> and/or --timeout <secs>"
                    .to_string(),
            );
        }

        // Parse timeout if present. Allow `--timeout 0` to mean "clear
        // the per-script override" (revert to kernel default) — this is
        // the one path to drop a previously-set timeout without
        // rm/add-ing the script. `rc add` still rejects 0 because there
        // it would be the only signal you'd given for `.kai` budget.
        let timeout_change: Option<Option<u32>> = match timeout_raw {
            None => None,
            Some(s) => match s.parse::<u32>() {
                Ok(0) => Some(None),
                Ok(n) => Some(Some(n)),
                Err(_) => {
                    return KjResult::Err(format!(
                        "kj rc edit: --timeout must be a non-negative integer (got '{s}'; \
                         use 0 to clear)"
                    ));
                }
            },
        };

        let db = self.kernel_db().lock();
        match db.update_rc_script(&path, content.as_deref(), timeout_change) {
            Ok(true) => {
                let mut changed: Vec<&str> = Vec::new();
                if content.is_some() {
                    changed.push("content");
                }
                if timeout_change.is_some() {
                    changed.push("timeout");
                }
                KjResult::ok(format!(
                    "edited rc script '{}' ({})",
                    path,
                    changed.join(", ")
                ))
            }
            Ok(false) => KjResult::Err(format!("kj rc edit: '{path}' not found")),
            Err(e) => KjResult::Err(format!("kj rc edit: {e}")),
        }
    }

    fn rc_reseed(&self, argv: &[String]) -> KjResult {
        let type_filter = extract_named_arg(argv, &["--type"]);

        if let Some(t) = type_filter.as_deref() {
            let allowed = crate::seed_scripts::seeded_context_types();
            if !allowed.contains(&t) {
                return KjResult::Err(format!(
                    "kj rc reseed: '{t}' has no built-in seed scripts\n\
                     known context_types with seeds: {}",
                    allowed.join(", ")
                ));
            }
        }

        let db = self.kernel_db().lock();
        match db.reseed_rc_scripts(type_filter.as_deref()) {
            Ok(count) => {
                let scope = match &type_filter {
                    Some(t) => format!(" (context_type={t})"),
                    None => String::new(),
                };
                KjResult::ok(format!(
                    "reseeded {count} rc script(s){scope} from in-code defaults"
                ))
            }
            Err(e) => KjResult::Err(format!("kj rc reseed: {e}")),
        }
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
        match db.delete_rc_script(&path) {
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

    /// `kj rc show <path>` round-trips content from an earlier `kj rc add`
    /// and surfaces the metadata fields (path, type, verb, ext, timeout).
    #[tokio::test]
    async fn rc_show_round_trips_content_and_metadata() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/showtest/create/S00-hello.kai"),
                s("--content"),
                s("echo hi"),
                s("--timeout"),
                s("30"),
            ],
            &c,
        )
        .await;

        let result = d
            .dispatch(
                &[
                    s("rc"),
                    s("show"),
                    s("/etc/rc/showtest/create/S00-hello.kai"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Ok {
                message,
                data: Some(v),
                ..
            } => {
                let obj = v.as_object().expect("show emits an object");
                assert_eq!(obj["context_type"].as_str(), Some("showtest"));
                assert_eq!(obj["verb"].as_str(), Some("create"));
                assert_eq!(obj["sort_key"].as_str(), Some("S00"));
                assert_eq!(obj["name"].as_str(), Some("hello"));
                assert_eq!(obj["extension"].as_str(), Some("kai"));
                assert_eq!(obj["timeout_secs"].as_u64(), Some(30));
                assert_eq!(obj["content"].as_str(), Some("echo hi"));
                assert!(message.contains("echo hi"), "fenced content in message: {message}");
                assert!(message.contains("```kai"), "extension-tagged fence: {message}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// `kj rc show <unknown>` is an error, not a silent empty result.
    #[tokio::test]
    async fn rc_show_missing_path_errors() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("show"), s("/etc/rc/none/create/S00-noop.kai")],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("not found"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `kj rc edit` updates content while preserving created_at/created_by.
    #[tokio::test]
    async fn rc_edit_updates_content_preserves_metadata() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/edittest/create/S00-foo.kai"),
                s("--content"),
                s("echo old"),
            ],
            &c,
        )
        .await;

        // Snapshot the original metadata.
        let before = d
            .kernel_db()
            .lock()
            .get_rc_script("/etc/rc/edittest/create/S00-foo.kai")
            .unwrap()
            .unwrap();

        let result = d
            .dispatch(
                &[
                    s("rc"),
                    s("edit"),
                    s("/etc/rc/edittest/create/S00-foo.kai"),
                    s("--content"),
                    s("echo new"),
                ],
                &c,
            )
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "edit failed: {result:?}");

        let after = d
            .kernel_db()
            .lock()
            .get_rc_script("/etc/rc/edittest/create/S00-foo.kai")
            .unwrap()
            .unwrap();
        assert_eq!(after.content, "echo new");
        assert_eq!(after.created_at, before.created_at, "created_at should be preserved");
        assert_eq!(after.created_by, before.created_by, "created_by should be preserved");
    }

    /// `kj rc edit --timeout 0` clears the per-script override (back to
    /// kernel default). Distinct from `rc add`, which rejects 0 because
    /// it would be the only timeout signal in the install path.
    #[tokio::test]
    async fn rc_edit_timeout_zero_clears_override() {
        use crate::kj::test_helpers::*;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/edittest/create/S05-with-timeout.kai"),
                s("--content"),
                s("true"),
                s("--timeout"),
                s("45"),
            ],
            &c,
        )
        .await;

        let pre = d
            .kernel_db()
            .lock()
            .get_rc_script("/etc/rc/edittest/create/S05-with-timeout.kai")
            .unwrap()
            .unwrap();
        assert_eq!(pre.timeout_secs, Some(45));

        d.dispatch(
            &[
                s("rc"),
                s("edit"),
                s("/etc/rc/edittest/create/S05-with-timeout.kai"),
                s("--timeout"),
                s("0"),
            ],
            &c,
        )
        .await;

        let post = d
            .kernel_db()
            .lock()
            .get_rc_script("/etc/rc/edittest/create/S05-with-timeout.kai")
            .unwrap()
            .unwrap();
        assert_eq!(post.timeout_secs, None, "0 should clear the override");
    }

    /// `kj rc edit` with no fields to change is a user error, not a no-op.
    #[tokio::test]
    async fn rc_edit_requires_at_least_one_field() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/edittest/create/S10-bare.kai"),
                s("--content"),
                s("noop"),
            ],
            &c,
        )
        .await;

        let result = d
            .dispatch(
                &[
                    s("rc"),
                    s("edit"),
                    s("/etc/rc/edittest/create/S10-bare.kai"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(msg.contains("nothing to change"), "msg: {msg}")
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `kj rc reseed` overwrites user edits on seeded paths.
    #[tokio::test]
    async fn rc_reseed_overwrites_user_edit_on_seeded_path() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Edit a default seed (test_dispatcher's KernelDb has seeds because
        // it goes through ensure_seeded_rc_scripts).
        d.dispatch(
            &[
                s("rc"),
                s("edit"),
                s("/etc/rc/default/create/S20-cache.kai"),
                s("--content"),
                s("# user override"),
            ],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("rc"), s("reseed")], &c).await;
        assert!(matches!(result, KjResult::Ok { .. }), "reseed failed: {result:?}");

        let row = d
            .kernel_db()
            .lock()
            .get_rc_script("/etc/rc/default/create/S20-cache.kai")
            .unwrap()
            .unwrap();
        assert!(
            row.content.contains("kj cache add --target=tools"),
            "reseed didn't restore: {}",
            row.content
        );
    }

    /// `kj rc reseed --type unknown` errors instead of silently no-oping.
    #[tokio::test]
    async fn rc_reseed_unknown_type_errors() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("reseed"), s("--type"), s("nope")],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(msg.contains("no built-in seed"), "msg: {msg}")
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `kj rc list` emits full absolute paths as iteration handles so
    /// `for s in $(kj rc list); do kj rc rm $s; done` works.
    #[tokio::test]
    async fn rc_list_emits_path_array() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Install two scripts via the dispatcher (round-trip through
        // `kj rc add` keeps the test honest about real path validation).
        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/test/create/S00-noop.kai"),
                s("--content"),
                s("true"),
            ],
            &c,
        )
        .await;
        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/etc/rc/test/create/S01-second.kai"),
                s("--content"),
                s("true"),
            ],
            &c,
        )
        .await;

        let result = d.dispatch(&[s("rc"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let paths: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(
                    paths.contains(&"/etc/rc/test/create/S00-noop.kai"),
                    "missing S00 in: {paths:?}"
                );
                assert!(
                    paths.contains(&"/etc/rc/test/create/S01-second.kai"),
                    "missing S01 in: {paths:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }
}
