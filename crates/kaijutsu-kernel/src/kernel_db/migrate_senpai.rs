//! Senpai edge migration, pre-character-slot columns, and structural edge drops.

use super::*;

/// The columns of a pre-instance `contexts` row that `senpai-edges-v1` reads.
pub(super) struct OldContext {
    context_id: ContextId,
    label: Option<String>,
    created_at: i64,
    forked_from: Option<ContextId>,
    archived_at: Option<i64>,
    played_by: Option<PrincipalId>,
}

/// Columns a database written before context characters may lack, added so
/// `senpai-edges-v1` and `context-characters-v1` can read them; the drop that
/// follows `context-characters-v1` removes them (`default_cast_id` is renamed
/// to `cast_id`). Each is `(table, column, definition)`.
pub(super) const PRE_CONTEXT_CHARACTER_COLUMNS: [(&str, &str, &str); 4] = [
    ("contexts", "cast_id", "BLOB REFERENCES casts(cast_id) ON DELETE SET NULL"),
    ("contexts", "played_by", "BLOB"),
    ("contexts", "senpai_ctx", "BLOB REFERENCES contexts(context_id)"),
    ("characters", "default_cast_id", "BLOB REFERENCES casts(cast_id) ON DELETE SET NULL"),
];

impl KernelDb {
    /// Add the pre-context-character columns a very old database lacks, so
    /// the two migrations that read them find them. Only called on a database
    /// that still has `contexts.provider`.
    pub(super) fn ensure_pre_context_character_columns(conn: &Connection) -> KernelDbResult<()> {
        for (table, column, definition) in PRE_CONTEXT_CHARACTER_COLUMNS {
            if !Self::has_column(conn, table, column)? {
                conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"), [])?;
            }
        }
        Ok(())
    }

    pub(super) const SENPAI_MIGRATION: &'static str = "senpai-edges-v1";

    /// Set `senpai_ctx` for every context from what routed it before the
    /// senpai edge (`docs/senpai-kohai.md`, "Migration"), once. The old
    /// columns and tables are read where they still exist;
    /// [`Self::drop_old_routing`] removes them after this has run.
    ///
    /// For a context C with a parent on `forked_from`, run once as the old
    /// resolver would have, acting as C's performer:
    ///
    /// - With an explicit reviewer override, or else the director's active
    ///   delegation, the reviewer R is that character. The senpai is the
    ///   nearest strict ancestor of C whose holder is R. With none, it is R's
    ///   root console when R is a root, else R's most recent live context
    ///   other than C; either is reported.
    /// - Otherwise the old walk: the nearest strict ancestor whose holder
    ///   (performer, else director) is not C's performer. A retired holder
    ///   there is mapped to that ancestor too, so the new resolver refuses by
    ///   name exactly as the old one did.
    /// - A context with no parent and no override is a top.
    ///
    /// A live context that fits none of these refuses the whole migration,
    /// naming each one; nothing is written. An archived one is left a top and
    /// reported instead, since it answers no asks.
    pub(super) fn migrate_senpai_edges(conn: &Connection) -> KernelDbResult<()> {
        use std::collections::{BTreeMap, HashMap};
        let done: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SENPAI_MIGRATION], |row| row.get(0),
        )?;
        if done {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;

        let contexts: Vec<OldContext> = {
            let mut stmt = tx.prepare(
                "SELECT context_id, label, created_at, forked_from, archived_at, played_by
                 FROM contexts ORDER BY created_at, context_id",
            )?;
            stmt.query_map([], |r| Ok(OldContext {
                context_id: read_context_id(r, 0)?,
                label: r.get(1)?,
                created_at: r.get(2)?,
                forked_from: read_opt_context_id(r, 3)?,
                archived_at: r.get(4)?,
                played_by: read_opt_principal_id(r, 5)?,
            }))?.collect::<SqliteResult<_>>()?
        };
        let characters: HashMap<PrincipalId, CharacterRow> = {
            let mut stmt = tx.prepare(
                "SELECT principal_id, name, created_at, retired_at, handoff_ctx, root, root_ctx, NULL, NULL, NULL, NULL FROM characters",
            )?;
            stmt.query_map([], row_to_character_row)?
                .map(|r| r.map(|c| (c.principal_id, c)))
                .collect::<SqliteResult<_>>()?
        };
        // The old routing, where this database still has it. A table with
        // `reviewer_id` but no `director_id` predates the director column, and
        // the open that added it cleared every override, so none survive.
        let has_reviewer = Self::has_column(&tx, "contexts", "reviewer_id")?;
        let has_director = Self::has_column(&tx, "contexts", "director_id")?;
        let old_routing: HashMap<ContextId, (Option<PrincipalId>, Option<PrincipalId>)> = if has_director {
            let reviewer = if has_reviewer { "reviewer_id" } else { "NULL" };
            let mut stmt = tx.prepare(&format!("SELECT context_id, {reviewer}, director_id FROM contexts"))?;
            stmt.query_map([], |r| Ok((read_context_id(r, 0)?, (read_opt_principal_id(r, 1)?, read_opt_principal_id(r, 2)?))))?
                .collect::<SqliteResult<_>>()?
        } else {
            HashMap::new()
        };
        let delegations: HashMap<PrincipalId, PrincipalId> = if Self::has_table(&tx, "approval_reviewer_delegations")? {
            let mut stmt = tx.prepare(
                "SELECT director_id, reviewer_id FROM approval_reviewer_delegations WHERE revoked_at IS NULL",
            )?;
            stmt.query_map([], |r| Ok((read_principal_id(r, 0)?, read_principal_id(r, 1)?)))?
                .collect::<SqliteResult<_>>()?
        } else {
            HashMap::new()
        };
        let by_id: HashMap<ContextId, &OldContext> = contexts.iter().map(|c| (c.context_id, c)).collect();
        let root_owner: HashMap<ContextId, PrincipalId> = characters.values()
            .filter_map(|c| c.root_ctx.map(|ctx| (ctx, c.principal_id))).collect();
        let holder = |c: &OldContext| c.played_by.or_else(|| root_owner.get(&c.context_id).copied());
        let name_of = |p: PrincipalId| characters.get(&p).map_or_else(|| p.short(), |c| c.name.clone());
        let describe = |c: &OldContext| c.label.as_ref().map_or_else(
            || c.context_id.short(), |l| format!("{l} ({})", c.context_id.short()),
        );

        enum Plan {
            Top,
            Senpai { senpai: ContextId, note: Option<String>, old_reviewer: Option<PrincipalId> },
            Unmapped(String),
        }

        // The strict ancestors of `c` on `forked_from`, nearest first.
        let ancestors = |c: &OldContext| -> Result<Vec<&OldContext>, String> {
            let mut chain = Vec::new();
            let mut seen = HashSet::from([c.context_id]);
            let mut next = c.forked_from;
            while let Some(id) = next {
                if !seen.insert(id) {
                    return Err(format!("its forked_from chain has a cycle at {}", id.short()));
                }
                let Some(parent) = by_id.get(&id) else {
                    return Err(format!("its forked_from names context {}, which has no row", id.short()));
                };
                chain.push(*parent);
                next = parent.forked_from;
            }
            Ok(chain)
        };

        let plan = |c: &OldContext| -> Plan {
            let (reviewer_id, director_id) = old_routing.get(&c.context_id).copied().unwrap_or((None, None));
            let override_reviewer = reviewer_id.map(|r| (r, "explicit reviewer override".to_string())).or_else(|| {
                let director = director_id?;
                delegations.get(&director).map(|r| (*r, format!("delegation from director {}", name_of(director))))
            });
            if c.forked_from.is_none() && override_reviewer.is_none() {
                return Plan::Top;
            }
            let chain = match ancestors(c) {
                Ok(chain) => chain,
                Err(why) => return Plan::Unmapped(why),
            };
            if let Some((reviewer, source)) = override_reviewer {
                match characters.get(&reviewer) {
                    None => return Plan::Unmapped(format!("its {source} names a character with no sheet")),
                    Some(r) if r.retired_at.is_some() => return Plan::Unmapped(format!(
                        "its {source} names {}, who is retired", r.name
                    )),
                    Some(_) => {}
                }
                if let Some(ancestor) = chain.iter().find(|a| holder(a) == Some(reviewer)) {
                    return Plan::Senpai { senpai: ancestor.context_id, note: None, old_reviewer: Some(reviewer) };
                }
                let who = name_of(reviewer);
                let sheet = &characters[&reviewer];
                let console = sheet.root.then_some(sheet.root_ctx).flatten()
                    .and_then(|ctx| by_id.get(&ctx))
                    .filter(|r| r.archived_at.is_none() && r.context_id != c.context_id);
                if let Some(console) = console {
                    return Plan::Senpai {
                        senpai: console.context_id,
                        note: Some(format!(
                            "its {source} named {who}, who holds nothing above it: mapped to {who}'s root console; \
                             set its senpai again with `kj context set <ctx> --senpai <ctx>`"
                        )),
                        old_reviewer: Some(reviewer),
                    };
                }
                let recent = contexts.iter()
                    .filter(|o| o.archived_at.is_none() && o.played_by == Some(reviewer) && o.context_id != c.context_id)
                    .max_by_key(|o| (o.created_at, o.context_id));
                return match recent {
                    Some(recent) => Plan::Senpai {
                        senpai: recent.context_id,
                        note: Some(format!(
                            "its {source} named {who}, who holds nothing above it: mapped to {who}'s most recent live context; \
                             set its senpai again with `kj context set <ctx> --senpai <ctx>`"
                        )),
                        old_reviewer: Some(reviewer),
                    },
                    None => Plan::Unmapped(format!(
                        "its {source} names {who}, who holds nothing above it, has no root console and plays no live context"
                    )),
                };
            }
            // The old walk, acting as the performer (or as nobody).
            for ancestor in chain {
                if let Some(h) = holder(ancestor) && Some(h) != c.played_by {
                    return Plan::Senpai { senpai: ancestor.context_id, note: None, old_reviewer: Some(h) };
                }
            }
            Plan::Top
        };

        let mut assigned: BTreeMap<ContextId, Option<ContextId>> = BTreeMap::new();
        let mut report: Vec<(ContextId, Option<ContextId>, String, Option<PrincipalId>)> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for c in &contexts {
            match plan(c) {
                Plan::Top => { assigned.insert(c.context_id, None); }
                Plan::Senpai { senpai, note, old_reviewer } => {
                    assigned.insert(c.context_id, Some(senpai));
                    if let Some(note) = note {
                        report.push((c.context_id, Some(senpai), note, old_reviewer));
                    }
                }
                Plan::Unmapped(why) if c.archived_at.is_some() => {
                    assigned.insert(c.context_id, None);
                    report.push((c.context_id, None, format!(
                        "archived and left without a senpai: {why}"
                    ), None));
                }
                Plan::Unmapped(why) => refused.push(format!("{}: {why}", describe(c))),
            }
        }
        // A fallback can point anywhere; a loop would make routing refuse forever.
        for start in assigned.keys() {
            let mut seen = HashSet::from([*start]);
            let mut at = assigned[start];
            while let Some(next) = at {
                if !seen.insert(next) {
                    refused.push(format!(
                        "{}: its mapped senpai chain loops back through {}",
                        by_id.get(start).map_or_else(|| start.short(), |c| describe(c)), next.short()
                    ));
                    break;
                }
                at = assigned.get(&next).copied().flatten();
            }
        }
        if !refused.is_empty() {
            refused.sort();
            refused.dedup();
            return Err(KernelDbError::Validation(format!(
                "the senpai migration cannot map {} live context{} and will not guess; nothing was changed. \
                 Give each a reachable reviewer, or clear its override, with the previous kernel, then start again:\n  {}",
                refused.len(), if refused.len() == 1 { "" } else { "s" }, refused.join("\n  ")
            )));
        }

        let now = now_millis() as i64;
        for (context, senpai) in &assigned {
            tx.execute(
                "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2",
                params![senpai.as_ref().map(|id| id.as_bytes().to_vec()), blob_param(context.as_bytes())],
            )?;
        }
        for (context, senpai, note, old_reviewer) in &report {
            tx.execute(
                "INSERT OR REPLACE INTO senpai_migration_report (context_id, senpai_ctx, note, old_reviewer, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    blob_param(context.as_bytes()),
                    senpai.as_ref().map(|id| id.as_bytes().to_vec()),
                    note,
                    old_reviewer.as_ref().map(|id| id.as_bytes().to_vec()),
                    now,
                ],
            )?;
        }
        tx.execute("INSERT INTO kernel_migrations (name) VALUES (?1)", [Self::SENPAI_MIGRATION])?;
        tx.commit()?;
        info!(
            mapped = assigned.values().filter(|s| s.is_some()).count(),
            reported = report.len(),
            "mapped every context to its senpai"
        );
        Ok(())
    }

    /// Drop the routing the senpai edge replaced: `contexts.reviewer_id`,
    /// `contexts.director_id`, and the delegation table and its events
    /// (`docs/senpai-kohai.md`, "Removed"). Runs only once
    /// [`Self::migrate_senpai_edges`] has recorded its mapping, which read
    /// them; a mapping that refused leaves them for the next open. What they
    /// said survives as each context's `senpai_ctx`, and, where that was not
    /// the nearest ancestor, as the old reviewer in `senpai_migration_report`.
    /// A fresh database never had them and does nothing.
    pub(super) fn drop_old_routing(conn: &Connection) -> KernelDbResult<()> {
        let mapped: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::SENPAI_MIGRATION], |row| row.get(0),
        )?;
        if !mapped {
            return Err(KernelDbError::Validation(
                "the old routing columns cannot be dropped before the senpai migration has mapped them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        let mut dropped = Vec::new();
        for column in ["reviewer_id", "director_id"] {
            if Self::has_column(&tx, "contexts", column)? {
                tx.execute(&format!("ALTER TABLE contexts DROP COLUMN {column}"), [])?;
                dropped.push(format!("contexts.{column}"));
            }
        }
        for table in ["approval_reviewer_delegation_events", "approval_reviewer_delegations"] {
            if Self::has_table(&tx, table)? {
                tx.execute(&format!("DROP TABLE {table}"), [])?;
                dropped.push(table.to_string());
            }
        }
        tx.commit()?;
        if !dropped.is_empty() {
            info!(dropped = %dropped.join(", "), "dropped the routing the senpai edge replaced");
        }
        Ok(())
    }

    /// Drop what context characters replaced (`docs/senpai-kohai.md`,
    /// "Removed"), in one transaction: `contexts.{played_by,
    /// cast_id, provider, model}`, `presets.{cast_id, system_prompt}` and the
    /// table `llm_defaults`; and rename `characters.default_cast_id` to
    /// `characters.cast_id`. Runs only once
    /// [`Self::migrate_context_characters`] has recorded its mapping, which read them;
    /// a mapping that refused leaves them for the next open.
    ///
    /// No schema this kernel has created puts one of these columns under an
    /// index, trigger, view, `CHECK` or table-level foreign key, so each is a
    /// plain `DROP COLUMN`, and SQLite refuses loudly if a hand-made one is
    /// there; a column-level `REFERENCES` goes with its column. Each step is
    /// guarded by what is still there, so a database already on the new shape
    /// does nothing. The preset values are logged before they go;
    /// `senpai_migration_report` stays, because `kj context info` prints its
    /// notes until the senpai is set again.
    pub(super) fn drop_pre_context_character_columns(conn: &Connection) -> KernelDbResult<()> {
        let mapped: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM kernel_migrations WHERE name = ?1)",
            [Self::CONTEXT_CHARACTERS_MIGRATION], |row| row.get(0),
        )?;
        if !mapped {
            return Err(KernelDbError::Validation(
                "the pre-context-character columns cannot be dropped before the context character migration has mapped them".into(),
            ));
        }
        let tx = conn.unchecked_transaction()?;
        if Self::has_column(&tx, "presets", "cast_id")? || Self::has_column(&tx, "presets", "system_prompt")? {
            let cast = if Self::has_column(&tx, "presets", "cast_id")? { "cast_id IS NOT NULL" } else { "0" };
            let prompt = if Self::has_column(&tx, "presets", "system_prompt")? { "system_prompt IS NOT NULL" } else { "0" };
            let mut stmt = tx.prepare(&format!("SELECT label FROM presets WHERE {cast} OR {prompt} ORDER BY label"))?;
            let labels: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<SqliteResult<_>>()?;
            for label in labels {
                info!(preset = %label, "dropping the cast and system prompt of a preset; a preset keeps only its fork filters");
            }
        }
        let mut dropped = Vec::new();
        for (table, column) in [
            ("contexts", "played_by"),
            ("contexts", "cast_id"),
            ("contexts", "provider"),
            ("contexts", "model"),
            ("presets", "cast_id"),
            ("presets", "system_prompt"),
        ] {
            if Self::has_column(&tx, table, column)? {
                tx.execute(&format!("ALTER TABLE {table} DROP COLUMN {column}"), [])?;
                dropped.push(format!("{table}.{column}"));
            }
        }
        if Self::has_column(&tx, "characters", "default_cast_id")? {
            tx.execute("ALTER TABLE characters RENAME COLUMN default_cast_id TO cast_id", [])?;
            dropped.push("characters.default_cast_id (renamed to cast_id)".to_string());
        }
        if Self::has_table(&tx, "llm_defaults")? {
            tx.execute("DROP TABLE llm_defaults", [])?;
            dropped.push("llm_defaults".to_string());
        }
        tx.commit()?;
        if !dropped.is_empty() {
            info!(dropped = %dropped.join(", "), "dropped what context characters replaced");
        }
        Ok(())
    }

    /// Remove the `kind = 'structural'` rows of `context_edges`, which
    /// duplicated `contexts.forked_from`, and the `kind` column with them.
    ///
    /// The column's presence is the state: a database without it is done.
    /// Before deleting, every structural row is compared with `forked_from`.
    /// A row whose target was not forked from its source, a target with two
    /// structural parents, and a kind that is neither `structural` nor
    /// `drift` each refuse the whole migration, naming the contexts; nothing
    /// is written. The refusal repeats on every open until the rows are
    /// corrected by hand.
    pub(super) fn drop_structural_edges(conn: &Connection) -> KernelDbResult<()> {
        if !Self::has_column(conn, "context_edges", "kind")? {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        let mut refusals = Vec::new();

        let unknown: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT kind FROM context_edges WHERE kind NOT IN ('structural', 'drift')",
            )?;
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<SqliteResult<_>>()?
        };
        for kind in unknown {
            refusals.push(format!("an edge has the unknown kind '{kind}'"));
        }

        let rows: Vec<(ContextId, ContextId, Option<ContextId>)> = {
            let mut stmt = tx.prepare(
                "SELECT e.source_id, e.target_id, c.forked_from
                 FROM context_edges e
                 JOIN contexts c ON c.context_id = e.target_id
                 WHERE e.kind = 'structural'
                 ORDER BY e.target_id, e.source_id",
            )?;
            stmt.query_map([], |r| {
                Ok((read_context_id(r, 0)?, read_context_id(r, 1)?, read_opt_context_id(r, 2)?))
            })?
            .collect::<SqliteResult<_>>()?
        };
        let mut parents: HashMap<ContextId, Vec<ContextId>> = HashMap::new();
        for (source, target, forked_from) in &rows {
            parents.entry(*target).or_default().push(*source);
            if *forked_from != Some(*source) {
                refusals.push(format!(
                    "context {} has structural parent {} but forked_from {}",
                    target.short(),
                    source.short(),
                    forked_from.map_or_else(|| "none".to_string(), |p| p.short()),
                ));
            }
        }
        for (target, sources) in &parents {
            if sources.len() > 1 {
                let names: Vec<String> = sources.iter().map(|s| s.short()).collect();
                refusals.push(format!(
                    "context {} has {} structural parents ({})",
                    target.short(),
                    sources.len(),
                    names.join(", "),
                ));
            }
        }
        if !refusals.is_empty() {
            refusals.sort();
            refusals.dedup();
            return Err(KernelDbError::Validation(format!(
                "cannot drop the structural context_edges rows: {}. Correct forked_from or the edge rows; \
                 forked_from is the only parent link.",
                refusals.join("; "),
            )));
        }

        let deleted = tx.execute("DELETE FROM context_edges WHERE kind = 'structural'", [])?;
        tx.execute_batch(
            "DROP INDEX IF EXISTS idx_edges_structural_unique;
             ALTER TABLE context_edges DROP COLUMN kind;",
        )?;
        tx.commit()?;
        info!(deleted, "dropped the structural context_edges rows; forked_from is the only parent link");
        Ok(())
    }

    /// Contexts whose senpai the migration could not take from the nearest
    /// ancestor: `(context, the senpai it chose, the note)`. `kj context info`
    /// prints the note until the senpai is set again.
    pub fn senpai_migration_report(&self) -> KernelDbResult<Vec<(ContextId, Option<ContextId>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, senpai_ctx, note FROM senpai_migration_report ORDER BY recorded_at, context_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((read_context_id(r, 0)?, read_opt_context_id(r, 1)?, r.get::<_, String>(2)?)))?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests;
