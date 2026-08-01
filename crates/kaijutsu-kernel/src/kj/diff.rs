//! `kj diff` — unified diffs of the things the kernel already holds.
//!
//! Not a git front-end. The first-class sources are kaijutsu's own: the CRDT
//! document that owns a file's text, the bytes actually on the backing store,
//! and any point in a document's journalled history. Git arrives later as
//! *another source* feeding the same pipeline (`docs/diff.md`).
//!
//! ## Three sources, one descriptor
//!
//! The CLI is deliberately two positional arguments. Internally every
//! invocation resolves into a pair of [`DiffSource`]s *before* anything is
//! read, so a future git/CAS source is a new variant plus one match arm rather
//! than a new command with its own argument grammar:
//!
//! | Invocation                        | old side                 | new side               |
//! |-----------------------------------|--------------------------|------------------------|
//! | `kj diff <path>`                  | [`DiffSource::Disk`]     | [`DiffSource::Document`] |
//! | `kj diff <a> <b>`                 | [`DiffSource::Document`] | [`DiffSource::Document`] |
//! | `kj diff <path> --from N [--to M]`| [`DiffSource::DocumentAt`] | document (or `--to`) |
//!
//! ## Ownership-aware resolution
//!
//! [`DiffSource::Document`] resolves through
//! [`resolve_editor_target`](crate::editor::resolve_editor_target) — the same
//! function the vi editor binds with, for the same reason. The mount table
//! answers "what owns this path?": a config-owned path (`/etc/rc/*`,
//! `/etc/config/*`) binds straight to its `ConfigCrdtFs` block, everything else
//! goes through the file-doc cache. Running a config path through
//! `FileDocumentCache::get_or_load` would mint a *second* CRDT document
//! shadowing the original — the dual-ownership bug class
//! `docs/config-crdt-ownership.md` deleted by construction. Reusing the
//! editor's resolver rather than reimplementing it means the two surfaces
//! cannot drift on what owns a path.
//!
//! ## `FileChange` is an input
//!
//! The diff engine will not guess creation or deletion, and the kernel does not
//! have to: it knows whether each side exists. A side resolves to
//! `Option<String>` — `None` meaning "no such thing" — and the pair picks the
//! [`FileSpec`] constructor. An empty file that stays empty is a modification,
//! not a deletion, and only the caller can tell the difference.
//!
//! ## Why two different paths format as a rename
//!
//! `kj diff a b` on genuinely different files emits `rename from`/`rename to`.
//! That is the dialect's only way to say "the pre-image path is a and the
//! post-image path is b": a `Modified` section with differing paths re-parses
//! as [`FileChange::Renamed`] anyway (`parse.rs`), so emitting `Modified` would
//! be a model that does not survive its own roundtrip. Renames are
//! *represented, not detected* here — nothing claims a similarity relationship.

use clap::Parser;
use kaijutsu_diff::{DiffOptions, FileSpec};
use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::vfs::VfsOps;

#[derive(Parser, Debug)]
#[command(
    name = "diff",
    about = "Unified diff between two kernel-held versions of a file",
    long_about = "Unified diff between two kernel-held versions of a file.\n\
        \n\
        With one path, diffs the bytes on disk against the CRDT document that \
        owns that path's text — the recovery view: what have edits changed that \
        the backing store has not seen? With two paths, diffs both documents. \
        With --from, diffs a point in the document's journalled history \
        (the `.data` payload of any `kj diff` reports the available seq range).\n\
        \n\
        Output is a typed diff block: the conversation renders it, and models \
        read a diffstat plus hunk-bounded content on the next turn.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct DiffArgs {
    /// VFS path to diff (the pre-image side when two are given)
    path: String,
    /// Second VFS path — diffs the two documents against each other
    other: Option<String>,
    /// Diff from this oplog sequence of `path`'s document instead of from disk
    #[arg(long)]
    from: Option<i64>,
    /// Diff to this oplog sequence instead of to the live document
    #[arg(long)]
    to: Option<i64>,
    /// Unchanged context lines kept around each hunk
    #[arg(long, short = 'U', default_value_t = kaijutsu_diff::limits::DEFAULT_CONTEXT_LINES)]
    context: usize,
}

/// Where one side of a `kj diff` comes from.
///
/// Kept typed even though the CLI is positional: resolution, labelling, and
/// error messages all branch on it, and the git/CAS sources the plan defers
/// land here as variants without touching the argument parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiffSource {
    /// The bytes on the backing store behind a VFS path — what a cold reader
    /// sees, ignoring any CRDT edits that have not been flushed.
    Disk(String),
    /// The live CRDT document that owns a path's text (ownership-aware: config
    /// documents answer through the mount table, never the file-doc cache).
    Document(String),
    /// A path's owning document as of oplog sequence `seq`, reconstructed by
    /// replaying its journal.
    DocumentAt { path: String, seq: i64 },
}

impl DiffSource {
    /// The VFS path this source is about — the label it contributes to the
    /// diff headers. Provenance (disk vs document vs seq) deliberately does
    /// *not* ride in the label: block content must be canonical unified text,
    /// so provenance travels in the structured `.data` payload instead.
    fn path(&self) -> &str {
        match self {
            DiffSource::Disk(p) | DiffSource::Document(p) => p,
            DiffSource::DocumentAt { path, .. } => path,
        }
    }

    /// A human/JSON description of where this side came from.
    fn describe(&self) -> String {
        match self {
            DiffSource::Disk(p) => format!("disk:{p}"),
            DiffSource::Document(p) => format!("doc:{p}"),
            DiffSource::DocumentAt { path, seq } => format!("doc:{path}@{seq}"),
        }
    }
}

/// A resolved side: its label, and its content — `None` when the source does
/// not exist, which is what tells [`file_spec`] to call it an add or a delete.
struct Resolved {
    label: String,
    text: Option<String>,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_diff(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<DiffArgs>();
        }
        let parsed = match DiffArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj diff: {e}"));
            }
        };

        let (old, new) = match plan_sources(&parsed) {
            Ok(pair) => pair,
            Err(e) => return KjResult::Err(format!("kj diff: {e}")),
        };

        let old_side = match self.resolve_source(&old).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj diff: {e}")),
        };
        let new_side = match self.resolve_source(&new).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj diff: {e}")),
        };

        let spec = match file_spec(&old_side, &new_side) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj diff: {e}")),
        };

        let options = DiffOptions {
            context: parsed.context,
            ..DiffOptions::default()
        };
        let file = match kaijutsu_diff::diff_file(&spec, &options) {
            Ok(f) => f,
            Err(e) => return KjResult::Err(format!("kj diff: {e}")),
        };
        let model = kaijutsu_diff::DiffModel::new(vec![file]);
        let stat = model.stat();

        if stat.is_empty() {
            // No hunks: a bare file header would read as "renamed, contents
            // identical" in this dialect, and a typed Diff block holding one is
            // a lie about what happened. Say nothing changed, in plain text.
            return KjResult::ok(format!(
                "no differences between {} and {}",
                old.describe(),
                new.describe()
            ));
        }

        let text = kaijutsu_diff::format(&model);
        let data = serde_json::json!({
            "old": old.describe(),
            "new": new.describe(),
            "files_changed": stat.files_changed,
            "insertions": stat.insertions,
            "deletions": stat.deletions,
            "stat": stat.to_string(),
            "bytes": text.len(),
            "history": self.history_hint(&new).await,
        });
        KjResult::ok_typed_with_data(text, ContentType::Diff, data)
    }

    /// Resolve one side to its label + content.
    async fn resolve_source(&self, source: &DiffSource) -> Result<Resolved, String> {
        let label = source.path().trim_start_matches('/').to_string();
        let text = match source {
            DiffSource::Disk(path) => self.read_disk(path).await?,
            DiffSource::Document(path) => Some(self.read_document(path).await?),
            DiffSource::DocumentAt { path, seq } => Some(self.read_document_at(path, *seq).await?),
        };
        Ok(Resolved { label, text })
    }

    /// The backing store's bytes for `path`, or `None` when nothing is there.
    ///
    /// Deliberately a raw VFS read, bypassing the file-doc cache: this side
    /// exists to answer "what would a cold reader see?". Note that for a
    /// config-owned path there *is* no host file — the CRDT is the backend —
    /// so disk and document agree by construction and the diff is empty. That
    /// is the honest answer, not a missing feature.
    async fn read_disk(&self, path: &str) -> Result<Option<String>, String> {
        match self
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(path))
            .await
        {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| format!("{path} is not UTF-8 text; binary diffs are not supported")),
            Err(crate::vfs::VfsError::NotFound(_)) => Ok(None),
            Err(crate::vfs::VfsError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(e) => Err(format!("read {path}: {e}")),
        }
    }

    /// The text of the CRDT document that owns `path`.
    ///
    /// Ownership-aware by delegation: `resolve_editor_target` asks the mount
    /// table, so a config document answers as itself and never as a shadow copy
    /// minted by the file-doc cache.
    async fn read_document(&self, path: &str) -> Result<String, String> {
        let target = self.editor_target(path).await?;
        self.block_store()
            .get_block_snapshot(target.context_id, &target.block_id)
            .map_err(|e| format!("read {path}: {e}"))?
            .map(|s| s.content)
            .ok_or_else(|| format!("no block holds the text of {path}"))
    }

    /// The text of `path`'s owning document as of oplog sequence `seq`.
    async fn read_document_at(&self, path: &str, seq: i64) -> Result<String, String> {
        let target = self.editor_target(path).await?;
        let blocks = self.block_store();
        blocks
            .block_content_at_seq(target.context_id, &target.block_id, seq)
            .map_err(|e| match blocks.oplog_seq_range(target.context_id) {
                Ok((oldest, head)) => {
                    format!("{path} at seq {seq}: {e} (available: {oldest}..={head})")
                }
                Err(_) => format!("{path} at seq {seq}: {e}"),
            })
    }

    /// Shared ownership-aware resolution step. Both document sources go
    /// through it so neither can drift from the editor's notion of ownership.
    async fn editor_target(&self, path: &str) -> Result<crate::editor::EditorTarget, String> {
        let blocks = self.block_store();
        let file_cache = self.kernel().file_cache(blocks);
        crate::editor::resolve_editor_target(path, blocks, &file_cache, self.kernel().vfs()).await
    }

    /// The seq range a `--from`/`--to` may address for this side's document,
    /// reported in `.data` so the historical source is discoverable without a
    /// separate history command. `None` when the kernel keeps no journal.
    async fn history_hint(&self, source: &DiffSource) -> Option<serde_json::Value> {
        let target = self.editor_target(source.path()).await.ok()?;
        let (oldest, head) = self.block_store().oplog_seq_range(target.context_id).ok()?;
        Some(serde_json::json!({ "oldest_seq": oldest, "head_seq": head }))
    }
}

/// Turn parsed arguments into the pair of sources they mean.
///
/// Split out from dispatch so the CLI-to-descriptor mapping — the part with
/// the interesting combinations — is testable without a kernel.
pub(crate) fn plan_sources(args: &DiffArgs) -> Result<(DiffSource, DiffSource), String> {
    let path = args.path.clone();
    match (&args.other, args.from, args.to) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("--from/--to address one document's history; drop the second path".to_string())
        }
        (Some(other), None, None) => Ok((
            DiffSource::Document(path),
            DiffSource::Document(other.clone()),
        )),
        (None, None, None) => Ok((DiffSource::Disk(path.clone()), DiffSource::Document(path))),
        (None, from, to) => {
            let old = match from {
                Some(seq) => DiffSource::DocumentAt {
                    path: path.clone(),
                    seq,
                },
                // `--to` alone: from disk to a historical point. Odd but
                // well-defined, and refusing it would be arbitrary.
                None => DiffSource::Disk(path.clone()),
            };
            let new = match to {
                Some(seq) => DiffSource::DocumentAt { path, seq },
                None => DiffSource::Document(path),
            };
            Ok((old, new))
        }
    }
}

/// Pick the [`FileSpec`] constructor the two resolved sides call for.
///
/// This is where "FileChange is an input, not a guess" is honored: existence
/// comes from the resolver, never from emptiness of the content.
fn file_spec<'a>(old: &'a Resolved, new: &'a Resolved) -> Result<FileSpec<'a>, String> {
    match (&old.text, &new.text) {
        (None, None) => Err(format!("neither {} nor {} exists", old.label, new.label)),
        (None, Some(after)) => Ok(FileSpec::added(&new.label, after)),
        (Some(before), None) => Ok(FileSpec::deleted(&old.label, before)),
        (Some(before), Some(after)) if old.label == new.label => {
            Ok(FileSpec::modified(&old.label, before, after))
        }
        (Some(before), Some(after)) => Ok(FileSpec::renamed(&old.label, &new.label, before, after)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use std::sync::Arc;

    fn args(argv: &[&str]) -> DiffArgs {
        DiffArgs::try_parse_from(argv.iter().map(|s| s.to_string())).expect("parse")
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── The CLI → descriptor mapping ────────────────────────────────────────

    #[test]
    fn one_path_means_disk_against_document() {
        let (old, new) = plan_sources(&args(&["/mnt/p/a.txt"])).unwrap();
        assert_eq!(old, DiffSource::Disk("/mnt/p/a.txt".into()));
        assert_eq!(new, DiffSource::Document("/mnt/p/a.txt".into()));
    }

    #[test]
    fn two_paths_mean_two_documents() {
        let (old, new) = plan_sources(&args(&["/a", "/b"])).unwrap();
        assert_eq!(old, DiffSource::Document("/a".into()));
        assert_eq!(new, DiffSource::Document("/b".into()));
    }

    #[test]
    fn from_addresses_history_and_defaults_the_new_side_to_live() {
        let (old, new) = plan_sources(&args(&["/a", "--from", "3"])).unwrap();
        assert_eq!(
            old,
            DiffSource::DocumentAt {
                path: "/a".into(),
                seq: 3
            }
        );
        assert_eq!(new, DiffSource::Document("/a".into()));

        let (old, new) = plan_sources(&args(&["/a", "--from", "3", "--to", "7"])).unwrap();
        assert_eq!(
            old,
            DiffSource::DocumentAt {
                path: "/a".into(),
                seq: 3
            }
        );
        assert_eq!(
            new,
            DiffSource::DocumentAt {
                path: "/a".into(),
                seq: 7
            }
        );
    }

    #[test]
    fn history_flags_with_a_second_path_are_refused_loudly() {
        let err = plan_sources(&args(&["/a", "/b", "--from", "3"])).unwrap_err();
        assert!(err.contains("--from"), "error must name the flags: {err}");
    }

    // ── FileChange comes from existence, never from emptiness ───────────────

    fn side(label: &str, text: Option<&str>) -> Resolved {
        Resolved {
            label: label.to_string(),
            text: text.map(|t| t.to_string()),
        }
    }

    #[test]
    fn an_absent_pre_image_is_an_add_and_an_absent_post_image_is_a_delete() {
        let (absent, present) = (side("a", None), side("a", Some("x\n")));
        let spec = file_spec(&absent, &present).unwrap();
        assert_eq!(spec.change, kaijutsu_diff::FileChange::Added);
        let spec = file_spec(&present, &absent).unwrap();
        assert_eq!(spec.change, kaijutsu_diff::FileChange::Deleted);
    }

    #[test]
    fn an_empty_file_that_stays_empty_is_a_modification_not_a_deletion() {
        // The engine would have to guess; the kernel knows. This is the whole
        // reason FileChange is an input.
        let (before, after) = (side("a", Some("")), side("a", Some("")));
        let spec = file_spec(&before, &after).unwrap();
        assert_eq!(spec.change, kaijutsu_diff::FileChange::Modified);
    }

    #[test]
    fn two_different_paths_are_represented_as_a_rename_and_roundtrip() {
        let (a, b) = (side("a", Some("one\n")), side("b", Some("two\n")));
        let spec = file_spec(&a, &b).unwrap();
        assert_eq!(spec.change, kaijutsu_diff::FileChange::Renamed);
        // The roundtrip is the reason: a Modified section with differing paths
        // re-parses as Renamed, so only this shape survives format∘parse.
        let file = kaijutsu_diff::diff_file(&spec, &DiffOptions::default()).unwrap();
        let model = kaijutsu_diff::DiffModel::new(vec![file]);
        let text = kaijutsu_diff::format(&model);
        assert_eq!(kaijutsu_diff::parse(&text).unwrap(), model);
    }

    #[test]
    fn neither_side_existing_is_an_error_not_an_empty_diff() {
        let (absent, also_absent) = (side("a", None), side("a", None));
        let err = file_spec(&absent, &also_absent).unwrap_err();
        assert!(err.contains("neither"), "got: {err}");
    }

    // ── End-to-end through the dispatcher ───────────────────────────────────

    /// Mount a temp dir at `/mnt/diff` and return the dispatcher + dir.
    async fn dispatcher_with_mount() -> (Arc<KjDispatcher>, tempfile::TempDir) {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let dir = tempfile::tempdir().expect("tmpdir");
        dispatcher
            .kernel()
            .mount("/mnt/diff", crate::vfs::LocalBackend::new(dir.path()))
            .await;
        (dispatcher, dir)
    }

    #[tokio::test]
    async fn a_crdt_edit_shows_up_as_a_typed_diff_block_against_disk() {
        let (dispatcher, dir) = dispatcher_with_mount().await;
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");

        // Load the file into a CRDT document, then edit it there — the state
        // `kj diff <path>` exists to reveal: an edit disk has not seen.
        let blocks = dispatcher.block_store();
        let cache = dispatcher.kernel().file_cache(blocks);
        let (ctx, block) = cache.get_or_load("/mnt/diff/a.txt").await.expect("load");
        blocks
            .edit_text(ctx, &block, 4, "TWO\n", 4)
            .expect("edit the CRDT copy");
        cache.mark_dirty("/mnt/diff/a.txt");

        let result = dispatcher
            .dispatch_diff(&argv(&["/mnt/diff/a.txt"]), &test_caller())
            .await;
        assert!(result.is_ok(), "kj diff failed: {result:?}");

        let KjResult::Ok {
            message,
            content_type,
            data,
            ..
        } = result
        else {
            panic!("expected Ok");
        };
        assert_eq!(
            content_type,
            ContentType::Diff,
            "kj diff must emit a typed diff block"
        );
        assert!(message.contains("-two"), "got: {message}");
        assert!(message.contains("+TWO"), "got: {message}");
        // The text is canonical: it parses back into the same model.
        let model = kaijutsu_diff::parse(&message).expect("kj diff output must parse");
        assert_eq!(kaijutsu_diff::format(&model), message);

        let data = data.expect("kj diff attaches provenance");
        assert_eq!(data["old"], "disk:/mnt/diff/a.txt");
        assert_eq!(data["new"], "doc:/mnt/diff/a.txt");
        assert_eq!(data["stat"], "1 file, +1 −1");
    }

    #[tokio::test]
    async fn identical_sides_report_no_differences_instead_of_an_empty_patch() {
        let (dispatcher, dir) = dispatcher_with_mount().await;
        std::fs::write(dir.path().join("same.txt"), "unchanged\n").expect("write");

        let result = dispatcher
            .dispatch_diff(&argv(&["/mnt/diff/same.txt"]), &test_caller())
            .await;
        let KjResult::Ok {
            message,
            content_type,
            ..
        } = result
        else {
            panic!("expected Ok, got {result:?}");
        };
        assert_eq!(
            content_type,
            ContentType::Plain,
            "an empty diff must not be typed as one"
        );
        assert!(message.contains("no differences"), "got: {message}");
    }

    #[tokio::test]
    async fn two_paths_diff_both_documents() {
        let (dispatcher, dir) = dispatcher_with_mount().await;
        std::fs::write(dir.path().join("a.txt"), "alpha\n").expect("write a");
        std::fs::write(dir.path().join("b.txt"), "beta\n").expect("write b");

        let result = dispatcher
            .dispatch_diff(
                &argv(&["/mnt/diff/a.txt", "/mnt/diff/b.txt"]),
                &test_caller(),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let message = result.message().to_string();
        assert!(message.contains("-alpha"), "got: {message}");
        assert!(message.contains("+beta"), "got: {message}");
        assert!(
            message.contains("rename from"),
            "differing paths are represented as a rename: {message}"
        );
        kaijutsu_diff::parse(&message).expect("two-path output must parse");
    }

    #[tokio::test]
    async fn a_missing_pre_image_diffs_as_an_addition() {
        let (dispatcher, _dir) = dispatcher_with_mount().await;
        // Never written to disk; created directly as a CRDT document.
        let blocks = dispatcher.block_store();
        let cache = dispatcher.kernel().file_cache(blocks);
        cache
            .create_or_replace("/mnt/diff/new.txt", "fresh\n")
            .await
            .expect("create in the CRDT");

        let result = dispatcher
            .dispatch_diff(&argv(&["/mnt/diff/new.txt"]), &test_caller())
            .await;
        assert!(result.is_ok(), "{result:?}");
        let message = result.message().to_string();
        assert!(
            message.contains("--- /dev/null"),
            "an absent disk side is an addition: {message}"
        );
    }

    /// The historical source: a document's own journal is the pre-image.
    /// Needs the DB-backed store — an in-memory kernel journals nothing, and
    /// that has to fail loudly rather than silently diff against "now".
    #[tokio::test]
    async fn from_a_journalled_seq_diffs_against_that_point_in_history() {
        use crate::kj::test_helpers::test_dispatcher_crdt_rc;

        let dispatcher = Arc::new(test_dispatcher_crdt_rc().await);
        dispatcher.set_self_arc();
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("h.txt"), "first\n").expect("write");
        dispatcher
            .kernel()
            .mount("/mnt/hist", crate::vfs::LocalBackend::new(dir.path()))
            .await;

        let blocks = dispatcher.block_store();
        let cache = dispatcher.kernel().file_cache(blocks);
        let (ctx, block) = cache.get_or_load("/mnt/hist/h.txt").await.expect("load");

        // The point in history we will diff back to.
        let (_, before_edit) = blocks.oplog_seq_range(ctx).expect("seq range");

        blocks
            .edit_text(ctx, &block, 0, "second\n", "first\n".chars().count())
            .expect("edit");
        cache.mark_dirty("/mnt/hist/h.txt");

        let result = dispatcher
            .dispatch_diff(
                &argv(&["/mnt/hist/h.txt", "--from", &before_edit.to_string()]),
                &test_caller(),
            )
            .await;
        assert!(result.is_ok(), "kj diff --from failed: {result:?}");
        let message = result.message().to_string();
        assert!(message.contains("-first"), "got: {message}");
        assert!(message.contains("+second"), "got: {message}");

        // And an unreachable seq fails loud, naming the range that *is*
        // available rather than quietly diffing against something else.
        let result = dispatcher
            .dispatch_diff(
                &argv(&["/mnt/hist/h.txt", "--from", "99999"]),
                &test_caller(),
            )
            .await;
        assert!(!result.is_ok(), "a seq past head must error: {result:?}");
    }

    /// Ownership-awareness, asserted by absence: a config-owned path must
    /// answer through the mount table's `ConfigCrdtFs` block and must NOT mint
    /// a `FileDocumentCache` copy shadowing it. That shadow is the exact bug
    /// class `docs/config-crdt-ownership.md` deleted by construction, and
    /// `resolve_editor_target` is what keeps it deleted here.
    #[tokio::test]
    async fn a_config_path_answers_through_its_owner_and_mints_no_shadow_document() {
        use crate::kj::test_helpers::test_dispatcher_crdt_rc;

        let dispatcher = Arc::new(test_dispatcher_crdt_rc().await);
        dispatcher.set_self_arc();
        let path = "/etc/rc/coder/create/S00-stance.kai";

        let result = dispatcher
            .dispatch_diff(&argv(&[path]), &test_caller())
            .await;
        assert!(result.is_ok(), "kj diff on an rc script failed: {result:?}");
        // There is no host file behind a config path — the CRDT *is* the
        // backend — so both sides read the same document and agree. Honest
        // answer, not a missing feature.
        assert!(
            result.message().contains("no differences"),
            "got: {}",
            result.message()
        );

        let shadow = crate::file_tools::cache::file_context_id(path);
        assert!(
            !dispatcher.block_store().contains(shadow),
            "kj diff must not mint a file-doc shadow of a config-owned path"
        );
    }

    #[tokio::test]
    async fn a_path_that_does_not_exist_anywhere_errors() {
        let (dispatcher, _dir) = dispatcher_with_mount().await;
        let result = dispatcher
            .dispatch_diff(&argv(&["/mnt/diff/nope.txt"]), &test_caller())
            .await;
        assert!(!result.is_ok(), "expected an error, got {result:?}");
    }

    #[tokio::test]
    async fn bare_diff_renders_help_without_a_context() {
        let (dispatcher, _dir) = dispatcher_with_mount().await;
        let result = dispatcher.dispatch_diff(&[], &test_caller()).await;
        assert!(
            result.is_ok(),
            "bare `kj diff` should render help: {result:?}"
        );
    }
}
