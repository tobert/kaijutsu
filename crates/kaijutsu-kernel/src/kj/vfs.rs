//! `kj vfs` — VFS landscape queries (the FSN world's stage-0/1 kernel
//! plumbing, `docs/scenes/vfs.md`).
//!
//! Pure read discovery, no capability gate (same rationale as `kj cas`
//! get/ls/info: reading structure isn't escalation) and no active-context
//! requirement — a snapshot is addressed by VFS path, not by context.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::vfs::{FileType, SnapshotNode};

#[derive(Parser, Debug)]
#[command(
    name = "vfs",
    about = "VFS landscape queries (FSN world plumbing)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct VfsArgs {
    #[command(subcommand)]
    command: VfsCommand,
}

#[derive(Subcommand, Debug)]
enum VfsCommand {
    /// Recursive snapshot listing with generation stamps (stage 0/1 kernel
    /// plumbing). `depth`/`max-entries` are server-clamped regardless of
    /// what's asked.
    Snapshot {
        /// VFS path to snapshot (e.g. "/mnt/project")
        path: String,
        /// Recursion depth (0 = just this node, no children walked)
        #[arg(long, default_value_t = 3)]
        depth: u32,
        /// Max total nodes in the reply (root included)
        #[arg(long = "max-entries", default_value_t = 500)]
        max_entries: u32,
    },
    /// Per-directory activity totals since kernel boot (Lane K digest
    /// groundwork, `docs/scenes/vfs.md`) — heat, not structure: content
    /// mutations (write/truncate/setattr) count alongside every structural
    /// one. Debug/inspection surface for the push digest stream
    /// (`subscribeVfsActivity`); this command reads the same in-memory
    /// counters directly, no subscription involved.
    Activity {
        /// Restrict to directories at-or-under this VFS path prefix
        /// (default: every directory ever touched since boot)
        path: Option<String>,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_vfs(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<VfsArgs>();
        }
        let parsed = match VfsArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj vfs: {e}"));
            }
        };

        match parsed.command {
            VfsCommand::Snapshot {
                path,
                depth,
                max_entries,
            } => self.vfs_snapshot(&path, depth, max_entries).await,
            VfsCommand::Activity { path } => self.vfs_activity(path.as_deref()).await,
        }
    }

    async fn vfs_snapshot(&self, path: &str, depth: u32, max_entries: u32) -> KjResult {
        match self
            .kernel()
            .snapshot(std::path::Path::new(path), depth, max_entries)
            .await
        {
            Ok(result) => {
                let mut lines = Vec::new();
                render_snapshot_node(&result.root, 0, &mut lines);
                let mut text = lines.join("\n");
                if result.truncated {
                    text.push_str(
                        "\n(truncated — widen --depth/--max-entries or narrow the path)",
                    );
                }
                let data = serde_json::json!({
                    "generation": result.generation,
                    "truncated": result.truncated,
                    "root": result.root,
                });
                KjResult::ok_with_data(text, data)
            }
            Err(e) => KjResult::Err(format!("kj vfs snapshot: {e}")),
        }
    }

    /// `kj vfs activity` — direct read of the in-memory activity counters
    /// (`MountTable::activity_snapshot`/`global_activity`), the same state
    /// the `subscribeVfsActivity` push stream digests. No capability gate,
    /// no active-context requirement: same rationale as `vfs_snapshot`
    /// above — this is structure/heat discovery, not escalation.
    async fn vfs_activity(&self, path: Option<&str>) -> KjResult {
        let vfs = self.kernel().vfs();
        let prefix = path.map(std::path::Path::new);
        let entries = vfs.activity_snapshot(prefix);
        let global_total = vfs.global_activity();

        if entries.is_empty() {
            return KjResult::ok_ephemeral(
                "no activity since boot".to_string(),
                ContentType::Plain,
            );
        }

        let mut lines: Vec<String> = entries
            .iter()
            .map(|(p, total)| format!("{total}\t{}", p.display()))
            .collect();
        lines.push(format!("global: {global_total}"));
        let text = lines.join("\n");

        let data_entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|(p, total)| {
                serde_json::json!({
                    "path": p.display().to_string(),
                    "total": total,
                    "generation": vfs.generation_of(p),
                })
            })
            .collect();
        let data = serde_json::json!({
            "global_total": global_total,
            "entries": data_entries,
        });
        KjResult::ok_with_data(text, data)
    }
}

/// Render a `SnapshotNode` tree as an indented human-readable listing —
/// `kj vfs snapshot`'s text view (the structured `.data` carries the full
/// tree for programmatic consumers).
fn render_snapshot_node(node: &SnapshotNode, indent: usize, out: &mut Vec<String>) {
    let marker = match node.kind {
        FileType::Directory => "/",
        FileType::Symlink => "@",
        FileType::File => "",
    };
    let mut suffix = String::new();
    if node.kind.is_dir() {
        suffix.push_str(&format!(" (children={}, gen={})", node.child_count, node.generation));
    }
    if node.truncated_here {
        suffix.push_str(" [cut]");
    }
    if node.denied {
        suffix.push_str(" [denied]");
    }
    if node.ignored {
        suffix.push_str(" [ignored]");
    }
    out.push(format!(
        "{}{}{}{}",
        "  ".repeat(indent),
        node.name,
        marker,
        suffix
    ));
    for child in &node.children {
        render_snapshot_node(child, indent + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use crate::vfs::VfsOps;
    use std::sync::Arc;

    /// End-to-end through the dispatcher: `kj vfs snapshot` against a real
    /// mounted tree returns a human view plus a structured `.data` tree.
    #[tokio::test]
    async fn vfs_snapshot_walks_a_real_mount() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("a.txt"), "hi").expect("write a.txt");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir sub");
        std::fs::write(dir.path().join("sub/b.txt"), "hi").expect("write b.txt");
        dispatcher
            .kernel()
            .mount("/mnt/snap", crate::vfs::LocalBackend::new(dir.path()))
            .await;

        let result = dispatcher
            .dispatch_vfs(
                &[
                    "snapshot".to_string(),
                    "/mnt/snap".to_string(),
                    "--depth".to_string(),
                    "5".to_string(),
                ],
                &caller,
            )
            .await;
        assert!(result.is_ok(), "kj vfs snapshot failed: {result:?}");
        assert!(result.message().contains("a.txt"), "got: {}", result.message());
        assert!(result.message().contains("sub"), "got: {}", result.message());

        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        let data = data.expect("snapshot must attach structured data");
        assert_eq!(data["root"]["name"], "snap");
        assert_eq!(data["truncated"], false);
    }

    #[tokio::test]
    async fn vfs_snapshot_missing_path_errors() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let result = dispatcher
            .dispatch_vfs(
                &["snapshot".to_string(), "/nope/nowhere".to_string()],
                &caller,
            )
            .await;
        assert!(!result.is_ok(), "snapshot of a missing path should error");
    }

    #[tokio::test]
    async fn vfs_bare_help_does_not_require_context() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = crate::kj::test_helpers::test_caller();
        // Empty argv routes to help, not an error.
        let result = dispatcher.dispatch_vfs(&[], &caller).await;
        assert!(result.is_ok(), "kj vfs (bare) should render help: {result:?}");
    }

    // ========================================================================
    // `kj vfs activity` (Lane K, FSN slice-1 digest groundwork)
    // ========================================================================

    #[tokio::test]
    async fn activity_bare_on_a_fresh_kernel_is_ok_and_empty() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let result = dispatcher
            .dispatch_vfs(&["activity".to_string()], &caller)
            .await;
        assert!(result.is_ok(), "kj vfs activity on a fresh kernel should be Ok: {result:?}");
        assert!(
            result.message().contains("no activity"),
            "expected a no-activity message, got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn activity_reports_touched_dirs_with_the_right_data_shape() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let dir = tempfile::tempdir().expect("tmpdir");
        dispatcher
            .kernel()
            .mount("/mnt/heat", crate::vfs::LocalBackend::new(dir.path()))
            .await;
        dispatcher
            .kernel()
            .vfs()
            .create(std::path::Path::new("/mnt/heat/a.txt"), 0o644)
            .await
            .expect("create a.txt");
        dispatcher
            .kernel()
            .vfs()
            .write(std::path::Path::new("/mnt/heat/a.txt"), 0, b"hi")
            .await
            .expect("write a.txt");

        let result = dispatcher
            .dispatch_vfs(&["activity".to_string()], &caller)
            .await;
        assert!(result.is_ok(), "kj vfs activity failed: {result:?}");
        assert!(
            result.message().contains("/mnt/heat"),
            "expected /mnt/heat in the text view, got: {}",
            result.message()
        );
        assert!(
            result.message().contains("global:"),
            "expected a global total footer, got: {}",
            result.message()
        );

        let crate::kj::KjResult::Ok { data, .. } = result else {
            panic!("expected Ok result");
        };
        let data = data.expect("activity must attach structured data");
        assert!(data["global_total"].as_u64().unwrap() >= 2, "create + write = 2 bumps");
        let entries = data["entries"].as_array().expect("entries array");
        let heat = entries
            .iter()
            .find(|e| e["path"] == "/mnt/heat")
            .expect("/mnt/heat entry present");
        assert_eq!(heat["total"], 2);
        assert!(heat["generation"].as_u64().is_some());
    }

    #[tokio::test]
    async fn activity_prefix_filter_shows_only_the_matching_mount() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        dispatcher
            .kernel()
            .mount("/mnt/a", crate::vfs::MemoryBackend::new())
            .await;
        dispatcher
            .kernel()
            .mount("/mnt/b", crate::vfs::MemoryBackend::new())
            .await;
        dispatcher
            .kernel()
            .vfs()
            .create(std::path::Path::new("/mnt/a/x.txt"), 0o644)
            .await
            .expect("create in /mnt/a");
        dispatcher
            .kernel()
            .vfs()
            .create(std::path::Path::new("/mnt/b/y.txt"), 0o644)
            .await
            .expect("create in /mnt/b");

        let result = dispatcher
            .dispatch_vfs(
                &["activity".to_string(), "/mnt/a".to_string()],
                &caller,
            )
            .await;
        assert!(result.is_ok(), "kj vfs activity /mnt/a failed: {result:?}");
        assert!(result.message().contains("/mnt/a"), "got: {}", result.message());
        assert!(
            !result.message().contains("/mnt/b"),
            "prefix filter must exclude /mnt/b, got: {}",
            result.message()
        );
    }
}
