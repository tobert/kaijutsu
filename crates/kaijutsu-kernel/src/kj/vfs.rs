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
}
