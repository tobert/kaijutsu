//! Run-control (rc) subcommands: add, list, rm, show, edit, reseed.
//!
//! Manages lifecycle script **files** at canonical paths
//! `/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}` (deployed under
//! `~/.config/kaijutsu/rc/`). The path itself is the user-facing key;
//! structural fields (context_type, verb, sort_key, name, extension) are
//! derived from it via `parse_rc_path`.
//!
//! These commands write through the kernel's CRDT file cache (admin-only
//! surface, so they bypass the gated `builtin.file:write` tool). Lifecycle
//! dispatch reads the same files; see `kj/lifecycle.rs`.

use kaijutsu_types::ContentType;
use regex::Regex;
use std::sync::OnceLock;

use super::parse::extract_named_arg;
use super::{KjCaller, KjDispatcher, KjResult};

/// Canonical rc path format. `attach` is a reserved verb — scripts install fine
/// but lifecycle dispatch is a no-op until that hook is wired (tracked in
/// `docs/issues.md`). `tick` is the beat verb (fired by the beat scheduler on a
/// context's OODA cadence).
const RC_PATH_PATTERN: &str = r"^/etc/rc/([a-z][a-z0-9_-]*)/(create|fork|attach|drift|tick)/(S\d{1,3})-([a-z][a-z0-9_-]*)\.(kai|md)$";

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
/// `S\d{1,3}`. Verbs: create, fork, attach, drift, tick.
pub fn parse_rc_path(path: &str) -> Result<RcPathParts, String> {
    let caps = rc_path_regex().captures(path).ok_or_else(|| {
        format!(
            "invalid rc path: '{path}'\n\
             expected /etc/rc/<context_type>/<verb>/SXX-name.{{kai,md}}\n\
             - context_type and name must be lowercase ([a-z][a-z0-9_-]*)\n\
             - verb must be one of: create, fork, attach, drift, tick\n\
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
    pub(crate) async fn dispatch_rc(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.rc_help());
        }
        // Writing /etc/rc lifecycle scripts is gated on `rc-write`. `kj rc`
        // writes go through the admin-only rc_cache (not builtin.file:write), so
        // this is the *only* place the rc-write capability is enforced for the
        // kj surface. Reads (list/show) stay ungated.
        if matches!(argv[0].as_str(), "add" | "rm" | "remove" | "edit" | "update" | "reseed") {
            if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::RcWrite, "rc") {
                return denied;
            }
        }
        match argv[0].as_str() {
            "add" => self.rc_add(argv, caller).await,
            "list" | "ls" => self.rc_list(argv).await,
            "rm" | "remove" => self.rc_rm(argv).await,
            "show" | "cat" => self.rc_show(argv).await,
            "edit" | "update" => self.rc_edit(argv).await,
            "reseed" => self.rc_reseed(argv).await,
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

    /// The shared CRDT file cache that backs `/etc/rc`. `kj rc` writes go
    /// through this rather than the gated `builtin.file:write` tool; the
    /// rc-write capability is enforced up front in [`Self::dispatch_rc`] so the
    /// two write paths (`kj rc` and `builtin.file`) share the same gate.
    fn rc_cache(&self) -> std::sync::Arc<crate::file_tools::FileDocumentCache> {
        self.kernel().file_cache(self.block_store())
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
- `kj rc show <path> [--json]` — print one script's content + metadata
- `kj rc edit <path> --content <body>` — replace a script's content
- `kj rc rm <path>` — remove a script
- `kj rc reseed [--type <ctx_type>]` — overwrite built-in seed scripts from the embedded defaults (destructive: clobbers edits to seeded paths)

Scripts are files under `/etc/rc` (`~/.config/kaijutsu/rc/`); these
commands edit them through the CRDT file cache. You can also edit the
files directly with an external editor — dispatch picks up changes on the
next lifecycle event. All `.kai` scripts run under the kernel-default
timeout.

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

    async fn rc_add(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
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
                    "kj rc add: missing content\nusage: kj rc add <path> --content <body>"
                        .to_string(),
                );
            }
        };

        let cache = self.rc_cache();
        // `add` must not clobber an existing script — use `edit` for that.
        if cache.exists(&path).await {
            return KjResult::Err(format!("kj rc add: '{path}' already exists (use edit)"));
        }
        if let Err(e) = self.write_rc_file(&cache, &path, &content).await {
            return KjResult::Err(format!("kj rc add: {e}"));
        }
        KjResult::ok(format!(
            "installed rc script '{}' (type={}, verb={}, sort={}, name={})",
            path, parts.context_type, parts.verb, parts.sort_key, parts.name
        ))
    }

    /// Write `content` to the rc file at `path` through the CRDT cache and
    /// flush it to disk, so dispatch (which reads through the same cache)
    /// and external tools (`vim`, readdir) all see the new bytes.
    async fn write_rc_file(
        &self,
        cache: &crate::file_tools::FileDocumentCache,
        path: &str,
        content: &str,
    ) -> Result<(), String> {
        cache.create_or_replace(path, content).await?;
        cache.mark_dirty(path);
        cache.flush_one(path).await
    }

    async fn rc_list(&self, argv: &[String]) -> KjResult {
        let type_filter = extract_named_arg(argv, &["--type"]);
        let verb_filter = extract_named_arg(argv, &["--verb"]);

        let mut paths = match self.walk_rc_paths().await {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc list: {e}")),
        };
        paths.retain(|p| {
            let parts = match parse_rc_path(p) {
                Ok(parts) => parts,
                Err(_) => return false, // stray non-canonical file
            };
            type_filter.as_ref().is_none_or(|t| parts.context_type == *t)
                && verb_filter.as_ref().is_none_or(|v| parts.verb == *v)
        });
        paths.sort();

        // Iteration handles: full rc-script paths (the resolver key for
        // `kj rc rm` / `kj rc show`), absolute so there's nothing to truncate.
        let data = serde_json::Value::Array(
            paths.iter().cloned().map(serde_json::Value::String).collect(),
        );
        if paths.is_empty() {
            return KjResult::ok_with_data("(no rc scripts)".to_string(), data);
        }
        let lines: Vec<String> = paths.iter().map(|p| format!("  {p}")).collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// Walk the `/etc/rc` tree (`<type>/<verb>/SXX-name.ext`) and return all
    /// canonical script paths. A missing tree yields an empty list.
    async fn walk_rc_paths(&self) -> Result<Vec<String>, String> {
        use crate::vfs::{VfsError, VfsOps};
        use std::path::Path;

        let vfs = self.kernel().vfs();
        // readdir, mapping "directory absent" to an empty listing.
        async fn entries(
            vfs: &crate::vfs::MountTable,
            dir: &str,
        ) -> Result<Vec<crate::vfs::DirEntry>, String> {
            match vfs.readdir(Path::new(dir)).await {
                Ok(e) => Ok(e),
                Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Ok(Vec::new()),
                Err(e) => Err(format!("readdir {dir}: {e}")),
            }
        }

        let mut out = Vec::new();
        for type_e in entries(vfs, "/etc/rc").await?.into_iter().filter(|e| e.kind.is_dir()) {
            let type_dir = format!("/etc/rc/{}", type_e.name);
            for verb_e in entries(vfs, &type_dir).await?.into_iter().filter(|e| e.kind.is_dir()) {
                let verb_dir = format!("{type_dir}/{}", verb_e.name);
                for file_e in entries(vfs, &verb_dir).await?.into_iter().filter(|e| e.kind.is_file()) {
                    if file_e.name.ends_with(".kai") || file_e.name.ends_with(".md") {
                        out.push(format!("{verb_dir}/{}", file_e.name));
                    }
                }
            }
        }
        Ok(out)
    }

    async fn rc_show(&self, argv: &[String]) -> KjResult {
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

        let parts = match parse_rc_path(&path) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc show: {e}")),
        };
        let cache = self.rc_cache();
        let content = match cache.read_content(&path).await {
            Ok(c) => c,
            Err(_) => return KjResult::Err(format!("kj rc show: '{path}' not found")),
        };

        // Metadata is derived from the canonical path; provenance lives in
        // the CRDT block, not here.
        let record = serde_json::json!({
            "path": path,
            "context_type": parts.context_type,
            "verb": parts.verb,
            "sort_key": parts.sort_key,
            "name": parts.name,
            "extension": parts.extension,
            "content_length": content.len(),
            "content": content,
        });

        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }

        // Fence content with the extension so .md renders as markdown and
        // .kai displays as a shell-ish block in surfaces that highlight it.
        let out = format!(
            "path:       {}\ntype:       {}\nverb:       {}\nsort_key:   {}\nname:       {}\nextension:  {}\nlength:     {} bytes\n\n```{}\n{}\n```\n",
            path,
            parts.context_type,
            parts.verb,
            parts.sort_key,
            parts.name,
            parts.extension,
            content.len(),
            parts.extension,
            content,
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }

    async fn rc_edit(&self, argv: &[String]) -> KjResult {
        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc edit: missing <path>\nusage: kj rc edit <path> --content <body>"
                        .to_string(),
                );
            }
        };
        if let Err(e) = parse_rc_path(&path) {
            return KjResult::Err(format!("kj rc edit: {e}"));
        }

        let content = match extract_named_arg(argv, &["--content"]) {
            Some(c) => c,
            None => {
                return KjResult::Err(
                    "kj rc edit: nothing to change\nsupply --content <body>".to_string(),
                );
            }
        };

        let cache = self.rc_cache();
        if !cache.exists(&path).await {
            return KjResult::Err(format!("kj rc edit: '{path}' not found"));
        }
        if let Err(e) = self.write_rc_file(&cache, &path, &content).await {
            return KjResult::Err(format!("kj rc edit: {e}"));
        }
        KjResult::ok(format!("edited rc script '{path}' (content)"))
    }

    async fn rc_reseed(&self, argv: &[String]) -> KjResult {
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

        // Write each embedded seed back through the cache (→ disk). Matches
        // the type filter against the canonical path's context_type segment.
        let cache = self.rc_cache();
        let mut count = 0usize;
        for (seed_path, body) in crate::seed_scripts::seed_files() {
            if let Some(t) = type_filter.as_deref() {
                let seg = seed_path
                    .strip_prefix(crate::seed_scripts::RC_VFS_ROOT)
                    .and_then(|r| r.split('/').next());
                if seg != Some(t) {
                    continue;
                }
            }
            if let Err(e) = self.write_rc_file(&cache, seed_path, body).await {
                return KjResult::Err(format!("kj rc reseed: {seed_path}: {e}"));
            }
            count += 1;
        }

        let scope = match &type_filter {
            Some(t) => format!(" (context_type={t})"),
            None => String::new(),
        };
        KjResult::ok(format!(
            "reseeded {count} rc script(s){scope} from embedded defaults"
        ))
    }

    async fn rc_rm(&self, argv: &[String]) -> KjResult {
        use crate::vfs::VfsOps;

        let path = match argv.get(1) {
            Some(p) => p.clone(),
            None => {
                return KjResult::Err(
                    "kj rc rm: missing <path>\nusage: kj rc rm <path>".to_string(),
                );
            }
        };

        let cache = self.rc_cache();
        if !cache.exists(&path).await {
            return KjResult::Err(format!("kj rc rm: '{path}' not found"));
        }
        // Drop the cached CRDT doc, then unlink the backing file.
        cache.invalidate(&path);
        if let Err(e) = self.kernel().vfs().unlink(std::path::Path::new(&path)).await {
            return KjResult::Err(format!("kj rc rm: unlink '{path}': {e}"));
        }
        KjResult::ok(format!("removed rc script '{path}'"))
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

    /// Read an rc script's content back through the file cache (the same
    /// path dispatch reads), so tests verify the on-disk file, not a row.
    async fn read_rc(d: &KjDispatcher, path: &str) -> Option<String> {
        d.kernel()
            .file_cache(d.block_store())
            .read_content(path)
            .await
            .ok()
    }

    /// `kj rc edit` replaces a script's content in the file.
    #[tokio::test]
    async fn rc_edit_updates_content() {
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

        assert_eq!(
            read_rc(&d, "/etc/rc/edittest/create/S00-foo.kai").await.as_deref(),
            Some("echo new"),
        );
    }

    /// `kj rc edit` with no --content is a user error, not a no-op.
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

        let restored = read_rc(&d, "/etc/rc/default/create/S20-cache.kai")
            .await
            .expect("seed file present after reseed");
        assert!(
            restored.contains("kj cache add --target=tools"),
            "reseed didn't restore: {restored}"
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

    /// `kj rc reseed` (no filter) must succeed even when a seed doc with
    /// multi-byte UTF-8 (the stances: 改善, em-dashes, …) is already in the
    /// cache — the production state when a live mcp context has loaded its
    /// stance. Reproduces the create_or_replace byte/char overrun via the
    /// reseed path end-to-end.
    #[tokio::test]
    async fn rc_reseed_succeeds_with_cached_multibyte_stance() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Warm the cache with a multi-byte stance, as an mcp/coder context's
        // create lifecycle would (read_content loads + caches the doc).
        let _ = d
            .kernel()
            .file_cache(d.block_store())
            .read_content("/etc/rc/mcp/create/S00-stance.md")
            .await
            .expect("seeded stance is readable");

        let result = d.dispatch(&[s("rc"), s("reseed")], &c).await;
        assert!(
            matches!(result, KjResult::Ok { .. }),
            "reseed over a cached multi-byte stance must succeed, got: {result:?}"
        );
    }
}
