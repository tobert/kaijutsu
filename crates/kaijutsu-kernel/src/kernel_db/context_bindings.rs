//! Context tool bindings: storage and CRUD.

use super::*;

impl KernelDb {
    /// Upsert a full `ContextToolBinding` for `context_id`.
    ///
    /// Transactional: bumps the parent row's `updated_at`, then wholesale
    /// replaces the child `context_binding_instances` / `context_binding_names`
    /// rows. Callers treat the binding as the unit of write. This matches the
    /// in-memory model (`Broker::set_binding` is also wholesale) and keeps
    /// restoration trivial (read parent → read children → fold).
    pub fn upsert_context_binding(
        &mut self,
        context_id: ContextId,
        binding: &ContextToolBinding,
    ) -> KernelDbResult<()> {
        let tx = self.conn.transaction()?;
        Self::write_binding(&tx, context_id, binding)?;
        tx.commit()?;
        Ok(())
    }

    /// Wholesale-replace a context's binding (parent flags + every child row)
    /// against `conn`. The caller owns the transaction: `upsert_context_binding`
    /// wraps a fresh one, `fork_context_config` reuses its multi-write tx so
    /// the binding lands atomically alongside the shell + env copy. Does NOT
    /// commit — that is the caller's job.
    pub(super) fn write_binding(
        conn: &Connection,
        context_id: ContextId,
        binding: &ContextToolBinding,
    ) -> KernelDbResult<()> {
        conn.execute(
            "INSERT INTO context_bindings
                 (context_id, all_instances, all_facades, binding_admin, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(context_id) DO UPDATE SET
                 all_instances    = excluded.all_instances,
                 all_facades      = excluded.all_facades,
                 binding_admin    = excluded.binding_admin,
                 updated_at       = excluded.updated_at",
            params![
                blob_param(context_id.as_bytes()),
                binding.all_instances as i64,
                binding.all_facades as i64,
                binding.binding_admin as i64,
                now_millis(),
            ],
        )?;
        conn.execute(
            "DELETE FROM context_binding_instances WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_names WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_tools WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_facades WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        conn.execute(
            "DELETE FROM context_binding_authorities WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        for (idx, instance) in binding.allowed_instances.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_instances (context_id, instance_id, order_idx)
                 VALUES (?1, ?2, ?3)",
                params![
                    blob_param(context_id.as_bytes()),
                    instance.as_str(),
                    idx as i64,
                ],
            )?;
        }
        for (idx, (instance, tool)) in binding.allowed_tools.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_tools
                     (context_id, instance_id, original_tool, order_idx)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(context_id.as_bytes()),
                    instance.as_str(),
                    tool,
                    idx as i64,
                ],
            )?;
        }
        for (idx, facade) in binding.allowed_facades.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_binding_facades (context_id, facade_id, order_idx)
                 VALUES (?1, ?2, ?3)",
                params![blob_param(context_id.as_bytes()), facade, idx as i64],
            )?;
        }
        // Authorities are a set (BTreeSet iterates sorted) — no order_idx.
        for authority in &binding.authorities {
            conn.execute(
                "INSERT INTO context_binding_authorities (context_id, authority)
                 VALUES (?1, ?2)",
                params![blob_param(context_id.as_bytes()), authority],
            )?;
        }
        for (visible_name, (instance, tool)) in &binding.name_map {
            conn.execute(
                "INSERT INTO context_binding_names
                     (context_id, visible_name, instance_id, original_tool)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob_param(context_id.as_bytes()),
                    visible_name,
                    instance.as_str(),
                    tool,
                ],
            )?;
        }
        Ok(())
    }

    /// Load a `ContextToolBinding` for `context_id`, or `None` if the context
    /// has never been bound. Deny-by-default: callers treat `None` (and an
    /// all-empty binding) as "grants nothing" — there is no first-touch
    /// auto-populate.
    pub fn get_context_binding(
        &self,
        context_id: ContextId,
    ) -> KernelDbResult<Option<ContextToolBinding>> {
        let flags: Option<(bool, bool, bool)> = self
            .conn
            .query_row(
                "SELECT all_instances, all_facades, binding_admin
                 FROM context_bindings WHERE context_id = ?1",
                params![blob_param(context_id.as_bytes())],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .optional()?;
        let Some((all_instances, all_facades, binding_admin)) = flags else {
            return Ok(None);
        };

        let mut allowed_instances: Vec<InstanceId> = {
            let mut stmt = self.conn.prepare(
                "SELECT instance_id FROM context_binding_instances
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let s: String = row.get(0)?;
                Ok(InstanceId::new(s))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_instances.shrink_to_fit();

        let mut allowed_tools: Vec<(InstanceId, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT instance_id, original_tool FROM context_binding_tools
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let inst: String = row.get(0)?;
                let tool: String = row.get(1)?;
                Ok((InstanceId::new(inst), tool))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_tools.shrink_to_fit();

        let mut allowed_facades: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT facade_id FROM context_binding_facades
                 WHERE context_id = ?1
                 ORDER BY order_idx ASC",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            out
        };
        allowed_facades.shrink_to_fit();

        let authorities: std::collections::BTreeSet<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT authority FROM context_binding_authorities
                 WHERE context_id = ?1",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = std::collections::BTreeSet::new();
            for r in rows {
                out.insert(r?);
            }
            out
        };

        let mut name_map = std::collections::HashMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT visible_name, instance_id, original_tool
                 FROM context_binding_names
                 WHERE context_id = ?1",
            )?;
            let rows = stmt.query_map(params![blob_param(context_id.as_bytes())], |row| {
                let visible: String = row.get(0)?;
                let inst: String = row.get(1)?;
                let tool: String = row.get(2)?;
                Ok((visible, (InstanceId::new(inst), tool)))
            })?;
            for r in rows {
                let (visible, pair) = r?;
                name_map.insert(visible, pair);
            }
        }

        Ok(Some(ContextToolBinding {
            all_instances,
            all_facades,
            binding_admin,
            allowed_instances,
            allowed_tools,
            allowed_facades,
            authorities,
            name_map,
        }))
    }

    /// Test-only fault injection: drop a binding detail table out from under
    /// a context that already has a `context_bindings` parent row, so
    /// `get_context_binding` fails partway through instead of returning
    /// `Ok(None)`/`Ok(Some(_))`. Exists to exercise the "a real DB error must
    /// surface, not silently become deny-all" fix in `Broker::binding_checked`
    /// — there is no other way to force `get_context_binding` down its `Err`
    /// path through public API alone. Never compiled outside test builds.
    #[cfg(test)]
    pub(crate) fn poison_context_binding_detail_table_for_test(&self) -> KernelDbResult<()> {
        self.conn
            .execute_batch("DROP TABLE context_binding_instances")?;
        Ok(())
    }

    /// Delete the full binding for a context (cascades to instances + names).
    /// Returns true if a parent row existed.
    pub fn delete_context_binding(&self, context_id: ContextId) -> KernelDbResult<bool> {
        let rows = self.conn.execute(
            "DELETE FROM context_bindings WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
        )?;
        Ok(rows > 0)
    }
}

#[cfg(test)]
mod tests;
