//! The kernel-owned register store (`docs/vi.md`, "A kernel-owned register
//! store"): applying a client's register writes, writing them to the table,
//! and publishing each accepted write.
//!
//! `EditorSessions` holds the store and stays free of database access; it
//! loads a player's rows through the loader installed here, and every write
//! to `editor_registers` is made here. A write reaches the store only after
//! the table took it.

use super::*;
use crate::editor::{EditorSessions, RegisterBatch, RegisterLoader};
use crate::flows::EditorFlow;
use kaijutsu_editor::{RegisterShape, RegisterText, RegisterWrite, Registers};
use kaijutsu_types::SessionId;

/// The rows `writes` set in `registers`, which already hold them: for each
/// write, its target registers' cells with its sequence number, counted from
/// `first_seq`. A later write's row for a register replaces an earlier one's.
fn rows_for(registers: &Registers, writes: &[RegisterWrite], first_seq: u64) -> Vec<(u64, RegisterText)> {
    writes
        .iter()
        .enumerate()
        .flat_map(|(i, write)| {
            let seq = first_seq + i as u64;
            Registers::targets(write).into_iter().map(move |name| {
                let cell = registers.cell(name).unwrap_or(RegisterText {
                    name,
                    text: String::new(),
                    shape: RegisterShape::Charwise,
                });
                (seq, cell)
            })
        })
        .collect()
}

impl Kernel {
    /// The loader `EditorSessions` fills a player's registers with: the
    /// player's rows, put in sequence order, and the sequence number after
    /// the highest.
    pub(super) fn register_loader(db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>) -> RegisterLoader {
        Box::new(move |principal| {
            let rows = db.lock().editor_registers(principal).map_err(|e| e.to_string())?;
            let next_seq = rows.iter().map(|(seq, _)| *seq).max().map_or(1, |m| m + 1);
            Ok((Registers::from_rows(rows)?, next_seq))
        })
    }

    /// Apply the register writes a client's editor made, in order, to
    /// `principal`'s store. The table takes them first; then the store does,
    /// and each is published with `origin`, the connection that sent them.
    /// A refused write, or a failed table write, changes nothing and is the
    /// error.
    pub fn registers_put(
        &self,
        principal: PrincipalId,
        origin: Option<SessionId>,
        writes: &[RegisterWrite],
    ) -> Result<(), String> {
        for write in writes {
            write.check()?;
        }
        if writes.is_empty() {
            return Ok(());
        }
        let mut sessions = self.editor_sessions.lock();
        let player = sessions.0.player_registers(principal)?;
        let mut applied = Registers::default();
        applied.replace_text(&player.registers.text_cells())?;
        for write in writes {
            applied.apply(write)?;
        }
        let first_seq = player.next_seq;
        let rows = rows_for(&applied, writes, first_seq);
        self.db.lock().write_editor_registers(principal, &rows).map_err(|e| {
            tracing::error!("editor registers: write for {} failed: {e}", principal.short());
            format!("vi registers not saved: {e}")
        })?;
        for write in writes {
            player.registers.apply(write).expect("the copy took this write");
        }
        player.next_seq += writes.len() as u64;
        self.publish_register_writes(principal, origin, first_seq, writes);
        Ok(())
    }

    /// `principal`'s text registers as the kernel holds them.
    pub fn registers_get(&self, principal: PrincipalId) -> Result<Vec<RegisterText>, String> {
        Ok(self.editor_sessions.lock().0.player_registers(principal)?.registers.text_cells())
    }

    /// Write a vi key batch's register writes to the table and publish them
    /// to every connection of `principal`. The batch already applied them to
    /// the store; when the table refuses them, the store goes back to the
    /// registers from before the batch. Returns the status-line text of a
    /// failure, after logging it. Called with the sessions lock held, so no
    /// other write lands between the batch and its table write.
    pub(super) fn commit_batch_registers(
        &self,
        sessions: &mut EditorSessions,
        principal: PrincipalId,
        batch: RegisterBatch,
    ) -> Option<String> {
        if batch.writes.is_empty() {
            return None;
        }
        let player = match sessions.player_registers(principal) {
            Ok(player) => player,
            Err(e) => return Some(e),
        };
        let first_seq = player.next_seq;
        let rows = rows_for(&player.registers, &batch.writes, first_seq);
        if let Err(e) = self.db.lock().write_editor_registers(principal, &rows) {
            tracing::error!("editor registers: write for {} failed: {e}", principal.short());
            player
                .registers
                .replace_text(&batch.before)
                .expect("a snapshot of text registers names only text registers");
            return Some(format!("vi registers not saved, so this batch's yanks and deletes are not in them: {e}"));
        }
        player.next_seq += batch.writes.len() as u64;
        self.publish_register_writes(principal, None, first_seq, &batch.writes);
        None
    }

    fn publish_register_writes(
        &self,
        principal: PrincipalId,
        origin: Option<SessionId>,
        first_seq: u64,
        writes: &[RegisterWrite],
    ) {
        for (i, write) in writes.iter().enumerate() {
            self.editor_flows.publish(EditorFlow::RegisterWrite {
                principal,
                origin,
                seq: first_seq + i as u64,
                write: write.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;
    use kaijutsu_editor::RegisterWriteKind;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    const A: &str = "/mem/a.txt";

    fn w(kind: RegisterWriteKind, text: &str, shape: RegisterShape) -> RegisterWrite {
        RegisterWrite { kind, text: text.to_string(), shape }
    }

    fn line_delete(text: &str) -> RegisterWrite {
        w(RegisterWriteKind::Delete, text, RegisterShape::Linewise)
    }

    async fn kernel_with_file() -> Kernel {
        let kernel = Kernel::new_ephemeral("registers").await;
        kernel.mount("/mem", MemoryBackend::new()).await;
        kernel.vfs().write_all(std::path::Path::new(A), b"alpha\nbeta").await.unwrap();
        kernel
    }

    /// The register writes published since `sub` was taken.
    async fn published(sub: &mut crate::flows::Subscription<EditorFlow>) -> Vec<(PrincipalId, Option<SessionId>, u64, RegisterWrite)> {
        let mut out = Vec::new();
        while let Ok(Some(msg)) = tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await {
            if let EditorFlow::RegisterWrite { principal, origin, seq, write } = msg.payload {
                out.push((principal, origin, seq, write));
            }
        }
        out
    }

    fn deny_register_inserts(kernel: &Kernel) {
        kernel
            .kernel_db()
            .lock()
            .conn_for_test_mut()
            .authorizer(Some(|auth: AuthContext<'_>| match auth.action {
                AuthAction::Insert { table_name } if table_name == "editor_registers" => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .unwrap();
    }

    #[tokio::test]
    async fn a_put_lands_in_the_store_and_the_table_and_is_published_with_its_origin() {
        let kernel = kernel_with_file().await;
        let (amy, origin) = (PrincipalId::new(), SessionId::new());
        let mut sub = kernel.editor_flows().subscribe("editor.*");
        let writes = [
            w(RegisterWriteKind::Yank, "yanked\n", RegisterShape::Linewise),
            line_delete("gone\n"),
        ];
        kernel.registers_put(amy, Some(origin), &writes).unwrap();

        let cells = kernel.registers_get(amy).unwrap();
        assert_eq!(cells[0], RegisterText { name: '"', text: "gone\n".into(), shape: RegisterShape::Linewise });
        assert_eq!(cells.iter().map(|c| c.name).collect::<String>(), "\"01");
        let mut rows = kernel.kernel_db().lock().editor_registers(amy).unwrap();
        rows.retain(|(_, r)| !r.text.is_empty());
        rows.sort_by_key(|(seq, r)| (*seq, r.name));
        assert_eq!(
            rows.iter().map(|(seq, r)| (*seq, r.name, r.text.as_str())).collect::<Vec<_>>(),
            [(1, '0', "yanked\n"), (2, '1', "gone\n")]
        );
        assert_eq!(
            published(&mut sub).await,
            [(amy, Some(origin), 1, writes[0].clone()), (amy, Some(origin), 2, writes[1].clone())]
        );
        assert!(kernel.registers_get(PrincipalId::new()).unwrap().is_empty(), "another player has none");
    }

    #[tokio::test]
    async fn a_refused_write_refuses_the_whole_put() {
        let kernel = kernel_with_file().await;
        let amy = PrincipalId::new();
        let mut sub = kernel.editor_flows().subscribe("editor.*");
        let bad = [line_delete("ok\n"), w(RegisterWriteKind::Named('Q'), "x", RegisterShape::Charwise)];
        assert!(kernel.registers_put(amy, None, &bad).is_err());
        assert!(kernel.registers_get(amy).unwrap().is_empty());
        assert!(kernel.kernel_db().lock().editor_registers(amy).unwrap().is_empty());
        assert!(published(&mut sub).await.is_empty());
    }

    /// The table first: a write the table refuses never reaches the store
    /// and is never published.
    #[tokio::test]
    async fn a_failed_table_write_is_the_error_and_changes_nothing() {
        let kernel = kernel_with_file().await;
        let amy = PrincipalId::new();
        kernel.registers_put(amy, None, &[line_delete("kept\n")]).unwrap();
        let mut sub = kernel.editor_flows().subscribe("editor.*");
        deny_register_inserts(&kernel);
        let err = kernel.registers_put(amy, None, &[line_delete("lost\n")]).unwrap_err();
        assert!(err.contains("not saved"), "{err}");
        assert_eq!(kernel.registers_get(amy).unwrap()[0].text, "kept\n");
        assert!(published(&mut sub).await.is_empty());
    }

    /// A kernel vi session's `dd` goes to the table and is published for the
    /// player who typed it, with no origin.
    #[tokio::test]
    async fn a_vi_delete_is_written_and_published_for_the_typist() {
        let kernel = kernel_with_file().await;
        let amy = PrincipalId::new();
        let (id, _) = kernel.editor_open(A).await.unwrap();
        let mut sub = kernel.editor_flows().subscribe("editor.*");
        kernel.editor_keys(id, "dd", amy).await.unwrap();

        assert_eq!(published(&mut sub).await, [(amy, None, 1, line_delete("alpha\n"))]);
        let rows = kernel.kernel_db().lock().editor_registers(amy).unwrap();
        assert!(rows.contains(&(1, RegisterText { name: '1', text: "alpha\n".into(), shape: RegisterShape::Linewise })));
        assert_eq!(kernel.registers_get(amy).unwrap()[0].text, "alpha\n");
    }

    /// A client's put and a vi session share one store: `p` in the session
    /// puts what the client yanked.
    #[tokio::test]
    async fn a_vi_session_puts_what_a_client_put() {
        let kernel = kernel_with_file().await;
        let amy = PrincipalId::new();
        kernel.registers_put(amy, Some(SessionId::new()), &[line_delete("from compose\n")]).unwrap();
        let (id, _) = kernel.editor_open(A).await.unwrap();
        let st = kernel.editor_keys(id, "p", amy).await.unwrap();
        assert_eq!(st.text, "alpha\nfrom compose\nbeta");
        kernel.editor_keys(id, "yy", amy).await.unwrap();
        let rows = kernel.kernel_db().lock().editor_registers(amy).unwrap();
        assert!(rows.iter().any(|(seq, r)| *seq == 2 && r.name == '0'), "the vi yank takes the next seq: {rows:?}");
    }

    /// The table refusing a vi batch's writes puts the store back as it was
    /// before the batch; the edit stands and the status line says why.
    #[tokio::test]
    async fn a_failed_table_write_for_a_vi_batch_restores_the_registers() {
        let kernel = kernel_with_file().await;
        let amy = PrincipalId::new();
        let (id, _) = kernel.editor_open(A).await.unwrap();
        kernel.editor_keys(id, "yy", amy).await.unwrap();
        let mut sub = kernel.editor_flows().subscribe("editor.*");
        deny_register_inserts(&kernel);
        let st = kernel.editor_keys(id, "jdd", amy).await.unwrap();
        assert_eq!(st.text, "alpha", "the delete itself stands");
        assert!(st.message.as_deref().is_some_and(|m| m.contains("not saved")), "{:?}", st.message);
        let cells = kernel.registers_get(amy).unwrap();
        assert_eq!(cells.iter().map(|c| (c.name, c.text.as_str())).collect::<Vec<_>>(), [('"', "alpha\n"), ('0', "alpha\n")]);
        assert!(published(&mut sub).await.is_empty());
    }

    /// Durability: a write survives a restart, and `p` in a vi session on
    /// the new kernel puts it.
    #[tokio::test]
    async fn a_write_survives_a_restart() {
        let old = kernel_with_file().await;
        let amy = PrincipalId::new();
        old.registers_put(amy, None, &[w(RegisterWriteKind::Yank, "yanked\n", RegisterShape::Linewise)]).unwrap();
        let (id, _) = old.editor_open(A).await.unwrap();
        old.editor_keys(id, "dd", amy).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let new = Kernel::with_flows_and_threads(
            kaijutsu_types::KernelId::new(),
            "restarted",
            old.block_flows().clone(),
            dir.path(),
            old.blocks().clone(),
            old.kernel_db().clone(),
            1,
        )
        .await;
        new.mount("/mem", MemoryBackend::new()).await;
        new.vfs().write_all(std::path::Path::new("/mem/b.txt"), b"one").await.unwrap();
        let (id, _) = new.editor_open("/mem/b.txt").await.unwrap();
        let st = new.editor_keys(id, "p\"0p", amy).await.unwrap();
        assert_eq!(st.text, "one\nalpha\nyanked", "the unnamed register is the delete, \"0 the yank");
        new.editor_keys(id, "yy", amy).await.unwrap();
        let rows = new.kernel_db().lock().editor_registers(amy).unwrap();
        assert!(rows.iter().any(|(seq, r)| *seq == 3 && r.name == '0'), "seq carries on after the restart: {rows:?}");
    }
}
