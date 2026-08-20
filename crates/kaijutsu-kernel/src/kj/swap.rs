//! `kj swap` — resolve a recovered file buffer (docs/file-buffers.md rule 4).
//!
//! A dirty file buffer survives a kernel restart as a durable
//! `dirty_file_buffers` row. The next thing that reads the path recovers it
//! into the cache flagged `swap_recovered`, and every flush on it refuses
//! (`FlushError::UnacknowledgedSwap`) until acknowledged — a recovered swap
//! is announced, never silently served as authoritative. `kj swap` is the
//! surface that resolves the announcement, the same two ways `/v/swap`'s own
//! refusal message names: acknowledge and flush (keep the buffer), or
//! discard (disk wins). Read the buffer first at
//! `/v/swap/<kernel_id>/<path>` — a read-only mount, never the write side.

use clap::{Parser, Subcommand};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::mcp::{Capability, InstanceId};

#[derive(Parser, Debug)]
#[command(
    name = "swap",
    about = "Resolve a file buffer recovered after a kernel restart (list/ack/discard)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct SwapArgs {
    #[command(subcommand)]
    command: SwapCommand,
}

#[derive(Subcommand, Debug)]
enum SwapCommand {
    /// List every unflushed file buffer the kernel is holding, and whether
    /// each one is a recovered swap (unacknowledged — flush refuses on it).
    /// Source: the durable swap-marker table, not just what happens to be
    /// loaded right now.
    List,
    /// Keep the unsaved buffer for `path` and write it to disk. Refuses when
    /// `path` has no recovered swap. Read the buffer first at
    /// `/v/swap/<kernel_id>/<path>` to see what you'd be keeping.
    Ack {
        /// Path whose recovered swap to acknowledge and flush to disk.
        path: String,
    },
    /// Drop the unsaved buffer for `path`; disk content wins on the next
    /// read. Refuses when `path` has no swap. Read the buffer first at
    /// `/v/swap/<kernel_id>/<path>` to see what you'd be discarding.
    Discard {
        /// Path whose swap to discard.
        path: String,
    },
}

/// The write capability `kj swap ack`/`discard` gate on — the same
/// `builtin.file` `edit` tool capability that governs every other write to a
/// file buffer through this cache (`mcp/servers/file.rs`'s hashline `edit`).
/// `kj editor save` flushes through this same `FileDocumentCache` but the kj
/// front door does not yet gate it (a separate, already-known gap); this verb
/// does not repeat that gap.
fn write_cap() -> Capability {
    Capability::Tool {
        instance: InstanceId::new("builtin.file"),
        tool: "edit".to_string(),
    }
}

impl KjDispatcher {
    pub(crate) async fn dispatch_swap(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<SwapArgs>();
        }
        let parsed = match SwapArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(
                        e.to_string(),
                        kaijutsu_types::ContentType::Plain,
                    );
                }
                return KjResult::Err(format!("kj swap: {e}"));
            }
        };

        match parsed.command {
            SwapCommand::List => self.swap_list().await,
            SwapCommand::Ack { path } => {
                if let Err(denied) = self.require_cap(caller, write_cap(), "swap ack") {
                    return denied;
                }
                self.swap_ack(&path).await
            }
            SwapCommand::Discard { path } => {
                if let Err(denied) = self.require_cap(caller, write_cap(), "swap discard") {
                    return denied;
                }
                self.swap_discard(&path)
            }
        }
    }

    async fn swap_list(&self) -> KjResult {
        let rows = match self.kernel_db().lock().list_dirty_file_buffers() {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj swap list: {e}")),
        };

        let cache = self.kernel().file_cache();
        let mut entries = Vec::with_capacity(rows.len());
        let mut lines = Vec::with_capacity(rows.len());
        for row in &rows {
            // Force the same recovery a read would perform, so `recovered`
            // reflects the buffer's real status rather than "nothing has
            // looked at this path since the restart yet."
            if let Err(e) = cache.try_get_or_load(&row.path).await {
                return KjResult::Err(format!("kj swap list: {}: {e}", row.path));
            }
            let recovered = cache.swap_recovered(&row.path);
            entries.push(serde_json::json!({
                "path": row.path,
                "dirtied_at": row.dirtied_at,
                "recovered": recovered,
            }));
            lines.push(format!(
                "{}{}",
                row.path,
                if recovered {
                    " [recovered, unacknowledged]"
                } else {
                    ""
                }
            ));
        }

        let line = if lines.is_empty() {
            "no unflushed file buffers".to_string()
        } else {
            lines.join("\n")
        };
        KjResult::ok_with_data(line, serde_json::Value::Array(entries))
    }

    async fn swap_ack(&self, path: &str) -> KjResult {
        let cache = self.kernel().file_cache();
        // Ensure the entry is loaded — recovers it from the durable row if
        // this is the first thing to look at `path` since a restart.
        if let Err(e) = cache.try_get_or_load(path).await {
            return KjResult::Err(format!("kj swap ack: {path}: {e}"));
        }
        if !cache.swap_recovered(path) {
            return KjResult::Err(format!("kj swap ack: no swap for {path}"));
        }
        cache.acknowledge_swap(path);
        match cache.flush_one(path).await {
            Ok(()) => KjResult::ok_with_data(
                format!("kept the unsaved buffer for {path} and wrote it to disk"),
                serde_json::json!({"path": path}),
            ),
            Err(e) => KjResult::Err(format!(
                "kj swap ack: {path}: acknowledged but the flush to disk failed — {e}"
            )),
        }
    }

    fn swap_discard(&self, path: &str) -> KjResult {
        match self.kernel().file_cache().discard_swap(path) {
            Ok(()) => KjResult::ok_with_data(
                format!("discarded the unsaved buffer for {path} — disk content wins on the next read"),
                serde_json::json!({"path": path}),
            ),
            Err(e) => KjResult::Err(format!("kj swap discard: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::{KjDispatcher, KjResult};
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::VfsOps;

    /// Every test here needs a plain-file VFS mount, which `test_dispatcher`
    /// doesn't provide by default (only `/etc/rc` is mounted).
    async fn test_dispatcher_with_tmp() -> KjDispatcher {
        let d = test_dispatcher().await;
        d.kernel().mount("/tmp", MemoryBackend::new()).await;
        d
    }

    async fn write_disk(d: &KjDispatcher, path: &str, content: &str) {
        d.kernel()
            .vfs()
            .write_all(std::path::Path::new(path), content.as_bytes())
            .await
            .unwrap();
    }

    /// Dirty a path through the shared file cache the way a real edit does,
    /// then simulate a kernel restart (`invalidate` drops only the in-memory
    /// entry — the durable `dirty_file_buffers` row survives, same as
    /// `file_tools::cache`'s and `swap_filesystem`'s own tests).
    async fn dirty_then_go_cold(d: &KjDispatcher, path: &str, unsaved_content: &str) {
        let cache = d.kernel().file_cache();
        cache.create_or_replace(path, unsaved_content).await.unwrap();
        cache.mark_dirty(path).unwrap();
        cache.invalidate(path).unwrap();
    }

    fn data_array(r: &KjResult) -> Vec<serde_json::Value> {
        match r {
            KjResult::Ok { data: Some(d), .. } => {
                d.as_array().expect("array data").clone()
            }
            other => panic!("expected ok-with-data array, got {other:?}"),
        }
    }

    fn data_obj(r: &KjResult) -> serde_json::Value {
        match r {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("expected ok-with-data object, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_shows_a_recovered_swap() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/a.txt", "disk-v1").await;
        dirty_then_go_cold(&d, "/tmp/a.txt", "unsaved-a").await;

        let listed = d
            .dispatch(&[s("swap"), s("list")], &c)
            .await;
        let arr = data_array(&listed);
        assert_eq!(arr.len(), 1, "one recovered swap");
        assert_eq!(arr[0]["path"], "/tmp/a.txt");
        assert_eq!(arr[0]["recovered"], true);
        assert!(arr[0]["dirtied_at"].is_i64() || arr[0]["dirtied_at"].is_u64());
    }

    #[tokio::test]
    async fn list_is_empty_with_no_dirty_buffers() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        let listed = d.dispatch(&[s("swap"), s("list")], &c).await;
        assert_eq!(data_array(&listed).len(), 0);
    }

    #[tokio::test]
    async fn ack_keeps_the_buffer_flushes_it_and_clears_the_list() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/ack.txt", "disk-v1").await;
        dirty_then_go_cold(&d, "/tmp/ack.txt", "unsaved-ack").await;

        // Before ack: flush_one refuses (the underlying rule-4 guard).
        let err = d
            .kernel()
            .file_cache()
            .read_content("/tmp/ack.txt")
            .await
            .unwrap();
        assert_eq!(err, "unsaved-ack", "recovered content is served, not disk");
        let refused = d.kernel().file_cache().flush_one("/tmp/ack.txt").await;
        assert!(refused.is_err(), "flush must refuse before ack");

        let acked = d.dispatch(&[s("swap"), s("ack"), s("/tmp/ack.txt")], &c).await;
        assert!(acked.is_ok(), "kj swap ack must succeed: {acked:?}");
        assert_eq!(data_obj(&acked)["path"], "/tmp/ack.txt");

        // The kept content is now on disk.
        let disk_bytes = d
            .kernel()
            .vfs()
            .read_all(std::path::Path::new("/tmp/ack.txt"))
            .await
            .unwrap();
        assert_eq!(String::from_utf8(disk_bytes).unwrap(), "unsaved-ack");

        // The row is cleared: list no longer shows it.
        let listed = d.dispatch(&[s("swap"), s("list")], &c).await;
        assert_eq!(data_array(&listed).len(), 0, "ack + flush must clear the swap marker");
    }

    #[tokio::test]
    async fn discard_drops_the_buffer_and_disk_wins_on_next_read() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/discard.txt", "disk-v1").await;
        dirty_then_go_cold(&d, "/tmp/discard.txt", "unsaved-discard").await;

        let discarded = d
            .dispatch(&[s("swap"), s("discard"), s("/tmp/discard.txt")], &c)
            .await;
        assert!(discarded.is_ok(), "kj swap discard must succeed: {discarded:?}");
        assert_eq!(data_obj(&discarded)["path"], "/tmp/discard.txt");

        // Disk wins: the next read serves disk content, not the discarded edit.
        let content = d
            .kernel()
            .file_cache()
            .read_content("/tmp/discard.txt")
            .await
            .unwrap();
        assert_eq!(content, "disk-v1");

        // The row is gone: list no longer shows it.
        let listed = d.dispatch(&[s("swap"), s("list")], &c).await;
        assert_eq!(data_array(&listed).len(), 0, "discard must clear the swap marker");
    }

    #[tokio::test]
    async fn ack_on_a_path_with_no_swap_is_an_error() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/clean.txt", "disk-only").await;
        // Never dirtied — no dirty_file_buffers row, no recovered swap.
        let result = d
            .dispatch(&[s("swap"), s("ack"), s("/tmp/clean.txt")], &c)
            .await;
        assert!(matches!(result, KjResult::Err(_)), "ack on a clean path must refuse");
    }

    #[tokio::test]
    async fn discard_on_a_path_with_no_swap_is_an_error() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/clean2.txt", "disk-only").await;
        let result = d
            .dispatch(&[s("swap"), s("discard"), s("/tmp/clean2.txt")], &c)
            .await;
        assert!(matches!(result, KjResult::Err(_)), "discard on a clean path must refuse");
    }

    /// C regression (`docs/audits/2026-08-20-editor-fileio.md`, `docs/issues.md`
    /// "Tech-debt audits, 2026-08-20"): the pin `d45e0484` added only stopped
    /// eviction — `invalidate`/`invalidate_document` still dropped a pinned
    /// entry outright, so `kj swap discard` on a path with an open editor
    /// session pulled the buffer out from under it. Falsify by reverting
    /// `FileDocumentCache::invalidate`'s pin check.
    #[tokio::test]
    async fn discard_on_a_path_with_an_open_editor_session_refuses_and_the_session_survives() {
        let d = test_dispatcher_with_tmp().await;
        let c = test_caller();

        write_disk(&d, "/tmp/pinned.txt", "orig").await;
        let (id, _) = d.kernel().editor_open("/tmp/pinned.txt").await.unwrap();
        // Dirty the session's buffer — this is what gives it a
        // `dirty_file_buffers` row, the precondition `discard` checks for.
        d.kernel().editor_keys(id, "iX<Esc>").await.unwrap();

        let result = d
            .dispatch(&[s("swap"), s("discard"), s("/tmp/pinned.txt")], &c)
            .await;
        assert!(
            matches!(result, KjResult::Err(_)),
            "discard on a path with an open editor session must refuse, got: {result:?}"
        );

        // The session must be untouched: still open, still holding the edit.
        let state = d.kernel().editor_state(id).unwrap();
        assert_eq!(state.text, "Xorig", "the open session's buffer must survive the refused discard");

        // The swap marker must still be there too — discard did not partially apply.
        assert!(
            d.kernel()
                .kernel_db()
                .lock()
                .get_dirty_file_buffer("/tmp/pinned.txt")
                .unwrap()
                .is_some(),
            "a refused discard must not clear the swap marker"
        );
    }

    fn s(v: &str) -> String {
        v.to_string()
    }
}
