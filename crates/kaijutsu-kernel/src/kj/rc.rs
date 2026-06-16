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

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use regex::Regex;
use std::sync::OnceLock;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "rc",
    about = "Run-control lifecycle scripts at /etc/rc/<type>/<verb>/SXX-name.{kai,md}",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct RcArgs {
    #[command(subcommand)]
    command: RcCommand,
}

#[derive(Subcommand, Debug)]
enum RcCommand {
    /// Install a script. `--content <body>` (or piped stdin) is the script text.
    Add {
        /// Canonical path: /etc/rc/<type>/<verb>/SXX-name.{kai,md}
        path: String,
        /// Script body (stdin is piped here for `kj rc add` when omitted)
        #[arg(long)]
        content: Option<String>,
    },
    /// List installed scripts, optionally filtered.
    #[command(alias = "ls")]
    List {
        /// Filter by context_type
        #[arg(long = "type")]
        type_filter: Option<String>,
        /// Filter by verb (create|fork|attach|drift|tick)
        #[arg(long = "verb")]
        verb_filter: Option<String>,
    },
    /// Remove a script.
    #[command(alias = "remove")]
    Rm {
        /// Canonical rc path to remove
        path: String,
    },
    /// Print one script's content + metadata.
    #[command(alias = "cat")]
    Show {
        /// Canonical rc path to show
        path: String,
        /// Emit a JSON object instead of a labelled view
        #[arg(long)]
        json: bool,
    },
    /// Replace a script's content.
    #[command(alias = "update")]
    Edit {
        /// Canonical rc path to edit
        path: String,
        /// Replacement body
        #[arg(long)]
        content: Option<String>,
    },
    /// Restore one script to its embedded seed (targeted recovery from a
    /// botched edit; recreates the file if it was removed). Errors if the
    /// path has no built-in seed — there is nothing to reset it to.
    Reset {
        /// Canonical rc path to restore from the embedded default
        path: String,
    },
}

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
            return clap_help_for::<RcArgs>();
        }
        let parsed = match RcArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj rc: {e}"));
            }
        };
        // Writing /etc/rc lifecycle scripts is gated on `rc-write`. `kj rc`
        // writes go through the admin-only rc_cache (not builtin.file:write), so
        // this is the *only* place the rc-write capability is enforced for the
        // kj surface. Reads (list/show) stay ungated.
        if matches!(
            parsed.command,
            RcCommand::Add { .. }
                | RcCommand::Rm { .. }
                | RcCommand::Edit { .. }
                | RcCommand::Reset { .. }
        ) && let Err(denied) = self.require_cap(caller, crate::mcp::Capability::RcWrite, "rc")
        {
            return denied;
        }
        match parsed.command {
            RcCommand::Add { path, content } => {
                self.rc_add(&path, content.as_deref(), caller).await
            }
            RcCommand::List {
                type_filter,
                verb_filter,
            } => self.rc_list(type_filter.as_deref(), verb_filter.as_deref()).await,
            RcCommand::Rm { path } => self.rc_rm(&path).await,
            RcCommand::Show { path, json } => self.rc_show(&path, json).await,
            RcCommand::Edit { path, content } => self.rc_edit(&path, content.as_deref()).await,
            RcCommand::Reset { path } => self.rc_reset(&path).await,
        }
    }

    async fn rc_add(&self, path: &str, content: Option<&str>, _caller: &KjCaller) -> KjResult {
        let parts = match parse_rc_path(path) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc add: {e}")),
        };

        let content = match content {
            Some(c) => c,
            None => {
                return KjResult::Err(
                    "kj rc add: missing content\nusage: kj rc add <path> --content <body>"
                        .to_string(),
                );
            }
        };

        // `add` must not clobber an existing script — use `edit` for that.
        if self.rc_exists(path).await {
            return KjResult::Err(format!("kj rc add: '{path}' already exists (use edit)"));
        }
        if let Err(e) = self.write_rc_file(path, content).await {
            return KjResult::Err(format!("kj rc add: {e}"));
        }
        KjResult::ok(format!(
            "installed rc script '{}' (type={}, verb={}, sort={}, name={})",
            path, parts.context_type, parts.verb, parts.sort_key, parts.name
        ))
    }

    /// Write `content` to the rc script at `path` straight through the VFS to
    /// the CRDT-native `/etc/rc` backend. There is no host file and no
    /// FileDocumentCache mirror: the CRDT document IS the script. Dispatch
    /// (`load_rc_scripts`) reads the same document through the same VFS.
    async fn write_rc_file(&self, path: &str, content: &str) -> Result<(), String> {
        use crate::vfs::VfsOps;
        self.kernel()
            .vfs()
            .write_all(std::path::Path::new(path), content.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// Whether an rc script exists at `path` (a file, not a virtual directory).
    async fn rc_exists(&self, path: &str) -> bool {
        use crate::vfs::VfsOps;
        self.kernel()
            .vfs()
            .getattr(std::path::Path::new(path))
            .await
            .map(|a| a.is_file())
            .unwrap_or(false)
    }

    /// Read an rc script's content from the VFS. `Ok(None)` for an absent
    /// script (NotFound / no mount); `Err` for a real backend failure or
    /// non-UTF-8 content — never masked as "not found".
    async fn read_rc_content(&self, path: &str) -> Result<Option<String>, String> {
        use crate::vfs::{VfsError, VfsOps};
        let bytes = match self
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(path))
            .await
        {
            Ok(b) => b,
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|e| format!("not valid UTF-8: {e}"))
    }

    async fn rc_list(&self, type_filter: Option<&str>, verb_filter: Option<&str>) -> KjResult {
        let mut paths = match self.walk_rc_paths().await {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc list: {e}")),
        };
        paths.retain(|p| {
            let parts = match parse_rc_path(p) {
                Ok(parts) => parts,
                Err(_) => return false, // stray non-canonical file
            };
            type_filter.is_none_or(|t| parts.context_type == t)
                && verb_filter.is_none_or(|v| parts.verb == v)
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

    async fn rc_show(&self, path: &str, json: bool) -> KjResult {
        let parts = match parse_rc_path(path) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj rc show: {e}")),
        };
        // Read straight from the CRDT-native backend. NotFound = absent script;
        // any other VfsError is a real backend failure (surfaced, not masked as
        // "not found").
        let content = match self.read_rc_content(path).await {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj rc show: '{path}' not found")),
            Err(e) => return KjResult::Err(format!("kj rc show: '{path}': {e}")),
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

    async fn rc_edit(&self, path: &str, content: Option<&str>) -> KjResult {
        if let Err(e) = parse_rc_path(path) {
            return KjResult::Err(format!("kj rc edit: {e}"));
        }

        let content = match content {
            Some(c) => c,
            None => {
                return KjResult::Err(
                    "kj rc edit: nothing to change\nsupply --content <body>".to_string(),
                );
            }
        };

        if !self.rc_exists(path).await {
            return KjResult::Err(format!("kj rc edit: '{path}' not found"));
        }
        if let Err(e) = self.write_rc_file(path, content).await {
            return KjResult::Err(format!("kj rc edit: {e}"));
        }
        KjResult::ok(format!("edited rc script '{path}' (content)"))
    }

    /// Restore one script to its embedded seed. Targeted recovery: unlike a
    /// bulk reseed, it touches exactly the path you name, and create-or-replace
    /// means it also recovers a file you `rm`'d. Errors (no silent no-op) when
    /// the path ships no embedded seed — there is nothing to reset it to.
    async fn rc_reset(&self, path: &str) -> KjResult {
        if let Err(e) = parse_rc_path(path) {
            return KjResult::Err(format!("kj rc reset: {e}"));
        }
        let Some(body) = crate::seed_scripts::seed_body(path) else {
            return KjResult::Err(format!(
                "kj rc reset: '{path}' has no built-in seed (nothing to reset to)\n\
                 only paths shipped under assets/defaults/rc can be reset"
            ));
        };
        if let Err(e) = self.write_rc_file(path, body).await {
            return KjResult::Err(format!("kj rc reset: {e}"));
        }
        KjResult::ok(format!("reset rc script '{path}' to its embedded seed"))
    }

    async fn rc_rm(&self, path: &str) -> KjResult {
        use crate::vfs::VfsOps;

        if !self.rc_exists(path).await {
            return KjResult::Err(format!("kj rc rm: '{path}' not found"));
        }
        // Delete the CRDT document directly (no host file, no cache mirror).
        if let Err(e) = self.kernel().vfs().unlink(std::path::Path::new(path)).await {
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

        let d = test_dispatcher_crdt_rc().await;
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

        let d = test_dispatcher_crdt_rc().await;
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

    /// Read an rc script's content back through the VFS (the same path
    /// dispatch reads), so tests verify the live CRDT document.
    async fn read_rc(d: &KjDispatcher, path: &str) -> Option<String> {
        use crate::vfs::VfsOps;
        let bytes = d
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(path))
            .await
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    /// `kj rc edit` replaces a script's content in the file.
    #[tokio::test]
    async fn rc_edit_updates_content() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
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

        let d = test_dispatcher_crdt_rc().await;
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

    /// `kj rc reset <path>` restores a single seeded script to its embedded
    /// default, undoing a user edit — targeted recovery, not a bulk reseed.
    #[tokio::test]
    async fn rc_reset_restores_seeded_path_after_edit() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Botch a default seed (test_dispatcher's tree is bootstrap-seeded).
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

        let result = d
            .dispatch(
                &[s("rc"), s("reset"), s("/etc/rc/default/create/S20-cache.kai")],
                &c,
            )
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "reset failed: {result:?}");

        let restored = read_rc(&d, "/etc/rc/default/create/S20-cache.kai")
            .await
            .expect("seed file present after reset");
        assert!(
            restored.contains("kj cache add --target=tools"),
            "reset didn't restore: {restored}"
        );
    }

    /// `kj rc reset <path>` recreates a script the user `rm`'d — recovery
    /// works even when the live file is gone, since the seed is the source.
    #[tokio::test]
    async fn rc_reset_recreates_removed_seeded_path() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[s("rc"), s("rm"), s("/etc/rc/default/create/S20-cache.kai")],
            &c,
        )
        .await;
        assert!(
            read_rc(&d, "/etc/rc/default/create/S20-cache.kai").await.is_none(),
            "precondition: file removed"
        );

        let result = d
            .dispatch(
                &[s("rc"), s("reset"), s("/etc/rc/default/create/S20-cache.kai")],
                &c,
            )
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "reset failed: {result:?}");
        assert!(
            read_rc(&d, "/etc/rc/default/create/S20-cache.kai")
                .await
                .is_some_and(|b| b.contains("kj cache add --target=tools")),
            "reset should recreate the removed seed from its embedded default"
        );
    }

    /// `kj rc reset <unseeded>` errors instead of silently no-oping — there
    /// is no embedded default to reset a user-authored script to.
    #[tokio::test]
    async fn rc_reset_unseeded_path_errors() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("reset"), s("/etc/rc/mine/create/S00-custom.kai")],
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

    /// `kj rc reset` must succeed when a multi-byte seed doc (the stances:
    /// 改善, em-dashes, …) is already warm in the cache — the production state
    /// once an mcp/coder context has loaded its stance. Guards the
    /// create_or_replace byte/char overrun via the reset write path.
    #[tokio::test]
    async fn rc_reset_succeeds_with_cached_multibyte_stance() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Warm the cache with a multi-byte stance, as a create lifecycle would.
        let _ = d
            .kernel()
            .file_cache(d.block_store())
            .read_content("/etc/rc/mcp/create/S00-stance.md")
            .await
            .expect("seeded stance is readable");

        let result = d
            .dispatch(
                &[s("rc"), s("reset"), s("/etc/rc/mcp/create/S00-stance.md")],
                &c,
            )
            .await;
        assert!(
            matches!(result, KjResult::Ok { .. }),
            "reset over a cached multi-byte stance must succeed, got: {result:?}"
        );
    }

    /// `kj rc list` emits full absolute paths as iteration handles so
    /// `for s in $(kj rc list); do kj rc rm $s; done` works.
    #[tokio::test]
    async fn rc_list_emits_path_array() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_crdt_rc().await;
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
