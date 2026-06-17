//! `kj config` — read and edit the CRDT-owned config files.
//!
//! Config files (`models.toml`, `system.md`, `theme.toml`, `mcp.toml`) live at
//! `/etc/config` on the same CRDT-native backend as `/etc/rc` (slice 2,
//! `docs/config-crdt-ownership.md`): the kernel is the sole owner — no host
//! file, no write-through. `show`/`list` read the live CRDT; `set` writes it;
//! `reset` restores one file to its embedded default.
//!
//! Writes go straight through the VFS to the CRDT backend (the admin-only
//! surface, bypassing the gated `builtin.file:write` tool), so the `config-write`
//! capability is enforced here — the only place it gates the `kj` surface.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

/// Mount root the config files live under.
const CONFIG_ROOT: &str = "/etc/config";

#[derive(Parser, Debug)]
#[command(
    name = "config",
    about = "CRDT-owned config files at /etc/config (models.toml, system.md, theme.toml, mcp.toml)",
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
    },
    /// Replace a config file's content (direct CRDT write).
    #[command(alias = "edit")]
    Set {
        /// Config file name (e.g. models.toml) or full /etc/config path
        path: String,
        /// Replacement body (stdin is piped here when omitted)
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
            ConfigCommand::Set { .. } | ConfigCommand::Reset { .. }
        ) && let Err(denied) =
            self.require_cap(caller, crate::mcp::Capability::ConfigWrite, "config")
        {
            return denied;
        }
        match parsed.command {
            ConfigCommand::List { json } => self.config_list(json).await,
            ConfigCommand::Show { path, json } => self.config_show(&path, json).await,
            ConfigCommand::Set { path, content } => self.config_set(&path, content.as_deref()).await,
            ConfigCommand::Reset { path } => self.config_reset(&path).await,
        }
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
        let entries = match self.kernel().vfs().readdir(std::path::Path::new(CONFIG_ROOT)).await {
            Ok(e) => e,
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Vec::new(),
            Err(e) => return KjResult::Err(format!("kj config list: {e}")),
        };
        let mut names: Vec<String> = entries
            .into_iter()
            .filter(|e| e.kind.is_file())
            .map(|e| e.name)
            .collect();
        names.sort();

        // Iteration handles: bare file names (the key for `kj config show/set`).
        let data = serde_json::Value::Array(
            names.iter().cloned().map(serde_json::Value::String).collect(),
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

    async fn config_show(&self, path: &str, json: bool) -> KjResult {
        let canonical = match config_canonical(path) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj config show: {e}")),
        };
        let content = match self.read_config_content(&canonical).await {
            Ok(Some(c)) => c,
            Ok(None) => return KjResult::Err(format!("kj config show: '{canonical}' not found")),
            Err(e) => return KjResult::Err(format!("kj config show: '{canonical}': {e}")),
        };

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
                    "kj config set: missing content\nusage: kj config set <path> --content <body>"
                        .to_string(),
                );
            }
        };
        if let Err(e) = self.write_config_content(&canonical, content).await {
            return KjResult::Err(format!("kj config set: {e}"));
        }
        KjResult::ok(format!("set config '{canonical}' ({} bytes)", content.len()))
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
        KjResult::ok(format!("reset config '{canonical}' to its embedded default"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::*;
    use crate::kj::KjResult;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn canonical_accepts_bare_and_full_rejects_nesting() {
        assert_eq!(config_canonical("models.toml").unwrap(), "/etc/config/models.toml");
        assert_eq!(
            config_canonical("/etc/config/system.md").unwrap(),
            "/etc/config/system.md"
        );
        assert!(config_canonical("sub/dir.toml").is_err());
        assert!(config_canonical("/etc/config/a/b.toml").is_err());
        assert!(config_canonical("").is_err());
    }

    /// `kj config show models.toml` round-trips the seeded default.
    #[tokio::test]
    async fn show_round_trips_seeded_models() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("config"), s("show"), s("models.toml")], &c).await;
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
                let names: Vec<&str> =
                    v.as_array().expect("array").iter().filter_map(|x| x.as_str()).collect();
                assert!(names.contains(&"models.toml"), "names: {names:?}");
                assert!(names.contains(&"theme.toml"), "names: {names:?}");
                assert!(names.contains(&"system.md"), "names: {names:?}");
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
                &[s("config"), s("set"), s("theme.toml"), s("--content"), s("bg = \"#000000\"")],
                &c,
            )
            .await;
        assert!(matches!(set, KjResult::Ok { .. }), "set failed: {set:?}");

        let show = d.dispatch(&[s("config"), s("show"), s("theme.toml"), s("--json")], &c).await;
        match show {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["content"].as_str(), Some("bg = \"#000000\""));
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
            &[s("config"), s("set"), s("models.toml"), s("--content"), s("# broken")],
            &c,
        )
        .await;
        let reset = d.dispatch(&[s("config"), s("reset"), s("models.toml")], &c).await;
        assert!(matches!(reset, KjResult::Ok { .. }), "reset failed: {reset:?}");

        let show = d.dispatch(&[s("config"), s("show"), s("models.toml"), s("--json")], &c).await;
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
                &[s("config"), s("set"), s("theme.toml"), s("--content"), s("bg = \"#fff\"")],
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
        let result = d.dispatch(&[s("config"), s("reset"), s("nonesuch.toml")], &c).await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("no built-in default"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
