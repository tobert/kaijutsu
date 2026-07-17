//! `kj config` — read and edit the CRDT-owned config files.
//!
//! Config files (`models.toml`, `system.md`, `theme.toml`, `mcp.toml`) live at
//! `/etc/config` on the same CRDT-native backend as `/etc/rc` (slice 2,
//! `docs/config-crdt-ownership.md`): the kernel is the sole owner — no host
//! file, no write-through. `show`/`list` read the live CRDT; `set` writes it
//! (requiring `--content` or piped stdin); `edit` does the same but opens an
//! interactive vi session (the `kj rc edit` analog) when no body is given;
//! `reset` restores one file to its embedded default.
//!
//! Writes go straight through the VFS to the CRDT backend (the admin-only
//! surface, bypassing the gated `builtin.file:write` tool), so the `config-write`
//! capability is enforced here — the only place it gates the `kj` surface.
//!
//! `models.toml` gets one extra write-time check (2026-06-30 config
//! papercuts, Fix 2): a `[providers.<name>]` table whose name isn't a
//! provider type `Provider::from_config` understands is rejected outright,
//! rather than silently dropped at boot (`initialize_llm_registry`) and
//! discovered only when a turn later hangs on the missing provider. See
//! [`validate_config_write`].

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths::{CLIENT_ROOT, CONFIG_ROOT};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "config",
    about = "CRDT-owned config: kernel-global at /etc/config (models.toml, system.md, theme.toml, mcp.toml) + per-client at /etc/client (metronome.toml)",
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
        /// Config file name (e.g. models.toml) or full /etc/config path
        path: String,
        /// Emit a JSON object instead of a labelled view
        #[arg(long)]
        json: bool,
        /// Emit exactly the stored content — no path/length header, no code
        /// fence. Round-trips byte-identical through `kj config set`.
        #[arg(long, conflicts_with = "json")]
        raw: bool,
    },
    /// Replace a config file's content (direct CRDT write). Requires a body
    /// (`--content` or piped stdin) — use `edit` with no body to open an
    /// interactive session instead.
    Set {
        /// Config file name (e.g. models.toml) or full /etc/config path
        path: String,
        /// Replacement body (stdin is piped here when omitted)
        #[arg(long)]
        content: Option<String>,
    },
    /// Edit a config file. With `--content` (or piped stdin) it replaces the
    /// body just like `set`; with no body it opens an interactive vi editor
    /// session on the file (docs/vi.md) — the `kj rc edit` analog `kj config`
    /// lacked.
    #[command(alias = "update")]
    Edit {
        /// Config file name (e.g. models.toml) or full /etc/config path
        path: String,
        /// Replacement body (omit to open the editor instead)
        #[arg(long)]
        content: Option<String>,
    },
    /// Restore a config file to its embedded default. Errors if the path ships
    /// no built-in seed — there is nothing to reset it to.
    Reset {
        /// Config file name (e.g. models.toml) or full /etc/config path
        path: String,
    },
}

/// Canonicalize a user-supplied config arg to its `/etc/config/<name>` path.
/// Accepts a bare name (`models.toml`) or an already-full path. Rejects nested
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
        return Err("missing config file name (e.g. models.toml)".to_string());
    }
    if name.contains('/') || name == ".." || name == "." {
        return Err(format!(
            "invalid config path '{path}': config is a flat namespace under {CONFIG_ROOT}"
        ));
    }
    Ok(format!("{CONFIG_ROOT}/{name}"))
}

/// Validate a config file's content before it's written to the CRDT.
///
/// Only `models.toml` gets structural validation today: the TOML must parse,
/// and every `[providers.<name>]` table name must be a provider type
/// `Provider::from_config` understands (`crate::llm::SUPPORTED_PROVIDER_TYPES`).
/// This is deliberately narrow — not a general schema validator, just the one
/// closed-set invariant that turns a silent boot-time drop
/// (`initialize_llm_registry`) into a loud write-time rejection, per the
/// house fail-loud posture (2026-06-30 config papercuts, Fix 2).
fn validate_config_write(canonical: &str, content: &str) -> Result<(), String> {
    if canonical != kaijutsu_types::paths::config_path("models.toml") {
        return Ok(());
    }
    let value: toml::Value = toml::from_str(content).map_err(|e| format!("invalid TOML: {e}"))?;
    let Some(providers) = value.get("providers").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for name in providers.keys() {
        if !crate::llm::SUPPORTED_PROVIDER_TYPES.contains(&name.as_str()) {
            return Err(crate::llm::unknown_provider_type_message(name));
        }
    }
    Ok(())
}

impl KjDispatcher {
    pub(crate) async fn dispatch_config(&self, argv: &[String], caller: &KjCaller) -> KjResult {
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
        // Writes go through the admin-only VFS path (not builtin.file:write), so
        // this is the only place `config-write` gates the kj surface. Reads stay
        // ungated.
        if matches!(
            parsed.command,
            ConfigCommand::Set { .. } | ConfigCommand::Edit { .. } | ConfigCommand::Reset { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "config")
        {
            return denied;
        }
        // A direct config write touches the ConfigCrdtFs block, not the
        // FileDocumentCache shadow that backs kaish `cat`/file tools — capture
        // the canonical path so we can drop the stale shadow after a success.
        // (The `edit`-opens-editor branch is covered too: invalidation is a
        // harmless reload there, and the editor self-invalidates on its writes
        // — mirrors `kj rc edit`.)
        let write_path = match &parsed.command {
            ConfigCommand::Set { path, .. }
            | ConfigCommand::Edit { path, .. }
            | ConfigCommand::Reset { path } => config_canonical(path).ok(),
            _ => None,
        };
        let result = match parsed.command {
            ConfigCommand::List { json } => self.config_list(json).await,
            ConfigCommand::Show { path, json, raw } => self.config_show(&path, json, raw).await,
            ConfigCommand::Set { path, content } => {
                self.config_set(&path, content.as_deref()).await
            }
            ConfigCommand::Edit { path, content } => {
                self.config_edit(&path, content.as_deref(), caller).await
            }
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

    /// Write `content` straight through the VFS to the CRDT-native config
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

    async fn config_set(&self, path: &str, content: Option<&str>) -> KjResult {
        let canonical = match config_canonical(path) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj config set: {e}")),
        };
        let content = match content {
            Some(c) => c,
            None => {
                return KjResult::Err(
                    "kj config set: missing content\n\
                     usage: kj config set <path> --content <body> (or pipe it: cat <file> | kj config set <path>)"
                        .to_string(),
                );
            }
        };
        if let Err(e) = validate_config_write(&canonical, content) {
            return KjResult::Err(format!("kj config set: {canonical}: {e}"));
        }
        if let Err(e) = self.write_config_content(&canonical, content).await {
            return KjResult::Err(format!("kj config set: {e}"));
        }
        KjResult::ok(format!(
            "set config '{canonical}' ({} bytes)",
            content.len()
        ))
    }

    /// `kj config edit`: with a body (`--content` or piped stdin) it's the same
    /// validate-then-write `set` does; with none it opens an interactive vi
    /// editor session on the owning CRDT block — the same
    /// `Kernel::editor_open_signaled` primitive `kj rc edit` uses (docs/vi.md
    /// step 4). Config has no symlink-composition concept (`config_canonical`
    /// enforces a flat namespace), so there's no analog to rc's composed-link
    /// guard here.
    async fn config_edit(&self, path: &str, content: Option<&str>, caller: &KjCaller) -> KjResult {
        let canonical = match config_canonical(path) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj config edit: {e}")),
        };

        let Some(content) = content else {
            let opener = caller
                .context_id
                .map(|context_id| crate::editor::EditorOpener {
                    principal: caller.principal_id,
                    context_id,
                    session_id: caller.session_id,
                });
            return match self
                .kernel()
                .editor_open_signaled(&canonical, self.block_store(), opener)
                .await
            {
                Ok((id, st)) => KjResult::ok_with_data(
                    format!(
                        "opened editor session {id} on {canonical} \
                         — drive it with `kj editor keys {id} …`",
                        id = id.as_u64(),
                    ),
                    st.to_json(id),
                ),
                Err(e) => KjResult::Err(format!("kj config edit: {e}")),
            };
        };

        if let Err(e) = validate_config_write(&canonical, content) {
            return KjResult::Err(format!("kj config edit: {canonical}: {e}"));
        }
        if let Err(e) = self.write_config_content(&canonical, content).await {
            return KjResult::Err(format!("kj config edit: {e}"));
        }
        KjResult::ok(format!(
            "set config '{canonical}' ({} bytes)",
            content.len()
        ))
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
            config_canonical("models.toml").unwrap(),
            "/etc/config/models.toml"
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

    /// `kj config show models.toml` round-trips the seeded default.
    #[tokio::test]
    async fn show_round_trips_seeded_models() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("config"), s("show"), s("models.toml")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let obj = v.as_object().expect("show emits an object");
                assert_eq!(obj["path"].as_str(), Some("/etc/config/models.toml"));
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
        let d = test_dispatcher_crdt_rc().await;
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
                assert!(names.contains(&"models.toml"), "names: {names:?}");
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
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        // A per-client override: config_set writes through config_canonical,
        // which recognizes the CLIENT_ROOT/<client-id>/<file> shape.
        let set = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("/etc/client/abc-123/metronome.toml"),
                    s("--content"),
                    s("enabled = false"),
                ],
                &c,
            )
            .await;
        assert!(matches!(set, KjResult::Ok { .. }), "set failed: {set:?}");

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
                assert!(names.contains(&"models.toml"), "names: {names:?}");
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

    /// `kj config set` then `show` reflects the new content via the live CRDT.
    #[tokio::test]
    async fn set_then_show_reflects_new_content() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let set = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("theme.toml"),
                    s("--content"),
                    s("bg = \"#000000\""),
                ],
                &c,
            )
            .await;
        assert!(matches!(set, KjResult::Ok { .. }), "set failed: {set:?}");

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
    /// path/length header, no code fence — so piping it into `kj config set`
    /// round-trips byte-identical instead of storing the decoration.
    #[tokio::test]
    async fn show_raw_round_trips_byte_identical_through_set() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let body = "bg = \"#123456\"\nfg = \"#abcdef\"\n";
        let set = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("theme.toml"),
                    s("--content"),
                    s(body),
                ],
                &c,
            )
            .await;
        assert!(matches!(set, KjResult::Ok { .. }), "set failed: {set:?}");

        let raw = d
            .dispatch(&[s("config"), s("show"), s("theme.toml"), s("--raw")], &c)
            .await;
        let raw_message = match raw {
            KjResult::Ok { message, .. } => message,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(raw_message, body, "raw output must be exactly the content");

        // Round-trip: set it right back using the raw output as the body.
        let set_again = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("theme.toml"),
                    s("--content"),
                    raw_message,
                ],
                &c,
            )
            .await;
        assert!(
            matches!(set_again, KjResult::Ok { .. }),
            "round-trip set failed: {set_again:?}"
        );

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
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.dispatch(
            &[
                s("config"),
                s("set"),
                s("models.toml"),
                s("--content"),
                s("# broken"),
            ],
            &c,
        )
        .await;
        let reset = d
            .dispatch(&[s("config"), s("reset"), s("models.toml")], &c)
            .await;
        assert!(
            matches!(reset, KjResult::Ok { .. }),
            "reset failed: {reset:?}"
        );

        let show = d
            .dispatch(&[s("config"), s("show"), s("models.toml"), s("--json")], &c)
            .await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(
                    v["content"].as_str(),
                    Some(crate::config_seed::DEFAULT_MODELS_CONFIG),
                    "reset should restore the embedded default"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj config set` is denied for a context without `config-write` — the gate
    /// is real, not advisory.
    #[tokio::test]
    async fn set_denied_without_config_write() {
        let d = test_dispatcher_crdt_rc().await;
        // A non-privileged caller whose context has no binding → no config-write.
        let c = caller_with_context(kaijutsu_crdt::ContextId::new());
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("theme.toml"),
                    s("--content"),
                    s("bg = \"#fff\""),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => assert!(
                msg.contains("config-write"),
                "denial should name the missing cap: {msg}"
            ),
            other => panic!("expected denial, got {other:?}"),
        }
    }

    /// `kj config reset` on an unknown file errors instead of silently no-oping.
    #[tokio::test]
    async fn reset_unknown_file_errors() {
        let d = test_dispatcher_crdt_rc().await;
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
    // ── `models.toml` before writing — invalid TOML or an unsupported
    // ── `[providers.<name>]` type is rejected loudly, not silently dropped
    // ── at the next boot.

    /// Invalid TOML syntax is rejected outright — never written to the CRDT.
    #[tokio::test]
    async fn set_models_toml_rejects_invalid_toml() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("models.toml"),
                    s("--content"),
                    s("[providers"),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("invalid TOML"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A `[providers.<name>]` table whose name isn't a provider type
    /// `Provider::from_config` understands is rejected at write time with the
    /// same wording `Provider::from_config` uses at boot — the whole point of
    /// Fix 2: catch the typo before it's saved, not after a turn hangs.
    #[tokio::test]
    async fn set_models_toml_rejects_unknown_provider_type() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let toml = "[providers.local-e4b]\nenabled = true\n";
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("models.toml"),
                    s("--content"),
                    s(toml),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(
                    msg.contains("unknown provider type 'local-e4b'"),
                    "msg: {msg}"
                );
                assert!(
                    msg.contains("supported: anthropic, deepseek, openai, ollama, lemonade, local"),
                    "msg: {msg}"
                );
            }
            other => panic!("expected Err, got {other:?}"),
        }

        // The rejected write must not have landed — `show` still sees the
        // seeded default, not the bad content.
        let show = d
            .dispatch(&[s("config"), s("show"), s("models.toml"), s("--json")], &c)
            .await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_ne!(
                    v["content"].as_str(),
                    Some(toml),
                    "rejected content must not have been written"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// The flip side: a real supported type still writes fine — validation
    /// isn't accidentally rejecting everything.
    #[tokio::test]
    async fn set_models_toml_accepts_known_provider_type() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let toml = "[providers.ollama]\nenabled = true\nbase_url = \"http://localhost:11434\"\n";
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("models.toml"),
                    s("--content"),
                    s(toml),
                ],
                &c,
            )
            .await;
        assert!(
            matches!(result, KjResult::Ok { .. }),
            "set failed: {result:?}"
        );
    }

    /// Validation is narrow-scoped to `models.toml` — other config files
    /// aren't TOML at all (system.md) or just don't get the providers-table
    /// check, so `set` must not choke on content that wouldn't parse as this
    /// validator's shape.
    #[tokio::test]
    async fn set_non_models_toml_skips_provider_validation() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("set"),
                    s("system.md"),
                    s("--content"),
                    s("# not a providers table, not even TOML {{{"),
                ],
                &c,
            )
            .await;
        assert!(
            matches!(result, KjResult::Ok { .. }),
            "set failed: {result:?}"
        );
    }

    // ── Stretch: `kj config edit` mirrors `kj rc edit` — optional content
    // ── replaces (validated like `set`); no content opens an interactive vi
    // ── session on the owning CRDT block.

    /// `kj config edit <path>` with no `--content` opens an interactive editor
    /// session on the owning block (mirrors
    /// `rc_edit_without_content_opens_an_editor_session` in `kj/rc.rs`).
    #[tokio::test]
    async fn config_edit_without_content_opens_an_editor_session() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("config"), s("edit"), s("theme.toml")], &c)
            .await;
        match result {
            KjResult::Ok {
                message,
                data: Some(v),
                ..
            } => {
                assert!(message.contains("opened editor session"), "msg: {message}");
                assert!(
                    v["session"].as_u64().is_some(),
                    "data carries a numeric session id: {v}"
                );
            }
            other => panic!("expected ok-with-data session, got {other:?}"),
        }
    }

    /// `kj config edit <path> --content <body>` behaves exactly like `set`,
    /// including validation — `models.toml` with an unknown provider type is
    /// still rejected, not just when using `set`.
    #[tokio::test]
    async fn config_edit_with_content_validates_like_set() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let toml = "[providers.local-e4b]\nenabled = true\n";
        let result = d
            .dispatch(
                &[
                    s("config"),
                    s("edit"),
                    s("models.toml"),
                    s("--content"),
                    s(toml),
                ],
                &c,
            )
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(
                    msg.contains("unknown provider type 'local-e4b'"),
                    "msg: {msg}"
                )
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `kj config edit` is gated by `config-write` exactly like `set` — it's
    /// still a write surface, even in its interactive-open branch.
    #[tokio::test]
    async fn config_edit_denied_without_config_write() {
        let d = test_dispatcher_crdt_rc().await;
        let c = caller_with_context(kaijutsu_crdt::ContextId::new());
        let result = d
            .dispatch(&[s("config"), s("edit"), s("theme.toml")], &c)
            .await;
        match result {
            KjResult::Err(msg) => assert!(
                msg.contains("config-write"),
                "denial should name the missing cap: {msg}"
            ),
            other => panic!("expected denial, got {other:?}"),
        }
    }
}
