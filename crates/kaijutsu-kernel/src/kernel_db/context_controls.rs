//! Control lists: the ordered control vectors a context sends with each turn.
//!
//! One `context_controls` row per item, ordered by `seq`, and one
//! `context_control_layers` row per layer weight of an item. A position is an
//! index into the ordered list; `seq` may have gaps after a removal. Storage
//! holds no vector bytes, only the content id the council server resolves.

use gogi_proto::ContentId;
use gogi_proto::control::{Control, ControlOp};

use super::*;

fn op_name(op: ControlOp) -> &'static str {
    match op {
        ControlOp::Add => "add",
        ControlOp::Ablate => "ablate",
        ControlOp::Clamp => "clamp",
    }
}

fn parse_op(name: &str) -> Option<ControlOp> {
    match name {
        "add" => Some(ControlOp::Add),
        "ablate" => Some(ControlOp::Ablate),
        "clamp" => Some(ControlOp::Clamp),
        _ => None,
    }
}

/// Refuses what the council server would refuse: a non-finite strength or
/// layer weight, and an empty `layers` map.
fn check_writable(item: &Control) -> KernelDbResult<()> {
    item.check("control").map_err(|e| KernelDbError::Validation(e.message))?;
    if let Some(layers) = &item.layers {
        if layers.is_empty() {
            return Err(KernelDbError::Validation(
                "a control item's layers names no layer; name a layer or more, or leave layers out to act on every layer".into(),
            ));
        }
        if let Some((layer, weight)) = layers.iter().find(|(_, w)| !w.is_finite()) {
            return Err(KernelDbError::Validation(format!(
                "layer {layer} has the weight {weight}; a weight is a finite number"
            )));
        }
    }
    Ok(())
}

impl KernelDb {
    /// Append `item` to the context's control list and return the position it
    /// took. Both tables are written in one transaction.
    pub fn add_context_control(&self, context_id: ContextId, item: &Control) -> KernelDbResult<usize> {
        check_writable(item)?;
        let tx = self.conn.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM context_controls WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO context_controls (context_id, seq, vector, op, strength)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![blob_param(context_id.as_bytes()), seq, item.vector.as_str(), op_name(item.op), item.strength],
        )?;
        for (layer, weight) in item.layers.iter().flatten() {
            tx.execute(
                "INSERT INTO context_control_layers (context_id, seq, layer, weight)
                 VALUES (?1, ?2, ?3, ?4)",
                params![blob_param(context_id.as_bytes()), seq, layer, weight],
            )?;
        }
        let position: i64 = tx.query_row(
            "SELECT COUNT(*) FROM context_controls WHERE context_id = ?1 AND seq < ?2",
            params![blob_param(context_id.as_bytes()), seq],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(position as usize)
    }

    /// The context's control list in order. A stored row that does not decode
    /// (an op that is not add, ablate, or clamp, a vector that is not a content
    /// id, a strength or weight that is not finite, a layer that is not a layer
    /// number) is [`KernelDbError::Corrupt`], never a skipped row: dropping an
    /// item changes what the model does.
    pub fn list_context_controls(&self, context_id: ContextId) -> KernelDbResult<Vec<Control>> {
        Ok(self.read_controls(context_id)?.into_iter().map(|(_, item)| item).collect())
    }

    /// The list with each item's `seq`.
    fn read_controls(&self, context_id: ContextId) -> KernelDbResult<Vec<(i64, Control)>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, vector, op, strength FROM context_controls
             WHERE context_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![blob_param(context_id.as_bytes())], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, f64>(3)?))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        let mut layer_stmt = self.conn.prepare(
            "SELECT layer, weight FROM context_control_layers
             WHERE context_id = ?1 AND seq = ?2 ORDER BY layer ASC",
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for (position, (seq, vector, op, strength)) in rows.into_iter().enumerate() {
            let corrupt = |what: String| {
                KernelDbError::Corrupt(format!(
                    "context {} control list, position {position}: {what}",
                    context_id.short()
                ))
            };
            let vector = ContentId::parse(vector).map_err(|e| corrupt(e))?;
            let op = parse_op(&op).ok_or_else(|| corrupt(format!("op {op:?} is not add, ablate, or clamp")))?;
            if !strength.is_finite() {
                return Err(corrupt(format!("strength is {strength}")));
            }
            let layer_rows = layer_stmt
                .query_map(params![blob_param(context_id.as_bytes()), seq], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<SqliteResult<Vec<_>>>()?;
            let layers = if layer_rows.is_empty() {
                None
            } else {
                let mut layers = std::collections::BTreeMap::new();
                for (layer, weight) in layer_rows {
                    let layer = u32::try_from(layer).map_err(|_| corrupt(format!("layer {layer} is not a layer number")))?;
                    if !weight.is_finite() {
                        return Err(corrupt(format!("layer {layer} has the weight {weight}")));
                    }
                    layers.insert(layer, weight);
                }
                Some(layers)
            };
            out.push((seq, Control { vector, op, strength, layers }));
        }
        Ok(out)
    }

    /// Remove the item at `position` of the ordered list and return it.
    /// [`KernelDbError::NotFound`] when the list is shorter. The layer rows go
    /// with the item.
    pub fn remove_context_control(&self, context_id: ContextId, position: usize) -> KernelDbResult<Control> {
        let items = self.read_controls(context_id)?;
        let Some((seq, item)) = items.into_iter().nth(position) else {
            return Err(KernelDbError::NotFound(format!(
                "context {} has no control at position {position}",
                context_id.short()
            )));
        };
        self.conn.execute(
            "DELETE FROM context_controls WHERE context_id = ?1 AND seq = ?2",
            params![blob_param(context_id.as_bytes()), seq],
        )?;
        Ok(item)
    }

    /// Delete the context's whole control list and return how many items it held.
    pub fn clear_context_controls(&self, context_id: ContextId) -> KernelDbResult<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM context_controls WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(deleted as u64)
    }

    /// Copy the control list of `source` to `successor`, as a rotation does.
    pub(crate) fn copy_context_controls(&self, source: ContextId, successor: ContextId) -> KernelDbResult<()> {
        Self::copy_controls_for_fork(&self.conn, source, successor)
    }

    /// Copy the control list of `source` to `child`, rows as stored, inside the
    /// caller's transaction. A fork keeps the list so a change in the child is
    /// an A/B against the parent.
    pub(super) fn copy_controls_for_fork(
        conn: &Connection,
        source: ContextId,
        child: ContextId,
    ) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_controls (context_id, seq, vector, op, strength)
             SELECT ?2, seq, vector, op, strength FROM context_controls WHERE context_id = ?1",
            params![blob_param(source.as_bytes()), blob_param(child.as_bytes())],
        )?;
        conn.execute(
            "INSERT INTO context_control_layers (context_id, seq, layer, weight)
             SELECT ?2, seq, layer, weight FROM context_control_layers WHERE context_id = ?1",
            params![blob_param(source.as_bytes()), blob_param(child.as_bytes())],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
