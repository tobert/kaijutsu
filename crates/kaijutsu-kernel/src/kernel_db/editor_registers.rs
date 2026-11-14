//! Editor registers: each character's vi text registers, one row per
//! register (`docs/vi.md`, "A kernel-owned register store").

use super::*;
use kaijutsu_types::editor::{RegisterShape, RegisterText};

fn shape_name(shape: RegisterShape) -> &'static str {
    match shape {
        RegisterShape::Charwise => "charwise",
        RegisterShape::Linewise => "linewise",
        RegisterShape::Blockwise => "blockwise",
    }
}

fn shape_from(name: &str) -> Option<RegisterShape> {
    match name {
        "charwise" => Some(RegisterShape::Charwise),
        "linewise" => Some(RegisterShape::Linewise),
        "blockwise" => Some(RegisterShape::Blockwise),
        _ => None,
    }
}

impl KernelDb {
    /// Write `rows`, `(seq, cell)`, for `principal` in one transaction: each
    /// replaces the row of its register. A row naming a register that has no
    /// row (the unnamed register, or anything but `0`-`9`, `-` and `a`-`z`)
    /// refuses the whole batch.
    pub fn write_editor_registers(&self, principal: PrincipalId, rows: &[(u64, RegisterText)]) -> KernelDbResult<()> {
        if let Some((_, bad)) = rows
            .iter()
            .find(|(_, r)| !matches!(r.name, '0'..='9' | '-' | 'a'..='z'))
        {
            return Err(KernelDbError::Validation(format!(
                "editor register {:?} has no row; a row names 0-9, - or a-z",
                bad.name
            )));
        }
        self.in_transaction(|db| {
            let now = now_millis();
            for (seq, row) in rows {
                db.conn.execute(
                    "INSERT INTO editor_registers (principal, name, shape, text, seq, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(principal, name) DO UPDATE SET
                        shape = excluded.shape,
                        text = excluded.text,
                        seq = excluded.seq,
                        updated_at = excluded.updated_at",
                    params![
                        blob_param(principal.as_bytes()),
                        row.name.to_string(),
                        shape_name(row.shape),
                        row.text,
                        *seq as i64,
                        now,
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// Every row of `principal`'s registers, `(seq, cell)`, in no order.
    pub fn editor_registers(&self, principal: PrincipalId) -> KernelDbResult<Vec<(u64, RegisterText)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, shape, text, seq FROM editor_registers WHERE principal = ?1")?;
        let rows = stmt
            .query_map(params![blob_param(principal.as_bytes())], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
            })?
            .collect::<SqliteResult<Vec<_>>>()?;
        rows.into_iter()
            .map(|(name, shape, text, seq)| {
                let mut chars = name.chars();
                let (Some(name), None) = (chars.next(), chars.next()) else {
                    return Err(KernelDbError::Validation(format!("editor register row has name {name:?}")));
                };
                let shape = shape_from(&shape).ok_or_else(|| {
                    KernelDbError::Validation(format!("editor register {name:?} has shape {shape:?}"))
                })?;
                Ok((seq as u64, RegisterText { name, text, shape }))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
