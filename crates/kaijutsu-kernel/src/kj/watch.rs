//! `kj watch` — what this context watches (`docs/watch.md`).
//!
//! ```text
//! kj watch add <path>
//! kj watch list
//! kj watch show <id>
//! kj watch rm <id>
//! ```
//!
//! A watch never starts a turn. Changes accrue while the context is idle and
//! go with the next prompt as one notice block placed before it.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::effect::{Classify, Effect};
use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::kernel_db::WatchRow;

#[derive(Parser, Debug)]
#[command(
    name = "watch",
    about = "Watch files; changes go with the next prompt as a notice",
    long_about = "Watch files for this context. A watch never starts a turn: changes \
                  accrue while the context is idle, and the next prompt carries one notice \
                  block before it naming each changed path, how many times it changed, and \
                  whether it still exists. The notice holds no file content. Rotation gives \
                  the successor fresh copies of the watches; a fork copies none.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct WatchArgs {
    #[command(subcommand)]
    command: WatchCommand,
}

#[derive(Subcommand, Debug)]
enum WatchCommand {
    /// Watch a file.
    ///
    /// The path is absolute or relative to this context's working
    /// directory, and must name an existing file on a host directory mount;
    /// a path with no host file behind it is refused. Adding a stopped
    /// watch's path starts it again with no changes.
    Add {
        /// The file to watch.
        path: String,
    },
    /// List this context's watches and the changes each holds.
    ///
    /// A watch stopped by `kj context promote` says so.
    List,
    /// Show one watch.
    Show {
        /// The watch id from `kj watch list`.
        id: i64,
    },
    /// Remove a watch and the changes it holds.
    Rm {
        /// The watch id from `kj watch list`.
        id: i64,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_watch(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<WatchArgs>();
        }
        let parsed = match WatchArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj watch: {e}"));
            }
        };
        let context = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };
        match parsed.command {
            WatchCommand::Add { path } => self.watch_add(context, caller, &path).await,
            WatchCommand::List => {
                let rows = match self.kernel_db().lock().list_watches(context) {
                    Ok(rows) => rows,
                    Err(e) => return KjResult::Err(format!("kj watch list: {e}")),
                };
                if rows.is_empty() {
                    return KjResult::ok_with_data("no watches".to_string(), serde_json::json!([]));
                }
                let lines: Vec<String> = rows.iter().map(line).collect();
                KjResult::ok_with_data(lines.join("\n"), serde_json::Value::Array(rows.iter().map(data).collect()))
            }
            WatchCommand::Show { id } => match self.kernel_db().lock().get_watch(context, id) {
                Ok(Some(row)) => KjResult::ok_with_data(
                    format!("{}\n  host file: {}\n  last delivered: {}", line(&row), row.host_path,
                        row.delivered_at.map_or("never".to_string(), super::format::format_timestamp)),
                    data(&row),
                ),
                Ok(None) => KjResult::Err(format!("kj watch show: this context has no watch {id}")),
                Err(e) => KjResult::Err(format!("kj watch show: {e}")),
            },
            WatchCommand::Rm { id } => {
                // Bound first: the lock must be released before the resync
                // below takes it again.
                let removed = self.kernel_db().lock().remove_watch(context, id);
                match removed {
                Ok(true) => {
                    // A failed resync only leaves a directory watched; the
                    // removed row records nothing either way.
                    if let Err(e) = self.kernel().path_watcher().sync(self.kernel().kernel_db()) {
                        tracing::warn!(error = %e, "kj watch rm: the path watcher did not resync");
                    }
                    KjResult::ok_with_data(format!("removed watch {id}"), serde_json::json!({"watch_id": id}))
                }
                Ok(false) => KjResult::Err(format!("kj watch rm: this context has no watch {id}")),
                Err(e) => KjResult::Err(format!("kj watch rm: {e}")),
                }
            }
        }
    }

    async fn watch_add(&self, context: kaijutsu_types::ContextId, caller: &KjCaller, path: &str) -> KjResult {
        use crate::vfs::VfsOps;
        let target = if path.starts_with('/') {
            crate::seed_scripts::normalize_abs(path)
        } else {
            match crate::runtime::shell_state::context_cwd(self.kernel(), context) {
                Ok(Some(cwd)) => crate::seed_scripts::normalize_abs(&format!("{}/{path}", cwd.display())),
                Ok(None) => return KjResult::Err(format!(
                    "kj watch add: {path} is relative and this context has no working directory; give an absolute path")),
                Err(e) => return KjResult::Err(format!("kj watch add: {e}")),
            }
        };
        let host = match self.kernel().vfs().real_path(std::path::Path::new(&target)).await {
            Ok(Some(host)) => host,
            Ok(None) => return KjResult::Err(format!(
                "kj watch add: {target} has no host file behind it; only files on a host directory mount can be watched")),
            Err(e) => return KjResult::Err(format!("kj watch add: {target}: {e}")),
        };
        if !host.is_file() {
            return KjResult::Err(format!("kj watch add: {target} is not a file"));
        }
        let host = host.to_string_lossy().into_owned();
        let row = match self.kernel_db().lock().add_path_watch(context, &target, &host, caller.actor_id) {
            Ok(row) => row,
            Err(e) => return KjResult::Err(format!("kj watch add: {e}")),
        };
        if let Err(e) = self.kernel().path_watcher().sync(self.kernel().kernel_db()) {
            return KjResult::Err(format!("kj watch add: watch {} is stored, but the path watcher failed: {e}", row.watch_id));
        }
        KjResult::ok_with_data(format!("watching {target} as watch {}", row.watch_id), data(&row))
    }
}

/// `3  /work/repo/AGENTS.md  2 changes, last 4m ago` or `... no changes`.
fn line(row: &WatchRow) -> String {
    if row.stopped_at.is_some() {
        return format!("{}  {}  stopped; `kj watch add {}` starts it again", row.watch_id, row.target, row.target);
    }
    let status = match (row.changes, row.last_change_at) {
        (0, _) | (_, None) => "no changes".to_string(),
        (n, Some(at)) => format!("{n} change{}, last {}{}", if n == 1 { "" } else { "s" },
            super::format::format_timestamp(at), if row.present == Some(false) { ", removed" } else { "" }),
    };
    format!("{}  {}  {status}", row.watch_id, row.target)
}

fn data(row: &WatchRow) -> serde_json::Value {
    serde_json::json!({
        "watch_id": row.watch_id, "kind": row.kind, "target": row.target, "host_path": row.host_path,
        "changes": row.changes, "first_change_at": row.first_change_at, "last_change_at": row.last_change_at,
        "present": row.present, "delivered_at": row.delivered_at, "created_at": row.created_at,
        "stopped_at": row.stopped_at,
    })
}

impl Classify for WatchArgs {
    fn effect(&self) -> Effect {
        match self.command {
            WatchCommand::Add { .. } | WatchCommand::Rm { .. } => Effect::Write,
            WatchCommand::List | WatchCommand::Show { .. } => Effect::Read,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| s(w)).collect()
    }

    /// Add, list, show, and remove round-trip through the verb, and a
    /// relative path resolves against the context's working directory.
    #[tokio::test]
    async fn add_list_show_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "rules").unwrap();
        let d = test_dispatcher().await;
        d.kernel().mount("/mnt/w", crate::vfs::LocalBackend::new(dir.path())).await;
        let ctx = register_context(&d, Some("w"), None, PrincipalId::new());
        let c = caller_with_context(ctx);

        let added = d.dispatch(&argv(&["watch", "add", "/mnt/w/./AGENTS.md"]), &c).await;
        assert!(added.is_ok(), "{added:?}");
        let rows = d.kernel_db().lock().list_watches(ctx).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, "/mnt/w/AGENTS.md");
        assert_eq!(std::path::Path::new(&rows[0].host_path), dunce::canonicalize(dir.path().join("AGENTS.md")).unwrap());
        let id = rows[0].watch_id.to_string();

        let listed = d.dispatch(&argv(&["watch", "list"]), &c).await;
        assert!(listed.message().contains("/mnt/w/AGENTS.md  no changes"), "{listed:?}");
        let shown = d.dispatch(&argv(&["watch", "show", &id]), &c).await;
        assert!(shown.message().contains("last delivered: never"), "{shown:?}");
        let removed = d.dispatch(&argv(&["watch", "rm", &id]), &c).await;
        assert!(removed.is_ok(), "{removed:?}");
        assert!(d.kernel_db().lock().list_watches(ctx).unwrap().is_empty());
        let again = d.dispatch(&argv(&["watch", "rm", &id]), &c).await;
        assert!(again.message().contains("has no watch"), "{again:?}");
    }

    /// A path with no host file behind it is refused, and nothing is stored.
    #[tokio::test]
    async fn a_path_without_a_host_file_is_refused() {
        let d = test_dispatcher().await;
        d.kernel().mount("/tmp", crate::vfs::backends::MemoryBackend::new()).await;
        crate::vfs::VfsOps::write_all(d.kernel().vfs().as_ref(), std::path::Path::new("/tmp/a"), b"x").await.unwrap();
        let ctx = register_context(&d, Some("w"), None, PrincipalId::new());
        let c = caller_with_context(ctx);
        let refused = d.dispatch(&argv(&["watch", "add", "/tmp/a"]), &c).await;
        assert!(refused.message().contains("no host file behind it"), "{refused:?}");
        assert!(d.kernel_db().lock().list_watches(ctx).unwrap().is_empty());
    }
}
