//! Tracks and their attachments.

use super::*;

/// Parse a nullable `context_id` BLOB into `Option<ContextId>`, failing loud on a
/// present-but-malformed blob (crash over corruption — a bad id is never silently None).
pub(super) fn parse_optional_context_id(
    track_id: &str,
    blob: Option<Vec<u8>>,
) -> KernelDbResult<Option<ContextId>> {
    match blob {
        None => Ok(None),
        Some(bytes) => ContextId::try_from_slice(&bytes).map(Some).ok_or_else(|| {
            KernelDbError::Validation(format!(
                "track {track_id} has a malformed score_context_id blob — corrupt"
            ))
        }),
    }
}

/// Build a [`PersistedTrack`] from a raw `tracks` row, validating the numeric
/// invariants BEFORE the `i64 -> u64` cast: a negative field or a zero
/// `period_ms` (a beat that never advances) is corruption, refused loudly rather
/// than wrapped into a nonsense cadence (`0 - 1` would surface as `u64::MAX`).
/// The single construction path both `get_track` and `list_tracks` funnel
/// through, so neither read API can skip the check.
// One argument per raw `tracks` row column, pre-validation.
#[allow(clippy::too_many_arguments)]
pub(super) fn validated_track(
    track_id: String,
    period_ms: i64,
    beats_per_phrase: i64,
    playhead_tick: Option<i64>,
    playing: i64,
    score_blob: Option<Vec<u8>>,
    clock_kind: String,
) -> KernelDbResult<PersistedTrack> {
    if period_ms < 0 || beats_per_phrase < 0 {
        return Err(KernelDbError::Validation(format!(
            "track {track_id} has a negative field (period_ms={period_ms}, \
             beats_per_phrase={beats_per_phrase}) — corrupt"
        )));
    }
    if period_ms == 0 {
        return Err(KernelDbError::Validation(format!(
            "track {track_id} has a zero period_ms — corrupt (a beat that never advances)"
        )));
    }
    let score_context_id = parse_optional_context_id(&track_id, score_blob)?;
    Ok(PersistedTrack {
        track_id,
        period_ms: period_ms as u64,
        beats_per_phrase: beats_per_phrase as u64,
        playhead_tick,
        playing: playing != 0,
        score_context_id,
        clock_kind,
    })
}

/// A persisted track clock domain — the restart-recovery row for a `TrackState`.
/// Primitive-typed so `kernel_db` stays decoupled from `hyoushigi`'s runtime
/// types; the beat/scheduler layer converts at the seam. (`period_ms` ↔
/// `Duration`, `track_id` ↔ `TrackId`, `playing` ↔ `TrackTransport`.)
///
/// Cold-start always re-arms stopped (`playing` restored from this row) so the
/// scheduler never fires stale wakeups on a warm start — the caller calls
/// `play` explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTrack {
    pub track_id: String,
    pub period_ms: u64,
    pub beats_per_phrase: u64,
    /// The track's musical position (playhead tick) as of the last durable update.
    /// `None` until the track first beats; thereafter the max committed tick so
    /// a restart can reseed without scanning the block log.
    pub playhead_tick: Option<i64>,
    /// Transport state: `true` = Playing, `false` = Stopped.
    pub playing: bool,
    /// The non-producer context whose document holds the track's materialized
    /// score. Set once at creation; `write_track` never overwrites it, so an
    /// upsert that omits it (passes `None`) can't wipe it. `None` on a pre-score row.
    pub score_context_id: Option<ContextId>,
    /// The clock-source discriminator (`ClockSourceKind::kind_str`: `"system"` /
    /// `"modeled"`). MUTABLE — `write_track` rewrites it on every upsert (Stage 3:
    /// a track's clock source is circumstantial, slaveable to MIDI), unlike the
    /// set-once `score_context_id`. A pre-Stage-3 row reads back as `"system"`.
    pub clock_kind: String,
}

/// A persisted attachment — one (track_id, context_id) binding, the restart-
/// recovery row for a `TrackState.attached[ctx]` entry. Primitive-typed for the
/// same reason as `PersistedTrack`; the scheduler converts at the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAttachment {
    pub track_id: String,
    pub context_id: ContextId,
    /// Beat divisor: wake this context every N beats on the track clock.
    pub wakeup_every: u64,
    /// Self-fork rotate cadence in phrases; `None` = never auto-rotate.
    pub rotate_every_phrases: Option<u64>,
    /// Whether the OODA loop is armed for this attachment (blocks the next tick
    /// until the context produces output).
    pub ooda_armed: bool,
}

impl KernelDb {
    /// Upsert the durable `PersistedTrack` row. Called by the scheduler on every
    /// policy mutation (attach, tempo, playhead advance) so the row always
    /// reflects the running state and a cold-start can reconstruct it.
    /// In-place (PK is `track_id`): a tempo change or playhead advance updates
    /// one row without inserting a duplicate.
    pub fn upsert_track(&self, t: &PersistedTrack) -> KernelDbResult<()> {
        Self::write_track(&self.conn, t)
    }

    /// Upsert a track row against `conn` (a `&Connection` or `&Transaction` via
    /// deref). Shared by `upsert_track` and transactional callers. Does NOT commit.
    pub(super) fn write_track(conn: &Connection, t: &PersistedTrack) -> KernelDbResult<()> {
        // `score_context_id` is set-once: written on INSERT, omitted from the
        // ON CONFLICT UPDATE, so the frequent tempo/playhead upserts can't null it.
        // `clock_kind` is the opposite — MUTABLE, in the UPDATE SET, so a driver
        // swap (system → modeled) persists (docs/tracks.md Stage 3).
        conn.execute(
            "INSERT INTO tracks
                 (track_id, period_ms, beats_per_phrase, playhead_tick, playing, score_context_id, clock_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(track_id) DO UPDATE SET
                 period_ms = ?2, beats_per_phrase = ?3,
                 playhead_tick = ?4, playing = ?5, clock_kind = ?7",
            params![
                t.track_id,
                t.period_ms as i64,
                t.beats_per_phrase as i64,
                t.playhead_tick,
                t.playing as i64,
                t.score_context_id.map(|c| c.as_bytes().to_vec()),
                t.clock_kind,
            ],
        )?;
        Ok(())
    }

    /// Read the persisted track for `track_id`, or `None` when no such track exists.
    /// A *present but malformed* row — an empty `track_id` or a zero `period_ms`
    /// (a beat that never advances) — is corruption and returns `Err(Validation)`
    /// so the caller refuses to reconstruct a broken clock.
    pub fn get_track(&self, track_id: &str) -> KernelDbResult<Option<PersistedTrack>> {
        if track_id.is_empty() {
            return Err(KernelDbError::Validation(
                "get_track called with an empty track_id — corrupt (no identity)".into(),
            ));
        }
        // Read raw signed integers and validate BEFORE casting. SQLite INTEGER is
        // signed; the upsert only ever writes `u64 as i64` (≥ 0). A negative is
        // corruption; silently wrapping to u64 would produce nonsense cadences.
        let row = self
            .conn
            .query_row(
                "SELECT period_ms, beats_per_phrase, playhead_tick, playing, score_context_id, clock_kind
                 FROM tracks WHERE track_id = ?1",
                params![track_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((period_ms, beats_per_phrase, playhead_tick, playing, score_blob, clock_kind)) = row
        else {
            return Ok(None);
        };
        validated_track(
            track_id.to_string(),
            period_ms,
            beats_per_phrase,
            playhead_tick,
            playing,
            score_blob,
            clock_kind,
        )
        .map(Some)
    }

    /// List all **live** persisted tracks — tombstoned rows (`deleted_at IS NOT
    /// NULL`) are excluded, matching `get_track`'s implicit skip (a tombstoned
    /// row no longer answers to its original name because `tombstone_track`
    /// renamed it aside). Order is not guaranteed.
    pub fn list_tracks(&self) -> KernelDbResult<Vec<PersistedTrack>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, period_ms, beats_per_phrase, playhead_tick, playing, score_context_id, clock_kind
             FROM tracks WHERE deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut tracks = Vec::new();
        for row in rows {
            let (track_id, period_ms, beats_per_phrase, playhead_tick, playing, score_blob, clock_kind) =
                row?;
            // Same validating construction path as `get_track` — a corrupt row
            // (negative field, zero period) is refused here too, not cast into a
            // nonsense `u64` cadence that the roster would then display.
            tracks.push(validated_track(
                track_id,
                period_ms,
                beats_per_phrase,
                playhead_tick,
                playing,
                score_blob,
                clock_kind,
            )?);
        }
        Ok(tracks)
    }

    /// Delete a track row by `track_id`. Cascades to all its `attachments` rows
    /// via the FK `ON DELETE CASCADE`. Returns the row count (0 or 1).
    pub fn delete_track(&self, track_id: &str) -> KernelDbResult<u64> {
        let deleted = self
            .conn
            .execute("DELETE FROM tracks WHERE track_id = ?1", params![track_id])?;
        Ok(deleted as u64)
    }

    /// Tombstone a track: **rename it aside** to `{track_id}~tombstone-{deleted_at}`
    /// and stamp `deleted_at`, atomically, in one transaction. `kj transport
    /// delete`'s DB half — the scheduler half (stop the clock, detach every
    /// attached context, tear down the live `Timeline`, drop the in-memory
    /// `TrackState`) has already run by the time this is called.
    ///
    /// A rename, not a `DELETE`: the row (and the durable `tracks` history it
    /// carries — `score_context_id` above all) survives, just invisible under
    /// its old name. `list_tracks`/`get_track` (by construction — the PK no
    /// longer matches) stop returning it, so name reuse via `kj transport attach
    /// --track <name>` starts a genuinely fresh track + score. Recovery is
    /// **sqlite-only by decision** (no `kj` verb): `UPDATE tracks SET track_id =
    /// '<original>', deleted_at = NULL WHERE track_id =
    /// '<original>~tombstone-<deleted_at>'`.
    ///
    /// `~` is outside the track-id charset (`[a-z0-9_-]`,
    /// `kaijutsu_types::track::is_track_char`) and `TrackId::new` enforces that
    /// charset on every attach-path call (`kj/transport.rs`
    /// `resolve_track_for_ctx` / `beat_attach_payload`), so a tombstone name can
    /// never collide with — or be re-attached under — a live track id.
    ///
    /// PRECONDITION, checked here loudly rather than silently repaired: every
    /// `attachments` row for `track_id` must already be gone. The scheduler's
    /// delete path detaches every attached context AND deletes its persisted
    /// attachment row before calling here (the same "move to a new track"
    /// idiom `attach` already uses for a context switching lanes). A non-zero
    /// remaining count means that step was skipped — refuse rather than rename
    /// the track out from under a still-live binding, which would leave the
    /// attachment row pointing at a `track_id` that resolves to nothing.
    ///
    /// The epoch unit for both the suffix and `deleted_at` is milliseconds —
    /// the same unit `concluded_at`/`last_activity_at` use (this module's
    /// header: "All timestamps are Unix milliseconds") — and it is the SAME
    /// read of the clock for both, so the name and the column can never
    /// disagree by a tick. Returns the tombstone name on success; `NotFound`
    /// if `track_id` doesn't exist (already deleted, or never existed).
    pub fn tombstone_track(&mut self, track_id: &str) -> KernelDbResult<String> {
        if track_id.is_empty() {
            return Err(KernelDbError::Validation(
                "tombstone_track called with an empty track_id".into(),
            ));
        }
        let tx = self.conn.transaction()?;
        let remaining: i64 = tx.query_row(
            "SELECT COUNT(*) FROM attachments WHERE track_id = ?1",
            params![track_id],
            |row| row.get(0),
        )?;
        if remaining != 0 {
            return Err(KernelDbError::Validation(format!(
                "tombstone_track({track_id}): refusing — {remaining} attachment row(s) still \
                 reference this track; detach (and delete the persisted row for) every attached \
                 context before tombstoning, never rename a track out from under a live binding"
            )));
        }
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tracks WHERE track_id = ?1",
            params![track_id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(KernelDbError::NotFound(format!("track {track_id}")));
        }
        let deleted_at = now_millis();
        let tombstone_name = format!("{track_id}~tombstone-{deleted_at}");
        tx.execute(
            "UPDATE tracks SET track_id = ?1, deleted_at = ?2 WHERE track_id = ?3",
            params![tombstone_name, deleted_at, track_id],
        )?;
        tx.commit()?;
        Ok(tombstone_name)
    }

    /// Upsert an attachment row. In-place (PK is `(track_id, context_id)`):
    /// a wakeup change or ooda-arm toggle updates one row.
    pub fn upsert_attachment(&self, a: &PersistedAttachment) -> KernelDbResult<()> {
        Self::write_attachment(&self.conn, a)
    }

    /// Upsert an attachment against `conn` (a `&Connection` or `&Transaction` via
    /// deref). Shared by `upsert_attachment` and transactional callers (the fork
    /// copy). Does NOT commit.
    pub(super) fn write_attachment(conn: &Connection, a: &PersistedAttachment) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO attachments
                 (track_id, context_id, wakeup_every, rotate_every_phrases, ooda_armed)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(track_id, context_id) DO UPDATE SET
                 wakeup_every = ?3, rotate_every_phrases = ?4, ooda_armed = ?5",
            params![
                a.track_id,
                blob_param(a.context_id.as_bytes()),
                a.wakeup_every as i64,
                a.rotate_every_phrases.map(|n| n as i64),
                a.ooda_armed as i64,
            ],
        )?;
        Ok(())
    }

    /// Read one attachment by `(track_id, context_id)`, or `None` when absent.
    pub fn get_attachment(
        &self,
        track_id: &str,
        context_id: ContextId,
    ) -> KernelDbResult<Option<PersistedAttachment>> {
        let row = self
            .conn
            .query_row(
                "SELECT wakeup_every, rotate_every_phrases, ooda_armed
                 FROM attachments WHERE track_id = ?1 AND context_id = ?2",
                params![track_id, blob_param(context_id.as_bytes())],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((wakeup_every, rotate_every_phrases, ooda_armed)) = row else {
            return Ok(None);
        };
        Ok(Some(PersistedAttachment {
            track_id: track_id.to_string(),
            context_id,
            wakeup_every: wakeup_every as u64,
            rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
            ooda_armed: ooda_armed != 0,
        }))
    }

    /// List all attachments for a track (all contexts attached to `track_id`).
    pub fn list_attachments_for_track(
        &self,
        track_id: &str,
    ) -> KernelDbResult<Vec<PersistedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE track_id = ?1",
        )?;
        let rows = stmt.query_map(params![track_id], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ctx_bytes, wakeup_every, rotate_every_phrases, ooda_armed) = row?;
            let context_id = ContextId::try_from_slice(&ctx_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    "invalid ContextId bytes in attachments row".into(),
                )
            })?;
            out.push(PersistedAttachment {
                track_id: track_id.to_string(),
                context_id,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            });
        }
        Ok(out)
    }

    /// List every attachment (all tracks, all contexts) — one query, for
    /// callers building a whole-kernel context→track map (e.g. `listContexts`
    /// populating `trackId` per handle) instead of N per-context lookups.
    pub fn list_all_attachments(&self) -> KernelDbResult<Vec<PersistedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, context_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (track_id, ctx_bytes, wakeup_every, rotate_every_phrases, ooda_armed) = row?;
            let context_id = ContextId::try_from_slice(&ctx_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    "invalid ContextId bytes in attachments row".into(),
                )
            })?;
            out.push(PersistedAttachment {
                track_id,
                context_id,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            });
        }
        Ok(out)
    }

    /// List all attachments for a context (all tracks this context is attached to).
    pub fn list_attachments_for_context(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Vec<PersistedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT track_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE context_id = ?1",
        )?;
        let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (track_id, wakeup_every, rotate_every_phrases, ooda_armed) = row?;
            out.push(PersistedAttachment {
                track_id,
                context_id,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            });
        }
        Ok(out)
    }

    /// Delete one attachment by `(track_id, context_id)`. Returns the row count
    /// (0 = already absent, 1 = deleted).
    pub fn delete_attachment(&self, track_id: &str, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM attachments WHERE track_id = ?1 AND context_id = ?2",
            params![track_id, blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Copy all attachments from `source` context to `child` context, keyed on
    /// the SAME `track_id`. Called inside the `insert_forked_context` transaction
    /// so the attachment inheritance is atomic with the context-row write.
    ///
    /// This is the attachment equivalent of the old `write_beat_state` fork copy
    /// (docs/tracks.md §3 — "the child inherits the bind at fork"). A non-musician
    /// source has no attachments, so the copy is a clean no-op in that case.
    /// Does NOT commit.
    pub(super) fn copy_attachments_for_fork(
        conn: &Connection,
        source: ContextId,
        child: ContextId,
    ) -> KernelDbResult<()> {
        // Read all source attachments first so the transaction holds only writes.
        let mut stmt = conn.prepare(
            "SELECT track_id, wakeup_every, rotate_every_phrases, ooda_armed
             FROM attachments WHERE context_id = ?1",
        )?;
        let rows: Vec<_> = stmt
            .query_map(params![blob_param(source.as_bytes())], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        for (track_id, wakeup_every, rotate_every_phrases, ooda_armed) in rows {
            let a = PersistedAttachment {
                track_id,
                context_id: child,
                wakeup_every: wakeup_every as u64,
                rotate_every_phrases: rotate_every_phrases.map(|n| n as u64),
                ooda_armed: ooda_armed != 0,
            };
            Self::write_attachment(conn, &a)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
