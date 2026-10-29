//! Model prices: an append-only history of what a model's tokens cost.
//!
//! A price is keyed by the `(provider, model)` strings a turn records, and
//! holds the rates from `effective_from` until a later row replaces it. A
//! change inserts a row; no row is updated or deleted (triggers refuse both),
//! so a turn's cost can always be recomputed from the price in force when it
//! started. A correction inserts a row with the same `effective_from`; the
//! later insert wins.
//!
//! Rates are integer micro-dollars per million tokens: $3.00 per million is
//! `3_000_000`, and tokens times a rate over `1_000_000` is micro-dollars.
//! `input` and `output` are always known. A cache rate may be `None`, which
//! means unknown: a price cannot cost tokens it has no rate for.

use rusqlite::{OptionalExtension, params};

use kaijutsu_types::{ContextId, PrincipalId};

use super::{KernelDb, KernelDbError, KernelDbResult, blob_param, now_millis, read_context_id};

/// One stored price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPriceRow {
    pub price_id: i64,
    pub provider: String,
    pub model: String,
    /// Unix milliseconds the rates take effect.
    pub effective_from: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: Option<i64>,
    pub cache_write_5m: Option<i64>,
    pub cache_write_1h: Option<i64>,
    /// Free text saying where the numbers came from.
    pub source: String,
    pub set_by: Option<PrincipalId>,
    pub created_at: i64,
}

/// A price to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewModelPrice {
    pub provider: String,
    pub model: String,
    pub effective_from: i64,
    pub input: i64,
    pub output: i64,
    pub cache_read: Option<i64>,
    pub cache_write_5m: Option<i64>,
    pub cache_write_1h: Option<i64>,
    pub source: String,
    pub set_by: Option<PrincipalId>,
}

/// A context's turns on one model, summed over the turns that ran under one
/// price. `price_id` is `None` when no price was in force for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostGroup {
    pub context_id: ContextId,
    pub provider: String,
    pub model: String,
    pub price_id: Option<i64>,
    pub llm_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_write_1h_tokens: i64,
}

const PRICE_COLUMNS: &str = "price_id, provider, model, effective_from, input_micros,
    output_micros, cache_read_micros, cache_write_5m_micros, cache_write_1h_micros,
    source, set_by, created_at";

fn read_price(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelPriceRow> {
    let set_by: Option<Vec<u8>> = row.get(10)?;
    Ok(ModelPriceRow {
        price_id: row.get(0)?,
        provider: row.get(1)?,
        model: row.get(2)?,
        effective_from: row.get(3)?,
        input: row.get(4)?,
        output: row.get(5)?,
        cache_read: row.get(6)?,
        cache_write_5m: row.get(7)?,
        cache_write_1h: row.get(8)?,
        source: row.get(9)?,
        set_by: set_by.as_deref().and_then(PrincipalId::try_from_slice),
        created_at: row.get(11)?,
    })
}

impl KernelDb {
    /// Append a price and return its id. Refuses a negative rate.
    pub fn insert_model_price(&self, price: &NewModelPrice) -> KernelDbResult<i64> {
        let rates = [Some(price.input), Some(price.output), price.cache_read, price.cache_write_5m, price.cache_write_1h];
        if rates.iter().flatten().any(|rate| *rate < 0) {
            return Err(KernelDbError::Validation("a price rate cannot be negative".into()));
        }
        if price.provider.is_empty() || price.model.is_empty() {
            return Err(KernelDbError::Validation("a price names a provider and a model".into()));
        }
        let set_by = price.set_by.map(|p| *p.as_bytes());
        self.conn.execute(
            "INSERT INTO model_prices (provider, model, effective_from, input_micros, output_micros,
                cache_read_micros, cache_write_5m_micros, cache_write_1h_micros, source, set_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                price.provider,
                price.model,
                price.effective_from,
                price.input,
                price.output,
                price.cache_read,
                price.cache_write_5m,
                price.cache_write_1h,
                price.source,
                set_by.as_ref().map(blob_param),
                now_millis(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The price in force for a model at `at`: the latest `effective_from` at
    /// or before it, the later insert winning a tie. `None` when no price
    /// applies, which is "unknown", never free.
    pub fn effective_model_price(&self, provider: &str, model: &str, at: i64) -> KernelDbResult<Option<ModelPriceRow>> {
        let sql = format!(
            "SELECT {PRICE_COLUMNS} FROM model_prices
             WHERE provider = ?1 AND model = ?2 AND effective_from <= ?3
             ORDER BY effective_from DESC, price_id DESC LIMIT 1"
        );
        Ok(self.conn.query_row(&sql, params![provider, model, at], read_price).optional()?)
    }

    /// Every price, oldest first, optionally narrowed to one model.
    pub fn list_model_prices(&self, model: Option<(&str, &str)>) -> KernelDbResult<Vec<ModelPriceRow>> {
        let (provider, name) = model.unzip();
        let sql = format!(
            "SELECT {PRICE_COLUMNS} FROM model_prices
             WHERE (?1 IS NULL OR provider = ?1) AND (?2 IS NULL OR model = ?2)
             ORDER BY provider, model, effective_from, price_id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![provider, name], read_price)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One price by id.
    pub fn get_model_price(&self, price_id: i64) -> KernelDbResult<Option<ModelPriceRow>> {
        let sql = format!("SELECT {PRICE_COLUMNS} FROM model_prices WHERE price_id = ?1");
        Ok(self.conn.query_row(&sql, params![price_id], read_price).optional()?)
    }

    /// Turns summed per context, model and the price in force when each
    /// started, for one context or every context. A turn with no recorded
    /// call is left out.
    pub fn context_cost_groups(&self, context: Option<ContextId>) -> KernelDbResult<Vec<CostGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT context_id, provider, model, price_id,
                    SUM(llm_calls), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_read_tokens), SUM(cache_write_tokens), SUM(cache_write_1h_tokens)
             FROM (
                SELECT t.context_id, t.provider, t.model, t.llm_calls, t.input_tokens, t.output_tokens,
                       t.cache_read_tokens, t.cache_write_tokens, t.cache_write_1h_tokens,
                       (SELECT p.price_id FROM model_prices p
                         WHERE p.provider = t.provider AND p.model = t.model AND p.effective_from <= t.started_at
                         ORDER BY p.effective_from DESC, p.price_id DESC LIMIT 1) AS price_id
                FROM turns t
                WHERE t.llm_calls > 0 AND (?1 IS NULL OR t.context_id = ?1)
             )
             GROUP BY context_id, provider, model, price_id
             ORDER BY context_id, provider, model, price_id",
        )?;
        let context = context.map(|c| *c.as_bytes());
        let context = context.as_ref().map(blob_param);
        let rows = stmt.query_map(params![context], |row| {
            Ok(CostGroup {
                context_id: read_context_id(row, 0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                price_id: row.get(3)?,
                llm_calls: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                cache_write_tokens: row.get(8)?,
                cache_write_1h_tokens: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
