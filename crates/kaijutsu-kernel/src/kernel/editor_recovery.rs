//! Recovering a vi argument list on the kernel (`docs/vi.md`, "Recovering an
//! argument list"): writing a session's record, listing the records a
//! restart left, reopening one, and discarding one.
//!
//! `EditorSessions` stays free of database access; every write to
//! `editor_recovery_*` is made here, from the `Kernel` methods that drive a
//! session. A failed write goes to the session's status line and to the log,
//! and never stops the session.

use super::*;
use crate::editor::{EditorOpener, EditorSessionId, EditorState};
use crate::kernel_db::{RecoveryFile, RecoveryId, RecoverySession};

/// One file of a [`RecoverableList`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecoverableFile {
    pub path: String,
    /// Whether this is the list's current file.
    pub current: bool,
    /// Whether the file has a `dirty_file_buffers` row.
    pub dirty: bool,
}

/// One recoverable list, as `vi -r` and `kj vi recover` show it. Full ids,
/// like `kj vi list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecoverableList {
    /// 1-based position in the listing: the number a pick takes.
    pub pick: usize,
    /// Full id of the record (`RecoveryId::to_hex`).
    pub recovery_id: String,
    /// The tail of `recovery_id`, which a pick may name by prefix.
    pub short_id: String,
    /// The current file.
    pub path: String,
    /// The current file's 0-based position in the list.
    pub arg_index: usize,
    pub arg_count: usize,
    pub files: Vec<RecoverableFile>,
    /// The files with a `dirty_file_buffers` row, in list order.
    pub dirty_files: Vec<String>,
    pub dirty: bool,
    /// Full id string of the opener's principal.
    pub opener: String,
    /// The opener's character name.
    pub opener_name: String,
    /// Full id string of the context the opener opened it from.
    pub context_id: String,
    pub opened_at: i64,
    pub updated_at: i64,
    /// Seconds since `updated_at`, when listed.
    pub age_secs: i64,
}

/// `a` then `b`, as one status line.
pub(super) fn join_notes(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    }
}

/// A duration in seconds as the largest whole unit: `45s`, `12m`, `3h`, `2d`.
pub fn age_label(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The text `vi -r` and `kj vi recover` print for a listing: one line per
/// list, `pick  id  age  opener  files`, the current file in brackets and a
/// `+` after each file with unsaved changes.
pub fn render_recoverable_lists(lists: &[RecoverableList]) -> String {
    if lists.is_empty() {
        return "no recoverable vi lists".to_string();
    }
    lists
        .iter()
        .map(|l| {
            let files = l
                .files
                .iter()
                .map(|f| {
                    let name = if f.current { format!("[{}]", f.path) } else { f.path.clone() };
                    if f.dirty { format!("{name}+") } else { name }
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("{}  {}  {}  {}  {files}", l.pick, l.short_id, age_label(l.age_secs), l.opener_name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Kernel {
    /// Write the recovery record of session `id`. Returns the status-line
    /// text of a failure, after logging it. A headless session has no record.
    pub(super) fn write_editor_recovery(&self, id: EditorSessionId) -> Option<String> {
        let snapshot = match self.editor_sessions.lock().0.recovery_snapshot(id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return None,
            Err(e) => {
                tracing::error!("editor recovery: no snapshot of session {}: {e}", id.as_u64());
                return Some(format!("recovery record not written: {e}"));
            }
        };
        let written = self.db.lock().write_editor_recovery(
            snapshot.recovery_id,
            snapshot.opener_principal,
            snapshot.opener_context_id,
            snapshot.current_index,
            &snapshot.files,
        );
        match written {
            Ok(()) => None,
            Err(e) => {
                tracing::error!(
                    "editor recovery: write of record {} for session {} failed: {e}",
                    snapshot.recovery_id.short(),
                    id.as_u64()
                );
                Some(format!("this list cannot be recovered with vi -r: record write failed: {e}"))
            }
        }
    }

    /// Delete a closed session's recovery record. Returns the text of a
    /// failure, after logging it; the record then stays listed.
    pub(super) fn delete_editor_recovery(&self, id: RecoveryId) -> Option<String> {
        match self.db.lock().delete_editor_recovery(id) {
            Ok(_) => None,
            Err(e) => {
                tracing::error!("editor recovery: delete of record {} failed: {e}", id.short());
                Some(format!("recovery record {} not removed: {e}", id.short()))
            }
        }
    }

    /// The records a restart left: every record except those of sessions open
    /// now, newest write first.
    fn recoverable_records(&self) -> Result<Vec<RecoverySession>, String> {
        let live = self.editor_sessions.lock().0.live_recovery_ids();
        let mut records = self
            .db
            .lock()
            .list_editor_recoveries()
            .map_err(|e| format!("cannot read the editor recovery records: {e}"))?;
        records.retain(|r| !live.contains(&r.recovery_id));
        Ok(records)
    }

    /// Every recoverable list, newest first, for `vi -r` and `kj vi recover`.
    pub fn editor_recoveries(&self) -> Result<Vec<RecoverableList>, String> {
        let records = self.recoverable_records()?;
        self.recoverable_lists(&records)
    }

    /// The listing of `records`: `pick` is each record's 1-based position in
    /// the slice.
    fn recoverable_lists(&self, records: &[RecoverySession]) -> Result<Vec<RecoverableList>, String> {
        let db = self.db.lock();
        let dirty: std::collections::HashSet<String> = db
            .list_dirty_file_buffers()
            .map_err(|e| format!("cannot read the swap markers: {e}"))?
            .into_iter()
            .map(|row| row.path)
            .collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(records
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let files: Vec<RecoverableFile> = r
                    .files
                    .iter()
                    .enumerate()
                    .map(|(at, f)| RecoverableFile {
                        path: f.path.clone(),
                        current: at == r.current_index,
                        dirty: dirty.contains(&f.path),
                    })
                    .collect();
                let dirty_files: Vec<String> =
                    files.iter().filter(|f| f.dirty).map(|f| f.path.clone()).collect();
                RecoverableList {
                    pick: i + 1,
                    recovery_id: r.recovery_id.to_hex(),
                    short_id: r.recovery_id.short(),
                    path: files[r.current_index].path.clone(),
                    arg_index: r.current_index,
                    arg_count: files.len(),
                    dirty: !dirty_files.is_empty(),
                    dirty_files,
                    files,
                    opener: r.opener_principal.to_hex(),
                    opener_name: db.name_for(r.opener_principal),
                    context_id: r.opener_context_id.to_hex(),
                    opened_at: r.opened_at,
                    updated_at: r.updated_at,
                    age_secs: (now - r.updated_at) / 1000,
                }
            })
            .collect())
    }

    /// The record `pick` names. A pick of digits only is a 1-based position
    /// in the listing and is refused when it is outside it; it never matches
    /// an id. Any other pick is a prefix of a record's short id, or a full
    /// id. An unknown or ambiguous pick is refused by name.
    fn pick_recovery(records: &[RecoverySession], pick: &str) -> Result<usize, String> {
        if records.is_empty() {
            return Err("no recoverable vi lists".to_string());
        }
        let full_id = uuid::Uuid::parse_str(pick).is_ok();
        if !full_id && !pick.is_empty() && pick.bytes().all(|b| b.is_ascii_digit()) {
            return match pick.parse::<usize>() {
                Ok(n) if (1..=records.len()).contains(&n) => Ok(n - 1),
                _ => Err(format!(
                    "no recoverable list is numbered {pick}: use a number from 1 to {} (an id made only of digits is picked by its full id)",
                    records.len()
                )),
            };
        }
        let matches: Vec<usize> = records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.recovery_id.matches_short(pick))
            .map(|(i, _)| i)
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!(
                "no recoverable list matches \"{pick}\": use a number from 1 to {} or a recovery id",
                records.len()
            )),
            many => Err(format!(
                "\"{pick}\" matches {} recoverable lists ({}): name more of the id",
                many.len(),
                many.iter()
                    .map(|i| records[*i].recovery_id.short())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Reopen a recoverable list for `opener`: one session over the same
    /// list at the same index, each visited file's cursor restored when the
    /// file loads. A file with a swap loads as unsaved changes against the
    /// file on disk, and its swap is acknowledged when it loads (`:w` then
    /// writes it, `ZQ` restores the disk text). The session carries the
    /// record's id and updates its rows in place.
    ///
    /// Refused, with nothing opened or acknowledged and the record kept, when
    /// the opener already has a live session holding a file of the list: one
    /// session per file, per person.
    ///
    /// A path that no longer resolves is dropped from the list and named on
    /// the status line. A list with nothing left is refused and its record
    /// kept.
    pub async fn editor_recover(
        &self,
        pick: &str,
        opener: EditorOpener,
    ) -> Result<(EditorSessionId, EditorState), String> {
        let records = self.recoverable_records()?;
        let record = &records[Self::pick_recovery(&records, pick)?];
        let blocks = self.blocks();
        let file_cache = self.file_cache().clone();

        let paths: Vec<String> = record.files.iter().map(|f| f.path.clone()).collect();
        if let Some((held, path)) = self.editor_sessions.lock().0.find_joinable(opener.principal, &paths) {
            return Err(format!(
                "you already have session {} open on {path}: close it first, since a person has one session per file; recoverable list {} is kept",
                held.as_u64(),
                record.recovery_id.short()
            ));
        }

        let mut kept: Vec<(usize, &RecoveryFile, crate::editor::EditorTarget)> = Vec::new();
        let mut dropped: Vec<String> = Vec::new();
        for (position, file) in record.files.iter().enumerate() {
            match crate::editor::resolve_editor_target(&file.path, &file_cache).await {
                Ok(target) => kept.push((position, file, target)),
                Err(e) => dropped.push(format!("{} ({e})", file.path)),
            }
        }
        if kept.is_empty() {
            return Err(format!(
                "recoverable list {} has no file that opens, so its record is kept: {}",
                record.recovery_id.short(),
                dropped.join("; ")
            ));
        }
        let current = kept
            .iter()
            .position(|(position, _, _)| *position >= record.current_index)
            .unwrap_or(kept.len() - 1);
        let (_, current_file, target) = kept[current];
        let swapped = self
            .db
            .lock()
            .get_dirty_file_buffer(&current_file.path)
            .map_err(|e| format!("cannot read the swap marker of {}: {e}", current_file.path))?
            .is_some();
        let disk_text = if swapped { Some(file_cache.read_disk_text(&current_file.path).await?) } else { None };

        let files: Vec<RecoveryFile> = kept.iter().map(|(_, f, _)| (*f).clone()).collect();
        let (id, mut state) = self.editor_sessions.lock().0.open_recovered(
            &files,
            current,
            target,
            disk_text.as_deref(),
            blocks,
            opener,
            record.recovery_id,
        )?;
        if let Err(e) = file_cache.pin(&current_file.path, id.as_u64()) {
            let _ = self.editor_sessions.lock().0.forget(id);
            return Err(e);
        }
        if swapped {
            file_cache.acknowledge_swap(&current_file.path);
        }

        let mut message = state.message.take();
        if !dropped.is_empty() {
            message = join_notes(message, Some(format!("dropped from the list: {}", dropped.join("; "))));
        }
        // The session carries the record's id, so this write updates the
        // record in place; dropped files leave with the old file rows.
        message = join_notes(message, self.write_editor_recovery(id));
        state.message = message;
        Ok((id, state))
    }

    /// [`editor_recover`](Self::editor_recover) plus the `open_editor` signal
    /// to the opener's app windows, as `editor_open_signaled` does.
    pub async fn editor_recover_signaled(
        &self,
        pick: &str,
        opener: EditorOpener,
    ) -> Result<(EditorSessionId, EditorState), String> {
        let (id, state) = self.editor_recover(pick, opener).await?;
        self.signal_open_editor(id, &state, Some(opener.principal)).await;
        Ok((id, state))
    }

    /// Delete the record `pick` names and nothing else: no swap is touched.
    /// Returns the list as it was listed.
    pub fn editor_recovery_discard(&self, pick: &str) -> Result<RecoverableList, String> {
        let records = self.recoverable_records()?;
        let at = Self::pick_recovery(&records, pick)?;
        let id = records[at].recovery_id;
        let mut listed = self
            .recoverable_lists(std::slice::from_ref(&records[at]))?
            .pop()
            .ok_or_else(|| "recovery listing lost its record".to_string())?;
        listed.pick = at + 1;
        let removed = self
            .db
            .lock()
            .delete_editor_recovery(id)
            .map_err(|e| format!("cannot delete recovery record {}: {e}", listed.short_id))?;
        if !removed {
            return Err(format!("recovery record {} was already gone", listed.short_id));
        }
        Ok(listed)
    }

    /// The status line a plain `vi` shows when a recoverable list holds one
    /// of `paths`, one clause per path held. A failure to read the records is
    /// reported in its place rather than hidden.
    pub(super) fn recovery_notice(&self, paths: &[String]) -> Option<String> {
        match self.recoverable_records() {
            Ok(records) => {
                let held: Vec<String> = paths
                    .iter()
                    .filter(|p| records.iter().any(|r| r.files.iter().any(|f| &f.path == *p)))
                    .map(|p| format!("{p} is in a recoverable list (vi -r)"))
                    .collect();
                (!held.is_empty()).then(|| held.join("; "))
            }
            Err(e) => {
                tracing::error!("editor recovery: {e}");
                Some(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;
    use kaijutsu_types::{ContextId, SessionId};
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    const A: &str = "/mem/a.txt";
    const B: &str = "/mem/b.txt";
    const C: &str = "/mem/c.txt";
    const D: &str = "/mem/d.txt";
    const ALL: [&str; 4] = [A, B, C, D];

    fn me() -> PrincipalId {
        PrincipalId::system()
    }

    fn opener_for(principal: PrincipalId) -> EditorOpener {
        EditorOpener {
            principal,
            performer: principal,
            reviewer: None,
            context_id: ContextId::new(),
            session_id: SessionId::new(),
        }
    }

    fn abc() -> Vec<String> {
        [A, B, C].iter().map(|p| p.to_string()).collect()
    }

    /// A kernel with `/mem/{a,b,c,d}.txt` on disk. Each file holds two
    /// lines: `alpha`/`one`, `beta`/`two`, `gamma`/`three`, `delta`/`four`.
    async fn kernel_with_files() -> Kernel {
        let kernel = Kernel::new_ephemeral("recovery").await;
        kernel.mount("/mem", MemoryBackend::new()).await;
        for (path, text) in [(A, "alpha\none"), (B, "beta\ntwo"), (C, "gamma\nthree"), (D, "delta\nfour")] {
            kernel.vfs().write_all(std::path::Path::new(path), text.as_bytes()).await.unwrap();
        }
        kernel
    }

    /// A kernel started over the same block store and database as `old`: a
    /// cold file cache, an empty session registry, and disk files copied from
    /// `old` for each of `keep`. What a restart leaves is the database and
    /// the disk, so that is all this shares.
    async fn restart(old: &Kernel, keep: &[&str]) -> (Kernel, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let kernel = Kernel::with_flows_and_threads(
            kaijutsu_types::KernelId::new(),
            "restarted",
            old.block_flows().clone(),
            dir.path(),
            old.blocks().clone(),
            old.kernel_db().clone(),
            1,
        )
        .await;
        kernel.mount("/mem", MemoryBackend::new()).await;
        for path in keep {
            let bytes = old.vfs().read_all(std::path::Path::new(path)).await.unwrap();
            kernel.vfs().write_all(std::path::Path::new(path), &bytes).await.unwrap();
        }
        (kernel, dir)
    }

    async fn disk(kernel: &Kernel, path: &str) -> String {
        String::from_utf8(kernel.vfs().read_all(std::path::Path::new(path)).await.unwrap()).unwrap()
    }

    fn records(kernel: &Kernel) -> Vec<RecoverySession> {
        kernel.kernel_db().lock().list_editor_recoveries().unwrap()
    }

    fn only_record(kernel: &Kernel) -> RecoverySession {
        let mut all = records(kernel);
        assert_eq!(all.len(), 1, "expected exactly one record: {all:?}");
        all.remove(0)
    }

    fn cursor_of(file: &RecoveryFile) -> (usize, usize) {
        (file.cursor_line, file.cursor_col)
    }

    fn deny(table: &'static str, kernel: &Kernel, delete: bool) {
        kernel.kernel_db().lock().conn_for_test_mut().authorizer(Some(move |auth: AuthContext<'_>| {
            match auth.action {
                AuthAction::Insert { table_name } if !delete && table_name == table => Authorization::Deny,
                AuthAction::Delete { table_name } if delete && table_name == table => Authorization::Deny,
                _ => Authorization::Allow,
            }
        }))
        .unwrap();
    }

    fn allow_all(kernel: &Kernel) {
        kernel
            .kernel_db()
            .lock()
            .conn_for_test_mut()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
    }

    // ── Writing the record ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_session_with_an_opener_records_its_list_on_open_a_move_e_and_w() {
        let kernel = kernel_with_files().await;
        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();

        let opened = only_record(&kernel);
        let paths = |r: &RecoverySession| r.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&opened), abc());
        assert_eq!(opened.current_index, 0);
        assert_eq!(
            opened.files.iter().map(|f| f.visited).collect::<Vec<_>>(),
            [true, false, false],
            "only the file the session loaded is visited"
        );

        kernel.editor_keys(id, ":n<CR>", me()).await.unwrap();
        let moved = only_record(&kernel);
        assert_eq!(moved.recovery_id, opened.recovery_id, "one record for the session's life");
        assert_eq!(moved.current_index, 1);
        assert!(moved.files[1].visited);

        kernel.editor_keys(id, &format!(":e {D}<CR>"), me()).await.unwrap();
        let grown = only_record(&kernel);
        assert_eq!(grown.files.len(), 4, "`:e` of a new path grows the list");
        assert_eq!((grown.current_index, grown.files[3].path.as_str()), (3, D));

        // Keys between writes record nothing; `:w` records the cursor.
        kernel.editor_keys(id, "jl", me()).await.unwrap();
        assert_eq!(cursor_of(&only_record(&kernel).files[3]), (0, 0), "cursor travel is not written");
        kernel.editor_keys(id, ":w<CR>", me()).await.unwrap();
        assert_eq!(cursor_of(&only_record(&kernel).files[3]), (1, 1), "`:w` writes the cursor");
    }

    #[tokio::test]
    async fn a_direct_save_records_the_cursor_too() {
        let kernel = kernel_with_files().await;
        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        kernel.editor_keys(id, "jl", me()).await.unwrap();
        kernel.editor_save(id).await.unwrap();
        assert_eq!(cursor_of(&only_record(&kernel).files[0]), (1, 1));
    }

    #[tokio::test]
    async fn a_move_records_the_cursor_of_the_buffer_it_leaves() {
        let kernel = kernel_with_files().await;
        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        kernel.editor_keys(id, "jll:n<CR>", me()).await.unwrap();
        assert_eq!(cursor_of(&only_record(&kernel).files[0]), (1, 2));
    }

    #[tokio::test]
    async fn joining_a_session_with_more_paths_records_the_longer_list() {
        let kernel = kernel_with_files().await;
        let who = me();
        kernel.editor_open_list(&[A.to_string()], Some(opener_for(who))).await.unwrap();
        kernel.editor_open_list(&[A.to_string(), D.to_string()], Some(opener_for(who))).await.unwrap();
        let record = only_record(&kernel);
        assert_eq!(record.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), [A, D]);
    }

    #[tokio::test]
    async fn every_close_route_deletes_the_record() {
        let kernel = kernel_with_files().await;
        for keys in [":q<CR>", "ZZ", "ZQ", ":q!<CR>", ":wq<CR>", ":x<CR>"] {
            let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
            assert_eq!(records(&kernel).len(), 1, "record exists before {keys}");
            kernel.editor_keys(id, keys, me()).await.unwrap();
            assert!(!kernel.editor_sessions.lock().0.is_open(id), "{keys} closed the session");
            assert!(records(&kernel).is_empty(), "{keys} must delete the record");
        }

        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        let report = kernel.editor_quit(id, me()).unwrap();
        assert!(report.recovery.is_some() && report.recovery_error.is_none());
        assert!(records(&kernel).is_empty(), "`kj vi quit` deletes the record");

        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        let recovery = kernel.editor_sessions.lock().0.forget(id);
        assert_eq!(recovery, Some(only_record(&kernel).recovery_id), "`forget` names the record to delete");
    }

    #[tokio::test]
    async fn a_headless_session_is_not_recorded() {
        let kernel = kernel_with_files().await;
        let (id, _) = kernel.editor_open_list(&abc(), None).await.unwrap();
        kernel.editor_keys(id, ":n<CR>", me()).await.unwrap();
        kernel.editor_keys(id, ":w<CR>", me()).await.unwrap();
        kernel.editor_save(id).await.unwrap();
        assert!(records(&kernel).is_empty(), "nobody opened it, so nobody can recover it");
        assert!(kernel.editor_recoveries().unwrap().is_empty());
    }

    // ── A failed write is visible and the session keeps working ───────────

    #[tokio::test]
    async fn a_record_write_failure_reaches_the_status_line_and_the_session_keeps_working() {
        let kernel = kernel_with_files().await;
        deny("editor_recovery_files", &kernel, false);
        let (id, st) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        let message = st.message.expect("the open reports the failed write");
        assert!(message.contains("cannot be recovered with vi -r"), "{message}");
        assert!(records(&kernel).is_empty());
        allow_all(&kernel);

        // The write at `:n` lands, then a failure at `:w` leaves it whole.
        kernel.editor_keys(id, ":n<CR>", me()).await.unwrap();
        let before = only_record(&kernel);
        deny("editor_recovery_files", &kernel, false);
        kernel.editor_keys(id, "x", me()).await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>", me()).await.unwrap();
        assert!(st.message.as_deref().is_some_and(|m| m.contains("record write failed")), "{st:?}");
        assert!(!st.dirty, "the text was still written: the session keeps working");
        assert_eq!(disk(&kernel, B).await, "eta\ntwo");
        allow_all(&kernel);
        let after = only_record(&kernel);
        assert_eq!(after.files, before.files, "a failed write leaves the earlier record whole");
    }

    #[tokio::test]
    async fn a_record_delete_failure_on_close_is_reported_and_the_record_stays_listed() {
        let kernel = kernel_with_files().await;
        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        deny("editor_recovery_sessions", &kernel, true);
        let st = kernel.editor_keys(id, ":q<CR>", me()).await.unwrap();
        allow_all(&kernel);
        assert!(st.message.as_deref().is_some_and(|m| m.contains("not removed")), "{st:?}");
        assert_eq!(records(&kernel).len(), 1, "the record stays, and is offered by vi -r");

        let (id, _) = kernel.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        deny("editor_recovery_sessions", &kernel, true);
        let report = kernel.editor_quit(id, me()).unwrap();
        allow_all(&kernel);
        assert!(report.recovery_error.is_some());
    }

    // ── After a restart ───────────────────────────────────────────────────

    /// Open `a b c` as `me`, edit `b` and leave it hidden and dirty, end on
    /// `a`, and restart. Returns the old kernel (kept alive: its database and
    /// block store are the ones the new kernel runs over) and the new one.
    async fn a_list_cut_off_with_a_dirty_hidden_b() -> (Kernel, Kernel, tempfile::TempDir) {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, ":n<CR>jlx:N!<CR>", me()).await.unwrap();
        assert_eq!(old.editor_state(id).unwrap().path, A);
        let (new, dir) = restart(&old, &[A, B, C, D]).await;
        (old, new, dir)
    }

    #[tokio::test]
    async fn a_live_session_is_not_offered_but_a_cut_off_one_is() {
        let (old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        assert!(old.editor_recoveries().unwrap().is_empty(), "a session open now is not recoverable");

        let lists = new.editor_recoveries().unwrap();
        assert_eq!(lists.len(), 1);
        let list = &lists[0];
        assert_eq!((list.pick, list.arg_index, list.arg_count, list.path.as_str()), (1, 0, 3, A));
        assert_eq!(list.opener_name, "system");
        assert_eq!(list.dirty_files, [B]);
        assert_eq!(
            list.files.iter().map(|f| (f.current, f.dirty)).collect::<Vec<_>>(),
            [(true, false), (false, true), (false, false)]
        );
        let text = render_recoverable_lists(&lists);
        assert!(text.starts_with(&format!("1  {}  ", list.short_id)), "{text}");
        assert!(text.ends_with(&format!("system  [{A}] {B}+ {C}")), "{text}");
        assert_eq!(render_recoverable_lists(&[]), "no recoverable vi lists");
    }

    #[tokio::test]
    async fn reopening_restores_the_list_the_index_and_each_cursor() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        // Cursor at line 1, column 2 of b, then on to c.
        old.editor_keys(id, ":n<CR>jll:n<CR>", me()).await.unwrap();
        let (new, _dir) = restart(&old, &ALL).await;
        let stale = only_record(&new);

        let (nid, st) = new.editor_recover("1", opener_for(me())).await.unwrap();
        assert_eq!((st.path.as_str(), st.arg_index, st.arg_count), (C, 2, 3));
        assert_eq!(st.text, "gamma\nthree");

        let fresh = only_record(&new);
        assert_eq!(fresh.recovery_id, stale.recovery_id, "the session carries the record's id");
        assert_eq!(fresh.opened_at, stale.opened_at, "a reopen keeps opened_at");
        assert!(fresh.updated_at >= stale.updated_at);
        assert_eq!(fresh.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), abc());
        assert_eq!(cursor_of(&fresh.files[1]), (1, 2), "an unvisited-again file keeps its recorded cursor");
        assert!(fresh.files[1].visited);

        let st = new.editor_keys(nid, ":N<CR>", me()).await.unwrap();
        assert_eq!((st.path.as_str(), st.cursor), (B, 7), "line 1, column 2 of b: the cursor recorded when the session left it");
    }

    #[tokio::test]
    async fn a_recovered_buffer_is_dirty_writable_without_ack_and_zq_restores_disk() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&[A.to_string(), B.to_string()], Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, "iX<Esc>", me()).await.unwrap();
        assert!(old.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_some());
        let (new, _dir) = restart(&old, &ALL).await;

        let (nid, st) = new.editor_recover("1", opener_for(me())).await.unwrap();
        assert_eq!(st.text, "Xalpha\none", "the buffer is the swap's text");
        assert!(st.dirty, "and it reads as changed against the file on disk");
        assert_eq!(disk(&new, A).await, "alpha\none");
        assert!(!new.file_cache().swap_recovered(A), "vi -r acknowledged the swap");
        assert!(
            new.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_some(),
            "the swap row stays until a write clears it"
        );
        let listed = new.editor_list();
        assert_eq!(listed[0].dirty_files, [A], "kj vi list shows it dirty");

        // `:w` writes the buffer with no `kj swap ack`.
        let st = new.editor_keys(nid, ":w<CR>", me()).await.unwrap();
        assert!(st.message.is_none(), "{st:?}");
        assert!(!st.dirty);
        assert_eq!(disk(&new, A).await, "Xalpha\none");
        assert!(new.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_none(), "the write clears the row");
    }

    /// The disk-moved check (`docs/file-buffers.md`, rule 3) still guards a
    /// recovered buffer: a file written elsewhere after the restart refuses
    /// `:w`, and `:w!` overrides it.
    #[tokio::test]
    async fn a_recovered_buffer_still_meets_the_disk_moved_check() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&[A.to_string()], Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, "iX<Esc>", me()).await.unwrap();
        let (new, _dir) = restart(&old, &ALL).await;
        // The new disk's generation counter starts over, so write enough to
        // pass the generation the swap row recorded.
        for _ in 0..200 {
            new.vfs().write_all(std::path::Path::new(A), b"moved\nunder us").await.unwrap();
        }

        let (nid, _) = new.editor_recover("1", opener_for(me())).await.unwrap();
        let st = new.editor_keys(nid, ":w<CR>", me()).await.unwrap();
        assert!(st.message.as_deref().is_some_and(|m| m.contains("W12")), "{st:?}");
        assert!(st.dirty);
        assert_eq!(disk(&new, A).await, "moved\nunder us");

        let st = new.editor_keys(nid, ":w!<CR>", me()).await.unwrap();
        assert!(st.message.is_none() && !st.dirty, "{st:?}");
        assert_eq!(disk(&new, A).await, "Xalpha\none");
    }

    #[tokio::test]
    async fn zq_on_a_recovered_buffer_rolls_back_to_the_disk_text() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&[A.to_string()], Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, "iX<Esc>", me()).await.unwrap();
        let (new, _dir) = restart(&old, &ALL).await;

        let (nid, _) = new.editor_recover("1", opener_for(me())).await.unwrap();
        new.editor_keys(nid, "ZQ", me()).await.unwrap();
        assert_eq!(new.file_cache().try_read_content(A).await.unwrap(), "alpha\none");
        assert_eq!(disk(&new, A).await, "alpha\none", "ZQ writes nothing to disk");
        assert!(records(&new).is_empty(), "the close deleted the new record");
        assert!(
            new.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_none(),
            "the block equals disk again, so no swap row remains"
        );
        assert!(new.kernel_db().lock().list_dirty_file_buffers().unwrap().is_empty());

        // A later plain `vi` and `:w` need no `kj swap ack`.
        let (pid, st) = new.editor_open_list(&[A.to_string()], Some(opener_for(me()))).await.unwrap();
        assert_eq!(st.text, "alpha\none");
        assert!(!new.file_cache().swap_recovered(A));
        new.editor_keys(pid, "x:w<CR>", me()).await.unwrap();
        assert_eq!(disk(&new, A).await, "lpha\none");
    }

    /// `kj vi quit` settles the same way: the rollback to disk text leaves no
    /// row; when disk moved since, the row stays.
    #[tokio::test]
    async fn quit_on_a_recovered_buffer_leaves_no_row_unless_disk_moved() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&[A.to_string()], Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, "iX<Esc>", me()).await.unwrap();
        let (new, _dir) = restart(&old, &ALL).await;
        let (nid, _) = new.editor_recover("1", opener_for(me())).await.unwrap();
        let report = new.editor_quit(nid, me()).unwrap();
        new.editor_settle_rolled_back(&report.rolled_back).await.unwrap();
        assert_eq!(report.rolled_back, [A]);
        assert!(new.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_none());

        // Disk moved after the reopen: the rollback text differs from disk, so
        // the entry stays dirty as before.
        let old2 = kernel_with_files().await;
        let (id, _) = old2.editor_open_list(&[A.to_string()], Some(opener_for(me()))).await.unwrap();
        old2.editor_keys(id, "iX<Esc>", me()).await.unwrap();
        let (new2, _dir2) = restart(&old2, &ALL).await;
        let (nid, _) = new2.editor_recover("1", opener_for(me())).await.unwrap();
        new2.vfs().write_all(std::path::Path::new(A), b"moved").await.unwrap();
        let report = new2.editor_quit(nid, me()).unwrap();
        new2.editor_settle_rolled_back(&report.rolled_back).await.unwrap();
        assert!(new2.kernel_db().lock().get_dirty_file_buffer(A).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_hidden_swap_stays_unacknowledged_until_the_file_is_visited() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let (nid, st) = new.editor_recover("1", opener_for(me())).await.unwrap();
        assert_eq!(st.path, A);
        assert!(new.file_cache().swap_recovered(B), "the resolve at reopen recovered b's swap, unacknowledged");

        let st = new.editor_keys(nid, ":n<CR>", me()).await.unwrap();
        assert_eq!((st.path.as_str(), st.text.as_str(), st.dirty), (B, "beta\nto", true));
        assert!(!new.file_cache().swap_recovered(B), "visiting b acknowledged it");
        assert_eq!(st.cursor, 5 + 1, "b's recorded cursor is restored on first load");

        let st = new.editor_keys(nid, ":w<CR>", me()).await.unwrap();
        assert!(st.message.is_none(), "{st:?}");
        assert_eq!(disk(&new, B).await, "beta\nto");
    }

    #[tokio::test]
    async fn a_path_that_no_longer_opens_is_dropped_and_named() {
        let old = kernel_with_files().await;
        let (id, _) = old.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        old.editor_keys(id, ":n<CR>", me()).await.unwrap();
        let (new, _dir) = restart(&old, &[A, C]).await;

        let (_, st) = new.editor_recover("1", opener_for(me())).await.unwrap();
        let message = st.message.expect("the drop is announced");
        assert!(message.contains("dropped from the list") && message.contains(B), "{message}");
        assert_eq!((st.arg_count, st.path.as_str(), st.arg_index), (2, C, 1), "the next surviving file is current");
        assert_eq!(only_record(&new).files.len(), 2);
    }

    #[tokio::test]
    async fn a_list_with_nothing_left_is_refused_and_its_record_kept() {
        let old = kernel_with_files().await;
        old.editor_open_list(&abc(), Some(opener_for(me()))).await.unwrap();
        let (new, _dir) = restart(&old, &[]).await;
        let stale = only_record(&new);

        let err = new.editor_recover("1", opener_for(me())).await.unwrap_err();
        assert!(err.contains(A) && err.contains("record is kept"), "{err}");
        assert_eq!(only_record(&new), stale, "the refused reopen left the record alone");
        assert!(new.editor_list().is_empty(), "no session opened");
    }

    #[tokio::test]
    async fn a_plain_open_of_a_held_path_says_so_once() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let (_, st) = new.editor_open_list(&[B.to_string()], Some(opener_for(me()))).await.unwrap();
        assert_eq!(st.message.as_deref(), Some(&*format!("{B} is in a recoverable list (vi -r)")));
        assert_eq!(st.text, "beta\nto", "the open is otherwise as before: the swap's text");
        assert!(new.file_cache().swap_recovered(B), "a plain open does not acknowledge the swap");

        let (_, st) = new.editor_open_list(&[D.to_string()], Some(opener_for(me()))).await.unwrap();
        assert!(st.message.is_none(), "a path no list holds gets no notice: {st:?}");

        // The notice is for records of sessions that are not open: the
        // session the reopen starts holds the same paths and says nothing.
        new.editor_recover("1", opener_for(PrincipalId::new())).await.unwrap();
        let (_, st) = new.editor_open_list(&[B.to_string()], Some(opener_for(PrincipalId::new()))).await.unwrap();
        assert!(
            st.message.as_deref().is_none_or(|m| !m.contains("recoverable")),
            "a live session's record is not a recoverable list: {st:?}"
        );
    }

    #[tokio::test]
    async fn discard_deletes_the_record_and_touches_no_swap() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let discarded = new.editor_recovery_discard("1").unwrap();
        assert_eq!(discarded.dirty_files, [B]);
        assert!(records(&new).is_empty());
        assert!(new.editor_recoveries().unwrap().is_empty());
        assert!(
            new.kernel_db().lock().get_dirty_file_buffer(B).unwrap().is_some(),
            "the swap row of b survives a discard"
        );
        assert!(new.editor_recovery_discard("1").unwrap_err().contains("no recoverable"));
    }

    #[tokio::test]
    async fn a_record_already_open_in_a_live_session_refuses_a_second_open() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let rec = only_record(&new);
        let (first, _) = new.editor_recover("1", opener_for(me())).await.unwrap();
        // Two callers that both listed the record: the second open meets the
        // live session that carries its id.
        let target = crate::editor::resolve_editor_target(A, new.file_cache()).await.unwrap();
        let err = new
            .editor_sessions
            .lock()
            .0
            .open_recovered(&rec.files, 0, target, None, new.blocks(), opener_for(PrincipalId::new()), rec.recovery_id)
            .unwrap_err();
        assert!(err.contains(&rec.recovery_id.short()) && err.contains(&format!("session {}", first.as_u64())), "{err}");
        assert_eq!(new.editor_list().len(), 1, "no second session opened");
        assert!(new.editor_recover("1", opener_for(me())).await.is_err(), "and the pick no longer lists it");
    }

    #[tokio::test]
    async fn a_failed_record_write_at_reopen_leaves_the_record_unlisted_while_the_session_lives() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let before = only_record(&new);
        deny("editor_recovery_files", &new, false);
        let (_, st) = new.editor_recover("1", opener_for(me())).await.unwrap();
        allow_all(&new);
        assert!(st.message.as_deref().is_some_and(|m| m.contains("record write failed")), "{st:?}");
        assert_eq!(only_record(&new), before, "the failed write left the record as it was");
        assert!(new.editor_recoveries().unwrap().is_empty(), "a live list is not offered");

        let (after_restart, _dir2) = restart(&new, &ALL).await;
        assert_eq!(after_restart.editor_recoveries().unwrap().len(), 1, "and is offered again after a restart");
    }

    #[tokio::test]
    async fn a_person_with_a_live_session_on_a_file_of_the_list_cannot_reopen_it() {
        let (_old, new, _dir) = a_list_cut_off_with_a_dirty_hidden_b().await;
        let who = PrincipalId::new();
        let (held, _) = new.editor_open_list(&[B.to_string()], Some(opener_for(who))).await.unwrap();

        let err = new.editor_recover("1", opener_for(who)).await.unwrap_err();
        assert!(err.contains(B) && err.contains(&format!("session {}", held.as_u64())), "{err}");
        assert!(new.file_cache().swap_recovered(B), "nothing was acknowledged");
        assert_eq!(new.editor_list().len(), 1, "nothing was opened");
        assert_eq!(new.editor_recoveries().unwrap().len(), 1, "the record is kept and listed");

        // Another person is not held back by it.
        assert!(new.editor_recover("1", opener_for(PrincipalId::new())).await.is_ok());
    }

    #[test]
    fn a_pick_is_a_number_or_an_id_prefix_and_a_bad_one_is_refused_by_name() {
        let record = |tail: &str| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&[1; 8]);
            for (i, b) in hex_tail(tail).iter().enumerate() {
                bytes[12 + i] = *b;
            }
            RecoverySession {
                recovery_id: RecoveryId::try_from_slice(&bytes).unwrap(),
                opener_principal: me(),
                opener_context_id: ContextId::new(),
                current_index: 0,
                opened_at: 0,
                updated_at: 0,
                files: vec![],
            }
        };
        fn hex_tail(s: &str) -> Vec<u8> {
            (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
        }
        let records = [record("aabbccd1"), record("aabbccd2"), record("a1223344"), record("99887766")];

        assert_eq!(Kernel::pick_recovery(&records, "2"), Ok(1), "digits that fit the listing are a number");
        assert_eq!(Kernel::pick_recovery(&records, "a122"), Ok(2), "an id prefix");
        assert_eq!(Kernel::pick_recovery(&records, &records[0].recovery_id.to_hex()), Ok(0), "a full id");
        let ambiguous = Kernel::pick_recovery(&records, "aabbccd").unwrap_err();
        assert!(ambiguous.contains("matches 2") && ambiguous.contains("aabbccd1") && ambiguous.contains("aabbccd2"), "{ambiguous}");
        let unknown = Kernel::pick_recovery(&records, "ffff").unwrap_err();
        assert!(unknown.contains("\"ffff\""), "{unknown}");
        let too_big = Kernel::pick_recovery(&records, "9").unwrap_err();
        assert!(too_big.contains(" 9") && too_big.contains("1 to 4"), "{too_big}");
        // "99" starts the last record's short id, but digits are a number,
        // and 99 is not in the listing.
        let digits = Kernel::pick_recovery(&records, "99").unwrap_err();
        assert!(digits.contains("numbered 99") && digits.contains("1 to 4"), "{digits}");
        assert!(Kernel::pick_recovery(&records, "99887766").unwrap_err().contains("numbered"));
        assert_eq!(Kernel::pick_recovery(&records, &records[3].recovery_id.to_hex()), Ok(3), "a full id of any shape");
        assert_eq!(Kernel::pick_recovery(&[], "1"), Err("no recoverable vi lists".to_string()));
    }

    #[test]
    fn age_label_picks_the_largest_unit() {
        assert_eq!(
            [0, 59, 60, 3599, 3600, 86_399, 86_400, 200_000].map(age_label),
            ["0s", "59s", "1m", "59m", "1h", "23h", "1d", "2d"]
        );
        assert_eq!(age_label(-5), "0s");
    }

}
