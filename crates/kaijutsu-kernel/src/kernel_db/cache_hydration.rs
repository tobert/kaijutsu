//! Claude cache breakpoints and hydration policy.

use super::*;

/// Wire a [`CacheTarget`] onto the three storage columns:
/// `(target_kind, target_index, ttl)`. `target_index` is non-null only
/// for `MessageIndex`. `ttl` is the lowercase variant name.
pub(super) fn encode_cache_target(target: &CacheTarget) -> (&'static str, Option<i64>, &'static str) {
    let (kind, index) = match target {
        CacheTarget::Tools(_) => ("tools", None),
        CacheTarget::System(_) => ("system", None),
        CacheTarget::MessageIndex(i, _) => ("message_index", Some(*i as i64)),
    };
    let ttl = match target.ttl() {
        CacheTtl::Ephemeral => "ephemeral",
        CacheTtl::Extended => "extended",
    };
    (kind, index, ttl)
}

/// Reverse of [`encode_cache_target`]. Returns `None` when the row carries
/// an unrecognized `target_kind`, missing `target_index` for
/// `message_index`, or an unrecognized `ttl` — the caller logs and skips.
pub(super) fn decode_cache_target(kind: &str, index: Option<i64>, ttl: &str) -> Option<CacheTarget> {
    let ttl = match ttl {
        "ephemeral" => CacheTtl::Ephemeral,
        "extended" => CacheTtl::Extended,
        _ => return None,
    };
    match kind {
        "tools" => Some(CacheTarget::Tools(ttl)),
        "system" => Some(CacheTarget::System(ttl)),
        "message_index" => {
            let i = index?;
            if i < 0 {
                return None;
            }
            Some(CacheTarget::MessageIndex(i as usize, ttl))
        }
        _ => None,
    }
}

/// Default hydration window (in blocks) applied as a backstop when a context
/// becomes beat-driven without an explicit policy — keeps a musician's per-turn
/// hydration bounded so an at-tempo driver can't grow the wire history without
/// limit (the crash-loop this guards). Matches the rc default (`kj context
/// hydrate --window 16` in assets/defaults/rc/musician/…). See docs/chameleon.md.
///
/// Free-standing (not an associated const) so both `kj/fork.rs` (same crate,
/// `crate::kernel_db::DEFAULT_HYDRATION_WINDOW`) and `kaijutsu-server`
/// (`kaijutsu_kernel::kernel_db::DEFAULT_HYDRATION_WINDOW`) can import it by
/// module path without naming `KernelDb`.
pub const DEFAULT_HYDRATION_WINDOW: u32 = 16;

impl KernelDb {
    /// Append a cache breakpoint for `context_id`. The breakpoint takes the
    /// next available `seq` slot, preserving declaration order across
    /// subsequent reads. Storage is liberal — the 4-cap and dedupe live
    /// in the Claude wire layer (`crate::llm::claude::build::plan_cache`),
    /// not here, so rc populators can over-spec without storage rejecting
    /// them.
    pub fn add_cache_breakpoint(
        &self,
        context_id: ContextId,
        target: &CacheTarget,
    ) -> KernelDbResult<i64> {
        let next_seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1
                 FROM cache_breakpoints WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| row.get(0),
            )?;
        let (kind, index, ttl) = encode_cache_target(target);
        self.conn.execute(
            "INSERT INTO cache_breakpoints
                 (context_id, seq, target_kind, target_index, ttl)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blob_param(context_id.as_bytes()),
                next_seq,
                kind,
                index,
                ttl,
            ],
        )?;
        Ok(next_seq)
    }

    /// Read all cache breakpoints for `context_id`, ordered by insertion
    /// (`seq` ASC). Rows with an unrecognized `target_kind` or `ttl`
    /// are skipped with a `tracing::warn!` — storage is forwards-compatible
    /// with future variants, but Claude's build layer must not see them.
    pub fn list_cache_breakpoints(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Vec<CacheTarget>> {
        let mut stmt = self.conn.prepare(
            "SELECT target_kind, target_index, ttl
             FROM cache_breakpoints
             WHERE context_id = ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], |row| {
                let kind: String = row.get(0)?;
                let index: Option<i64> = row.get(1)?;
                let ttl: String = row.get(2)?;
                Ok((kind, index, ttl))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (kind, index, ttl) in rows {
            match decode_cache_target(&kind, index, &ttl) {
                Some(t) => out.push(t),
                None => {
                    warn!(
                        context = %context_id.short(),
                        target_kind = %kind,
                        target_index = ?index,
                        ttl = %ttl,
                        "cache breakpoint row dropped: unrecognized fields"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Delete all cache breakpoints for `context_id`. Returns the row count.
    /// Used by rc-on-drift scripts that want to clear-then-rebuild after a
    /// conversation reshape (compact, model swap, doc inject).
    pub fn clear_cache_breakpoints(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM cache_breakpoints WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Set (upsert) the hydration window policy for `context_id`: from now on the
    /// conversation hydrates only `[0, marker] ∪ last-window_size`, not its whole
    /// history. `marker` is the pinned-prefix end P (a durable block — `[0,P]`
    /// always hydrates and stays cache-stable); `window_size` is the sliding tail
    /// W. The cost guard for endless musician logs (design: `docs/chameleon.md`,
    /// the hydration marker). Upserted at musician-create and on a durable
    /// revision (marker advance) — NOT per turn; the tail slides in memory.
    pub fn set_hydration_policy(
        &self,
        context_id: ContextId,
        marker: BlockId,
        window_size: u32,
    ) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_hydration (context_id, marker, window_size)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(context_id) DO UPDATE SET marker = ?2, window_size = ?3",
            params![
                blob_param(context_id.as_bytes()),
                marker.to_key(),
                window_size,
            ],
        )?;
        Ok(())
    }

    /// Read the hydration window policy for `context_id`, or `None` when unset —
    /// `None` means hydrate everything (the default; every non-musician context).
    /// A *present but malformed* row (window < 1, or a marker that no longer
    /// parses) is corruption, and returns `Err(Validation)` so the caller fails
    /// the turn loudly. Silently degrading corrupt config to "hydrate everything"
    /// would disable the cost guard on a context driving at tempo (unbounded
    /// spend) — a silent fallback on a safety mechanism. Absent row = legible
    /// default (`Ok(None)`); malformed presence = loud failure.
    pub fn get_hydration_policy(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<(BlockId, u32)>> {
        let row = self
            .conn
            .query_row(
                "SELECT marker, window_size FROM context_hydration WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| {
                    let marker: String = row.get(0)?;
                    let window: i64 = row.get(1)?;
                    Ok((marker, window))
                },
            )
            .optional()?;
        let Some((marker_key, window)) = row else {
            return Ok(None);
        };
        // A 0/negative window or an unparseable marker is CORRUPT stored state,
        // not a legible policy. Refuse loudly (the caller fails the turn) rather
        // than silently degrade to full history — disabling the cost guard on a
        // context driving at tempo is a silent fallback on a safety mechanism.
        if window < 1 {
            return Err(KernelDbError::Validation(format!(
                "context {} hydration policy has window {window} (< 1) — corrupt",
                context_id.short()
            )));
        }
        match BlockId::from_key(&marker_key) {
            Some(marker) => Ok(Some((marker, window as u32))),
            None => Err(KernelDbError::Validation(format!(
                "context {} hydration marker {marker_key:?} is unparseable — corrupt",
                context_id.short()
            ))),
        }
    }

    /// Clear the hydration policy for `context_id` → revert to hydrating
    /// everything. Returns the row count (0 or 1).
    pub fn clear_hydration_policy(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM context_hydration WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }
}

#[cfg(test)]
mod tests;
