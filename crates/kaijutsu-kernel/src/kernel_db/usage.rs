//! Context usage and turns.

use super::*;

/// A SNAPSHOT of the most recent completed LLM call this context made —
/// see the `context_usage` table doc comment in `SCHEMA` for why this is a
/// last-write-wins snapshot rather than a running total. Fields are `i64`
/// (never negative in practice) to match the SQLite column type directly,
/// matching the project's other persisted-row structs (e.g. `PersistedTrack`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageRow {
    pub context_id: ContextId,
    /// The provider that served this call (`Provider::name()`, e.g.
    /// "anthropic", "deepseek") — carried so a context-window denominator
    /// can be attached later by (provider, model), not implemented here.
    pub provider: String,
    pub model: String,
    /// Total prompt size on the last completed call, ALREADY normalized
    /// across providers to include any cache-read/cache-creation tokens
    /// (see the caller in `llm_stream.rs` for the per-provider math).
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Informational cache breakdown of `input_tokens` — zero when the
    /// provider reported none.
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Informational — reasoning/thinking tokens the provider billed
    /// separately from `output_tokens` (0 when not reported/applicable).
    pub reasoning_tokens: i64,
    /// The `CacheTtl` (`llm/stream.rs`) actually chosen for this call's
    /// cache breakpoints, in seconds: 300 (`Ephemeral`), 3600 (`Extended`),
    /// or the longest of the two when a request mixed both. 0 when the
    /// request built no breakpoints (a provider that ignores them, or none
    /// configured) — never a guess.
    pub cache_ttl_secs: i64,
    /// Unix MILLISECONDS (matches `kaijutsu_types::now_millis`, not
    /// `unixepoch()`'s seconds) of this snapshot's write.
    pub updated_at: i64,
}

impl ContextUsageRow {
    /// Total tokens filled by the LAST completed call (input + output) —
    /// the fill `context_used_pct` is computed against, never a running
    /// total across turns.
    pub fn total_tokens(&self) -> i64 {
        self.input_tokens + self.output_tokens
    }
}

/// What one model turn is, recorded when it starts. The turn id is the
/// `TurnId` the runtime minted; every other field is what the resolver and the
/// identity check decided for this turn. See the `turns` table in `SCHEMA`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnStart {
    pub turn_id: TurnId,
    pub context_id: ContextId,
    /// The character that performed the turn.
    pub performer: PrincipalId,
    /// The context type the context slot was matched on.
    pub role: String,
    pub provider: String,
    pub model: String,
    /// Unix milliseconds.
    pub started_at: i64,
}

/// Token counts exactly as the provider reported them for one call. Nothing is
/// normalized: Claude's `input` excludes `cache_read` and `cache_write`;
/// DeepSeek's and the other OpenAI-compatible providers' `input` already
/// includes `cache_read`. Sums stay honest per provider; compare across
/// providers only through the cache columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Every cache write, whatever its TTL.
    pub cache_write: u64,
    /// The part of `cache_write` written with the 1-hour TTL. Claude only.
    pub cache_write_1h: u64,
    pub reasoning: u64,
}

/// One row of `turns`: a turn's start facts, its end, and the provider's raw
/// token counts summed over every call the turn made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    pub start: TurnStart,
    /// `None` while the turn runs, and for a turn the kernel stopped before
    /// it could write its end.
    pub ended_at: Option<i64>,
    pub stop_reason: Option<String>,
    /// Provider calls the turn made that reported usage (a `Done`, or an `Error` that carried counts).
    pub llm_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// The part of `cache_write_tokens` written with the 1-hour TTL.
    pub cache_write_1h_tokens: i64,
    pub reasoning_tokens: i64,
}

/// A context's turns, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnTotals {
    pub turns: i64,
    pub llm_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_1h_tokens: i64,
    pub reasoning_tokens: i64,
}

/// How full a context is, as a percentage of `window` (0-100+, never
/// clamped), computed from the LAST completed call's fill
/// (`usage.total_tokens()`) — not a running total across turns. `None`
/// when `window` is `None` (the model's context window isn't configured)
/// — the caller must render that as an explicit "unknown", never a
/// fabricated denominator standing in for one.
///
/// This is the ONE percentage-math site: `kj context info --json`
/// (`kj/context.rs`) and the wire's `contextUsedPct`
/// (`ContextHandleInfo` in `kaijutsu.capnp`, populated in
/// `kaijutsu-server/src/rpc.rs::list_contexts`) both call through here so
/// the two surfaces can never disagree.
///
/// A window of `Some(0)` is treated as unknown rather than divided by: it
/// would yield `NaN` (0 tokens) or `+Inf` (any tokens), and neither is
/// caught downstream — the wire carries the percentage as `Float32` and the
/// client's unknown-sentinel test is `pct < 0.0`, which is FALSE for both
/// `NaN` and `+Inf`, so the garbage would sail through to the dock badge as
/// a confident-looking `Some(...)`. A zero window is a config error, not a
/// measurement, and "unknown" is the honest reading of it.
pub fn context_used_pct(usage: &ContextUsageRow, window: Option<u64>) -> Option<f64> {
    window
        .filter(|&w| w > 0)
        .map(|w| usage.total_tokens() as f64 / w as f64 * 100.0)
}

impl KernelDb {
    /// Upsert the context-usage snapshot — overwrites whatever was there
    /// (this is a snapshot of "the last call," not an accumulator; see the
    /// `context_usage` table doc comment in `SCHEMA`).
    pub fn set_context_usage(&self, row: &ContextUsageRow) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_usage (
                context_id, provider, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens,
                cache_ttl_secs, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(context_id) DO UPDATE SET
                provider = excluded.provider,
                model = excluded.model,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                cache_ttl_secs = excluded.cache_ttl_secs,
                updated_at = excluded.updated_at",
            params![
                blob_param(row.context_id.as_bytes()),
                row.provider,
                row.model,
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.reasoning_tokens,
                row.cache_ttl_secs,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Read the context-usage snapshot for `context_id`, or `None` when this
    /// context has never completed an LLM call (a fresh context, or one that
    /// only ever failed/was interrupted before a `Done` event — honest
    /// absence, not a fabricated zero).
    pub fn get_context_usage(&self, context_id: ContextId) -> KernelDbResult<Option<ContextUsageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, provider, model, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    cache_ttl_secs, updated_at
             FROM context_usage WHERE context_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
            Ok(ContextUsageRow {
                context_id: read_context_id(row, 0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_write_tokens: row.get(6)?,
                reasoning_tokens: row.get(7)?,
                cache_ttl_secs: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Read every context's usage snapshot in one query — the bulk
    /// counterpart to [`Self::get_context_usage`] for callers that need to
    /// annotate a whole listing (e.g. `listContexts`'s wire response)
    /// without one DB round-trip per context, mirroring
    /// [`Self::list_all_contexts`] / [`Self::list_all_attachments`].
    pub fn list_all_context_usage(&self) -> KernelDbResult<Vec<ContextUsageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, provider, model, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    cache_ttl_secs, updated_at
             FROM context_usage",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ContextUsageRow {
                context_id: read_context_id(row, 0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_write_tokens: row.get(6)?,
                reasoning_tokens: row.get(7)?,
                cache_ttl_secs: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Write the row for a turn that is starting. The turn id is unique: a
    /// second start for the same id fails.
    pub fn begin_turn(&self, turn: &TurnStart) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO turns (
                turn_id, context_id, performer, role,
                provider, model, started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob_param(turn.turn_id.as_bytes()),
                blob_param(turn.context_id.as_bytes()),
                blob_param(turn.performer.as_bytes()),
                turn.role,
                turn.provider,
                turn.model,
                turn.started_at,
            ],
        )?;
        Ok(())
    }

    /// Add one provider call's raw counts to its turn. Fails when the turn has
    /// no row or has already ended: a count that cannot be recorded is an error.
    pub fn add_turn_usage(&self, turn_id: TurnId, usage: RawUsage) -> KernelDbResult<()> {
        let changed = self.conn.execute(
            "UPDATE turns SET
                llm_calls = llm_calls + 1,
                input_tokens = input_tokens + ?2,
                output_tokens = output_tokens + ?3,
                cache_read_tokens = cache_read_tokens + ?4,
                cache_write_tokens = cache_write_tokens + ?5,
                reasoning_tokens = reasoning_tokens + ?6,
                cache_write_1h_tokens = cache_write_1h_tokens + ?7
             WHERE turn_id = ?1 AND ended_at IS NULL",
            params![
                blob_param(turn_id.as_bytes()),
                usage.input as i64,
                usage.output as i64,
                usage.cache_read as i64,
                usage.cache_write as i64,
                usage.reasoning as i64,
                usage.cache_write_1h as i64,
            ],
        )?;
        if changed == 0 {
            return Err(KernelDbError::NotFound(format!("running turn {turn_id}")));
        }
        Ok(())
    }

    /// Close a turn with how it ended. Fails when the turn has no row or has
    /// already ended.
    pub fn finish_turn(&self, turn_id: TurnId, ended_at: i64, stop_reason: &str) -> KernelDbResult<()> {
        let changed = self.conn.execute(
            "UPDATE turns SET ended_at = ?2, stop_reason = ?3
             WHERE turn_id = ?1 AND ended_at IS NULL",
            params![blob_param(turn_id.as_bytes()), ended_at, stop_reason],
        )?;
        if changed == 0 {
            return Err(KernelDbError::NotFound(format!("running turn {turn_id}")));
        }
        Ok(())
    }

    pub(super) const TURN_COLUMNS: &'static str = "turn_id, context_id, performer, role, provider, model,
        started_at, ended_at, stop_reason, llm_calls, input_tokens, output_tokens,
        cache_read_tokens, cache_write_tokens, reasoning_tokens, cache_write_1h_tokens";

    pub(super) fn read_turn_row(row: &rusqlite::Row<'_>) -> SqliteResult<TurnRow> {
        let turn_bytes: Vec<u8> = row.get(0)?;
        let turn_id = TurnId::try_from_slice(&turn_bytes).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0, rusqlite::types::Type::Blob, "invalid TurnId bytes".into())
        })?;
        Ok(TurnRow {
            start: TurnStart {
                turn_id,
                context_id: read_context_id(row, 1)?,
                performer: read_principal_id(row, 2)?,
                role: row.get(3)?,
                provider: row.get(4)?,
                model: row.get(5)?,
                started_at: row.get(6)?,
            },
            ended_at: row.get(7)?,
            stop_reason: row.get(8)?,
            llm_calls: row.get(9)?,
            input_tokens: row.get(10)?,
            output_tokens: row.get(11)?,
            cache_read_tokens: row.get(12)?,
            cache_write_tokens: row.get(13)?,
            reasoning_tokens: row.get(14)?,
            cache_write_1h_tokens: row.get(15)?,
        })
    }

    /// One turn, or `None` when no such turn was recorded.
    pub fn get_turn(&self, turn_id: TurnId) -> KernelDbResult<Option<TurnRow>> {
        let sql = format!("SELECT {} FROM turns WHERE turn_id = ?1", Self::TURN_COLUMNS);
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![blob_param(turn_id.as_bytes())], Self::read_turn_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// A context's most recent turns, newest first.
    pub fn list_turns(&self, context_id: ContextId, limit: usize) -> KernelDbResult<Vec<TurnRow>> {
        let sql = format!(
            "SELECT {} FROM turns WHERE context_id = ?1 ORDER BY started_at DESC, turn_id DESC LIMIT ?2",
            Self::TURN_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![blob_param(context_id.as_bytes()), limit as i64],
            Self::read_turn_row,
        )?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Every turn of a context, summed. All zero for a context that never ran one.
    pub fn context_turn_totals(&self, context_id: ContextId) -> KernelDbResult<TurnTotals> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(llm_calls), 0), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0), COALESCE(SUM(reasoning_tokens), 0),
                    COALESCE(SUM(cache_write_1h_tokens), 0)
             FROM turns WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| Ok(TurnTotals {
                turns: row.get(0)?,
                llm_calls: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_write_tokens: row.get(5)?,
                reasoning_tokens: row.get(6)?,
                cache_write_1h_tokens: row.get(7)?,
            }),
        )?)
    }
}

#[cfg(test)]
mod tests;
