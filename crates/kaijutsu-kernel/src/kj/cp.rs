//! `kj cp` — copy a file between VFS paths via the streaming pump
//! (`docs/slash-r.md` slice 0). The first consumer of `vfs::pump`: a plain
//! `cp` between any two mounts (including a future cross-share `cp`), never
//! buffering the whole file in kernel memory.
//!
//! Directory copies are out of scope for this slice — `-r`/`--recursive` is
//! accepted (so the CLI shape matches `cp(1)`) but always errors, and a
//! directory source errors the same way even without the flag.

use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::vfs::{FileType, VfsOps, VfsSink, pump_stream};

#[derive(Parser, Debug)]
#[command(
    name = "cp",
    about = "Copy a file between VFS paths via the streaming pump",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct CpArgs {
    /// Source VFS path
    src: String,
    /// Destination VFS path — or an existing directory, cp(1)-style
    /// (copies to `<dst>/<basename of src>`)
    dst: String,
    /// Recursive copy — NOT implemented yet; directories are out of scope
    /// for this slice.
    #[arg(short = 'r', long = "recursive")]
    recursive: bool,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_cp(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<CpArgs>();
        }
        let parsed = match CpArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj cp: {e}"));
            }
        };

        // Writing a destination is kernel-authoritative and bypasses the
        // broker/facade gates like `kj cas put` — same Operator gate.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Operator, "cp") {
            return denied;
        }

        if parsed.recursive {
            return KjResult::Err("kj cp: -r not supported yet".to_string());
        }

        self.cp(&parsed.src, &parsed.dst).await
    }

    async fn cp(&self, src: &str, dst: &str) -> KjResult {
        let vfs = self.kernel().vfs();
        let src_path = Path::new(src);

        let src_attr = match vfs.getattr(src_path).await {
            Ok(a) => a,
            Err(e) => return KjResult::Err(format!("kj cp: {src}: {e}")),
        };
        if src_attr.kind == FileType::Directory {
            return KjResult::Err(format!(
                "kj cp: {src} is a directory (-r not supported yet)"
            ));
        }

        let dst_path = match Self::resolve_cp_dest(vfs, src_path, Path::new(dst)).await {
            Ok(p) => p,
            Err(msg) => return KjResult::Err(msg),
        };

        let source: Arc<dyn VfsOps> = vfs.clone();
        let sink = match VfsSink::create(source.clone(), dst_path.clone()).await {
            Ok(s) => s,
            Err(e) => {
                return KjResult::Err(format!(
                    "kj cp: creating destination {}: {e}",
                    dst_path.display()
                ));
            }
        };

        match pump_stream(&source, src_path, sink).await {
            Ok(outcome) => KjResult::ok(format!(
                "copied {} bytes: {} -> {}",
                outcome.bytes_transferred,
                src_path.display(),
                dst_path.display()
            )),
            Err(e) => KjResult::Err(format!(
                "kj cp: {src} -> {}: {e}",
                dst_path.display()
            )),
        }
    }

    /// cp(1) semantics: an existing directory destination means
    /// `<dst>/<basename of src>`, not "replace the directory".
    async fn resolve_cp_dest(
        vfs: &Arc<crate::vfs::MountTable>,
        src: &Path,
        dst: &Path,
    ) -> Result<PathBuf, String> {
        match vfs.getattr(dst).await {
            Ok(attr) if attr.kind == FileType::Directory => {
                let basename = src.file_name().ok_or_else(|| {
                    format!("kj cp: source path '{}' has no file name", src.display())
                })?;
                Ok(dst.join(basename))
            }
            _ => Ok(dst.to_path_buf()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use crate::vfs::{MemoryBackend, VfsOps};
    use std::path::Path;
    use std::sync::Arc;

    #[tokio::test]
    async fn cp_happy_path_across_two_mounts() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        dispatcher.kernel().mount("/a", MemoryBackend::new()).await;
        dispatcher.kernel().mount("/b", MemoryBackend::new()).await;
        dispatcher
            .kernel()
            .vfs()
            .create(Path::new("/a/src.txt"), 0o644)
            .await
            .unwrap();
        dispatcher
            .kernel()
            .vfs()
            .write(Path::new("/a/src.txt"), 0, b"hello from a")
            .await
            .unwrap();

        let result = dispatcher
            .dispatch(
                &[
                    "cp".to_string(),
                    "/a/src.txt".to_string(),
                    "/b/dst.txt".to_string(),
                ],
                &caller,
            )
            .await;
        assert!(result.is_ok(), "kj cp failed: {result:?}");
        assert!(result.message().contains("copied 12 bytes"), "got: {}", result.message());

        let content = dispatcher
            .kernel()
            .vfs()
            .read_all(Path::new("/b/dst.txt"))
            .await
            .unwrap();
        assert_eq!(content, b"hello from a");
    }

    #[tokio::test]
    async fn cp_into_existing_directory_uses_basename() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        dispatcher.kernel().mount("/a", MemoryBackend::new()).await;
        dispatcher.kernel().mount("/b", MemoryBackend::new()).await;
        dispatcher
            .kernel()
            .vfs()
            .create(Path::new("/a/report.txt"), 0o644)
            .await
            .unwrap();
        dispatcher
            .kernel()
            .vfs()
            .write(Path::new("/a/report.txt"), 0, b"contents")
            .await
            .unwrap();
        dispatcher.kernel().vfs().mkdir(Path::new("/b/dir"), 0o755).await.unwrap();

        let result = dispatcher
            .dispatch(
                &[
                    "cp".to_string(),
                    "/a/report.txt".to_string(),
                    "/b/dir".to_string(),
                ],
                &caller,
            )
            .await;
        assert!(result.is_ok(), "kj cp into a directory failed: {result:?}");

        let content = dispatcher
            .kernel()
            .vfs()
            .read_all(Path::new("/b/dir/report.txt"))
            .await
            .unwrap();
        assert_eq!(content, b"contents");
    }

    #[tokio::test]
    async fn cp_recursive_flag_errors_politely() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        dispatcher.kernel().mount("/a", MemoryBackend::new()).await;
        dispatcher.kernel().vfs().mkdir(Path::new("/a/dir"), 0o755).await.unwrap();

        let result = dispatcher
            .dispatch(
                &[
                    "cp".to_string(),
                    "-r".to_string(),
                    "/a/dir".to_string(),
                    "/a/dir2".to_string(),
                ],
                &caller,
            )
            .await;
        assert!(!result.is_ok(), "kj cp -r should be rejected");
        assert!(
            result.message().contains("not supported yet"),
            "got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn cp_of_a_directory_without_flag_errors_politely() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        dispatcher.kernel().mount("/a", MemoryBackend::new()).await;
        dispatcher.kernel().vfs().mkdir(Path::new("/a/dir"), 0o755).await.unwrap();

        let result = dispatcher
            .dispatch(
                &["cp".to_string(), "/a/dir".to_string(), "/a/dir2".to_string()],
                &caller,
            )
            .await;
        assert!(!result.is_ok(), "copying a directory without -r must error");
        assert!(
            result.message().contains("is a directory"),
            "got: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn cp_bare_help_does_not_require_context() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let caller = test_caller();

        let result = dispatcher.dispatch(&["cp".to_string()], &caller).await;
        assert!(result.is_ok(), "kj cp (bare) should render help: {result:?}");
    }
}
