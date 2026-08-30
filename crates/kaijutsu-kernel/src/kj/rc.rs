//! Run-control (rc) subcommands: add, list, rm, show.
//!
//! Manages lifecycle script **files** at canonical paths
//! `/config/rc/<context_type>/<verb>/SXX-name.{kai,md}` (deployed under
//! `~/.config/kaijutsu/config/rc/`). The path itself is the user-facing key;
//! structural fields (context_type, verb, sort_key, name, extension) are
//! derived from it via `parse_rc_path`.
//!
//! **rc scripts are host files, so this is a convenience, not a gateway.**
//! The editor, the file tools, host `vim` and git all reach the same files;
//! what these verbs add is canonical-path validation and the seed comparison
//! `kj rc list` reports. Editing a script in place is a plain file write —
//! there is no rc verb for it, and restoring the shipped defaults is
//! `kaijutsu-server rc reseed`, off the kernel. Lifecycle dispatch reads the
//! latest body from disk on every run; see `kj/lifecycle.rs` and
//! `docs/rc-on-disk.md`.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths;
use regex::Regex;
use std::sync::OnceLock;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "rc",
    about = "Run-control lifecycle scripts at /config/rc/<type>/<verb>/SXX-name.{kai,md}",
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
        /// Canonical path: /config/rc/<type>/<verb>/SXX-name.{kai,md}
        path: String,
        /// Script body (stdin is piped here for `kj rc add` when omitted).
        /// Free text: a body may legitimately begin with `-`, so this must
        /// accept a hyphen-prefixed value without clap reading it as a flag.
        #[arg(long, allow_hyphen_values = true)]
        content: Option<String>,
    },
    /// List installed scripts, optionally filtered. Each entry is marked
    /// against its embedded seed: in-sync, differs (edited since seeding),
    /// no-seed (a live-only, user-authored script), not-installed (a seed
    /// ships for the path and nothing is live at it — a script added to the
    /// embedded set after this kernel was first seeded), or dangling (a
    /// symlink whose target is gone, which fails every lifecycle run through
    /// it). Indicator only — it never writes anything. `kaijutsu-server rc
    /// reseed` installs what is not-installed; `--force` also restores what
    /// differs. Repair a dangling link by restoring its **target** — the link
    /// itself is fine.
    #[command(alias = "ls")]
    List {
        /// Filter by context_type
        #[arg(long = "type")]
        type_filter: Option<String>,
        /// Filter by verb (create|fork|attach|drift|tick|rotate)
        #[arg(long = "verb")]
        verb_filter: Option<String>,
        /// Emit a JSON object (with a per-entry seed_status record) instead
        /// of the marked human listing. `.data` stays the flat path array
        /// either way (the kj list-command iteration convention).
        #[arg(long)]
        json: bool,
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
}

/// Staleness classification for `kj rc list`'s per-entry seed comparison
/// (indicator only — nothing here writes anything; `kaijutsu-server rc
/// reseed` is the pull).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RcSeedStatus {
    /// The live script's body (or symlink target) matches its embedded seed
    /// exactly.
    InSync,
    /// A seed ships for this path, but the live script has drifted from it
    /// (edited, or a link/file shape mismatch).
    Differs,
    /// No embedded seed ships for this path — a live-only, user-authored
    /// script. Nothing to compare against, so it's neither in-sync nor
    /// differing.
    NoSeed,
    /// A seed ships for this path and nothing is live at it. The kernel owns
    /// `/config/rc` and seeds the namespace only when it is entirely empty, so a
    /// script added to the embedded set after this kernel was first seeded
    /// never lands on its own. `kaijutsu-server rc reseed` installs it; a path
    /// you deliberately removed keeps reporting this until you do.
    NotInstalled,
    /// The live entry is a symlink whose target cannot be read. This outranks
    /// every seed comparison because it is the more urgent fact: the lifecycle
    /// loader treats an unreadable entry as fatal, so one dangling link fails
    /// every run of the verb it sits in. The repair is restoring the
    /// **target**, not the link — the link itself is fine.
    Dangling,
}

impl RcSeedStatus {
    /// Human-readable marker appended to a `kj rc list` line.
    fn as_str(&self) -> &'static str {
        match self {
            RcSeedStatus::InSync => "in-sync",
            RcSeedStatus::Differs => "differs from seed",
            RcSeedStatus::NoSeed => "no seed",
            RcSeedStatus::NotInstalled => "not installed",
            RcSeedStatus::Dangling => "dangling — target missing",
        }
    }

    /// snake_case token for the `--json` structured record.
    fn as_json_str(&self) -> &'static str {
        match self {
            RcSeedStatus::InSync => "in_sync",
            RcSeedStatus::Differs => "differs",
            RcSeedStatus::NoSeed => "no_seed",
            RcSeedStatus::NotInstalled => "not_installed",
            RcSeedStatus::Dangling => "dangling",
        }
    }
}

/// Canonical rc path format. The verb alternation is built from
/// [`crate::kj::lifecycle::RC_VERBS`] — the single source shared with the firing
/// gate — so the validator can never reject a verb the scheduler fires.
/// `tick` is the beat verb (fired by the beat scheduler on a context's OODA
/// cadence); `rotate` is the page-turn verb.
/// The filename half of a canonical rc path: `SXX-name.{kai,md}`. Shared
/// with [`rc_path_pattern`] and with the lifecycle runner's own directory
/// filter ([`is_rc_script_filename`]) so that "what counts as a script"
/// has one definition. A runner that executes a file the validator would
/// reject is the same class of trap as a verb the scheduler fires and the
/// validator refuses.
const RC_FILENAME_PATTERN: &str = r"(S\d{1,3})-([a-z][a-z0-9_-]*)\.(kai|md)";

fn rc_path_pattern() -> String {
    let verbs = crate::kj::lifecycle::RC_VERBS.join("|");
    let root = paths::RC_ROOT;
    let file = RC_FILENAME_PATTERN;
    format!(r"^{root}/([a-z][a-z0-9_-]*)/({verbs})/{file}$")
}

fn rc_path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&rc_path_pattern()).expect("rc path regex compiles"))
}

/// Whether a bare filename is a canonical rc script name (`SXX-name.kai`
/// or `SXX-name.md`).
///
/// The lifecycle runner uses this to decide what in a verb directory is a
/// script. A `.kai` or `.md` file that fails this check is a hard error
/// there rather than a silent skip: a `.md` in a verb directory reaches
/// the model's system-prompt slot, so quietly ignoring an unexpected one
/// hides exactly the mistake worth catching. Non-script data belongs
/// outside a verb directory — see `docs/rc-on-disk.md`.
pub fn is_rc_script_filename(name: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"^{RC_FILENAME_PATTERN}$")).expect("rc filename regex compiles")
    })
    .is_match(name)
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
/// Format: `/config/rc/<context_type>/<verb>/SXX-name.{kai,md}`. Type and
/// name are lowercase identifiers (`[a-z][a-z0-9_-]*`); sort_key matches
/// `S\d{1,3}`. Valid verbs are [`crate::kj::lifecycle::RC_VERBS`].
pub fn parse_rc_path(path: &str) -> Result<RcPathParts, String> {
    let caps = rc_path_regex().captures(path).ok_or_else(|| {
        let verbs = crate::kj::lifecycle::RC_VERBS.join(", ");
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
        // No capability gate on writes. `/config/rc` is an ordinary write
        // surface like `/config/kernel` — the same file tools that can already
        // write it enforce nothing of their own, so a gate here would deny
        // `kj rc add` to a caller who could achieve the identical result
        // with `builtin.file:write`.
        // An rc write goes to the host file, not the FileDocumentCache shadow
        // that backs kaish `cat` and the file tools — capture the path so we
        // can drop that stale shadow after a successful write.
        let write_path = match &parsed.command {
            RcCommand::Add { path, .. } | RcCommand::Rm { path } => Some(path.clone()),
            _ => None,
        };
        let result = match parsed.command {
            RcCommand::Add { path, content } => {
                self.rc_add(&path, content.as_deref(), caller).await
            }
            RcCommand::List {
                type_filter,
                verb_filter,
                json,
            } => {
                self.rc_list(type_filter.as_deref(), verb_filter.as_deref(), json)
                    .await
            }
            RcCommand::Rm { path } => self.rc_rm(&path).await,
            RcCommand::Show { path, json } => self.rc_show(&path, json).await,
        };
        if let Some(path) = write_path
            && matches!(result, KjResult::Ok { .. })
        {
            self.kernel().invalidate_config_file_cache(&path);
        }
        result
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

        // `add` installs; it never replaces. Replacing is an ordinary file
        // write, and naming that is more useful than a second rc verb for it.
        if self.rc_exists(path).await {
            return KjResult::Err(format!(
                "kj rc add: '{path}' already exists\n\
                 rewrite it in place with `vi {path}`, or `kj rc rm {path}` first"
            ));
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
    /// whatever backend is mounted at `/config/rc` — a host file in
    /// production. No FileDocumentCache mirror sits in the way; dispatch
    /// (`load_rc_scripts`) reads the same file through the same VFS.
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
            // A symlinked (composed) script counts as present: `rm`/`add`-clobber
            // and `reset`-replace must see it, not treat it as absent.
            .map(|a| a.is_file() || a.is_symlink())
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

    /// The raw symlink target at `path`, or `None` when the path is not a link
    /// (regular file, absent, or unmounted). Used to annotate `kj rc list`/`show`
    /// so init.d-style composed links read as links, not opaque files.
    async fn rc_link_target(&self, path: &str) -> Option<String> {
        use crate::vfs::VfsOps;
        self.kernel()
            .vfs()
            .readlink(std::path::Path::new(path))
            .await
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    }

    /// Build the `(path, live_link, seed_status)` triple for every rc path
    /// this kernel knows about — every live path under `/config/rc` (`type_filter`/
    /// `verb_filter` narrow the walk, as `kj rc list` does) plus every embedded
    /// seed with nothing live at its path (reported as [`RcSeedStatus::NotInstalled`]
    /// rather than silently omitted — the anti-join `kj rc list` has always
    /// done).
    async fn rc_status_rows(
        &self,
        type_filter: Option<&str>,
        verb_filter: Option<&str>,
    ) -> Result<Vec<(String, Option<String>, RcSeedStatus)>, String> {
        let mut paths = self.walk_rc_paths().await?;
        let matches_filters = |p: &str| {
            let parts = match parse_rc_path(p) {
                Ok(parts) => parts,
                Err(_) => return false, // stray non-canonical file
            };
            type_filter.is_none_or(|t| parts.context_type == t)
                && verb_filter.is_none_or(|v| parts.verb == v)
        };
        paths.retain(|p| matches_filters(p));
        paths.sort();

        // Anti-join: a seed that ships in this binary with nothing live at its
        // path. The namespace is seeded only when it is entirely empty, so a
        // script added to the embedded set after this kernel was first seeded
        // never lands on its own and is otherwise invisible here — the listing
        // walks live paths, and a path with nothing at it has no entry to walk.
        // Reporting it is the whole point: `kaijutsu-server rc reseed` is the
        // install.
        let live: std::collections::HashSet<&str> = paths.iter().map(|p| p.as_str()).collect();
        let mut missing: Vec<String> = crate::seed_scripts::seed_files()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| !live.contains(p.as_str()) && matches_filters(p))
            .collect();
        missing.sort();

        // One pass: resolve each live path's symlink target (if any) and its
        // seed staleness.
        let mut rows: Vec<(String, Option<String>, RcSeedStatus)> =
            Vec::with_capacity(paths.len() + missing.len());
        for p in &paths {
            let link = self.rc_link_target(p).await;
            let status = self.rc_seed_status(p, link.as_deref()).await;
            rows.push((p.clone(), link, status));
        }
        // Not-installed rows have no live link to resolve and skip the
        // body comparison — there is no body.
        rows.extend(
            missing
                .into_iter()
                .map(|p| (p, None, RcSeedStatus::NotInstalled)),
        );
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(rows)
    }

    async fn rc_list(
        &self,
        type_filter: Option<&str>,
        verb_filter: Option<&str>,
        json: bool,
    ) -> KjResult {
        let rows = match self.rc_status_rows(type_filter, verb_filter).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj rc list: {e}")),
        };

        if rows.is_empty() {
            return KjResult::ok_with_data(
                "(no rc scripts)".to_string(),
                serde_json::Value::Array(Vec::new()),
            );
        }

        // `data` stays an array of full path strings (the resolver keys for
        // `kj rc rm`/`show`) per the kj structured-data convention
        // (`project_kj_structured_data.md`: list commands emit an array of
        // identifier strings so `for x in $(kj …)` iterates handles) — the
        // per-entry seed status below rides `--json`'s message instead of
        // reshaping `data` into an array of records. A not-installed row has
        // nothing live to resolve to, so it's excluded here same as before.
        let data = serde_json::Value::Array(
            rows.iter()
                .filter(|(_, _, status)| *status != RcSeedStatus::NotInstalled)
                .map(|(p, _, _)| serde_json::Value::String(p.clone()))
                .collect(),
        );

        if json {
            let scripts: Vec<serde_json::Value> = rows
                .iter()
                .map(|(p, link, status)| {
                    serde_json::json!({
                        "path": p,
                        "link": link,
                        "seed_status": status.as_json_str(),
                    })
                })
                .collect();
            let out = serde_json::json!({ "count": rows.len(), "scripts": scripts });
            return KjResult::ok_with_data(out.to_string(), data);
        }

        let mut lines = Vec::with_capacity(rows.len());
        for (p, link, status) in &rows {
            let marker = format!(" [{}]", status.as_str());
            match link {
                Some(target) => lines.push(format!("  {p} → {target}{marker}")),
                None => lines.push(format!("  {p}{marker}")),
            }
        }
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// Compare a live rc script at `path` against its embedded seed, without
    /// touching anything — an indicator only (`kaijutsu-server rc reseed` is
    /// the pull; live is truth by design). `live_link` is the raw
    /// symlink target at `path` (the caller already resolved it for the `→
    /// target` annotation, so this reuses it instead of re-issuing the same
    /// VFS readlink).
    ///
    /// A seed whose body is itself a link-target path ([`config_doc_fs::
    /// seed_link_target`], the init.d composition seeding reconstructs as a
    /// symlink) compares link-target-to-link-target; every other seed compares
    /// literal body-to-body. Mixing the two (a live file where the seed wants
    /// a link, or vice versa) is `Differs`, never silently treated as a match.
    async fn rc_seed_status(&self, path: &str, live_link: Option<&str>) -> RcSeedStatus {
        // A link that resolves to nothing outranks every seed comparison, and
        // is checked before the seed lookup so a user-authored link reports it
        // too. Comparing target strings alone cannot see this: the link matches
        // its seed exactly and still breaks every lifecycle run through it.
        // `Ok(None)` is the absent-target signal; `Err` is a read fault, which
        // is a different problem and is left to the comparison below.
        if live_link.is_some()
            && matches!(self.read_rc_content(path).await, Ok(None))
        {
            return RcSeedStatus::Dangling;
        }
        let Some(seed) = crate::seed_scripts::seed_body(path) else {
            return RcSeedStatus::NoSeed;
        };
        let known: std::collections::HashSet<String> = crate::seed_scripts::seed_files()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let expected_link =
            crate::runtime::config_doc_fs::seed_link_target(path, seed, &known);

        match (expected_link.as_deref(), live_link) {
            (Some(want), Some(got)) => {
                if want == got {
                    RcSeedStatus::InSync
                } else {
                    RcSeedStatus::Differs
                }
            }
            // Seed wants a symlink but live is a real file (or absent) — or
            // vice versa: a mismatched shape is a divergence, not a partial
            // match.
            (Some(_), None) | (None, Some(_)) => RcSeedStatus::Differs,
            (None, None) => match self.read_rc_content(path).await {
                Ok(Some(body)) if body == seed => RcSeedStatus::InSync,
                Ok(_) | Err(_) => RcSeedStatus::Differs,
            },
        }
    }

    /// Walk the `/config/rc` tree (`<type>/<verb>/SXX-name.ext`) and return all
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
        for type_e in entries(vfs, paths::RC_ROOT).await?.into_iter().filter(|e| e.kind.is_dir()) {
            let type_dir = format!("{}/{}", paths::RC_ROOT, type_e.name);
            for verb_e in entries(vfs, &type_dir).await?.into_iter().filter(|e| e.kind.is_dir()) {
                let verb_dir = paths::rc_dir(&type_e.name, &verb_e.name);
                for file_e in entries(vfs, &verb_dir)
                    .await?
                    .into_iter()
                    // Include symlinks: init.d-style composed scripts are links.
                    .filter(|e| e.kind.is_file() || e.kind.is_symlink())
                {
                    if file_e.name.ends_with(".kai") || file_e.name.ends_with(".md") {
                        out.push(paths::rc_script_path(&type_e.name, &verb_e.name, &file_e.name));
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
        // Read straight from the kernel-owned backend. NotFound = absent script;
        // any other VfsError is a real backend failure (surfaced, not masked as
        // "not found").
        let content = match self.read_rc_content(path).await {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj rc show: '{path}' not found")),
            Err(e) => return KjResult::Err(format!("kj rc show: '{path}': {e}")),
        };

        // When `path` is a symlink, `content` above is the *followed target's*
        // body (read_all follows links). Surface the link relationship too.
        let symlink_target = self.rc_link_target(path).await;

        // Metadata is derived from the canonical path; provenance lives in
        // the kernel block, not here.
        let record = serde_json::json!({
            "path": path,
            "context_type": parts.context_type,
            "verb": parts.verb,
            "sort_key": parts.sort_key,
            "name": parts.name,
            "extension": parts.extension,
            "symlink": symlink_target,
            "content_length": content.len(),
            "content": content,
        });

        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }

        // A symlink gets an explicit `→ target` header line; the fenced content
        // below is what the link resolves to.
        let link_line = match &symlink_target {
            Some(t) => format!("symlink:    → {t}\n"),
            None => String::new(),
        };
        // Fence content with the extension so .md renders as markdown and
        // .kai displays as a shell-ish block in surfaces that highlight it.
        let out = format!(
            "path:       {}\ntype:       {}\nverb:       {}\nsort_key:   {}\nname:       {}\nextension:  {}\n{}length:     {} bytes\n\n```{}\n{}\n```\n",
            path,
            parts.context_type,
            parts.verb,
            parts.sort_key,
            parts.name,
            parts.extension,
            link_line,
            content.len(),
            parts.extension,
            content,
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }

    async fn rc_rm(&self, path: &str) -> KjResult {
        use crate::vfs::VfsOps;

        if !self.rc_exists(path).await {
            return KjResult::Err(format!("kj rc rm: '{path}' not found"));
        }
        // Delete the file straight through the VFS (no cache mirror).
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
            ("/config/rc/planner/create/S00-prompt.md", "planner", "create", "md"),
            ("/config/rc/coder/fork/S05-record.kai", "coder", "fork", "kai"),
            ("/config/rc/test_v2/attach/S100-many.md", "test_v2", "attach", "md"),
            ("/config/rc/long-name-here/drift/S0-noop.kai", "long-name-here", "drift", "kai"),
        ] {
            let parts = parse_rc_path(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(parts.context_type, expected_type, "type for {path}");
            assert_eq!(parts.verb, expected_verb, "verb for {path}");
            assert_eq!(parts.extension, expected_ext, "ext for {path}");
        }
    }

    #[test]
    fn path_rejects_uppercase() {
        assert!(parse_rc_path("/config/rc/Planner/create/S00-foo.md").is_err());
        assert!(parse_rc_path("/config/rc/planner/create/S00-Foo.md").is_err());
    }

    #[test]
    fn path_rejects_unknown_verb() {
        assert!(parse_rc_path("/config/rc/planner/spawn/S00-foo.md").is_err());
        assert!(parse_rc_path("/config/rc/planner/destroy/S00-foo.kai").is_err());
    }

    #[test]
    fn path_rejects_unknown_extension() {
        assert!(parse_rc_path("/config/rc/planner/create/S00-foo.sh").is_err());
        assert!(parse_rc_path("/config/rc/planner/create/S00-foo.txt").is_err());
    }

    #[test]
    fn path_rejects_missing_s_prefix() {
        assert!(parse_rc_path("/config/rc/planner/create/00-foo.md").is_err());
        assert!(parse_rc_path("/config/rc/planner/create/foo.md").is_err());
    }

    #[test]
    fn path_rejects_wrong_root() {
        assert!(parse_rc_path("/rc/planner/create/S00-foo.md").is_err());
        assert!(parse_rc_path("/config/init/planner/create/S00-foo.md").is_err());
    }

    #[test]
    fn attach_and_drift_install_paths_validate() {
        // Reserved-verb scripts validate now; lifecycle dispatch will
        // no-op them until those hooks land.
        assert!(parse_rc_path("/config/rc/test/attach/S00-foo.md").is_ok());
        assert!(parse_rc_path("/config/rc/test/drift/S00-foo.kai").is_ok());
    }

    /// The path validator and the firing gate share one verb list
    /// (`lifecycle::RC_VERBS`). Every canonical verb MUST parse — a verb the
    /// scheduler fires but `parse_rc_path` rejects is a migration trap (the
    /// rotate regression: an rc verb refused a path the beat scheduler runs).
    /// This fails the moment a verb is added to one source but not the other.
    #[test]
    fn every_canonical_verb_parses() {
        for verb in crate::kj::lifecycle::RC_VERBS {
            let path = format!("/config/rc/musician/{verb}/S10-x.kai");
            let parts = parse_rc_path(&path)
                .unwrap_or_else(|e| panic!("canonical verb {verb} must parse ({path}): {e}"));
            assert_eq!(&parts.verb, verb, "round-trip verb for {path}");
        }
    }

    /// The page-turn verb specifically — the one that regressed. `kj rc
    /// reset`/`edit` on the shipped rotate script must validate.
    #[test]
    fn rotate_path_validates() {
        assert!(parse_rc_path("/config/rc/musician/rotate/S10-rotate.kai").is_ok());
    }

    /// `kj rc show <path>` round-trips content from an earlier `kj rc add`
    /// and surfaces the metadata fields (path, type, verb, ext, timeout).
    #[tokio::test]
    async fn rc_show_round_trips_content_and_metadata() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/config/rc/showtest/create/S00-hello.kai"),
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
                    s("/config/rc/showtest/create/S00-hello.kai"),
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

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("show"), s("/config/rc/none/create/S00-noop.kai")],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("not found"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A kaish `cat`/file-tool read after `kj rc add` sees the new body, not
    /// the copy cached before it.
    ///
    /// **This does not pin the invalidation hook, and measurement is why.**
    /// Dropping `RcCommand::Add` from the dispatcher's `write_path` match
    /// leaves this test passing: `/config/rc` is host files now, so the shadow
    /// self-heals from the file's changed stat. The hook is belt-and-braces
    /// on this path. `rc_rm_invalidates_the_file_cache_shadow` below is the
    /// one that fails when its arm is dropped, because a removed file has no
    /// stat left to disagree with the shadow.
    #[tokio::test]
    async fn an_rc_add_is_visible_to_a_later_kaish_read() {
        use crate::block_store::SharedBlockStore;
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;
        use kaijutsu_types::{BlockId, ContextId};

        fn block_content(blocks: &SharedBlockStore, ctx: ContextId, block: &BlockId) -> String {
            blocks
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .find(|s| s.id == *block)
                .expect("shadow block present")
                .content
        }

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let path = "/config/rc/cachetest/create/S00-foo.kai";

        // The kernel's own file cache, over the dispatcher's store + kernel VFS.
        let cache = d.kernel().file_cache().clone();

        // Create the script, then read it through the cache to populate a shadow.
        d.dispatch(&[s("rc"), s("add"), s(path), s("--content"), s("echo old")], &c)
            .await;
        let (sctx, sblock) = cache.try_get_or_load(path).await.unwrap();
        assert_eq!(block_content(d.block_store(), sctx, &sblock), "echo old");

        // Remove the file BEHIND kj, straight through the VFS, so no
        // invalidation runs and the stale shadow is still there when `add`
        // writes over the path.
        {
            use crate::vfs::VfsOps;
            d.kernel()
                .vfs()
                .unlink(std::path::Path::new(path))
                .await
                .expect("unlink behind kj");
        }

        let result = d
            .dispatch(&[s("rc"), s("add"), s(path), s("--content"), s("echo new")], &c)
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "add failed: {result:?}");

        // The next cache read must reflect the write, by whichever route.
        let (sctx2, sblock2) = cache.try_get_or_load(path).await.unwrap();
        assert_eq!(
            block_content(d.block_store(), sctx2, &sblock2),
            "echo new",
            "a kaish read sees the rc write"
        );
    }

    /// The other surviving write verb, isolated the same way: `kj rc rm`
    /// must drop the shadow too, or a kaish `cat` keeps serving the body of
    /// a script that is gone — the worst version of this staleness, because
    /// the file it describes no longer exists to contradict it.
    ///
    /// Falsified by dropping `RcCommand::Rm` from the dispatcher's
    /// `write_path` match: the reload then still resolves and the shadow
    /// still reads "echo old". Reverted afterward.
    #[tokio::test]
    async fn rc_rm_invalidates_the_file_cache_shadow() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let path = "/config/rc/cachetest/create/S01-gone.kai";
        let cache = d.kernel().file_cache().clone();

        d.dispatch(&[s("rc"), s("add"), s(path), s("--content"), s("echo old")], &c)
            .await;
        cache.try_get_or_load(path).await.expect("shadow populated");

        let removed = d.dispatch(&[s("rc"), s("rm"), s(path)], &c).await;
        assert!(matches!(removed, KjResult::Ok { .. }), "rm failed: {removed:?}");

        assert!(
            cache.try_get_or_load(path).await.is_err(),
            "a removed rc script must not keep resolving through a stale shadow"
        );
    }

    /// `kj rc list` emits full absolute paths as iteration handles so
    /// `for s in $(kj rc list); do kj rc rm $s; done` works.
    #[tokio::test]
    async fn rc_list_emits_path_array() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Install two scripts via the dispatcher (round-trip through
        // `kj rc add` keeps the test honest about real path validation).
        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/config/rc/test/create/S00-noop.kai"),
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
                s("/config/rc/test/create/S01-second.kai"),
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
                    paths.contains(&"/config/rc/test/create/S00-noop.kai"),
                    "missing S00 in: {paths:?}"
                );
                assert!(
                    paths.contains(&"/config/rc/test/create/S01-second.kai"),
                    "missing S01 in: {paths:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// A composed (symlinked) rc script shows up in `kj rc list` with a
    /// `→ target` annotation, and `kj rc show` reports the link target plus the
    /// *followed* content. The link is created via `ln -s`-equivalent
    /// (`vfs().symlink`) — there is no `kj rc link`; the general VFS op is the
    /// surface.
    #[tokio::test]
    async fn rc_list_and_show_surface_symlinks() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;
        use crate::vfs::VfsOps;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // The shared canonical body under a `lib` type.
        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/config/rc/lib/create/S00-binding.kai"),
                s("--content"),
                s("kj binding allow drive"),
            ],
            &c,
        )
        .await;
        // Compose it into `coder` by symlink (the init.d move).
        d.kernel()
            .vfs()
            .symlink(
                std::path::Path::new("/config/rc/composed/create/S10-binding.kai"),
                std::path::Path::new("/config/rc/lib/create/S00-binding.kai"),
            )
            .await
            .expect("create rc symlink");

        // list: the link path appears in data (stable resolver key) and the
        // human message carries the `→ target` annotation.
        let listed = d.dispatch(&[s("rc"), s("list")], &c).await;
        match listed {
            KjResult::Ok { data: Some(v), message, .. } => {
                let paths: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(
                    paths.contains(&"/config/rc/composed/create/S10-binding.kai"),
                    "symlink missing from list data: {paths:?}"
                );
                assert!(
                    message.contains(
                        "/config/rc/composed/create/S10-binding.kai → /config/rc/lib/create/S00-binding.kai"
                    ),
                    "list message lacks arrow annotation: {message}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }

        // show: reports the link target and the followed content.
        let shown = d
            .dispatch(
                &[s("rc"), s("show"), s("/config/rc/composed/create/S10-binding.kai")],
                &c,
            )
            .await;
        match shown {
            KjResult::Ok { data: Some(v), .. } => {
                let obj = v.as_object().expect("object");
                assert_eq!(
                    obj["symlink"].as_str(),
                    Some("/config/rc/lib/create/S00-binding.kai"),
                    "show should report link target"
                );
                assert_eq!(
                    obj["content"].as_str(),
                    Some("kj binding allow drive"),
                    "show should follow the link to target content"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    // ── New: rc list seed-staleness indicator ──────────────────────────

    /// An untouched, freshly-seeded literal-content script (the coder stance)
    /// is marked in-sync — no divergence from its embedded default.
    #[tokio::test]
    async fn rc_list_marks_seeded_script_in_sync() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => assert!(
                message.contains("/config/rc/coder/create/S00-stance.kai [in-sync]"),
                "expected in-sync marker: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Editing a seeded script's body away from its embedded default flips
    /// its `kj rc list` marker to "differs from seed" — an indicator only;
    /// nothing auto-restores it. This is the report `kj rc list` earns its
    /// keep with: the filesystem cannot tell you a file has drifted from the
    /// seed it shipped as.
    ///
    /// The edit is a plain file write, because that is how a player edits an
    /// rc script now — the editor, the file tools, or host `vim`.
    #[tokio::test]
    async fn rc_list_marks_edited_seeded_script_as_differs() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.write_rc_file("/config/rc/coder/create/S00-stance.kai", "# user override")
            .await
            .expect("writing an rc script is an ordinary file write");

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => assert!(
                message.contains("/config/rc/coder/create/S00-stance.kai [differs from seed]"),
                "expected differs marker: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// A live-only script with no embedded seed (user-authored context_type)
    /// is marked "no seed" — neither in-sync nor differing, since there's
    /// nothing to compare it against.
    #[tokio::test]
    async fn rc_list_marks_user_only_script_as_no_seed() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/config/rc/mine/create/S00-custom.kai"),
                s("--content"),
                s("true"),
            ],
            &c,
        )
        .await;

        let result = d
            .dispatch(&[s("rc"), s("list"), s("--type"), s("mine")], &c)
            .await;
        match result {
            KjResult::Ok { message, .. } => assert!(
                message.contains("/config/rc/mine/create/S00-custom.kai [no seed]"),
                "expected no-seed marker: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// An untouched seed *symlink* (the init.d-style composed cache script)
    /// is in-sync too — the comparison follows the link-target-vs-seed-body
    /// rule, not a literal-content diff.
    #[tokio::test]
    async fn rc_list_marks_seed_symlink_in_sync() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("default"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => assert!(
                message.contains(
                    "/config/rc/default/create/S20-cache.kai → /config/rc/lib/create/S20-cache.kai [in-sync]"
                ),
                "expected in-sync symlink marker: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Replacing a seeded symlink with a literal file at the same path is a
    /// shape mismatch against the seed (which expects a link there) — marked
    /// "differs from seed", and it no longer carries the `→ target`
    /// annotation since it's a real file now.
    #[tokio::test]
    async fn rc_list_marks_symlink_replaced_by_real_file_as_differs() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(
            &[s("rc"), s("rm"), s("/config/rc/default/create/S20-cache.kai")],
            &c,
        )
        .await;
        d.dispatch(
            &[
                s("rc"),
                s("add"),
                s("/config/rc/default/create/S20-cache.kai"),
                s("--content"),
                s("# diverged, no longer a link"),
            ],
            &c,
        )
        .await;

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("default"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => {
                assert!(
                    message.contains("/config/rc/default/create/S20-cache.kai [differs from seed]"),
                    "expected differs marker: {message}"
                );
                assert!(
                    !message.contains("/config/rc/default/create/S20-cache.kai →"),
                    "should no longer show a symlink annotation: {message}"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `--json` carries the same per-entry seed_status fact in its message
    /// object; `.data` stays the flat path-string array regardless (the
    /// list-command iteration convention — `project_kj_structured_data.md`).
    #[tokio::test]
    async fn rc_list_json_carries_per_entry_seed_status() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        let result = d
            .dispatch(
                &[
                    s("rc"),
                    s("list"),
                    s("--type"),
                    s("coder"),
                    s("--verb"),
                    s("create"),
                    s("--json"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, data: Some(v), .. } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&message).expect("--json message is JSON");
                let scripts = parsed["scripts"].as_array().expect("scripts array");
                let entry = scripts
                    .iter()
                    .find(|e| e["path"] == "/config/rc/coder/create/S00-stance.kai")
                    .expect("stance entry present");
                assert_eq!(entry["seed_status"], "in_sync");

                let paths: Vec<&str> = v
                    .as_array()
                    .expect("data stays an array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(
                    paths.contains(&"/config/rc/coder/create/S00-stance.kai"),
                    "data must stay the flat path array even under --json: {paths:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// A seed that ships in this binary with nothing live at its path is
    /// reported as "not installed" rather than being silently absent from the
    /// listing. This is the state a kernel lands in whenever a script is added
    /// to the embedded set after that kernel was first seeded: the namespace
    /// seeds only when it is entirely empty, so the new path never arrives on
    /// its own, and a listing that walks live paths has no entry to show. `rm`
    /// is the reachable way to produce the same state in a test.
    #[tokio::test]
    async fn rc_list_marks_a_seed_with_nothing_live_as_not_installed() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let path = "/config/rc/coder/create/S00-stance.kai";

        d.dispatch(&[s("rc"), s("rm"), s(path)], &c).await;

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, data: Some(v), .. } => {
                assert!(
                    message.contains(&format!("{path} [not installed]")),
                    "a seed with nothing live must be reported, not omitted: {message}"
                );
                // `data` stays the live-path array: it is the resolver key list
                // for `kj rc show`/`rm`, and a not-installed path resolves to
                // nothing — its path comes from the listing text.
                let paths: Vec<&str> = v
                    .as_array()
                    .expect("data stays an array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(
                    !paths.contains(&path),
                    "data must not offer a path with nothing live at it: {paths:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The not-installed marker rides `--json` as `not_installed` too, so a
    /// client can find the gap without parsing the human listing.
    #[tokio::test]
    async fn rc_list_json_carries_not_installed_status() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let path = "/config/rc/coder/create/S00-stance.kai";

        d.dispatch(&[s("rc"), s("rm"), s(path)], &c).await;

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create"), s("--json")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&message).expect("--json message is JSON");
                let scripts = parsed["scripts"].as_array().expect("scripts array");
                let entry = scripts
                    .iter()
                    .find(|e| e["path"] == path)
                    .expect("removed seed still appears as an entry");
                assert_eq!(entry["seed_status"], "not_installed");
                assert!(entry["link"].is_null(), "nothing live means no link target");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Removing the target of a seed symlink leaves every link to it dangling.
    /// The link still matches its seed byte for byte, so a status that compares
    /// target strings reports it healthy; the marker resolves the link instead
    /// and says "dangling". This is the more urgent fact than any seed
    /// comparison — the lifecycle loader treats an unreadable entry as fatal,
    /// so one dangling link fails every run of the verb it sits in.
    #[tokio::test]
    async fn rc_list_reports_a_dangling_seed_symlink_as_dangling() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let target = "/config/rc/lib/create/S20-cache.kai";
        let link = "/config/rc/coder/create/S20-cache.kai";

        // Precondition: the link is a real seed symlink onto the shared target.
        let before = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match before {
            KjResult::Ok { message, .. } => assert!(
                message.contains(&format!("{link} → {target} [in-sync]")),
                "expected a healthy seed symlink first: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }

        d.dispatch(&[s("rc"), s("rm"), s(target)], &c).await;

        // The link now points at nothing. Reading through it fails...
        let show = d.dispatch(&[s("rc"), s("show"), s(link)], &c).await;
        assert!(
            matches!(show, KjResult::Err(_)),
            "reading through a dangling link must fail, got {show:?}"
        );

        // ...and the listing says so instead of calling it healthy.
        let after = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("coder"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match after {
            KjResult::Ok { message, .. } => assert!(
                message.contains(&format!("{link} → {target} [dangling — target missing]")),
                "a link resolving to nothing must not read in-sync: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// A dangling link that has no embedded seed at all still reports
    /// "dangling", not "no seed": the broken target is what fails the
    /// lifecycle, and it is the fact worth surfacing either way.
    #[tokio::test]
    async fn rc_list_reports_a_dangling_unseeded_symlink_as_dangling() {
        use crate::kj::test_helpers::*;
        use crate::kj::KjResult;

        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let target = "/config/rc/bassist/create/S05-chair.md";
        let link = "/config/rc/bassist/create/S00-stance.md";

        // Point a user-authored link at another user-authored path, then
        // remove the target out from under it.
        d.dispatch(&[s("rc"), s("add"), s(target), s("--content"), s("# chair")], &c)
            .await;
        d.dispatch(&[s("rc"), s("rm"), s(link)], &c).await;
        use crate::vfs::VfsOps;
        let made = d
            .kernel()
            .vfs()
            .symlink(std::path::Path::new(link), std::path::Path::new(target))
            .await;
        assert!(made.is_ok(), "symlink setup failed: {made:?}");
        d.dispatch(&[s("rc"), s("rm"), s(target)], &c).await;

        let result = d
            .dispatch(
                &[s("rc"), s("list"), s("--type"), s("bassist"), s("--verb"), s("create")],
                &c,
            )
            .await;
        match result {
            KjResult::Ok { message, .. } => assert!(
                message.contains(&format!("{link} → {target} [dangling — target missing]")),
                "an unseeded dangling link must report dangling, not no-seed: {message}"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

}
