//! Migration `context-fold-v1`: the `context_characters` table folds into
//! `contexts` (`performer`, `senpai_ctx`, `persona`), and every live context
//! gets a senpai (`docs/senpai-kohai.md`, "Migration").

use super::*;

/// `context_slots` as it is after the fold: keyed by a context.
const CONTEXT_SLOTS_FOLDED: &str = "
CREATE TABLE context_slots_fold (
    context_id      BLOB    NOT NULL REFERENCES contexts(context_id),
    role            TEXT    NOT NULL,
    backend_id      BLOB    NOT NULL REFERENCES backends(backend_id),
    model           TEXT    NOT NULL CHECK (model != ''),
    max_tokens      INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    context_window  INTEGER CHECK (context_window IS NULL OR context_window > 0),
    temperature     REAL    CHECK (temperature IS NULL OR (temperature >= 0.0 AND temperature <= 2.0)),
    top_p           REAL    CHECK (top_p IS NULL OR (top_p > 0.0 AND top_p <= 1.0)),
    effort          TEXT,
    thinking_budget INTEGER CHECK (thinking_budget IS NULL OR thinking_budget > 0),
    thinking_style  TEXT,
    PRIMARY KEY (context_id, role)
);
INSERT INTO context_slots_fold
    (context_id, role, backend_id, model, max_tokens, context_window, temperature, top_p, effort,
     thinking_budget, thinking_style)
SELECT context_id, role, backend_id, model, max_tokens, context_window, temperature, top_p, effort,
       thinking_budget, thinking_style FROM context_slots;
DROP TABLE context_slots;
ALTER TABLE context_slots_fold RENAME TO context_slots;
";

/// A context as the fold reads it.
struct FoldCtx {
    id: ContextId,
    label: Option<String>,
    forked_from: Option<ContextId>,
    archived: bool,
    senpai: Option<ContextId>,
}

impl FoldCtx {
    fn name(&self) -> String {
        self.label.as_ref().map_or_else(|| self.id.short(), |l| format!("{l} ({})", self.id.short()))
    }
}

impl KernelDb {
    pub(super) const CONTEXT_FOLD_MIGRATION: &'static str = "context-fold-v1";

    /// Where a database that still has its `context_characters` table is
    /// copied before `context-fold-v1` drops it: `kernel.db.pre-context-fold`.
    pub(super) fn pre_context_fold_backup_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".pre-context-fold");
        path.with_file_name(name)
    }

    /// Copy a database that `context-fold-v1` is about to change aside first.
    /// A database needs the fold when it has a `context_characters` table, or
    /// is old enough that `context-characters-v1` will make one in this open
    /// (`contexts.provider`). The copy is a consistent snapshot; an existing
    /// copy stays as it is, and a copy that cannot be made fails the open.
    pub(super) fn back_up_before_context_fold(conn: &Connection, path: &Path) -> KernelDbResult<()> {
        let needs_fold = Self::has_table(conn, "context_characters")?
            || (Self::has_table(conn, "contexts")? && Self::has_column(conn, "contexts", "provider")?);
        if !needs_fold {
            return Ok(());
        }
        Self::back_up_aside(
            conn,
            &Self::pre_context_fold_backup_path(path),
            "the context characters fold into contexts",
        )
    }

    /// Fold `context_characters` into `contexts`, once, in one transaction.
    ///
    /// Copies each row's principal, senpai and persona into the context's
    /// `performer`, `senpai_ctx` and `persona`. Every live context that has no
    /// senpai and is not a root console gets one: the only live root's console
    /// when the kernel has exactly one live root, else the console of the live
    /// root at the top of the senpai chain above its nearest `forked_from`
    /// ancestor, for an archived context as for a live one. A live context
    /// with neither refuses the whole migration, naming it. An archived one
    /// with neither keeps a NULL senpai and never refuses: it answers no asks,
    /// the database trigger only fires on an update to NULL, and each is
    /// listed in `context_character_migration_report` as `archived_no_senpai`.
    /// Each assignment is listed as `senpai_assigned`. Then
    /// `context_slots` is rebuilt to reference `contexts` and
    /// `context_characters` is dropped. The presence of the table is the whole
    /// state of the migration.
    pub(super) fn migrate_context_fold(conn: &Connection) -> KernelDbResult<()> {
        if !Self::has_table(conn, "context_characters")? {
            return Ok(());
        }
        if Self::has_column(conn, "contexts", "played_by")? {
            return Err(KernelDbError::Validation(
                "the context characters cannot fold into contexts before context-characters-v1 has mapped them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        for (column, definition) in [
            ("performer", "BLOB"),
            ("senpai_ctx", "BLOB REFERENCES contexts(context_id)"),
            ("persona", "TEXT"),
        ] {
            if !Self::has_column(&tx, "contexts", column)? {
                tx.execute(&format!("ALTER TABLE contexts ADD COLUMN {column} {definition}"), [])?;
            }
        }
        // The guards are recreated once the fold has written (`ensure_context_guards`).
        tx.execute_batch(
            "DROP TRIGGER IF EXISTS contexts_performer_fixed; DROP TRIGGER IF EXISTS contexts_senpai_required;",
        )?;
        // A context with a row takes its values. One with none keeps the
        // `senpai_ctx` an older migration left, which is the same column.
        let folded = tx.execute(
            "UPDATE contexts SET
                 performer  = (SELECT cc.principal_id FROM context_characters cc WHERE cc.context_id = contexts.context_id),
                 senpai_ctx = (SELECT cc.senpai_ctx    FROM context_characters cc WHERE cc.context_id = contexts.context_id),
                 persona    = (SELECT cc.persona       FROM context_characters cc WHERE cc.context_id = contexts.context_id)
             WHERE context_id IN (SELECT context_id FROM context_characters)",
            [],
        )?;

        let contexts: Vec<FoldCtx> = {
            let mut stmt =
                tx.prepare("SELECT context_id, label, forked_from, archived_at, senpai_ctx FROM contexts ORDER BY created_at, context_id")?;
            stmt.query_map([], |r| {
                Ok(FoldCtx {
                    id: read_context_id(r, 0)?,
                    label: r.get(1)?,
                    forked_from: read_opt_context_id(r, 2)?,
                    archived: r.get::<_, Option<i64>>(3)?.is_some(),
                    senpai: read_opt_context_id(r, 4)?,
                })
            })?
            .collect::<SqliteResult<_>>()?
        };
        let by_id: HashMap<ContextId, &FoldCtx> = contexts.iter().map(|c| (c.id, c)).collect();
        // (root_ctx, root, retired) of every character that has a console.
        let consoles: Vec<(ContextId, bool, bool)> = {
            let mut stmt = tx.prepare("SELECT root_ctx, root, retired_at IS NOT NULL FROM characters WHERE root_ctx IS NOT NULL")?;
            stmt.query_map([], |r| Ok((read_context_id(r, 0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?)))?
                .collect::<SqliteResult<_>>()?
        };
        let console_ids: HashSet<ContextId> = consoles.iter().map(|(ctx, _, _)| *ctx).collect();
        let mut live_roots: Vec<ContextId> =
            consoles.iter().filter(|(_, root, retired)| *root && !*retired).map(|(ctx, _, _)| *ctx).collect();
        live_roots.sort();
        let name_of = |id: ContextId| by_id.get(&id).map_or_else(|| id.short(), |c| c.name());

        // The live root console at the top of the senpai chain above `from`'s
        // nearest `forked_from` ancestor that reaches one.
        let lineage_console = |from: &FoldCtx| -> Option<ContextId> {
            let mut ancestor = from.forked_from;
            let mut seen = HashSet::new();
            while let Some(id) = ancestor {
                if !seen.insert(id) {
                    return None;
                }
                let mut top = id;
                let mut chain = HashSet::new();
                while let Some(above) = by_id.get(&top).and_then(|c| c.senpai) {
                    if !chain.insert(top) {
                        break;
                    }
                    top = above;
                }
                if live_roots.contains(&top) {
                    return Some(top);
                }
                ancestor = by_id.get(&id).and_then(|c| c.forked_from);
            }
            None
        };

        let mut assigned: Vec<(ContextId, ContextId, &'static str)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        let mut archived_bare: Vec<ContextId> = Vec::new();
        for c in contexts.iter().filter(|c| c.senpai.is_none() && !console_ids.contains(&c.id)) {
            if let [only] = live_roots.as_slice() {
                assigned.push((c.id, *only, "the console of the only live root"));
            } else if let Some(console) = lineage_console(c) {
                assigned.push((c.id, console, "the console of the live root at the top of its forked_from lineage"));
            } else if c.archived {
                archived_bare.push(c.id);
            } else {
                refused.push(format!(
                    "{}: it has no senpai, the kernel has {} live roots, and no forked_from ancestor reaches a live root console",
                    c.name(),
                    live_roots.len()
                ));
            }
        }
        if !refused.is_empty() {
            refused.sort();
            return Err(KernelDbError::Validation(format!(
                "the context fold cannot give {} live context{} a senpai and will not guess; nothing was changed. \
                 Give each a forked_from lineage that reaches a root console with the previous kernel, then start again:\n  {}",
                refused.len(),
                if refused.len() == 1 { "" } else { "s" },
                refused.join("\n  ")
            )));
        }
        let recorded_at = now_millis();
        let mut notes: Vec<(&'static str, ContextId, String)> = Vec::new();
        for (ctx, console, rule) in &assigned {
            tx.execute(
                "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
                params![blob_param(console.as_bytes()), blob_param(ctx.as_bytes())],
            )?;
            notes.push((
                "senpai_assigned",
                *ctx,
                format!("had no senpai; set to {}, {rule}", name_of(*console)),
            ));
        }
        for ctx in &archived_bare {
            notes.push((
                "archived_no_senpai",
                *ctx,
                "archived with no senpai; it answers no asks, and promoting it needs a senpai first (`kj context set <ctx> --senpai <ctx>`)".to_string(),
            ));
        }
        for (kind, ctx, note) in &notes {
            tx.execute(
                "INSERT INTO context_character_migration_report (kind, context_id, note, recorded_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![kind, blob_param(ctx.as_bytes()), note, recorded_at],
            )?;
        }

        let slots_reference_old_table: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('context_slots') WHERE \"table\" = 'context_characters')",
            [],
            |r| r.get(0),
        )?;
        if slots_reference_old_table {
            tx.execute_batch(CONTEXT_SLOTS_FOLDED)?;
        }
        tx.execute("DROP TABLE context_characters", [])?;
        {
            let mut stmt = tx.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                return Err(KernelDbError::Validation(format!(
                    "the context fold left a dangling foreign key on {table}; refusing to commit it, nothing was changed"
                )));
            }
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::CONTEXT_FOLD_MIGRATION])?;
        tx.commit()?;
        info!(
            folded,
            senpai_assigned = assigned.len(),
            archived_without_senpai = archived_bare.len(),
            "folded the context characters into contexts"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
