//! Context presets and their verb-scoped args.

use super::*;

/// A preset template row.
#[derive(Debug, Clone)]
pub struct PresetRow {
    pub preset_id: PresetId,
    pub label: String,
    pub description: Option<String>,
    pub created_at: i64,
    pub created_by: PrincipalId,
}

/// One normalized, verb-scoped preset argument (a row of `preset_args`). The
/// "filter knobs" a patch recalls — e.g. `("include", "0:5")` under verb
/// `"fork"`. Repeatable arg names carry multiple `PresetArg`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetArg {
    pub arg_name: String,
    pub arg_value: String,
}

impl KernelDb {
    /// Insert a new preset.
    pub fn insert_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        self.conn
            .execute(
                "INSERT INTO presets (
                preset_id, label, description, created_at, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    blob_param(row.preset_id.as_bytes()),
                    row.label,
                    row.description,
                    row.created_at,
                    blob_param(row.created_by.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;
        Ok(())
    }

    /// Get a preset by ID.
    pub fn get_preset(&self, id: PresetId) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, created_at, created_by
             FROM presets WHERE preset_id = ?1",
        )?;

        let mut rows = stmt.query(params![blob_param(id.as_bytes())])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a preset by label.
    pub fn get_preset_by_label(&self, label: &str) -> KernelDbResult<Option<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, created_at, created_by
             FROM presets WHERE label = ?1",
        )?;

        let mut rows = stmt.query(params![label])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_preset_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all presets.
    pub fn list_presets(&self) -> KernelDbResult<Vec<PresetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT preset_id, label, description, created_at, created_by
             FROM presets
             ORDER BY label",
        )?;

        let rows = stmt.query_map([], row_to_preset_row)?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Update a preset.
    pub fn update_preset(&self, row: &PresetRow) -> KernelDbResult<()> {
        validate_label(&row.label)?;

        let updated = self
            .conn
            .execute(
                "UPDATE presets SET
                label = ?1, description = ?2
             WHERE preset_id = ?3",
                params![
                    row.label,
                    row.description,
                    blob_param(row.preset_id.as_bytes()),
                ],
            )
            .map_err(|e| {
                map_unique_violation(e, format!("preset label '{}' already in use", row.label))
            })?;

        if updated == 0 {
            return Err(KernelDbError::NotFound(format!("preset {}", row.preset_id)));
        }
        Ok(())
    }

    /// Delete a preset. Returns true if it existed. Its `preset_args` rows
    /// cascade-delete with it.
    pub fn delete_preset(&self, id: PresetId) -> KernelDbResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM presets WHERE preset_id = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        Ok(deleted > 0)
    }

    /// Replace all args for `(preset_id, verb)` with `args`, transactionally.
    /// Empty `args` clears the verb's args. Idempotent — duplicate
    /// `(arg_name, arg_value)` pairs collapse via the composite PK.
    pub fn set_preset_args(
        &mut self,
        preset_id: PresetId,
        verb: &str,
        args: &[PresetArg],
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM preset_args WHERE preset_id = ?1 AND verb = ?2",
            params![blob_param(preset_id.as_bytes()), verb],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO preset_args (preset_id, verb, arg_name, arg_value)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for a in args {
                stmt.execute(params![
                    blob_param(preset_id.as_bytes()),
                    verb,
                    a.arg_name,
                    a.arg_value,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the args for `(preset_id, verb)`, ordered by `(arg_name,
    /// arg_value)` for a stable round-trip. The selection algebra is order-free,
    /// so value-order is purely for determinism.
    pub fn get_preset_args(&self, preset_id: PresetId, verb: &str) -> KernelDbResult<Vec<PresetArg>> {
        let mut stmt = self.conn.prepare(
            "SELECT arg_name, arg_value FROM preset_args
             WHERE preset_id = ?1 AND verb = ?2
             ORDER BY arg_name, arg_value",
        )?;
        let rows = stmt.query_map(params![blob_param(preset_id.as_bytes()), verb], |row| {
            Ok(PresetArg {
                arg_name: row.get(0)?,
                arg_value: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<SqliteResult<Vec<_>>>()?)
    }

    /// Count contexts using a specific preset.
    pub fn contexts_using_preset(&self, preset_id: PresetId) -> KernelDbResult<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts
             WHERE preset_id = ?1 AND archived_at IS NULL",
            params![blob_param(preset_id.as_bytes())],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests;
