//! `kj config` — read config files, and restore one to its shipped default.
//!
//! Config files (`system.md`, `theme.toml`, `mcp.toml`) live at `/etc/config`
//! (`docs/config-ownership.md`), with per-client overrides at
//! `/etc/client`.
//!
//! **There is no write verb here, deliberately.** Config is a file: write it
//! with the ordinary file tools, or open it with `kj editor <path>` / the `vi`
//! builtin. `set` and `edit` existed only because `/etc/config` used to be
//! unreachable from `builtin.file:write` — once that flat deny was narrowed to
//! the host's own `/etc`, they were a second way to do one thing, with a
//! capability guarding one of the two doors. Both are gone, and with them the
//! `config-write` gate on this surface: it would have denied `reset` to a
//! caller who could achieve the identical result by writing the file.
//! `rc-write` still guards `/etc/rc`, which is executable rather than data.
//!
//! What survives is what has no file-tool equivalent: `list`, `show`, and
//! `reset` — restoring a file to the default embedded in the binary, which is
//! the reseed tool Amy asked to keep.
//!
//! Model configuration is **not** here. It is SQL-native — `kj backend`,
//! `kj cast`, `kj alias` over `kernel_db` tables — and `models.toml` was
//! demolished along with its CRDT doc. The write-time provider-type check
//! this module used to carry moved with it: the closed set now lives in
//! `BackendKind`, enforced by `kj backend set` and a SQL CHECK.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths::{CLIENT_ROOT, CONFIG_ROOT};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "config",
    about = "Read config: kernel-global at /etc/config (system.md, theme.toml, mcp.toml) + per-client at /etc/client (metronome.toml). To CHANGE a config file, just write it with the file tools or open it with `kj editor` — there is no set/edit verb. Model config is SQL-native — see `kj backend`/`kj cast`/`kj alias`.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// List the config files the CRDT currently holds.
    #[command(alias = "ls")]
    List {
        /// Emit a JSON array of names instead of a labelled view
        #[arg(long)]
        json: bool,
    },
    /// Print one config file's content.
    #[command(alias = "cat")]
    Show {
        /// Config file name (e.g. theme.toml) or full /etc/config path
        path: String,
        /// Emit a JSON object instead of a labelled view
        #[arg(long)]
        json: bool,
        /// Emit exactly the stored content — no path/length header, no code
        /// fence. Round-trips byte-identical through `builtin.file:write`.
        #[arg(long, conflicts_with = "json")]
        raw: bool,
    },
    // `set` and `edit` are deliberately absent. Config is a file: write it with
    // the ordinary file tools, or open it with `kj editor <path>` / the `vi`
    // builtin, exactly as you would any other file. Config is not a special
    // category owed its own write verbs (Amy, 2026-08-15). `show --raw` still
    // round-trips byte-identical, now into `builtin.file:write` rather than
    // into a verb that existed only because `/etc/config` used to be
    // unreachable from the file tools.
    /// Restore a config file to its embedded default. Errors if the path ships
    /// no built-in seed — there is nothing to reset it to.
    Reset {
        /// Config file name (e.g. theme.toml) or full /etc/config path
        path: String,
    },
}

/// Canonicalize a user-supplied config arg to its `/etc/config/<name>` path.
/// Accepts a bare name (`theme.toml`) or an already-full path. Rejects nested
/// paths and parent escapes — config is a flat namespace.
fn config_canonical(path: &str) -> Result<String, String> {
    let trimmed = path.trim();
    // Per-client config namespace: hierarchical. `<root>/<file>` (shared client
    // default) or `<root>/<client-id>/<file>` (one client's override) — at most
    // one nesting level, no parent escapes.
    if trimmed == CLIENT_ROOT || trimmed.starts_with(&format!("{CLIENT_ROOT}/")) {
        let rest = trimmed
            .strip_prefix(&format!("{CLIENT_ROOT}/"))
            .unwrap_or("")
            .trim_matches('/');
        if rest.is_empty() {
            return Err(format!(
                "missing config file name under {CLIENT_ROOT} (e.g. metronome.toml)"
            ));
        }
        let segments: Vec<&str> = rest.split('/').collect();
        if segments.len() > 2 || segments.iter().any(|s| s.is_empty() || *s == ".." || *s == ".") {
            return Err(format!(
                "invalid client config path '{path}': expected \
                 {CLIENT_ROOT}/<file> or {CLIENT_ROOT}/<client-id>/<file>"
            ));
        }
        return Ok(format!("{CLIENT_ROOT}/{rest}"));
    }
    // Kernel-global config: a flat namespace under /etc/config.
    let name = trimmed
        .strip_prefix(&format!("{CONFIG_ROOT}/"))
        .unwrap_or(trimmed)
        .trim_start_matches('/');
    if name.is_empty() {
        return Err("missing config file name (e.g. theme.toml)".to_string());
    }
    if name.contains('/') || name == ".." || name == "." {
        return Err(format!(
            "invalid config path '{path}': config is a flat namespace under {CONFIG_ROOT}"
        ));
    }
    Ok(format!("{CONFIG_ROOT}/{name}"))
}

impl KjDispatcher {
    pub(crate) async fn dispatch_config(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<ConfigArgs>();
        }
        let parsed = match ConfigArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj config: {e}"));
            }
        };
        // No capability gate. Config is ungated for the same reason it has no
        // write verbs: it is a file, and the file tools that can already write
        // it enforce nothing of their own. A gate here would have been theatre
        // — it would deny `kj config reset` to a caller who could achieve the
        // identical result with `builtin.file:write`. `rc-write` still guards
        // `/etc/rc`, which is executable rather than data.
        //
        // A `reset` touches the ConfigDocFs block, not the FileDocumentCache
        // shadow that backs kaish `cat`/file tools — capture the canonical path
        // so we can drop the stale shadow after a success.
        let write_path = match &parsed.command {
            ConfigCommand::Reset { path } => config_canonical(path).ok(),
            _ => None,
        };
        let result = match parsed.command {
            ConfigCommand::List { json } => self.config_list(json).await,
            ConfigCommand::Show { path, json, raw } => self.config_show(&path, json, raw).await,
            ConfigCommand::Reset { path } => self.config_reset(&path).await,
        };
        if let Some(canonical) = write_path
            && matches!(result, KjResult::Ok { .. })
        {
            self.kernel().invalidate_config_file_cache(&canonical);
        }
        result
    }

    /// Read a config file's content from the VFS. `Ok(None)` for an absent file
    /// (NotFound / no mount); `Err` for a real backend failure or non-UTF-8
    /// content — never masked as "not found".
    async fn read_config_content(&self, canonical: &str) -> Result<Option<String>, String> {
        use crate::vfs::{VfsError, VfsOps};
        let bytes = match self
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(canonical))
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

    /// Write `content` straight through the VFS to the kernel-owned config
    /// backend. There is no host file and no FileDocumentCache mirror.
    async fn write_config_content(&self, canonical: &str, content: &str) -> Result<(), String> {
        use crate::vfs::VfsOps;
        self.kernel()
            .vfs()
            .write_all(std::path::Path::new(canonical), content.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    async fn config_list(&self, json: bool) -> KjResult {
        use crate::vfs::{VfsError, VfsOps};
        // readdir a directory, mapping "absent" (no mount, nothing seeded yet)
        // to an empty listing rather than an error.
        async fn dir_entries(
            vfs: &crate::vfs::MountTable,
            dir: &str,
        ) -> Result<Vec<crate::vfs::DirEntry>, String> {
            match vfs.readdir(std::path::Path::new(dir)).await {
                Ok(e) => Ok(e),
                Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Ok(Vec::new()),
                Err(e) => Err(format!("readdir {dir}: {e}")),
            }
        }

        let vfs = self.kernel().vfs();
        let config_entries = match dir_entries(vfs, CONFIG_ROOT).await {
            Ok(e) => e,
            Err(e) => return KjResult::Err(format!("kj config list: {e}")),
        };
        let mut names: Vec<String> = config_entries
            .into_iter()
            .filter(|e| e.kind.is_file())
            .map(|e| e.name)
            .collect();

        // Per-client config namespace (config_canonical's "at most one nesting
        // level" shape): shared defaults flat at /etc/client/<file>, one
        // override level at /etc/client/<client-id>/<file>. Listed as full
        // paths (not bare names) since `kj config show` needs the CLIENT_ROOT
        // prefix to disambiguate them from /etc/config names.
        let client_top = match dir_entries(vfs, CLIENT_ROOT).await {
            Ok(e) => e,
            Err(e) => return KjResult::Err(format!("kj config list: {e}")),
        };
        for entry in client_top {
            if entry.kind.is_file() {
                names.push(format!("{CLIENT_ROOT}/{}", entry.name));
            } else if entry.kind.is_dir() {
                let client_dir = format!("{CLIENT_ROOT}/{}", entry.name);
                let client_files = match dir_entries(vfs, &client_dir).await {
                    Ok(e) => e,
                    Err(e) => return KjResult::Err(format!("kj config list: {e}")),
                };
                for file in client_files.into_iter().filter(|e| e.kind.is_file()) {
                    names.push(format!("{client_dir}/{}", file.name));
                }
            }
        }
        names.sort();

        // Iteration handles accepted by `kj config show/set`: bare names for
        // /etc/config, full /etc/client/... paths for the per-client namespace.
        let data = serde_json::Value::Array(
            names
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        );
        if json {
            return KjResult::ok_with_data(data.to_string(), data);
        }
        if names.is_empty() {
            return KjResult::ok_with_data("(no config files)".to_string(), data);
        }
        let lines: Vec<String> = names.iter().map(|n| format!("  {n}")).collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    async fn config_show(&self, path: &str, json: bool, raw: bool) -> KjResult {
        let canonical = match config_canonical(path) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj config show: {e}")),
        };
        let content = match self.read_config_content(&canonical).await {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj config show: '{canonical}' not found")),
            Err(e) => return KjResult::Err(format!("kj config show: '{canonical}': {e}")),
        };

        if raw {
            // Exactly the stored content — no header, no fence — so piping it
            // into a file and `kj config set`-ing it back round-trips
            // byte-identical instead of storing the decoration as content.
            return KjResult::ok(content);
        }

        let name = canonical.rsplit('/').next().unwrap_or(&canonical);
        let record = serde_json::json!({
            "path": canonical,
            "name": name,
            "content_length": content.len(),
            "content": content,
        });
        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }
        // Fence with the extension so .md renders as markdown and .toml as a
        // config block in surfaces that highlight it.
        let ext = name.rsplit('.').next().unwrap_or("");
        let out = format!(
            "path:    {canonical}\nlength:  {} bytes\n\n```{ext}\n{content}\n```\n",
            content.len(),
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }

    async fn config_reset(&self, path: &str) -> KjResult {
        let canonical = match config_canonical(path) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj config reset: {e}")),
        };
        let Some(body) = crate::config_seed::config_seed_body(&canonical) else {
            return KjResult::Err(format!(
                "kj config reset: '{canonical}' has no built-in default (nothing to reset to)"
            ));
        };
        if let Err(e) = self.write_config_content(&canonical, body).await {
            return KjResult::Err(format!("kj config reset: {e}"));
        }
        KjResult::ok(format!(
            "reset config '{canonical}' to its embedded default"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn canonical_accepts_bare_and_full_rejects_nesting() {
        assert_eq!(
            config_canonical("theme.toml").unwrap(),
            "/etc/config/theme.toml"
        );
        assert_eq!(
            config_canonical("/etc/config/system.md").unwrap(),
            "/etc/config/system.md"
        );
        assert!(config_canonical("sub/dir.toml").is_err());
        assert!(config_canonical("/etc/config/a/b.toml").is_err());
        assert!(config_canonical("").is_err());
    }

    #[test]
    fn canonical_accepts_the_hierarchical_client_namespace() {
        // Shared client default (flat under /etc/client).
        assert_eq!(
            config_canonical("/etc/client/metronome.toml").unwrap(),
            "/etc/client/metronome.toml"
        );
        // One client's override: exactly one nesting level (<client-id>/<file>).
        assert_eq!(
            config_canonical("/etc/client/abc-123/metronome.toml").unwrap(),
            "/etc/client/abc-123/metronome.toml"
        );
        // Deeper nesting, parent escapes, and a bare mount root are rejected.
        assert!(config_canonical("/etc/client/a/b/c.toml").is_err());
        assert!(config_canonical("/etc/client/../secret").is_err());
        assert!(config_canonical("/etc/client").is_err(), "needs a file name");
        assert!(config_canonical("/etc/client/").is_err());
    }

    /// `kj config show theme.toml` round-trips the seeded default.
    #[tokio::test]
    async fn show_round_trips_seeded_theme() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("config"), s("show"), s("theme.toml")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let obj = v.as_object().expect("show emits an object");
                assert_eq!(obj["path"].as_str(), Some("/etc/config/theme.toml"));
                assert!(
                    obj["content"].as_str().is_some_and(|s| !s.is_empty()),
                    "seeded content present"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// `kj config list` emits the seeded file names as a JSON array.
    #[tokio::test]
    async fn list_emits_seeded_names() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("config"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let names: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                assert!(names.contains(&"theme.toml"), "names: {names:?}");
                assert!(names.contains(&"theme.toml"), "names: {names:?}");
                assert!(names.contains(&"system.md"), "names: {names:?}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// `kj config list` also surfaces the per-client namespace at
    /// `/etc/client` — the shared metronome default at the mount root, plus a
    /// per-client override written under a client id — not just `/etc/config`.
    #[tokio::test]
    async fn list_also_surfaces_client_namespace() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        // A per-client override, written the way anything writes config now.
        d.write_config_content("/etc/client/abc-123/metronome.toml", "enabled = false")
            .await
            .expect("client override is writable");

        let result = d.dispatch(&[s("config"), s("list")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let names: Vec<&str> = v
                    .as_array()
                    .expect("array")
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect();
                // /etc/config entries are still there, as bare names.
                assert!(names.contains(&"theme.toml"), "names: {names:?}");
                // The shared client default, seeded at the mount root.
                assert!(
                    names.contains(&"/etc/client/metronome.toml"),
                    "names: {names:?}"
                );
                // The per-client override, one nesting level down.
                assert!(
                    names.contains(&"/etc/client/abc-123/metronome.toml"),
                    "names: {names:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// A write then `show` reflects the new content via the live backend.
    #[tokio::test]
    async fn a_write_then_show_reflects_new_content() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        d.write_config_content("/etc/config/theme.toml", "bg = \"#000000\"")
            .await
            .expect("theme is writable");

        let show = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--json")], &c)
            .await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["content"].as_str(), Some("bg = \"#000000\""));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj config show --raw` emits exactly the stored content — no
    /// path/length header, no code fence — so it round-trips byte-identical
    /// back through a write instead of storing the decoration.
    #[tokio::test]
    async fn show_raw_round_trips_byte_identical() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let body = "bg = \"#123456\"\nfg = \"#abcdef\"\n";
        d.write_config_content("/etc/config/theme.toml", body)
            .await
            .expect("theme is writable");

        let raw = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--raw")], &c)
            .await;
        let raw_message = match raw {
            KjResult::Ok { message, .. } => message,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(raw_message, body, "raw output must be exactly the content");

        // Round-trip: write it right back using the raw output as the body.
        d.write_config_content("/etc/config/theme.toml", &raw_message)
            .await
            .expect("round-trip write");

        let show = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--json")], &c)
            .await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["content"].as_str(), Some(body));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj config reset` restores a file to its embedded default after an edit.
    #[tokio::test]
    async fn reset_restores_embedded_default() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        d.write_config_content("/etc/config/theme.toml", "# clobbered")
            .await
            .expect("clobber the theme");
        // Prove the clobber landed — otherwise `reset` would be "restoring" a
        // file that was already the default and this test would pass on air.
        let clobbered = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--raw")], &c)
            .await;
        match clobbered {
            KjResult::Ok { ref message, .. } => {
                assert_eq!(message, "# clobbered", "the clobber must actually land")
            }
            ref other => panic!("expected Ok, got {other:?}"),
        }

        let reset = d
            .dispatch(&[s("config"), s("reset"), s("theme.toml")], &c)
            .await;
        assert!(
            matches!(reset, KjResult::Ok { .. }),
            "reset failed: {reset:?}"
        );

        let show = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--json")], &c)
            .await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(
                    v["content"].as_str(),
                    Some(crate::config_seed::DEFAULT_THEME),
                    "reset should restore the embedded default"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj config reset` on an unknown file errors instead of silently no-oping.
    #[tokio::test]
    async fn reset_unknown_file_errors() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("config"), s("reset"), s("nonesuch.toml")], &c)
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("no built-in default"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    // ── Fix 2 (2026-06-30 config papercuts): `kj config set` validates
    /// A config file is written with the ordinary file tools, and `show` sees
    /// it immediately — the claim that made `kj config set` deletable.
    ///
    /// `deny_etc_write` used to refuse every path under `/etc`, so this write
    /// was impossible and the verb was the only door. The assertion is that
    /// the door is now the same one every other file uses.
    #[tokio::test]
    async fn a_file_tool_write_reaches_config_and_show_sees_it() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let body = "bg = \"#0d0d0d\"\n";

        // The path guard is what stood in the way; assert it lets config through.
        assert!(
            crate::file_tools::path::deny_etc_write("/etc/config/theme.toml").is_none(),
            "config must be an ordinary write surface for the file tools"
        );

        // Write it the way a file tool does — straight through the VFS.
        d.write_config_content("/etc/config/theme.toml", body)
            .await
            .expect("config is writable through the VFS");

        let show = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--raw")], &c)
            .await;
        match show {
            KjResult::Ok { message, .. } => assert_eq!(
                message, body,
                "show --raw returns exactly what was written"
            ),
            other => panic!("expected Ok, got {other:?}"),
        }
    }
}
