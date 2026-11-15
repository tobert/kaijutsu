//! Context shadows: the marker that tells a shadow from an ordinary kohai,
//! the rendition links from a shadow's blocks to their source blocks, and the
//! index that finds a rendition again by what it was made from (`docs/shadows.md`).
//!
//! A shadow is a child of its source that answers to it. `context_shadows`
//! holds one row per shadow with its kind. `shadow_renditions` links each
//! rendition block to the source block it renders, with an optional span of
//! it. `rendition_index` maps a source's content id, a kind, and a maker to the
//! rendition that holds the result; the rendition block is where the text lives.

use super::*;

/// The kind of the council's judge shadow.
pub const JUDGE: &str = "judge";

/// The kind of a shadow that holds descriptions of its source's media blocks.
pub const DESCRIBE: &str = "describe";

/// A rendition block and the source block it renders. `span` is a half-open
/// byte range of the source block, or `None` for the whole block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    /// The rendition block, in the shadow.
    pub block: BlockId,
    /// The source block, in the source context.
    pub source: BlockId,
    pub span: Option<(u64, u64)>,
}

/// What a rendition was made from: the source's content id, the kind of
/// rendition, the maker (`<backend>/<model>`), and a preset, empty when none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexKey {
    pub content_id: String,
    pub kind: String,
    pub maker: String,
    pub preset: String,
}

impl IndexKey {
    pub fn new(content_id: &str, kind: &str, maker: &str) -> Self {
        Self { content_id: content_id.into(), kind: kind.into(), maker: maker.into(), preset: String::new() }
    }
}

fn read_block_id(r: &rusqlite::Row<'_>, context: usize) -> SqliteResult<BlockId> {
    let seq: i64 = r.get(context + 2)?;
    Ok(BlockId::new(read_context_id(r, context)?, read_principal_id(r, context + 1)?, seq as u64))
}

impl KernelDb {
    /// The kind of `row` when it is a shadow: it has a `context_shadows` row
    /// and still answers to its parent. `None` for any other context.
    pub fn shadow_kind(&self, row: &ContextRow) -> KernelDbResult<Option<String>> {
        let Some(parent) = row.forked_from else { return Ok(None) };
        if row.senpai_ctx != Some(parent) {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT kind FROM context_shadows WHERE context_id = ?1",
                params![blob_param(row.context_id.as_bytes())],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Whether `row` is a judge shadow: the council made it as one
    /// ([`Self::mark_judge_shadow`]), and it still answers to its parent.
    pub fn is_judge_shadow(&self, row: &ContextRow) -> KernelDbResult<bool> {
        Ok(self.shadow_kind(row)?.as_deref() == Some(JUDGE))
    }

    /// Why `context_id` takes no chat turn because it is a shadow that only
    /// holds renditions, or `None`. A description shadow is written by
    /// `kj shadow describe`; the judge shadow keeps the rule its model slot
    /// gives it.
    pub fn chat_turn_refusal(&self, context_id: ContextId) -> KernelDbResult<Option<String>> {
        let Some(row) = self.get_context(context_id)? else { return Ok(None) };
        if self.shadow_kind(&row)?.as_deref() != Some(DESCRIBE) {
            return Ok(None);
        }
        Ok(Some(format!(
            "{} is a description shadow: it holds descriptions of {}'s media and takes no chat turn. \
             `kj shadow describe <block>` asks for a description",
            self.context_name(context_id),
            self.context_name(row.forked_from.unwrap_or(context_id)),
        )))
    }

    /// Record that `id` is a shadow of `kind`. The caller writes it in the
    /// transaction that inserts the shadow's row.
    pub fn mark_shadow(&self, id: ContextId, kind: &str) -> KernelDbResult<()> {
        self.conn.execute(
            "INSERT INTO context_shadows (context_id, kind) VALUES (?1, ?2)
             ON CONFLICT(context_id) DO NOTHING",
            params![blob_param(id.as_bytes()), kind],
        )?;
        Ok(())
    }

    /// Record that the council made `id` as a judge shadow.
    pub fn mark_judge_shadow(&self, id: ContextId) -> KernelDbResult<()> {
        self.mark_shadow(id, JUDGE)
    }

    /// The live shadows of `source`, each with its kind, oldest first. A shadow
    /// that is archived is not listed: `fork_children` lists live children only.
    pub fn shadows_of(&self, source: ContextId) -> KernelDbResult<Vec<(ContextRow, String)>> {
        let mut out = Vec::new();
        for child in self.fork_children(source)? {
            if let Some(kind) = self.shadow_kind(&child)? {
                out.push((child, kind));
            }
        }
        Ok(out)
    }

    /// The live shadow of `source` of `kind`, if it has one.
    pub fn live_shadow(&self, source: ContextId, kind: &str) -> KernelDbResult<Option<ContextRow>> {
        Ok(self.shadows_of(source)?.into_iter().find(|(_, k)| k == kind).map(|(row, _)| row))
    }

    /// Link a rendition to its source block and, when `key` is given, index it,
    /// in one transaction. A key that already names a rendition is refused and
    /// nothing is written.
    pub fn record_rendition(&self, rendition: &Rendition, key: Option<&IndexKey>) -> KernelDbResult<()> {
        let shadow = rendition.block.context_id;
        let (span_start, span_end) = match rendition.span {
            Some((start, end)) => (Some(start as i64), Some(end as i64)),
            None => (None, None),
        };
        self.atomically_in_shadows(|db| {
            db.conn.execute(
                "INSERT INTO shadow_renditions
                     (shadow_context_id, rendition_principal, rendition_seq,
                      source_context_id, source_principal, source_seq, span_start, span_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    blob_param(shadow.as_bytes()),
                    blob_param(rendition.block.principal_id.as_bytes()),
                    rendition.block.seq as i64,
                    blob_param(rendition.source.context_id.as_bytes()),
                    blob_param(rendition.source.principal_id.as_bytes()),
                    rendition.source.seq as i64,
                    span_start,
                    span_end,
                ],
            )?;
            if let Some(key) = key {
                db.conn.execute(
                    "INSERT INTO rendition_index
                         (content_id, kind, maker, preset,
                          shadow_context_id, rendition_principal, rendition_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        key.content_id,
                        key.kind,
                        key.maker,
                        key.preset,
                        blob_param(shadow.as_bytes()),
                        blob_param(rendition.block.principal_id.as_bytes()),
                        rendition.block.seq as i64,
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// Link a rendition and index it under `key`, in one transaction, unless
    /// the index already names a rendition for `key`: then the link alone is
    /// written and the index keeps the earlier one. Returns whether `key` now
    /// names `rendition`. Two requests that made the same rendition at once
    /// both land: one is indexed and both are linked.
    pub fn record_rendition_unless_indexed(&self, rendition: &Rendition, key: &IndexKey) -> KernelDbResult<bool> {
        self.atomically_in_shadows(|db| {
            if db.indexed_rendition(key)?.is_some() {
                db.record_rendition(rendition, None)?;
                return Ok(false);
            }
            db.record_rendition(rendition, Some(key))?;
            Ok(true)
        })
    }

    /// Run `f` in a transaction, or in a savepoint inside the caller's.
    fn atomically_in_shadows<T>(&self, f: impl FnOnce(&Self) -> KernelDbResult<T>) -> KernelDbResult<T> {
        if self.conn.is_autocommit() {
            return self.in_transaction(f);
        }
        self.conn.execute_batch("SAVEPOINT shadow_rendition")?;
        match f(self) {
            Ok(out) => {
                self.conn.execute_batch("RELEASE SAVEPOINT shadow_rendition")?;
                Ok(out)
            }
            Err(error) => {
                self.conn.execute_batch("ROLLBACK TO SAVEPOINT shadow_rendition; RELEASE SAVEPOINT shadow_rendition")?;
                Err(error)
            }
        }
    }

    /// The rendition block the index holds for `key`.
    pub fn indexed_rendition(&self, key: &IndexKey) -> KernelDbResult<Option<BlockId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT shadow_context_id, rendition_principal, rendition_seq FROM rendition_index
                 WHERE content_id = ?1 AND kind = ?2 AND maker = ?3 AND preset = ?4",
                params![key.content_id, key.kind, key.maker, key.preset],
                |r| read_block_id(r, 0),
            )
            .optional()?)
    }

    /// The renditions of `shadow`, oldest first.
    pub fn renditions_of(&self, shadow: ContextId) -> KernelDbResult<Vec<Rendition>> {
        let mut stmt = self.conn.prepare(
            "SELECT shadow_context_id, rendition_principal, rendition_seq,
                    source_context_id, source_principal, source_seq, span_start, span_end
             FROM shadow_renditions WHERE shadow_context_id = ?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(params![blob_param(shadow.as_bytes())], |r| {
            let span: (Option<i64>, Option<i64>) = (r.get(6)?, r.get(7)?);
            Ok(Rendition {
                block: read_block_id(r, 0)?,
                source: read_block_id(r, 3)?,
                span: match span {
                    (Some(start), Some(end)) => Some((start as u64, end as u64)),
                    _ => None,
                },
            })
        })?;
        Ok(rows.collect::<SqliteResult<_>>()?)
    }

    /// How many renditions `shadow` holds.
    pub fn rendition_count(&self, shadow: ContextId) -> KernelDbResult<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM shadow_renditions WHERE shadow_context_id = ?1",
            params![blob_param(shadow.as_bytes())],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests;
