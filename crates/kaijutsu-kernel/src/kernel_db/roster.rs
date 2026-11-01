//! Roster storage: the storage half of crate::roster.

use super::*;

/// Reconstruct a [`RosterEntity`] from its stored `(entity_kind, entity_id)`
/// columns at `kind_idx`/`id_idx`. Fails loud (a typed SQLite conversion
/// error, not a silently-dropped row) on an unrecognized kind or malformed
/// id bytes — the CHECK constraint on `entity_kind` means the first should
/// be unreachable in practice, but a read path never trusts that from the
/// Rust side alone (no-silent-fallbacks).
pub(super) fn read_roster_entity(
    row: &rusqlite::Row<'_>,
    kind_idx: usize,
    id_idx: usize,
) -> SqliteResult<RosterEntity> {
    let kind: String = row.get(kind_idx)?;
    let id: Vec<u8> = row.get(id_idx)?;
    RosterEntity::from_parts(&kind, &id).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            id_idx,
            rusqlite::types::Type::Blob,
            format!("invalid roster entity: kind={kind:?}").into(),
        )
    })
}

/// Nullable liveness_kind — `None` when the LEFT JOIN found no
/// `roster_presence` row at all (a status-only entity). A NON-NULL value
/// that fails to parse is still a loud error: the CHECK constraint means
/// that should be unreachable, and a read path never silently swallows a
/// value that shouldn't exist (no-silent-fallbacks).
pub(super) fn read_opt_liveness_kind(row: &rusqlite::Row<'_>, idx: usize) -> SqliteResult<Option<LivenessKind>> {
    let s: Option<String> = row.get(idx)?;
    s.map(|s| {
        LivenessKind::parse(&s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                idx,
                rusqlite::types::Type::Text,
                format!("invalid liveness_kind: {s:?}").into(),
            )
        })
    })
    .transpose()
}

pub(super) fn row_to_roster_row(row: &rusqlite::Row<'_>) -> SqliteResult<RosterRow> {
    let entity = read_roster_entity(row, 0, 1)?;
    let liveness_kind = read_opt_liveness_kind(row, 5)?;
    let availability: Option<String> = row.get(14)?;
    let availability = availability
        .map(|s| {
            Availability::parse(&s).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    format!("invalid availability: {s:?}").into(),
                )
            })
        })
        .transpose()?;
    let live: Option<i64> = row.get(10)?;
    Ok(RosterRow {
        entity,
        label: row.get(2)?,
        entity_first_seen_at: row.get(17)?,
        source: row.get(3)?,
        source_local_id: row.get(4)?,
        liveness_kind,
        host: row.get(6)?,
        pid: row.get(7)?,
        proc_start: row.get(8)?,
        presence_first_seen_at: row.get(9)?,
        live: live.map(|v| v != 0),
        observed_at: row.get(11)?,
        recorded_at: row.get(12)?,
        status_text: row.get(13)?,
        availability,
        status_observed_at: row.get(15)?,
        status_recorded_at: row.get(16)?,
    })
}

impl KernelDb {
    /// Reconcile one source's full current presence picture — see
    /// `RosterStore::reconcile`'s doc comment for the shared-primitive
    /// rationale. Upserts every row given (entity, presence, activity) and
    /// prunes every existing `roster_presence` row under `source` this call
    /// did not reconfirm, cascading to `roster_activity`. Transactional: a
    /// failure partway (e.g. the joinability trigger firing on a phantom
    /// context) leaves the prior picture intact rather than half-applied.
    pub fn roster_reconcile_presence(
        &mut self,
        source: &str,
        liveness_kind: LivenessKind,
        rows: &[PresenceSnapshotRow],
        recorded_at: i64,
    ) -> KernelDbResult<Vec<String>> {
        let tx = self.conn.transaction()?;
        for row in rows {
            let id_bytes = row.entity.id_bytes();
            tx.execute(
                "INSERT INTO roster_entity (entity_kind, entity_id, label, first_seen_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (entity_kind, entity_id) DO UPDATE SET
                    label = COALESCE(excluded.label, roster_entity.label)",
                params![
                    row.entity.kind_str(),
                    blob_param(&id_bytes),
                    row.entity_label,
                    recorded_at
                ],
            )?;
            tx.execute(
                "INSERT INTO roster_presence
                    (source, source_local_id, entity_kind, entity_id, liveness_kind,
                     host, pid, proc_start, first_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (source, source_local_id) DO UPDATE SET
                    entity_kind   = excluded.entity_kind,
                    entity_id     = excluded.entity_id,
                    liveness_kind = excluded.liveness_kind,
                    host          = excluded.host,
                    pid           = excluded.pid,
                    proc_start    = excluded.proc_start",
                params![
                    source,
                    row.source_local_id,
                    row.entity.kind_str(),
                    blob_param(&id_bytes),
                    liveness_kind.as_str(),
                    row.host,
                    row.pid,
                    row.proc_start,
                    recorded_at
                ],
            )?;
            tx.execute(
                "INSERT INTO roster_activity (source, source_local_id, live, observed_at, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (source, source_local_id) DO UPDATE SET
                    live        = excluded.live,
                    observed_at = excluded.observed_at,
                    recorded_at = excluded.recorded_at",
                params![source, row.source_local_id, row.live as i64, row.observed_at, recorded_at],
            )?;
        }

        // Prune: anything this source previously stood behind that this
        // call did not reconfirm is gone. `midi_presence.rs`'s
        // `reap_connection` doc explains why removal (not a lingering
        // `live=0`) is the honest shape for a source whose absence means
        // "no longer true" — cascades into roster_activity via FK.
        let keep: HashSet<&str> = rows.iter().map(|r| r.source_local_id.as_str()).collect();
        let existing: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT source_local_id FROM roster_presence WHERE source = ?1")?;
            stmt.query_map(params![source], |r| r.get(0))?
                .collect::<SqliteResult<_>>()?
        };
        let removed: Vec<String> =
            existing.into_iter().filter(|id| !keep.contains(id.as_str())).collect();
        for id in &removed {
            tx.execute(
                "DELETE FROM roster_presence WHERE source = ?1 AND source_local_id = ?2",
                params![source, id],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Write (or update in place) one entity's self-reported status. Callers
    /// MUST construct `entity` from a connection-stamped identity — this
    /// method trusts it completely (module doc on `RosterEntity`:
    /// joinability is the caller's responsibility, backed for `context`
    /// entities by the `roster_entity_context_must_exist` trigger).
    pub fn roster_write_status(
        &self,
        entity: RosterEntity,
        entity_label: Option<&str>,
        status_text: Option<&str>,
        availability: Availability,
        observed_at: Option<i64>,
        recorded_at: i64,
    ) -> KernelDbResult<()> {
        let id_bytes = entity.id_bytes();
        self.conn.execute(
            "INSERT INTO roster_entity (entity_kind, entity_id, label, first_seen_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (entity_kind, entity_id) DO UPDATE SET
                label = COALESCE(excluded.label, roster_entity.label)",
            params![entity.kind_str(), blob_param(&id_bytes), entity_label, recorded_at],
        )?;
        self.conn.execute(
            "INSERT INTO roster_status
                (entity_kind, entity_id, status_text, availability, observed_at, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (entity_kind, entity_id) DO UPDATE SET
                status_text  = excluded.status_text,
                availability = excluded.availability,
                observed_at  = excluded.observed_at,
                recorded_at  = excluded.recorded_at",
            params![
                entity.kind_str(),
                blob_param(&id_bytes),
                status_text,
                availability.as_str(),
                observed_at.unwrap_or(recorded_at),
                recorded_at
            ],
        )?;
        Ok(())
    }

    /// The full current roster view: one row per live presence, joined
    /// against its entity and (if any) self-reported status. The one SELECT
    /// a read surface (later slices) runs — no fan-out to `PeerRegistry` or
    /// anywhere else.
    ///
    /// Rooted at `roster_entity` with LEFT JOINs to presence/activity/status
    /// — NOT rooted at `roster_presence` — so an entity that has posted a
    /// status but has no known liveness source at all still surfaces (with
    /// `source`/`liveness_kind`/`live`/… all `None`). Same "unknown, never
    /// absent" stance as everywhere else in this module: no presence row
    /// means liveness is unknown, not that the entity should be invisible.
    /// An entity WITH multiple presence rows (not expected in v1 — `bound`
    /// is principal-only, `recent` is context-only, so today's two sources
    /// never overlap on one entity — but the schema doesn't forbid it once
    /// `attested` lands) yields one result row per presence, each carrying
    /// the same entity/status columns.
    ///
    /// `WHERE p.source IS NOT NULL OR s.entity_id IS NOT NULL` excludes the
    /// other direction of orphan: `roster_entity` rows are never deleted (no
    /// history-table/GC machinery — module doc), so a `bound` entity whose
    /// sole presence row was just pruned by reconcile, and who has never
    /// posted a status, would otherwise linger here as a ghost with every
    /// column `NULL`. It has nothing current to say about itself any more,
    /// so it drops out of the view entirely rather than surfacing as a
    /// phantom "known but empty" row — this is what keeps the restart-shaped
    /// guarantee (a pruned `bound` row must not read back live, or at all)
    /// true even after the LEFT JOIN widening above.
    pub fn roster_snapshot(&self) -> KernelDbResult<Vec<RosterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.entity_kind, e.entity_id, e.label,
                    p.source, p.source_local_id, p.liveness_kind,
                    p.host, p.pid, p.proc_start, p.first_seen_at,
                    a.live, a.observed_at, a.recorded_at,
                    s.status_text, s.availability, s.observed_at, s.recorded_at,
                    e.first_seen_at
             FROM roster_entity e
             LEFT JOIN roster_presence p ON p.entity_kind = e.entity_kind AND p.entity_id = e.entity_id
             LEFT JOIN roster_activity a ON a.source = p.source AND a.source_local_id = p.source_local_id
             LEFT JOIN roster_status s ON s.entity_kind = e.entity_kind AND s.entity_id = e.entity_id
             WHERE p.source IS NOT NULL OR s.entity_id IS NOT NULL
             ORDER BY e.entity_kind, e.entity_id, p.source, p.source_local_id",
        )?;
        let rows = stmt.query_map([], row_to_roster_row)?.collect::<SqliteResult<Vec<_>>>()?;
        Ok(rows)
    }

    /// The current availability on file for one entity's self-reported
    /// status, or `None` if it has never posted one. `kj roster status`
    /// reads this when the caller omits `--availability`, so a text-only
    /// update never silently resets a standing `dnd`/`away` back to a
    /// default.
    pub fn roster_get_availability(&self, entity: RosterEntity) -> KernelDbResult<Option<Availability>> {
        let id_bytes = entity.id_bytes();
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT availability FROM roster_status WHERE entity_kind = ?1 AND entity_id = ?2",
                params![entity.kind_str(), blob_param(&id_bytes)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match s {
            None => None,
            Some(s) => Some(Availability::parse(&s).ok_or_else(|| {
                KernelDbError::Validation(format!("roster_status holds an invalid availability: {s:?}"))
            })?),
        })
    }

    /// Every `source_local_id` currently on file for `source`, before a
    /// refresh pass. A source's refresh loop (`roster_sources.rs`) reads
    /// this first to tell "appeared" (in the new snapshot, not in this set)
    /// from "already known" — `roster_reconcile_presence` itself only
    /// reports what it PRUNED, not what's new, since new-vs-updated isn't a
    /// distinction the upsert needs to make.
    pub fn roster_presence_source_local_ids(&self, source: &str) -> KernelDbResult<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_local_id FROM roster_presence WHERE source = ?1")?;
        let ids = stmt
            .query_map(params![source], |r| r.get(0))?
            .collect::<SqliteResult<HashSet<String>>>()?;
        Ok(ids)
    }
}

